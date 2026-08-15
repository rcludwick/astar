// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `dstar-listen` subcommand (iax-a9d4 Task 7): link to a D-Star `DExtra`
//! reflector module and decode received voice. iax-2f6b added a minimal
//! manual TX control: stdin `key`/`unkey`/`toggle` commands call
//! `Station::set_ptt`, reusing the exact `crate::ptt` reader `call` already
//! uses for IAX2/M17 (see that module's doc). The session ALWAYS starts
//! unkeyed and NOTHING here ever calls `set_ptt(true)` except in direct,
//! synchronous response to one of those operator-typed stdin lines — this
//! command still never keys on its own. Whether to point this at a live
//! reflector and actually type `key` remains entirely the operator's manual
//! call.
//!
//! Unlike `dial`/`parrot` (which drive `astar_iax::Manager` directly), this
//! command drives [`astar_station::Station`]: the milestone's binding
//! requirement wires this crate's `dstar` feature to `astar-station/dstar`
//! (see `Cargo.toml`), and `Station::dstar_connect`/`dstar_disconnect`/
//! `dstar_state` is the one entry point that already carries the module's
//! mutual-exclusion guards and poll/snapshot contract end to end — reaching
//! past it to `DstarSession` directly would just re-implement that facade
//! for no benefit to a single-purpose CLI invocation.
//!
//! Audio is played on the default output device (real `CpalBackend`), or,
//! with `--wav`, captured into an 8 kHz s16 mono WAV file instead: `--wav`
//! swaps in a small in-process [`WavBackend`] (implementing
//! `astar_audio::AudioBackend`) via [`Station::with_backend_factory`], so
//! `DstarSession`'s own output-routing code (see the module docs on
//! `astar_console::dstar`) never has to know the difference.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_codec::ambe::AmbeBackend;
use astar_console::DstarSnapshotState;
use astar_dstar::LinkState;
use astar_station::{Station, StationConfig};

use crate::cli::DSTAR_LISTEN_USAGE;
use crate::ptt::{self, PttCommand};

const DEFAULT_PORT: u16 = 30_001;
/// How often the control loop polls `Station::dstar_state()` for link/talker/
/// slow-text changes. Cheap (atomics-backed snapshot, see
/// `astar_console::dstar`'s module doc) — no need to poll faster than a
/// human needs to see a transition.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Bound on how long `run` waits, after requesting `dstar_disconnect`, for
/// `Station::dstar_state()` to actually clear before exiting — mirrors the
/// station test suite's own `wait_until` deadlines.
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
    pub module: char,
    pub callsign: String,
    pub wav: Option<PathBuf>,
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    match parse(args)? {
        Parsed::Help => {
            print!("{DSTAR_LISTEN_USAGE}");
            Ok(())
        }
        Parsed::Listen(opts) => listen(&opts),
    }
}

/// Parse `dstar-listen`'s arguments. Pure (no I/O) so it's unit-testable
/// without a network stack or audio devices — mirrors `dial::parse`'s shape.
pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut port = DEFAULT_PORT;
    let mut callsign: Option<String> = None;
    let mut wav: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--port" => {
                let v = crate::cli::flag_value(&mut args, "--port")?;
                port = v
                    .parse()
                    .map_err(|_| format!("--port expects a 16-bit port number, got {v:?}"))?;
            }
            "--callsign" => callsign = Some(crate::cli::flag_value(&mut args, "--callsign")?),
            "--wav" => wav = Some(PathBuf::from(crate::cli::flag_value(&mut args, "--wav")?)),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}\n\n{DSTAR_LISTEN_USAGE}"));
            }
            _ => positional.push(arg),
        }
    }

    let mut pos = positional.into_iter();
    let host = pos
        .next()
        .ok_or_else(|| format!("missing <host>\n\n{DSTAR_LISTEN_USAGE}"))?;
    let module_arg = pos
        .next()
        .ok_or_else(|| format!("missing <module>\n\n{DSTAR_LISTEN_USAGE}"))?;
    let mut module_chars = module_arg.chars();
    let module = match (module_chars.next(), module_chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => {
            return Err(format!(
                "<module> must be a single A-Z letter, got {module_arg:?}\n\n{DSTAR_LISTEN_USAGE}"
            ));
        }
    };
    let callsign =
        callsign.ok_or_else(|| format!("missing --callsign (required)\n\n{DSTAR_LISTEN_USAGE}"))?;
    if callsign.is_empty() {
        return Err(format!(
            "--callsign must not be empty\n\n{DSTAR_LISTEN_USAGE}"
        ));
    }

    Ok(Parsed::Listen(ListenOptions {
        host,
        port,
        module,
        callsign,
        wav,
    }))
}

/// Build a station with a real `CpalBackend` (play to the default output
/// device), or, with `--wav`, one backed by [`WavBackend`] so decoded audio
/// lands in a file instead.
fn build_station(wav: Option<&Path>) -> Station {
    match wav {
        None => Station::new(StationConfig::default()),
        Some(path) => {
            let path = path.to_path_buf();
            Station::with_backend_factory(
                StationConfig::default(),
                Box::new(move || Box::new(WavBackend::new(path.clone())) as Box<dyn AudioBackend>),
            )
        }
    }
}

/// The `dstar-listen` control loop: connect, print link/talker/slow-text
/// transitions until Ctrl-C, then unlink cleanly.
fn listen(opts: &ListenOptions) -> Result<(), String> {
    let station = build_station(opts.wav.as_deref());

    let stop = Arc::new(AtomicBool::new(false));
    let ctrlc_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        // A second Ctrl-C escalates to an immediate hard exit (mirrors
        // astar-server's own install_signal_handler) so an operator who
        // hits it twice never gets stuck waiting on teardown.
        if ctrlc_stop.swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
    })
    .map_err(|e| format!("failed to install Ctrl-C handler: {e}"))?;

    if let Some(path) = &opts.wav {
        println!("writing decoded audio to {}", path.display());
    }
    println!(
        "connecting to {} module {} as {}…",
        opts.host, opts.module, opts.callsign
    );
    station
        .dstar_connect(&opts.host, opts.port, opts.module, &opts.callsign)
        .map_err(|e| format!("dstar connect failed: {e}"))?;

    let mut tracker = PrintTracker::new(opts.host.clone(), opts.module);
    let mut link_failed = false;

    // iax-2f6b: manual TX control. Reuses `call`/`dial`'s exact stdin reader
    // (see `crate::ptt`'s module doc) — line-based, press Enter. `keyed`
    // starts `false` and the ONLY code path that can ever flip it `true` is
    // `apply_ptt_command` reacting to an operator-typed `key`/`toggle` line
    // below; nothing in this loop keys on its own.
    let ptt_rx = ptt::spawn_reader();
    println!(
        "PTT: type key/k to transmit, unkey/u to release, t to toggle, q to quit \
         (starts UNKEYED — nothing transmits until you type key)"
    );
    let mut keyed = false;

    'poll: while !stop.load(Ordering::Relaxed) {
        let Some(state) = station.dstar_state() else {
            // The session tore itself down on its own (e.g. a fatal socket
            // error) — nothing left to poll.
            break;
        };

        for line in tracker.on_snapshot(&state) {
            println!("{line}");
        }

        // The run loop never tears the session down on its own from
        // `LinkState::Failed` (`dstar_state` keeps returning `Some`), so
        // without this check the operator would be left staring at
        // "connecting…" forever after a NAKed connect, a 30s keepalive
        // timeout, or a reflector-initiated DISCONNECTED. Treat it like an
        // operator-requested Ctrl-C: stop polling, unlink/tear down below,
        // then report failure via a non-zero exit. (The "link failed" line
        // itself was already printed above, by `tracker.on_snapshot`.)
        if state.link == LinkState::Failed {
            link_failed = true;
            break;
        }

        // Drain stdin PTT commands. `try_recv` never blocks, so this can't
        // stall the link/talker polling above.
        loop {
            match ptt_rx.try_recv() {
                Ok(cmd) => {
                    if apply_ptt_command(&station, &mut keyed, cmd) {
                        break 'poll;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // stdin closed without an explicit quit line — treat like Ctrl-C.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'poll,
            }
        }

        thread::sleep(POLL_INTERVAL);
    }

    println!("unlinking…");
    station.dstar_disconnect();
    let deadline = Instant::now() + UNLINK_TIMEOUT;
    while station.dstar_state().is_some() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    println!("unlinked.");

    if link_failed {
        return Err("link failed".to_string());
    }
    Ok(())
}

/// Applies one stdin PTT command (`crate::ptt::PttCommand`) to the live
/// station, mirroring `call.rs`'s `apply_ptt`/`set_ptt` shape exactly. Returns
/// `true` when the caller should stop polling and proceed to unlink
/// (`Quit`/`Eof`).
///
/// D-Star has no DTMF concept in this codebase, so `PttCommand::Dtmf` is
/// reported as unsupported rather than silently ignored.
///
/// Safety-relevant: this is the ONLY function `listen()`'s loop calls that can
/// change PTT state, and it only ever runs in direct, synchronous response to
/// one stdin line already read by `crate::ptt::spawn_reader`'s reader thread
/// — nothing in this file calls `Station::set_ptt(true)` any other way, and
/// `keyed` starts `false` (see `listen()`).
fn apply_ptt_command(station: &Station, keyed: &mut bool, cmd: PttCommand) -> bool {
    match cmd {
        PttCommand::Key => set_ptt(station, keyed, true),
        PttCommand::Unkey => set_ptt(station, keyed, false),
        PttCommand::Toggle => {
            let next = !*keyed;
            set_ptt(station, keyed, next);
        }
        PttCommand::Dtmf(_) => println!("DTMF is not supported over D-Star"),
        PttCommand::Unknown(line) => {
            println!("? unknown command {line:?} — try key / unkey / toggle / quit");
        }
        PttCommand::Quit | PttCommand::Eof => return true,
    }
    false
}

/// Sets PTT to `on`, idempotently, printing the transition (or the failure —
/// e.g. calling this after the session already tore itself down).
fn set_ptt(station: &Station, keyed: &mut bool, on: bool) {
    if *keyed == on {
        return;
    }
    match station.set_ptt(on) {
        Ok(()) => {
            *keyed = on;
            println!(
                "PTT: {}",
                if on {
                    "KEYED — transmitting"
                } else {
                    "unkeyed"
                }
            );
        }
        Err(e) => println!("PTT change failed: {e}"),
    }
}

/// Turns `Station::dstar_state()` snapshots into the print lines the brief
/// specifies, pure and independent of any live `Station` so the
/// talker/slow-text attribution logic is unit-testable without a network
/// stack. `listen()` is the only non-test caller; `#[cfg(test)] mod tests`
/// below drives it directly (private items are visible to a child module).
struct PrintTracker {
    host: String,
    module: char,
    printed_link: bool,
    last_talker: Option<String>,
    /// The `slow_text` value already "accounted for" — either printed, or
    /// (see `on_snapshot`'s talker-change branch) carried over from a PRIOR
    /// talker and therefore deliberately withheld rather than printed. Never
    /// compare `state.slow_text` against anything else: `DstarSnapshotState`
    /// has "last-heard" semantics (it only changes on a fresh completed
    /// reassembly, and is never cleared on a talker change — see
    /// `astar_console::dstar`'s module doc), so on its own a non-empty
    /// value tells you nothing about which talker it belongs to.
    last_slow: Option<String>,
}

impl PrintTracker {
    fn new(host: String, module: char) -> Self {
        Self {
            host,
            module,
            printed_link: false,
            last_talker: None,
            last_slow: None,
        }
    }

    /// Returns the lines to print for this poll, in order, and updates
    /// internal tracking state.
    ///
    /// The key rule (fixes a real misattribution bug caught in review): a
    /// talker change NEVER prints whatever `slow_text` happens to be present
    /// at that moment, even if it's non-empty — that value most likely
    /// belongs to the PREVIOUS talker (their own identification text,
    /// completed just before they unkeyed) still sitting in the snapshot's
    /// last-heard field. Instead, the current `slow_text` is silently
    /// adopted as the new "already accounted for" baseline; only once it
    /// changes AGAIN (a fresh reassembly, which can only complete while
    /// decoding the CURRENTLY tracked stream — see the module docs) is it
    /// safe to attribute to the current talker and print.
    fn on_snapshot(&mut self, state: &DstarSnapshotState) -> Vec<String> {
        let mut lines = Vec::new();

        // A NAKed connect, a 30s keepalive timeout, or a reflector-
        // initiated DISCONNECTED all land here (see `astar_dstar::fsm`'s
        // `LinkState` doc) — `listen()`'s poll loop stops on this line
        // being returned (see its own comment), so there is nothing further
        // to track once it fires.
        if state.link == LinkState::Failed {
            lines.push("link failed".to_string());
            return lines;
        }

        if !self.printed_link && state.link == LinkState::Linked {
            self.printed_link = true;
            // D-Star is hardware-only since iax-b3e7 M0: the only backend is
            // the ThumbDV. `None` would mean a session with no backend at
            // all, which `DstarSession::connect` refuses to produce — name it
            // rather than inventing a backend.
            let backend = match state.backend {
                Some(AmbeBackend::Hardware) => "thumbdv",
                None => "unknown",
            };
            lines.push(format!(
                "linked {} module {} (backend: {backend})",
                self.host, self.module
            ));
        }

        if state.talker != self.last_talker {
            self.last_talker.clone_from(&state.talker);
            // Adopt (don't print) whatever slow_text is already sitting in
            // the snapshot — see the doc comment above.
            self.last_slow.clone_from(&state.slow_text);
            if let Some(cs) = &self.last_talker {
                lines.push(stream_line(cs, None));
            }
        } else if state.slow_text != self.last_slow {
            self.last_slow.clone_from(&state.slow_text);
            if let (Some(cs), Some(text)) = (&self.last_talker, &self.last_slow)
                && !text.is_empty()
            {
                lines.push(stream_line(cs, Some(text)));
            }
        }

        lines
    }
}

fn stream_line(callsign: &str, slow_text: Option<&str>) -> String {
    match slow_text {
        Some(t) if !t.is_empty() => format!("▶ {callsign} {t}"),
        _ => format!("▶ {callsign}"),
    }
}

// ---- `--wav`: an `AudioBackend` that captures the D-Star output bus into a
// ---- file instead of a real device ------------------------------------------

/// 44-byte canonical RIFF/WAVE header size (no extra chunks).
const WAV_HEADER_LEN: usize = 44;
/// How often the `--wav` writer thread re-patches the header in place with
/// the data length written so far — see the call site's comment for why
/// (bounds the worst case on an abrupt exit to "up to this much of the
/// declared length is stale", never "corrupt file").
const HEADER_PATCH_INTERVAL: Duration = Duration::from_secs(1);

fn wav_device() -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new("dstar-listen-wav"),
        name: "dstar-listen --wav".to_string(),
        direction: Direction::Output,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

/// An [`AudioBackend`] with no real input/output devices: its one output
/// "device" pulls decoded audio on a background thread at the stream's own
/// cadence (matching how a real cpal callback would be driven) and appends
/// it, as 16-bit PCM, to a WAV file opened once for the whole session — every
/// D-Star stream (transmission) received while linked lands in the same
/// file, one after another.
struct WavBackend {
    path: PathBuf,
}

impl WavBackend {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl AudioBackend for WavBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![wav_device()])
    }

    fn default_input(&self) -> Option<DeviceInfo> {
        None
    }

    fn default_output(&self) -> Option<DeviceInfo> {
        Some(wav_device())
    }

    fn open_input(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Err(AudioError::DeviceNotFound(
            "dstar-listen --wav has no input device (D-Star RX never opens one)".into(),
        ))
    }

    fn open_output(
        &self,
        _device: &DeviceInfo,
        config: StreamConfig,
        mut source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let file = File::create(&self.path)
            .map_err(|e| AudioError::BuildStream(format!("create {}: {e}", self.path.display())))?;
        let mut writer = BufWriter::new(file);
        let sample_rate = config.sample_rate;
        let channels = config.channels;
        // A real (zero-length-data) header, not an all-zero placeholder: if
        // the process is killed before the writer thread's first periodic
        // patch below ever runs (see that comment for why this can't just be
        // "patch once at clean shutdown"), the file on disk is still a
        // valid, readable (if silent) WAV rather than unparseable garbage.
        // The explicit `flush` is NOT redundant with `write_all`: `BufWriter`
        // only writes through to the underlying `File` (and hence to disk)
        // once its internal buffer fills, is explicitly flushed, or is
        // dropped — and an abrupt kill (SIGKILL, or the double-Ctrl-C
        // escalation below) skips `Drop` entirely, same as it skips every
        // other destructor. Without this, a kill in the first
        // `HEADER_PATCH_INTERVAL` leaves a literal EMPTY (0-byte) file, not
        // the intended silent-but-valid one — caught by this fix's own
        // smoke test, not a hypothetical.
        write_wav_header(&mut writer, sample_rate, channels, 0)
            .and_then(|()| writer.flush())
            .map_err(|e| AudioError::BuildStream(format!("write initial header: {e}")))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let pull_ms = config.buffer_ms.max(1);
        let samples_per_pull = ((u64::from(sample_rate) * u64::from(pull_ms)) / 1_000)
            .max(1)
            .saturating_mul(u64::from(channels.max(1))) as usize;
        // How many pulls between header re-patches — see the loop below.
        let patch_every_n_pulls = (HEADER_PATCH_INTERVAL.as_millis() / u128::from(pull_ms)).max(1);

        let join = thread::Builder::new()
            .name("dstar-wav-writer".to_string())
            .spawn(move || {
                let mut buf = vec![0.0_f32; samples_per_pull];
                let mut data_len: u64 = 0;
                let mut pcm = Vec::<u8>::with_capacity(buf.len() * 2);
                let mut pulls_since_patch: u128 = 0;
                while !thread_stop.load(Ordering::Relaxed) {
                    buf.fill(0.0);
                    let _ = source.read(&mut buf);
                    pcm.clear();
                    for &s in &buf {
                        // Same f32-to-i16 device-sample conversion
                        // `astar_audio::stream`'s real cpal output
                        // callback uses (see its module doc's `#![allow(...)]`
                        // for the same lint) — the `clamp` just above bounds
                        // the input to `[-1.0, 1.0]` first, so this never
                        // wraps, only (intentionally) truncates the fraction.
                        #[allow(clippy::cast_possible_truncation)]
                        let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                        pcm.extend_from_slice(&v.to_le_bytes());
                    }
                    if writer.write_all(&pcm).is_err() {
                        break;
                    }
                    data_len += pcm.len() as u64;
                    pulls_since_patch += 1;

                    // Periodically rewrite the header in place with the
                    // data length written SO FAR (iax-a9d4 Task 7 review
                    // fix): a double-Ctrl-C escalates to `process::exit`
                    // (see `listen`), which skips the clean-shutdown path
                    // below entirely — without this, that path is the WAV's
                    // only chance at a non-placeholder header, so a
                    // double-Ctrl-C (or a SIGKILL, or a crash) would leave
                    // an unreadable file. Patching here instead bounds the
                    // worst case to "missing up to ~1s of the declared
                    // `data` length" (the file is always independently
                    // readable), not "corrupt", regardless of how the
                    // process ends.
                    if pulls_since_patch >= patch_every_n_pulls {
                        pulls_since_patch = 0;
                        let capped = u32::try_from(data_len).unwrap_or(u32::MAX);
                        if patch_header_in_place(&mut writer, sample_rate, channels, capped)
                            .is_err()
                        {
                            break;
                        }
                    }

                    thread::sleep(Duration::from_millis(u64::from(pull_ms)));
                }
                let capped = u32::try_from(data_len).unwrap_or(u32::MAX);
                let _ = patch_header_in_place(&mut writer, sample_rate, channels, capped);
                let _ = writer.flush();
            })
            .map_err(|e| AudioError::BuildStream(format!("spawn wav writer thread: {e}")))?;

        Ok(Box::new(WavStreamHandle {
            stop,
            join: Some(join),
        }))
    }
}

/// Rewinds to the start of the file, overwrites the header with the current
/// `data_len`, then seeks back to the write position (`WAV_HEADER_LEN +
/// data_len`) so the caller's next `write_all` resumes appending PCM exactly
/// where it left off. `BufWriter::seek` flushes any buffered bytes before
/// moving the underlying file position, both times — so this never
/// reorders/loses PCM already handed to `writer.write_all`.
fn patch_header_in_place(
    writer: &mut BufWriter<File>,
    sample_rate: u32,
    channels: u16,
    data_len: u32,
) -> io::Result<()> {
    writer.seek(SeekFrom::Start(0))?;
    write_wav_header(writer, sample_rate, channels, data_len)?;
    let resume_at = u64::try_from(WAV_HEADER_LEN).unwrap_or(44) + u64::from(data_len);
    writer.seek(SeekFrom::Start(resume_at))?;
    Ok(())
}

/// Canonical 44-byte RIFF/WAVE header for 16-bit PCM.
fn write_wav_header(
    w: &mut impl Write,
    sample_rate: u32,
    channels: u16,
    data_len: u32,
) -> io::Result<()> {
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let block_align = channels * 2;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size (PCM)
    w.write_all(&1u16.to_le_bytes())?; // audio format: PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?; // bits per sample
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

struct WavStreamHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl WavStreamHandle {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl StreamHandle for WavStreamHandle {
    fn stop(mut self: Box<Self>) {
        self.shutdown();
    }

    fn pause(&self) -> Result<(), AudioError> {
        Ok(())
    }

    fn resume(&self) -> Result<(), AudioError> {
        Ok(())
    }
}

impl Drop for WavStreamHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Station` with no live session: every `set_ptt` on it returns
    /// `NotConnected`, which is the error branch `set_ptt` below has to get
    /// right (a failed change must NOT move the local `keyed` flag).
    fn idle_station() -> Station {
        Station::new(StationConfig::default())
    }

    #[test]
    fn ptt_commands_map_to_key_unkey_and_toggle() {
        // The ONLY code in the tree that can key a D-Star transmission from
        // operator input (iax-2f6b review): it was entirely untested,
        // including `Toggle`'s `!*keyed` and the idempotence guard.
        let station = idle_station();
        let mut keyed = true; // pretend a key succeeded earlier

        // Unkey against a station with no session: the change FAILS, so the
        // local flag must stay `true` — otherwise a later `Unkey` would be
        // swallowed by the idempotence guard and the operator's unkey would
        // become a silent no-op.
        assert!(!apply_ptt_command(&station, &mut keyed, PttCommand::Unkey));
        assert!(
            keyed,
            "a FAILED unkey must not clear the local keyed flag — that would make the next unkey \
             a silent no-op"
        );

        // Same for a failed key.
        let mut keyed = false;
        assert!(!apply_ptt_command(&station, &mut keyed, PttCommand::Key));
        assert!(!keyed, "a failed key must not set the local keyed flag");

        // Toggle computes the opposite of the current state (and, here,
        // fails to apply it — leaving the flag alone).
        let mut keyed = false;
        assert!(!apply_ptt_command(&station, &mut keyed, PttCommand::Toggle));
        assert!(!keyed);
    }

    #[test]
    fn quit_and_eof_stop_the_loop_and_other_commands_do_not() {
        let station = idle_station();
        let mut keyed = false;
        assert!(apply_ptt_command(&station, &mut keyed, PttCommand::Quit));
        assert!(apply_ptt_command(&station, &mut keyed, PttCommand::Eof));
        assert!(!apply_ptt_command(
            &station,
            &mut keyed,
            PttCommand::Dtmf('1')
        ));
        assert!(!apply_ptt_command(
            &station,
            &mut keyed,
            PttCommand::Unknown("wat".into())
        ));
        assert!(!keyed, "no non-PTT command may ever change the keyed state");
    }

    #[test]
    fn set_ptt_is_idempotent() {
        let station = idle_station();
        let mut keyed = false;
        // Already unkeyed: nothing is attempted at all (so no error print).
        set_ptt(&station, &mut keyed, false);
        assert!(!keyed);
    }

    fn args(s: &[&str]) -> impl Iterator<Item = String> {
        s.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn parse_ok(a: &[&str]) -> ListenOptions {
        match parse(args(a)).expect("expected Ok") {
            Parsed::Listen(o) => o,
            Parsed::Help => panic!("expected Listen, got Help"),
        }
    }

    #[test]
    fn parses_required_positionals_and_callsign() {
        let o = parse_ok(&["127.0.0.1", "b", "--callsign", "AJ7HR"]);
        assert_eq!(o.host, "127.0.0.1");
        assert_eq!(o.module, 'B', "module is uppercased");
        assert_eq!(o.callsign, "AJ7HR");
        assert_eq!(o.port, DEFAULT_PORT);
        assert!(o.wav.is_none());
    }

    #[test]
    fn missing_host_is_an_error() {
        let err = parse(args(&[])).unwrap_err();
        assert!(err.contains("missing <host>"), "got {err:?}");
    }

    #[test]
    fn missing_module_is_an_error() {
        let err = parse(args(&["127.0.0.1"])).unwrap_err();
        assert!(err.contains("missing <module>"), "got {err:?}");
    }

    #[test]
    fn missing_callsign_is_an_error() {
        let err = parse(args(&["127.0.0.1", "B"])).unwrap_err();
        assert!(err.contains("missing --callsign"), "got {err:?}");
    }

    #[test]
    fn empty_callsign_is_rejected() {
        let err = parse(args(&["127.0.0.1", "B", "--callsign", ""])).unwrap_err();
        assert!(err.contains("--callsign must not be empty"), "got {err:?}");
    }

    #[test]
    fn rejects_a_module_that_is_not_a_single_letter() {
        for bad in ["1", "BC", ""] {
            let err = parse(args(&["127.0.0.1", bad, "--callsign", "AJ7HR"])).unwrap_err();
            assert!(
                err.contains("must be a single A-Z letter"),
                "input {bad:?} got {err:?}"
            );
        }
    }

    #[test]
    fn parses_port_and_wav_overrides() {
        let o = parse_ok(&[
            "xrf757.example.org",
            "a",
            "--callsign",
            "AJ7HR",
            "--port",
            "30002",
            "--wav",
            "/tmp/out.wav",
        ]);
        assert_eq!(o.port, 30_002);
        assert_eq!(o.wav, Some(PathBuf::from("/tmp/out.wav")));
    }

    #[test]
    fn bad_port_is_an_error() {
        let err = parse(args(&["h", "B", "--callsign", "C", "--port", "not-a-port"])).unwrap_err();
        assert!(err.contains("--port expects"), "got {err:?}");
    }

    #[test]
    fn help_flag_short_circuits() {
        assert!(matches!(parse(args(&["-h"])), Ok(Parsed::Help)));
        assert!(matches!(parse(args(&["--help"])), Ok(Parsed::Help)));
        // even with positionals already queued up
        assert!(matches!(
            parse(args(&["127.0.0.1", "B", "--help"])),
            Ok(Parsed::Help)
        ));
    }

    #[test]
    fn unknown_flag_is_rejected() {
        let err = parse(args(&["h", "B", "--callsign", "C", "--bogus"])).unwrap_err();
        assert!(err.contains("unknown flag: --bogus"), "got {err:?}");
    }

    #[test]
    fn wav_header_round_trips_riff_fields() {
        let mut buf = Vec::new();
        write_wav_header(&mut buf, 8_000, 1, 320).expect("write header");
        assert_eq!(buf.len(), WAV_HEADER_LEN);
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 36 + 320);
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(buf[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(buf[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(buf[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(buf[24..28].try_into().unwrap()), 8_000);
        assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), 16_000);
        assert_eq!(u16::from_le_bytes(buf[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(buf[34..36].try_into().unwrap()), 16);
        assert_eq!(&buf[36..40], b"data");
        assert_eq!(u32::from_le_bytes(buf[40..44].try_into().unwrap()), 320);
    }

    // ---- WAV periodic header patching (review fix 2) -----------------------

    #[test]
    fn patch_header_in_place_preserves_already_written_pcm_and_resumes_appending() {
        let path = std::env::temp_dir().join(format!(
            "astar-cli-dstar-wav-patch-test-{}-{:?}.wav",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = File::create(&path).expect("create temp wav");
        let mut writer = BufWriter::new(file);
        write_wav_header(&mut writer, 8_000, 1, 0).expect("initial header");

        // Write one "pull"'s worth of PCM, patch the header mid-stream (as
        // the writer thread does periodically — see HEADER_PATCH_INTERVAL),
        // then write a second pull's worth and patch again.
        let first: [u8; 4] = [0x01, 0x02, 0x03, 0x04];
        writer.write_all(&first).expect("write first pull");
        let after_first = u32::try_from(first.len()).unwrap();
        patch_header_in_place(&mut writer, 8_000, 1, after_first).expect("patch mid-stream");

        let second: [u8; 4] = [0x05, 0x06, 0x07, 0x08];
        writer.write_all(&second).expect("write second pull");
        let total = u32::try_from(first.len() + second.len()).unwrap();
        patch_header_in_place(&mut writer, 8_000, 1, total).expect("final patch");
        writer.flush().expect("flush");
        drop(writer);

        let bytes = std::fs::read(&path).expect("read back");
        std::fs::remove_file(&path).ok();

        assert_eq!(bytes.len(), WAV_HEADER_LEN + first.len() + second.len());
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            total,
            "final data chunk size reflects everything written"
        );
        // The PCM itself must be intact, in order, and land right after the
        // header — proves the mid-stream seek-patch-seek-back never
        // reordered, duplicated, or clobbered already-written data.
        let mut expected_pcm = first.to_vec();
        expected_pcm.extend_from_slice(&second);
        assert_eq!(&bytes[WAV_HEADER_LEN..], expected_pcm.as_slice());
    }

    #[test]
    fn a_wav_file_is_immediately_valid_before_any_pcm_or_patch() {
        // The initial header written by `open_output` (data_len = 0), not
        // the old all-zero placeholder — a process killed before the first
        // periodic patch still leaves a parseable (if silent) file.
        let mut buf = Vec::new();
        write_wav_header(&mut buf, 8_000, 1, 0).expect("initial header");
        assert_eq!(buf.len(), WAV_HEADER_LEN);
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[36..40], b"data");
        assert_eq!(u32::from_le_bytes(buf[40..44].try_into().unwrap()), 0);
    }

    // ---- talker/slow-text print-decision logic (review fix 1) --------------

    fn snapshot(talker: Option<&str>, slow_text: Option<&str>) -> DstarSnapshotState {
        DstarSnapshotState {
            link: LinkState::Linked,
            talker: talker.map(str::to_string),
            slow_text: slow_text.map(str::to_string),
            backend: Some(AmbeBackend::Hardware),
            tx_capable: true,
            ptt: false,
            tx_dbfs: -60.0,
            rx_dbfs: -60.0,
            input_dbfs: -60.0,
        }
    }

    #[test]
    fn print_tracker_prints_link_failed_on_the_failed_transition() {
        // Regression test (whole-branch review finding 2, IMPORTANT): a
        // NAKed connect, a 30s keepalive timeout, or a reflector-initiated
        // DISCONNECTED all land `DstarSnapshotState.link` in
        // `LinkState::Failed`. Before this fix `listen()`'s poll loop had
        // no exit for that transition, leaving the operator staring at
        // "connecting…" forever; now the tracker surfaces it as a line the
        // loop can also key its exit off of.
        let mut t = PrintTracker::new("h".to_string(), 'A');
        let mut snap = snapshot(None, None);
        snap.link = LinkState::Failed;
        assert_eq!(t.on_snapshot(&snap), vec!["link failed".to_string()]);
    }

    #[test]
    fn print_tracker_prints_link_failed_even_mid_transmission() {
        // The Failed check must win regardless of what else changed in the
        // same snapshot (e.g. a talker mid-transmission when the link
        // drops) — no talker/slow-text line should sneak out instead.
        let mut t = PrintTracker::new("h".to_string(), 'A');
        let _ = t.on_snapshot(&snapshot(None, None)); // link line
        let _ = t.on_snapshot(&snapshot(Some("AJ7HR"), None)); // talker keys up
        let mut snap = snapshot(Some("AJ7HR"), Some("hello"));
        snap.link = LinkState::Failed;
        assert_eq!(t.on_snapshot(&snap), vec!["link failed".to_string()]);
    }

    #[test]
    fn print_tracker_prints_the_link_line_exactly_once() {
        let mut t = PrintTracker::new("127.0.0.1".to_string(), 'B');
        assert_eq!(
            t.on_snapshot(&snapshot(None, None)),
            vec!["linked 127.0.0.1 module B (backend: thumbdv)".to_string()]
        );
        assert!(t.on_snapshot(&snapshot(None, None)).is_empty());
    }

    #[test]
    fn print_tracker_reports_the_thumbdv_backend_name() {
        let mut t = PrintTracker::new("h".to_string(), 'C');
        let mut snap = snapshot(None, None);
        snap.backend = Some(AmbeBackend::Hardware);
        assert_eq!(
            t.on_snapshot(&snap),
            vec!["linked h module C (backend: thumbdv)".to_string()]
        );
    }

    #[test]
    fn print_tracker_prints_a_talker_then_its_own_slow_text() {
        let mut t = PrintTracker::new("h".to_string(), 'A');
        let _ = t.on_snapshot(&snapshot(None, None)); // link line, not under test
        assert_eq!(
            t.on_snapshot(&snapshot(Some("AJ7HR"), None)),
            vec!["▶ AJ7HR".to_string()]
        );
        assert_eq!(
            t.on_snapshot(&snapshot(Some("AJ7HR"), Some("hello"))),
            vec!["▶ AJ7HR hello".to_string()]
        );
        // An unchanged snapshot prints nothing further.
        assert!(
            t.on_snapshot(&snapshot(Some("AJ7HR"), Some("hello")))
                .is_empty()
        );
    }

    #[test]
    fn print_tracker_never_attributes_a_stale_slow_text_to_a_new_talker() {
        // Regression test (review finding 1): `DstarSnapshotState.slow_text`
        // has last-heard/persists-across-streams semantics — it is NOT
        // cleared when the talker changes. Station A keys up and finishes
        // with text "hi from A"; station B keys up next, and the snapshot's
        // `slow_text` field is STILL "hi from A" (stale) until B completes
        // its own reassembly. B's first printed line must not carry A's
        // text.
        let mut t = PrintTracker::new("h".to_string(), 'Z');
        let _ = t.on_snapshot(&snapshot(None, None)); // link line
        let _ = t.on_snapshot(&snapshot(Some("A"), None)); // A keys up
        assert_eq!(
            t.on_snapshot(&snapshot(Some("A"), Some("hi from A"))),
            vec!["▶ A hi from A".to_string()],
            "A's own text is correctly attributed to A"
        );

        // B keys up; the snapshot's slow_text is unchanged (still A's).
        assert_eq!(
            t.on_snapshot(&snapshot(Some("B"), Some("hi from A"))),
            vec!["▶ B".to_string()],
            "B's first line must NOT carry A's stale leftover text"
        );

        // Still nothing new for B: slow_text hasn't changed since the
        // (silently adopted) baseline at the talker-change moment.
        assert!(
            t.on_snapshot(&snapshot(Some("B"), Some("hi from A")))
                .is_empty(),
            "the stale text must never be printed later either"
        );

        // B's OWN text now completes (a genuinely new value) — THIS is
        // correctly attributed and printed.
        assert_eq!(
            t.on_snapshot(&snapshot(Some("B"), Some("hi from B"))),
            vec!["▶ B hi from B".to_string()],
            "B's own, later, genuinely-new text must still print"
        );
    }

    #[test]
    fn print_tracker_never_prints_stale_text_even_across_many_polls_of_a_silent_talker() {
        // A talker who transmits voice only (no slow text of their own)
        // must never surface a PRIOR talker's leftover text — not just "not
        // on B's first line", but on every subsequent poll for the rest of
        // B's transmission too.
        let mut t = PrintTracker::new("h".to_string(), 'Z');
        let _ = t.on_snapshot(&snapshot(None, None));
        let _ = t.on_snapshot(&snapshot(Some("A"), None));
        let _ = t.on_snapshot(&snapshot(Some("A"), Some("hi from A")));

        // B's talker-change edge: exactly one bare line, no text.
        assert_eq!(
            t.on_snapshot(&snapshot(Some("B"), Some("hi from A"))),
            vec!["▶ B".to_string()]
        );
        // Every poll after that, for as long as B's transmission continues
        // sending no new text of its own, must stay silent.
        for _ in 0..5 {
            assert!(
                t.on_snapshot(&snapshot(Some("B"), Some("hi from A")))
                    .is_empty(),
                "B never sends its own text; A's must never leak through"
            );
        }
    }
}
