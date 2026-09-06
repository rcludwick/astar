// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `ysf-listen` subcommand: link to a `YSFReflector` and decode the voice on
//! it. The hardware checkpoint for astar-e7b3 §2, in one command.
//!
//! **Receive only, and there is no PTT here at all** — not a stubbed one, not
//! a key that refuses. `dstar-listen` grew a stdin PTT reader because D-Star
//! can transmit; YSF cannot, so this command has no way to ask. See
//! `astar_console::ysf`'s module docs for why (the vendored deframer cannot
//! read a 49-bit channel response, so an encode would time out into a D-Star
//! null codeword — the wrong vocoder's noise on the air rather than silence),
//! and `iax-ysftx` in the backlog for the fix.
//!
//! **This is the thing an agent cannot run.** Everything under `astar-codec`
//! and `astar-console` proves bytes in and bytes out; whether the result is
//! intelligible speech needs a `ThumbDV` and a live reflector, and both
//! "point this at a real reflector" and "listen to what comes out" are the
//! operator's. What this command is for is making that checkpoint one line
//! instead of a UI walkthrough.
//!
//! Like `dstar-listen` it drives [`astar_station::Station`] rather than
//! reaching past it to the session: the facade already carries the
//! mutual-exclusion guards and the poll/snapshot contract.
//!
//! Audio plays on the default output device, or with `--wav` lands in an
//! 8 kHz s16 mono file via [`crate::wav_backend::WavBackend`] — the same
//! writer `dstar-listen` uses, so a capture from either is the same format.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::AudioBackend;
use astar_console::YsfSnapshot;
use astar_station::{Station, StationConfig};

use crate::cli::YSF_LISTEN_USAGE;
use crate::wav_backend::WavBackend;

/// The port the plurality of `YSFReflector`s listen on. A default for
/// convenience only — YSF standardises nothing here and the long tail is
/// real, so the directory's own port always wins where there is one.
const DEFAULT_PORT: u16 = 42_000;
/// How often the control loop polls `Station::ysf_state()`. Cheap
/// (atomics-backed) — no faster than a human needs to see a transition.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Bound on how long `listen` waits, after requesting the unlink, for the
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
    pub wav: Option<PathBuf>,
    /// The YCS room request (`set_options`). `None` for a plain reflector,
    /// which is what every `YSFReflector` is.
    pub options: Option<String>,
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    match parse(args)? {
        Parsed::Help => {
            print!("{YSF_LISTEN_USAGE}");
            Ok(())
        }
        Parsed::Listen(opts) => listen(&opts),
    }
}

/// Parse `ysf-listen`'s arguments. Pure (no I/O) so it is unit-testable
/// without a network stack, a dongle, or audio devices.
///
/// `<host>` accepts `host` or `host:port`; `--port` sets it too. A `host:port`
/// and an explicit `--port` that disagree is an error rather than a silent
/// precedence rule — the whole point of carrying a port on this network is
/// that guessing it wrong links you to nothing.
pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut port_flag: Option<u16> = None;
    let mut callsign: Option<String> = None;
    let mut wav: Option<PathBuf> = None;
    let mut options: Option<String> = None;
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
            "--options" => options = Some(crate::cli::flag_value(&mut args, "--options")?),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}\n\n{YSF_LISTEN_USAGE}"));
            }
            _ => positional.push(arg),
        }
    }

    let raw_host = positional
        .into_iter()
        .next()
        .ok_or_else(|| format!("missing <host>\n\n{YSF_LISTEN_USAGE}"))?;
    let (host, embedded_port) = split_host_port(&raw_host)?;

    let port = match (embedded_port, port_flag) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "<host> says port {a} but --port says {b} — pick one\n\n{YSF_LISTEN_USAGE}"
            ));
        }
        (Some(p), _) | (None, Some(p)) => p,
        (None, None) => DEFAULT_PORT,
    };

    let callsign =
        callsign.ok_or_else(|| format!("missing --callsign (required)\n\n{YSF_LISTEN_USAGE}"))?;
    if callsign.is_empty() {
        return Err(format!(
            "--callsign must not be empty\n\n{YSF_LISTEN_USAGE}"
        ));
    }

    Ok(Parsed::Listen(ListenOptions {
        host,
        port,
        callsign,
        wav,
        options,
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
        None => Station::new(StationConfig::default()),
        Some(path) => {
            let path = path.to_path_buf();
            Station::with_backend_factory(
                StationConfig::default(),
                Box::new(move || {
                    Box::new(WavBackend::new(path.clone(), "ysf-listen")) as Box<dyn AudioBackend>
                }),
            )
        }
    }
}

/// Connect, print link/talker transitions until Ctrl-C, then unlink.
fn listen(opts: &ListenOptions) -> Result<(), String> {
    let station = build_station(opts.wav.as_deref());

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

    if let Some(path) = &opts.wav {
        println!("writing decoded audio to {}", path.display());
    }
    println!(
        "connecting to {}:{} as {}…",
        opts.host, opts.port, opts.callsign
    );
    let target = format!("{}:{}", opts.host, opts.port);
    station
        .ysf_connect(&target, &opts.callsign, opts.options.as_deref())
        .map_err(|e| format!("ysf connect failed: {e}"))?;
    println!("receive only — this command has no PTT and astar has no YSF transmit path.");

    let mut tracker = PrintTracker::new(target.clone());
    let mut link_failed = false;

    while !stop.load(Ordering::Relaxed) {
        let Some(state) = station.ysf_state() else {
            // The link tore itself down (a fatal socket error) — nothing
            // left to poll.
            break;
        };
        for line in tracker.on_snapshot(&state) {
            println!("{line}");
        }
        if state.link_state == "failed" {
            link_failed = true;
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    println!("unlinking…");
    station.ysf_disconnect();
    let deadline = Instant::now() + UNLINK_TIMEOUT;
    while station.ysf_state().is_some() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    println!("unlinked.");

    if link_failed {
        return Err("link failed".to_string());
    }
    Ok(())
}

/// Turns a stream of snapshots into the lines to print, once each.
///
/// Pure and separately tested, because the interesting behaviour here is
/// exactly "what is said, and how often" — and the one line that matters most
/// is the one an operator would otherwise never get: a reflector sending a
/// mode astar cannot decode looks identical to a broken dongle without it.
struct PrintTracker {
    target: String,
    printed_link: bool,
    last_heard: Option<String>,
    receiving: bool,
    reported_unsupported: Option<&'static str>,
}

impl PrintTracker {
    fn new(target: String) -> Self {
        Self {
            target,
            printed_link: false,
            last_heard: None,
            receiving: false,
            reported_unsupported: None,
        }
    }

    fn on_snapshot(&mut self, state: &YsfSnapshot) -> Vec<String> {
        let mut lines = Vec::new();

        if state.link_state == "failed" {
            lines.push("link failed".to_string());
            return lines;
        }

        if !self.printed_link && state.link_state == "linked" {
            self.printed_link = true;
            // A link opened with audio always has a backend; naming it
            // proves the dongle is the thing decoding, which is the whole
            // point of running this.
            let backend = state.backend.unwrap_or("none");
            lines.push(format!("linked {} (backend: {backend})", self.target));
        }

        // The refusal, once per mode. Louder than the frames that caused it:
        // a link that says `linked` with a climbing frame counter and no
        // sound is otherwise indistinguishable from broken hardware.
        if state.unsupported_mode != self.reported_unsupported
            && let Some(mode) = state.unsupported_mode
        {
            self.reported_unsupported = Some(mode);
            lines.push(match mode {
                "voice-fr" => "!! this reflector is sending VW (full-rate voice); astar decodes \
                               DN only — no audio will be produced"
                    .to_string(),
                "data-fr" => {
                    "!! this reflector is sending data frames, which carry no voice".to_string()
                }
                other => format!("!! unsupported frame mode {other}; astar decodes DN only"),
            });
        }

        // Transmissions. Print on the start of one, and on a talker change
        // mid-stream (a reflector can hand straight from one to the next).
        if state.receiving && (!self.receiving || state.last_heard != self.last_heard) {
            self.last_heard.clone_from(&state.last_heard);
            if let Some(cs) = &self.last_heard {
                lines.push(format!("▶ {cs}"));
            }
        }
        self.receiving = state.receiving;

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parses_a_host_and_callsign() {
        let o = opts(&["ysf.example", "--callsign", "AJ7HR"]);
        assert_eq!(o.host, "ysf.example");
        assert_eq!(o.port, DEFAULT_PORT);
        assert_eq!(o.callsign, "AJ7HR");
        assert!(o.wav.is_none());
        assert!(o.options.is_none());
    }

    /// The port is the field this network gets wrong if it is assumed, so it
    /// can be written the way a directory prints it.
    #[test]
    fn a_port_can_ride_on_the_host() {
        assert_eq!(
            opts(&["ysf.example:42003", "--callsign", "N0CALL"]).port,
            42003
        );
        assert_eq!(
            opts(&["ysf.example", "--port", "42001", "--callsign", "N0CALL"]).port,
            42001
        );
    }

    /// Two ports that disagree is a mistake worth stopping for, not a
    /// precedence rule to memorise — the wrong one links to nothing.
    #[test]
    fn a_host_port_that_contradicts_the_flag_is_refused() {
        let e = err(&[
            "ysf.example:42003",
            "--port",
            "42000",
            "--callsign",
            "N0CALL",
        ]);
        assert!(e.contains("42003") && e.contains("42000"), "{e}");
    }

    #[test]
    fn agreeing_ports_are_fine() {
        assert_eq!(
            opts(&[
                "ysf.example:42000",
                "--port",
                "42000",
                "--callsign",
                "N0CALL"
            ])
            .port,
            42000
        );
    }

    #[test]
    fn a_bare_ipv4_address_parses() {
        let o = opts(&["45.56.69.219:42001", "--callsign", "N0CALL"]);
        assert_eq!(o.host, "45.56.69.219");
        assert_eq!(o.port, 42001);
    }

    #[test]
    fn wav_and_options_are_carried() {
        let o = opts(&[
            "ysf.example",
            "--callsign",
            "N0CALL",
            "--wav",
            "/tmp/out.wav",
            "--options",
            "ROOM1",
        ]);
        assert_eq!(o.wav.as_deref(), Some(Path::new("/tmp/out.wav")));
        assert_eq!(o.options.as_deref(), Some("ROOM1"));
    }

    #[test]
    fn missing_pieces_are_named() {
        assert!(err(&["--callsign", "N0CALL"]).contains("missing <host>"));
        assert!(err(&["ysf.example"]).contains("missing --callsign"));
        assert!(err(&["ysf.example", "--callsign", ""]).contains("must not be empty"));
    }

    #[test]
    fn malformed_hosts_and_ports_are_refused() {
        assert!(err(&[":42000", "--callsign", "N0CALL"]).contains("must not be empty"));
        assert!(err(&["ysf.example:0", "--callsign", "N0CALL"]).contains("zero"));
        assert!(err(&["ysf.example:abc", "--callsign", "N0CALL"]).contains("16-bit"));
        assert!(err(&["a:1:2", "--callsign", "N0CALL"]).contains("more than one"));
        assert!(err(&["ysf.example", "--port", "nope", "--callsign", "N0CALL"]).contains("--port"));
    }

    #[test]
    fn help_and_unknown_flags_short_circuit() {
        assert!(matches!(
            parse(["--help".to_string()].into_iter()).expect("help"),
            Parsed::Help
        ));
        assert!(err(&["ysf.example", "--nope"]).contains("unknown flag"));
    }

    // ---- what the operator is told ----

    fn snap(link: &'static str) -> YsfSnapshot {
        YsfSnapshot {
            link_state: link,
            last_heard: None,
            frames_rx: 0,
            receiving: false,
            unsupported_mode: None,
            backend: Some("thumbdv"),
        }
    }

    #[test]
    fn the_link_line_is_printed_once_and_names_the_backend() {
        let mut t = PrintTracker::new("ysf.example:42000".into());
        assert!(t.on_snapshot(&snap("linking")).is_empty());
        let lines = t.on_snapshot(&snap("linked"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("ysf.example:42000"), "{:?}", lines[0]);
        assert!(lines[0].contains("thumbdv"), "{:?}", lines[0]);
        assert!(
            t.on_snapshot(&snap("linked")).is_empty(),
            "a steady link must not repeat itself every 50 ms"
        );
    }

    #[test]
    fn a_failed_link_says_so_and_says_nothing_else() {
        let mut t = PrintTracker::new("ysf.example:42000".into());
        let lines = t.on_snapshot(&snap("failed"));
        assert_eq!(lines, vec!["link failed".to_string()]);
    }

    /// The line this whole command exists to make possible. Without it, a
    /// reflector carrying VW is a linked session with a climbing frame count
    /// and no sound — which reads as broken hardware.
    #[test]
    fn vw_is_reported_once_and_explains_itself() {
        let mut t = PrintTracker::new("ysf.example:42000".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.unsupported_mode = Some("voice-fr");
        let lines = t.on_snapshot(&s);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("VW"), "{:?}", lines[0]);
        assert!(lines[0].contains("DN only"), "{:?}", lines[0]);
        assert!(
            t.on_snapshot(&s).is_empty(),
            "the warning must not repeat every poll"
        );
    }

    #[test]
    fn a_data_frame_gets_its_own_sentence() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.unsupported_mode = Some("data-fr");
        let lines = t.on_snapshot(&s);
        assert!(lines[0].contains("no voice"), "{:?}", lines[0]);
    }

    /// A mode string this build does not recognise still gets named rather
    /// than swallowed — a newer engine talking to an older CLI.
    #[test]
    fn an_unknown_mode_is_still_reported() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.unsupported_mode = Some("something-new");
        assert!(t.on_snapshot(&s)[0].contains("something-new"));
    }

    #[test]
    fn a_transmission_prints_its_talker_once() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.receiving = true;
        s.last_heard = Some("AJ7HR".into());
        assert_eq!(t.on_snapshot(&s), vec!["▶ AJ7HR".to_string()]);
        assert!(
            t.on_snapshot(&s).is_empty(),
            "a transmission in progress must not reprint every poll"
        );
    }

    /// `last_heard` persists past end-of-transmission by design, so the next
    /// over by the SAME operator must still announce itself — keying on the
    /// callsign alone would silently swallow it.
    #[test]
    fn the_same_talker_keying_again_is_announced_again() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut on = snap("linked");
        on.receiving = true;
        on.last_heard = Some("AJ7HR".into());
        assert_eq!(t.on_snapshot(&on).len(), 1);

        let mut off = snap("linked");
        off.receiving = false;
        off.last_heard = Some("AJ7HR".into()); // persists — this is "last heard"
        assert!(t.on_snapshot(&off).is_empty());

        assert_eq!(t.on_snapshot(&on), vec!["▶ AJ7HR".to_string()]);
    }

    /// A reflector can hand straight from one talker to the next without a
    /// gap; that must not read as one long over by the first.
    #[test]
    fn a_talker_change_mid_stream_is_announced() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut a = snap("linked");
        a.receiving = true;
        a.last_heard = Some("AJ7HR".into());
        assert_eq!(t.on_snapshot(&a).len(), 1);
        let mut b = snap("linked");
        b.receiving = true;
        b.last_heard = Some("W1AW".into());
        assert_eq!(t.on_snapshot(&b), vec!["▶ W1AW".to_string()]);
    }
}
