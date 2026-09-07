// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `nxdn-listen` subcommand: link to an `NXDNReflector` talkgroup and decode
//! the voice on it. The hardware checkpoint for iax-b9c2, in one command.
//!
//! **Receive only, and there is no PTT here at all** — not a stubbed one, not
//! a key that refuses. `astar` has no NXDN transmit path yet (that is Task 9
//! of `docs/superpowers/plans/2026-09-07-nxdn-network.md`, fenced until Rob
//! confirms a clean YSF parrot round trip on the fixed build), so this
//! command has no way to ask; `Station::set_ptt` already refuses a key-down
//! against a live NXDN link, but that guard is defence in depth, not
//! something this command relies on by omitting a reader instead.
//!
//! **This is the thing an agent cannot run.** Everything under `astar-nxdn`,
//! `astar-codec` and `astar-console` proves bytes in and bytes out; whether
//! the result is intelligible speech needs a `ThumbDV` and a live reflector,
//! and both "point this at a real reflector" and "listen to what comes out"
//! are the operator's. What this command is for is making that checkpoint
//! one line instead of a UI walkthrough.
//!
//! Like `ysf-listen` it drives [`astar_station::Station`] rather than
//! reaching past it to the session: the facade already carries the
//! mutual-exclusion guards and the poll/snapshot contract.
//!
//! NXDN addresses stations by NUMBER, not callsign: `radio_id` is this
//! station's registration and `talkgroup` is the TG to join. A talker on the
//! wire is reported the same way — as its numeric id — because turning that
//! number into a callsign needs a directory astar does not hold; see
//! `astar_console::nxdn`'s `NxdnSnapshot::last_heard` doc for why inventing
//! one here would be a guess presented as identification.
//!
//! Audio plays on the default output device, or with `--wav` lands in an
//! 8 kHz s16 mono file via [`crate::wav_backend::WavBackend`] — the same
//! writer `ysf-listen` and `dstar-listen` use, so a capture from any of the
//! three is the same format.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::AudioBackend;
use astar_console::NxdnSnapshot;
use astar_station::{Station, StationConfig};

use crate::cli::NXDN_LISTEN_USAGE;
use crate::wav_backend::WavBackend;

/// The port the large majority of the directory's NXDN rows publish. A
/// default for convenience only — the directory's own port always wins
/// where there is one.
const DEFAULT_PORT: u16 = 41_400;
/// How often the control loop polls `Station::nxdn_state()`. Cheap
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
    pub radio_id: u16,
    pub talkgroup: u16,
    pub wav: Option<PathBuf>,
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    match parse(args)? {
        Parsed::Help => {
            print!("{NXDN_LISTEN_USAGE}");
            Ok(())
        }
        Parsed::Listen(opts) => listen(&opts),
    }
}

/// Parse `nxdn-listen`'s arguments. Pure (no I/O) so it is unit-testable
/// without a network stack, a dongle, or audio devices.
///
/// `<host>` accepts `host` or `host:port`; `--port` sets it too. A `host:port`
/// and an explicit `--port` that disagree is an error rather than a silent
/// precedence rule — the whole point of carrying a port on this network is
/// that guessing it wrong links you to nothing.
///
/// `--radio-id` and `--tg` are both required, unlike `ysf-listen`'s optional
/// room: `NXDNReflector.cpp` registers a client only when the poll's TG
/// matches its own, so a guessed or absent talkgroup links to somebody
/// else's room or to nothing at all — worth stopping for, not defaulting.
pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut port_flag: Option<u16> = None;
    let mut callsign: Option<String> = None;
    let mut radio_id: Option<u16> = None;
    let mut talkgroup: Option<u16> = None;
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
            "--radio-id" => {
                let v = crate::cli::flag_value(&mut args, "--radio-id")?;
                radio_id = Some(v.parse().map_err(|_| {
                    format!("--radio-id expects a 16-bit NXDN id (1-65535), got {v:?}")
                })?);
            }
            "--tg" => {
                let v = crate::cli::flag_value(&mut args, "--tg")?;
                talkgroup =
                    Some(v.parse().map_err(|_| {
                        format!("--tg expects a 16-bit talkgroup number, got {v:?}")
                    })?);
            }
            "--wav" => wav = Some(PathBuf::from(crate::cli::flag_value(&mut args, "--wav")?)),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}\n\n{NXDN_LISTEN_USAGE}"));
            }
            _ => positional.push(arg),
        }
    }

    let raw_host = positional
        .into_iter()
        .next()
        .ok_or_else(|| format!("missing <host>\n\n{NXDN_LISTEN_USAGE}"))?;
    let (host, embedded_port) = split_host_port(&raw_host)?;

    let port = match (embedded_port, port_flag) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "<host> says port {a} but --port says {b} — pick one\n\n{NXDN_LISTEN_USAGE}"
            ));
        }
        (Some(p), _) | (None, Some(p)) => p,
        (None, None) => DEFAULT_PORT,
    };

    let callsign =
        callsign.ok_or_else(|| format!("missing --callsign (required)\n\n{NXDN_LISTEN_USAGE}"))?;
    if callsign.is_empty() {
        return Err(format!(
            "--callsign must not be empty\n\n{NXDN_LISTEN_USAGE}"
        ));
    }

    let radio_id =
        radio_id.ok_or_else(|| format!("missing --radio-id (required)\n\n{NXDN_LISTEN_USAGE}"))?;
    if radio_id == 0 {
        return Err(format!(
            "--radio-id must not be 0 — NXDN addresses stations by number, and 0 is not a \
             registration\n\n{NXDN_LISTEN_USAGE}"
        ));
    }

    let talkgroup =
        talkgroup.ok_or_else(|| format!("missing --tg (required)\n\n{NXDN_LISTEN_USAGE}"))?;
    if talkgroup == 0 {
        return Err(format!(
            "--tg must not be 0 — a reflector answers only polls carrying its own \
             talkgroup\n\n{NXDN_LISTEN_USAGE}"
        ));
    }

    Ok(Parsed::Listen(ListenOptions {
        host,
        port,
        callsign,
        radio_id,
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
        None => Station::new(StationConfig::default()),
        Some(path) => {
            let path = path.to_path_buf();
            Station::with_backend_factory(
                StationConfig::default(),
                Box::new(move || {
                    Box::new(WavBackend::new(path.clone(), "nxdn-listen")) as Box<dyn AudioBackend>
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
        "connecting to {}:{} as {} (radio {}, TG {})…",
        opts.host, opts.port, opts.callsign, opts.radio_id, opts.talkgroup
    );
    let target = format!("{}:{}", opts.host, opts.port);
    station
        .nxdn_connect(&target, &opts.callsign, opts.radio_id, opts.talkgroup)
        .map_err(|e| format!("nxdn connect failed: {e}"))?;
    println!(
        "receive only — this command has no PTT. astar has no NXDN transmit path yet; keying \
         is not available on any platform for this network."
    );

    let mut tracker = PrintTracker::new(target.clone());
    let mut link_failed = false;

    while !stop.load(Ordering::Relaxed) {
        let Some(state) = station.nxdn_state() else {
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
    station.nxdn_disconnect();
    let deadline = Instant::now() + UNLINK_TIMEOUT;
    while station.nxdn_state().is_some() && Instant::now() < deadline {
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
/// Pure and separately tested, exactly as `ysf_listen`'s `PrintTracker` is —
/// the interesting behaviour is "what is said, and how often", not the I/O
/// around it.
struct PrintTracker {
    target: String,
    printed_link: bool,
    last_heard: Option<String>,
    last_heard_id: Option<u16>,
    receiving: bool,
}

impl PrintTracker {
    fn new(target: String) -> Self {
        Self {
            target,
            printed_link: false,
            last_heard: None,
            last_heard_id: None,
            receiving: false,
        }
    }

    fn on_snapshot(&mut self, state: &NxdnSnapshot) -> Vec<String> {
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

        // Transmissions. Print on the start of one, and on a talker change
        // mid-stream (a reflector can hand straight from one to the next).
        // Compared on the numeric id, not the formatted string, because that
        // id — not a callsign — is the only identity NXDN carries on the
        // wire; see this module's doc.
        if state.receiving && (!self.receiving || state.last_heard_id != self.last_heard_id) {
            self.last_heard_id = state.last_heard_id;
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

    // 41400 is the port the large majority of the directory's 297 NXDN
    // rows publish. A default for convenience only — the directory's own
    // port always wins where there is one.
    #[test]
    fn a_bare_host_takes_the_default_port() {
        let Parsed::Listen(o) = parse(
            [
                "nxdn.example",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "4242",
                "--tg",
                "31313",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("parse") else {
            panic!("expected listen")
        };
        assert_eq!(o.host, "nxdn.example");
        assert_eq!(o.port, 41_400);
        assert_eq!(o.radio_id, 4242);
        assert_eq!(o.talkgroup, 31313);
    }

    #[test]
    fn an_explicit_port_wins() {
        let Parsed::Listen(o) = parse(
            [
                "nxdn.example:41401",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "4242",
                "--tg",
                "100",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("parse") else {
            panic!("expected listen")
        };
        assert_eq!(o.port, 41_401);
    }

    // NXDNReflector.cpp registers a client only when the poll's TG matches
    // its own; guessing one links to somebody else's room or to nothing at
    // all.
    #[test]
    fn a_missing_talkgroup_is_an_error_not_a_guess() {
        assert!(
            parse(
                ["nxdn.example", "--callsign", "KC0ABC", "--radio-id", "4242"]
                    .into_iter()
                    .map(String::from)
            )
            .is_err()
        );
    }

    #[test]
    fn a_radio_id_that_does_not_fit_sixteen_bits_is_an_error() {
        assert!(
            parse(
                [
                    "nxdn.example",
                    "--callsign",
                    "KC0ABC",
                    "--radio-id",
                    "3153591",
                    "--tg",
                    "100",
                ]
                .into_iter()
                .map(String::from)
            )
            .is_err()
        );
    }

    #[test]
    fn help_is_help() {
        assert!(matches!(
            parse(["--help".to_string()].into_iter()),
            Ok(Parsed::Help)
        ));
    }

    #[test]
    fn a_zero_radio_id_is_refused() {
        let e = err(&[
            "nxdn.example",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "0",
            "--tg",
            "100",
        ]);
        assert!(e.contains("--radio-id"), "{e}");
    }

    #[test]
    fn a_zero_talkgroup_is_refused() {
        let e = err(&[
            "nxdn.example",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "4242",
            "--tg",
            "0",
        ]);
        assert!(e.contains("--tg"), "{e}");
    }

    #[test]
    fn missing_callsign_is_named() {
        assert!(
            err(&["nxdn.example", "--radio-id", "4242", "--tg", "100"])
                .contains("missing --callsign")
        );
    }

    #[test]
    fn missing_radio_id_is_named() {
        assert!(
            err(&["nxdn.example", "--callsign", "KC0ABC", "--tg", "100"])
                .contains("missing --radio-id")
        );
    }

    #[test]
    fn wav_is_carried() {
        let o = opts(&[
            "nxdn.example",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "4242",
            "--tg",
            "100",
            "--wav",
            "/tmp/out.wav",
        ]);
        assert_eq!(o.wav.as_deref(), Some(Path::new("/tmp/out.wav")));
    }

    #[test]
    fn a_host_port_that_contradicts_the_flag_is_refused() {
        let e = err(&[
            "nxdn.example:41401",
            "--port",
            "41400",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "4242",
            "--tg",
            "100",
        ]);
        assert!(e.contains("41401") && e.contains("41400"), "{e}");
    }

    #[test]
    fn help_and_unknown_flags_short_circuit() {
        assert!(err(&["nxdn.example", "--nope"]).contains("unknown flag"));
    }

    // ---- what the operator is told ----

    fn snap(link: &'static str) -> NxdnSnapshot {
        NxdnSnapshot {
            link_state: link,
            last_heard: None,
            last_heard_id: None,
            frames_rx: 0,
            receiving: false,
            backend: Some("thumbdv"),
            ptt: false,
            tx_dbfs: -60.0,
            rx_dbfs: -60.0,
        }
    }

    #[test]
    fn the_link_line_is_printed_once_and_names_the_backend() {
        let mut t = PrintTracker::new("nxdn.example:41400".into());
        assert!(t.on_snapshot(&snap("linking")).is_empty());
        let lines = t.on_snapshot(&snap("linked"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("nxdn.example:41400"), "{:?}", lines[0]);
        assert!(lines[0].contains("thumbdv"), "{:?}", lines[0]);
        assert!(
            t.on_snapshot(&snap("linked")).is_empty(),
            "a steady link must not repeat itself every 50 ms"
        );
    }

    #[test]
    fn a_failed_link_says_so_and_says_nothing_else() {
        let mut t = PrintTracker::new("nxdn.example:41400".into());
        let lines = t.on_snapshot(&snap("failed"));
        assert_eq!(lines, vec!["link failed".to_string()]);
    }

    #[test]
    fn a_transmission_prints_its_talker_once() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.receiving = true;
        s.last_heard = Some("4242".into());
        s.last_heard_id = Some(4242);
        assert_eq!(t.on_snapshot(&s), vec!["▶ 4242".to_string()]);
        assert!(
            t.on_snapshot(&s).is_empty(),
            "a transmission in progress must not reprint every poll"
        );
    }

    /// `last_heard`/`last_heard_id` persist past end-of-transmission by
    /// design, so the next over by the SAME id must still announce itself —
    /// keying on the id alone would silently swallow it.
    #[test]
    fn the_same_talker_keying_again_is_announced_again() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut on = snap("linked");
        on.receiving = true;
        on.last_heard = Some("4242".into());
        on.last_heard_id = Some(4242);
        assert_eq!(t.on_snapshot(&on).len(), 1);

        let mut off = snap("linked");
        off.receiving = false;
        off.last_heard = Some("4242".into()); // persists — this is "last heard"
        off.last_heard_id = Some(4242);
        assert!(t.on_snapshot(&off).is_empty());

        assert_eq!(t.on_snapshot(&on), vec!["▶ 4242".to_string()]);
    }

    /// A reflector can hand straight from one talker to the next without a
    /// gap; that must not read as one long over by the first.
    #[test]
    fn a_talker_change_mid_stream_is_announced() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut a = snap("linked");
        a.receiving = true;
        a.last_heard = Some("4242".into());
        a.last_heard_id = Some(4242);
        assert_eq!(t.on_snapshot(&a).len(), 1);
        let mut b = snap("linked");
        b.receiving = true;
        b.last_heard = Some("100".into());
        b.last_heard_id = Some(100);
        assert_eq!(t.on_snapshot(&b), vec!["▶ 100".to_string()]);
    }
}
