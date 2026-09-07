// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `dmr-listen` subcommand: log in to a DMR master, join a talkgroup on a
//! timeslot, and decode the voice on it. The hardware checkpoint for
//! iax-d4f7, in one command.
//!
//! **Receive only, and there is no PTT here at all** — not a stubbed one, not
//! a key that refuses. astar has no DMR transmit path yet (that is Task 12 of
//! `docs/superpowers/plans/2026-09-07-dmr-network.md`, fenced until Rob
//! confirms a clean YSF parrot round trip on the fixed build), so this
//! command has no way to ask; `Station::set_ptt` already refuses a key-down
//! against a live DMR link, but that guard is defence in depth, not something
//! this command relies on by omitting a reader instead.
//!
//! **This is the thing an agent cannot run.** Everything under `astar-dmr`,
//! `astar-codec` and `astar-console` proves bytes in and bytes out; whether
//! the result is intelligible speech needs a `ThumbDV` and a live master, and
//! both "point this at a real master" and "listen to what comes out" are the
//! operator's. What this command is for is making that checkpoint one line
//! instead of a UI walkthrough. `just dmr-parrot` is the safe first target:
//! a real master, on 127.0.0.1, that nobody else can hear.
//!
//! # The password comes from the environment, never from `argv`
//!
//! There is no `--password` flag and there will not be one. Every process on
//! the machine can read another's command line, and a shell keeps it in
//! history; `ASTAR_DMR_PASSWORD` is neither. It is read once in `main`,
//! passed to [`run`] **by value**, moved on into
//! [`astar_station::Station::dmr_connect`] which moves it into the link's
//! FSM, spent on one login digest and dropped. This module keeps no copy, and
//! nothing here ever prints it — not on success, not in an error, not in the
//! "connecting to …" line.
//!
//! Like `ysf-listen` and `nxdn-listen` it drives [`astar_station::Station`]
//! rather than reaching past it to the session: the facade already carries
//! the mutual-exclusion guards, the consent gate and the poll/snapshot
//! contract.
//!
//! DMR addresses stations by NUMBER, not callsign: `radio_id` is this
//! station's radioid.net registration and `talkgroup` is the room to join, on
//! `timeslot`. A talker on the wire is reported the same way — as its numeric
//! id — because a `DMRD` carries `srcId` and no callsign at all; see
//! `astar_console::dmr`'s `DmrSnapshot::last_heard` for why inventing one
//! here would be a guess presented as identification.
//!
//! Audio plays on the default output device, or with `--wav` lands in an
//! 8 kHz s16 mono file via [`crate::wav_backend::WavBackend`] — the same
//! writer `ysf-listen`, `nxdn-listen` and `dstar-listen` use, so a capture
//! from any of the four is the same format.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::AudioBackend;
use astar_console::DmrSnapshot;
use astar_station::{Station, StationConfig};

use crate::cli::DMR_LISTEN_USAGE;
use crate::wav_backend::WavBackend;

/// The homebrew master port every MMDVM client already defaults to. A default
/// for convenience only — the network's own published port always wins where
/// there is one, and no hostname is compiled in beside it.
const DEFAULT_PORT: u16 = 62_031;
/// The slot hotspot-class connections conventionally carry network traffic
/// on. A convention, not a specification (`dmr-wire.md` §10) — which is why
/// it is a default an operator can override and not a constant.
const DEFAULT_TIMESLOT: u8 = 2;
/// The largest value a radio ID can take: 24 bits. `astar_dmr::RadioId` is the
/// one definition of this rule and `Station::dmr_connect` asks it on every
/// connect; this restates the bound only to fail before an audio device or a
/// dongle is opened, with a message naming the flag.
const RADIO_ID_MAX: u32 = 0x00FF_FFFF;
/// The environment variable the master password is read from. Named here so
/// the reader in `main` and the usage text cannot drift apart.
pub const PASSWORD_ENV: &str = "ASTAR_DMR_PASSWORD";
/// How often the control loop polls `Station::dmr_state()`. Cheap
/// (atomics-backed) — no faster than a human needs to see a transition.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Bound on how long `listen` waits, after requesting the logout, for the
/// state to clear before exiting.
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum Parsed {
    Help,
    Listen(ListenOptions),
}

#[derive(Debug)]
pub struct ListenOptions {
    pub system: String,
    pub host: String,
    pub port: u16,
    pub radio_id: u32,
    pub callsign: String,
    pub talkgroup: u32,
    pub timeslot: u8,
    pub wav: Option<PathBuf>,
}

/// Run the subcommand. `password` is taken **by value** and is the master's,
/// read from [`PASSWORD_ENV`] by the caller — see this module's doc for why
/// it is not an argument. An empty one is refused here rather than sent, so
/// the operator is told what is missing instead of watching an auth failure.
///
/// # Errors
/// A usage error, a missing password, or a failed link.
pub fn run(args: impl Iterator<Item = String>, password: String) -> Result<(), String> {
    match parse(args)? {
        // `--help` must work with no password set: it is how an operator
        // finds out that the password comes from the environment.
        Parsed::Help => {
            print!("{DMR_LISTEN_USAGE}");
            Ok(())
        }
        Parsed::Listen(opts) => {
            if password.is_empty() {
                return Err(format!(
                    "{PASSWORD_ENV} is unset or empty. A DMR master needs its network's \
                     password, and there is no --password flag: a secret in argv is readable \
                     by every process on the machine and lands in shell history. Set \
                     {PASSWORD_ENV} in the environment of this one command."
                ));
            }
            listen(&opts, password)
        }
    }
}

/// Parse `dmr-listen`'s arguments. Pure (no I/O, and no environment read) so
/// it is unit-testable without a network stack, a dongle, or audio devices.
///
/// `<host>` accepts `host` or `host:port`; `--port` sets it too. A `host:port`
/// and an explicit `--port` that disagree is an error rather than a silent
/// precedence rule.
///
/// `--system`, `--callsign`, `--radio-id` and `--tg` are all required. A
/// talkgroup number names nothing on its own — TG 91 is a different room on
/// every one of these networks — so a guessed system links to somebody else's
/// room under the operator's own registered ID, which is the one mistake here
/// that is attributable to a licence. `--ts` defaults to 2 because that
/// default is a convention with no consequence when wrong: the master simply
/// has nothing to send.
pub fn parse(args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let Some(raw) = scan(args)? else {
        return Ok(Parsed::Help);
    };
    let (host, embedded_port) = split_host_port(&raw.host)?;

    let port = match (embedded_port, raw.port) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "<host> says port {a} but --port says {b} — pick one\n\n{DMR_LISTEN_USAGE}"
            ));
        }
        (Some(p), _) | (None, Some(p)) => p,
        (None, None) => DEFAULT_PORT,
    };

    // The network is part of the address, not a label on it. Passed through
    // verbatim: `Station::dmr_connect` resolves it against
    // `astar_dmr::DmrNetwork` and lists what this build knows when it cannot,
    // which is one table rather than two that drift.
    let system = raw
        .system
        .ok_or_else(|| format!("missing --system (required)\n\n{DMR_LISTEN_USAGE}"))?;
    if system.is_empty() {
        return Err(format!("--system must not be empty\n\n{DMR_LISTEN_USAGE}"));
    }

    let callsign = raw
        .callsign
        .ok_or_else(|| format!("missing --callsign (required)\n\n{DMR_LISTEN_USAGE}"))?;
    if callsign.is_empty() {
        return Err(format!(
            "--callsign must not be empty\n\n{DMR_LISTEN_USAGE}"
        ));
    }

    let radio_id = raw
        .radio_id
        .ok_or_else(|| format!("missing --radio-id (required)\n\n{DMR_LISTEN_USAGE}"))?;
    if radio_id == 0 {
        return Err(format!(
            "--radio-id must not be 0 — that is what an unset field looks like, not a \
             registration\n\n{DMR_LISTEN_USAGE}"
        ));
    }
    if radio_id > RADIO_ID_MAX {
        return Err(format!(
            "--radio-id must fit 24 bits (at most {RADIO_ID_MAX}) — a larger number cannot be a \
             DMRD source id\n\n{DMR_LISTEN_USAGE}"
        ));
    }

    let talkgroup = raw
        .talkgroup
        .ok_or_else(|| format!("missing --tg (required)\n\n{DMR_LISTEN_USAGE}"))?;
    if talkgroup == 0 {
        return Err(format!(
            "--tg must not be 0 — a master routes by the room you joined\n\n{DMR_LISTEN_USAGE}"
        ));
    }

    let timeslot = raw.timeslot.unwrap_or(DEFAULT_TIMESLOT);
    if timeslot != 1 && timeslot != 2 {
        return Err(format!(
            "--ts must be 1 or 2 — a DMR frame can name no other slot\n\n{DMR_LISTEN_USAGE}"
        ));
    }

    Ok(Parsed::Listen(ListenOptions {
        system,
        host,
        port,
        radio_id,
        callsign,
        talkgroup,
        timeslot,
        wav: raw.wav,
    }))
}

/// The flags as typed, before any of them are checked against each other.
/// Separate from the validation above so neither half is long enough to hide
/// a missing check.
struct RawArgs {
    host: String,
    port: Option<u16>,
    system: Option<String>,
    callsign: Option<String>,
    radio_id: Option<u32>,
    talkgroup: Option<u32>,
    timeslot: Option<u8>,
    wav: Option<PathBuf>,
}

/// Read the flags. `Ok(None)` is `--help`, which short-circuits before any
/// required argument is missed — an operator asking what the arguments ARE
/// must not be told one is missing.
fn scan(mut args: impl Iterator<Item = String>) -> Result<Option<RawArgs>, String> {
    let mut port: Option<u16> = None;
    let mut system: Option<String> = None;
    let mut callsign: Option<String> = None;
    let mut radio_id: Option<u32> = None;
    let mut talkgroup: Option<u32> = None;
    let mut timeslot: Option<u8> = None;
    let mut wav: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--port" => {
                let v = crate::cli::flag_value(&mut args, "--port")?;
                port = Some(
                    v.parse()
                        .map_err(|_| format!("--port expects a 16-bit port number, got {v:?}"))?,
                );
            }
            "--system" => system = Some(crate::cli::flag_value(&mut args, "--system")?),
            "--callsign" => callsign = Some(crate::cli::flag_value(&mut args, "--callsign")?),
            "--radio-id" => {
                let v = crate::cli::flag_value(&mut args, "--radio-id")?;
                radio_id = Some(v.parse().map_err(|_| {
                    format!("--radio-id expects a 24-bit DMR id (1-{RADIO_ID_MAX}), got {v:?}")
                })?);
            }
            "--tg" => {
                let v = crate::cli::flag_value(&mut args, "--tg")?;
                talkgroup = Some(
                    v.parse()
                        .map_err(|_| format!("--tg expects a talkgroup number, got {v:?}"))?,
                );
            }
            "--ts" => {
                let v = crate::cli::flag_value(&mut args, "--ts")?;
                timeslot = Some(
                    v.parse()
                        .map_err(|_| format!("--ts expects 1 or 2, got {v:?}"))?,
                );
            }
            "--wav" => wav = Some(PathBuf::from(crate::cli::flag_value(&mut args, "--wav")?)),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}\n\n{DMR_LISTEN_USAGE}"));
            }
            _ => positional.push(arg),
        }
    }

    let host = positional
        .into_iter()
        .next()
        .ok_or_else(|| format!("missing <host>\n\n{DMR_LISTEN_USAGE}"))?;

    Ok(Some(RawArgs {
        host,
        port,
        system,
        callsign,
        radio_id,
        talkgroup,
        timeslot,
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
                    Box::new(WavBackend::new(path.clone(), "dmr-listen")) as Box<dyn AudioBackend>
                }),
            )
        }
    }
}

/// Log in, print link/talker transitions until Ctrl-C, then log out.
///
/// `password` is moved in and moved on into the facade; it is never printed,
/// and the "connecting to …" line above names the target and the operator's
/// own identity only.
fn listen(opts: &ListenOptions, password: String) -> Result<(), String> {
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
        "logging in to {} at {}:{} as {} (radio {}, TG {} on TS{})…",
        opts.system,
        opts.host,
        opts.port,
        opts.callsign,
        opts.radio_id,
        opts.talkgroup,
        opts.timeslot
    );
    station
        .dmr_connect(
            &opts.system,
            &opts.host,
            opts.port,
            opts.radio_id,
            &opts.callsign,
            opts.talkgroup,
            opts.timeslot,
            password,
        )
        .map_err(|e| format!("dmr connect failed: {e}"))?;
    println!(
        "receive only — this command has no PTT. astar has no DMR transmit path yet; keying \
         is not available on any platform for this network."
    );

    let target = format!("{}:{}", opts.host, opts.port);
    let mut tracker = PrintTracker::new(target);
    let mut link_failed = false;

    while !stop.load(Ordering::Relaxed) {
        let Some(state) = station.dmr_state() else {
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

    println!("logging out…");
    station.dmr_disconnect();
    let deadline = Instant::now() + LOGOUT_TIMEOUT;
    while station.dmr_state().is_some() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    println!("logged out.");

    if link_failed {
        return Err("link failed".to_string());
    }
    Ok(())
}

/// Turns a stream of snapshots into the lines to print, once each.
///
/// Pure and separately tested, exactly as `ysf_listen`'s and `nxdn_listen`'s
/// are — the interesting behaviour is "what is said, and how often", not the
/// I/O around it.
struct PrintTracker {
    target: String,
    printed_link: bool,
    last_heard: Option<String>,
    last_heard_id: Option<u32>,
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

    fn on_snapshot(&mut self, state: &DmrSnapshot) -> Vec<String> {
        let mut lines = Vec::new();

        if state.link_state == "failed" {
            // The step it failed at is the whole diagnosis: a bad password
            // and an ID the master will not have are the same silence
            // otherwise. The snapshot carries the step and nothing else —
            // never the password, never the master's words.
            lines.push(match state.failure {
                Some(why) => format!("link failed ({why})"),
                None => "link failed".to_string(),
            });
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
        // mid-stream (a master can hand straight from one to the next).
        // Compared on the numeric id, not the formatted string, because that
        // id — not a callsign — is the only identity DMR carries on the
        // wire; see this module's doc.
        if state.receiving && (!self.receiving || state.last_heard_id != self.last_heard_id) {
            self.last_heard_id = state.last_heard_id;
            self.last_heard.clone_from(&state.last_heard);
            if let Some(id) = &self.last_heard {
                lines.push(format!("▶ {id}"));
            }
        }
        self.receiving = state.receiving;

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(|s| (*s).to_string()))
    }

    fn opts(args: &[&str]) -> ListenOptions {
        match parse_args(args).expect("parse") {
            Parsed::Listen(o) => o,
            Parsed::Help => panic!("expected options, got help"),
        }
    }

    fn err(args: &[&str]) -> String {
        parse_args(args).expect_err("expected an error")
    }

    #[test]
    fn a_bare_host_takes_the_default_port_and_timeslot() {
        // 62031 is the homebrew convention every MMDVM client already
        // defaults to, and TS2 is the hotspot convention. Defaults for
        // convenience only -- the network's own published port always wins
        // where there is one.
        let o = opts(&[
            "tgif.network",
            "--system",
            "tgif",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "3153591",
            "--tg",
            "31313",
        ]);
        assert_eq!(o.host, "tgif.network");
        assert_eq!(o.port, 62_031);
        assert_eq!(o.timeslot, 2);
        assert_eq!(o.radio_id, 3_153_591);
        assert_eq!(o.talkgroup, 31_313);
        assert_eq!(o.system, "tgif");
    }

    #[test]
    fn an_explicit_port_and_slot_win() {
        let o = opts(&[
            "m.example:55555",
            "--system",
            "freedmr-network",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "3153591",
            "--tg",
            "91",
            "--ts",
            "1",
        ]);
        assert_eq!(o.port, 55_555);
        assert_eq!(o.timeslot, 1);
    }

    #[test]
    fn a_missing_talkgroup_or_system_is_an_error_not_a_guess() {
        // A talkgroup number names nothing without its network, and a
        // guessed one links to somebody else's room under your own ID.
        assert!(
            parse_args(&[
                "m.example",
                "--system",
                "tgif",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "3153591",
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "m.example",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "3153591",
                "--tg",
                "91",
            ])
            .is_err()
        );
    }

    #[test]
    fn a_radio_id_that_does_not_fit_twenty_four_bits_is_an_error() {
        assert!(
            parse_args(&[
                "m.example",
                "--system",
                "tgif",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "999999999",
                "--tg",
                "91",
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "m.example",
                "--system",
                "tgif",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "0",
                "--tg",
                "91",
            ])
            .is_err()
        );
    }

    #[test]
    fn there_is_no_password_argument() {
        // A secret in argv is readable by every process on the machine and
        // ends up in shell history. ASTAR_DMR_PASSWORD or nothing.
        assert!(
            parse_args(&[
                "m.example",
                "--system",
                "tgif",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "3153591",
                "--tg",
                "91",
                "--password",
                "hunter2",
            ])
            .is_err()
        );
        assert!(!DMR_LISTEN_USAGE.contains("--password"));
        assert!(DMR_LISTEN_USAGE.contains("ASTAR_DMR_PASSWORD"));
    }

    #[test]
    fn a_slot_other_than_one_or_two_is_an_error() {
        assert!(
            parse_args(&[
                "m.example",
                "--system",
                "tgif",
                "--callsign",
                "KC0ABC",
                "--radio-id",
                "3153591",
                "--tg",
                "91",
                "--ts",
                "3",
            ])
            .is_err()
        );
    }

    #[test]
    fn help_is_help() {
        assert!(matches!(parse_args(&["--help"]), Ok(Parsed::Help)));
    }

    #[test]
    fn a_zero_talkgroup_is_refused() {
        let e = err(&[
            "m.example",
            "--system",
            "tgif",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "3153591",
            "--tg",
            "0",
        ]);
        assert!(e.contains("--tg"), "{e}");
    }

    #[test]
    fn missing_callsign_is_named() {
        assert!(
            err(&[
                "m.example",
                "--system",
                "tgif",
                "--radio-id",
                "3153591",
                "--tg",
                "91",
            ])
            .contains("missing --callsign")
        );
    }

    #[test]
    fn an_empty_system_is_refused() {
        let e = err(&[
            "m.example",
            "--system",
            "",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "3153591",
            "--tg",
            "91",
        ]);
        assert!(e.contains("--system"), "{e}");
    }

    #[test]
    fn wav_is_carried() {
        let o = opts(&[
            "m.example",
            "--system",
            "tgif",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "3153591",
            "--tg",
            "91",
            "--wav",
            "/tmp/out.wav",
        ]);
        assert_eq!(o.wav.as_deref(), Some(Path::new("/tmp/out.wav")));
    }

    #[test]
    fn a_host_port_that_contradicts_the_flag_is_refused() {
        let e = err(&[
            "m.example:62032",
            "--port",
            "62031",
            "--system",
            "tgif",
            "--callsign",
            "KC0ABC",
            "--radio-id",
            "3153591",
            "--tg",
            "91",
        ]);
        assert!(e.contains("62032") && e.contains("62031"), "{e}");
    }

    #[test]
    fn unknown_flags_are_refused() {
        assert!(err(&["m.example", "--nope"]).contains("unknown flag"));
    }

    // ---- what the operator is told ----

    fn snap(link: &'static str) -> DmrSnapshot {
        DmrSnapshot {
            link_state: link,
            failure: None,
            last_heard: None,
            last_heard_id: None,
            talkgroup: 91,
            timeslot: "ts2",
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
        let mut t = PrintTracker::new("m.example:62031".into());
        assert!(t.on_snapshot(&snap("logging_in")).is_empty());
        let lines = t.on_snapshot(&snap("linked"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("m.example:62031"), "{:?}", lines[0]);
        assert!(lines[0].contains("thumbdv"), "{:?}", lines[0]);
        assert!(
            t.on_snapshot(&snap("linked")).is_empty(),
            "a steady link must not repeat itself every 50 ms"
        );
    }

    /// "Your password is wrong" and "your ID is not allowed here" are the two
    /// things an operator needs told apart, and the snapshot's `failure` is
    /// the only place that distinction survives. Printing "link failed" and
    /// dropping it would throw away the one useful byte.
    #[test]
    fn a_failed_link_names_the_step_it_failed_at() {
        let mut t = PrintTracker::new("m.example:62031".into());
        let mut s = snap("failed");
        s.failure = Some("auth");
        let lines = t.on_snapshot(&s);
        assert_eq!(lines, vec!["link failed (auth)".to_string()]);
    }

    #[test]
    fn a_failure_with_no_reason_still_says_so() {
        let mut t = PrintTracker::new("x".into());
        assert_eq!(
            t.on_snapshot(&snap("failed")),
            vec!["link failed".to_string()]
        );
    }

    #[test]
    fn a_transmission_prints_its_talker_once() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut s = snap("linked");
        s.receiving = true;
        s.last_heard = Some("3153591".into());
        s.last_heard_id = Some(3_153_591);
        assert_eq!(t.on_snapshot(&s), vec!["▶ 3153591".to_string()]);
        assert!(
            t.on_snapshot(&s).is_empty(),
            "a transmission in progress must not reprint every poll"
        );
    }

    /// `last_heard`/`last_heard_id` persist past end-of-transmission by
    /// design, so the next over by the SAME id must still announce itself.
    #[test]
    fn the_same_talker_keying_again_is_announced_again() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut on = snap("linked");
        on.receiving = true;
        on.last_heard = Some("3153591".into());
        on.last_heard_id = Some(3_153_591);
        assert_eq!(t.on_snapshot(&on).len(), 1);

        let mut off = snap("linked");
        off.receiving = false;
        off.last_heard = Some("3153591".into());
        off.last_heard_id = Some(3_153_591);
        assert!(t.on_snapshot(&off).is_empty());

        assert_eq!(t.on_snapshot(&on), vec!["▶ 3153591".to_string()]);
    }

    /// A master can hand straight from one talker to the next without a gap;
    /// that must not read as one long over by the first.
    #[test]
    fn a_talker_change_mid_stream_is_announced() {
        let mut t = PrintTracker::new("x".into());
        let _ = t.on_snapshot(&snap("linked"));
        let mut a = snap("linked");
        a.receiving = true;
        a.last_heard = Some("3153591".into());
        a.last_heard_id = Some(3_153_591);
        assert_eq!(t.on_snapshot(&a).len(), 1);
        let mut b = snap("linked");
        b.receiving = true;
        b.last_heard = Some("3100000".into());
        b.last_heard_id = Some(3_100_000);
        assert_eq!(t.on_snapshot(&b), vec!["▶ 3100000".to_string()]);
    }
}
