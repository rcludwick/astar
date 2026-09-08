// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A live `NXDNReflector` link: the socket and the thread that drive
//! [`astar_nxdn::NxdnFsm`] (iax-b9c2, designed in `docs/design/nxdn-wire.md`).
//!
//! `astar-nxdn` is deliberately pure — the FSM decides and hands back bytes
//! to send, and `astar_codec::nxdn` turns a frame's two 14-byte blocks into
//! voice — so something has to own a `UdpSocket`, feed it datagrams, call
//! `tick`, and put the result on the audio bus. That is this module, and it
//! is [`crate::ysf`] with the differences NXDN actually has and no others.
//!
//! # Two ways to open a link, and only one of them makes sound
//!
//! [`NxdnLink::connect`] is the link alone: it polls to hold the
//! registration open and reports who is transmitting from the `NXDND`
//! header. That needs no hardware — `srcId` is in bytes 5..7 of the
//! datagram in clear, so "is this reflector alive, and who is on this
//! talkgroup" is answerable with no vocoder involved.
//!
//! [`NxdnLink::connect_with_audio`] adds the voice path: an [`AmbeStream`]
//! opened in [`VocoderMode::YsfDn`], the console's audio lane, and the loop
//! that lifts four 20 ms voice frames out of each network frame with
//! [`astar_codec::nxdn::unpack_voice`] and plays them.
//!
//! **`VocoderMode::YsfDn` is not a typo.** NXDN voice and YSF DN voice are
//! the same 49-bit AMBE+2 frame with the same field order and the same
//! RATEP word — `MMDVMHost/NXDNControl.cpp` regenerates NXDN's on-air AMBE
//! with `CAMBEFEC::regenerateYSFDN`, and `DroidStar`'s
//! `SerialAMBE::config_ambe` sends one rate word for both `"YSF"` and
//! `"NXDN"`. The ruling is recorded in `docs/design/nxdn-wire.md`; the
//! alternative was a second mode constant that configured the dongle
//! identically.
//!
//! NXDN voice is therefore AMBE+2, which on astar means the AMBE-3000 in a
//! `ThumbDV` and nothing else, so the audio constructor fails without a
//! dongle rather than pretending.
//!
//! # Transmit
//!
//! **There is none, and the key is refused rather than ignored.**
//! [`NxdnLink::set_ptt`] stores a request exactly as [`crate::ysf`]'s does,
//! and `apply_ptt` — the one place a request becomes a transmission —
//! drops it on the floor and clears it. [`NxdnSnapshot::ptt`] is therefore
//! always false, and no `NXDND` datagram this module can build exists.
//!
//! That is a receive-first gate, not an oversight: the transmit path is
//! Task 9 of `docs/superpowers/plans/2026-09-07-nxdn-network.md`, and a key
//! that silently did nothing would be worse than one that is refused —
//! the operator would hear nothing happen and reasonably conclude the radio
//! or the link was broken. A caller shows a receive-only indicator instead
//! of a PTT button, and `set_ptt_is_refused_until_transmit_lands` holds it
//! to that in code rather than in prose.
//!
//! Two consequences of that gate are visible in the shape of this file, and
//! both come back with Task 9: [`Audio`] carries a `transmitting` flag
//! where `ysf.rs` carries an `Option<Tx>` — a `Tx` nothing can construct is
//! a struct that exists only to be dead — and there is no `discard_rx`,
//! because nothing here can produce the key-DOWN edge that calls it.
//!
//! # Half-duplex
//!
//! The `ThumbDV` is one physical link with one AMBE-3000 behind it, so this
//! link never decodes and encodes at the same time — the rule
//! [`crate::dstar`] and [`crate::ysf`] both enforce. While transmitting, a
//! received frame's voice is never submitted to the decoder. Reading
//! `srcId` costs no vocoder, so last-heard stays truthful either way: only
//! the voice is refused.
//!
//! # What ends a transmission
//!
//! The wire says so twice, and this module reads both.
//!
//! * The flags byte's `0x08`, which `NXDNGateway/NXDNNetwork.cpp:
//!   writeData` sets and `NXDNReflector.cpp` reads back
//!   (`(buffer[9U] & 0x08U) == 0x08U`).
//! * The LICH, for a sender that speaks the wire directly and never set the
//!   flag: `USC_SACCH_NS` with both blocks stolen for FACCH1 and
//!   `MESSAGE_TYPE_TX_REL` in byte 0 of block 0 — the terminator frame,
//!   LICH `0x81`/`0x83`.
//!
//! What must NOT end a transmission is `FrameKind::Signalling` on its own.
//! A mid-transmission frame with both halves stolen (`USC_SACCH_SS` +
//! `STEAL_FACCH`) is Signalling too, and treating it as a terminator would
//! cut every over short at its first stolen frame — quietly, and only on
//! the reflectors that send them.
//!
//! And when neither arrives — a client that stops dead — [`RX_WATCHDOG`]
//! closes the transmission client-side, so "receiving" cannot latch on
//! forever.
//!
//! # Threading
//!
//! One thread per link, owning the socket. It blocks on `recv_from` with a
//! read timeout so `tick` still runs when the reflector goes quiet — that
//! timeout is what makes the poll cadence and the link timeout work at all.
//! The control side sees an `AtomicU*` snapshot and a mutex-guarded
//! last-heard string, the same arrangement [`crate::ysf`] uses.

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
use astar_codec::nxdn::{NxdnVoiceError, unpack_voice};
use astar_codec::ysf::DnFrame;
use astar_nxdn::frame::{MESSAGE_TYPE_TX_REL, NetFrame, USC_SACCH_NS};
use astar_nxdn::{FsmAction, LinkState, NxdnFsm, wire};

use crate::session::ConsoleError;

/// How long the socket blocks before the loop runs `tick` anyway.
///
/// Shorter than the 5 s poll interval by enough that a poll is never late by
/// a meaningful fraction of it, and long enough that an idle link is not a
/// busy loop.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// [`RECV_TIMEOUT`] for a link that is decoding audio.
///
/// A reflector sends one network frame every 80 ms carrying four 20 ms voice
/// frames, and the vocoder accepts only [`AMBE_STREAM_MAX_IN_FLIGHT`] (4) at
/// a time — so a burst that arrives while one is still in flight cannot be
/// submitted in one go, and the loop has to come back around to feed the
/// rest. Waking roughly four times per arriving frame keeps the device fed
/// without blocking the link thread in a drain loop, which would add jitter
/// to the polls that hold the registration open.
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
/// NXDN's wire cadence is bursty: four 20 ms frames arrive together every
/// **80 ms** — the 384-bit RTCH frame at NXDN's 4800 bit/s
/// (`docs/design/nxdn-wire.md`; [`astar_codec::nxdn::FRAMES_PER_FRAME`] is
/// the four, and `astar_nxdn::reflector::FRAME_INTERVAL` paces the loopback
/// reflector at exactly that) — where D-Star sends one every 20 ms and is
/// paced by the air itself. Handing a burst straight to the bus empties the
/// vocoder in ~30 ms and then starves the bus for the rest, which is heard
/// as a beat. So the frames are released on the audio clock instead, and
/// this cushion absorbs the arrival jitter.
///
/// Two, not YSF's three: two of four is the same fraction of the burst that
/// three of five is, and it costs ~40 ms of one-way latency instead of ~60.
const PRIME_FRAMES: usize = 2;

/// Silence after which a transmission is treated as over even though no
/// end-flagged frame and no terminator ever arrived.
///
/// The reference reflector's own watchdog, so a client that stops dead
/// leaves this link showing exactly what the reflector shows:
/// `NXDNReflector.cpp: CTimer watchdogTimer(1000U, 0U, 1500U);`.
const RX_WATCHDOG: Duration = Duration::from_millis(1_500);

/// What the control side can see of a link, without touching the thread.
///
/// `PartialEq` but not `Eq`: it carries a level in dBFS, and `f32` has no
/// total equality — the same pair [`crate::ysf::YsfSnapshot`] derives, for
/// the same reason.
#[derive(Debug, Clone, PartialEq)]
pub struct NxdnSnapshot {
    /// The link state's ABI string — `idle`, `linking`, `linked`,
    /// `unlinking`, `failed`. See [`LinkState::as_str`], which documents why
    /// these strings are not a debug convenience.
    pub link_state: &'static str,
    /// The source id of the most recent transmission, as its decimal string.
    ///
    /// NXDN addresses stations by NUMBER: an `NXDND` datagram carries
    /// `srcId` and no callsign at all (a callsign appears only in a poll,
    /// and it is the polling client's own). Turning a number into a
    /// callsign is a directory lookup against a registry astar does not
    /// hold, and inventing one here would be a guess presented as
    /// identification — so this is the number, and a caller that has a
    /// directory may resolve it.
    pub last_heard: Option<String>,
    /// That same id, unformatted, for a caller doing the lookup.
    pub last_heard_id: Option<u16>,
    /// Network frames received since the link came up. A liveness counter —
    /// a link that is up but silent and one that is receiving look the same
    /// from `link_state` alone.
    pub frames_rx: u64,
    /// Whether a transmission is in progress: set by the wire's start flag
    /// and any frame that is not the end, cleared by the end flag, the
    /// terminator LICH, or [`RX_WATCHDOG`].
    pub receiving: bool,
    /// Which vocoder backend is decoding, or `None` for a link with no
    /// audio. An ABI string via [`AmbeBackend::as_str`].
    pub backend: Option<&'static str>,
    /// Whether this station is transmitting. **Always false**: the transmit
    /// path is Task 9 and the key is refused, not queued. See this module's
    /// Transmit section.
    pub ptt: bool,
    /// Transmit level in dBFS. CONSOLE-OWNED, exactly as
    /// [`crate::ysf::YsfSnapshot::tx_dbfs`] is: the link always fills this
    /// with the -60.0 floor and the session overwrites it with the number
    /// read once, at the one audio lane. A link owns no meters — see
    /// [`crate::voice_route`].
    pub tx_dbfs: f32,
    /// Receive level in dBFS, console-owned exactly as [`Self::tx_dbfs`] is.
    pub rx_dbfs: f32,
}

/// Operator-supplied configuration for a link that carries audio.
///
/// Carries no devices: `ConsoleSession` opens the one audio lane
/// ([`crate::voice_route`]) and hands the link its channel ends.
pub struct NxdnConfig {
    /// Reflector `host:port`, as [`NxdnLink::connect`] takes it.
    pub host: String,
    /// This station's callsign. Ten bytes on the wire, space padded; it
    /// rides in the poll, not in a voice frame.
    pub callsign: String,
    /// This station's NXDN radio id — sixteen bits, as
    /// `NXDNGateway/Reflectors.h: CNXDNReflector::m_id` is. Zero is refused
    /// at connect.
    pub radio_id: u16,
    /// The talkgroup to join. A poll carries it, and the reflector answers
    /// only polls for its own (`NXDNReflector.cpp`: `if (id == tg)`).
    pub talkgroup: u16,
}

/// A live link to one `NXDNReflector`.
///
/// `Debug` reports the snapshot rather than the internals: the thread handle
/// and the socket are not something a caller can act on, and the link state
/// is.
pub struct NxdnLink {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// `Some` only for a link opened with audio.
    backend: Option<AmbeBackend>,
}

#[derive(Debug)]
struct Shared {
    /// `LinkState` as its discriminant index; see `state_index`.
    link_state: AtomicU32,
    last_heard: Mutex<Option<String>>,
    /// The same id as a number, plus one sentinel: `u32::MAX` for "nobody
    /// has transmitted". A `u16` id cannot collide with it.
    last_heard_id: AtomicU32,
    frames_rx: AtomicU64,
    receiving: AtomicBool,
    stop: AtomicBool,
    /// What the operator asked for. NOTHING in this module sets it except
    /// [`NxdnLink::set_ptt`], which is the only path a key-down can take —
    /// and `apply_ptt` refuses it while transmit is gated.
    ptt_request: AtomicBool,
    /// What the run loop actually applied. False for the life of this
    /// module: see the Transmit section.
    ptt: AtomicBool,
}

/// Sentinel for [`Shared::last_heard_id`]: nobody has transmitted yet.
const NO_TALKER: u32 = u32::MAX;

impl Shared {
    fn new() -> Shared {
        Shared {
            link_state: AtomicU32::new(state_index(LinkState::Idle)),
            last_heard: Mutex::new(None),
            last_heard_id: AtomicU32::new(NO_TALKER),
            frames_rx: AtomicU64::new(0),
            receiving: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            ptt_request: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
        }
    }
}

/// Everything the run loop needs to turn frame bytes into sound. Owned by
/// the link thread; `None` for a link opened without audio.
struct Audio {
    ambe: Box<dyn AmbeStream>,
    /// The one audio lane `ConsoleSession` opened for this link
    /// ([`crate::voice_route`]): decoded frames go out on `rx_frames`,
    /// captured frames arrive on `tx_frames` while the console has the gate
    /// open. The link owns neither end's device.
    bus: CallAudio,
    /// Arrived-but-not-yet-submitted voice frames. A network frame carries
    /// four and the vocoder accepts four, so a frame that arrives while the
    /// previous one is still in flight has nowhere else to go — without this
    /// queue those frames would be dropped, evenly spread through every
    /// transmission.
    pending: VecDeque<DnFrame>,
    /// Decoded frames waiting to be released to the bus on the audio clock.
    /// See [`PRIME_FRAMES`] for why they are not handed over as they finish.
    decoded: VecDeque<[i16; 160]>,
    /// When the next frame is due. `None` while re-priming — before the
    /// first frame of a transmission, and after the queue has run dry.
    next_release: Option<Instant>,
    /// Whether this station is transmitting.
    ///
    /// `ysf.rs` carries an `Option<Tx>` here, and Task 9 brings that back
    /// with the transmit path. Today nothing can key, so a `Tx` type would
    /// be a struct that exists only to be constructed by a test — this is
    /// the same guard with nothing invented around it.
    transmitting: bool,
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
/// Exists so callers that must branch on the link state match an enum the
/// compiler can check rather than the ABI strings. A new `LinkState`
/// variant then breaks those call sites at compile time instead of silently
/// falling into a catch-all.
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
    state_from_index(i).as_str()
}

impl NxdnLink {
    /// Resolve `host`, bind a local socket, and start polling `talkgroup`.
    ///
    /// The link makes no sound: it holds the registration open and reports
    /// who is transmitting. [`NxdnLink::connect_with_audio`] is the one that
    /// decodes.
    ///
    /// # Errors
    /// [`ConsoleError::Nxdn`] if the callsign is not one this wire can
    /// carry, if `radio_id` is zero, if `host` does not resolve, or if the
    /// socket cannot be bound.
    pub fn connect(
        host: &str,
        callsign: &str,
        radio_id: u16,
        talkgroup: u16,
    ) -> Result<NxdnLink, ConsoleError> {
        Self::spawn(host, callsign, radio_id, talkgroup, None, None)
    }

    /// Open a link that decodes the audio on it.
    ///
    /// `audio` is the one audio lane `ConsoleSession` already opened on the
    /// station's router ([`crate::voice_route`]): this link plays what it
    /// decodes onto `audio.rx_frames`. It builds no router, resolves no
    /// device, carries no preference and never touches the PTT gate.
    ///
    /// Opens a `ThumbDV` in [`VocoderMode::YsfDn`] — NXDN's AMBE+2 frame and
    /// YSF DN's are the same object; see this module's docs — then plays
    /// every voice frame the reflector sends.
    ///
    /// # Errors
    /// Everything [`NxdnLink::connect`] can fail with, plus
    /// [`ConsoleError::Nxdn`] when no `ThumbDV` is available (the message
    /// comes from `classify_thumbdv_failure`, so "unplugged" and "busy" are
    /// told apart).
    pub fn connect_with_audio(
        cfg: &NxdnConfig,
        audio: CallAudio,
    ) -> Result<NxdnLink, ConsoleError> {
        // Hardware-only, exactly as D-Star and YSF: the AMBE-3000 in a
        // `ThumbDV` is the whole vocoder story, so a missing dongle is an
        // error naming its reason rather than a link that quietly makes no
        // sound.
        let (ambe, backend) = open_ambe_stream(Some(AmbeBackend::Hardware), VocoderMode::YsfDn)
            .ok_or_else(|| {
                ConsoleError::Nxdn(astar_codec::ambe::classify_thumbdv_failure().message())
            })?;
        Self::connect_with_stream(cfg, audio, ambe, backend)
    }

    /// [`Self::connect_with_audio`] with the vocoder supplied by the caller.
    ///
    /// Two callers want this: the `astar-station` facade, so the `ThumbDV`
    /// probe runs OUTSIDE its session mutex, and the tests, where a fake
    /// [`AmbeStream`] decodes without a dongle so the whole
    /// frame-to-speaker path is provable against the loopback reflector on
    /// `127.0.0.1`.
    ///
    /// # Errors
    /// As [`Self::connect_with_audio`], minus the `ThumbDV` probe.
    pub fn connect_with_stream(
        cfg: &NxdnConfig,
        audio: CallAudio,
        ambe: Box<dyn AmbeStream>,
        backend: AmbeBackend,
    ) -> Result<NxdnLink, ConsoleError> {
        let audio = Audio {
            ambe,
            bus: audio,
            pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            transmitting: false,
        };
        Self::spawn(
            &cfg.host,
            &cfg.callsign,
            cfg.radio_id,
            cfg.talkgroup,
            Some(audio),
            Some(backend),
        )
    }

    /// The body both constructors share: validate, resolve, bind, spawn.
    fn spawn(
        host: &str,
        callsign: &str,
        radio_id: u16,
        talkgroup: u16,
        audio: Option<Audio>,
        backend: Option<AmbeBackend>,
    ) -> Result<NxdnLink, ConsoleError> {
        let fsm = NxdnFsm::new(callsign, talkgroup)
            .map_err(|e| ConsoleError::Nxdn(format!("callsign: {e}")))?;

        // A radio id is a registration, not a default. Zero is what an unset
        // field looks like, and a transmission claiming it would claim
        // somebody else's number — or nobody's. Refused here, where the
        // operator is still looking at the dialog.
        if radio_id == 0 {
            return Err(ConsoleError::Nxdn(
                "radio id must be set: NXDN addresses stations by number, and 0 is not a \
                 registration"
                    .to_string(),
            ));
        }

        let addr: SocketAddr = host
            .to_socket_addrs()
            .map_err(|e| ConsoleError::Nxdn(format!("resolve {host}: {e}")))?
            .next()
            .ok_or_else(|| ConsoleError::Nxdn(format!("resolve {host}: no addresses")))?;

        // Bind to the unspecified address on an ephemeral port, matching the
        // family of whatever we resolved to.
        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 bind")
        } else {
            "[::]:0".parse().expect("valid v6 bind")
        };
        let socket = UdpSocket::bind(bind).map_err(|e| ConsoleError::Nxdn(format!("bind: {e}")))?;
        let timeout = if audio.is_some() {
            AUDIO_RECV_TIMEOUT
        } else {
            RECV_TIMEOUT
        };
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|e| ConsoleError::Nxdn(format!("socket timeout: {e}")))?;

        let shared = Arc::new(Shared::new());

        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("astar-nxdn-link".into())
                .spawn(move || run(&socket, addr, fsm, &shared, audio))
                .map_err(|e| ConsoleError::Nxdn(format!("thread: {e}")))?
        };

        Ok(NxdnLink {
            shared,
            thread: Some(thread),
            backend,
        })
    }

    /// Current link state, cheap enough to poll.
    #[must_use]
    pub fn snapshot(&self) -> NxdnSnapshot {
        let id = self.shared.last_heard_id.load(Ordering::Relaxed);
        NxdnSnapshot {
            link_state: state_str(self.shared.link_state.load(Ordering::Relaxed)),
            last_heard: self.shared.last_heard.lock().map_or(None, |g| g.clone()),
            last_heard_id: (id != NO_TALKER).then(|| u16::try_from(id).unwrap_or(0)),
            frames_rx: self.shared.frames_rx.load(Ordering::Relaxed),
            receiving: self.shared.receiving.load(Ordering::Relaxed),
            backend: self.backend.map(AmbeBackend::as_str),
            ptt: self.shared.ptt.load(Ordering::Relaxed),
            // Console-owned: the floor here, overwritten by the session from
            // the one meter read at the lane. See the field docs.
            tx_dbfs: -60.0,
            rx_dbfs: -60.0,
        }
    }

    /// Request transmit on or off.
    ///
    /// Stores a request; the run loop applies the edge on its next pass —
    /// and **while transmit is gated it applies none**, so a key-down here
    /// is refused rather than queued and [`NxdnSnapshot::ptt`] never becomes
    /// true. See this module's Transmit section.
    ///
    /// This is still the ONLY path that can set the request true, which is
    /// what makes "nothing transmits unless the operator asked" checkable
    /// rather than asserted, and what Task 9 inherits.
    pub fn set_ptt(&self, on: bool) {
        self.shared.ptt_request.store(on, Ordering::Relaxed);
    }

    /// The link state as the protocol crate's own enum.
    ///
    /// [`NxdnSnapshot::link_state`] carries the ABI string for anything
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

impl std::fmt::Debug for NxdnLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NxdnLink")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl Drop for NxdnLink {
    /// Dropping a link unlinks it. An `NXDNReflector` would otherwise keep
    /// the client for the full 120 s of its own timeout, sending audio at a
    /// socket nobody is reading.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The link thread's body. The socket is owned by the spawned closure for
/// the thread's lifetime and borrowed here.
fn run(
    socket: &UdpSocket,
    addr: SocketAddr,
    mut fsm: NxdnFsm,
    shared: &Arc<Shared>,
    mut audio: Option<Audio>,
) {
    let publish = |fsm: &NxdnFsm| {
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

    // When the last network frame arrived, for the watchdog that closes a
    // transmission nobody ever ended.
    let mut last_data: Option<Instant> = None;

    let mut buf = [0_u8; astar_nxdn::reflector::MAX_DATAGRAM];
    while !shared.stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == addr => {
                let action = fsm.on_packet(&buf[..n], Instant::now());
                if matches!(action, FsmAction::Data(_)) {
                    last_data = Some(Instant::now());
                }
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
        rx_watchdog(shared, &mut last_data, Instant::now(), audio.as_mut());

        if let Some(a) = audio.as_mut() {
            // Apply the pending PTT edge — which, while transmit is gated,
            // means refusing it — and, on a pass that is not transmitting,
            // drop whatever the lane captured. In that order, in one step,
            // before anything else on this pass can block. See
            // `run_ptt_step`, where the ordering is the whole point.
            run_ptt_step(a, shared);
            // Keep the vocoder fed and the speaker supplied between
            // arrivals: four frames arrive at once and only four fit in the
            // pipeline, so the rest are submitted here.
            pump(a);
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
            // `srcId` is bytes 5..7 of the datagram, in clear — no vocoder
            // involved, so who is talking stays truthful even while this
            // station transmits.
            shared
                .last_heard_id
                .store(u32::from(packet.src_id), Ordering::Relaxed);
            if let Ok(mut slot) = shared.last_heard.lock() {
                *slot = Some(packet.src_id.to_string());
            }
            let end = is_end_of_transmission(packet);
            shared.receiving.store(!end, Ordering::Relaxed);
            if let Some(a) = audio {
                // HALF-DUPLEX. The ThumbDV is one physical link with one
                // AMBE-3000 behind it, and no session here decodes and
                // encodes at the same time: interleaving the two directions
                // on one chip is how both come out wrong. Only the voice is
                // refused; the header above is still read.
                if !a.transmitting {
                    decode_frame(&packet.frame, a);
                    if end {
                        // The tail of a transmission is still working
                        // through the pipeline when its last frame arrives.
                        // Play it, or every over loses its final ~80 ms —
                        // and one shorter than the pipeline depth would be
                        // silent.
                        flush(a);
                    }
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

/// Whether this frame ends the transmission.
///
/// Two independent statements of the same fact, because the wire carries it
/// twice and a client may set only one:
///
/// * the flags byte's `0x08`, which `NXDNGateway/NXDNNetwork.cpp: writeData`
///   sets from the LICH it saw and `NXDNReflector.cpp` reads back;
/// * the terminator frame itself — LICH `0x81`/`0x83`, [`USC_SACCH_NS`] with
///   both blocks stolen for FACCH1 and [`MESSAGE_TYPE_TX_REL`] in byte 0 of
///   block 0.
///
/// What is deliberately NOT here is `FrameKind::Signalling` on its own. A
/// mid-transmission frame with both halves stolen (`USC_SACCH_SS` +
/// `STEAL_FACCH`) reads as Signalling too, and ending an over on that would
/// cut every transmission short at its first stolen frame. The `USC` check
/// is what separates the two.
fn is_end_of_transmission(packet: &wire::DataPacket) -> bool {
    if packet.end {
        return true;
    }
    let view = NetFrame::new(&packet.frame);
    view.lich().fct() == USC_SACCH_NS
        && matches!(
            view.kind(),
            astar_nxdn::FrameKind::Signalling { message_type } if message_type == MESSAGE_TYPE_TX_REL
        )
}

/// Close a transmission that stopped without saying so.
///
/// A client that is unplugged mid-over sends no terminator and no
/// end-flagged frame, and without this "receiving" would latch on until the
/// next transmission — a UI lying about the air. [`RX_WATCHDOG`] is the
/// reference reflector's own 1.5 s, so this link and the reflector agree
/// about when the over ended.
///
/// Clearing `last_data` is what makes it fire once rather than on every pass
/// of a quiet link.
fn rx_watchdog(
    shared: &Arc<Shared>,
    last_data: &mut Option<Instant>,
    now: Instant,
    audio: Option<&mut Audio>,
) {
    let Some(last) = *last_data else {
        return;
    };
    if now.duration_since(last) < RX_WATCHDOG {
        return;
    }
    *last_data = None;
    if !shared.receiving.swap(false, Ordering::Relaxed) {
        return;
    }
    tracing::debug!("nxdn: no frame for {RX_WATCHDOG:?}, treating the transmission as over");
    if let Some(a) = audio
        && !a.transmitting
    {
        flush(a);
    }
}

/// Turn one received network frame into voice on the output bus.
///
/// Everything that can go wrong here is a property of the frame, not of the
/// link, so nothing in this function tears the session down.
fn decode_frame(bytes: &[u8; astar_nxdn::FRAME_LEN], audio: &mut Audio) {
    match unpack_voice(bytes) {
        Ok(frames) => {
            audio.pending.extend(frames);
            pump(audio);
        }
        // A header or a terminator is not an error and not a refusal to
        // report: it is the normal shape of the first and last frame of
        // every transmission, and there is exactly one voice mode on this
        // network for an operator to be told about. So this is silent,
        // where `ysf.rs` records an unsupported mode.
        Err(NxdnVoiceError::Signalling { .. }) => {}
        Err(NxdnVoiceError::NotVoice { lich }) => {
            tracing::debug!(lich, "nxdn: a frame this build does not decode");
        }
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
    // finishes a whole network frame's worth in ~30 ms and the bus wants
    // them spread over 80. See `release`.
    while let Some(pcm) = audio.ambe.poll_decoded() {
        audio.decoded.push_back(pcm);
    }
    release(audio, Instant::now());
}

/// One run-loop pass's transmit step: apply the pending PTT edge, then — on
/// a pass that is NOT transmitting — drain and DROP whatever the capture
/// lane has queued.
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
/// it exists to protect. D-Star shipped that bug once; neither YSF nor NXDN
/// repeats it.
///
/// While transmit is gated the drop runs on every pass, since no pass is
/// ever keyed. The ordering is kept anyway, because Task 9 adds the
/// key-DOWN edge back into `apply_ptt` and not into this function.
///
/// Task 9 also gives both functions the socket, the callsign and the radio
/// id they will need to put a frame on the wire; nothing here can use them
/// yet, and carrying arguments no line reads would only make the refusal
/// look like an implementation.
fn run_ptt_step(audio: &mut Audio, shared: &Arc<Shared>) {
    apply_ptt(audio, shared);
    if !audio.transmitting {
        while audio.bus.tx_frames.try_recv().is_ok() {}
    }
}

/// Start or stop a transmission, if the request differs from what is
/// applied — except that, today, it can only refuse.
fn apply_ptt(audio: &mut Audio, shared: &Arc<Shared>) {
    let want = shared.ptt_request.load(Ordering::Relaxed);
    if want == audio.transmitting {
        return;
    }
    if want {
        // RECEIVE-FIRST GATE. The transmit path is Task 9 of
        // docs/superpowers/plans/2026-09-07-nxdn-network.md and does not
        // exist yet. A key that silently did nothing would be worse than one
        // that is refused: the operator would hear nothing happen and
        // reasonably assume the radio or the link was broken. So the request
        // is dropped here, `ptt` never becomes true, and the app shows a
        // receive-only indicator instead of a PTT button.
        shared.ptt_request.store(false, Ordering::Relaxed);
        tracing::warn!("nxdn: transmit is not implemented; the key was refused, not queued");
        return;
    }
    // Unreachable while the gate above holds — nothing can set
    // `transmitting` — and written out rather than assumed, so that Task 9
    // replaces a branch instead of adding one.
    shared.ptt.store(false, Ordering::Relaxed);
    audio.transmitting = false;
}

/// Hand decoded frames to the output bus on the audio clock — one per
/// [`FRAME_INTERVAL`] — rather than as fast as the vocoder produces them.
///
/// The bus consumes at exactly 50 frames a second. Handing it four at once
/// and then nothing for the rest of the 80 ms makes it starve in the hole,
/// which is audible as a beat; this is what turns a bursty wire cadence back
/// into a steady one.
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
                "nxdn: vocoder flush hit its {FLUSH_DEADLINE:?} deadline, abandoning the rest"
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
    use astar_codec::nxdn::{FRAMES_PER_FRAME, pack_voice_block};
    use astar_nxdn::frame::{
        MESSAGE_TYPE_TX_REL, MESSAGE_TYPE_VCALL, RFCT_RDCH, STEAL_FACCH, STEAL_NONE, USC_SACCH_SS,
    };
    use astar_nxdn::{Callsign, FRAME_LEN, Reflector};
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// The talkgroup every test here links to. Any number does; this one is
    /// the reference implementations' own example.
    const TG: u16 = 31313;
    /// A radio id for the station under test. Sixteen bits, as the wire's
    /// `srcId` is.
    const RADIO_ID: u16 = 4242;

    /// Everything here binds `127.0.0.1` and talks to a reflector this test
    /// started. Nothing reaches a real network — CLAUDE.md's on-air safety
    /// rule, which this crate is squarely inside.
    fn loopback() -> (astar_nxdn::ReflectorHandle, SocketAddr) {
        let r = Reflector::bind("127.0.0.1:0".parse().expect("v4"), TG).expect("bind reflector");
        let addr = r.local_addr();
        (r.run(), addr)
    }

    fn wait_for(link: &NxdnLink, want: &str, within: Duration) -> NxdnSnapshot {
        let deadline = Instant::now() + within;
        loop {
            let snap = link.snapshot();
            if snap.link_state == want || Instant::now() > deadline {
                return snap;
            }
            thread::sleep(Duration::from_millis(20));
        }
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

    /// A vocoder that answers immediately and remembers what it was asked.
    ///
    /// Each frame decodes to 160 samples of its own first byte, so decoded
    /// audio can be attributed to the exact frame that produced it.
    struct FakeVocoder {
        submitted: Vec<ChannelFrame>,
        ready: VecDeque<[i16; 160]>,
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
        fn poll_encoded(&mut self) -> Option<ChannelFrame> {
            self.encoded.pop_front().map(ChannelFrame::YsfDn)
        }
        fn in_flight_encoded(&self) -> usize {
            self.encoded.len()
        }
    }

    /// An `Audio` wired to channels the test can read, with no device
    /// anywhere.
    fn test_audio() -> (Audio, Receiver<Vec<i16>>) {
        let (audio, rx, _tx) = test_audio_with_capture();
        (audio, rx)
    }

    /// [`test_audio`] with the capture end kept, for the tests that deliver
    /// mic frames into the lane the way the router's own `MicLane` does.
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
                decoded: VecDeque::new(),
                next_release: None,
                transmitting: false,
            },
            rx_rx,
            tx_tx,
        )
    }

    /// One voice frame carrying four distinguishable AMBE frames, built with
    /// the codec's own packer so the test reads the same layout the decoder
    /// does.
    ///
    /// LICH: `RFCT_RDCH`, `USC_SACCH_SS`, `STEAL_NONE` — the four-AMBE case
    /// of `MMDVMHost/NXDNControl.cpp`'s AUDIO branch.
    fn voice_frame() -> [u8; FRAME_LEN] {
        let voice: [DnFrame; FRAMES_PER_FRAME] = core::array::from_fn(|i| {
            DnFrame::from_bytes([u8::try_from(i + 1).expect("small"), 0, 0, 0, 0, 0, 0])
        });
        let mut bytes = [0u8; FRAME_LEN];
        bytes[0] = (RFCT_RDCH << 6) | (USC_SACCH_SS << 4) | (STEAL_NONE << 2);
        bytes[5..19].copy_from_slice(&pack_voice_block(voice[0], voice[1]));
        bytes[19..33].copy_from_slice(&pack_voice_block(voice[2], voice[3]));
        bytes
    }

    /// A voice header or terminator: LICH `0x81` — `RFCT_RDCH`,
    /// `USC_SACCH_NS`, `STEAL_FACCH`, parity 1 — with the Layer-3 message
    /// type in byte 0 of each block, exactly where
    /// `NXDNGateway/NXDNNetwork.cpp: writeData` reads it (`data[5U]`).
    fn header_frame(message_type: u8) -> [u8; FRAME_LEN] {
        let mut bytes = [0u8; FRAME_LEN];
        bytes[0] = 0x81;
        bytes[5] = message_type;
        bytes[19] = message_type;
        bytes
    }

    /// A MID-TRANSMISSION frame with both halves stolen for FACCH1:
    /// `USC_SACCH_SS` + `STEAL_FACCH`. It reads as `FrameKind::Signalling`
    /// like a terminator does, and it is not the end of anything.
    fn stolen_frame(message_type: u8) -> [u8; FRAME_LEN] {
        let mut bytes = [0u8; FRAME_LEN];
        bytes[0] = (RFCT_RDCH << 6) | (USC_SACCH_SS << 4) | (STEAL_FACCH << 2);
        bytes[5] = message_type;
        bytes[19] = message_type;
        bytes
    }

    fn packet(frame: [u8; FRAME_LEN], start: bool, end: bool) -> wire::DataPacket {
        wire::DataPacket {
            src_id: RADIO_ID,
            dst_id: TG,
            group: true,
            data: false,
            start,
            end,
            frame,
        }
    }

    // ── The link ────────────────────────────────────────────────────────

    #[test]
    fn linking_to_a_reflector_reaches_linked() {
        let (reflector, addr) = loopback();
        let link = NxdnLink::connect(&addr.to_string(), "KC0ABC", RADIO_ID, TG).expect("connect");
        let snap = wait_for(&link, "linked", Duration::from_secs(5));
        assert_eq!(snap.link_state, "linked", "link never came up: {snap:?}");
        assert_eq!(snap.frames_rx, 0, "a poll is not a network frame");
        assert!(snap.last_heard.is_none(), "nobody has transmitted");
        link.disconnect();
        reflector.shutdown();
    }

    #[test]
    fn an_impossible_callsign_is_refused_at_connect() {
        let err = NxdnLink::connect("127.0.0.1:41400", "0123456789A", RADIO_ID, TG)
            .expect_err("should refuse");
        let text = err.to_string();
        assert!(text.contains("nxdn:"), "{text}");
        assert!(text.contains("callsign"), "{text}");
    }

    /// A radio id is a registration, not a default. Zero is what an unset
    /// field looks like, and putting it on the air would claim somebody
    /// else's number — or nobody's.
    #[test]
    fn a_zero_radio_id_is_refused_at_connect() {
        let err = NxdnLink::connect("127.0.0.1:41400", "KC0ABC", 0, TG).expect_err("should refuse");
        assert!(err.to_string().contains("radio id"), "{err}");
    }

    #[test]
    fn an_unresolvable_host_is_refused_at_connect() {
        let err = NxdnLink::connect("no-such-host.invalid:41400", "KC0ABC", RADIO_ID, TG)
            .expect_err("should refuse");
        assert!(err.to_string().contains("resolve"), "{err}");
    }

    #[test]
    fn dropping_the_handle_stops_the_thread() {
        let (reflector, addr) = loopback();
        {
            let link =
                NxdnLink::connect(&addr.to_string(), "KC0ABC", RADIO_ID, TG).expect("connect");
            let _ = wait_for(&link, "linked", Duration::from_secs(5));
        }
        reflector.shutdown();
    }

    #[test]
    fn snapshot_is_readable_before_the_link_comes_up() {
        let (reflector, addr) = loopback();
        let link = NxdnLink::connect(&addr.to_string(), "KC0ABC", RADIO_ID, TG).expect("connect");
        let snap = link.snapshot();
        assert!(
            ["idle", "linking", "linked"].contains(&snap.link_state),
            "unexpected early state {snap:?}"
        );
        link.disconnect();
        reflector.shutdown();
    }

    #[test]
    fn a_link_without_audio_reports_no_backend() {
        let (reflector, addr) = loopback();
        let link = NxdnLink::connect(&addr.to_string(), "KC0ABC", RADIO_ID, TG).expect("connect");
        assert_eq!(link.snapshot().backend, None);
        link.disconnect();
        reflector.shutdown();
    }

    // ── The receive path ────────────────────────────────────────────────

    #[test]
    fn a_voice_frame_becomes_four_frames_of_speech() {
        let (mut audio, rx) = test_audio();
        decode_frame(&voice_frame(), &mut audio);
        flush(&mut audio);
        let played: Vec<Vec<i16>> = rx.try_iter().collect();
        assert_eq!(
            played.len(),
            FRAMES_PER_FRAME,
            "one 80 ms network frame carries four 20 ms voice frames"
        );
        for (i, pcm) in played.iter().enumerate() {
            assert_eq!(pcm.len(), 160, "8 kHz, 20 ms");
            assert_eq!(
                pcm[0],
                i16::try_from(i + 1).expect("small"),
                "frames must reach the speaker in the order they were carried"
            );
        }
    }

    /// A header is not an error and not audio: it must produce no samples
    /// and tear nothing down.
    #[test]
    fn a_signalling_frame_is_not_decoded_as_voice() {
        let (mut audio, rx) = test_audio();
        decode_frame(&header_frame(MESSAGE_TYPE_VCALL), &mut audio);
        flush(&mut audio);
        assert_eq!(rx.try_iter().count(), 0);
        assert!(audio.pending.is_empty());
    }

    /// The vocoder accepts `AMBE_STREAM_MAX_IN_FLIGHT` (4) at a time and a
    /// frame carries exactly four, so the queue is the margin, not an
    /// optimisation: two frames back to back must lose nothing.
    #[test]
    fn the_fourth_frame_of_a_burst_is_not_dropped() {
        let (mut audio, rx) = test_audio();
        decode_frame(&voice_frame(), &mut audio);
        decode_frame(&voice_frame(), &mut audio);
        flush(&mut audio);
        assert_eq!(rx.try_iter().count(), 2 * FRAMES_PER_FRAME);
        assert!(audio.pending.is_empty(), "the queue must drain");
    }

    /// `release` on a fabricated clock — the pacing rule itself, with no
    /// wall-clock dependence.
    #[test]
    fn frames_are_released_one_per_frame_interval_after_priming() {
        let (mut audio, rx) = test_audio();
        let t0 = Instant::now();

        for _ in 0..(PRIME_FRAMES - 1) {
            audio.decoded.push_back([1i16; 160]);
        }
        release(&mut audio, t0);
        assert_eq!(rx.try_iter().count(), 0, "must not release under-primed");

        for _ in 0..8 {
            audio.decoded.push_back([1i16; 160]);
        }
        release(&mut audio, t0);
        assert_eq!(rx.try_iter().count(), 1, "one frame, not the burst");

        release(&mut audio, t0 + Duration::from_millis(19));
        assert_eq!(rx.try_iter().count(), 0, "not due yet");

        release(&mut audio, t0 + FRAME_INTERVAL);
        assert_eq!(rx.try_iter().count(), 1, "exactly one per interval");

        release(&mut audio, t0 + FRAME_INTERVAL * 4);
        assert_eq!(
            rx.try_iter().count(),
            3,
            "catch-up is bounded by what is due"
        );

        while audio.decoded.pop_front().is_some() {}
        release(&mut audio, t0 + FRAME_INTERVAL * 10);
        assert!(
            audio.next_release.is_none(),
            "an emptied queue must re-prime, or the next frame reopens the gap"
        );
    }

    /// NXDND carries `srcId` in bytes 5..7 in clear, so a link with no
    /// vocoder still answers "who is on this talkgroup".
    #[test]
    fn the_talker_is_read_from_the_header_and_needs_no_vocoder() {
        let shared = Arc::new(Shared::new());
        let (sock, addr, _peer) = udp_pair();
        assert!(handle(
            &FsmAction::Data(Box::new(packet(voice_frame(), true, false))),
            &sock,
            addr,
            &shared,
            None,
        ));
        assert_eq!(
            shared.last_heard_id.load(Ordering::Relaxed),
            u32::from(RADIO_ID)
        );
        assert_eq!(
            shared.last_heard.lock().expect("mutex").as_deref(),
            Some("4242")
        );
        assert!(shared.receiving.load(Ordering::Relaxed));
        assert_eq!(shared.frames_rx.load(Ordering::Relaxed), 1);
    }

    /// `NXDNReflector.cpp` treats `flags & 0x08` as end of transmission.
    #[test]
    fn the_end_flag_clears_receiving() {
        let shared = Arc::new(Shared::new());
        let (sock, addr, _peer) = udp_pair();
        shared.receiving.store(true, Ordering::Relaxed);
        handle(
            &FsmAction::Data(Box::new(packet(
                header_frame(MESSAGE_TYPE_TX_REL),
                false,
                true,
            ))),
            &sock,
            addr,
            &shared,
            None,
        );
        assert!(!shared.receiving.load(Ordering::Relaxed));
    }

    /// The flag is set by the GATEWAY, from the LICH it saw. A sender that
    /// speaks the wire directly may send the terminator without it, so the
    /// LICH is read too: `USC_SACCH_NS` + `TX_REL` is the end.
    #[test]
    fn a_terminator_lich_ends_the_transmission_without_the_flag() {
        let shared = Arc::new(Shared::new());
        let (sock, addr, _peer) = udp_pair();
        shared.receiving.store(true, Ordering::Relaxed);
        handle(
            &FsmAction::Data(Box::new(packet(
                header_frame(MESSAGE_TYPE_TX_REL),
                false,
                false,
            ))),
            &sock,
            addr,
            &shared,
            None,
        );
        assert!(!shared.receiving.load(Ordering::Relaxed));
    }

    /// The trap in the middle: a mid-transmission frame with both halves
    /// stolen for FACCH1 is `FrameKind::Signalling` too, and ending a
    /// transmission on that would cut every over short at its first stolen
    /// frame. Only `USC_SACCH_NS` — the non-superframe SACCH a header and
    /// terminator carry — ends anything.
    #[test]
    fn a_mid_transmission_stolen_frame_is_not_the_end() {
        let shared = Arc::new(Shared::new());
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(packet(voice_frame(), true, false))),
            &sock,
            addr,
            &shared,
            None,
        );
        handle(
            &FsmAction::Data(Box::new(packet(
                stolen_frame(MESSAGE_TYPE_TX_REL),
                false,
                false,
            ))),
            &sock,
            addr,
            &shared,
            None,
        );
        assert!(
            shared.receiving.load(Ordering::Relaxed),
            "a stolen half is not a terminator"
        );
    }

    /// A transmission that stops without saying so must not leave the UI
    /// showing "receiving" forever. The reference reflector's own watchdog
    /// is 1.5 s (`NXDNReflector.cpp: CTimer watchdogTimer(1000U, 0U,
    /// 1500U);`) and this is the client-side half of it.
    #[test]
    fn a_transmission_that_stops_dead_is_closed_by_the_watchdog() {
        let shared = Arc::new(Shared::new());
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(packet(voice_frame(), true, false))),
            &sock,
            addr,
            &shared,
            None,
        );
        let mut last_data = Some(Instant::now());
        rx_watchdog(&shared, &mut last_data, Instant::now(), None);
        assert!(
            shared.receiving.load(Ordering::Relaxed),
            "a frame just arrived; nothing has expired"
        );
        let late = Instant::now() + RX_WATCHDOG + Duration::from_millis(1);
        rx_watchdog(&shared, &mut last_data, late, None);
        assert!(!shared.receiving.load(Ordering::Relaxed));
        assert!(last_data.is_none(), "and does not fire twice");
    }

    /// One `ThumbDV`, one AMBE-3000: never decode and encode at once.
    #[test]
    fn a_received_frame_is_not_decoded_while_transmitting() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new());
        let (sock, addr, _peer) = udp_pair();
        audio.transmitting = true;
        handle(
            &FsmAction::Data(Box::new(packet(voice_frame(), false, false))),
            &sock,
            addr,
            &shared,
            Some(&mut audio),
        );
        flush(&mut audio);
        assert_eq!(
            rx.try_iter().count(),
            0,
            "no received audio may be decoded while this station transmits"
        );
        // The talker is still tracked: reading the header costs no vocoder.
        assert_eq!(
            shared.last_heard_id.load(Ordering::Relaxed),
            u32::from(RADIO_ID)
        );
    }

    // ── Transmit, which does not exist yet ──────────────────────────────

    /// The gate can be open before the loop observes the request; without
    /// this drop the next transmission would open with stale speech.
    #[test]
    fn an_unkeyed_pass_drops_whatever_the_lane_captured() {
        let (mut audio, _rx, mic) = test_audio_with_capture();
        let shared = Arc::new(Shared::new());
        for _ in 0..10 {
            mic.send(vec![9i16; 160]).expect("queue a mic frame");
        }
        run_ptt_step(&mut audio, &shared);
        assert!(!audio.transmitting, "nothing asked for a transmission");
        assert!(
            audio.bus.tx_frames.try_recv().is_err(),
            "an unkeyed pass must empty the capture channel"
        );
    }

    /// The safety property, stated as a test: a link puts NOTHING on the
    /// wire but polls, even with a vocoder attached, a mic lane feeding it
    /// and the operator holding the key down. If this ever fails, astar
    /// transmitted without being asked.
    ///
    /// The "reflector" here is a bare socket on `127.0.0.1` that answers
    /// nothing — every datagram it receives is one astar chose to send.
    #[test]
    fn an_unkeyed_link_never_sends_a_voice_frame() {
        let peer = UdpSocket::bind("127.0.0.1:0").expect("bind peer");
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        let addr = peer.local_addr().expect("addr");

        let (rx_tx, _rx_rx) = channel::<Vec<i16>>();
        let (mic, tx_rx) = channel::<Vec<i16>>();
        let link = NxdnLink::connect_with_stream(
            &NxdnConfig {
                host: addr.to_string(),
                callsign: "KC0ABC".to_string(),
                radio_id: RADIO_ID,
                talkgroup: TG,
            },
            CallAudio {
                tx_frames: tx_rx,
                rx_frames: rx_tx,
                preroll_lead: Arc::new(AtomicU32::new(0)),
            },
            Box::new(FakeVocoder::new()),
            AmbeBackend::Hardware,
        )
        .expect("connect");

        // Everything a transmission would need: speech in the lane and a
        // key held down.
        link.set_ptt(true);
        for _ in 0..50 {
            mic.send(vec![9i16; 160]).expect("queue a mic frame");
        }
        thread::sleep(Duration::from_millis(300));

        let mut buf = [0u8; 128];
        let mut polls = 0usize;
        while let Ok((n, _)) = peer.recv_from(&mut buf) {
            match wire::parse(&buf[..n]) {
                Some(wire::Packet::Poll { .. } | wire::Packet::Unlink { .. }) => polls += 1,
                other => panic!("an unkeyed link put this on the wire: {other:?}"),
            }
        }
        assert!(polls > 0, "the link must at least be polling");
        assert!(!link.snapshot().ptt, "nothing may key on its own");
        link.disconnect();
    }

    /// TASK 9 REPLACES THIS TEST. Until the transmit path exists,
    /// `set_ptt(true)` must not be able to produce a keyed state — the
    /// receive-first gate, enforced in code rather than asserted in prose.
    #[test]
    fn set_ptt_is_refused_until_transmit_lands() {
        let (reflector, addr) = loopback();
        let link = NxdnLink::connect(&addr.to_string(), "KC0ABC", RADIO_ID, TG).expect("connect");
        let _ = wait_for(&link, "linked", Duration::from_secs(5));
        link.set_ptt(true);
        thread::sleep(Duration::from_millis(200));
        assert!(!link.snapshot().ptt, "a request is not a transmission");
        link.disconnect();
        reflector.shutdown();
    }

    /// And the same refusal at the level below, with audio present: the
    /// applied state never moves, and the capture lane is drained rather
    /// than encoded.
    #[test]
    fn a_key_request_with_audio_present_is_still_refused() {
        let (mut audio, _rx, mic) = test_audio_with_capture();
        let shared = Arc::new(Shared::new());
        shared.ptt_request.store(true, Ordering::Relaxed);
        mic.send(vec![7i16; 160]).expect("queue");
        run_ptt_step(&mut audio, &shared);
        assert!(!audio.transmitting, "the key must not open a transmission");
        assert!(!shared.ptt.load(Ordering::Relaxed));
        assert!(
            !shared.ptt_request.load(Ordering::Relaxed),
            "the refused request must be cleared, not left latched"
        );
    }

    // ── Delivery cadence ────────────────────────────────────────────────
    //
    // The output bus consumes 20 ms frames at a steady 50/s. What this
    // measures is whether the decode path DELIVERS them at that rate, or in
    // bursts with gaps the bus has to paper over — a gap is a dropout, and a
    // periodic gap is a beat. NXDN's burst is four frames every 80 ms, which
    // is a tighter margin than YSF's five every 100.

    /// A vocoder with realistic latency: a frame submitted now is answered
    /// `LATENCY` later, matching the `ThumbDV`'s measured ~7.45 ms/frame
    /// pipelined cost. An instant fake would hide the very timing under
    /// test.
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
            unreachable!("this link never encodes")
        }
        fn poll_encoded(&mut self) -> Option<ChannelFrame> {
            None
        }
        fn in_flight_encoded(&self) -> usize {
            0
        }
    }

    /// Drives the real `decode_frame`/`pump` at the reflector's real cadence
    /// — one 4-frame network frame every 80 ms, the run loop waking every
    /// 20 ms in between — and reports when audio actually reached the bus.
    #[test]
    #[ignore = "timing probe, run explicitly: cargo test -p astar-console --features nxdn -- --ignored delivery"]
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
            transmitting: false,
        };
        let frame = voice_frame();

        let t0 = Instant::now();
        let mut arrivals = Vec::new();
        // 75 network frames = 6 seconds of continuous speech, which is the
        // "after a few seconds" a beat is reported at.
        for n in 0..75u64 {
            let due = t0 + Duration::from_millis(n * 80);
            while Instant::now() < due {
                thread::sleep(Duration::from_millis(20));
                pump(&mut audio);
                while rx_rx.try_recv().is_ok() {
                    arrivals.push(Instant::now().duration_since(t0).as_secs_f64());
                }
            }
            decode_frame(&frame, &mut audio);
            while rx_rx.try_recv().is_ok() {
                arrivals.push(Instant::now().duration_since(t0).as_secs_f64());
            }
        }
        flush(&mut audio);
        while rx_rx.try_recv().is_ok() {
            arrivals.push(Instant::now().duration_since(t0).as_secs_f64());
        }

        println!("frames sent: 75 (= 300 voice frames = 6.00s of audio)");
        println!("frames delivered: {}", arrivals.len());
        println!("pending left over: {}", audio.pending.len());
        let gaps: Vec<f64> = arrivals.windows(2).map(|w| w[1] - w[0]).collect();
        let mut sorted = gaps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        if !sorted.is_empty() {
            println!(
                "gap p50={:.1}ms p90={:.1}ms max={:.1}ms",
                sorted[sorted.len() / 2] * 1000.0,
                sorted[sorted.len() * 9 / 10] * 1000.0,
                sorted[sorted.len() - 1] * 1000.0
            );
        }
        for thresh_ms in [40.0, 60.0, 80.0] {
            let n = gaps.iter().filter(|g| **g * 1000.0 >= thresh_ms).count();
            println!(
                "  gaps >= {:>3.0}ms: {:>3}  = {:.1}/second",
                thresh_ms,
                n,
                f64::from(u32::try_from(n).unwrap_or(u32::MAX)) / 6.0
            );
        }
    }

    // ── End to end, over 127.0.0.1 ──────────────────────────────────────

    /// The whole path: a frame put on the reflector by another client is
    /// relayed, parsed, decoded and released as speech. Nothing here leaves
    /// the loopback interface.
    #[test]
    fn a_relayed_frame_reaches_the_speaker() {
        let (reflector, addr) = loopback();
        let (rx_tx, rx_rx) = channel::<Vec<i16>>();
        let (_mic, tx_rx) = channel::<Vec<i16>>();
        let link = NxdnLink::connect_with_stream(
            &NxdnConfig {
                host: addr.to_string(),
                callsign: "KC0ABC".to_string(),
                radio_id: RADIO_ID,
                talkgroup: TG,
            },
            CallAudio {
                tx_frames: tx_rx,
                rx_frames: rx_tx,
                preroll_lead: Arc::new(AtomicU32::new(0)),
            },
            Box::new(FakeVocoder::new()),
            AmbeBackend::Hardware,
        )
        .expect("connect");
        assert_eq!(
            wait_for(&link, "linked", Duration::from_secs(5)).link_state,
            "linked"
        );

        // A second client, registered on the same talkgroup, transmits.
        let other = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let them = Callsign::new("W1AW").expect("callsign");
        other
            .send_to(&wire::poll(&them, TG), addr)
            .expect("register");
        let mut speech = packet(voice_frame(), true, false);
        speech.src_id = 7863;
        for _ in 0..4 {
            other.send_to(&wire::data(&speech), addr).expect("send");
        }
        let mut done = speech.clone();
        done.frame = header_frame(MESSAGE_TYPE_TX_REL);
        done.start = false;
        done.end = true;
        other.send_to(&wire::data(&done), addr).expect("send");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frames = 0usize;
        while frames < 4 * FRAMES_PER_FRAME && Instant::now() < deadline {
            if let Ok(pcm) = rx_rx.recv_timeout(Duration::from_millis(200)) {
                assert_eq!(pcm.len(), 160);
                frames += 1;
            }
        }
        assert_eq!(
            frames,
            4 * FRAMES_PER_FRAME,
            "every relayed voice frame must reach the speaker"
        );
        let snap = link.snapshot();
        assert_eq!(snap.last_heard_id, Some(7863));
        assert_eq!(
            snap.backend,
            Some("thumbdv"),
            "the fake stands in for the dongle"
        );
        assert!(snap.frames_rx >= 5);

        link.disconnect();
        reflector.shutdown();
    }
}
