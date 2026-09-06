// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A live `YSFReflector` link: the socket and the thread that drive
//! [`astar_ysf::YsfFsm`] (iax-e8a4, designed in `docs/design/ysf-network.md`).
//!
//! `astar-ysf` is deliberately pure — the FSM decides, and hands back bytes
//! to send — so something has to own a `UdpSocket`, feed it datagrams, and
//! call `tick`. That is this module, and it is the piece the design named as
//! missing: "the protocol crate is built; the vocoder and the session are
//! not."
//!
//! # Two ways to open a link, and only one of them makes sound
//!
//! [`YsfLink::connect`] is the link alone: it polls to hold the session
//! open, reports who is talking from the `YSFD` header, and hands the ninety
//! payload bytes nowhere. That is useful on its own and needs no hardware —
//! the header is not the payload, so `DataPacket::source` answers "is this
//! reflector alive, and who is on it" with no vocoder involved.
//!
//! [`YsfLink::connect_with_audio`] adds the receive path (astar-e7b3 §2):
//! an [`AmbeStream`] opened in [`VocoderMode::YsfDn`], an output bus, and
//! the loop that lifts five 20 ms voice frames out of each payload with
//! [`astar_codec::ysf::unpack_dn`] and plays them. YSF voice is **AMBE+2**,
//! which on astar means the AMBE-3000 in a `ThumbDV` and nothing else, so
//! this constructor fails without a dongle rather than pretending.
//!
//! # There is still no TX path, and that is deliberate
//!
//! Not a stubbed one, not one that sends silence: a transmit function that
//! put nothing on the air would be a worse lie than an absent one, and other
//! people are listening.
//!
//! The blocker is specific and worth naming, because it is not effort.
//! `vendor/ambe-thumbdv`'s `parse_channel` rejects any `Channel` response
//! whose bit count is not `0x48` — 72 bits — as "rate lost". A YSF encode
//! comes back at `0x31`, 49 bits, so the reply to every encode request would
//! be discarded, the request would time out, and the substituted value would
//! be a **D-Star** null codeword: on a YSF link, the wrong vocoder's noise
//! rather than silence. Widening that parser means editing a vendored
//! MIT/Apache crate, which the design says to raise and re-vendor rather
//! than patch in place. [`astar_codec::ambe`] refuses the encode at the
//! stream so nothing downstream can accidentally rely on it.
//!
//! # Modes astar refuses, out loud
//!
//! DN — V/D mode 1 and mode 2 — is decoded. VW (full-rate voice) and data
//! frames are not, and the refusal reaches [`YsfSnapshot::unsupported_mode`]
//! rather than turning into noise or into silence with no reason attached.
//!
//! # Threading
//!
//! One thread per link, owning the socket. It blocks on `recv_from` with a
//! read timeout so `tick` still runs when the reflector goes quiet — that
//! timeout is what makes the poll cadence and the link timeout work at all.
//! The control side sees an `AtomicU*` snapshot and a mutex-guarded
//! last-heard string, the same arrangement the audio lanes use.

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_audio::{AudioBackend, AudioRouter, CallAudio, OutputId, StreamConfig};
use astar_codec::ambe::{
    AMBE_STREAM_MAX_IN_FLIGHT, AmbeBackend, AmbeStream, VocoderMode, open_ambe_stream,
};
use astar_codec::ysf::{DnError, DnFrame, unpack_dn};
use astar_ysf::{DataType, Frame, FsmAction, LinkState, YsfFsm};

use crate::session::{ConsoleError, resolve_device};

/// How long the socket blocks before the loop runs `tick` anyway.
///
/// Shorter than the 5 s poll interval by enough that a poll is never late by
/// a meaningful fraction of it, and long enough that an idle link is not a
/// busy loop.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// [`RECV_TIMEOUT`] for a link that is decoding audio.
///
/// A reflector sends one radio frame every 100 ms carrying five 20 ms voice
/// frames, and the vocoder accepts only [`AMBE_STREAM_MAX_IN_FLIGHT`] (4) at
/// a time — so a burst cannot be submitted in one go, and the loop has to
/// come back around to feed the rest. Waking roughly five times per arriving
/// frame keeps the device fed without blocking the link thread in a drain
/// loop, which would add jitter to the polls that hold the session open.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(20);

/// Bound on the end-of-transmission drain, so a wedged or unplugged dongle
/// ends a transmission late rather than hanging the link thread — and with
/// it `disconnect()` and `Drop` (the iax-239a rule: a dead device surfaces
/// as an error, never as a hang).
const FLUSH_DEADLINE: Duration = Duration::from_millis(500);

/// How long the drain sleeps between empty polls, rather than spinning.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// One voice frame's worth of wall clock. The output bus consumes frames at
/// exactly this rate, so this is the rate they must be handed over at.
const FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// Decoded frames to accumulate before releasing the first one.
///
/// YSF is the only network here whose wire cadence is bursty: five 20 ms
/// frames arrive together every 100 ms, where D-Star sends one every 20 ms
/// and is paced by the air itself. Handing a burst straight to the bus
/// empties the vocoder in ~40 ms and then starves the bus for ~60, which is
/// heard as a beat at roughly two per second. So the frames are released on
/// the audio clock instead, and this cushion is what absorbs the arrival
/// jitter — the same reason D-Star primes, at the same cost of ~60 ms of
/// one-way latency.
const PRIME_FRAMES: usize = 3;

/// What the control side can see of a link, without touching the thread.
///
/// `PartialEq` but not `Eq`: it carries a level in dBFS, and `f32` has no
/// total equality. `DstarSnapshotState` derives the same pair for the same
/// reason.
#[derive(Debug, Clone, PartialEq)]
pub struct YsfSnapshot {
    /// The link state's ABI string — `idle`, `linking`, `linked`,
    /// `unlinking`, `failed`. See [`LinkState::as_str`], which documents why
    /// these strings are not a debug convenience.
    pub link_state: &'static str,
    /// Callsign of whoever transmitted most recently, if anyone has.
    /// Read from the `YSFD` header, so it needs no vocoder.
    pub last_heard: Option<String>,
    /// Radio frames received since the link came up. A liveness counter —
    /// a link that is up but silent and one that is receiving look the same
    /// from `link_state` alone.
    pub frames_rx: u64,
    /// Whether a transmission is in progress, from the header's end flag.
    pub receiving: bool,
    /// The last frame mode this link could not decode, as
    /// [`DataType::as_str`] — `voice-fr` (VW) or `data-fr`. `None` until one
    /// arrives, and it is never cleared: a caller that has seen it once has
    /// something true to tell the operator, and clearing it on the next DN
    /// frame would make a mixed-mode reflector flicker.
    ///
    /// Always `None` on a link opened without audio, which decodes nothing
    /// and therefore refuses nothing.
    pub unsupported_mode: Option<&'static str>,
    /// Which vocoder backend is decoding, or `None` for a link with no
    /// audio. An ABI string via [`AmbeBackend::as_str`].
    pub backend: Option<&'static str>,
    /// Receive level in dBFS on this link's output bus, or -60.0 when
    /// nothing is being decoded — mirrors [`AudioRouter::output_rx_dbfs`],
    /// refreshed every run-loop pass. Always -60.0 on a link opened without
    /// audio, which has no bus to meter.
    pub rx_dbfs: f32,
}

/// Operator-supplied configuration for a link that decodes audio.
///
/// Mirrors [`crate::dstar::DstarConfig`] minus everything that only a
/// transmitter needs: there is no capture device here because there is no
/// TX path (see this module's docs).
pub struct YsfConfig {
    /// Reflector `host:port`, as [`YsfLink::connect`] takes it.
    pub host: String,
    /// This station's callsign.
    pub callsign: String,
    /// The YCS room request (`set_options`); `None` for a plain
    /// `YSFReflector`.
    pub options: Option<String>,
    /// Playback device substring; `None` = system default.
    pub output: Option<String>,
}

/// A live link to one `YSFReflector`.
///
/// `Debug` reports the snapshot rather than the internals: the thread handle
/// and the socket are not something a caller can act on, and the link state
/// is.
pub struct YsfLink {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// `Some` only for a link opened with audio.
    backend: Option<AmbeBackend>,
}

#[derive(Debug)]
struct Shared {
    /// `LinkState` as its discriminant index; see `state_from_u32`.
    link_state: AtomicU32,
    last_heard: Mutex<Option<String>>,
    frames_rx: AtomicU64,
    receiving: AtomicBool,
    stop: AtomicBool,
    /// [`DataType::as_str`] for the last mode refused, or empty. A mutex
    /// rather than an atomic because the value is a `&'static str` chosen
    /// from a closed set and read far less often than it is skipped.
    unsupported_mode: Mutex<Option<&'static str>>,
    /// Listener-side preferences, re-asserted onto the output bus every
    /// pass. Same three cells and the same reason as
    /// [`crate::dstar`]'s: two networks applying the same preferences by
    /// different mechanisms is how one of them silently stops applying them.
    output_gain: AtomicU32,
    rx_compress: AtomicBool,
    rx_compress_level: AtomicU32,
    /// Peak-hold decay for the RX analyzer, pushed onto the bus by
    /// `apply_audio` like the other listener preferences.
    spectrum_decay: AtomicU32,
    /// Receive level, refreshed by the run loop from the router.
    rx_dbfs: AtomicU32,
    /// `(bins, count)` from [`AudioRouter::output_rx_spectrum`], refreshed
    /// every run-loop pass. `count` stays 0 until the bus has produced a
    /// reading, mirroring the router's own "nothing to report yet" contract
    /// rather than publishing a zeroed array as though it were real.
    ///
    /// A mutex rather than atomics because it is an array: the run loop
    /// writes it once per pass and a UI reads it at frame rate, so it is
    /// uncontended in practice.
    rx_spectrum: Mutex<([f32; astar_audio::SPECTRUM_BINS], usize)>,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            link_state: AtomicU32::new(state_index(LinkState::Idle)),
            last_heard: Mutex::new(None),
            frames_rx: AtomicU64::new(0),
            receiving: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            unsupported_mode: Mutex::new(None),
            output_gain: AtomicU32::new(1.0f32.to_bits()),
            rx_compress: AtomicBool::new(false),
            rx_compress_level: AtomicU32::new(0.5f32.to_bits()),
            spectrum_decay: AtomicU32::new(
                astar_audio::spectrum::DEFAULT_DECAY_DB_PER_SEC.to_bits(),
            ),
            rx_dbfs: AtomicU32::new((-60.0f32).to_bits()),
            rx_spectrum: Mutex::new(([0.0; astar_audio::SPECTRUM_BINS], 0)),
        }
    }

    /// Push the operator's volume and RX leveling onto the output bus.
    /// Cheap enough to run every pass — three atomic loads and the router's
    /// own atomic stores — so no dirty-flag tracking.
    fn apply_audio(&self, router: &AudioRouter, out: &OutputId) {
        router.set_output_gain(
            out,
            f32::from_bits(self.output_gain.load(Ordering::Relaxed)),
        );
        router.set_output_compress(out, self.rx_compress.load(Ordering::Relaxed));
        router.set_output_compress_level(
            out,
            f32::from_bits(self.rx_compress_level.load(Ordering::Relaxed)),
        );
        router.set_output_spectrum_decay(
            out,
            f32::from_bits(self.spectrum_decay.load(Ordering::Relaxed)),
        );
    }

    /// Pull the bus's meter and analyzer into the cells the control side
    /// reads. Called every run-loop pass, next to `apply_audio`, so the two
    /// directions of the same conversation with the router stay together.
    fn read_meters(&self, router: &AudioRouter, out: &OutputId, buf: &mut [f32]) {
        if let Some(db) = router.output_rx_dbfs(out) {
            self.rx_dbfs.store(db.to_bits(), Ordering::Relaxed);
        }
        if let Some(n) = router.output_rx_spectrum(out, buf)
            && let Ok(mut slot) = self.rx_spectrum.lock()
        {
            let n = n.min(astar_audio::SPECTRUM_BINS);
            slot.0[..n].copy_from_slice(&buf[..n]);
            slot.1 = n;
        }
    }
}

/// Everything the run loop needs to turn payload bytes into sound. Owned by
/// the link thread; `None` for a link opened without audio.
struct Audio {
    ambe: Box<dyn AmbeStream>,
    /// The output bus decoded frames are played on.
    bus: CallAudio,
    /// Arrived-but-not-yet-submitted voice frames. A payload carries five
    /// and the vocoder accepts four, so this queue is not an optimisation —
    /// without it the fifth frame of every burst would be dropped.
    pending: VecDeque<DnFrame>,
    /// Decoded frames waiting to be released to the bus on the audio clock.
    /// See [`PRIME_FRAMES`] for why they are not handed over as they finish.
    decoded: VecDeque<[i16; 160]>,
    /// When the next frame is due. `None` while re-priming — before the
    /// first frame of a transmission, and after the queue has run dry.
    next_release: Option<Instant>,
    router: AudioRouter,
    out: OutputId,
}

/// `LinkState` has no numeric repr of its own — it is a protocol type and
/// does not need one — so the mapping lives here, next to the atomic it
/// exists for, rather than being pushed into the protocol crate.
fn state_index(s: LinkState) -> u32 {
    match s {
        LinkState::Idle => 0,
        LinkState::Linking => 1,
        LinkState::Linked => 2,
        LinkState::Unlinking => 3,
        LinkState::Failed => 4,
    }
}

/// The typed inverse of [`state_index`].
///
/// Exists so callers that must branch on the link state — the console's
/// snapshot mirror in particular — match an enum the compiler can check
/// rather than the ABI strings. A new `LinkState` variant then breaks those
/// call sites at compile time instead of silently falling into a catch-all.
fn state_from_index(i: u32) -> LinkState {
    match i {
        1 => LinkState::Linking,
        2 => LinkState::Linked,
        3 => LinkState::Unlinking,
        4 => LinkState::Failed,
        _ => LinkState::Idle,
    }
}

fn state_str(i: u32) -> &'static str {
    match i {
        1 => LinkState::Linking.as_str(),
        2 => LinkState::Linked.as_str(),
        3 => LinkState::Unlinking.as_str(),
        4 => LinkState::Failed.as_str(),
        _ => LinkState::Idle.as_str(),
    }
}

impl YsfLink {
    /// Resolve `host`, bind a local socket, and start polling.
    ///
    /// `options` is the YCS room request (`set_options`); `None` for a plain
    /// `YSFReflector`.
    ///
    /// # Errors
    /// [`ConsoleError::Ysf`] if the callsign is not a valid YSF callsign, if
    /// `host` does not resolve, or if the socket cannot be bound.
    pub fn connect(
        host: &str,
        callsign: &str,
        options: Option<String>,
    ) -> Result<YsfLink, ConsoleError> {
        Self::spawn(host, callsign, options, None, None)
    }

    /// Open a link that decodes the audio on it.
    ///
    /// Opens a `ThumbDV` in [`VocoderMode::YsfDn`] and an output bus, then
    /// plays every DN frame the reflector sends. VW and data frames are
    /// refused into [`YsfSnapshot::unsupported_mode`] rather than decoded.
    ///
    /// There is no transmit path — see this module's docs for the specific
    /// reason, which is the vendored deframer and not effort.
    ///
    /// # Errors
    /// Everything [`YsfLink::connect`] can fail with, plus
    /// [`ConsoleError::Ysf`] when no `ThumbDV` is available (the message
    /// comes from `classify_thumbdv_failure`, so "unplugged" and "busy" are
    /// told apart) and [`ConsoleError::Audio`] when the output device cannot
    /// be opened.
    pub fn connect_with_audio(
        cfg: &YsfConfig,
        make_backend: &dyn Fn() -> Box<dyn AudioBackend>,
    ) -> Result<YsfLink, ConsoleError> {
        // Hardware-only, exactly as D-Star: no software AMBE exists, so a
        // missing dongle is an error with a reason rather than a silent
        // fallback to a link that makes no sound.
        let (ambe, backend) = open_ambe_stream(Some(AmbeBackend::Hardware), VocoderMode::YsfDn)
            .ok_or_else(|| {
                ConsoleError::Ysf(astar_codec::ambe::classify_thumbdv_failure().message())
            })?;
        Self::connect_with_stream(cfg, make_backend, ambe, backend)
    }

    /// [`Self::connect_with_audio`] with the vocoder supplied by the caller.
    ///
    /// The seam the tests use: a fake [`AmbeStream`] decodes without a
    /// dongle, so the whole payload-to-speaker path is provable against the
    /// loopback reflector on `127.0.0.1`.
    ///
    /// # Errors
    /// As [`Self::connect_with_audio`], minus the `ThumbDV` probe.
    pub fn connect_with_stream(
        cfg: &YsfConfig,
        make_backend: &dyn Fn() -> Box<dyn AudioBackend>,
        ambe: Box<dyn AmbeStream>,
        backend: AmbeBackend,
    ) -> Result<YsfLink, ConsoleError> {
        let backend_audio = make_backend();
        let out_id = resolve_device(
            backend_audio.as_ref(),
            cfg.output.as_deref(),
            astar_audio::Direction::Output,
        )?;
        let mut router = AudioRouter::new(backend_audio);
        let out = OutputId::new(&out_id);
        // 8 kHz mono 20 ms: AMBE+2 half-rate decodes to exactly 160 samples
        // per 20 ms frame, the same shape D-Star's full-rate frames take.
        let (call_audio, _mic_tx, _mix_id) = router
            .open_monitor_call(&out, StreamConfig::default())
            .map_err(ConsoleError::Audio)?;

        let audio = Audio {
            ambe,
            bus: call_audio,
            pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            router,
            out,
        };
        Self::spawn(
            &cfg.host,
            &cfg.callsign,
            cfg.options.clone(),
            Some(audio),
            Some(backend),
        )
    }

    /// The body both constructors share: validate, resolve, bind, spawn.
    fn spawn(
        host: &str,
        callsign: &str,
        options: Option<String>,
        audio: Option<Audio>,
        backend: Option<AmbeBackend>,
    ) -> Result<YsfLink, ConsoleError> {
        let mut fsm =
            YsfFsm::new(callsign).map_err(|e| ConsoleError::Ysf(format!("callsign: {e}")))?;
        fsm.set_options(options);

        let addr: SocketAddr = host
            .to_socket_addrs()
            .map_err(|e| ConsoleError::Ysf(format!("resolve {host}: {e}")))?
            .next()
            .ok_or_else(|| ConsoleError::Ysf(format!("resolve {host}: no addresses")))?;

        // Bind to the unspecified address on an ephemeral port, matching the
        // family of whatever we resolved to.
        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 bind")
        } else {
            "[::]:0".parse().expect("valid v6 bind")
        };
        let socket = UdpSocket::bind(bind).map_err(|e| ConsoleError::Ysf(format!("bind: {e}")))?;
        let timeout = if audio.is_some() {
            AUDIO_RECV_TIMEOUT
        } else {
            RECV_TIMEOUT
        };
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|e| ConsoleError::Ysf(format!("socket timeout: {e}")))?;

        let shared = Arc::new(Shared::new());

        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("astar-ysf-link".into())
                .spawn(move || run(&socket, addr, fsm, &shared, audio))
                .map_err(|e| ConsoleError::Ysf(format!("thread: {e}")))?
        };

        Ok(YsfLink {
            shared,
            thread: Some(thread),
            backend,
        })
    }

    /// Current link state, cheap enough to poll.
    #[must_use]
    pub fn snapshot(&self) -> YsfSnapshot {
        YsfSnapshot {
            link_state: state_str(self.shared.link_state.load(Ordering::Relaxed)),
            last_heard: self.shared.last_heard.lock().map_or(None, |g| g.clone()),
            frames_rx: self.shared.frames_rx.load(Ordering::Relaxed),
            receiving: self.shared.receiving.load(Ordering::Relaxed),
            unsupported_mode: self.shared.unsupported_mode.lock().map_or(None, |g| *g),
            backend: self.backend.map(AmbeBackend::as_str),
            rx_dbfs: f32::from_bits(self.shared.rx_dbfs.load(Ordering::Relaxed)),
        }
    }

    /// Copy the live RX spectrum into `out`, returning the number of
    /// log-binned, peak-held dBFS bins written — the SAME values the mic
    /// monitor and the IAX2/M17 paths produce, so one UI widget renders them
    /// all. `0` before the bus has produced a reading, and always `0` on a
    /// link opened without audio.
    #[must_use]
    pub fn rx_spectrum(&self, out: &mut [f32]) -> usize {
        let Ok(slot) = self.shared.rx_spectrum.lock() else {
            return 0;
        };
        let n = slot.1.min(out.len());
        out[..n].copy_from_slice(&slot.0[..n]);
        n
    }

    /// Set the RX analyzer's peak-hold decay in dB/second. Applied to the
    /// live bus on the next run-loop pass.
    pub fn set_spectrum_decay(&self, db_per_sec: f32) {
        self.shared
            .spectrum_decay
            .store(db_per_sec.to_bits(), Ordering::Relaxed);
    }

    /// The link state as the protocol crate's own enum.
    ///
    /// [`YsfSnapshot::link_state`] carries the ABI string for anything
    /// crossing a boundary; this is for in-process callers that need to
    /// branch, so they get an exhaustive match instead of string comparison.
    #[must_use]
    pub fn link_state(&self) -> LinkState {
        state_from_index(self.shared.link_state.load(Ordering::Relaxed))
    }

    /// Set the output (RX/speaker) gain multiplier, 0.0..=4.0 (clamped).
    ///
    /// `&self`, as [`crate::dstar::DstarSession::set_output_gain`], so a
    /// preference can be fanned out to every live network without a mutable
    /// borrow of each. A no-op on a link with no audio.
    pub fn set_output_gain(&self, gain: f32) {
        let gain = if gain.is_nan() {
            1.0
        } else {
            gain.clamp(0.0, 4.0)
        };
        self.shared
            .output_gain
            .store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Toggle automatic leveling of the received audio.
    pub fn set_rx_compression(&self, on: bool) {
        self.shared.rx_compress.store(on, Ordering::Relaxed);
    }

    /// Set the RX compression strength (0.0..=1.0, clamped).
    pub fn set_rx_compression_level(&self, level: f32) {
        let level = if level.is_nan() {
            0.5
        } else {
            level.clamp(0.0, 1.0)
        };
        self.shared
            .rx_compress_level
            .store(level.to_bits(), Ordering::Relaxed);
    }

    /// The listener-side preferences currently in force — for tests, and for
    /// anything that needs to prove a fan-out reached this link.
    #[must_use]
    pub fn audio_prefs(&self) -> (f32, bool, f32) {
        (
            f32::from_bits(self.shared.output_gain.load(Ordering::Relaxed)),
            self.shared.rx_compress.load(Ordering::Relaxed),
            f32::from_bits(self.shared.rx_compress_level.load(Ordering::Relaxed)),
        )
    }

    /// Send the unlink and stop the thread. Consumes the link.
    pub fn disconnect(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl std::fmt::Debug for YsfLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YsfLink")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl Drop for YsfLink {
    /// Dropping a link unlinks it. A `YSFReflector` would otherwise keep the
    /// client until its own timeout expires, and a caller who dropped the
    /// handle has plainly finished with it.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The link thread's body. The socket is owned by the spawned closure for
/// the thread's lifetime and borrowed here.
fn run(
    socket: &UdpSocket,
    addr: SocketAddr,
    mut fsm: YsfFsm,
    shared: &Arc<Shared>,
    mut audio: Option<Audio>,
) {
    let publish = |fsm: &YsfFsm| {
        shared
            .link_state
            .store(state_index(fsm.state()), Ordering::Relaxed);
    };

    for datagram in fsm.connect(Instant::now()) {
        if socket.send_to(&datagram, addr).is_err() {
            shared
                .link_state
                .store(state_index(LinkState::Failed), Ordering::Relaxed);
            return;
        }
    }
    publish(&fsm);

    let mut buf = [0_u8; astar_ysf::reflector::MAX_DATAGRAM];
    // Reused every pass, never reallocated.
    let mut spectrum_buf = [0.0f32; astar_audio::SPECTRUM_BINS];
    while !shared.stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == addr => {
                let action = fsm.on_packet(&buf[..n], Instant::now());
                if !handle(&action, socket, addr, shared, audio.as_mut()) {
                    break;
                }
            }
            // A datagram from somewhere else on our ephemeral port. Ignore
            // it rather than feeding the FSM a stranger's bytes.
            Ok(_) => {}
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => break,
        }
        let action = fsm.tick(Instant::now());
        if !handle(&action, socket, addr, shared, audio.as_mut()) {
            break;
        }
        publish(&fsm);

        if let Some(a) = audio.as_mut() {
            // Keep the vocoder fed and the speaker supplied between
            // arrivals: a payload is five frames and only four fit in the
            // pipeline at once, so the rest are submitted here.
            pump(a);
            // The operator's volume and leveling, re-asserted every pass so
            // a change made mid-transmission is heard on the next frame.
            shared.apply_audio(&a.router, &a.out);
            // …and the other direction: the bus's meter and analyzer into
            // the cells a UI polls. Without this a YSF link plays audio
            // while every level and every spectrum bar sits at the floor,
            // which reads as a dead session.
            shared.read_meters(&a.router, &a.out, &mut spectrum_buf);
        }
    }

    // Best effort: the reflector drops us on its own timeout anyway, and a
    // failed send here is not worth reporting to a caller that has gone.
    for datagram in fsm.unlink(Instant::now()) {
        let _ = socket.send_to(&datagram, addr);
    }
    shared
        .link_state
        .store(state_index(LinkState::Idle), Ordering::Relaxed);
}

/// Apply one action. Returns false when the loop should stop.
fn handle(
    action: &FsmAction,
    socket: &UdpSocket,
    addr: SocketAddr,
    shared: &Arc<Shared>,
    audio: Option<&mut Audio>,
) -> bool {
    match action {
        FsmAction::None | FsmAction::Linked => true,
        FsmAction::Send(bytes) => socket.send_to(bytes, addr).is_ok(),
        FsmAction::Data(packet) => {
            shared.frames_rx.fetch_add(1, Ordering::Relaxed);
            shared.receiving.store(!packet.end, Ordering::Relaxed);
            // The source callsign is in the header, in clear — no vocoder
            // involved. `to_trimmed_string` drops the space padding the
            // ten-byte wire field requires.
            let source = packet.source.to_trimmed_string();
            if !source.is_empty()
                && let Ok(mut slot) = shared.last_heard.lock()
            {
                *slot = Some(source);
            }
            if let Some(a) = audio {
                decode_frame(&packet.frame, a, shared);
                if packet.end {
                    // The tail of a transmission is still working through
                    // the pipeline when its last frame arrives. Play it, or
                    // every transmission loses its final ~100 ms — and one
                    // shorter than the pipeline depth would be silent.
                    flush(a);
                }
            }
            true
        }
        FsmAction::Unlinked | FsmAction::Timeout => {
            shared.receiving.store(false, Ordering::Relaxed);
            false
        }
    }
}

/// Turn one received radio frame into voice on the output bus.
///
/// Everything that can go wrong here is a property of the frame, not of the
/// link, so nothing in this function tears the session down: a malformed
/// FICH is one lost frame, and an undecodable mode is recorded where the
/// operator can be told about it.
fn decode_frame(bytes: &[u8; astar_ysf::FRAME_LEN], audio: &mut Audio, shared: &Arc<Shared>) {
    let Some(frame) = Frame::new(bytes) else {
        // Unreachable given the array length, but `Frame::new` is the
        // crate's only way in and a panic here would kill the link thread.
        return;
    };
    let fich = match frame.fich() {
        Ok(f) => f,
        Err(e) => {
            // The FICH is Golay- and interleave-protected; one that still
            // fails to decode is a corrupted frame, which happens on a lossy
            // path and is not worth more than a debug line.
            tracing::debug!(error = ?e, "ysf: undecodable FICH, dropping the frame");
            return;
        }
    };
    match unpack_dn(fich.data_type, frame.payload()) {
        Ok(frames) => {
            audio.pending.extend(frames);
            pump(audio);
        }
        Err(DnError::UnsupportedMode { mode }) => note_unsupported(mode, shared),
        Err(e) => {
            tracing::debug!(error = ?e, "ysf: could not unpack a DN payload");
        }
    }
}

/// Record a mode astar cannot decode, once per mode, where a caller can see
/// it — and say so in the log the first time.
///
/// Silence with no reason attached is the worst outcome this network has, so
/// the refusal is deliberately louder than the frame that caused it.
fn note_unsupported(mode: DataType, shared: &Arc<Shared>) {
    let Ok(mut slot) = shared.unsupported_mode.lock() else {
        return;
    };
    if *slot != Some(mode.as_str()) {
        tracing::warn!(
            mode = mode.as_str(),
            "ysf: this reflector is sending a mode astar cannot decode; \
             astar decodes DN (V/D modes 1 and 2) only"
        );
        *slot = Some(mode.as_str());
    }
}

/// Feed the vocoder while it has room, then drain whatever it has ready.
///
/// Nothing here blocks: `submit_decode` is guarded by the in-flight bound so
/// it is always accepted rather than dropped, and `poll_decoded` is
/// non-blocking by contract.
fn pump(audio: &mut Audio) {
    while audio.ambe.in_flight() < AMBE_STREAM_MAX_IN_FLIGHT {
        let Some(frame) = audio.pending.pop_front() else {
            break;
        };
        audio.ambe.submit_decode(frame.into());
    }
    // Decoded frames go to a queue, NOT straight to the bus: the vocoder
    // finishes a whole payload's worth in ~40 ms and the bus wants them
    // spread over 100. See `release`.
    while let Some(pcm) = audio.ambe.poll_decoded() {
        audio.decoded.push_back(pcm);
    }
    release(audio, Instant::now());
}

/// Hand decoded frames to the output bus on the audio clock — one per
/// [`FRAME_INTERVAL`] — rather than as fast as the vocoder produces them.
///
/// The bus consumes at exactly 50 frames a second. Handing it five at once
/// and then nothing for 60 ms makes it starve in the hole, which is audible
/// as a beat; this is what turns a bursty wire cadence back into a steady
/// one.
///
/// Running dry re-primes rather than free-running: a queue that has emptied
/// means the cushion was too small for the jitter actually seen, and
/// releasing the next frame the instant it arrives would just reopen the
/// same hole.
fn release(audio: &mut Audio, now: Instant) {
    if audio.next_release.is_none() {
        if audio.decoded.len() < PRIME_FRAMES {
            return;
        }
        audio.next_release = Some(now);
    }
    while let Some(due) = audio.next_release {
        if now < due {
            break;
        }
        let Some(pcm) = audio.decoded.pop_front() else {
            audio.next_release = None;
            break;
        };
        let _ = audio.bus.rx_frames.send(pcm.to_vec());
        audio.next_release = Some(due + FRAME_INTERVAL);
    }
}

/// Empty the decode path — queued frames and everything still in flight —
/// and play all of it.
///
/// Bounded by [`FLUSH_DEADLINE`]: without a bound, a dongle that stops
/// answering mid-transmission would hang the link thread, and with it
/// `disconnect()` and `Drop`.
fn flush(audio: &mut Audio) {
    let deadline = Instant::now() + FLUSH_DEADLINE;
    loop {
        pump(audio);
        if audio.pending.is_empty() && audio.ambe.in_flight() == 0 {
            drain_tail(audio);
            return;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                pending = audio.pending.len(),
                in_flight = audio.ambe.in_flight(),
                "ysf: vocoder flush hit its {FLUSH_DEADLINE:?} deadline, abandoning the rest"
            );
            break;
        }
        thread::sleep(DRAIN_POLL_INTERVAL);
    }
    audio.pending.clear();
    drain_tail(audio);
}

/// Push whatever is still queued at end-of-transmission and re-prime.
///
/// Unpaced, deliberately: this is the tail of an over that has already
/// finished, so a short burst at the end costs nothing an ear will notice,
/// where holding it back would clip the last few frames. Clearing
/// `next_release` makes the NEXT transmission prime again from scratch
/// instead of inheriting a stale clock.
fn drain_tail(audio: &mut Audio) {
    while let Some(pcm) = audio.decoded.pop_front() {
        let _ = audio.bus.rx_frames.send(pcm.to_vec());
    }
    audio.next_release = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_codec::ambe::ChannelFrame;
    use astar_codec::ysf::{FRAMES_PER_PAYLOAD, pack_dn};
    use astar_ysf::{Fich, Reflector};
    use std::sync::mpsc::{Receiver, channel};

    /// Everything here binds `127.0.0.1` and talks to a reflector this test
    /// started. Nothing reaches a real network — see CLAUDE.md's on-air
    /// safety rule, which this crate is squarely inside.
    fn loopback() -> (astar_ysf::ReflectorHandle, SocketAddr) {
        let r = Reflector::bind("127.0.0.1:0".parse().expect("v4")).expect("bind reflector");
        let addr = r.local_addr();
        (r.run(), addr)
    }

    fn wait_for(link: &YsfLink, want: &str, within: Duration) -> YsfSnapshot {
        let deadline = Instant::now() + within;
        loop {
            let snap = link.snapshot();
            if snap.link_state == want || Instant::now() > deadline {
                return snap;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The whole point of the link: polls go out, the reflector's poll comes
    /// back, and that round trip is the acknowledgement. Nothing about this
    /// needs a vocoder.
    #[test]
    fn linking_to_a_reflector_reaches_linked() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");

        let snap = wait_for(&link, "linked", Duration::from_secs(5));
        assert_eq!(snap.link_state, "linked", "link never came up: {snap:?}");
        assert_eq!(snap.frames_rx, 0, "a poll is not a radio frame");
        assert!(snap.last_heard.is_none(), "nobody has transmitted");

        link.disconnect();
        reflector.shutdown();
    }

    /// A callsign the wire format cannot carry is refused before a socket is
    /// bound — the error names the problem rather than surfacing as a failed
    /// link a caller has to diagnose.
    #[test]
    fn an_impossible_callsign_is_refused_at_connect() {
        let err = YsfLink::connect("127.0.0.1:1", "WAY-TOO-LONG-FOR-YSF", None)
            .expect_err("should refuse");
        let text = err.to_string();
        assert!(text.contains("ysf:"), "{text}");
        assert!(text.contains("callsign"), "{text}");
    }

    /// An unresolvable host fails at connect, not later and not silently.
    #[test]
    fn an_unresolvable_host_is_refused_at_connect() {
        let err = YsfLink::connect("no-such-host.invalid:42000", "N0CALL", None)
            .expect_err("should refuse");
        assert!(err.to_string().contains("resolve"), "{err}");
    }

    /// Dropping the handle unlinks and joins the thread. A link that outlived
    /// its handle would hold a socket and keep polling a reflector nobody is
    /// listening to.
    #[test]
    fn dropping_the_handle_stops_the_thread() {
        let (reflector, addr) = loopback();
        {
            let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");
            let _ = wait_for(&link, "linked", Duration::from_secs(5));
        }
        // The reflector drops a client on its own timeout too, so this is
        // asserting the thread joined rather than the reflector noticing.
        reflector.shutdown();
    }

    /// The snapshot is readable immediately, before the link has settled —
    /// a UI polls it from the first frame it draws.
    #[test]
    fn snapshot_is_readable_before_the_link_comes_up() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");
        let snap = link.snapshot();
        assert!(
            ["idle", "linking", "linked"].contains(&snap.link_state),
            "unexpected early state {snap:?}"
        );
        link.disconnect();
        reflector.shutdown();
    }

    // ── The receive path (astar-e7b3 §2) ────────────────────────────────

    /// A vocoder that answers immediately and remembers what it was asked.
    ///
    /// Each frame decodes to 160 samples of its own first byte, so decoded
    /// audio can be attributed to the exact frame that produced it.
    struct FakeVocoder {
        submitted: Vec<ChannelFrame>,
        ready: VecDeque<[i16; 160]>,
    }

    impl FakeVocoder {
        fn new() -> FakeVocoder {
            FakeVocoder {
                submitted: Vec::new(),
                ready: VecDeque::new(),
            }
        }
    }

    impl AmbeStream for FakeVocoder {
        fn submit_decode(&mut self, frame: ChannelFrame) {
            let value = i16::from(frame.as_slice()[0]);
            self.submitted.push(frame);
            self.ready.push_back([value; 160]);
        }
        fn poll_decoded(&mut self) -> Option<[i16; 160]> {
            self.ready.pop_front()
        }
        fn in_flight(&self) -> usize {
            self.ready.len()
        }
        fn submit_encode(&mut self, _pcm: [i16; 160]) {
            unreachable!("YSF has no transmit path");
        }
        fn poll_encoded(&mut self) -> Option<[u8; 9]> {
            None
        }
        fn in_flight_encoded(&self) -> usize {
            0
        }
    }

    /// An `Audio` wired to channels the test can read, with no device
    /// anywhere: `NullBackend` opens nothing and `CallAudio`'s fields are
    /// public, so the decoded PCM lands somewhere assertable.
    fn test_audio() -> (Audio, Receiver<Vec<i16>>) {
        let (rx_tx, rx_rx) = channel::<Vec<i16>>();
        let (_tx_tx, tx_rx) = channel::<Vec<i16>>();
        let call_audio = CallAudio {
            tx_frames: tx_rx,
            rx_frames: rx_tx,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        };
        let router = AudioRouter::new(Box::new(astar_audio::NullBackend::new()));
        (
            Audio {
                ambe: Box::new(FakeVocoder::new()),
                bus: call_audio,
                pending: VecDeque::new(),
                decoded: VecDeque::new(),
                next_release: None,
                router,
                out: OutputId::new("out:test"),
            },
            rx_rx,
        )
    }

    /// One radio frame in `data_type`, carrying five distinguishable voice
    /// frames. Built with the protocol crate's own encoder, so the test is
    /// reading the same layout the decoder is.
    fn frame_of(data_type: DataType) -> [u8; astar_ysf::FRAME_LEN] {
        let fich = Fich {
            data_type,
            ..Fich::default()
        };
        let mut payload = [0u8; astar_ysf::PAYLOAD_LEN];
        if data_type.is_half_rate_voice() {
            let voice: [DnFrame; FRAMES_PER_PAYLOAD] = core::array::from_fn(|i| {
                DnFrame::from_bytes([u8::try_from(i + 1).expect("small"), 0, 0, 0, 0, 0, 0])
            });
            pack_dn(data_type, &voice, &mut payload).expect("pack");
        }
        astar_ysf::frame::build(fich, &payload)
    }

    #[test]
    fn a_dn_payload_becomes_five_frames_of_speech() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());

        decode_frame(&frame_of(DataType::VDMode2), &mut audio, &shared);
        flush(&mut audio);

        let played: Vec<Vec<i16>> = rx.try_iter().collect();
        assert_eq!(
            played.len(),
            FRAMES_PER_PAYLOAD,
            "one 100 ms radio frame carries five 20 ms voice frames"
        );
        for (i, pcm) in played.iter().enumerate() {
            assert_eq!(pcm.len(), 160, "8 kHz, 20 ms");
            assert_eq!(
                pcm[0],
                i16::try_from(i + 1).expect("small"),
                "frames must reach the speaker in the order they were carried"
            );
        }
        assert_eq!(
            shared.unsupported_mode.lock().expect("mutex").as_ref(),
            None,
            "DN is decoded, not refused"
        );
    }

    /// Mode 1 is the rare one, and the one the two reference implementations
    /// disagree about — so it gets its own assertion rather than riding on
    /// mode 2's.
    #[test]
    fn mode_one_is_decoded_too() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());
        decode_frame(&frame_of(DataType::VDMode1), &mut audio, &shared);
        flush(&mut audio);
        assert_eq!(rx.try_iter().count(), FRAMES_PER_PAYLOAD);
    }

    #[test]
    fn vw_is_refused_out_loud_and_never_decoded() {
        // The worst outcome this network has is audio that is noise, or
        // silence with no reason attached. VW must produce neither: no
        // samples, and a refusal a caller can put in front of the operator.
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());

        decode_frame(&frame_of(DataType::VoiceFrMode), &mut audio, &shared);
        flush(&mut audio);

        assert_eq!(rx.try_iter().count(), 0, "VW must never become samples");
        assert_eq!(
            *shared.unsupported_mode.lock().expect("mutex"),
            Some("voice-fr"),
            "the refusal must name the mode"
        );
    }

    #[test]
    fn a_data_frame_is_refused_the_same_way() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());
        decode_frame(&frame_of(DataType::DataFrMode), &mut audio, &shared);
        assert_eq!(rx.try_iter().count(), 0);
        assert_eq!(
            *shared.unsupported_mode.lock().expect("mutex"),
            Some("data-fr")
        );
    }

    /// A payload carries five frames and the vocoder takes four, so the
    /// fifth exists only because `pending` does. Without the queue every
    /// transmission would lose a fifth of itself, evenly spread.
    #[test]
    fn the_fifth_frame_of_a_burst_is_not_dropped() {
        const {
            assert!(
                FRAMES_PER_PAYLOAD > AMBE_STREAM_MAX_IN_FLIGHT,
                "if this stops being true the queue's reason for existing has changed"
            );
        }
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());
        decode_frame(&frame_of(DataType::VDMode2), &mut audio, &shared);
        flush(&mut audio);
        assert_eq!(rx.try_iter().count(), FRAMES_PER_PAYLOAD);
        assert!(audio.pending.is_empty(), "the queue must drain");
    }

    /// Preferences round-trip through `f32::to_bits`, so they come back
    /// bit-identical — but comparing floats exactly is a habit worth not
    /// having in a test that might later grow a conversion.
    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    /// `release` on a fabricated clock — the pacing rule itself, with no
    /// wall-clock dependence, so it guards the behaviour in CI where the
    /// timing probe below would be flaky.
    #[test]
    fn frames_are_released_one_per_frame_interval_after_priming() {
        let (audio_rx_tx, rx) = channel::<Vec<i16>>();
        let (_t, tx_rx) = channel::<Vec<i16>>();
        let mut audio = Audio {
            ambe: Box::new(FakeVocoder::new()),
            bus: CallAudio {
                tx_frames: tx_rx,
                rx_frames: audio_rx_tx,
                preroll_lead: Arc::new(AtomicU32::new(0)),
            },
            pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            router: AudioRouter::new(Box::new(astar_audio::NullBackend::new())),
            out: OutputId::new("out:test"),
        };
        let t0 = Instant::now();

        // Below the cushion, nothing goes out at all — releasing early is
        // what reopens the hole the cushion exists to close.
        for _ in 0..(PRIME_FRAMES - 1) {
            audio.decoded.push_back([1i16; 160]);
        }
        release(&mut audio, t0);
        assert_eq!(rx.try_iter().count(), 0, "must not release under-primed");

        // At the cushion, exactly one frame goes out — not the whole queue.
        audio.decoded.push_back([1i16; 160]);
        for _ in 0..7 {
            audio.decoded.push_back([1i16; 160]);
        }
        release(&mut audio, t0);
        assert_eq!(rx.try_iter().count(), 1, "one frame, not the burst");

        // Nothing more until the next frame is due.
        release(&mut audio, t0 + Duration::from_millis(19));
        assert_eq!(rx.try_iter().count(), 0, "not due yet");

        release(&mut audio, t0 + FRAME_INTERVAL);
        assert_eq!(rx.try_iter().count(), 1, "exactly one per interval");

        // A late wake-up catches up rather than losing the time: three
        // intervals elapsed means three frames owed.
        release(&mut audio, t0 + FRAME_INTERVAL * 4);
        assert_eq!(
            rx.try_iter().count(),
            3,
            "catch-up is bounded by what is due"
        );

        // Running dry re-primes instead of free-running.
        while audio.decoded.pop_front().is_some() {}
        release(&mut audio, t0 + FRAME_INTERVAL * 10);
        assert!(
            audio.next_release.is_none(),
            "an emptied queue must re-prime, or the next frame reopens the gap"
        );
    }

    // ── Delivery cadence (astar-ysfbeat) ────────────────────────────────
    //
    // The output bus consumes 20 ms frames at a steady 50/s. What this
    // measures is whether the decode path DELIVERS them at that rate, or in
    // bursts with gaps the bus has to paper over — a gap is a dropout, and a
    // periodic gap is a beat.

    /// A vocoder with realistic latency: a frame submitted now is answered
    /// `LATENCY` later, matching the `ThumbDV`'s measured ~7.45 ms/frame
    /// pipelined cost. An instant fake would hide the very timing under test.
    struct LatentVocoder {
        queue: VecDeque<(Instant, [i16; 160])>,
        in_flight: usize,
    }

    const LATENCY: Duration = Duration::from_micros(7_450);

    impl AmbeStream for LatentVocoder {
        fn submit_decode(&mut self, frame: ChannelFrame) {
            let v = i16::from(frame.as_slice()[0]);
            self.queue.push_back((Instant::now() + LATENCY, [v; 160]));
            self.in_flight += 1;
        }
        fn poll_decoded(&mut self) -> Option<[i16; 160]> {
            if self
                .queue
                .front()
                .is_some_and(|(due, _)| Instant::now() >= *due)
            {
                self.in_flight -= 1;
                return self.queue.pop_front().map(|(_, p)| p);
            }
            None
        }
        fn in_flight(&self) -> usize {
            self.in_flight
        }
        fn submit_encode(&mut self, _pcm: [i16; 160]) {
            unreachable!()
        }
        fn poll_encoded(&mut self) -> Option<[u8; 9]> {
            None
        }
        fn in_flight_encoded(&self) -> usize {
            0
        }
    }

    /// Drives the real `decode_frame`/`pump` at the reflector's real cadence
    /// — one 5-frame payload every 100 ms, the run loop waking every 20 ms in
    /// between — and reports when audio actually reached the bus.
    #[test]
    #[ignore = "timing probe, run explicitly: cargo test -p astar-console --features ysf -- --ignored delivery"]
    fn delivery_cadence_probe() {
        let (rx_tx, rx_rx) = channel::<Vec<i16>>();
        let (_tx_tx, tx_rx) = channel::<Vec<i16>>();
        let mut audio = Audio {
            ambe: Box::new(LatentVocoder {
                queue: VecDeque::new(),
                in_flight: 0,
            }),
            bus: CallAudio {
                tx_frames: tx_rx,
                rx_frames: rx_tx,
                preroll_lead: Arc::new(AtomicU32::new(0)),
            },
            pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            router: AudioRouter::new(Box::new(astar_audio::NullBackend::new())),
            out: OutputId::new("out:test"),
        };
        let shared = Arc::new(Shared::new());
        let frame = frame_of(DataType::VDMode2);

        let t0 = Instant::now();
        let mut arrivals = Vec::new();
        // 60 payloads = 6 seconds of continuous speech, which is the "after a
        // few seconds" the beat is reported at.
        for payload in 0..60 {
            let due = t0 + Duration::from_millis(payload * 100);
            while Instant::now() < due {
                std::thread::sleep(Duration::from_millis(20));
                pump(&mut audio);
                while rx_rx.try_recv().is_ok() {
                    arrivals.push(Instant::now().duration_since(t0).as_secs_f64());
                }
            }
            decode_frame(&frame, &mut audio, &shared);
            while rx_rx.try_recv().is_ok() {
                arrivals.push(Instant::now().duration_since(t0).as_secs_f64());
            }
        }
        flush(&mut audio);
        while rx_rx.try_recv().is_ok() {
            arrivals.push(Instant::now().duration_since(t0).as_secs_f64());
        }

        println!("payloads sent: 60 (= 300 voice frames = 6.00s of audio)");
        println!("frames delivered: {}", arrivals.len());
        println!("pending left over: {}", audio.pending.len());
        let gaps: Vec<f64> = arrivals.windows(2).map(|w| w[1] - w[0]).collect();
        let over = gaps.iter().filter(|g| **g > 0.030).count();
        println!("inter-frame gaps > 30ms: {over}");
        let mut sorted = gaps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if !sorted.is_empty() {
            println!(
                "gap p50={:.1}ms p90={:.1}ms max={:.1}ms",
                sorted[sorted.len() / 2] * 1000.0,
                sorted[sorted.len() * 9 / 10] * 1000.0,
                sorted[sorted.len() - 1] * 1000.0
            );
        }
        println!(
            "delivery spans {:.2}s for 6.00s of audio",
            arrivals.last().unwrap_or(&0.0)
        );
        // How often does a gap exceed what a small bus buffer can absorb?
        // That, not the gap count, is what an ear hears as a beat.
        for thresh_ms in [40.0, 60.0, 70.0, 80.0] {
            let n = gaps.iter().filter(|g| **g * 1000.0 >= thresh_ms).count();
            println!(
                "  gaps >= {:>3.0}ms: {:>3}  = {:.1}/second",
                thresh_ms,
                n,
                f64::from(u32::try_from(n).unwrap_or(u32::MAX)) / 6.0
            );
        }
    }

    #[test]
    fn audio_preferences_are_stored_and_clamped() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");

        let (gain, compress, level) = link.audio_prefs();
        assert!(near(gain, 1.0) && !compress && near(level, 0.5), "defaults");
        link.set_output_gain(2.0);
        link.set_rx_compression(true);
        link.set_rx_compression_level(0.25);
        let (gain, compress, level) = link.audio_prefs();
        assert!(near(gain, 2.0) && compress && near(level, 0.25));

        link.set_output_gain(99.0);
        assert!(near(link.audio_prefs().0, 4.0), "gain clamps to 4.0");
        link.set_output_gain(f32::NAN);
        assert!(near(link.audio_prefs().0, 1.0), "NaN falls back to unity");
        link.set_rx_compression_level(-1.0);
        assert!(near(link.audio_prefs().2, 0.0), "level clamps to 0.0");

        link.disconnect();
        reflector.shutdown();
    }

    /// A link with no vocoder decodes nothing, so it refuses nothing and
    /// names no backend. A caller must be able to tell the two kinds apart.
    #[test]
    fn a_link_without_audio_reports_no_backend() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");
        let snap = link.snapshot();
        assert_eq!(snap.backend, None);
        assert_eq!(snap.unsupported_mode, None);
        link.disconnect();
        reflector.shutdown();
    }
}
