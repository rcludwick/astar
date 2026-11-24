// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `p25-listen` subcommand: link to a `P25Reflector` and decode the voice on
//! it.
//!
//! **Receive only, and there is no PTT here at all.** astar has no P25
//! transmit path, so this command has no way to ask for one.
//!
//! **Needs no hardware.** P25 voice is decoded by the software IMBE
//! vocoder, so unlike `nxdn-listen` there is no dongle and no
//! `ASTAR_VOCODER_SERIALS`. P25 has no password either; were a secret ever
//! added it would come from the environment, never from argv.
//!
//! A `P25Reflector` has no rooms: it relays every over to every linked
//! client. So the optional `[tg]` argument is only a label, printed on the
//! link line and never sent.
//!
//! P25 addresses stations by NUMBER. A talker is reported as its 24-bit
//! radio id; turning that into a callsign needs a directory astar does not
//! hold, so inventing one here would be a guess presented as identification.
//! An encrypted over is announced and muted; astar never decrypts.
//!
//! Audio plays on the default output device, or with `--wav` lands in an
//! 8 kHz s16 mono file via [`crate::wav_backend::WavBackend`], the same
//! writer the other listen commands use.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::AudioBackend;
use astar_console::P25Snapshot;
use astar_p25::wire::{Callsign, DEFAULT_PORT};
use astar_station::Station;

use crate::cli::P25_LISTEN_USAGE;
use crate::wav_backend::WavBackend;

/// The largest talkgroup a P25 call carries: 24 bits.
const TALKGROUP_MAX: u32 = 0x00FF_FFFF;
/// How often the control loop polls `Station::p25_state()`.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How often a quiet link reports its frame count.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// Bound on how long the command waits, after requesting the unlink, for the
/// state to clear before exiting.
const UNLINK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum Parsed {
    Help,
    Listen(ListenOptions),
}

#[derive(Debug)]
pub struct ListenOptions {
    pub host: String,
    pub port: u16,
    pub callsign: String,
    /// A label for the link line. Never sent.
    pub talkgroup: Option<u32>,
    pub wav: Option<PathBuf>,
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    match parse(args)? {
        Parsed::Help => {
            print!("{P25_LISTEN_USAGE}");
            Ok(())
        }
        Parsed::Listen(opts) => listen(&opts),
    }
}

/// Parse `p25-listen`'s arguments. Pure (no I/O).
///
/// `<host>` accepts `host` or `host:port`; `--port` sets it too. A `host:port`
/// and an explicit `--port` that disagree is an error, not a precedence rule.
pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut port_flag: Option<u16> = None;
    let mut callsign: Option<String> = None;
    let mut wav: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--port" => {
                let v = crate::cli::flag_value(&mut args, "--port")?;
                port_flag = Some(
                    v.parse()
                        .map_err(|_| format!("--port expects a 16-bit port number, got {v:?}"))?,
                );
            }
            "--callsign" => callsign = Some(crate::cli::flag_value(&mut args, "--callsign")?),
            "--wav" => wav = Some(PathBuf::from(crate::cli::flag_value(&mut args, "--wav")?)),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}\n\n{P25_LISTEN_USAGE}"));
            }
            _ => positional.push(arg),
        }
    }

    let mut positional = positional.into_iter();
    let raw_host = positional
        .next()
        .ok_or_else(|| format!("missing <host>\n\n{P25_LISTEN_USAGE}"))?;
    let talkgroup = positional
        .next()
        .map(|raw| {
            let tg: u32 = raw.parse().map_err(|_| {
                format!("[tg] expects a talkgroup number, 1-{TALKGROUP_MAX}, got {raw:?}")
            })?;
            if tg == 0 || tg > TALKGROUP_MAX {
                return Err(format!(
                    "[tg] must be 1-{TALKGROUP_MAX} (24 bits), got {tg}\n\n{P25_LISTEN_USAGE}"
                ));
            }
            Ok(tg)
        })
        .transpose()?;
    if let Some(extra) = positional.next() {
        return Err(format!(
            "unexpected argument {extra:?}\n\n{P25_LISTEN_USAGE}"
        ));
    }
    let (host, embedded_port) = split_host_port(&raw_host)?;

    let port = match (embedded_port, port_flag) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "<host> says port {a} but --port says {b} — pick one\n\n{P25_LISTEN_USAGE}"
            ));
        }
        (Some(p), _) | (None, Some(p)) => p,
        (None, None) => DEFAULT_PORT,
    };

    let callsign =
        callsign.ok_or_else(|| format!("missing --callsign (required)\n\n{P25_LISTEN_USAGE}"))?;
    Callsign::new(&callsign).map_err(|e| format!("--callsign {e}\n\n{P25_LISTEN_USAGE}"))?;

    Ok(Parsed::Listen(ListenOptions {
        host,
        port,
        callsign,
        talkgroup,
        wav,
    }))
}

/// Split `host` or `host:port`. Rejects an empty host, a port that is not a
/// non-zero 16-bit number, and more than one colon.
fn split_host_port(raw: &str) -> Result<(String, Option<u16>), String> {
    let parts: Vec<&str> = raw.split(':').collect();
    let (host, port) = match parts.as_slice() {
        [h] => (*h, None),
        [h, p] => {
            let parsed: u16 = p
                .parse()
                .map_err(|_| format!("<host> port must be a 16-bit number, got {p:?}"))?;
            if parsed == 0 {
                return Err("<host> port must not be zero".to_string());
            }
            (*h, Some(parsed))
        }
        _ => return Err(format!("<host> has more than one ':' — got {raw:?}")),
    };
    if host.is_empty() {
        return Err("<host> must not be empty".to_string());
    }
    Ok((host.to_string(), port))
}

/// Build a station playing to the default output device, or, with `--wav`,
/// one whose decoded audio lands in a file.
fn build_station(wav: Option<&Path>) -> Station {
    match wav {
        None => Station::new(crate::vocoder::listener_config()),
        Some(path) => {
            let path = path.to_path_buf();
            Station::with_backend_factory(
                crate::vocoder::listener_config(),
                Box::new(move || {
                    Box::new(WavBackend::new(path.clone(), "p25-listen")) as Box<dyn AudioBackend>
                }),
            )
        }
    }
}

/// Connect, print link/talker transitions until Ctrl-C, then unlink.
fn listen(opts: &ListenOptions) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let ctrlc_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        // A second Ctrl-C hard-exits, so an operator who hits it twice is
        // never stuck waiting on teardown.
        if ctrlc_stop.swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
    })
    .map_err(|e| format!("failed to install Ctrl-C handler: {e}"))?;
    listen_until(opts, &stop, &mut std::io::stdout())
}

/// The body of `listen`, with the stop flag and the output injected so a
/// test can drive it on loopback.
fn listen_until(
    opts: &ListenOptions,
    stop: &AtomicBool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let station = build_station(opts.wav.as_deref());
    let say = |out: &mut dyn Write, line: &str| {
        // A closed stdout is not a reason to leave the link up.
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };

    if let Some(path) = &opts.wav {
        say(out, &format!("writing decoded audio to {}", path.display()));
    }
    say(
        out,
        &format!(
            "connecting to {}:{} as {}…",
            opts.host, opts.port, opts.callsign
        ),
    );
    let target = format!("{}:{}", opts.host, opts.port);
    station
        .p25_connect(&target, &opts.callsign)
        .map_err(|e| format!("p25 connect failed: {e}"))?;
    say(
        out,
        "receive only — this command has no PTT. astar has no P25 transmit path.",
    );

    let mut tracker = PrintTracker::new(target, opts.talkgroup);
    let mut link_failed = false;
    // A quiet link says so every HEARTBEAT, with its frame count, so a silent
    // half hour reads as "linked, nobody talking" rather than "hung".
    let mut next_heartbeat = Instant::now() + HEARTBEAT;

    while !stop.load(Ordering::Relaxed) {
        let Some(state) = station.p25_state() else {
            break;
        };
        for line in tracker.on_snapshot(&state) {
            say(out, &line);
        }
        if Instant::now() >= next_heartbeat {
            next_heartbeat = Instant::now() + HEARTBEAT;
            if !state.receiving
                && let Some(line) = tracker.heartbeat(&state)
            {
                say(out, &line);
            }
        }
        if state.link_state == "failed" {
            link_failed = true;
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    say(out, "unlinking…");
    station.p25_disconnect();
    let deadline = Instant::now() + UNLINK_TIMEOUT;
    while station.p25_state().is_some() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    // Dropping the station finishes the WAV file, so do it before saying so.
    drop(station);
    say(out, "unlinked.");

    if link_failed {
        return Err("link failed".to_string());
    }
    Ok(())
}

/// Turns a stream of snapshots into the lines to print, once each.
#[allow(clippy::struct_excessive_bools)]
struct PrintTracker {
    target: String,
    talkgroup: Option<u32>,
    printed_link: bool,
    last_heard_id: Option<u32>,
    receiving: bool,
    /// Whether the current transmission's encrypted note has been printed.
    /// The algorithm id rides in a later record than the talker, so an over
    /// can be announced before it is known to be encrypted.
    printed_encrypted: bool,
    printed_frames: bool,
}

impl PrintTracker {
    fn new(target: String, talkgroup: Option<u32>) -> Self {
        Self {
            target,
            talkgroup,
            printed_link: false,
            last_heard_id: None,
            receiving: false,
            printed_encrypted: false,
            printed_frames: false,
        }
    }

    /// The periodic line for a quiet link. `None` before the link is up.
    fn heartbeat(&self, state: &P25Snapshot) -> Option<String> {
        if !self.printed_link {
            return None;
        }
        Some(format!(
            "still linked · {} voice record{} received so far",
            state.frames_rx,
            if state.frames_rx == 1 { "" } else { "s" }
        ))
    }

    fn on_snapshot(&mut self, state: &P25Snapshot) -> Vec<String> {
        let mut lines = Vec::new();

        if state.link_state == "failed" {
            lines.push("link failed".to_string());
            return lines;
        }

        if !self.printed_link && state.link_state == "linked" {
            self.printed_link = true;
            let backend = state.backend.unwrap_or("none");
            let label = self
                .talkgroup
                .map_or_else(String::new, |tg| format!(", TG {tg} (label only)"));
            lines.push(format!(
                "linked {} (backend: {backend}{label})",
                self.target
            ));
        }

        if !self.printed_frames && state.frames_rx > 0 {
            self.printed_frames = true;
            lines.push("frames arriving — the reflector is relaying to this link".to_string());
        }

        // Print on the start of a transmission, and on a talker change
        // mid-stream. Compared on the numeric id, the only identity P25
        // carries.
        if state.receiving && (!self.receiving || state.last_heard_id != self.last_heard_id) {
            self.last_heard_id = state.last_heard_id;
            self.printed_encrypted = false;
            if let Some(id) = &state.last_heard {
                let note = if state.encrypted {
                    self.printed_encrypted = true;
                    " (encrypted, muted)"
                } else {
                    ""
                };
                lines.push(format!("▶ {id}{note}"));
            }
        } else if state.receiving && state.encrypted && !self.printed_encrypted {
            self.printed_encrypted = true;
            lines.push("  (encrypted, muted)".to_string());
        }
        self.receiving = state.receiving;

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_codec::p25::ImbeEncoder;
    use astar_p25::Reflector;
    use astar_p25::ldu::{self, RECORDS, Superframe};
    use astar_p25::wire;
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::Mutex;

    fn opts(args: &[&str]) -> ListenOptions {
        match parse(args.iter().map(|s| (*s).to_string())).expect("parse") {
            Parsed::Listen(o) => o,
            Parsed::Help => panic!("expected options, got help"),
        }
    }

    fn err(args: &[&str]) -> String {
        parse(args.iter().map(|s| (*s).to_string())).expect_err("expected an error")
    }

    #[test]
    fn a_bare_host_takes_port_41000() {
        let o = opts(&["p25.example", "--callsign", "KC0ABC"]);
        assert_eq!(o.host, "p25.example");
        assert_eq!(o.port, 41_000);
    }

    #[test]
    fn an_explicit_port_wins() {
        assert_eq!(
            opts(&["p25.example:42011", "--callsign", "KC0ABC"]).port,
            42_011
        );
        assert_eq!(
            opts(&["p25.example", "--port", "42012", "--callsign", "KC0ABC"]).port,
            42_012
        );
    }

    #[test]
    fn a_host_port_that_contradicts_the_flag_is_refused() {
        let e = err(&[
            "p25.example:41001",
            "--port",
            "41000",
            "--callsign",
            "KC0ABC",
        ]);
        assert!(e.contains("41001") && e.contains("41000"), "{e}");
    }

    #[test]
    fn the_talkgroup_is_an_optional_label() {
        assert_eq!(opts(&["h", "--callsign", "KC0ABC"]).talkgroup, None);
        assert_eq!(
            opts(&["h", "10200", "--callsign", "KC0ABC"]).talkgroup,
            Some(10_200)
        );
        assert_eq!(
            opts(&["h", "--callsign", "KC0ABC", "10200"]).talkgroup,
            Some(10_200)
        );
    }

    #[test]
    fn a_zero_or_oversized_talkgroup_is_refused() {
        for tg in ["0", "16777216"] {
            let e = err(&["h", tg, "--callsign", "KC0ABC"]);
            assert!(e.contains("[tg]"), "{e}");
        }
        assert_eq!(
            opts(&["h", "16777215", "--callsign", "KC0ABC"]).talkgroup,
            Some(16_777_215)
        );
    }

    #[test]
    fn missing_callsign_is_named() {
        assert!(err(&["p25.example"]).contains("missing --callsign"));
    }

    #[test]
    fn an_over_long_callsign_is_refused_up_front() {
        assert!(err(&["h", "--callsign", "ABCDEFGHIJK"]).contains("--callsign"));
    }

    #[test]
    fn help_and_unknown_flags_short_circuit() {
        assert!(matches!(
            parse(["--help".to_string()].into_iter()),
            Ok(Parsed::Help)
        ));
        assert!(err(&["p25.example", "--nope"]).contains("unknown flag"));
    }

    #[test]
    fn wav_is_carried() {
        let o = opts(&["h", "--callsign", "KC0ABC", "--wav", "/tmp/out.wav"]);
        assert_eq!(o.wav.as_deref(), Some(Path::new("/tmp/out.wav")));
    }

    fn snap(link: &'static str) -> P25Snapshot {
        P25Snapshot {
            link_state: link,
            reflector: "x".into(),
            last_heard: None,
            last_heard_id: None,
            talkgroup: None,
            frames_rx: 0,
            receiving: false,
            encrypted: false,
            backend: Some("software"),
            ptt: false,
            tx_dbfs: -60.0,
            rx_dbfs: -60.0,
        }
    }

    #[test]
    fn the_link_line_names_the_backend_and_the_label_once() {
        let mut t = PrintTracker::new("p25.example:41000".into(), Some(10_200));
        assert_eq!(t.on_snapshot(&snap("linking")), Vec::<String>::new());
        let lines = t.on_snapshot(&snap("linked"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("p25.example:41000"), "{}", lines[0]);
        assert!(lines[0].contains("software"), "{}", lines[0]);
        assert!(lines[0].contains("10200"), "{}", lines[0]);
        assert_eq!(t.on_snapshot(&snap("linked")), Vec::<String>::new());
    }

    #[test]
    fn a_failed_link_says_so_and_nothing_else() {
        let mut t = PrintTracker::new("x".into(), None);
        assert_eq!(
            t.on_snapshot(&snap("failed")),
            vec!["link failed".to_string()]
        );
    }

    #[test]
    fn a_transmission_prints_its_talker_once() {
        let mut t = PrintTracker::new("x".into(), None);
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.receiving = true;
        s.last_heard = Some("7654321".into());
        s.last_heard_id = Some(7_654_321);
        assert_eq!(t.on_snapshot(&s), vec!["▶ 7654321".to_string()]);
        assert_eq!(t.on_snapshot(&s), Vec::<String>::new());
    }

    #[test]
    fn an_encrypted_transmission_says_it_is_muted() {
        let mut t = PrintTracker::new("x".into(), None);
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.receiving = true;
        s.encrypted = true;
        s.last_heard = Some("42".into());
        s.last_heard_id = Some(42);
        assert_eq!(
            t.on_snapshot(&s),
            vec!["▶ 42 (encrypted, muted)".to_string()]
        );
        assert_eq!(t.on_snapshot(&s), Vec::<String>::new());
    }

    #[test]
    fn an_over_found_encrypted_later_says_so_once() {
        let mut t = PrintTracker::new("x".into(), None);
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.receiving = true;
        s.last_heard = Some("42".into());
        s.last_heard_id = Some(42);
        assert_eq!(t.on_snapshot(&s), vec!["▶ 42".to_string()]);
        s.encrypted = true;
        assert_eq!(t.on_snapshot(&s).len(), 1);
        assert_eq!(t.on_snapshot(&s), Vec::<String>::new());
    }

    #[test]
    fn the_same_talker_keying_again_is_announced_again() {
        let mut t = PrintTracker::new("x".into(), None);
        let _ = t.on_snapshot(&snap("linked"));
        let mut on = snap("linked");
        on.receiving = true;
        on.last_heard = Some("42".into());
        on.last_heard_id = Some(42);
        assert_eq!(t.on_snapshot(&on).len(), 1);
        let mut off = on.clone();
        off.receiving = false;
        assert_eq!(t.on_snapshot(&off), Vec::<String>::new());
        assert_eq!(t.on_snapshot(&on), vec!["▶ 42".to_string()]);
    }

    /// A `Write` a test can read while the command still holds it.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// Signal power at `hz` in `pcm` (Goertzel).
    fn power_at(pcm: &[i16], hz: f64) -> f64 {
        let w = std::f64::consts::TAU * hz / 8000.0;
        let c = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &x in pcm {
            let s0 = f64::from(x) + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn p25_listen_hears_a_tone_fed_through_the_parrot_on_loopback() {
        let reflector =
            Reflector::bind_parrot("127.0.0.1:0".parse::<SocketAddr>().unwrap()).expect("bind");
        let addr = reflector.local_addr();
        let handle = reflector.run();

        let dir = std::env::temp_dir().join(format!("astar-p25-listen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("out.wav");

        let listen_opts = ListenOptions {
            host: addr.ip().to_string(),
            port: addr.port(),
            callsign: "KC0LST".into(),
            talkgroup: Some(10_200),
            wav: Some(wav.clone()),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let out = Shared::default();
        let t = {
            let (stop, mut out) = (Arc::clone(&stop), out.clone());
            thread::spawn(move || listen_until(&listen_opts, &stop, &mut out))
        };
        wait_for("the link line", || out.text().contains("linked 127.0.0.1"));

        // A second client, the talker.
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(&wire::poll(&Callsign::new("KC0TX").unwrap()), addr)
            .unwrap();
        wait_for("both clients registered", || handle.client_count() == 2);

        let mut enc = ImbeEncoder::new();
        let radians = std::f32::consts::TAU * 1000.0 / 8000.0;
        let amp = 32768.0 * 0.1 * std::f32::consts::SQRT_2;
        let mut n = 0usize;
        let info = Superframe {
            source_id: 7_654_321,
            talkgroup: 10_200,
            algid: 0x80,
            kid: 0,
        };
        let mut next = Instant::now();
        for _ in 0..5 {
            let mut frames = [[0u8; 11]; RECORDS];
            for f in &mut frames {
                let pcm: [i16; 160] = std::array::from_fn(|i| {
                    (amp * (radians * (n + i) as f32).sin()).round() as i16
                });
                n += 160;
                *f = enc.encode(&pcm);
            }
            for datagram in ldu::superframe(&info, &frames) {
                tx.send_to(&datagram, addr).unwrap();
                next += Duration::from_millis(20);
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        }
        tx.send_to(&wire::end_record(), addr).unwrap();

        thread::sleep(Duration::from_millis(1500));
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap().expect("listen_until");
        handle.shutdown();

        let text = out.text();
        assert!(text.contains("▶ 7654321"), "{text}");
        assert!(!text.contains("encrypted"), "{text}");

        let bytes = std::fs::read(&wav).expect("wav written");
        let pcm: Vec<i16> = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        assert!(pcm.len() >= 12_000, "only {} samples", pcm.len());
        let best = (500..=3500)
            .step_by(10)
            .map(|hz| (hz, power_at(&pcm, f64::from(hz))))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap()
            .0;
        assert!((940..=1060).contains(&best), "dominant {best} Hz\n{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
