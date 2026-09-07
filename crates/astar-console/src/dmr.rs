// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A live DMR master link: the socket and the thread that drive
//! [`astar_dmr::DmrFsm`] (iax-d4f7, designed in `docs/design/dmr-wire.md`).
//!
//! `astar-dmr` is deliberately pure — the FSM decides and hands back bytes to
//! send, and `astar_codec::dmr` lifts the three AMBE+2 frames out of a burst —
//! so something has to own a `UdpSocket`, feed it datagrams, call `tick`, and
//! put the result on the audio bus. That is this module, and it is
//! [`crate::nxdn`] with the differences DMR actually has and no others.
//!
//! # Three ways to open a link, and only two of them make sound
//!
//! [`DmrLink::connect`] is the link alone: it completes the homebrew
//! handshake, pings to hold the session open, and reports who is transmitting
//! from the `DMRD` header. That needs no hardware — `srcId` is bytes 5..8 of
//! the datagram in clear, so "is this master alive, and who is on this
//! talkgroup" is answerable with no vocoder involved.
//!
//! [`DmrLink::connect_with_audio`] adds the voice path: an [`AmbeStream`]
//! opened in [`VocoderMode::Dmr`], the console's audio lane, and the loop that
//! lifts three 20 ms voice frames out of each burst with
//! [`astar_codec::dmr::unpack_voice`] and plays them.
//!
//! [`DmrLink::connect_with_stream`] is the same with the vocoder supplied by
//! the caller — the `astar-station` facade, so the `ThumbDV` probe runs
//! outside its session mutex, and the tests, where a fake [`AmbeStream`]
//! decodes against a loopback master on `127.0.0.1`.
//!
//! **[`VocoderMode::Dmr`] is not [`VocoderMode::Dstar`].** They share the
//! channel width — 72 bits, nine bytes — and share nothing else: DMR is
//! 2450 + 1150 and D-Star is 2400 + 1200, one 17-byte rate word apart. The
//! chip does not complain about the wrong one; it returns confident noise.
//! `astar_codec::dmr` has the reading.
//!
//! # Secrets
//!
//! One secret exists anywhere in the DMR path: the master password, and it is
//! a connect-time in-arg and nothing else. [`DmrConfig::password`] is moved
//! into [`DmrFsm`] at connect, spent on one `RPTK` digest, and dropped with
//! the FSM. It is not on [`DmrLink`], not in [`DmrSnapshot`], not in a
//! [`ConsoleError`] this module builds, and [`DmrConfig`]'s hand-written
//! `Debug` prints `<redacted>` where it would otherwise print it. That is
//! CLAUDE.md's rule for the node secret and the portal pass, applied to the
//! one credential this network has, and
//! `the_password_never_reaches_a_snapshot_an_error_or_debug_output` makes it a
//! property of the code rather than a promise in a comment.
//!
//! It is also why [`DmrLink::connect`] takes its config **by value** where
//! YSF's and NXDN's take a reference: a caller that keeps a borrowed config
//! keeps the password alive somewhere this module cannot see. Moving it in
//! means the compiler enforces that the caller no longer has it.
//!
//! [`FailureStage::as_str`] is the only thing that crosses a boundary about
//! why a link died. `auth` means the digest was refused; it does not carry,
//! echo or hint at what was digested.
//!
//! # Transmit
//!
//! **There is none, and the key is refused rather than ignored.**
//! [`DmrLink::set_ptt`] stores a request exactly as [`crate::ysf`]'s does, and
//! `apply_ptt` — the one place a request could become a transmission — drops
//! it and clears it. [`DmrSnapshot::ptt`] is therefore always false.
//!
//! That is a receive-first gate, not an oversight: the transmit path is Task
//! 12 of `docs/superpowers/plans/2026-09-07-dmr-network.md`, and a key that
//! silently did nothing would be worse than one that is refused — the operator
//! would hear nothing happen and reasonably conclude the radio or the link was
//! broken. A caller shows a receive-only indicator instead of a PTT button,
//! and `keying_is_refused_while_transmit_is_gated` holds it to that in code.
//!
//! It matters more here than it did for YSF or NXDN: a DMR master routes by
//! the radio ID this link logged in with, so a half-built transmit path would
//! put malformed bursts on a live network under the operator's own
//! registration.
//!
//! One thing Task 12 inherits: `run_ptt_step` — the one place a request is
//! dropped and cleared — runs only inside `if let Some(a) = audio`, so a link
//! opened WITHOUT audio latches `ptt_request` and never clears it. Harmless
//! while nothing reads it (the snapshot's `ptt` is a constant false), and
//! invisible: an audio-less link is the loopback tests' shape, not an
//! operator's. Whoever builds transmit must move the clear out of that arm or
//! decide deliberately that an audio-less link cannot be keyed.
//!
//! # Half-duplex
//!
//! The `ThumbDV` is one physical link with one AMBE-3000 behind it, so this
//! link never decodes and encodes at the same time — the rule
//! [`crate::dstar`] and [`crate::ysf`] both enforce. While transmitting, a
//! received burst's voice is never submitted to the decoder. Reading `srcId`
//! costs no vocoder, so last-heard stays truthful either way: only the voice
//! is refused.
//!
//! # The room this link is in
//!
//! DMR is two networks in one wire: TDMA gives a master two timeslots, and a
//! talkgroup number means nothing without one. Every `DMRD` is filtered by
//! **both** before anything else in this module touches it — the slot from
//! the bits byte's `0x80`, the talkgroup from the destination id. The FSM
//! filters too, because a homebrew master relays every room it carries and
//! somebody has to choose; this second filter is what keeps a burst from the
//! other slot out of the audio path and out of the talker display.
//!
//! # What ends a transmission
//!
//! Three things, and this module reads all of them.
//!
//! * `DT_TERMINATOR_WITH_LC` — the data-sync burst that closes a stream
//!   (`MMDVMHost/DMRDefines.h`), and what `hblink3/playback.py` stops
//!   recording on.
//! * A change of stream id. It is constant for one key-down and random across
//!   them, so a change is the far end of an over — and terminators get lost.
//! * [`RX_WATCHDOG`], for a client that stops dead and sends neither.
//!
//! # Threading
//!
//! One thread per link, owning the socket. It blocks on `recv_from` with a
//! read timeout so `tick` still runs when the master goes quiet — that timeout
//! is what makes the ping cadence and the link timeout work at all. The
//! control side sees an `AtomicU*` snapshot and a mutex-guarded last-heard
//! string, the same arrangement [`crate::nxdn`] uses.

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_audio::CallAudio;
use astar_codec::ambe::{
    AMBE_STREAM_MAX_IN_FLIGHT, AmbeBackend, AmbeStream, ChannelFrame, VocoderMode, open_ambe_stream,
};
use astar_codec::dmr::unpack_voice;
use astar_dmr::frame::DT_TERMINATOR_WITH_LC;
use astar_dmr::{CallType, DmrFsm, DmrNetwork, FailureStage, FsmAction, LinkState, Timeslot, wire};

use crate::session::ConsoleError;

/// How long the socket blocks before the loop runs `tick` anyway.
///
/// Shorter than the 5 s ping interval by enough that a ping is never late by a
/// meaningful fraction of it, and long enough that an idle link is not a busy
/// loop.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// [`RECV_TIMEOUT`] for a link that is decoding audio.
///
/// A master sends one burst every 60 ms carrying three 20 ms voice frames, and
/// the vocoder accepts only [`AMBE_STREAM_MAX_IN_FLIGHT`] (4) at a time — so a
/// burst that arrives while one is still in flight cannot be submitted in one
/// go, and the loop has to come back around to feed the rest. Waking roughly
/// three times per arriving burst keeps the device fed without blocking the
/// link thread in a drain loop, which would add jitter to the pings that hold
/// the session open.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(20);

/// Bound on the end-of-transmission drain, so a wedged or unplugged dongle
/// ends a transmission late rather than hanging the link thread — and with it
/// `disconnect()` and `Drop` (the iax-239a rule: a dead device surfaces as an
/// error, never as a hang).
const FLUSH_DEADLINE: Duration = Duration::from_millis(500);

/// How long the drain sleeps between empty polls, rather than spinning.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// One voice frame's worth of wall clock. The output bus consumes frames at
/// exactly this rate, so this is the rate they must be handed over at.
const FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// Decoded frames to accumulate before releasing the first one.
///
/// DMR's wire cadence is bursty: three 20 ms frames arrive together every
/// **60 ms**, because one TDMA slot recurs once every 60 ms of wall clock and
/// carries 60 ms of speech when it does (`docs/design/dmr-wire.md`;
/// [`astar_codec::dmr::FRAMES_PER_BURST`] is the three, and
/// `astar_dmr::master::BURST_INTERVAL` is the 60 ms). Handing a burst straight
/// to the bus empties the vocoder in ~20 ms and then starves the bus for the
/// rest, which is heard as a beat. So the frames are released on the audio
/// clock instead, and this cushion absorbs the arrival jitter.
///
/// Two, not YSF's three: ~40 ms of cushion against a 60 ms burst is the same
/// order as YSF's three-of-five and NXDN's two-of-four, and it costs ~40 ms of
/// one-way latency instead of ~60.
const PRIME_FRAMES: usize = 2;

/// Silence after which a transmission is treated as over even though no
/// terminator burst and no new stream id ever arrived.
///
/// Twenty-five missed bursts at `astar_dmr::master::BURST_INTERVAL`. DMR has
/// no client-side watchdog constant to copy — `hblink3` reaps peers from its
/// own config file and `MMDVMHost` times out on the air, not on the network —
/// so this is NXDN's reflector-derived 1.5 s, which is long enough that
/// ordinary jitter never trips it and short enough that a UI is not left
/// claiming somebody is talking half a minute after they stopped.
const RX_WATCHDOG: Duration = Duration::from_millis(1_500);

/// What the control side can see of a link, without touching the thread.
///
/// `PartialEq` but not `Eq`: it carries a level in dBFS, and `f32` has no
/// total equality — the same pair [`crate::nxdn::NxdnSnapshot`] derives, for
/// the same reason.
#[derive(Debug, Clone, PartialEq)]
pub struct DmrSnapshot {
    /// The link state's ABI string — `idle`, `logging_in`, `authenticating`,
    /// `configuring`, `linked`, `closing`, `failed`. See
    /// [`LinkState::as_str`], which documents why these strings are not a
    /// debug convenience.
    pub link_state: &'static str,
    /// Why the link failed, when it did — `login`, `auth`, `config`,
    /// `session`, `closed`, `timeout`. `None` while it has not.
    ///
    /// "Your password is wrong" and "your ID is not allowed here" are the two
    /// things an operator needs told apart, and on the wire the only
    /// difference between them is which packet the `MSTNAK` answered. This is
    /// that distinction, and it carries nothing else — see this module's
    /// Secrets section.
    pub failure: Option<&'static str>,
    /// The most recent transmission's source id as its decimal string.
    ///
    /// DMR addresses stations by NUMBER: a `DMRD` carries `srcId` and no
    /// callsign at all. Turning a number into a callsign is a directory lookup
    /// against radioid.net, which astar does not hold here, and inventing one
    /// would be a guess presented as identification — so this is the number,
    /// and a caller that has a directory may resolve it.
    pub last_heard: Option<String>,
    /// That same id, unformatted, for a caller doing the lookup.
    pub last_heard_id: Option<u32>,
    /// The talkgroup this link joined.
    pub talkgroup: u32,
    /// The timeslot it joined it on, as [`Timeslot::as_str`]'s ABI string.
    pub timeslot: &'static str,
    /// `DMRD` bursts accepted for this room since the link came up. A liveness
    /// counter — a link that is up but silent and one that is receiving look
    /// the same from `link_state` alone.
    pub frames_rx: u64,
    /// Whether a transmission is in progress: set by a voice burst, cleared by
    /// the terminator, a new stream id, or [`RX_WATCHDOG`].
    pub receiving: bool,
    /// Which vocoder backend is decoding, or `None` for a link with no audio.
    /// An ABI string via [`AmbeBackend::as_str`].
    pub backend: Option<&'static str>,
    /// Whether this station is transmitting. **Always false**: the transmit
    /// path is Task 12 and the key is refused, not queued. See this module's
    /// Transmit section.
    pub ptt: bool,
    /// Transmit level in dBFS. CONSOLE-OWNED, exactly as
    /// [`crate::nxdn::NxdnSnapshot::tx_dbfs`] is: the link always fills this
    /// with the -60.0 floor and the session overwrites it with the number read
    /// once, at the one audio lane. A link owns no meters — see
    /// [`crate::voice_route`].
    pub tx_dbfs: f32,
    /// Receive level in dBFS, console-owned exactly as [`Self::tx_dbfs`] is.
    pub rx_dbfs: f32,
}

/// Everything needed to reach one DMR master.
///
/// Carries no devices: `ConsoleSession` opens the one audio lane
/// ([`crate::voice_route`]) and hands the link its channel ends.
///
/// `password` is the ONE secret in this struct and the only secret anywhere in
/// the DMR path. It is moved into the FSM at connect, used for one `RPTK`, and
/// dropped. It is not on [`DmrLink`], not in [`DmrSnapshot`], not in any
/// error, and this struct's `Debug` prints `password: <redacted>`.
pub struct DmrConfig {
    /// Which DMR this is, as the caller spelled it. A talkgroup number names
    /// nothing on its own — TG 91 exists on several of these networks and is a
    /// different room on each — so the network is part of the address, not a
    /// label.
    ///
    /// A **string**, not a [`DmrNetwork`], because a directory row's `system`
    /// names one server (`freedmr-network`, `ipsc2-poland`, `xlx696`) while
    /// `DmrNetwork` names one of nine families, and neither vocabulary can be
    /// derived from the other by renaming. This carries what the operator or
    /// the directory said; [`Self::family`] carries what the engine made of
    /// it.
    pub system: String,
    /// The family [`Self::system`] resolved to, or `None` for one this build
    /// does not recognise.
    ///
    /// `None` is "independent, unrecognised", never "refuse" — most directory
    /// rows answer it. Resolved by `Station::dmr_connect` through
    /// `DmrNetwork::from_slug` then `DmrNetwork::from_system_slug`, and read
    /// only for the consent gate and for the log line below.
    pub family: Option<DmrNetwork>,
    /// The master's hostname or address.
    pub host: String,
    /// Its port. 62031 is the homebrew convention; nothing here assumes it.
    pub port: u16,
    /// This station's DMR radio ID, registered at radioid.net. Zero and
    /// anything past 24 bits are refused at connect.
    pub radio_id: u32,
    /// The operator's callsign. It rides in the `RPTC` config and never on the
    /// air: DMR identifies a station by the number above.
    pub callsign: String,
    /// The talkgroup to listen to.
    pub talkgroup: u32,
    /// The timeslot to listen on. TS2 is the hotspot convention, and a
    /// convention is all it is.
    pub timeslot: Timeslot,
    /// The master's password. See this struct's own note and the module's
    /// Secrets section: it is moved into the FSM and never stored here.
    pub password: String,
}

impl std::fmt::Debug for DmrConfig {
    /// Hand written for one reason: the derived one would print the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmrConfig")
            .field("system", &self.system)
            .field("family", &self.family)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("radio_id", &self.radio_id)
            .field("callsign", &self.callsign)
            .field("talkgroup", &self.talkgroup)
            .field("timeslot", &self.timeslot)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A live link to one DMR master.
///
/// `Debug` reports the snapshot rather than the internals: the thread handle
/// and the socket are not something a caller can act on, and the link state
/// is. It also cannot leak a password, because there is none here to leak.
pub struct DmrLink {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// `Some` only for a link opened with audio.
    backend: Option<AmbeBackend>,
}

#[derive(Debug)]
struct Shared {
    /// `LinkState` as its discriminant index; see `state_index`.
    link_state: AtomicU32,
    /// `FailureStage` as its index, or [`NO_FAILURE`]; see `failure_index`.
    failure: AtomicU32,
    last_heard: Mutex<Option<String>>,
    /// The same id as a number, plus one sentinel: [`NO_TALKER`] for "nobody
    /// has transmitted". A 24-bit id cannot collide with it.
    last_heard_id: AtomicU32,
    /// The stream id of the transmission in progress. Constant for one
    /// key-down at the far end and random across them, so a change is an over
    /// ending even when its terminator was lost.
    stream_id: Mutex<Option<[u8; 4]>>,
    frames_rx: AtomicU64,
    receiving: AtomicBool,
    stop: AtomicBool,
    /// What the operator asked for. NOTHING in this module sets it except
    /// [`DmrLink::set_ptt`], which is the only path a key-down can take — and
    /// `apply_ptt` refuses it while transmit is gated.
    ptt_request: AtomicBool,
    /// What the run loop actually applied. False for the life of this module:
    /// see the Transmit section.
    ptt: AtomicBool,
    /// The room. Immutable for the life of the link: a talkgroup change is a
    /// new `RPTC` and, on some masters, a new session — so it is a reconnect,
    /// not a field to write.
    talkgroup: u32,
    timeslot: Timeslot,
}

/// Sentinel for [`Shared::last_heard_id`]: nobody has transmitted yet.
const NO_TALKER: u32 = u32::MAX;

/// Sentinel for [`Shared::failure`]: the link has not failed.
const NO_FAILURE: u32 = u32::MAX;

impl Shared {
    fn new(talkgroup: u32, timeslot: Timeslot) -> Shared {
        Shared {
            link_state: AtomicU32::new(state_index(LinkState::Idle)),
            failure: AtomicU32::new(NO_FAILURE),
            last_heard: Mutex::new(None),
            last_heard_id: AtomicU32::new(NO_TALKER),
            stream_id: Mutex::new(None),
            frames_rx: AtomicU64::new(0),
            receiving: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            ptt_request: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            talkgroup,
            timeslot,
        }
    }
}

/// Everything the run loop needs to turn burst bytes into sound. Owned by the
/// link thread; `None` for a link opened without audio.
struct Audio {
    ambe: Box<dyn AmbeStream>,
    /// The one audio lane `ConsoleSession` opened for this link
    /// ([`crate::voice_route`]): decoded frames go out on `rx_frames`,
    /// captured frames arrive on `tx_frames` while the console has the gate
    /// open. The link owns neither end's device.
    bus: CallAudio,
    /// Arrived-but-not-yet-submitted voice frames. A burst carries three and
    /// the vocoder accepts four, so a burst that arrives while the previous
    /// one is still in flight has nowhere else to go — without this queue two
    /// of every two back-to-back bursts would be dropped, evenly spread
    /// through every transmission.
    pending: VecDeque<ChannelFrame>,
    /// Captured frames waiting to be submitted to the encoder.
    ///
    /// The counterpart of `pending` on the encode side, and Task 12's to fill:
    /// `submit_encode` DROPS at the in-flight bound, so submitting a burst
    /// straight from the mic would throw away every frame past the fourth.
    /// Today the only thing that touches it is `run_ptt_step`, which empties
    /// it on every unkeyed pass along with the lane behind it.
    mic_pending: VecDeque<[i16; 160]>,
    /// Decoded frames waiting to be released to the bus on the audio clock.
    /// See [`PRIME_FRAMES`] for why they are not handed over as they finish.
    decoded: VecDeque<[i16; 160]>,
    /// When the next frame is due. `None` while re-priming — before the first
    /// frame of a transmission, and after the queue has run dry.
    next_release: Option<Instant>,
    /// Transmit state, `None` while unkeyed. Its presence IS "this station is
    /// transmitting", so there is no separate flag to keep in step.
    tx: Option<Tx>,
}

/// One transmission in progress.
///
/// Empty in this build, and constructed nowhere in it: `apply_ptt` refuses the
/// key (see the module's Transmit section), so the only thing that puts a `Tx`
/// in [`Audio::tx`] is the test that proves a received burst is not decoded
/// while one is there. Task 12 fills it with the stream id, the sequence
/// number and the burst counter a transmission needs, and deletes this note —
/// which is why the field is an `Option<Tx>` rather than the bool NXDN used:
/// half-duplex is expressed the same way here as it will be then.
#[allow(dead_code)]
struct Tx;

#[allow(dead_code)]
impl Tx {
    fn new() -> Tx {
        Tx
    }
}

/// `LinkState` has no numeric repr of its own — it is a protocol type and does
/// not need one — so the mapping lives here, next to the atomic it exists for,
/// rather than being pushed into the protocol crate.
fn state_index(s: LinkState) -> u32 {
    match s {
        LinkState::Idle => 0,
        LinkState::LoggingIn => 1,
        LinkState::Authenticating => 2,
        LinkState::Configuring => 3,
        LinkState::Linked => 4,
        LinkState::Closing => 5,
        LinkState::Failed => 6,
    }
}

/// The typed inverse of [`state_index`].
///
/// Exists so callers that must branch on the link state match an enum the
/// compiler can check rather than the ABI strings. A new `LinkState` variant
/// then breaks those call sites at compile time instead of silently falling
/// into a catch-all.
fn state_from_index(i: u32) -> LinkState {
    match i {
        1 => LinkState::LoggingIn,
        2 => LinkState::Authenticating,
        3 => LinkState::Configuring,
        4 => LinkState::Linked,
        5 => LinkState::Closing,
        6 => LinkState::Failed,
        _ => LinkState::Idle,
    }
}

fn state_str(i: u32) -> &'static str {
    state_from_index(i).as_str()
}

/// [`state_index`] for [`FailureStage`], with [`NO_FAILURE`] for "it has not".
fn failure_index(stage: FailureStage) -> u32 {
    match stage {
        FailureStage::Login => 0,
        FailureStage::Auth => 1,
        FailureStage::Config => 2,
        FailureStage::Session => 3,
        FailureStage::Closed => 4,
        FailureStage::Timeout => 5,
    }
}

/// The ABI string for a stored failure index, or `None` for [`NO_FAILURE`].
fn failure_str(i: u32) -> Option<&'static str> {
    let stage = match i {
        0 => FailureStage::Login,
        1 => FailureStage::Auth,
        2 => FailureStage::Config,
        3 => FailureStage::Session,
        4 => FailureStage::Closed,
        5 => FailureStage::Timeout,
        _ => return None,
    };
    Some(stage.as_str())
}

impl DmrLink {
    /// Resolve the master, bind a local socket, and run the homebrew
    /// handshake for `cfg.talkgroup` on `cfg.timeslot`.
    ///
    /// The link makes no sound: it holds the session open and reports who is
    /// transmitting. [`DmrLink::connect_with_audio`] is the one that decodes.
    ///
    /// Takes the config **by value** so the password is moved rather than
    /// borrowed; see the module's Secrets section.
    ///
    /// # Errors
    /// [`ConsoleError::Dmr`] if the radio id is zero or wider than 24 bits, if
    /// the callsign is empty, if the password is empty, if the host does not
    /// resolve, or if the socket cannot be bound.
    pub fn connect(cfg: DmrConfig) -> Result<DmrLink, ConsoleError> {
        Self::spawn(cfg, None, None)
    }

    /// Open a link that decodes the audio on it.
    ///
    /// `audio` is the one audio lane `ConsoleSession` already opened on the
    /// station's router ([`crate::voice_route`]): this link plays what it
    /// decodes onto `audio.rx_frames`. It builds no router, resolves no
    /// device, carries no preference and never touches the PTT gate.
    ///
    /// Opens a `ThumbDV` in [`VocoderMode::Dmr`] — DMR's own rate word, not
    /// D-Star's; see this module's docs — then plays every voice burst for the
    /// room this link joined.
    ///
    /// # Errors
    /// Everything [`DmrLink::connect`] can fail with, plus
    /// [`ConsoleError::Dmr`] when no `ThumbDV` is available (the message comes
    /// from `classify_thumbdv_failure`, so "unplugged" and "busy" are told
    /// apart).
    pub fn connect_with_audio(cfg: DmrConfig, audio: CallAudio) -> Result<DmrLink, ConsoleError> {
        // Hardware-only, exactly as D-Star, YSF and NXDN: the AMBE-3000 in a
        // `ThumbDV` is the whole vocoder story, so a missing dongle is an
        // error naming its reason rather than a link that quietly makes no
        // sound.
        let (ambe, backend) = open_ambe_stream(Some(AmbeBackend::Hardware), VocoderMode::Dmr)
            .ok_or_else(|| {
                ConsoleError::Dmr(astar_codec::ambe::classify_thumbdv_failure().message())
            })?;
        Self::connect_with_stream(cfg, audio, ambe, backend)
    }

    /// [`Self::connect_with_audio`] with the vocoder supplied by the caller.
    ///
    /// Two callers want this: the `astar-station` facade, so the `ThumbDV`
    /// probe runs OUTSIDE its session mutex, and the tests, where a fake
    /// [`AmbeStream`] decodes without a dongle so the whole burst-to-speaker
    /// path is provable against the loopback master on `127.0.0.1`.
    ///
    /// # Errors
    /// As [`Self::connect_with_audio`], minus the `ThumbDV` probe.
    pub fn connect_with_stream(
        cfg: DmrConfig,
        audio: CallAudio,
        ambe: Box<dyn AmbeStream>,
        backend: AmbeBackend,
    ) -> Result<DmrLink, ConsoleError> {
        let audio = Audio {
            ambe,
            bus: audio,
            pending: VecDeque::new(),
            mic_pending: VecDeque::new(),
            decoded: VecDeque::new(),
            next_release: None,
            tx: None,
        };
        Self::spawn(cfg, Some(audio), Some(backend))
    }

    /// The body all three constructors share: validate, resolve, bind, spawn.
    ///
    /// The validation order is the order an operator meets the fields in, so
    /// the first thing they are told about is the first thing they can fix.
    /// None of these messages interpolates the password.
    fn spawn(
        cfg: DmrConfig,
        audio: Option<Audio>,
        backend: Option<AmbeBackend>,
    ) -> Result<DmrLink, ConsoleError> {
        // A radio ID is a registration, not a default. Zero is what an unset
        // field looks like, and putting it on a DMR network would claim
        // somebody else's number — or nobody's, which a master answers with
        // MSTNAK and an operator reads as "astar is broken".
        let id = wire::RadioId::new(cfg.radio_id).map_err(|e| match e {
            wire::RadioIdError::Zero => ConsoleError::Dmr(
                "radio id must be set: DMR addresses stations by number, and 0 is not a \
                 registration"
                    .to_string(),
            ),
            wire::RadioIdError::TooLarge => {
                ConsoleError::Dmr("radio id is too large: a DMR source id is 24 bits".to_string())
            }
        })?;

        if cfg.callsign.trim().is_empty() {
            return Err(ConsoleError::Dmr("callsign must be set".to_string()));
        }

        // Every master in the directory lists `requires: ["dmr_id",
        // "password"]`. An empty one produces a valid-looking digest of the
        // salt alone and a MSTNAK the operator cannot explain.
        if cfg.password.is_empty() {
            return Err(ConsoleError::Dmr(
                "a password is required: every DMR master astar can reach asks for one".to_string(),
            ));
        }

        let addr: SocketAddr = (cfg.host.as_str(), cfg.port)
            .to_socket_addrs()
            .map_err(|e| ConsoleError::Dmr(format!("resolve {}: {e}", cfg.host)))?
            .next()
            .ok_or_else(|| ConsoleError::Dmr(format!("resolve {}: no addresses", cfg.host)))?;

        // Bind to the unspecified address on an ephemeral port, matching the
        // family of whatever we resolved to.
        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 bind")
        } else {
            "[::]:0".parse().expect("valid v6 bind")
        };
        let socket = UdpSocket::bind(bind).map_err(|e| ConsoleError::Dmr(format!("bind: {e}")))?;
        let timeout = if audio.is_some() {
            AUDIO_RECV_TIMEOUT
        } else {
            RECV_TIMEOUT
        };
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|e| ConsoleError::Dmr(format!("socket timeout: {e}")))?;

        let shared = Arc::new(Shared::new(cfg.talkgroup, cfg.timeslot));

        tracing::debug!(
            system = cfg.system.as_str(),
            family = cfg.family.map_or("unrecognised", DmrNetwork::slug),
            talkgroup = cfg.talkgroup,
            timeslot = cfg.timeslot.as_str(),
            "dmr: linking"
        );

        // The password is moved into the FSM here and read nowhere else. What
        // is left of `cfg` after this line no longer holds one.
        let fsm = DmrFsm::new(
            id,
            cfg.talkgroup,
            cfg.timeslot,
            wire::ConfigFields::softclient(
                cfg.callsign.trim(),
                concat!("astar ", env!("CARGO_PKG_VERSION")),
            ),
            cfg.password,
        );

        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("astar-dmr-link".into())
                .spawn(move || run(&socket, addr, fsm, &shared, audio))
                .map_err(|e| ConsoleError::Dmr(format!("thread: {e}")))?
        };

        Ok(DmrLink {
            shared,
            thread: Some(thread),
            backend,
        })
    }

    /// Current link state, cheap enough to poll.
    #[must_use]
    pub fn snapshot(&self) -> DmrSnapshot {
        let id = self.shared.last_heard_id.load(Ordering::Relaxed);
        // Acquire, paired with the Release store in `handle`'s failure arm:
        // a reader that sees `failed` must also see the stage that says why.
        let state = self.shared.link_state.load(Ordering::Acquire);
        DmrSnapshot {
            link_state: state_str(state),
            failure: failure_str(self.shared.failure.load(Ordering::Relaxed)),
            last_heard: self.shared.last_heard.lock().map_or(None, |g| g.clone()),
            last_heard_id: (id != NO_TALKER).then_some(id),
            talkgroup: self.shared.talkgroup,
            timeslot: self.shared.timeslot.as_str(),
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
    /// Stores a request; the run loop applies the edge on its next pass — and
    /// **while transmit is gated it applies none**, so a key-down here is
    /// refused rather than queued and [`DmrSnapshot::ptt`] never becomes true.
    /// See this module's Transmit section.
    ///
    /// This is still the ONLY path that can set the request true, which is
    /// what makes "nothing transmits unless the operator asked" checkable
    /// rather than asserted, and what Task 12 inherits.
    pub fn set_ptt(&self, on: bool) {
        self.shared.ptt_request.store(on, Ordering::Relaxed);
    }

    /// The link state as the protocol crate's own enum.
    ///
    /// [`DmrSnapshot::link_state`] carries the ABI string for anything
    /// crossing a boundary; this is for in-process callers that need to
    /// branch, so they get an exhaustive match instead of string comparison.
    #[must_use]
    pub fn link_state(&self) -> LinkState {
        state_from_index(self.shared.link_state.load(Ordering::Relaxed))
    }

    /// Send the `RPTCL` and stop the thread. Consumes the link.
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

impl std::fmt::Debug for DmrLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmrLink")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl Drop for DmrLink {
    /// Dropping a link closes it. A master would otherwise keep the peer for
    /// the full minute of its own timeout, relaying audio at a socket nobody
    /// is reading — and, on the masters that allow one session per ID, refuse
    /// the operator's next connection as a duplicate.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The link thread's body. The socket is owned by the spawned closure for the
/// thread's lifetime and borrowed here.
fn run(
    socket: &UdpSocket,
    addr: SocketAddr,
    mut fsm: DmrFsm,
    shared: &Arc<Shared>,
    mut audio: Option<Audio>,
) {
    let publish = |fsm: &DmrFsm| {
        shared
            .link_state
            .store(state_index(fsm.state()), Ordering::Relaxed);
    };

    for datagram in fsm.connect(Instant::now()) {
        if socket.send_to(&datagram, addr).is_err() {
            fail(shared, send_failure_stage());
            return;
        }
    }
    publish(&fsm);

    // When the last burst arrived, for the watchdog that closes a transmission
    // nobody ever ended.
    let mut last_data: Option<Instant> = None;

    let mut buf = [0_u8; astar_dmr::master::MAX_DATAGRAM];
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
            // A datagram from somewhere else on our ephemeral port. Ignore it
            // rather than feeding the FSM a stranger's bytes.
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
            // before anything else on this pass can block. See `run_ptt_step`,
            // where the ordering is the whole point.
            run_ptt_step(a, shared, socket, addr);
            // Keep the vocoder fed and the speaker supplied between arrivals:
            // three frames arrive at once and only four fit in the pipeline,
            // so the rest are submitted here.
            pump(a);
        }
    }

    if state_from_index(shared.link_state.load(Ordering::Acquire)) == LinkState::Failed {
        // A failed link has nothing to close — the master refused it, dropped
        // it, or the socket stopped taking bytes — and `failed` plus its stage
        // is exactly what the operator needs to keep seeing. Resetting the
        // state to `idle` here would erase the diagnosis a moment after it
        // appeared. This reads what was PUBLISHED rather than `fsm.state()`,
        // because a send that could not leave the host is a failure the FSM
        // never hears about: it hands over the bytes and this loop is the only
        // thing that learns they did not go.
        return;
    }

    // Best effort: the master drops us on its own timeout anyway, and a failed
    // send here is not worth reporting to a caller that has gone.
    for datagram in fsm.close(Instant::now()) {
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
        FsmAction::Send(bytes) => {
            if socket.send_to(bytes, addr).is_ok() {
                return true;
            }
            // The socket refused the datagram: the interface went away, the
            // route did, or the address family stopped matching. Breaking the
            // loop without saying so would leave the link reporting `idle`
            // with no failure at all — indistinguishable from an operator
            // who disconnected on purpose, which is the one reading that
            // stops anybody looking for the cause.
            fail(shared, send_failure_stage());
            false
        }
        FsmAction::Data(packet) => {
            on_data(packet, shared, audio);
            true
        }
        FsmAction::Failed(stage) => {
            fail(shared, *stage);
            false
        }
    }
}

/// Which [`FailureStage`] a datagram this host could not send reports.
///
/// Pure, and its own function, because it is the one failure the FSM never
/// sees: `FsmAction::Send` hands over bytes and the run loop is the only thing
/// that learns they did not go, so nothing in `astar-dmr` can classify it.
///
/// [`FailureStage::Session`] is the honest one of the six. The other five each
/// name a thing the MASTER said — a NAK at a step of the chain, an `MSTCL`, or
/// silence past the timeout — and the master said nothing here. `Session` is
/// the stage that means "the session ended while it was running", which is
/// exactly what a socket that stops taking bytes does to it, and it renders as
/// `session`: an operator sees a link that ended rather than one that was
/// refused, which is the true distinction.
///
/// A stage of its own — `local`, say — would be a better word for it, but it
/// would be a new variant in `astar-dmr`'s ABI enum, and this build does not
/// need one badly enough to add it here. That is a note for a later task, not
/// a silent edit to the protocol crate.
fn send_failure_stage() -> FailureStage {
    FailureStage::Session
}

/// Publish a terminal failure: the stage first, then the state.
///
/// The ordering is the invariant. The `Release` store on `link_state` pairs
/// with the `Acquire` load in [`DmrLink::snapshot`], so a reader that sees
/// `failed` also sees the stage that says why — never `failed` with no
/// diagnosis.
fn fail(shared: &Arc<Shared>, stage: FailureStage) {
    shared.receiving.store(false, Ordering::Relaxed);
    shared
        .failure
        .store(failure_index(stage), Ordering::Relaxed);
    shared
        .link_state
        .store(state_index(LinkState::Failed), Ordering::Release);
}

/// One `DMRD` for this link's room: the talker, the transmission's edges, and
/// — for a link with audio that is not transmitting — its voice.
fn on_data(packet: &wire::DataPacket, shared: &Arc<Shared>, audio: Option<&mut Audio>) {
    if !for_this_room(packet, shared) {
        return;
    }
    shared.frames_rx.fetch_add(1, Ordering::Relaxed);

    // A stream id is constant for one key-down at the far end and random
    // across them, so a change is the previous over ending. Terminators get
    // lost; without this a dropped one would wedge "receiving" until the
    // watchdog fired, and would run two talkers' audio together.
    let new_stream = shared.stream_id.lock().is_ok_and(|mut slot| {
        let changed = *slot != Some(packet.stream_id);
        *slot = Some(packet.stream_id);
        changed
    });

    // `srcId` is bytes 5..8 of the datagram, in clear — no vocoder involved,
    // so who is talking stays truthful even while this station transmits.
    shared.last_heard_id.store(packet.src_id, Ordering::Relaxed);
    if let Ok(mut slot) = shared.last_heard.lock() {
        *slot = Some(packet.src_id.to_string());
    }

    let end = matches!(
        packet.frame_type,
        wire::FrameType::DataSync {
            data_type: DT_TERMINATOR_WITH_LC
        }
    );
    match packet.frame_type {
        wire::FrameType::Voice { .. } | wire::FrameType::VoiceSync => {
            shared.receiving.store(true, Ordering::Relaxed);
        }
        _ if end => shared.receiving.store(false, Ordering::Relaxed),
        // A voice-LC header, a CSBK, an idle burst: none of them says the
        // transmission ended, and none of them says it started either. The
        // voice bursts that follow, 60 ms apart, do.
        _ => {}
    }

    let Some(a) = audio else {
        return;
    };
    // HALF-DUPLEX. The ThumbDV is one physical link with one AMBE-3000 behind
    // it, and no session here decodes and encodes at the same time:
    // interleaving the two directions on one chip is how both come out wrong.
    // Only the voice is refused; the header above is still read.
    if a.tx.is_some() {
        return;
    }
    if new_stream {
        // Whatever is still in the pipeline belongs to the previous talker.
        // Play it before this one's first frame lands on top of it.
        flush(a);
    }
    match packet.frame_type {
        wire::FrameType::Voice { .. } | wire::FrameType::VoiceSync => {
            decode_burst(&packet.burst, a, shared);
        }
        // The tail of a transmission is still working through the pipeline
        // when its terminator arrives. Play it, or every over loses its final
        // ~60 ms — and one shorter than the pipeline depth would be silent.
        _ if end => flush(a),
        // A data-sync burst that is not the terminator is a BPTC-coded link
        // control, a CSBK or an idle: 33 bytes that are not audio. Decoding
        // them would emit a burst of noise at the start of every
        // transmission.
        _ => {}
    }
}

/// Whether this `DMRD` belongs to the room this link joined.
///
/// The FSM asks the same question and this asks it again, one layer up,
/// because the answer guards different things: the FSM decides what to hand
/// over at all, and this decides what reaches the talker display and the one
/// audio lane. DMR needs both halves of the address — TDMA gives a master two
/// timeslots and a talkgroup number means nothing without one.
///
/// A private call on the other timeslot is dropped here even though the FSM
/// accepts it on either: this link is listening in one slot, and audio from
/// the other one arriving mid-over would be two talkers through one vocoder.
fn for_this_room(packet: &wire::DataPacket, shared: &Arc<Shared>) -> bool {
    if packet.slot != shared.timeslot {
        return false;
    }
    match packet.call_type {
        CallType::Group => packet.dst_id == shared.talkgroup,
        // The FSM already checked that a private call is addressed to this
        // radio's own ID; there is no second question to ask about it here.
        CallType::Private => true,
    }
}

/// Close a transmission that stopped without saying so.
///
/// A client that is unplugged mid-over sends no terminator and no next stream,
/// and without this "receiving" would latch on until somebody else transmitted
/// — a UI lying about the air. Clearing `last_data` is what makes it fire once
/// rather than on every pass of a quiet link.
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
    tracing::debug!("dmr: no burst for {RX_WATCHDOG:?}, treating the transmission as over");
    if let Some(a) = audio
        && a.tx.is_none()
    {
        flush(a);
    }
}

/// Turn one received voice burst into speech on the output bus.
///
/// **There is no error case, and that asymmetry with NXDN is deliberate.** A
/// burst always carries 216 information bits and
/// [`astar_codec::dmr::unpack_voice`] only rearranges them; whether they are
/// *voice* was decided by the `DMRD` bits byte before this function was
/// called, one layer up, where a signalling burst is routed somewhere else
/// entirely. NXDN's decoder has to read the LICH to find that out and can
/// therefore refuse; this one cannot be handed anything to refuse.
///
/// `shared` is here for the log line: a station may hold more than one link,
/// and "three voice frames" is only useful when it says which room they came
/// from.
fn decode_burst(burst: &[u8; wire::BURST_LEN], audio: &mut Audio, shared: &Arc<Shared>) {
    tracing::trace!(
        talkgroup = shared.talkgroup,
        timeslot = shared.timeslot.as_str(),
        "dmr: a voice burst"
    );
    // `ChannelFrame::Dmr`, built by the codec and never converted: `.into()`
    // on nine bytes yields a D-Star frame, and a D-Star frame handed to a chip
    // configured for DMR is confident noise rather than an error.
    audio.pending.extend(unpack_voice(burst));
    pump(audio);
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
        audio.ambe.submit_decode(frame);
    }
    // Decoded frames go to a queue, NOT straight to the bus: the vocoder
    // finishes a whole burst's worth in a few milliseconds and the bus wants
    // them spread over 60. See `release`.
    while let Some(pcm) = audio.ambe.poll_decoded() {
        audio.decoded.push_back(pcm);
    }
    release(audio, Instant::now());
}

/// One run-loop pass's transmit step: apply the pending PTT edge, then — on a
/// pass that is NOT transmitting — drain and DROP whatever the capture lane
/// has queued.
///
/// The two are ONE function because the ordering between them is the whole
/// invariant, not a detail of layout. The gate belongs to
/// `ConsoleSession::set_ptt`, which opens it up to a poll interval before this
/// loop observes the request, and it can stay open while this loop is not
/// keyed — so without the drop the lane would pile audio into `tx_frames`
/// unbounded and the next transmission would open with somebody else's stale
/// speech.
///
/// What keeps the drop from eating the VOX pre-roll is that it runs on the
/// SAME pass that read `ptt_request`, with nothing in between: an operator
/// keying after this call does so against a pass that will never drain again.
/// Put it after the socket read instead and the 20 ms [`AUDIO_RECV_TIMEOUT`]
/// opens a window in which the gate opens, the lane flushes its look-back
/// ring, and this arm throws away precisely the audio it exists to protect.
/// D-Star shipped that bug once; YSF, NXDN and DMR do not repeat it.
///
/// While transmit is gated the drop runs on every pass, since no pass is ever
/// keyed. The ordering is kept anyway, because Task 12 adds the key-DOWN edge
/// back into `apply_ptt` and not into this function.
///
/// `_socket` and `_addr` are the wire Task 12 will put a burst on. They are
/// unread today on purpose, and the tests hand them a real socket and assert
/// that nothing arrives at the other end of it — a refusal that is checked
/// rather than described.
fn run_ptt_step(audio: &mut Audio, shared: &Arc<Shared>, _socket: &UdpSocket, _addr: SocketAddr) {
    apply_ptt(audio, shared);
    if audio.tx.is_none() {
        while audio.bus.tx_frames.try_recv().is_ok() {}
        audio.mic_pending.clear();
    }
}

/// Start or stop a transmission, if the request differs from what is applied —
/// except that, today, it can only refuse.
fn apply_ptt(audio: &mut Audio, shared: &Arc<Shared>) {
    let want = shared.ptt_request.load(Ordering::Relaxed);
    let keyed = audio.tx.is_some();
    if want == keyed {
        return;
    }
    if want {
        // RECEIVE-FIRST GATE. The transmit path is Task 12 of
        // docs/superpowers/plans/2026-09-07-dmr-network.md and does not
        // exist yet. A key that silently did nothing would be worse than one
        // that is refused: the operator would hear nothing happen and
        // reasonably assume the radio or the link was broken. So the request
        // is dropped here, `ptt` never becomes true, and the app shows a
        // receive-only indicator instead of a PTT button.
        //
        // It also matters more here than it did for YSF or NXDN: a DMR
        // master routes by the radio ID astar logged in with, so a half-built
        // transmit path would put malformed bursts on a network under the
        // operator's own registration.
        shared.ptt_request.store(false, Ordering::Relaxed);
        tracing::warn!("dmr: transmit is not implemented; the key was refused, not queued");
        return;
    }
    // Unreachable while the gate above holds — nothing can set `tx` — and
    // written out rather than assumed, so that Task 12 replaces a branch
    // instead of adding one.
    shared.ptt.store(false, Ordering::Relaxed);
    audio.tx = None;
}

/// Hand decoded frames to the output bus on the audio clock — one per
/// [`FRAME_INTERVAL`] — rather than as fast as the vocoder produces them.
///
/// The bus consumes at exactly 50 frames a second. Handing it three at once
/// and then nothing for the rest of the 60 ms makes it starve in the hole,
/// which is audible as a beat; this is what turns a bursty wire cadence back
/// into a steady one.
///
/// Running dry re-primes rather than free-running: a queue that has emptied
/// means the cushion was too small for the jitter actually seen, and releasing
/// the next frame the instant it arrives would just reopen the same hole.
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

/// Empty the decode path — queued frames and everything still in flight — and
/// play all of it.
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
                "dmr: vocoder flush hit its {FLUSH_DEADLINE:?} deadline, abandoning the rest"
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
/// `next_release` makes the NEXT transmission prime again from scratch instead
/// of inheriting a stale clock.
fn drain_tail(audio: &mut Audio) {
    while let Some(pcm) = audio.decoded.pop_front() {
        let _ = audio.bus.rx_frames.send(pcm.to_vec());
    }
    audio.next_release = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_codec::dmr::{FRAMES_PER_BURST, pack_voice};
    use astar_dmr::Master;
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// The talkgroup every test here joins. Any number does; this one is the
    /// reference implementations' own example.
    const TG: u32 = 31_313;
    /// A radio id for the station under test. Twenty-four bits, as a `DMRD`
    /// source id is.
    const RADIO_ID: u32 = 3_153_591;
    /// The loopback master's password, and the one this suite checks is never
    /// anywhere it should not be.
    const PASSWORD: &str = "passw0rd";

    /// Everything here binds `127.0.0.1` and talks to a master this test
    /// started. Nothing reaches a real network — CLAUDE.md's on-air safety
    /// rule, which this crate is squarely inside, and which DMR sharpens: a
    /// real master authenticates by a registered radio ID.
    fn loopback() -> (astar_dmr::MasterHandle, SocketAddr) {
        let m = Master::bind("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind master");
        let addr = m.local_addr();
        (m.run(), addr)
    }

    fn cfg(addr: SocketAddr) -> DmrConfig {
        DmrConfig {
            system: "tgif".into(),
            family: Some(DmrNetwork::Tgif),
            host: addr.ip().to_string(),
            port: addr.port(),
            radio_id: RADIO_ID,
            callsign: "KC0ABC".into(),
            talkgroup: TG,
            timeslot: Timeslot::Ts2,
            password: PASSWORD.into(),
        }
    }

    fn wait_for(link: &DmrLink, want: &str, within: Duration) -> DmrSnapshot {
        let deadline = Instant::now() + within;
        loop {
            let snap = link.snapshot();
            if snap.link_state == want || Instant::now() > deadline {
                return snap;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// What the vocoder was handed, in order — shared, because the `Audio` it
    /// goes into owns it as a `Box<dyn AmbeStream>` and a trait object cannot
    /// be looked back inside.
    type SubmitLog = Arc<Mutex<Vec<ChannelFrame>>>;

    /// A vocoder that answers immediately and remembers what it was asked.
    ///
    /// Each frame decodes to 160 samples of its own first byte, so decoded
    /// audio can be attributed to the exact frame that produced it.
    struct FakeVocoder {
        submitted: SubmitLog,
        ready: VecDeque<[i16; 160]>,
        encoded: VecDeque<[u8; 9]>,
    }

    impl FakeVocoder {
        fn new() -> FakeVocoder {
            FakeVocoder {
                submitted: SubmitLog::default(),
                ready: VecDeque::new(),
                encoded: VecDeque::new(),
            }
        }
    }

    impl AmbeStream for FakeVocoder {
        fn submit_decode(&mut self, frame: ChannelFrame) {
            let value = i16::from(frame.as_slice()[0]);
            if let Ok(mut log) = self.submitted.lock() {
                log.push(frame);
            }
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
            self.encoded.push_back([tag, 0, 0, 0, 0, 0, 0, 0, 0]);
        }
        fn poll_encoded(&mut self) -> Option<ChannelFrame> {
            self.encoded.pop_front().map(ChannelFrame::Dmr)
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
                mic_pending: VecDeque::new(),
                decoded: VecDeque::new(),
                next_release: None,
                tx: None,
            },
            rx_rx,
            tx_tx,
        )
    }

    /// [`test_audio`] with the vocoder's submit log kept, for the test that
    /// asks what the chip was actually handed.
    fn test_audio_with_log() -> (Audio, Receiver<Vec<i16>>, SubmitLog) {
        let (mut audio, rx) = test_audio();
        let vocoder = FakeVocoder::new();
        let log = Arc::clone(&vocoder.submitted);
        audio.ambe = Box::new(vocoder);
        (audio, rx, log)
    }

    /// A bound socket plus a peer to read what was sent to it.
    fn udp_pair() -> (UdpSocket, SocketAddr, UdpSocket) {
        let peer = UdpSocket::bind("127.0.0.1:0").expect("bind peer");
        let addr = peer.local_addr().expect("addr");
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind sock");
        (sock, addr, peer)
    }

    /// One voice burst carrying three distinguishable AMBE frames, built with
    /// the codec's own packer so the test reads the same layout the decoder
    /// does.
    fn voice_burst() -> [u8; astar_dmr::frame::BURST_LEN] {
        voice_burst_from(1)
    }

    /// [`voice_burst`] whose three frames are `first`, `first + 1`,
    /// `first + 2` — so audio from one transmission can be told from another's
    /// after both have been through the same vocoder.
    fn voice_burst_from(first: u8) -> [u8; astar_dmr::frame::BURST_LEN] {
        let frames: [[u8; 9]; 3] =
            core::array::from_fn(|i| [first.saturating_add(u8::try_from(i).expect("small")); 9]);
        let mut burst = pack_voice(&frames);
        astar_dmr::frame::write_sync(&mut burst, astar_dmr::frame::Sync::MsAudio);
        burst
    }

    fn voice_packet(seq: u8, n: u8) -> wire::DataPacket {
        wire::DataPacket {
            seq,
            src_id: 4242,
            dst_id: TG,
            peer_id: 4242,
            slot: Timeslot::Ts2,
            call_type: CallType::Group,
            frame_type: wire::FrameType::Voice { n },
            stream_id: [1, 2, 3, 4],
            burst: voice_burst(),
            ber: 0,
            rssi: 0,
        }
    }

    fn terminator_packet() -> wire::DataPacket {
        let mut p = voice_packet(9, 0);
        p.frame_type = wire::FrameType::DataSync {
            data_type: astar_dmr::frame::DT_TERMINATOR_WITH_LC,
        };
        p.burst = [0u8; astar_dmr::frame::BURST_LEN];
        p
    }

    #[test]
    fn linking_to_the_loopback_master_reaches_linked() {
        let (master, addr) = loopback();
        let link = DmrLink::connect(cfg(addr)).expect("connect");
        assert_eq!(
            wait_for(&link, "linked", Duration::from_secs(3)).link_state,
            "linked"
        );
        link.disconnect();
        master.shutdown();
    }

    #[test]
    fn a_wrong_password_fails_the_link_at_the_auth_stage() {
        // "Your password is wrong" and "your ID is not allowed here" are the
        // two things an operator needs told apart, and this is the layer that
        // can still tell them apart.
        let (master, addr) = loopback();
        let mut bad = cfg(addr);
        bad.password = "not-the-password".into();
        let link = DmrLink::connect(bad).expect("connect starts regardless");
        let snap = wait_for(&link, "failed", Duration::from_secs(3));
        assert_eq!(snap.link_state, "failed");
        assert_eq!(snap.failure, Some("auth"));
        link.disconnect();
        master.shutdown();
    }

    #[test]
    fn a_zero_radio_id_is_refused_at_connect() {
        // A radio ID is a registration, not a default. Zero is what an unset
        // field looks like, and putting it on a DMR network would claim
        // somebody else's number — or nobody's, which a master answers with
        // MSTNAK and an operator reads as "astar is broken".
        let mut c = cfg("127.0.0.1:62031".parse().expect("v4"));
        c.radio_id = 0;
        assert!(matches!(DmrLink::connect(c), Err(ConsoleError::Dmr(_))));
    }

    #[test]
    fn a_radio_id_past_twenty_four_bits_is_refused_at_connect() {
        // A `DMRD` source id is three bytes. A wider number cannot go on the
        // wire at all, and narrowing it silently would transmit under
        // somebody else's registration.
        let mut c = cfg("127.0.0.1:62031".parse().expect("v4"));
        c.radio_id = 0x0100_0000;
        assert!(matches!(DmrLink::connect(c), Err(ConsoleError::Dmr(_))));
    }

    #[test]
    fn an_empty_password_is_refused_at_connect() {
        // Every master in the directory lists `requires: ["dmr_id",
        // "password"]`. An empty one produces a valid-looking digest of the
        // salt alone and a MSTNAK the operator cannot explain.
        let mut c = cfg("127.0.0.1:62031".parse().expect("v4"));
        c.password = String::new();
        assert!(matches!(DmrLink::connect(c), Err(ConsoleError::Dmr(_))));
    }

    #[test]
    fn an_empty_callsign_is_refused_at_connect() {
        let mut c = cfg("127.0.0.1:62031".parse().expect("v4"));
        c.callsign = String::new();
        assert!(matches!(DmrLink::connect(c), Err(ConsoleError::Dmr(_))));
    }

    #[test]
    fn the_password_never_reaches_a_snapshot_an_error_or_debug_output() {
        // CLAUDE.md, verbatim: secrets are connect-time in-args ONLY —
        // never stored on a Station, never in snapshots, events, errors or
        // logs. This is the test that makes that a property of the code
        // rather than a promise in a comment.
        let (master, addr) = loopback();
        let c = cfg(addr);
        let rendered_config = format!("{c:?}");
        assert!(
            !rendered_config.contains(PASSWORD),
            "DmrConfig's Debug leaked it"
        );
        let link = DmrLink::connect(c).expect("connect");
        wait_for(&link, "linked", Duration::from_secs(3));
        let snap = link.snapshot();
        assert!(!format!("{snap:?}").contains(PASSWORD));
        assert!(!format!("{link:?}").contains(PASSWORD));

        let mut bad = cfg(addr);
        bad.radio_id = 0;
        let Err(e) = DmrLink::connect(bad) else {
            panic!("a zero id is refused")
        };
        assert!(!e.to_string().contains(PASSWORD));
        link.disconnect();
        master.shutdown();
    }

    #[test]
    fn a_voice_burst_becomes_three_frames_of_speech() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        decode_burst(&voice_burst(), &mut audio, &shared);
        flush(&mut audio);
        let played: Vec<Vec<i16>> = rx.try_iter().collect();
        assert_eq!(played.len(), 3);
        assert_eq!(played[0][0], 1);
        assert_eq!(played[2][0], 3);
    }

    #[test]
    fn a_signalling_burst_is_not_decoded_as_voice() {
        // The DMRD bits byte says data-sync; the 33 bytes behind it are a
        // BPTC-coded link control, not audio. Decoding them would emit a
        // burst of noise at the start and end of every transmission.
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(terminator_packet())),
            &sock,
            addr,
            &shared,
            Some(&mut audio),
        );
        flush(&mut audio);
        assert_eq!(rx.try_iter().count(), 0);
    }

    #[test]
    fn the_talker_is_read_from_the_dmrd_header_and_needs_no_vocoder() {
        // DMRD carries src_id in bytes 5..8 in clear. A link with no audio
        // still answers "who is on this talkgroup", and so does a link that
        // is transmitting.
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(voice_packet(0, 0))),
            &sock,
            addr,
            &shared,
            None,
        );
        assert_eq!(shared.last_heard_id.load(Ordering::Relaxed), 4242);
        assert!(shared.receiving.load(Ordering::Relaxed));
    }

    #[test]
    fn a_terminator_clears_receiving() {
        // MMDVMHost/DMRDefines.h: DT_TERMINATOR_WITH_LC = 0x02, carried in
        // the bits byte's low nibble with the data-sync bits set. Without
        // this the UI shows a talker until the next transmission.
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(voice_packet(0, 0))),
            &sock,
            addr,
            &shared,
            None,
        );
        handle(
            &FsmAction::Data(Box::new(terminator_packet())),
            &sock,
            addr,
            &shared,
            None,
        );
        assert!(!shared.receiving.load(Ordering::Relaxed));
        assert_eq!(
            shared.last_heard_id.load(Ordering::Relaxed),
            4242,
            "the talker is remembered"
        );
    }

    #[test]
    fn a_voice_lc_header_is_neither_a_start_nor_an_end() {
        // The terminator is ONE data type of several, and `end` is a value
        // comparison against DT_TERMINATOR_WITH_LC rather than "any data-sync
        // burst". A voice-LC header opening a stream must not read as its
        // end — that would clear `receiving` on the first burst of every
        // over — and must not be decoded as voice either.
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(voice_packet(0, 0))),
            &sock,
            addr,
            &shared,
            None,
        );
        let mut header = voice_packet(1, 0);
        header.frame_type = wire::FrameType::DataSync {
            data_type: astar_dmr::frame::DT_VOICE_LC_HEADER,
        };
        handle(
            &FsmAction::Data(Box::new(header)),
            &sock,
            addr,
            &shared,
            Some(&mut audio),
        );
        assert!(
            shared.receiving.load(Ordering::Relaxed),
            "a header does not end the transmission a voice burst started"
        );
        flush(&mut audio);
        assert_eq!(rx.try_iter().count(), 0, "and it is not audio");
    }

    #[test]
    fn a_new_stream_id_starts_a_new_transmission_even_without_a_terminator() {
        // Terminators get lost. The stream id is constant for one PTT and
        // random across them, so a change is the other end of an over —
        // and without this a dropped terminator wedges "receiving" forever.
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(voice_packet(0, 0))),
            &sock,
            addr,
            &shared,
            None,
        );
        let mut next = voice_packet(0, 0);
        next.src_id = 5150;
        next.stream_id = [9, 9, 9, 9];
        handle(&FsmAction::Data(Box::new(next)), &sock, addr, &shared, None);
        assert_eq!(shared.last_heard_id.load(Ordering::Relaxed), 5150);
    }

    #[test]
    fn a_new_stream_id_plays_the_previous_talker_out_before_the_new_one_starts() {
        // The audio half of the stream-id change. The previous over's frames
        // are still in the pipeline, paced 20 ms apart, when the next talker's
        // first burst arrives — so they have to be played out FIRST. Without
        // the flush they sit behind a release clock that belongs to a
        // transmission that has ended, and the new talker's speech is heard
        // before the old talker's last words.
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();

        handle(
            &FsmAction::Data(Box::new(voice_packet(0, 0))),
            &sock,
            addr,
            &shared,
            Some(&mut audio),
        );
        let first: Vec<i16> = rx.try_iter().map(|f| f[0]).collect();
        assert_eq!(first, vec![1], "paced: only the primed frame so far");

        let mut next = voice_packet(0, 0);
        next.src_id = 5150;
        next.stream_id = [9, 9, 9, 9];
        next.burst = voice_burst_from(7);
        handle(
            &FsmAction::Data(Box::new(next)),
            &sock,
            addr,
            &shared,
            Some(&mut audio),
        );
        let after: Vec<i16> = rx.try_iter().map(|f| f[0]).collect();
        assert_eq!(
            after,
            vec![2, 3, 7],
            "the previous stream's tail, then the new stream's first frame"
        );
    }

    #[test]
    fn a_burst_on_the_other_timeslot_is_ignored() {
        // TDMA gives a master two rooms on one wire, and this link is in one
        // of them. A burst from the other slot reaching the talker display
        // would name somebody who is not in the room the operator joined, and
        // reaching the audio lane would run two talkers through one vocoder.
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        let mut other = voice_packet(0, 0);
        other.slot = Timeslot::Ts1;
        handle(
            &FsmAction::Data(Box::new(other)),
            &sock,
            addr,
            &shared,
            None,
        );
        assert_eq!(shared.last_heard_id.load(Ordering::Relaxed), NO_TALKER);
        assert!(!shared.receiving.load(Ordering::Relaxed));
        assert_eq!(shared.frames_rx.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_burst_for_another_talkgroup_is_ignored() {
        // A talkgroup number is half an address and the timeslot is the other
        // half; both are checked, and a master that relays a room this link
        // did not join is relaying it to somebody else.
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        let mut other = voice_packet(0, 0);
        other.dst_id = TG + 1;
        handle(
            &FsmAction::Data(Box::new(other)),
            &sock,
            addr,
            &shared,
            None,
        );
        assert_eq!(shared.last_heard_id.load(Ordering::Relaxed), NO_TALKER);
        assert_eq!(shared.frames_rx.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn voice_frames_reach_the_vocoder_tagged_as_dmr() {
        // `ChannelFrame::Dmr`, built by the codec and never converted: nine
        // bytes `.into()` a D-Star frame, and the wrong tag is not an error
        // anywhere downstream — it is a chip configured for the wrong rate
        // word returning confident noise. So this asks the vocoder what it was
        // handed rather than inferring it from what came back.
        let (mut audio, _rx, submitted) = test_audio_with_log();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        decode_burst(&voice_burst(), &mut audio, &shared);
        assert!(
            audio.pending.is_empty(),
            "all three frames fit the vocoder in one pass"
        );
        let submitted = submitted.lock().expect("the log");
        assert_eq!(submitted.len(), FRAMES_PER_BURST, "three frames per burst");
        assert_eq!(
            *submitted,
            vec![
                ChannelFrame::Dmr([1u8; 9]),
                ChannelFrame::Dmr([2u8; 9]),
                ChannelFrame::Dmr([3u8; 9]),
            ],
            "tagged Dmr, and in the order the burst carries them"
        );
    }

    #[test]
    fn all_three_frames_of_a_burst_are_kept() {
        // The vocoder accepts AMBE_STREAM_MAX_IN_FLIGHT (4) at a time and a
        // burst carries three, so the queue is the margin, not an
        // optimisation — two bursts back to back would otherwise drop two.
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        decode_burst(&voice_burst(), &mut audio, &shared);
        decode_burst(&voice_burst(), &mut audio, &shared);
        flush(&mut audio);
        assert_eq!(rx.try_iter().count(), 6);
    }

    #[test]
    fn frames_are_released_one_per_frame_interval_after_priming() {
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        decode_burst(&voice_burst(), &mut audio, &shared);
        assert_eq!(rx.try_iter().count(), 1, "one released at prime");
        // `decode_burst` pumps, and pumping primes the release clock against
        // the wall clock — so the base for "one per interval" is the clock
        // priming set, read back here. Timing the rest from the instant the
        // test started would be a race with the machine.
        let due = audio.next_release.expect("the clock is primed");
        release(
            &mut audio,
            due.checked_sub(Duration::from_millis(1))
                .expect("before due"),
        );
        assert_eq!(rx.try_iter().count(), 0, "not yet due");
        release(&mut audio, due);
        assert_eq!(rx.try_iter().count(), 1);
        release(&mut audio, due + FRAME_INTERVAL);
        assert_eq!(rx.try_iter().count(), 1, "exactly one per interval");
    }

    #[test]
    fn a_received_burst_is_not_decoded_while_transmitting() {
        // One ThumbDV, one AMBE-3000: never decode and encode at once.
        let (mut audio, rx) = test_audio();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        audio.tx = Some(Tx::new());
        let (sock, addr, _peer) = udp_pair();
        handle(
            &FsmAction::Data(Box::new(voice_packet(0, 0))),
            &sock,
            addr,
            &shared,
            Some(&mut audio),
        );
        assert_eq!(rx.try_iter().count(), 0);
        // The talker is still tracked: reading the header costs no vocoder.
        assert_eq!(shared.last_heard_id.load(Ordering::Relaxed), 4242);
    }

    #[test]
    fn an_unkeyed_pass_drops_whatever_the_lane_captured() {
        // The gate can be open before the loop observes the request; without
        // this drop the next transmission opens with stale speech.
        let (mut audio, _rx, mic) = test_audio_with_capture();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        mic.send(vec![0i16; 160]).expect("queue a mic frame");
        run_ptt_step(&mut audio, &shared, &sock, addr);
        assert!(
            audio.bus.tx_frames.try_recv().is_err(),
            "the lane was drained"
        );
        assert!(audio.mic_pending.is_empty());
    }

    #[test]
    fn set_ptt_only_requests_and_an_unkeyed_link_never_sends_voice() {
        // Nothing here is on the air: the peer socket is 127.0.0.1 and the
        // assertion is that NOTHING arrived.
        let (sock, addr, peer) = udp_pair();
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        let (mut audio, _rx, mic) = test_audio_with_capture();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        mic.send(vec![1i16; 160]).expect("queue");
        run_ptt_step(&mut audio, &shared, &sock, addr);
        let mut buf = [0u8; 64];
        assert!(
            peer.recv_from(&mut buf).is_err(),
            "an unkeyed link sends nothing"
        );
        assert!(!shared.ptt.load(Ordering::Relaxed));
    }

    #[test]
    fn a_key_request_is_refused_on_the_pass_that_reads_it() {
        // The gate, at the one function that could open it: `apply_ptt` must
        // clear the request rather than leave it standing, so no later pass
        // can find it and act on it.
        let (mut audio, _rx, _mic) = test_audio_with_capture();
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, _peer) = udp_pair();
        shared.ptt_request.store(true, Ordering::Relaxed);
        run_ptt_step(&mut audio, &shared, &sock, addr);
        assert!(!shared.ptt_request.load(Ordering::Relaxed));
        assert!(!shared.ptt.load(Ordering::Relaxed));
        assert!(audio.tx.is_none());
    }

    #[test]
    fn keying_is_refused_while_transmit_is_gated() {
        // TASK 12 REMOVES THIS TEST. Until the transmit path exists,
        // `set_ptt(true)` must not be able to produce a keyed state — the
        // receive-first gate, enforced in code rather than asserted in prose.
        let (master, addr) = loopback();
        let link = DmrLink::connect(cfg(addr)).expect("connect");
        wait_for(&link, "linked", Duration::from_secs(3));
        link.set_ptt(true);
        thread::sleep(Duration::from_millis(100));
        assert!(!link.snapshot().ptt);
        link.disconnect();
        master.shutdown();
    }

    #[test]
    fn a_datagram_that_cannot_leave_the_host_fails_the_link_rather_than_idling_it() {
        // Nothing is transmitted here — the send is refused by the host before
        // a byte reaches a wire: an IPv4 socket cannot address an IPv6
        // destination. It is the deterministic local way to produce the error
        // the run loop must not swallow.
        //
        // Breaking the loop quietly would leave the link reporting `idle` with
        // no failure, which is exactly what a deliberate disconnect looks
        // like — and an operator told "idle" stops looking for the cause.
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind v4");
        let unsendable: SocketAddr = "[::1]:9".parse().expect("v6");
        let keep_going = handle(
            &FsmAction::Send(vec![0u8; 4]),
            &sock,
            unsendable,
            &shared,
            None,
        );
        assert!(!keep_going, "the loop stops");
        assert_eq!(
            state_str(shared.link_state.load(Ordering::Acquire)),
            "failed"
        );
        assert_eq!(
            failure_str(shared.failure.load(Ordering::Relaxed)),
            Some("session"),
            "and says the session ended, not that the master refused us"
        );
    }

    #[test]
    fn a_send_that_leaves_the_host_keeps_the_link_running() {
        // The other half, so the arm above cannot pass by failing everything:
        // a datagram the socket accepts leaves the link exactly as it was.
        let shared = Arc::new(Shared::new(TG, Timeslot::Ts2));
        let (sock, addr, peer) = udp_pair();
        peer.set_read_timeout(Some(Duration::from_millis(200)))
            .expect("timeout");
        let keep_going = handle(&FsmAction::Send(vec![7u8; 4]), &sock, addr, &shared, None);
        assert!(keep_going);
        assert_eq!(
            state_str(shared.link_state.load(Ordering::Acquire)),
            "idle",
            "nothing was failed"
        );
        assert_eq!(failure_str(shared.failure.load(Ordering::Relaxed)), None);
        let mut buf = [0u8; 8];
        let (n, _) = peer.recv_from(&mut buf).expect("the peer got it");
        assert_eq!(&buf[..n], &[7u8; 4]);
    }

    #[test]
    fn a_link_without_audio_reports_no_backend() {
        let (master, addr) = loopback();
        let link = DmrLink::connect(cfg(addr)).expect("connect");
        assert_eq!(link.snapshot().backend, None);
        assert_eq!(link.snapshot().talkgroup, TG);
        assert_eq!(link.snapshot().timeslot, "ts2");
        link.disconnect();
        master.shutdown();
    }

    #[test]
    fn every_link_state_and_failure_stage_round_trips_through_its_index() {
        // The atomics carry indices and the snapshot carries strings; a
        // variant added to either enum without a line here would silently
        // read back as `idle` or as no failure at all.
        for state in [
            LinkState::Idle,
            LinkState::LoggingIn,
            LinkState::Authenticating,
            LinkState::Configuring,
            LinkState::Linked,
            LinkState::Closing,
            LinkState::Failed,
        ] {
            assert_eq!(state_from_index(state_index(state)), state);
            assert_eq!(state_str(state_index(state)), state.as_str());
        }
        for stage in [
            FailureStage::Login,
            FailureStage::Auth,
            FailureStage::Config,
            FailureStage::Session,
            FailureStage::Closed,
            FailureStage::Timeout,
        ] {
            assert_eq!(failure_str(failure_index(stage)), Some(stage.as_str()));
        }
        assert_eq!(failure_str(NO_FAILURE), None);
    }
}
