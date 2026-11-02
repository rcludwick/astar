// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `M17Session`: a full-transceive M17 reflector client runtime (iax-f2b8
//! Task 3).
//!
//! Unlike [`crate::session::ConsoleSession`] (IAX2/`AllStar`), M17 has no
//! separate call-setup handshake or protocol-level PTT frame: a `CONN`/`ACKN`
//! exchange with a reflector brings the link up, and transmission is carried
//! entirely by voice-stream packets (start of a stream = key-down, the `EOS`
//! bit = key-up).
//!
//! The session does **not** own its audio. It is handed a
//! [`CallAudio`](astar_audio::CallAudio) — the two channel ends of the one
//! lane `ConsoleSession` opened on the station's router
//! (`crate::voice_route`) — and owns nothing else about audio: no
//! [`astar_audio::AudioRouter`], no mic or bus id, no meter mirror, no
//! preference cell, no spectrum copy, and no PTT gate. Meters, DSP
//! preferences and keying are read and driven once, at the lane, by
//! `ConsoleSession`. What [`M17Session`] does own:
//!
//! - a [`SessionFsm`](astar_m17::SessionFsm) (link state + keepalive);
//! - a [`Codec2Voice`](astar_codec::codec2::Codec2Voice) instance (Codec 2
//!   mode 3200, the rate M17 payloads use);
//! - ONE run-loop thread ("iax-m17") that owns the `UdpSocket` (plain,
//!   `set_read_timeout(50ms)` — no mio, no async) and drives all of the
//!   above.
//!
//! The control-side [`M17Session`] handle talks to the run-loop thread only
//! through a small set of atomics (poll-cheap, per [`M17SnapshotState`]) plus
//! a request flag for PTT — never a shared/locked `Codec2Voice`, so that stays
//! single-threaded (owned entirely by the run-loop) with no cross-thread
//! synchronization on the hot audio path.
//!
//! # Layout
//!
//! - `session` holds [`M17Session`] and every transmit symbol: `TxState`,
//!   `apply_ptt_edge`, `drain_tx_frames`, `send_voice_packet` and
//!   `send_disc_flushing_eos_if_keyed`. No non-test code outside that file
//!   builds a voice-stream packet.
//! - `rx` holds the receive half: the talker log, one receive step, the
//!   400 ms silence clock, the Codec 2 pair decode and the socket connect.
//!   None of it sends. `recv_step` hands back whatever the state machine
//!   wants sent, and each run loop sends it on its own socket, so a
//!   receive-only leg can share this code without sharing a path to the
//!   wire.
//!
//! # RX jitter handling (documented choice)
//!
//! This milestone's RX path is a simple in-order pass-through: packets are
//! decoded and forwarded to [`astar_audio::router::CallAudio::rx_frames`]
//! in arrival order, with no reordering/jitter-smoothing buffer. `receiving`
//! reflects "a stream packet has arrived within the last 400 ms", not
//! anything about buffer health. The loopback tests exercise same-process
//! UDP (negligible jitter/reordering), so this is sufficient to validate the
//! milestone; a `astar_codec::jitter::JitterBuf`-backed reorder stage is
//! the natural follow-on hardening before this ships against a real
//! Internet-routed reflector.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, SystemTime};

use astar_m17::{LinkState, StreamPacket};
use astar_record::frames::{CaptureMode, Dir, FrameEvent, Seq};
use astar_record::tap::TapSlot;

mod monitor;
mod rx;
mod session;

pub use monitor::{M17Monitor, M17MonitorConfig, SourceSink};
pub use session::M17Session;

/// Operator-supplied configuration for an [`M17Session`].
pub struct M17Config {
    /// Reflector hostname or IP address.
    pub host: String,
    /// Reflector UDP port (the M17 IP-framing default is 17000, but this is
    /// caller-supplied — no protocol default is assumed here).
    pub port: u16,
    /// Reflector module letter (e.g. `b'A'`).
    pub module: u8,
    /// This station's callsign (encoded via
    /// [`encode_callsign`](astar_m17::encode_callsign); invalid callsigns
    /// fail [`M17Session::connect`] with
    /// [`ConsoleError::Device`](crate::session::ConsoleError::Device)).
    pub callsign: String,
    /// Extra directories to search for a runtime `libcodec2` (ahead of the
    /// hard-coded system paths); see [`astar_codec::open_codec2`].
    pub codec_dirs: Vec<std::path::PathBuf>,
    /// How long to wait, with no packet from the reflector, before declaring
    /// the link [`LinkState::Failed`]. Default (via a fresh
    /// [`SessionFsm`](astar_m17::SessionFsm)) is 30 s; tests shorten this to
    /// avoid a 30-real-second wait.
    pub keepalive_timeout: Duration,
}

/// A poll-cheap snapshot of an [`M17Session`]'s live state. Backed by atomics
/// on the control side — [`M17Session::state`] never blocks on the run-loop
/// thread.
// NOT `Copy`: `talker` is an owned `String`. Cloning a snapshot is one
// small allocation on a UI-rate poll, which is the same cost D-Star's and
// YSF's snapshots already pay for the same reason.
#[derive(Debug, Clone, PartialEq)]
pub struct M17SnapshotState {
    /// Current reflector link state.
    pub link: LinkState,
    /// `true` while transmit is keyed (mirrors the last [`M17Session::set_ptt`]
    /// applied by the run-loop; the apply is bounded by one
    /// [`SOCKET_POLL_TIMEOUT`](rx::SOCKET_POLL_TIMEOUT) tick, ~50 ms).
    pub ptt: bool,
    /// `true` while voice-stream packets have arrived from the reflector
    /// within the last 400 ms.
    pub receiving: bool,
    /// The source callsign of the most recently heard transmission — the
    /// `LSF.src` address of the first packet of the current (or last) stream,
    /// decoded with [`decode_callsign`](astar_m17::decode_callsign) — or
    /// `None` until one arrives.
    ///
    /// **Last heard, not talking now.** It PERSISTS past end-of-stream, on
    /// purpose and exactly as D-Star's `talker` does: a reflector that has
    /// been quiet for a minute should still name whoever last keyed up.
    /// [`Self::receiving`] is the field that says whether a transmission is
    /// in progress. It goes away with the session — a disconnected link has
    /// no last-heard, and a `None` here is what a caller sees once
    /// `m17_state()` stops returning a snapshot at all.
    ///
    /// **Attacker-supplied**: it is whatever callsign whoever keyed up put in
    /// their LSF. Render it as text, never as markup.
    pub talker: Option<String>,
}

/// Atomics shared between the control-side [`M17Session`] and its run-loop
/// thread. `link` is written by the run-loop and read by
/// [`M17Session::state`]; `ptt`/`receiving` follow the same direction. Levels
/// are NOT here: they are read at the lane, by `ConsoleSession`.
pub(super) struct SharedState {
    pub(super) link: AtomicU8,
    pub(super) ptt: AtomicBool,
    pub(super) receiving: AtomicBool,
    /// The last-heard callsign and the heard history. They live in the shared
    /// receive helpers so every M17 run loop records talkers the same way.
    pub(super) talkers: rx::TalkerLog,
    /// The codec-frame recording tap ([`M17Session::frame_tap`]). The run
    /// loop only ever offers to it, never waits on it.
    pub(super) frame_tap: TapSlot<FrameEvent>,
}

impl SharedState {
    pub(super) fn new() -> Self {
        Self {
            link: AtomicU8::new(link_to_u8(LinkState::Idle)),
            ptt: AtomicBool::new(false),
            receiving: AtomicBool::new(false),
            talkers: rx::TalkerLog::new(),
            frame_tap: TapSlot::default(),
        }
    }

    pub(super) fn snapshot(&self) -> M17SnapshotState {
        M17SnapshotState {
            link: u8_to_link(self.link.load(Ordering::Relaxed)),
            ptt: self.ptt.load(Ordering::Relaxed),
            receiving: self.receiving.load(Ordering::Relaxed),
            talker: self.talkers.talker(),
        }
    }
}

/// [`LinkState`] as the byte the run loop stores in [`SharedState::link`].
pub(super) fn link_to_u8(s: LinkState) -> u8 {
    match s {
        LinkState::Idle => 0,
        LinkState::Connecting => 1,
        LinkState::Linked => 2,
        LinkState::Failed => 3,
    }
}

/// The inverse of [`link_to_u8`]; an unknown byte reads as `Idle`.
pub(super) fn u8_to_link(v: u8) -> LinkState {
    match v {
        1 => LinkState::Connecting,
        2 => LinkState::Linked,
        3 => LinkState::Failed,
        _ => LinkState::Idle,
    }
}

/// The `Start` of one recorded M17 over. Built only inside an
/// [`TapSlot::offer_with`] closure, so nothing is made without a tap.
pub(super) fn rec_start(dir: Dir, talker: Option<String>) -> FrameEvent {
    FrameEvent::Start {
        dir,
        network: "m17",
        mode: CaptureMode::Codec2_3200,
        talker,
        at: SystemTime::now(),
        vocoder: "codec2",
    }
}

/// One 8-byte Codec 2 frame of a packet numbered `frame_number`. Both halves
/// of a packet carry the same sequence number: the frame counter's 15 bits,
/// without the end-of-stream bit.
pub(super) fn rec_frame(dir: Dir, bytes: &[u8], frame_number: u16) -> FrameEvent {
    FrameEvent::Frame {
        dir,
        bytes: bytes.to_vec(),
        seq: Some(Seq {
            value: frame_number & !StreamPacket::EOS_BIT,
            modulus: 0x8000,
        }),
    }
}
