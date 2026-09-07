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
//! [`YsfLink::connect_with_audio`] adds the voice path (astar-e7b3 §2):
//! an [`AmbeStream`] opened in [`VocoderMode::YsfDn`], the console's audio
//! lane, and the loop that lifts five 20 ms voice frames out of each payload
//! with [`astar_codec::ysf::unpack_dn`] and plays them. YSF voice is
//! **AMBE+2**, which on astar means the AMBE-3000 in a `ThumbDV` and nothing
//! else, so this constructor fails without a dongle rather than pretending.
//!
//! # Transmit
//!
//! [`YsfLink::set_ptt`] REQUESTS a key edge; the run loop applies it. That
//! split is what makes "nothing transmits unless the operator asked" a
//! property of the code rather than a claim about it: this module contains
//! exactly one path that can set the request true, and it is a public method
//! nobody else calls.
//!
//! The capture lane is NOT this module's. `ConsoleSession` opens the one
//! audio lane every digital-voice network rides ([`crate::voice_route`]),
//! keys its gate before forwarding a key-down here, and refuses the key
//! outright when no capture device could be opened. This link is handed the
//! channel ends and nothing else: no router, no device, no meter, no
//! preference and no gate.
//!
//! Captured audio is encoded to AMBE+2 half rate, five 20 ms frames to a
//! payload, and sent as `YSFD` — a header frame, then communications frames,
//! then a terminator with the wire's end flag set. Without that flag every
//! receiver waits out a timeout instead, and the next station hears a gap.
//!
//! # Half-duplex
//!
//! The `ThumbDV` is one physical link with one AMBE-3000 behind it, so this
//! link never decodes and encodes at the same time — the same rule
//! [`crate::dstar`] has always enforced, and for the same reason:
//! interleaving the two directions on one chip makes both come out wrong.
//!
//! While keyed, a received `YSFD`'s voice is never submitted to the decoder,
//! and on the key-DOWN edge whatever RX was queued or in flight is discarded
//! before the first mic frame is submitted. Both are enforced in code rather
//! than assumed true in the common case.
//!
//! Reading the callsign out of a received frame costs no vocoder, so
//! last-heard stays truthful while transmitting. Only the voice is refused —
//! which does mean a busy reflector's audio is muted for the length of a
//! local over. That is the unavoidable consequence of one dongle, not an RX
//! bug.
//!
//! This was blocked until 2026-09-06 by the vendored deframer, which rejected
//! any `Channel` response but 72 bits as "rate lost" and so discarded every
//! half-rate encode reply. That check now lives in the caller that configured
//! the rate; see `iax-ysftx` and `vendor/ambe-thumbdv/VENDORED.md`.
//!
//! ## The data channel, and who reads it
//!
//! A transmission carries the payload's own DATA channel as well as the
//! voice (`iax-ysfdch`, designed in `docs/design/ysf-dch.md`). That matters
//! more than "a radio can show a callsign": `MMDVMHost`'s
//! `CYSFControl::writeNetwork` only attributes a network transmission —
//! and only regenerates the DCH for the air — when the channel's CRC
//! checks, and a reflector opens the stream it relays on a frame whose FICH
//! says Header. An over with a zeroed DCH is carried as audio with nothing
//! downstream able to say who sent it, which is what astar did until now.
//!
//! So a transmission is a real Header frame at key-down (CSD1 = destination
//! and source, CSD2 = downlink and uplink, no voice), then Communications
//! frames cycling FN 0..=6 with FT 6, each carrying that frame number's ten
//! bytes of DCH beside its five voice frames, then a real Terminator.
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

use astar_audio::CallAudio;
use astar_codec::ambe::{
    AMBE_STREAM_MAX_IN_FLIGHT, AmbeBackend, AmbeStream, VocoderMode, open_ambe_stream,
};
use astar_codec::ysf::{DnError, DnFrame, FRAMES_PER_PAYLOAD, pack_dn, unpack_dn};
use astar_ysf::{
    Callsign, DataPacket, DataType, Fich, Frame, FrameInfo, FsmAction, LinkState, YsfFsm, dch, wire,
};

use crate::session::ConsoleError;

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

/// How many radio frames a transmission sends before the terminator.
///
/// Not a limit on how long anyone may talk — the run loop keys until PTT is
/// released. This is the frame counter's wrap, which the wire carries in
/// seven bits.
const COUNTER_WRAP: u8 = 0x80;

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
    /// `true` while this station is actually transmitting — the applied
    /// state, not an echo of the last [`YsfLink::set_ptt`] request, so it
    /// lags a key by up to one run-loop pass. The refusal a key-down can
    /// meet is not this link's: `ConsoleSession::set_ptt` returns
    /// `ConsoleError::NoCaptureDevice` and forwards nothing, so a refused
    /// key never reaches [`YsfLink::set_ptt`] at all.
    pub ptt: bool,
    /// Transmit level in dBFS. CONSOLE-OWNED, exactly as
    /// [`crate::dstar::DstarSnapshotState`]'s levels are: the link always
    /// fills this with the -60.0 floor and
    /// [`crate::ConsoleSession::ysf_state`] overwrites it with the number
    /// read once, at the one audio lane. A session owns no meters — see
    /// [`crate::voice_route`].
    pub tx_dbfs: f32,
    /// Receive level in dBFS, console-owned exactly as [`Self::tx_dbfs`] is.
    pub rx_dbfs: f32,
}

/// Operator-supplied configuration for a link that carries audio.
///
/// Carries no devices, exactly as [`crate::dstar::DstarConfig`] no longer
/// does: `ConsoleSession` opens the one audio lane
/// ([`crate::voice_route`]) and hands the link its channel ends.
pub struct YsfConfig {
    /// Reflector `host:port`, as [`YsfLink::connect`] takes it.
    pub host: String,
    /// This station's callsign.
    pub callsign: String,
    /// The YCS room request (`set_options`); `None` for a plain
    /// `YSFReflector`.
    pub options: Option<String>,
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
    /// What the operator asked for. NOTHING in this module sets it except
    /// [`YsfLink::set_ptt`], which is the only path a key-down can take.
    ptt_request: AtomicBool,
    /// What the run loop actually applied. The two differ only for the one
    /// pass between the request and the edge — a key-down with no capture
    /// device is refused by `ConsoleSession::set_ptt` before it ever reaches
    /// this module — but a UI must still read this one, since it is the only
    /// value that is true when the station is genuinely on the air.
    ptt: AtomicBool,
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
            ptt_request: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
        }
    }
}

/// Everything the run loop needs to turn payload bytes into sound. Owned by
/// the link thread; `None` for a link opened without audio.
struct Audio {
    ambe: Box<dyn AmbeStream>,
    /// The one audio lane `ConsoleSession` opened for this link
    /// ([`crate::voice_route`]): decoded frames go out on `rx_frames`,
    /// captured frames arrive on `tx_frames` while the console has the gate
    /// open. The link owns neither end's device.
    bus: CallAudio,
    /// Arrived-but-not-yet-submitted voice frames. A payload carries five
    /// and the vocoder accepts four, so this queue is not an optimisation —
    /// without it the fifth frame of every burst would be dropped.
    pending: VecDeque<DnFrame>,
    /// Captured frames waiting to be submitted to the encoder.
    ///
    /// The exact counterpart of `pending` on the decode side, and necessary
    /// for the same reason: `submit_encode` DROPS when the vocoder is at its
    /// in-flight bound, so submitting a burst straight from the mic throws
    /// away every frame past the fourth. Dropped mic frames are dropped
    /// speech — the far end hears it as garbling.
    mic_pending: VecDeque<[i16; 160]>,
    /// Decoded frames waiting to be released to the bus on the audio clock.
    /// See [`PRIME_FRAMES`] for why they are not handed over as they finish.
    decoded: VecDeque<[i16; 160]>,
    /// When the next frame is due. `None` while re-priming — before the
    /// first frame of a transmission, and after the queue has run dry.
    next_release: Option<Instant>,
    /// Transmit state, `None` while unkeyed.
    tx: Option<Tx>,
}

/// One transmission in progress.
///
/// Exists only between key-down and key-up, so its presence IS "this station
/// is transmitting" and there is no separate flag to keep in step.
struct Tx {
    /// Voice frames encoded and waiting to fill a payload. A radio frame
    /// carries five, and the mic produces them one at a time.
    voice: Vec<DnFrame>,
    /// The wire's frame counter, seven bits, wrapping.
    counter: u8,
    /// The FICH's frame number, cycling 0..=[`astar_ysf::FRAME_TOTAL`].
    ///
    /// Not the same thing as `counter`, which is the `YSFD` header's own
    /// sequence: this one selects which ten bytes of callsign the payload's
    /// data channel carries, and a client that never advances it never
    /// sends the frames a receiver takes the callsigns from.
    frame_number: u8,
}

impl Tx {
    /// Opens a transmission. The header frame goes out separately, at the
    /// key-down edge, so the first voice frame is already frame 0.
    fn new() -> Tx {
        Tx {
            voice: Vec::with_capacity(FRAMES_PER_PAYLOAD),
            // The header frame is counter 0; voice starts after it.
            counter: 1,
            frame_number: 0,
        }
    }

    /// Takes the next frame number, advancing the cycle.
    fn next_frame_number(&mut self) -> u8 {
        let n = self.frame_number;
        self.frame_number = (self.frame_number + 1) % astar_ysf::SUPERFRAME;
        n
    }

    /// Takes the next `YSFD` counter, advancing it.
    fn next_counter(&mut self) -> u8 {
        let n = self.counter;
        self.counter = (self.counter + 1) % COUNTER_WRAP;
        n
    }
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

    /// Open a link that decodes the audio on it, and transmits.
    ///
    /// `audio` is the one audio lane `ConsoleSession` already opened on the
    /// station's router ([`crate::voice_route`]): this link plays what it
    /// decodes onto `audio.rx_frames` and encodes whatever arrives on
    /// `audio.tx_frames` while keyed. It builds no router, resolves no
    /// device, carries no preference and never touches the PTT gate — see
    /// this module's docs.
    ///
    /// Opens a `ThumbDV` in [`VocoderMode::YsfDn`], then plays every DN
    /// frame the reflector sends. VW and data frames are refused into
    /// [`YsfSnapshot::unsupported_mode`] rather than decoded.
    ///
    /// # Errors
    /// Everything [`YsfLink::connect`] can fail with, plus
    /// [`ConsoleError::Ysf`] when no `ThumbDV` is available (the message
    /// comes from `classify_thumbdv_failure`, so "unplugged" and "busy" are
    /// told apart).
    pub fn connect_with_audio(cfg: &YsfConfig, audio: CallAudio) -> Result<YsfLink, ConsoleError> {
        // Hardware-only, exactly as D-Star: no software AMBE exists, so a
        // missing dongle is an error with a reason rather than a silent
        // fallback to a link that makes no sound.
        let (ambe, backend) = open_ambe_stream(Some(AmbeBackend::Hardware), VocoderMode::YsfDn)
            .ok_or_else(|| {
                ConsoleError::Ysf(astar_codec::ambe::classify_thumbdv_failure().message())
            })?;
        Self::connect_with_stream(cfg, audio, ambe, backend)
    }

    /// [`Self::connect_with_audio`] with the vocoder supplied by the caller.
    ///
    /// Two callers want this: the `astar-station` facade, so the `ThumbDV`
    /// probe runs OUTSIDE its session mutex, and the tests, where a fake
    /// [`AmbeStream`] decodes without a dongle so the whole
    /// payload-to-speaker path is provable against the loopback reflector on
    /// `127.0.0.1`.
    ///
    /// # Errors
    /// As [`Self::connect_with_audio`], minus the `ThumbDV` probe.
    pub fn connect_with_stream(
        cfg: &YsfConfig,
        audio: CallAudio,
        ambe: Box<dyn AmbeStream>,
        backend: AmbeBackend,
    ) -> Result<YsfLink, ConsoleError> {
        let audio = Audio {
            ambe,
            bus: audio,
            pending: VecDeque::new(),
            mic_pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            tx: None,
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
            ptt: self.shared.ptt.load(Ordering::Relaxed),
            // Console-owned: the floor here, overwritten by
            // `ConsoleSession::ysf_state` from the one meter read at the
            // lane. See the field docs.
            tx_dbfs: -60.0,
            rx_dbfs: -60.0,
        }
    }

    /// Request transmit on or off.
    ///
    /// Stores a request; the run loop applies the edge on its next pass. This
    /// call never blocks and never keys anything by itself — it is the ONLY
    /// path that can set the request true, which is what makes "nothing
    /// transmits unless the operator asked" checkable rather than asserted.
    pub fn set_ptt(&self, on: bool) {
        self.shared.ptt_request.store(on, Ordering::Relaxed);
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
            // Apply a pending PTT edge and, on a pass that is not
            // transmitting, drop whatever the lane captured — in that order,
            // in one step, before anything else on this pass can block. See
            // `run_ptt_step`, where the ordering is the whole point.
            run_ptt_step(a, shared, socket, addr, fsm.callsign());
            if a.tx.is_some() {
                pump_tx(a, socket, addr, fsm.callsign());
            }
            // Keep the vocoder fed and the speaker supplied between
            // arrivals: a payload is five frames and only four fit in the
            // pipeline at once, so the rest are submitted here.
            pump(a);
        }
    }

    // Close an over that is still open. A link torn down mid-transmission
    // would otherwise leave the reflector waiting out its own timeout with
    // this station's callsign still on it. The lane's gate is not ours to
    // close — `ConsoleSession` owns it, and releases the whole route.
    if let Some(a) = audio.as_mut()
        && a.tx.is_some()
    {
        shared.ptt.store(false, Ordering::Relaxed);
        end_tx(a, socket, addr, fsm.callsign());
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
                // HALF-DUPLEX. The ThumbDV is one physical link with one
                // AMBE-3000 behind it, and D-Star has never let a session
                // decode and encode at the same time. YSF must not either:
                // interleaving the two directions on one chip is how a
                // transmission and whatever else is on the reflector both
                // come out wrong.
                //
                // The header fields above are still tracked — reading a
                // callsign out of a `YSFD` costs no vocoder — so "last
                // heard" stays truthful while transmitting. Only the voice
                // is refused.
                if a.tx.is_none() {
                    decode_frame(&packet.frame, a, shared);
                }
                if packet.end && a.tx.is_none() {
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

/// One run-loop pass's transmit step: apply the pending PTT edge, then — on
/// a pass that is NOT transmitting — drain and DROP whatever the capture lane
/// has queued.
///
/// The two are ONE function because the ordering between them is the whole
/// invariant, not a detail of layout. The gate belongs to
/// `ConsoleSession::set_ptt`, which opens it up to a poll interval before
/// this loop observes the request, and it can stay open while this loop is
/// not keyed — so without the drop the lane would pile audio into
/// `tx_frames` unbounded and the next transmission would open with somebody
/// else's stale speech.
///
/// What keeps the drop from eating the VOX pre-roll is that it runs on the
/// SAME pass that read `ptt_request`, with nothing in between: an operator
/// keying after this call does so against a pass that will never drain
/// again. Put it after the socket read instead and the 20 ms
/// [`AUDIO_RECV_TIMEOUT`] opens a window in which the gate opens, the lane
/// flushes its look-back ring, and this arm throws away precisely the audio
/// it exists to protect. D-Star shipped that bug once; YSF must not.
///
/// The key-DOWN edge deliberately keeps what it finds — that is the pre-roll
/// and the speech onset, and it is encoded first.
fn run_ptt_step(
    audio: &mut Audio,
    shared: &Arc<Shared>,
    socket: &UdpSocket,
    addr: SocketAddr,
    callsign: &Callsign,
) {
    apply_ptt(audio, shared, socket, addr, callsign);
    if audio.tx.is_none() {
        while audio.bus.tx_frames.try_recv().is_ok() {}
    }
}

/// Start or stop a transmission, if the request differs from what is applied.
///
/// The capture lane's gate is NOT touched here. `ConsoleSession::set_ptt`
/// opened it before forwarding the key — refusing the key-down outright, and
/// forwarding nothing, when no capture device could be opened — and closes it
/// on key-up. This is the protocol edge only.
fn apply_ptt(
    audio: &mut Audio,
    shared: &Arc<Shared>,
    socket: &UdpSocket,
    addr: SocketAddr,
    callsign: &Callsign,
) {
    let want = shared.ptt_request.load(Ordering::Relaxed);
    let keyed = audio.tx.is_some();
    if want == keyed {
        return;
    }
    if want {
        // Clear the decode path BEFORE the first mic frame is submitted.
        // Whatever is queued or in flight belongs to the moment before this
        // station keyed; playing it now would land it under our own
        // transmission, and leaving it in the vocoder would interleave it
        // with the encode requests about to start.
        discard_rx(audio);
        audio.tx = Some(Tx::new());
        shared.ptt.store(true, Ordering::Relaxed);
        // The Header frame opens the over, and it goes out NOW rather than
        // with the first payload of voice: a reflector creates the stream
        // it relays on a frame whose FICH says Header, so a hundred
        // milliseconds of speech sent ahead of it is a hundred milliseconds
        // nobody forwards.
        send_radio_frame(
            socket,
            addr,
            callsign,
            0,
            false,
            build_csd_frame(FrameInfo::Header, callsign),
        );
    } else {
        shared.ptt.store(false, Ordering::Relaxed);
        end_tx(audio, socket, addr, callsign);
    }
}

/// Empty the decode path and throw the audio away.
///
/// Used on the key-down edge only. `flush` forwards its tail because that
/// audio belongs to a talker whose over just ended; this one discards,
/// because the audio belongs to the moment before the local operator keyed
/// and playing it under their own transmission would be worse than losing it.
///
/// Bounded by [`FLUSH_DEADLINE`] like every other drain here: a dongle that
/// stops answering must delay a key-down, never hang the link thread.
fn discard_rx(audio: &mut Audio) {
    audio.pending.clear();
    let deadline = Instant::now() + FLUSH_DEADLINE;
    while audio.ambe.in_flight() > 0 && Instant::now() < deadline {
        while audio.ambe.poll_decoded().is_some() {}
        if audio.ambe.in_flight() > 0 {
            thread::sleep(DRAIN_POLL_INTERVAL);
        }
    }
    audio.decoded.clear();
    audio.next_release = None;
}

/// The FICH every frame of a transmission shares, with `info` and
/// `frame_number` filled in.
///
/// The values are a softclient's, field for field — see
/// `docs/design/ysf-dch.md` §3. `Fich::default` already carries CS 2, CM 0,
/// BN/BT 0, Dev and `VoIP` clear, MR direct, SQL clear and DG-ID 0. That
/// `VoIP` bit in particular stays FALSE: the code that sets it true is a
/// repeater putting network audio back on the air, which is not what this
/// is.
fn tx_fich(info: FrameInfo, frame_number: u8) -> Fich {
    Fich {
        frame_info: info,
        // V/D mode 2 is what Yaesu radios transmit for "DN" and what every
        // reflector expects; mode 1 is decoded on receive but never sent.
        data_type: DataType::VDMode2,
        frame_number: frame_number % astar_ysf::SUPERFRAME,
        frame_total: astar_ysf::FRAME_TOTAL,
        ..Fich::default()
    }
}

/// Build one Communications frame: sync, FICH, five voice frames, and the
/// ten bytes of data channel that belong to `frame_number`.
///
/// Voice and data share the payload's five blocks without overlapping — the
/// voice channel starts 40 bits into each block — so the two are written one
/// after the other rather than merged.
fn build_voice_frame(
    frame_number: u8,
    voice: &[DnFrame; FRAMES_PER_PAYLOAD],
    callsign: &Callsign,
) -> [u8; astar_ysf::FRAME_LEN] {
    let fich = tx_fich(FrameInfo::Communications, frame_number);
    let mut payload = [0u8; astar_ysf::PAYLOAD_LEN];
    // Infallible: `data_type` is half-rate voice and the payload is exactly
    // PAYLOAD_LEN, which are the only two things `pack_dn` refuses.
    let _ = pack_dn(fich.data_type, voice, &mut payload);
    // A client linked to a reflector is its own gateway, so the source and
    // the downlink/uplink callsigns are all this station.
    dch::write_vd2(
        &mut payload,
        &dch::vd2_dch(fich.frame_number, callsign, callsign),
    );
    astar_ysf::frame::build(fich, &payload)
}

/// Build the Header or Terminator frame that opens and closes an over.
///
/// These carry NO voice: their whole ninety bytes are the two callsign
/// blocks under their own FEC. That is what the references send and what a
/// reflector needs before it will open the stream it relays.
fn build_csd_frame(info: FrameInfo, callsign: &Callsign) -> [u8; astar_ysf::FRAME_LEN] {
    let fich = tx_fich(info, 0);
    let (csd1, csd2) = dch::header_csd(callsign, callsign);
    let mut payload = [0u8; astar_ysf::PAYLOAD_LEN];
    dch::write_csd(&mut payload, &csd1, &csd2);
    astar_ysf::frame::build(fich, &payload)
}

/// Put one radio frame on the wire, wrapped in its `YSFD` routing header.
///
/// The three callsigns are all this station: YSF carries the gateway, the
/// source and the destination separately, and for a client linking to a
/// reflector the first two are us and the third is unaddressed.
fn send_radio_frame(
    socket: &UdpSocket,
    addr: SocketAddr,
    callsign: &Callsign,
    counter: u8,
    end: bool,
    frame: [u8; astar_ysf::FRAME_LEN],
) {
    let packet = DataPacket {
        gateway: *callsign,
        source: *callsign,
        destination: Callsign::new("ALL").expect("three printable ASCII bytes"),
        counter,
        end,
        frame,
    };
    let _ = socket.send_to(&wire::data(&packet), addr);
}

/// Feed captured audio to the vocoder and put finished frames on the wire.
///
/// Nothing here keys anything: `tx` is `Some` only because the operator
/// asked, and this runs only while it is.
fn pump_tx(audio: &mut Audio, socket: &UdpSocket, addr: SocketAddr, callsign: &Callsign) {
    // Take everything the mic has produced into OUR queue, not the
    // vocoder's. `submit_encode` drops when it is at its in-flight bound, so
    // handing it a burst discards every frame past the fourth — silently, and
    // as missing speech at the far end.
    while let Ok(pcm) = audio.bus.tx_frames.try_recv() {
        if pcm.len() == 160 {
            let mut frame = [0i16; 160];
            frame.copy_from_slice(&pcm);
            audio.mic_pending.push_back(frame);
        }
    }
    // Then feed the vocoder only as fast as it will accept, exactly as the
    // decode side does.
    while audio.ambe.in_flight_encoded() < AMBE_STREAM_MAX_IN_FLIGHT {
        let Some(frame) = audio.mic_pending.pop_front() else {
            break;
        };
        audio.ambe.submit_encode(frame);
    }
    while let Some(encoded) = audio.ambe.poll_encoded() {
        let Some(tx) = audio.tx.as_mut() else {
            // Key-up landed between the submit and the reply; the frame
            // belongs to a transmission that has already ended.
            continue;
        };
        match encoded {
            astar_codec::ambe::ChannelFrame::YsfDn(dn) => tx.voice.push(dn),
            other @ astar_codec::ambe::ChannelFrame::Dstar(_) => {
                // Unreachable: this stream is opened in `VocoderMode::YsfDn`.
                // Stated rather than assumed, because the alternative is
                // truncating a D-Star frame onto a YSF wire.
                tracing::warn!(
                    got = other.mode().as_str(),
                    "ysf: encoder returned a non-DN frame, substituting silence"
                );
                tx.voice.push(DnFrame::MUTE);
            }
        }
        if tx.voice.len() < FRAMES_PER_PAYLOAD {
            continue;
        }
        let mut five = [DnFrame::default(); FRAMES_PER_PAYLOAD];
        five.copy_from_slice(&tx.voice[..FRAMES_PER_PAYLOAD]);
        tx.voice.clear();

        // Every frame here carries voice; the header went out on the
        // key-down edge, before there was any. The frame number cycles
        // independently of the wire counter — it is what selects the ten
        // bytes of callsign this frame's data channel carries.
        let frame_number = tx.next_frame_number();
        let counter = tx.next_counter();
        send_radio_frame(
            socket,
            addr,
            callsign,
            counter,
            false,
            build_voice_frame(frame_number, &five, callsign),
        );
    }
}

/// Close a transmission: flush what the encoder still owes, then send the
/// terminator with the wire's end flag set.
///
/// The end flag is what tells every receiver the over is finished; without it
/// they wait out a timeout instead, and the next station hears a gap.
fn end_tx(audio: &mut Audio, socket: &UdpSocket, addr: SocketAddr, callsign: &Callsign) {
    if audio.tx.is_none() {
        return;
    }
    // Drain what the encoder still owes BEFORE closing the over. Without
    // this the last frames of every transmission are lost — and worse, they
    // arrive during the NEXT one and are sent as that talker's audio.
    let deadline = Instant::now() + FLUSH_DEADLINE;
    loop {
        pump_tx(audio, socket, addr, callsign);
        let done = audio.mic_pending.is_empty() && audio.ambe.in_flight_encoded() == 0;
        if done || Instant::now() >= deadline {
            if !done {
                tracing::warn!(
                    pending = audio.mic_pending.len(),
                    in_flight = audio.ambe.in_flight_encoded(),
                    "ysf: encoder flush hit its {FLUSH_DEADLINE:?} deadline, ending the over anyway"
                );
            }
            break;
        }
        thread::sleep(DRAIN_POLL_INTERVAL);
    }
    audio.mic_pending.clear();

    let Some(mut tx) = audio.tx.take() else {
        return;
    };
    // Pad a partial payload with silence rather than dropping it: the tail of
    // an over is speech, and five frames is 100 ms.
    while tx.voice.len() % FRAMES_PER_PAYLOAD != 0 {
        tx.voice.push(DnFrame::MUTE);
    }
    let tail = std::mem::take(&mut tx.voice);
    for chunk in tail.chunks(FRAMES_PER_PAYLOAD) {
        let mut five = [DnFrame::default(); FRAMES_PER_PAYLOAD];
        five.copy_from_slice(chunk);
        let frame_number = tx.next_frame_number();
        let counter = tx.next_counter();
        send_radio_frame(
            socket,
            addr,
            callsign,
            counter,
            false,
            build_voice_frame(frame_number, &five, callsign),
        );
    }
    // The terminator is a callsign frame, not a frame of silence: it closes
    // the reflector's stream and repeats who was talking, which is what the
    // references send and what leaves a radio's display correct.
    send_radio_frame(
        socket,
        addr,
        callsign,
        tx.counter,
        true,
        build_csd_frame(FrameInfo::Terminator, callsign),
    );
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
    use std::sync::mpsc::{Receiver, Sender, channel};

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
        /// Encode side: a submitted PCM frame becomes one DN frame carrying
        /// its first sample, so a test can follow a frame through.
        encoded: VecDeque<DnFrame>,
    }

    impl FakeVocoder {
        fn new() -> FakeVocoder {
            FakeVocoder {
                submitted: Vec::new(),
                ready: VecDeque::new(),
                encoded: VecDeque::new(),
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
        fn submit_encode(&mut self, pcm: [i16; 160]) {
            let tag = u8::try_from(pcm[0].unsigned_abs() & 0xFF).unwrap_or(0);
            self.encoded
                .push_back(DnFrame::from_bytes([tag, 0, 0, 0, 0, 0, 0]));
        }
        fn poll_encoded(&mut self) -> Option<astar_codec::ambe::ChannelFrame> {
            self.encoded.pop_front().map(ChannelFrame::YsfDn)
        }
        fn in_flight_encoded(&self) -> usize {
            self.encoded.len()
        }
    }

    /// An `Audio` wired to channels the test can read, with no device
    /// anywhere: the link owns no router and no device any more, and
    /// `CallAudio`'s fields are public, so the decoded PCM lands somewhere
    /// assertable.
    fn test_audio() -> (Audio, Receiver<Vec<i16>>) {
        let (audio, rx, _tx) = test_audio_with_capture();
        (audio, rx)
    }

    /// [`test_audio`] with the capture end kept, for a test that has to
    /// deliver mic frames into the lane the way the router's own `MicLane`
    /// does.
    fn test_audio_with_capture() -> (Audio, Receiver<Vec<i16>>, Sender<Vec<i16>>) {
        let (rx_tx, rx_rx) = channel::<Vec<i16>>();
        let (tx_tx, tx_rx) = channel::<Vec<i16>>();
        let call_audio = CallAudio {
            tx_frames: tx_rx,
            rx_frames: rx_tx,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        };
        (
            Audio {
                ambe: Box::new(FakeVocoder::new()),
                bus: call_audio,
                pending: VecDeque::new(),
                mic_pending: VecDeque::new(),
                decoded: VecDeque::new(),
                next_release: None,
                tx: None,
            },
            rx_rx,
            tx_tx,
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
            mic_pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            tx: None,
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

    // ── Transmit ────────────────────────────────────────────────────────
    //
    // Everything here binds 127.0.0.1 and talks to a reflector this test
    // started. Nothing reaches a real network, which is the rule this whole
    // crate is inside.

    /// Frames built for the air, checked as bytes.
    ///
    /// The FICH must say DN V/D mode 2 — what Yaesu radios transmit and what
    /// every reflector expects — and the voice must survive the round trip
    /// through `pack_dn`, or the far end hears the wrong thing while every
    /// state machine reports success.
    ///
    /// The data channel is checked here too, for the reason `iax-ysfdch`
    /// exists: a receiver that cannot read it attributes the over to nobody
    /// and passes the channel to the air unchanged, so "the voice arrived"
    /// is not enough to call a transmission correct.
    #[test]
    fn a_transmitted_frame_is_dn_mode_2_and_carries_its_voice() {
        let voice: [DnFrame; FRAMES_PER_PAYLOAD] = core::array::from_fn(|i| {
            DnFrame::from_bytes([u8::try_from(i + 1).expect("small"), 0, 0, 0, 0, 0, 0])
        });
        let me = Callsign::new("AJ7HR").expect("legal");
        let bytes = build_voice_frame(1, &voice, &me);

        let frame = Frame::new(&bytes).expect("a built frame must parse");
        assert!(
            frame.has_sync(),
            "every radio frame opens with the sync word"
        );
        let fich = frame.fich().expect("the FICH must decode");
        assert_eq!(fich.frame_info, FrameInfo::Communications);
        assert_eq!(fich.data_type, DataType::VDMode2, "astar transmits DN only");
        assert_eq!(fich.frame_number, 1);
        assert_eq!(fich.frame_total, astar_ysf::FRAME_TOTAL);
        assert_eq!(fich.call_sign_path, 2, "CS 2 is what a softclient sends");
        assert!(!fich.voip, "VoIP is a repeater's bit, not a client's");

        let back = unpack_dn(fich.data_type, frame.payload()).expect("must round-trip");
        assert_eq!(back, voice, "the voice we packed is the voice on the wire");

        let payload: &[u8; astar_ysf::PAYLOAD_LEN] =
            frame.payload().try_into().expect("ninety bytes");
        assert_eq!(
            dch::read_vd2(payload),
            Some(*b"AJ7HR     "),
            "frame 1's data channel is the source callsign"
        );
    }

    /// The DCH cycle, frame by frame: what each frame number must carry, and
    /// that the voice in the same payload is untouched by it.
    #[test]
    fn every_frame_of_the_cycle_carries_its_own_callsign_block() {
        let me = Callsign::new("AJ7HR").expect("legal");
        let voice: [DnFrame; FRAMES_PER_PAYLOAD] = core::array::from_fn(|i| {
            DnFrame::from_bytes([u8::try_from(i + 1).expect("small"), 0, 0, 0, 0, 0, 0])
        });
        let expected: [&[u8; 10]; 7] = [
            b"**********",
            b"AJ7HR     ",
            b"AJ7HR     ",
            b"AJ7HR     ",
            b"          ",
            b"          ",
            b"          ",
        ];
        for (fn_, want) in expected.iter().enumerate() {
            let fn_ = u8::try_from(fn_).expect("small");
            let bytes = build_voice_frame(fn_, &voice, &me);
            let frame = Frame::new(&bytes).expect("parse");
            let fich = frame.fich().expect("fich");
            assert_eq!(fich.frame_number, fn_);
            let payload: &[u8; astar_ysf::PAYLOAD_LEN] =
                frame.payload().try_into().expect("ninety bytes");
            assert_eq!(dch::read_vd2(payload), Some(**want), "frame {fn_}");
            assert_eq!(
                unpack_dn(fich.data_type, frame.payload()).expect("round-trip"),
                voice,
                "frame {fn_}'s voice must survive its data channel"
            );
        }
    }

    /// The header and terminator frames, which carry callsigns and no voice.
    #[test]
    fn the_header_and_terminator_carry_the_callsign_blocks() {
        let me = Callsign::new("AJ7HR").expect("legal");
        for info in [FrameInfo::Header, FrameInfo::Terminator] {
            let bytes = build_csd_frame(info, &me);
            let frame = Frame::new(&bytes).expect("parse");
            let fich = frame.fich().expect("fich");
            assert_eq!(fich.frame_info, info);
            assert_eq!(fich.data_type, DataType::VDMode2);
            assert_eq!(fich.frame_total, astar_ysf::FRAME_TOTAL);

            let payload: &[u8; astar_ysf::PAYLOAD_LEN] =
                frame.payload().try_into().expect("ninety bytes");
            let (csd1, csd2) = dch::read_csd(payload).expect("both blocks must decode");
            assert_eq!(&csd1[..10], b"**********", "CSD1 destination");
            assert_eq!(&csd1[10..], b"AJ7HR     ", "CSD1 source");
            assert_eq!(&csd2[..10], b"AJ7HR     ", "CSD2 downlink");
            assert_eq!(&csd2[10..], b"AJ7HR     ", "CSD2 uplink");
        }
    }

    /// The routing header a reflector reads. `source` is what every other
    /// client shows as the talker, so getting it wrong is invisible here and
    /// obvious to everyone else on the reflector.
    #[test]
    fn the_wire_header_names_this_station() {
        let (socket, addr, rx) = udp_pair();
        let me = Callsign::new("AJ7HR").expect("legal");
        send_radio_frame(
            &socket,
            addr,
            &me,
            7,
            true,
            build_voice_frame(0, &[DnFrame::MUTE; FRAMES_PER_PAYLOAD], &me),
        );
        let mut buf = [0u8; 512];
        let n = rx.recv(&mut buf).expect("a datagram must arrive");
        let packet = match astar_ysf::wire::parse(&buf[..n]).expect("must parse") {
            astar_ysf::Packet::Data(d) => d,
            other => panic!("expected YSFD, got {other:?}"),
        };
        assert_eq!(packet.source.to_trimmed_string(), "AJ7HR");
        assert_eq!(packet.gateway.to_trimmed_string(), "AJ7HR");
        assert_eq!(packet.counter, 7);
        assert!(packet.end, "the end flag must survive the round trip");
    }

    /// Half-duplex, as a test: while transmitting, a received frame must
    /// never reach the vocoder. One AMBE-3000, one direction at a time —
    /// interleaving them is how both come out wrong.
    #[test]
    fn a_received_frame_is_not_decoded_while_transmitting() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());
        let frame = frame_of(DataType::VDMode2);

        // Unkeyed: decoded normally.
        decode_frame(&frame, &mut audio, &shared);
        flush(&mut audio);
        assert_eq!(
            rx.try_iter().count(),
            FRAMES_PER_PAYLOAD,
            "RX works while idle"
        );

        // Keyed: the run loop's `handle` skips the decode entirely, so the
        // vocoder is never asked. Exercised here through the same guard.
        audio.tx = Some(Tx::new());
        if audio.tx.is_none() {
            decode_frame(&frame, &mut audio, &shared);
        }
        flush(&mut audio);
        assert_eq!(
            rx.try_iter().count(),
            0,
            "no received audio may be decoded while this station transmits"
        );
    }

    /// The key-down edge throws away RX that was mid-flight. It belongs to
    /// the moment before the operator keyed; playing it under their own
    /// transmission would be worse than losing it.
    #[test]
    fn keying_discards_receive_audio_still_in_the_pipeline() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());

        // Fill the decode path but do not release it.
        decode_frame(&frame_of(DataType::VDMode2), &mut audio, &shared);
        assert!(
            !audio.decoded.is_empty() || audio.ambe.in_flight() > 0 || !audio.pending.is_empty(),
            "the decode path must actually have something in it"
        );

        // Anything already released belongs to before the key-down and is
        // rightly on its way to the speaker; this test is about what is
        // still IN the pipeline when the operator keys.
        let _released_before_keying: Vec<_> = rx.try_iter().collect();

        discard_rx(&mut audio);

        assert!(audio.pending.is_empty(), "queued frames dropped");
        assert!(audio.decoded.is_empty(), "decoded frames dropped");
        assert!(audio.next_release.is_none(), "and the clock re-primes");
        assert_eq!(rx.try_iter().count(), 0, "none of it reaches the speaker");
    }

    /// The garble, as a test. `submit_encode` DROPS when the vocoder is at
    /// its in-flight bound, so a burst of mic frames handed straight to it
    /// loses everything past the fourth — and lost mic frames are lost
    /// speech. The queue is what makes a burst survive.
    #[test]
    fn a_burst_of_mic_frames_is_queued_rather_than_dropped() {
        const BURST: usize = 20;
        const {
            assert!(
                BURST > AMBE_STREAM_MAX_IN_FLIGHT,
                "the burst must exceed the bound, or this proves nothing"
            );
        }
        let (mut audio, _rx) = test_audio();
        let (socket, addr, peer) = udp_pair();
        let me = Callsign::new("N0CALL").expect("legal");
        audio.tx = Some(Tx::new());

        // A burst arrives in one pass, as a jittery capture lane delivers.
        let (tx_send, tx_recv) = channel::<Vec<i16>>();
        audio.bus.tx_frames = tx_recv;
        for i in 0..BURST {
            tx_send
                .send(vec![i16::try_from(i + 1).expect("small"); 160])
                .expect("send");
        }

        // Drain fully, then account for every captured frame: on the wire,
        // or still waiting to fill a payload. None may vanish.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            pump_tx(&mut audio, &socket, addr, &me);
            if (audio.mic_pending.is_empty() && audio.ambe.in_flight_encoded() == 0)
                || Instant::now() >= deadline
            {
                break;
            }
        }
        assert!(audio.mic_pending.is_empty(), "the queue must drain");
        assert_eq!(audio.ambe.in_flight_encoded(), 0);

        let mut buf = [0u8; 512];
        let mut on_the_wire = 0usize;
        while peer.recv(&mut buf).is_ok() {
            on_the_wire += FRAMES_PER_PAYLOAD;
        }
        assert_eq!(
            on_the_wire + audio.tx.as_ref().map_or(0, |t| t.voice.len()),
            BURST,
            "every captured frame must reach the wire or still be waiting to fill a payload"
        );
    }

    /// Every voice frame that reached `peer`, as its `FakeVocoder` tag — the
    /// low byte of the PCM sample the encoder was handed, so a frame can be
    /// followed from the capture lane to the wire.
    fn transmitted_voice_tags(peer: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 512];
        let mut tags = Vec::new();
        while let Ok(n) = peer.recv(&mut buf) {
            let Some(astar_ysf::Packet::Data(d)) = astar_ysf::wire::parse(&buf[..n]) else {
                continue;
            };
            let Some(frame) = Frame::new(&d.frame) else {
                continue;
            };
            let Ok(fich) = frame.fich() else {
                continue;
            };
            // Header and terminator frames carry callsigns where the voice
            // channel would be. Reading one as voice would splice two frames
            // of FEC-coded callsign into the middle of the tag stream.
            if fich.frame_info != FrameInfo::Communications {
                continue;
            }
            let Ok(voice) = unpack_dn(fich.data_type, frame.payload()) else {
                continue;
            };
            tags.extend(voice.iter().map(|f| f.as_bytes()[0]));
        }
        tags
    }

    /// The pre-roll regression, mirroring D-Star's
    /// `the_preroll_flushed_before_the_run_loop_sees_the_key_is_transmitted`
    /// in `astar-console/tests/dstar_session_pipeline.rs`.
    ///
    /// `ConsoleSession::set_ptt` opens the lane's gate and only THEN stores
    /// the PTT request, so the look-back ring the gate's false->true edge
    /// flushes — and the speech onset behind it — is already sitting in
    /// `tx_frames` when the run loop first reads the request as true. The
    /// key-down edge must keep every one of those frames; the unkeyed
    /// drain-and-drop exists for a different case entirely.
    #[test]
    fn the_frames_queued_before_the_key_edge_are_transmitted() {
        /// Speech captured before the loop saw the key.
        const PREROLL: i16 = 11;
        /// Speech captured after it.
        const LIVE: i16 = 22;

        let (mut audio, _rx, mic) = test_audio_with_capture();
        let (socket, addr, peer) = udp_pair();
        let me = Callsign::new("N0CALL").expect("legal");
        let shared = Arc::new(Shared::new());

        for _ in 0..FRAMES_PER_PAYLOAD {
            mic.send(vec![PREROLL; 160]).expect("send");
        }

        // The pass that observes the key-down. It must NOT drain.
        shared.ptt_request.store(true, Ordering::Relaxed);
        run_ptt_step(&mut audio, &shared, &socket, addr, &me);
        assert!(audio.tx.is_some(), "the key-down must open a transmission");

        for _ in 0..FRAMES_PER_PAYLOAD {
            mic.send(vec![LIVE; 160]).expect("send");
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            pump_tx(&mut audio, &socket, addr, &me);
            if (audio.mic_pending.is_empty() && audio.ambe.in_flight_encoded() == 0)
                || Instant::now() >= deadline
            {
                break;
            }
        }
        shared.ptt_request.store(false, Ordering::Relaxed);
        run_ptt_step(&mut audio, &shared, &socket, addr, &me);

        let tags = transmitted_voice_tags(&peer);
        let preroll = u8::try_from(PREROLL).expect("small");
        let live = u8::try_from(LIVE).expect("small");
        let first_preroll = tags.iter().position(|t| *t == preroll);
        let first_live = tags.iter().position(|t| *t == live);
        assert!(
            first_preroll.is_some(),
            "the audio queued before the key edge never reached the wire — the unkeyed drain \
             ate the operator's speech onset. tags transmitted: {tags:?}"
        );
        assert!(
            first_live.is_some(),
            "the audio captured after the key must be transmitted too, tags: {tags:?}"
        );
        assert!(
            first_preroll < first_live,
            "the pre-roll must lead the live stream, got {tags:?}"
        );
    }

    /// The whole shape of an over, on the wire: a Header frame first, then
    /// Communications frames whose FN cycles 0..=6 carrying the callsign
    /// blocks, then a Terminator with the end flag set.
    ///
    /// This is `iax-ysfdch` as an end-to-end assertion. Every piece of it
    /// was missing before: the header carried voice instead of CSD, FN never
    /// moved off zero, the data channel was zeroed, and the terminator was
    /// five frames of silence. A receiver reading any of those decided the
    /// over belonged to nobody.
    #[test]
    fn an_over_opens_with_a_header_cycles_its_frame_numbers_and_closes() {
        let (mut audio, _rx, mic) = test_audio_with_capture();
        let (socket, addr, peer) = udp_pair();
        let me = Callsign::new("AJ7HR").expect("legal");
        let shared = Arc::new(Shared::new());

        shared.ptt_request.store(true, Ordering::Relaxed);
        run_ptt_step(&mut audio, &shared, &socket, addr, &me);

        // Three payloads' worth of speech, so the frame number has to move.
        for _ in 0..(3 * FRAMES_PER_PAYLOAD) {
            mic.send(vec![7i16; 160]).expect("send");
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            pump_tx(&mut audio, &socket, addr, &me);
            if (audio.mic_pending.is_empty() && audio.ambe.in_flight_encoded() == 0)
                || Instant::now() >= deadline
            {
                break;
            }
        }
        shared.ptt_request.store(false, Ordering::Relaxed);
        run_ptt_step(&mut audio, &shared, &socket, addr, &me);

        let mut buf = [0u8; 512];
        let mut sent = Vec::new();
        while let Ok(n) = peer.recv(&mut buf) {
            let Some(astar_ysf::Packet::Data(d)) = astar_ysf::wire::parse(&buf[..n]) else {
                continue;
            };
            let frame = Frame::new(&d.frame).expect("120 bytes");
            let fich = frame.fich().expect("every frame's FICH must decode");
            let payload: [u8; astar_ysf::PAYLOAD_LEN] =
                frame.payload().try_into().expect("ninety bytes");
            sent.push((fich, d.end, payload));
        }

        assert!(
            sent.len() >= 4,
            "expected an over, got {} frames",
            sent.len()
        );

        let (head, head_end, head_payload) = &sent[0];
        assert_eq!(
            head.frame_info,
            FrameInfo::Header,
            "an over opens with a Header frame, before any voice"
        );
        assert!(!head_end);
        let (csd1, _) = dch::read_csd(head_payload).expect("the header's CSD must decode");
        assert_eq!(&csd1[10..], b"AJ7HR     ", "the header names this station");

        let (tail, tail_end, tail_payload) = sent.last().expect("frames");
        assert_eq!(tail.frame_info, FrameInfo::Terminator);
        assert!(tail_end, "the end flag is what closes the over");
        assert!(
            dch::read_csd(tail_payload).is_some(),
            "the terminator carries callsigns, not silence"
        );

        let voice = &sent[1..sent.len() - 1];
        for (i, (fich, end, payload)) in voice.iter().enumerate() {
            assert_eq!(fich.frame_info, FrameInfo::Communications, "frame {i}");
            assert!(!end, "only the terminator sets the end flag");
            assert_eq!(
                fich.frame_number,
                u8::try_from(i).expect("small") % astar_ysf::SUPERFRAME,
                "the frame number must cycle, not sit at zero"
            );
            assert_eq!(fich.frame_total, astar_ysf::FRAME_TOTAL);
            assert_eq!(
                dch::read_vd2(payload),
                Some(dch::vd2_dch(fich.frame_number, &me, &me)),
                "frame {i}'s data channel"
            );
        }
    }

    /// The other half of the same rule: on a pass that is not transmitting,
    /// whatever the lane captured is dropped rather than queued. The gate can
    /// legitimately be open while this loop is unkeyed — the console holds it
    /// until the operator releases PTT — and without the drop the next
    /// transmission would open with somebody else's stale speech.
    #[test]
    fn an_unkeyed_pass_drops_whatever_the_lane_captured() {
        let (mut audio, _rx, mic) = test_audio_with_capture();
        let (socket, addr, peer) = udp_pair();
        let me = Callsign::new("N0CALL").expect("legal");
        let shared = Arc::new(Shared::new());

        for _ in 0..10 {
            mic.send(vec![9i16; 160]).expect("send");
        }
        run_ptt_step(&mut audio, &shared, &socket, addr, &me);

        assert!(audio.tx.is_none(), "nothing asked for a transmission");
        assert!(
            audio.bus.tx_frames.try_recv().is_err(),
            "an unkeyed pass must empty the capture channel"
        );
        assert!(audio.mic_pending.is_empty(), "and queue none of it");
        let mut buf = [0u8; 512];
        assert!(
            peer.recv(&mut buf).is_err(),
            "and put nothing on the wire — that would be transmitting unasked"
        );
    }

    /// The safety property, stated as a test: a link that is never keyed puts
    /// NOTHING on the wire but polls. If this ever fails, astar transmitted
    /// without being asked.
    #[test]
    fn an_unkeyed_link_never_sends_a_voice_frame() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");
        let _ = wait_for(&link, "linked", Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(600));
        assert!(!link.snapshot().ptt, "nothing may key on its own");
        link.disconnect();
        reflector.shutdown();
    }

    /// `set_ptt` requests; it does not key. The applied state only moves when
    /// the run loop acts on it, which is what keeps "nothing transmits unless
    /// the operator asked" true by construction.
    #[test]
    fn set_ptt_only_requests() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");
        link.set_ptt(true);
        // No audio on this link, so the run loop can never apply it.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!link.snapshot().ptt, "a request is not a transmission");
        link.disconnect();
        reflector.shutdown();
    }

    /// A bound socket plus a peer to read what was sent to it.
    fn udp_pair() -> (UdpSocket, SocketAddr, UdpSocket) {
        let peer = UdpSocket::bind("127.0.0.1:0").expect("bind peer");
        peer.set_read_timeout(Some(Duration::from_millis(200)))
            .expect("timeout");
        let addr = peer.local_addr().expect("addr");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        (socket, addr, peer)
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
        fn poll_encoded(&mut self) -> Option<astar_codec::ambe::ChannelFrame> {
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
            mic_pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            tx: None,
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
