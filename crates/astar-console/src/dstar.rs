// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `DstarSession`: a full-transceive D-Star (`DExtra`) reflector client
//! runtime (iax-a9d4 Task 6 built the RX-only version; iax-2f6b adds TX).
//! Mirrors [`crate::m17::M17Session`]'s shape (module layout, thread/shutdown
//! discipline, poll-cheap atomics-backed snapshot, `ptt_request` atomic
//! applied on the run-loop's next poll) wherever it maps cleanly. The
//! protocol-level differences from M17 are: D-Star carries PTT via a
//! DSVT/RF-header stream rather than bare stream-start/EOS packets (see
//! [`astar_dstar::tx::TxStream`], which owns the header/voice/EOT framing
//! and slow-data sync this module just drives), and D-Star is strictly
//! half-duplex against the one physical `ThumbDV` link — this session never
//! decodes and encodes at the same time (see "Half-duplex" below).
//!
//! - It does **not** own its audio. It is handed a [`CallAudio`] — the two
//!   channel ends of the one lane `ConsoleSession` opened on the station's
//!   router (`crate::voice_route`) — and owns nothing else about audio: no
//!   [`astar_audio::AudioRouter`], no mic or bus id, no meter mirror, no
//!   preference cell, no spectrum copy, and no PTT gate. Meters, DSP
//!   preferences and keying are read and driven once, at the lane, by
//!   `ConsoleSession`, which also decides whether a machine with no usable
//!   microphone may key at all (receive-only D-Star still works, as it
//!   always did — the decision simply moved).
//! - [`DstarSession::set_ptt`] stores a request atomic; the run-loop applies
//!   the edge on its next poll (bounded by [`SOCKET_POLL_TIMEOUT`]),
//!   building/sending the header on key-down and flushing a terminating
//!   (`end = true`) DSVT voice frame on key-up — see [`apply_ptt_edge`] for
//!   the full ordering and the safety guarantees around it.
//! - A [`DextraFsm`] (link keepalive/ACK-NAK state machine) stands in for
//!   `SessionFsm`; the same pipelined [`AmbeStream`] handle serves BOTH
//!   directions (`submit_decode`/`poll_decoded` for RX, `submit_encode`/
//!   `poll_encoded` for TX, iax-2f6b's shared-worker encode path) — never
//!   both at once, see "Half-duplex" below.
//!
//! # Half-duplex
//!
//! The `ThumbDV` is one physical serial link; D-Star itself is a
//! half-duplex protocol. While `keyed` this session (1) never submits a
//! received DSVT frame to the decoder — [`poll_socket`] drops inbound
//! `"DSVT"` traffic outright while transmitting, answering only link-level
//! control packets (keepalive) — and (2) on the key-DOWN edge itself,
//! flushes/discards whatever RX audio was still queued or in flight
//! (mirroring the same handoff [`flush_pipeline`] already does between
//! talkers) before the first mic frame is ever submitted to the encoder.
//! This is enforced in code, not just assumed true in the common case.
//!
//! # TX safety — unkey is unconditional
//!
//! [`apply_ptt_edge`]'s key-up branch ALWAYS emits exactly one DSVT voice
//! frame with the `end` bit set, however it gets there: normal drain (queued
//! mic audio flows through the encoder and out), an empty transmission (an
//! instant PTT tap with no audio ever captured — a bare
//! [`NULL_AMBE`](astar_dstar::NULL_AMBE) EOT frame is sent), or a wedged
//! encoder (bounded by [`FLUSH_DEADLINE`], same as RX's own drain bound —
//! whatever encoded so far is sent, still terminated). The run-loop's
//! shutdown branch and [`DstarSession::drop`] both route through the exact
//! same function before the link is torn down (see
//! [`unlink_flushing_eot_if_keyed`]), so a session can never be
//! dropped/disconnected while still keyed on the wire. PTT itself can only
//! ever be requested by an explicit [`DstarSession::set_ptt`] call — nothing
//! in this module keys on its own (connect, a received packet, or a timer
//! never key transmit).
//!
//! Every send on the TX path is checked, not fire-and-forget: a failed send
//! is logged (a connected UDP socket latches `ECONNREFUSED` from an ICMP
//! port-unreachable, which is exactly what a reflector dying under a keyed
//! operator looks like), a failed RF header REFUSES the key-down outright
//! rather than transmitting voice under a stream id no receiver ever saw a
//! header for, and the terminating frame gets a bounded retry
//! ([`EOT_SEND_ATTEMPTS`]) before giving up loudly.
//!
//! Three things force an unkey the operator did not ask for — see
//! [`PttGate`], which owns all of this policy and is unit-tested directly:
//!
//! - the link leaving [`LinkState::Linked`] (keepalive timeout, or a
//!   reflector-sent `DISCONNECTED`): continuing to encode and send voice
//!   frames at a peer that no longer has us linked is pointless and hides
//!   the failure from the operator;
//! - [`MAX_TX_DURATION`] elapsing (the conventional radio time-out timer): a
//!   lost key-up event — a dropped hotkey release, a wedged UI thread — must
//!   not leave the station transmitting indefinitely;
//! - a key-down that cannot be honoured (the header send failed) is refused
//!   rather than half-applied. A key-down with no capture device never
//!   reaches this module at all: `ConsoleSession::set_ptt` refuses it with
//!   `ConsoleError::NoCaptureDevice` before forwarding anything.
//!
//! In every one of those cases the gate closes, a terminating frame goes out
//! through the ordinary unkey path, and PTT stays refused until the operator
//! physically releases it (`ptt_request` going false clears the latch) — a
//! release-to-re-arm rule, so a stuck-down PTT can never auto-re-key.
//!
//! # Half-duplex, continued: rejoining an over
//!
//! A consequence of the inbound mute worth stating: because inbound headers
//! are dropped while keyed and stream tracking is reset at key-down, a remote
//! transmission that was already in progress when the operator keyed cannot
//! be rejoined after unkey — its voice frames hit the orphan guard in
//! [`handle_dsvt`] until that talker stops and a fresh `Header` arrives. On a
//! busy reflector a short over from the local operator therefore blanks the
//! remainder of whatever was already in progress. This is the deliberate
//! (and, for a half-duplex protocol on one physical dongle, unavoidable)
//! consequence of the mute, not an RX bug.
//!
//! # Talker / slow-data tracking
//!
//! Every [`DsvtPacket::Header`] starts a fresh tracked stream (`stream_id`)
//! and resets both the current talker (`RfHeader::my_callsign`) and the
//! [`SlowDataRx`] reassembler. A [`DsvtPacket::Voice`] frame is decoded and
//! forwarded ONLY when its `stream_id` matches the currently tracked one —
//! an orphan voice frame (no header ever seen, or a stray frame from an
//! abandoned/unrelated stream) is dropped rather than guessed at, mirroring
//! `astar_dstar::reflector`'s own parrot-capture policy. The talker
//! deliberately PERSISTS past a stream's `EOT` (last-heard semantics, like a
//! real D-Star radio's display) — only a fresh `Header` ever replaces it.
//!
//! # Pipelined AMBE decode (iax-b3e7 M0)
//!
//! [`AmbeVoice::decode`](astar_codec::ambe::AmbeVoice::decode)'s
//! stop-and-wait shape cannot sustain the `ThumbDV` hardware backend against
//! D-Star's 20 ms cadence (measured 24.5 ms mean per frame — see the M0
//! spec). Arriving [`DsvtPacket::Voice`] frames are instead queued as raw
//! 9-byte channel frames (`pending`, bounded by [`MAX_PENDING_FRAMES`]) and
//! fed to the pipeline by [`pump`] whenever the vocoder has room, with
//! [`poll_decoded`](AmbeStream::poll_decoded) drained to exhaustion on the
//! same pass.
//!
//! The raw-frame queue is what decouples the vocoder's in-flight bound
//! ([`AMBE_STREAM_MAX_IN_FLIGHT`], a property of the device) from network
//! arrival jitter (a property of the reflector path, which the device bound
//! does not model). Without it, a UDP burst delivering 2–3 frames back to
//! back — the ordinary case jitter buffers exist for — silently discarded
//! every frame past the pipeline's depth. Buffering nine bytes per frame
//! costs nothing; a dropped frame costs 20 ms of the operator's audio.
//!
//! There is no separate wall-clock playout ticker: the run loop pumps once
//! per pass and its socket read timeout tightens to
//! [`BACKLOG_POLL_TIMEOUT`] whenever `pending` is non-empty, so a backlog
//! drains at the device's rate rather than the 50 ms idle poll's. Playout
//! *cadence* is then the mixer's job — note that
//! [`astar_audio::mixer`]'s jitter buffer decouples playout from
//! arrival without ever blocking, but it does not trim or skew-correct, so
//! whatever latency a burst introduces is carried until the next silence.
//!
//! Each stream primes [`AMBE_STREAM_PRIME_FRAMES`] frames before its first
//! poll (see that constant's doc). [`flush_pipeline`] then bounds both ends
//! of a transmission: on `end` it drains what is still in flight so the tail
//! isn't clipped, and on a fresh `Header` (or an abandoned stream, see
//! [`STREAM_IDLE_TIMEOUT`]) it *discards* the previous talker's residue so
//! it can never be played out attributed to the new one.
//!
//! # Hardware-only (iax-b3e7 M0)
//!
//! D-Star runs through the `ThumbDV` dongle only: [`DstarSession::connect`]
//! requests [`AmbeBackend::Hardware`] from
//! [`open_ambe_stream`](astar_codec::ambe::open_ambe_stream)
//! unconditionally — no preference knob, no software-codec fallback. There is
//! no software AMBE backend in this workspace at all. A build without
//! `ambe-hw` compiled in, or a
//! `dstar` build with no dongle attached, fails [`DstarSession::connect`]
//! outright rather than silently substituting software decode. Since the
//! same handle now also serves TX (`submit_encode`/`poll_encoded`),
//! `tx_capable` is unconditionally `true` for every session that exists at
//! all — there is no partially-hardware-only, RX-but-not-TX state.

use std::collections::VecDeque;
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use astar_audio::CallAudio;
use astar_codec::ambe::{
    AMBE_STREAM_MAX_IN_FLIGHT, AmbeBackend, AmbeStream, VocoderMode, open_ambe_stream,
};
use astar_dstar::tx::general_call_header;
use astar_dstar::{
    DSVT_MAGIC, DextraFsm, DsvtPacket, FsmAction, LinkState, NULL_AMBE, RfHeader, SlowDataRx,
    TxStream, generate_stream_id, repeater_fields,
};

use crate::session::ConsoleError;

/// The run-loop thread's socket read timeout: also the cadence at which the
/// FSM keepalive tick is re-checked. Mirrors [`crate::m17`]'s
/// `SOCKET_POLL_TIMEOUT` (50 ms — a plain read-timeout loop, not mio/async).
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Pipeline priming cushion (iax-b3e7 M0, spec §2): the number of voice
/// frames submitted to the [`AmbeStream`] at the start of every transmission
/// before the run-loop starts pulling decoded audio back out via
/// `poll_decoded`. Needed because the pipelined `ThumbDV` worker answers
/// requests positionally/FIFO but not instantaneously — priming keeps the
/// device continuously fed (never waiting on a reply) at the cost of ~60 ms
/// (3 × 20 ms) of extra one-way latency, which is acceptable for a
/// listen-only conference client and required for the device to stay ahead
/// of the 20 ms D-Star cadence.
const AMBE_STREAM_PRIME_FRAMES: u8 = 3;

/// How long [`flush_pipeline`] sleeps between empty `poll_decoded` polls
/// while flushing a stream. Short relative to both the pipelined
/// `ThumbDV`'s measured per-frame cost (~7.45 ms) and the 20 ms D-Star
/// cadence — this just keeps a hardware drain from busy-spinning the
/// run-loop thread while it waits on the last few in-flight replies.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Wall-clock bound on ONE [`flush_pipeline`] (RX) or [`flush_tx_pipeline`]
/// (TX) call.
///
/// Both flushes run inline on the run-loop thread, so while either is
/// running the socket isn't read, the FSM keepalive isn't ticked and
/// shutdown isn't observed — they must therefore always terminate, and
/// quickly. A healthy end-of-stream/end-of-transmission flush is a few tens
/// of ms (at most [`MAX_PENDING_FRAMES`]/[`MAX_PENDING_TX_FRAMES`] +
/// [`AMBE_STREAM_MAX_IN_FLIGHT`] frames at the device's ~7.45 ms each,
/// usually far fewer).
///
/// The bound matters most when the vocoder is NOT healthy: `in_flight()`/
/// `in_flight_encoded()` only fall when `poll_decoded`/`poll_encoded` return
/// something, so a worker thread that died with frames outstanding would
/// otherwise leave either loop spinning forever — inside the run loop, which
/// `disconnect()`/`Drop` then `join()`, deadlocking teardown from the
/// caller's (UI) thread with no way out. That is precisely the failure mode
/// iax-239a's "a wedged transfer surfaces as an error, never a hang" rule
/// exists to prevent. On the TX side this bound is also what makes unkey
/// UNCONDITIONAL: [`apply_ptt_edge`]'s key-up branch always sends a
/// terminating frame once [`flush_tx_pipeline`] returns, deadline-hit or not
/// — see that function's doc.
const FLUSH_DEADLINE: Duration = Duration::from_millis(250);

/// Cap on the queue of arrived-but-not-yet-submitted 9-byte channel frames.
/// 32 frames is 640 ms of audio and ~288 bytes of memory — far more slack
/// than any reflector jitter this is meant to absorb, and still bounded so a
/// wedged vocoder can't grow it without limit. Overflow drops the NEWEST
/// frame, matching [`AmbeStream::submit_decode`]'s own drop rule.
const MAX_PENDING_FRAMES: usize = 32;

/// Cap on the queue of captured-but-not-yet-submitted 160-sample TX PCM
/// frames, mirroring [`MAX_PENDING_FRAMES`]'s RX-side reasoning. A mic
/// capture callback is not expected to burst the way UDP arrivals do (audio
/// arrives at a steady real-time rate), but the bound exists for the same
/// defense-in-depth reason: a wedged encoder must not let this grow without
/// limit. Overflow drops the NEWEST frame.
const MAX_PENDING_TX_FRAMES: usize = 32;

/// `RPT1`/`RPT2` fallback for astar's own TX header, used ONLY when the
/// destination reflector cannot be named (see [`tx_repeater_fields`]).
///
/// Research §4 documents "typically left blank or set to spaces" as a
/// convention for a pure reflector client, and this crate's own loopback
/// reflector doesn't inspect either field — but blank is NOT harmless in
/// general: the module character `xlxd`'s `DPlus` path gates on
/// (`IsValidModule(rpt2.GetModule())`) lives in `RPT2`, and a receiving
/// radio/dashboard renders both fields, so a blank pair transmits with no
/// destination or repeater identity at all. Hence the fallback is a last
/// resort, logged when taken, rather than the default.
const BLANK_RPT: [u8; 8] = *b"        ";

/// The DSVT voice-frame cadence D-Star's framing layer requires
/// ([`astar_dstar::tx`]'s module doc: "callers own pacing — send one
/// `next_voice_frame` every real 20 ms"). [`pump_tx`] paces egress off this,
/// rather than emitting whatever the encoder happened to finish since the
/// last run-loop pass: average packet rate is not the same thing as cadence,
/// and receivers downstream of a reflector (ircDDBGateway/MMDVMHost/hotspots)
/// hold only a small isochronous ring — a 50 ms gap followed by a 3-packet
/// burst is the classic recipe for choppy received audio and modem
/// underruns.
const TX_FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// How many encoded frames the pacer will hold before releasing the excess
/// immediately rather than paying for it in latency.
///
/// Pacing must never turn a source burst — a capture callback delivering a
/// long block, or a vocoder returning several frames at once after a stall —
/// into permanent one-way latency for the rest of the over. Above this
/// watermark the surplus goes out at once and the 20 ms schedule resumes from
/// there. 25 frames is 500 ms: far past anything a real 20 ms capture cadence
/// produces, and the point where added latency stops being conversational.
const MAX_TX_PACING_BACKLOG: usize = 25;

/// Time-out timer (`TOT`): the longest single transmission this session will
/// sustain before forcing an unkey, regardless of what the PTT source says.
///
/// The engine — not whatever UI happens to be driving it — is the layer that
/// owns the wire, so this belongs here: a lost key-up event (a dropped hotkey
/// release, a window focus change swallowing it, a UI thread wedging while
/// holding PTT) must not leave a station transmitting for as long as the
/// process lives. Five minutes is far longer than any legitimate over and
/// unambiguously a stuck key. Re-keying requires the operator to physically
/// release PTT first (see [`PttGate`]).
const MAX_TX_DURATION: Duration = Duration::from_secs(300);

/// How many times the terminating (EOT) frame is (re)sent before giving up.
/// Every other TX send is best-effort-with-a-warning; this one frame is what
/// tells the reflector the stream is over, so a transient send failure gets
/// bounded retries rather than a shrug — an unterminated stream is held open
/// at the far end until ITS idle timeout.
const EOT_SEND_ATTEMPTS: usize = 3;

/// Delay between [`EOT_SEND_ATTEMPTS`]. Deliberately tiny: this runs inline
/// on the run-loop thread during unkey, which must stay bounded.
const EOT_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Socket read timeout used while `pending` is non-empty, in place of
/// [`SOCKET_POLL_TIMEOUT`]. A backlog means the vocoder has frames waiting
/// on it, so the run loop needs to come back around and pump promptly rather
/// than sitting in a 50 ms idle read.
const BACKLOG_POLL_TIMEOUT: Duration = Duration::from_millis(2);

/// A tracked stream with no voice frame for this long is abandoned: its
/// remaining frames are discarded and tracking is reset.
///
/// D-Star streams routinely end without their `end`-flagged frame (the last
/// frame is lost, the talker's link drops, the reflector switches talkers
/// mid-stream). Without this, an abandoned stream's frames sit in the
/// vocoder indefinitely and the next talker's first polls return the
/// PREVIOUS talker's audio. 400 ms is 20 missed 20 ms frames — far past any
/// plausible jitter, well inside human perception of a transmission ending.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_millis(400);

/// Operator-supplied configuration for a [`DstarSession`].
pub struct DstarConfig {
    /// Reflector hostname or IP address.
    pub host: String,
    /// Reflector UDP port (`DExtra`'s IP-framing default is 30001, but this
    /// is caller-supplied — no protocol default is assumed here).
    pub port: u16,
    /// Reflector module letter (e.g. `b'B'`), already validated/uppercased
    /// by the caller (mirrors [`crate::m17::M17Config::module`] — this type
    /// trusts its caller the same way).
    pub module: u8,
    /// This station's callsign, fed to [`DextraFsm::new`] (space-padded /
    /// truncation-checked there; invalid callsigns fail
    /// [`DstarSession::connect`] with [`ConsoleError::Device`]).
    pub callsign: String,
    /// The destination reflector's CALLSIGN as the directories list it
    /// (e.g. `"XRF757"`, `"XLX836"`), used to fill the TX RF header's
    /// `RPT2`/`RPT1` fields — see [`tx_repeater_fields`] for why those matter
    /// and what is filled. An `XLX…` name is translated to the `XRF…` one the
    /// reflector answers to on the `DExtra` wire, so either spelling works
    /// here. `None` falls back to deriving it from [`Self::host`]'s first DNS
    /// label when that label looks like a reflector callsign (`XRF757`,
    /// `XLX458`, `XLXARG`, `REF030`, …), and to blank fields (with a warning)
    /// when even that fails — e.g. when connecting by bare IP address, which
    /// is exactly the case a caller should fill this in for.
    pub reflector_callsign: Option<String>,
}

/// A poll-cheap snapshot of a [`DstarSession`]'s live state. Backed by
/// shared cells on the control side — [`DstarSession::state`] never blocks
/// on the run-loop thread.
#[derive(Debug, Clone, PartialEq)]
pub struct DstarSnapshotState {
    /// Current reflector link state.
    pub link: LinkState,
    /// The MY callsign of the currently (or most recently) heard
    /// transmission's [`astar_dstar::RfHeader`], if any header has been
    /// seen since connecting. Persists past a stream's `EOT` — see the
    /// module docs.
    pub talker: Option<String>,
    /// The most recently fully-reassembled RX slow-data free-text message,
    /// if any has completed since connecting (see
    /// [`astar_dstar::SlowDataRx`]). Persists across streams, like
    /// `talker`.
    pub slow_text: Option<String>,
    /// Which concrete AMBE backend this session opened with. Always `Some`
    /// once a session exists ([`DstarSession::connect`] fails outright if no
    /// backend is available) — `Option` per the milestone design brief's
    /// literal interface shape.
    pub backend: Option<AmbeBackend>,
    /// Whether this station can transmit at all.
    ///
    /// CONSOLE-OWNED (one-audio-lane): the session itself always reports
    /// `true` — every [`DstarSession`] that exists opened a hardware
    /// `ThumbDV` handle capable of both directions, so there is no
    /// partially-hardware, RX-only state — but transmit also needs a
    /// capture device, and the lane, not the session, is what has one.
    /// `ConsoleSession::dstar_state` overwrites this with
    /// `voice_route_tx_capable()` before any caller sees it. Read it from
    /// there, never from [`DstarSession::state`].
    pub tx_capable: bool,
    /// `true` while transmit is keyed.
    ///
    /// This is the run loop's ACTUALLY-APPLIED state, not an echo of the last
    /// [`DstarSession::set_ptt`] request: a key-down that was refused (link
    /// not up, or the header send failed) never sets it, and a forced unkey
    /// (link lost mid-transmission, [`MAX_TX_DURATION`]) clears it without
    /// the operator asking. A request is applied within one
    /// [`SOCKET_POLL_TIMEOUT`] tick (~50 ms); on the unkey side the flag
    /// clears at the top of the unkey, BEFORE the (bounded, up to
    /// [`FLUSH_DEADLINE`]) encoder flush that puts the last frames on the
    /// wire — so a UI never shows "keyed" for a transmission that has already
    /// stopped capturing.
    pub ptt: bool,
    /// Transmit level in dBFS (post-gain, post-gate).
    ///
    /// CONSOLE-OWNED, like the two below and [`Self::tx_capable`]: levels are
    /// read once, at the lane, by `ConsoleSession` — a session owns no
    /// meters. [`DstarSession::state`] reports the -60 dBFS silence floor
    /// here and `ConsoleSession::dstar_state` overwrites it with
    /// `ConsoleState::tx_level_db`.
    pub tx_dbfs: f32,
    /// Receive level in dBFS on the lane's output bus. Console-owned — see
    /// [`Self::tx_dbfs`].
    pub rx_dbfs: f32,
    /// Raw microphone input level in dBFS, updated even while unkeyed — the
    /// meter a UI shows while the operator sets their gain. Console-owned —
    /// see [`Self::tx_dbfs`].
    pub input_dbfs: f32,
}

/// Atomics/cells shared between the control-side [`DstarSession`] and its
/// run-loop thread. All written by the run-loop, read by
/// [`DstarSession::state`].
struct SharedState {
    link: AtomicU8,
    talker: Mutex<Option<String>>,
    slow_text: Mutex<Option<String>>,
    ptt: AtomicBool,
}

impl SharedState {
    fn new() -> Self {
        Self {
            link: AtomicU8::new(link_to_u8(LinkState::Idle)),
            talker: Mutex::new(None),
            ptt: AtomicBool::new(false),
            slow_text: Mutex::new(None),
        }
    }

    /// The session's half of the snapshot. The four console-owned fields
    /// (`tx_capable` and the three levels — see [`DstarSnapshotState`]) are
    /// filled with the values a session with no audio of its own can honestly
    /// report: the vocoder handle is bidirectional, and the meters read the
    /// silence floor. `ConsoleSession::dstar_state` overwrites all four from
    /// the lane before any caller sees them.
    fn snapshot(&self, backend: AmbeBackend) -> DstarSnapshotState {
        DstarSnapshotState {
            link: u8_to_link(self.link.load(Ordering::Relaxed)),
            talker: self.talker.lock().expect("talker mutex").clone(),
            slow_text: self.slow_text.lock().expect("slow_text mutex").clone(),
            backend: Some(backend),
            tx_capable: true,
            ptt: self.ptt.load(Ordering::Relaxed),
            tx_dbfs: -60.0,
            rx_dbfs: -60.0,
            input_dbfs: -60.0,
        }
    }
}

fn link_to_u8(s: LinkState) -> u8 {
    match s {
        LinkState::Idle => 0,
        LinkState::Linking => 1,
        LinkState::Linked => 2,
        LinkState::Unlinking => 3,
        LinkState::Failed => 4,
    }
}

fn u8_to_link(v: u8) -> LinkState {
    match v {
        1 => LinkState::Linking,
        2 => LinkState::Linked,
        3 => LinkState::Unlinking,
        4 => LinkState::Failed,
        _ => LinkState::Idle,
    }
}

/// A full-transceive `DExtra` reflector client: connects, decodes received
/// AMBE voice to PCM, keys/unkeys transmit, and tracks the current
/// talker/slow-data — see the module docs for the TX safety guarantees.
pub struct DstarSession {
    /// `Some` until [`DstarSession::disconnect`] (or `Drop`) joins it.
    thread: Option<JoinHandle<()>>,
    /// Set to request the run-loop thread send an unlink and exit. The 50 ms
    /// socket read timeout bounds how long a join can take.
    shutdown: Arc<AtomicBool>,
    /// The last [`DstarSession::set_ptt`] request; the run-loop applies it
    /// (header-on-key / flush-and-EOT-on-unkey) on its next poll. Mirrors
    /// [`crate::m17::M17Session`]'s `ptt_request`.
    ptt_request: Arc<AtomicBool>,
    shared: Arc<SharedState>,
    /// The AMBE backend this session opened with (fixed for the session's
    /// lifetime — there is no live backend-switching knob).
    backend: AmbeBackend,
}

impl DstarSession {
    /// Connect to a `DExtra` reflector: opens an AMBE codec handle, binds a
    /// UDP socket, and starts the "iax-dstar" run-loop thread, which sends
    /// the initial link (connect) request.
    ///
    /// `audio` is the one audio lane `ConsoleSession` already opened on the
    /// station's router (`crate::voice_route`): the session encodes whatever
    /// arrives on `audio.tx_frames` while keyed and plays what it decodes
    /// onto `audio.rx_frames`. It builds no router, resolves no device,
    /// carries no preference and never touches the PTT gate — see the module
    /// docs.
    ///
    /// D-Star is hardware-only (iax-b3e7 M0): this always requests
    /// [`AmbeBackend::Hardware`] from [`open_ambe_stream`] — there is no
    /// preference knob and no software-codec fallback.
    ///
    /// # Errors
    /// [`ConsoleError::Device`] for an invalid callsign; [`ConsoleError::Dstar`]
    /// when no `ThumbDV` is available (`open_ambe_stream` returned `None` —
    /// `ambe-hw` isn't compiled in, or compiled in but no dongle was
    /// detected/it's busy — see
    /// [`classify_thumbdv_failure`](astar_codec::ambe::classify_thumbdv_failure)'s
    /// doc for how the message names the specific cause, iax-b3e7 spec §4);
    /// [`ConsoleError::Resolve`] if `cfg.host`/`cfg.port` don't resolve or
    /// the socket can't be bound.
    // `cfg` is taken by value per the M17/console `*Config` convention
    // ([`crate::m17::M17Session::connect`], `ConsoleSession::connect`'s own
    // `ConsoleConfig`): every field is read out (cloned/copied/borrowed)
    // rather than moved, which is why clippy would otherwise suggest a
    // reference here.
    #[allow(clippy::needless_pass_by_value)]
    pub fn connect(cfg: DstarConfig, audio: CallAudio) -> Result<DstarSession, ConsoleError> {
        Self::connect_inner(cfg, audio, None)
    }

    /// [`DstarSession::connect`] with the AMBE decoder supplied by the
    /// caller instead of opened here.
    ///
    /// Two callers want this:
    ///
    /// - the `astar-station` facade, so the `ThumbDV` probe + init
    ///   cookbook (a candidate-port scan, then up to two baud rates × eight
    ///   300 ms-bounded transactions per candidate) runs OUTSIDE its session
    ///   mutex — every other `Station` method takes that same lock, and the
    ///   `AstarStation` contract is poll-and-snapshot, never blocking;
    /// - tests, which pass an in-process fake [`AmbeStream`] so the priming
    ///   cushion, the end-of-stream drain, the talker-change discard and the
    ///   idle-stream guard all stay testable on a machine with no dongle.
    ///
    /// `backend` is reported verbatim through
    /// [`DstarSnapshotState::backend`]; it must describe `ambe`.
    ///
    /// # Errors
    /// Same as [`DstarSession::connect`], minus the vocoder-availability
    /// cases the caller has already resolved.
    pub fn connect_with_stream(
        cfg: DstarConfig,
        audio: CallAudio,
        ambe: Box<dyn AmbeStream>,
        backend: AmbeBackend,
    ) -> Result<DstarSession, ConsoleError> {
        Self::connect_inner(cfg, audio, Some((ambe, backend)))
    }

    /// The shared body of [`Self::connect`] and [`Self::connect_with_stream`].
    /// `vocoder` is `None` when this should open the `ThumbDV` itself.
    #[allow(clippy::needless_pass_by_value)]
    fn connect_inner(
        cfg: DstarConfig,
        audio: CallAudio,
        vocoder: Option<(Box<dyn AmbeStream>, AmbeBackend)>,
    ) -> Result<DstarSession, ConsoleError> {
        // Validate/build the FSM first (cheap, no I/O) — mirrors
        // M17Session::connect validating the callsign before touching any
        // device.
        let fsm = DextraFsm::new(&cfg.callsign, cfg.module)
            .map_err(|e| ConsoleError::Device(format!("invalid D-Star callsign: {e:?}")))?;
        // The TX header never changes for this session's lifetime: build it
        // once, here. `my` comes from the SAME padded bytes the FSM already
        // computed (no separate padding logic to keep in sync); RPT1/RPT2
        // name the destination reflector + module — see tx_repeater_fields.
        let (rpt1, rpt2) =
            tx_repeater_fields(cfg.reflector_callsign.as_deref(), &cfg.host, cfg.module);
        let header = general_call_header(rpt2, rpt1, fsm.callsign());

        // Hardware-only (iax-b3e7 M0): request Hardware unconditionally, no
        // preference knob and no software-codec fallback.
        //
        // On failure, classify WHY (iax-b3e7 spec §4) instead of surfacing a
        // generic message: `open_ambe_stream` collapses "no dongle" and
        // "dongle busy" into one `None`, exactly the ambiguity that left
        // `ambe-bench` holding the port indistinguishable from an unplugged
        // one. `classify_thumbdv_failure` re-scans the same candidate ports
        // and trial-opens each to tell them apart, naming the specific port
        // in the busy case.
        let (ambe, backend) = match vocoder {
            Some(v) => v,
            None => open_ambe_stream(Some(AmbeBackend::Hardware), VocoderMode::Dstar).ok_or_else(
                || ConsoleError::Dstar(astar_codec::ambe::classify_thumbdv_failure().message()),
            )?,
        };

        let socket = connect_udp_socket(&cfg.host, cfg.port)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let ptt_request = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SharedState::new());

        let thread_shutdown = Arc::clone(&shutdown);
        let thread_ptt_request = Arc::clone(&ptt_request);
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("iax-dstar".to_string())
            .spawn(move || {
                run_loop(RunLoopParams {
                    socket,
                    fsm,
                    ambe,
                    call_audio: audio,
                    header,
                    shutdown: thread_shutdown,
                    ptt_request: thread_ptt_request,
                    shared: thread_shared,
                });
            })
            .map_err(|e| ConsoleError::Resolve {
                node: cfg.host.clone(),
                source: e,
            })?;

        Ok(DstarSession {
            thread: Some(handle),
            shutdown,
            ptt_request,
            shared,
            backend,
        })
    }

    /// Engage/release transmit (iax-2f6b). Stores a request atomic; the
    /// run-loop applies the edge on its next poll (bounded by
    /// [`SOCKET_POLL_TIMEOUT`], ~50 ms) — this call itself never blocks.
    /// Mirrors [`crate::m17::M17Session::set_ptt`] exactly. Nothing in this
    /// session ever keys on its own — this is the ONLY path that can set the
    /// request true (see the module docs' TX-safety section).
    ///
    /// The lane's gate is NOT this session's: `ConsoleSession::set_ptt`
    /// opens it before forwarding the key here, and closes it on key-up.
    pub fn set_ptt(&mut self, on: bool) {
        self.ptt_request.store(on, Ordering::Relaxed);
    }

    /// A poll-cheap snapshot of the session's current state.
    #[must_use]
    pub fn state(&self) -> DstarSnapshotState {
        self.shared.snapshot(self.backend)
    }

    /// Disconnect: requests the run-loop send an unlink and exit, then joins
    /// the thread. The 50 ms socket read timeout bounds the join.
    pub fn disconnect(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.join_thread();
    }

    /// Join the run-loop thread and reconcile the snapshot with what actually
    /// happened to it.
    ///
    /// Every cell [`DstarSession::state`] reads is written ONLY by that
    /// thread, so a thread that unwound (a poisoned mutex propagating through
    /// the RX path, or any future panic in the loop) would otherwise freeze
    /// `link`/`ptt` at their last values — reporting a live, possibly KEYED
    /// session forever, with `disconnect()` swallowing the `Err` and
    /// reporting success. A panicked thread is a failed session: say so.
    fn join_thread(&mut self) {
        let Some(t) = self.thread.take() else {
            return;
        };
        if t.join().is_err() {
            tracing::error!(
                "dstar: the run-loop thread panicked; reporting the link as failed and PTT as \
                 unkeyed (no EOT/unlink was sent)"
            );
            self.shared
                .link
                .store(link_to_u8(LinkState::Failed), Ordering::Relaxed);
            self.shared.ptt.store(false, Ordering::Relaxed);
        }
    }
}

impl Drop for DstarSession {
    fn drop(&mut self) {
        // Defensive: a session dropped without an explicit `disconnect()`
        // call still shuts its thread down cleanly rather than leaking it —
        // mirrors M17Session's Drop.
        self.shutdown.store(true, Ordering::Relaxed);
        self.join_thread();
    }
}

/// Resolves `host:port` and returns a connected, read-timeout-armed
/// [`UdpSocket`]. A copy of [`crate::m17`]'s own `connect_udp_socket` (that
/// function is private to its module) — see its doc comment for the full
/// rationale on trying every resolved candidate address (notably
/// `"localhost"` on macOS resolving to both an IPv6 and IPv4 candidate).
///
/// # Errors
/// [`ConsoleError::Resolve`] if `host`/`port` resolve to no addresses, or no
/// candidate can be bound+connected, or the chosen socket's read timeout
/// can't be set.
fn connect_udp_socket(host: &str, port: u16) -> Result<UdpSocket, ConsoleError> {
    let resolve_err = || ConsoleError::Resolve {
        node: host.to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("could not resolve {host}:{port}"),
        ),
    };
    let candidates: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .unwrap_or_default();
    let mut connected = None;
    for candidate in &candidates {
        let bind_addr = if candidate.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let Ok(candidate_socket) = UdpSocket::bind(bind_addr) else {
            continue;
        };
        if candidate_socket.connect(candidate).is_ok() {
            connected = Some(candidate_socket);
            break;
        }
    }
    let socket = connected.ok_or_else(resolve_err)?;
    socket
        .set_read_timeout(Some(SOCKET_POLL_TIMEOUT))
        .map_err(|e| ConsoleError::Resolve {
            node: host.to_string(),
            source: e,
        })?;
    Ok(socket)
}

/// The TX RF header's `RPT1`/`RPT2` pair: `(rpt1, rpt2)`.
///
/// These are the fields that say WHERE a transmission is going. The one piece
/// of real traffic this project owns (the captured XLX458 header pinned in
/// `astar_dstar::dsvt`'s tests) carries `RPT2 = "XRF458 A"` — destination
/// reflector plus module — and `xlxd`'s `DPlus` path gates inbound headers on
/// `IsValidModule(rpt2.GetModule())`, dropping every transmission whose `RPT2`
/// module is blank without telling the client. Both fields are also rendered
/// by receiving radios and reflector dashboards, so a blank pair transmits
/// with no repeater/module identity at all.
///
/// Resolution order:
/// 1. `explicit` — the caller named the reflector ([`DstarConfig::reflector_callsign`]);
/// 2. `host`'s first DNS label, when it has the shape of a reflector callsign
///    (see [`reflector_callsign_from_host`]) — the naming convention every
///    public reflector host list follows;
/// 3. [`BLANK_RPT`] for both, with a warning — the bare-IP case, where
///    inventing a callsign would be a guess.
///
/// Either way the name is translated to the one the reflector answers to on
/// the `DExtra` wire before it is packed — an `XLX836` is `XRF836` there; see
/// [`astar_dstar::dextra_callsign`], which [`repeater_fields`] applies.
fn tx_repeater_fields(explicit: Option<&str>, host: &str, module: u8) -> ([u8; 8], [u8; 8]) {
    let derived = explicit
        .map(str::to_string)
        .or_else(|| reflector_callsign_from_host(host));
    if let Some(call) = derived {
        return repeater_fields(&call, module);
    }
    tracing::warn!(
        host,
        "dstar: could not name the destination reflector, transmitting with blank RPT1/RPT2 — \
         pass DstarConfig::reflector_callsign to fill them"
    );
    (BLANK_RPT, BLANK_RPT)
}

/// The reflector-family prefixes a six-character hostname label may be
/// derived from. Requiring one of these (rather than any three letters) is
/// what lets the suffix be ALPHANUMERIC without turning ordinary hostnames
/// into callsigns: `server.example.com` and `router.example.com` are
/// six-letter labels that are emphatically not reflectors, while `xlxarg`,
/// `xrf757` and `dcs019` all are.
const REFLECTOR_PREFIXES: [&str; 4] = ["XRF", "XLX", "REF", "DCS"];

/// `Some(uppercased label)` when `host`'s first DNS label looks like a
/// reflector callsign — one of [`REFLECTOR_PREFIXES`] followed by three ASCII
/// ALPHANUMERIC characters (`XRF757`, `XLX458`, `REF030`, `DCS019`, and the
/// letter-suffixed reflectors like `XLXARG`, which are ~15% of the published
/// XLX list and used to fall through to blank `RPT1`/`RPT2` here).
///
/// This returns the DIRECTORY name, not the wire name: `xlx836.…` derives
/// `XLX836`, which [`repeater_fields`] then addresses as `XRF836` (see
/// [`astar_dstar::dextra_callsign`]).
///
/// Deliberately narrow: anything else (a bare IP, `example.com`, a hostname
/// that merely contains digits) is left to the blank fallback rather than
/// guessed at.
fn reflector_callsign_from_host(host: &str) -> Option<String> {
    let label = host.split('.').next()?;
    if label.len() != 6 || !label.is_ascii() {
        return None;
    }
    let upper = label.to_ascii_uppercase();
    let (prefix, suffix) = upper.split_at(3);
    if !REFLECTOR_PREFIXES.contains(&prefix) {
        return None;
    }
    suffix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric())
        .then_some(upper)
}

/// Send one already-encoded packet, logging (rather than discarding) a
/// failure.
///
/// Returns whether it went out. Every TX-path send goes through here: a
/// connected UDP socket latches `ECONNREFUSED` from an ICMP port-unreachable,
/// so "the reflector process died under a keyed operator" surfaces as an
/// endless run of failed sends — which used to be completely silent, with
/// `shared.ptt` still reporting a clean unkey afterwards.
fn send_packet(socket: &UdpSocket, bytes: &[u8], what: &str) -> bool {
    match socket.send(bytes) {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, len = bytes.len(), "dstar: failed to send {what}");
            false
        }
    }
}

/// Send the terminating (EOT) frame, with bounded retries.
///
/// Unlike every other TX send, this one frame is what tells the far end the
/// stream is over: a relay or reflector that already forwarded the header
/// holds an unterminated stream until its own idle timeout otherwise. Bounded
/// by [`EOT_SEND_ATTEMPTS`] × [`EOT_RETRY_DELAY`] (15 ms worst case) because
/// this runs inline on the run-loop thread during unkey, which must stay
/// bounded — and it fails LOUDLY (`error!`) rather than silently.
fn send_terminating_frame(socket: &UdpSocket, bytes: &[u8]) {
    for attempt in 1..=EOT_SEND_ATTEMPTS {
        if send_packet(socket, bytes, "the terminating (EOT) voice frame") {
            return;
        }
        if attempt < EOT_SEND_ATTEMPTS {
            std::thread::sleep(EOT_RETRY_DELAY);
        }
    }
    tracing::error!(
        "dstar: the terminating (EOT) frame could NOT be sent after {EOT_SEND_ATTEMPTS} attempts \
         — the far end may hold this stream open until its own idle timeout"
    );
}

/// Everything [`run_loop`] needs, bundled to keep the spawn call's arity
/// sane (`clippy::too_many_arguments`).
struct RunLoopParams {
    socket: UdpSocket,
    fsm: DextraFsm,
    ambe: Box<dyn AmbeStream>,
    /// The lane's two channel ends. Its `preroll_lead` cell is carried but
    /// never read: it tells a media-clock ladder how many frames the VOX
    /// pre-roll flush put ahead of the live stream, and D-Star has no ladder
    /// — `TxStream` owns a plain per-transmission sequence counter.
    call_audio: CallAudio,
    /// This session's own TX header (`my`/`rpt1`/`rpt2`/`ur`/`suffix`),
    /// built once at connect time — see [`DstarSession::connect_inner`].
    header: RfHeader,
    shutdown: Arc<AtomicBool>,
    ptt_request: Arc<AtomicBool>,
    shared: Arc<SharedState>,
}

/// Per-transmission TX bookkeeping (iax-2f6b): `Some` from the moment the
/// header packet is sent (key-down) until the terminating EOT frame is sent
/// (key-up, or the shutdown/Drop path — see [`unlink_flushing_eot_if_keyed`]).
/// Owns [`astar_dstar::tx::TxStream`] (stream id + seq-wrap + slow-data
/// framing, iax-2f6b's TX-framing module) plus the mic-frame queue a session
/// needs on top of it — mirrors [`crate::m17::TxState`]'s role.
struct TxState {
    stream: Option<TxStream>,
    /// Captured mic PCM frames that arrived but haven't been submitted to
    /// the encoder yet. Mirrors RX's `pending` queue (see the module docs'
    /// "Pipelined AMBE decode" section) but for the encode direction.
    pending_pcm: VecDeque<[i16; 160]>,
    /// Encoded AMBE frames waiting for their 20 ms send slot — the pacing
    /// buffer that turns "whatever the encoder finished since the last
    /// run-loop pass" into the isochronous cadence the framing layer
    /// requires (see [`TX_FRAME_INTERVAL`]).
    ready: VecDeque<[u8; 9]>,
    /// When the next paced voice frame may go out. `None` before the first
    /// frame of a transmission (which goes out as soon as it is ready).
    next_due: Option<Instant>,
}

impl TxState {
    fn new() -> Self {
        TxState {
            stream: None,
            pending_pcm: VecDeque::new(),
            ready: VecDeque::new(),
            next_due: None,
        }
    }

    /// `true` when anything is queued anywhere on the TX path — the run
    /// loop's cue to keep polling fast rather than sitting in the 50 ms idle
    /// socket read.
    fn is_busy(&self) -> bool {
        !self.pending_pcm.is_empty() || !self.ready.is_empty()
    }
}

/// Everything a TX-side run-loop step needs, bundled so
/// [`apply_ptt_edge`]/[`drain_tx_mic_frames`]/[`pump_tx`] stay readable
/// (`clippy::too_many_arguments`). Constructed fresh in each run-loop pass
/// from borrows of the loop's own locals — mirrors [`RxState`]'s role on the
/// decode side.
struct TxCtx<'a> {
    socket: &'a UdpSocket,
    ambe: &'a mut dyn AmbeStream,
    call_audio: &'a CallAudio,
    shared: &'a SharedState,
    /// This session's own TX header — see [`RunLoopParams::header`].
    header: RfHeader,
}

/// Why the run loop is unkeying without the operator having asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForcedUnkey {
    /// The reflector link left [`LinkState::Linked`] mid-transmission (30 s
    /// keepalive timeout, or a reflector-sent `DISCONNECTED`).
    LinkLost(LinkState),
    /// [`MAX_TX_DURATION`] elapsed — the time-out timer.
    Timeout,
}

/// What the run loop should do about PTT on this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttAction {
    /// Nothing to do (state already matches the request, or a key-down is
    /// latched off and has already been reported).
    None,
    Key,
    Unkey,
    /// Unkey now, for a reason the operator did not ask for.
    Forced(ForcedUnkey),
    /// A key-down that cannot be honoured; report it once (the latch below
    /// stops it repeating every 2-50 ms until PTT is released).
    Refused(LinkState),
}

/// All of the session's PTT policy, in one place and free of I/O so it can be
/// unit-tested against fabricated clocks and link states (see this module's
/// `tx_tests`) instead of only through a 5-minute real transmission.
///
/// The rules, in priority order:
///
/// 1. releasing PTT always re-arms (clears `blocked`);
/// 2. while keyed, losing [`LinkState::Linked`] or exceeding
///    [`MAX_TX_DURATION`] forces an unkey and latches PTT off;
/// 3. a key-down is refused (and latched off) unless the link is `Linked`;
/// 4. otherwise the request is applied.
///
/// The latch is what makes this safe against a stuck-down PTT: after any
/// forced unkey or refusal, nothing re-keys until `ptt_request` has actually
/// gone false again.
struct PttGate {
    keyed: bool,
    /// Key-down is refused until PTT is released — set by every forced unkey
    /// and every refusal.
    blocked: bool,
    /// When the current transmission started (the TOT clock).
    keyed_since: Option<Instant>,
}

impl PttGate {
    fn new() -> PttGate {
        PttGate {
            keyed: false,
            blocked: false,
            keyed_since: None,
        }
    }

    fn is_keyed(&self) -> bool {
        self.keyed
    }

    /// Decide what to do this pass. Pure apart from the latch/clock
    /// bookkeeping it owns; the caller performs the resulting action and
    /// reports the outcome back through [`PttGate::applied`].
    fn decide(&mut self, want_key: bool, link: LinkState, now: Instant) -> PttAction {
        if !want_key {
            // Release always re-arms, whatever latched it off.
            self.blocked = false;
        }
        if self.keyed {
            if link != LinkState::Linked {
                self.blocked = true;
                return PttAction::Forced(ForcedUnkey::LinkLost(link));
            }
            if self
                .keyed_since
                .is_some_and(|t| now.duration_since(t) >= MAX_TX_DURATION)
            {
                self.blocked = true;
                return PttAction::Forced(ForcedUnkey::Timeout);
            }
            return if want_key {
                PttAction::None
            } else {
                PttAction::Unkey
            };
        }
        if want_key && !self.blocked {
            if link == LinkState::Linked {
                return PttAction::Key;
            }
            self.blocked = true;
            return PttAction::Refused(link);
        }
        PttAction::None
    }

    /// Record the state an attempted key/unkey actually landed in. A key-down
    /// that came back `false` was refused downstream (the RF header could not
    /// be sent) — latch PTT off exactly as an up-front refusal would.
    fn applied(&mut self, keyed: bool, now: Instant) {
        // The TOT clock starts at the key-down that actually took effect and
        // is cleared by any unkey; re-reporting the same keyed state (which
        // the run loop never does, but which is cheap to be robust about)
        // must not restart it.
        self.keyed_since = if keyed {
            self.keyed_since.or(Some(now))
        } else {
            None
        };
        self.keyed = keyed;
    }

    /// Latch PTT off until the operator releases it.
    fn block(&mut self) {
        self.blocked = true;
    }
}

/// Per-transmission bookkeeping threaded through [`poll_socket`]/
/// [`handle_dsvt`]: which DSVT `stream_id` is currently attributed to
/// `talker`/`slow_rx` (see the module docs' "Talker / slow-data tracking"
/// section), plus how many of its voice frames have been submitted to the
/// [`AmbeStream`] pipeline so far — the pipeline-priming cushion (see
/// [`AMBE_STREAM_PRIME_FRAMES`]).
struct StreamTracker {
    /// `Some` once a `Header` has been seen for the in-progress stream;
    /// `None` before the first `Header` or after that stream's `end`.
    current_stream: Option<u16>,
    /// Voice frames ACCEPTED by the pipeline since `current_stream` was last
    /// set, capped at [`AMBE_STREAM_PRIME_FRAMES`] (never needs to count
    /// higher — [`Self::is_primed`] is the only thing that reads it).
    /// Deliberately counts accepted submissions, not arrivals: counting
    /// arrivals would let frames the vocoder refused satisfy the cushion the
    /// vocoder is supposed to have built up.
    primed_frames: u8,
    /// When the most recent voice frame for `current_stream` arrived — the
    /// abandoned-stream guard, see [`STREAM_IDLE_TIMEOUT`].
    last_voice: Option<Instant>,
}

impl StreamTracker {
    fn new() -> Self {
        StreamTracker {
            current_stream: None,
            primed_frames: 0,
            last_voice: None,
        }
    }

    /// A fresh `Header` starts a new transmission: track its `stream_id` and
    /// reset the priming counter for it.
    fn start(&mut self, stream_id: u16, now: Instant) {
        self.current_stream = Some(stream_id);
        self.primed_frames = 0;
        self.last_voice = Some(now);
    }

    /// `true` once [`AMBE_STREAM_PRIME_FRAMES`] frames have been accepted by
    /// the pipeline for the current stream — from here on, every pump polls
    /// as well as submits.
    fn is_primed(&self) -> bool {
        self.primed_frames >= AMBE_STREAM_PRIME_FRAMES
    }

    /// Record one frame accepted during the priming window. A no-op once
    /// already primed (the counter never needs to grow past the threshold).
    fn note_frame_submitted(&mut self) {
        if !self.is_primed() {
            self.primed_frames += 1;
        }
    }

    /// Record that a voice frame for the current stream just arrived.
    fn note_voice_arrival(&mut self, now: Instant) {
        self.last_voice = Some(now);
    }

    /// `true` when a stream is being tracked but has gone quiet for longer
    /// than [`STREAM_IDLE_TIMEOUT`] — its `end` frame was lost, or the
    /// talker's link dropped. See that constant's doc.
    fn is_abandoned(&self, now: Instant) -> bool {
        self.current_stream.is_some()
            && self
                .last_voice
                .is_some_and(|t| now.duration_since(t) > STREAM_IDLE_TIMEOUT)
    }

    /// The stream ended (or is being abandoned): clear tracking so the next
    /// `Header` starts a clean priming window.
    fn end(&mut self) {
        self.current_stream = None;
        self.primed_frames = 0;
        self.last_voice = None;
    }
}

/// Everything the RX side of the run loop threads through
/// [`poll_socket`]/[`handle_dsvt`]/[`pump`], bundled so those signatures stay
/// readable (`clippy::too_many_arguments`).
struct RxState<'a> {
    ambe: &'a mut dyn AmbeStream,
    /// Arrived-but-not-yet-submitted 9-byte channel frames. See the module
    /// docs' "Pipelined AMBE decode" section for why this queue exists.
    pending: &'a mut VecDeque<[u8; 9]>,
    tracker: &'a mut StreamTracker,
    slow_rx: &'a mut SlowDataRx,
    call_audio: &'a CallAudio,
    shared: &'a SharedState,
    /// Consulted by [`flush_pipeline`] so a teardown request is observed
    /// even mid-flush.
    shutdown: &'a AtomicBool,
}

/// Feed the vocoder from `pending` while it has room, then drain every
/// decoded frame it has ready. Called on each voice-frame arrival AND once
/// per run-loop pass, so a backlog keeps moving even when packets stop
/// arriving.
///
/// Nothing here blocks: `submit_decode` is guarded by the vocoder's own
/// in-flight bound so it is always accepted rather than dropped, and
/// `poll_decoded` is non-blocking by contract.
fn pump(rx: &mut RxState<'_>) {
    while rx.ambe.in_flight() < AMBE_STREAM_MAX_IN_FLIGHT {
        let Some(frame) = rx.pending.pop_front() else {
            break;
        };
        rx.ambe.submit_decode(frame.into());
        rx.tracker.note_frame_submitted();
    }
    if rx.tracker.is_primed() {
        while let Some(pcm) = rx.ambe.poll_decoded() {
            let _ = rx.call_audio.rx_frames.send(pcm.to_vec());
        }
    }
}

/// Empty the whole decode path — the queued raw frames AND everything still
/// in flight in the vocoder — and either forward the decoded audio or throw
/// it away.
///
/// `forward` picks which:
///
/// - `true` at end-of-stream (and for an abandoned stream): the tail belongs
///   to the talker whose transmission just finished, so play it. Without
///   this the last few frames — still working through the pipeline when the
///   `end`-marked frame arrives — would never be polled at all, clipping the
///   tail (and clipping a whole transmission shorter than the priming
///   cushion).
/// - `false` when a fresh `Header` arrives: the residue belongs to the
///   PREVIOUS talker. Playing it now would emit talker A's audio under
///   talker B's callsign, which is worse than losing it.
///
/// Always terminates: bounded by [`FLUSH_DEADLINE`], and by the shutdown
/// flag. See [`FLUSH_DEADLINE`]'s doc for why an unbounded version deadlocks
/// `disconnect()`/`Drop`.
fn flush_pipeline(rx: &mut RxState<'_>, forward: bool) {
    let deadline = Instant::now() + FLUSH_DEADLINE;
    loop {
        while rx.ambe.in_flight() < AMBE_STREAM_MAX_IN_FLIGHT {
            let Some(frame) = rx.pending.pop_front() else {
                break;
            };
            rx.ambe.submit_decode(frame.into());
        }
        let mut delivered = false;
        while let Some(pcm) = rx.ambe.poll_decoded() {
            delivered = true;
            if forward {
                let _ = rx.call_audio.rx_frames.send(pcm.to_vec());
            }
        }
        if rx.pending.is_empty() && rx.ambe.in_flight() == 0 {
            return;
        }
        if rx.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                pending = rx.pending.len(),
                in_flight = rx.ambe.in_flight(),
                "dstar: vocoder flush hit its {FLUSH_DEADLINE:?} deadline, abandoning the rest"
            );
            break;
        }
        if !delivered {
            // Still outstanding but nothing ready yet: a short sleep instead
            // of a tight spin.
            std::thread::sleep(DRAIN_POLL_INTERVAL);
        }
    }
    rx.pending.clear();
}

/// Convert one TX frame (`StreamConfig::default()` guarantees 160 `i16`
/// samples per frame while keyed, matching AMBE's full-rate D-Star frame
/// size) into the fixed-size array the encoder wants. `None` on an
/// unexpected length (defensive only — should not happen given the router's
/// frame chunking; mirrors [`crate::m17`]'s own `frame_to_array`).
fn frame_to_array(v: &[i16]) -> Option<[i16; 160]> {
    if v.len() != 160 {
        return None;
    }
    let mut a = [0i16; 160];
    a.copy_from_slice(v);
    Some(a)
}

/// Run-loop TX step: drain whatever mic PCM frames the router's mic lane has
/// already queued into `tx.pending_pcm`, bounded by
/// [`MAX_PENDING_TX_FRAMES`] (overflow drops the newest frame, mirroring RX's
/// own drop rule). Only meaningful while keyed — the mic lane's gate stops
/// enqueuing anything the instant it's unkeyed, so calling this while
/// unkeyed just drains whatever the gate had already let through before
/// closing.
fn drain_tx_mic_frames(ctx: &TxCtx<'_>, tx: &mut TxState) {
    while let Ok(frame) = ctx.call_audio.tx_frames.try_recv() {
        let Some(pcm) = frame_to_array(&frame) else {
            continue;
        };
        if tx.pending_pcm.len() >= MAX_PENDING_TX_FRAMES {
            tracing::warn!(
                "dstar: {MAX_PENDING_TX_FRAMES} unencoded TX frames already queued, dropping newest"
            );
        } else {
            tx.pending_pcm.push_back(pcm);
        }
    }
}

/// Run-loop TX step, called every pass while keyed: feed the encoder from
/// `tx.pending_pcm` while it has room, then send every encoded frame it has
/// ready as an ordinary (`end = false`) DSVT voice frame. Mirrors RX's
/// [`pump`] exactly, but for the encode direction and network `send` instead
/// of decode and `rx_frames.send`.
///
/// Never blocks: `submit_encode` is guarded by the encoder's own in-flight
/// bound, and `poll_encoded` is non-blocking by contract. A `None` `tx.stream`
/// (not currently keyed — should not happen given this is only ever called
/// while keyed, but defensive) simply drops whatever came back rather than
/// panicking.
fn pump_tx(ctx: &mut TxCtx<'_>, tx: &mut TxState) {
    while ctx.ambe.in_flight_encoded() < AMBE_STREAM_MAX_IN_FLIGHT {
        let Some(pcm) = tx.pending_pcm.pop_front() else {
            break;
        };
        ctx.ambe.submit_encode(pcm);
    }
    while let Some(frame) = ctx.ambe.poll_encoded() {
        // This session's stream is opened in `VocoderMode::Dstar`, so a
        // frame of any other width cannot arrive — but substituting the null
        // codeword beats truncating something that is not a D-Star frame onto
        // the air.
        tx.ready.push_back(frame.as_dstar().unwrap_or_else(|| {
            tracing::warn!("dstar: encoder returned a non-D-Star frame, substituting silence");
            astar_dstar::NULL_AMBE
        }));
    }
    send_paced_voice_frames(ctx, tx, Instant::now());
}

/// Emit whatever encoded frames are due, one per [`TX_FRAME_INTERVAL`].
///
/// The 20 ms schedule is absolute, not "20 ms since the last pass", so a late
/// run-loop pass catches up (sending the frames it owes) without drifting the
/// average rate — and a schedule that has fallen more than
/// [`MAX_TX_PACING_BACKLOG`] frames behind is abandoned rather than paying
/// that latency for the rest of the transmission (a capture clock running
/// slightly fast against the run-loop's clock, or a mic lane that delivered a
/// long block in one callback).
fn send_paced_voice_frames(ctx: &mut TxCtx<'_>, tx: &mut TxState, now: Instant) {
    let Some(stream) = tx.stream.as_mut() else {
        // Not keyed (this only runs while keyed — but dropping the frames
        // beats transmitting under a stream that was never opened).
        if !tx.ready.is_empty() {
            tracing::warn!("dstar: encoded frames with no open TX stream, dropping");
            tx.ready.clear();
        }
        return;
    };
    // Bleed off any surplus beyond the pacing window first (see
    // MAX_TX_PACING_BACKLOG): those frames are already late, and holding them
    // back would only add latency, not cadence.
    if tx.ready.len() > MAX_TX_PACING_BACKLOG {
        tracing::debug!(
            backlog = tx.ready.len(),
            "dstar: TX pacing backlog exceeded, releasing the surplus"
        );
        while tx.ready.len() > MAX_TX_PACING_BACKLOG {
            let frame = tx.ready.pop_front().expect("checked non-empty");
            let pkt = stream.next_voice_frame(frame, false);
            send_packet(ctx.socket, &pkt.encode(), "a DSVT voice frame");
        }
        tx.next_due = Some(now + TX_FRAME_INTERVAL);
    }
    let mut due = tx.next_due.unwrap_or(now);
    while now >= due {
        let Some(frame) = tx.ready.pop_front() else {
            break;
        };
        let pkt = stream.next_voice_frame(frame, false);
        send_packet(ctx.socket, &pkt.encode(), "a DSVT voice frame");
        due += TX_FRAME_INTERVAL;
    }
    // Never bank send credit while there is nothing to send: an idle pacer
    // restarts from `now`, so a burst arriving after a drought is still
    // released on the 20 ms schedule rather than all at once.
    if tx.ready.is_empty() && due < now {
        due = now;
    }
    tx.next_due = Some(due);
}

/// Run-loop step, called every pass while keyed: builds a [`TxCtx`] and
/// drains+pumps the mic/encoder. Split out of [`run_loop`] purely to keep
/// that function under clippy's line-count limit — see [`drain_tx_mic_frames`]/
/// [`pump_tx`] for the actual behavior.
fn run_tx_pump_step(
    socket: &UdpSocket,
    ambe: &mut dyn AmbeStream,
    call_audio: &CallAudio,
    shared: &SharedState,
    header: RfHeader,
    tx: &mut TxState,
) {
    let mut ctx = TxCtx {
        socket,
        ambe,
        call_audio,
        shared,
        header,
    };
    drain_tx_mic_frames(&ctx, tx);
    pump_tx(&mut ctx, tx);
}

/// Empty the whole TX encode path — everything still queued in
/// `pending`/`pending_pcm` AND everything still in flight in the encoder —
/// and return every encoded frame it produced, in submission order.
///
/// Always terminates: bounded by [`FLUSH_DEADLINE`] exactly like RX's
/// [`flush_pipeline`] (see that constant's doc) — a wedged encoder still
/// returns whatever it managed to produce before the deadline rather than
/// spinning forever, which is what makes [`apply_ptt_edge`]'s unkey branch
/// able to promise an UNCONDITIONAL terminating frame (see the module docs'
/// TX-safety section): the deadline bounds how long unkey can take, never
/// whether it emits an EOT frame at all.
fn flush_tx_pipeline(
    ambe: &mut dyn AmbeStream,
    pending: &mut VecDeque<[i16; 160]>,
) -> Vec<[u8; 9]> {
    let deadline = Instant::now() + FLUSH_DEADLINE;
    let mut out = Vec::new();
    loop {
        while ambe.in_flight_encoded() < AMBE_STREAM_MAX_IN_FLIGHT {
            let Some(pcm) = pending.pop_front() else {
                break;
            };
            ambe.submit_encode(pcm);
        }
        let mut delivered = false;
        while let Some(frame) = ambe.poll_encoded() {
            delivered = true;
            out.push(frame.as_dstar().unwrap_or_else(|| {
                tracing::warn!("dstar: encoder returned a non-D-Star frame, substituting silence");
                astar_dstar::NULL_AMBE
            }));
        }
        if pending.is_empty() && ambe.in_flight_encoded() == 0 {
            return out;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                pending = pending.len(),
                in_flight = ambe.in_flight_encoded(),
                "dstar: TX encoder flush hit its {FLUSH_DEADLINE:?} deadline, unkeying with what \
                 encoded so far"
            );
            pending.clear();
            return out;
        }
        if !delivered {
            // Still outstanding but nothing ready yet: a short sleep instead
            // of a tight spin — see flush_pipeline's identical comment.
            std::thread::sleep(DRAIN_POLL_INTERVAL);
        }
    }
}

/// Run-loop step: apply a pending PTT edge. Returns the keyed state that was
/// actually reached — which for a key-down is NOT always `want_key`: see the
/// refusal cases below. [`crate::m17::apply_ptt_edge`] is the M17 precedent
/// this mirrors.
///
/// The lane's gate is NOT touched here — `ConsoleSession::set_ptt` opened it
/// before forwarding the key and closes it on key-up; this is the protocol
/// edge only.
///
/// Key-down, in order:
///
/// 1. clears this session's OWN queues (`pending_pcm`, `ready`, the pacer)
///    and DRAINS THE ENCODER's output queue. A previous unkey that hit
///    [`FLUSH_DEADLINE`] with frames still in flight leaves their responses
///    to land afterwards, unpolled; without this drain the first voice
///    frames of the NEW stream carry the PREVIOUS transmission's audio (and
///    the whole over runs a permanent 4-frame lag). RX has always been
///    guarded against exactly this — [`handle_dsvt`] flushes on a fresh
///    header — this is the TX twin.
///
///    It does NOT drain `call_audio.tx_frames`: the gate opens up to one
///    [`SOCKET_POLL_TIMEOUT`] tick BEFORE this edge is observed, so by now
///    that channel holds the VOX pre-roll ring (flushed by the mic lane's
///    own false→true gate edge) and the speech onset that followed it.
///    Draining here would throw both away. Nothing stale can be in it
///    either: the lane emits nothing while the gate is closed, and
///    [`run_loop`] drains and drops on every pass it is not transmitting;
/// 2. starts a fresh [`TxStream`] under a freshly generated
///    [`generate_stream_id`] and sends its header packet — refusing the
///    key-down if that send fails, rather than transmitting voice under a
///    stream id no receiver ever saw a header for.
///
/// Key-up (and the shutdown/Drop path via [`unlink_flushing_eot_if_keyed`]):
/// immediately publishes `ptt = false` (the transmission is over as far as
/// capture is concerned; a UI must not keep showing "keyed" for the duration
/// of the flush below), THEN drains + encodes whatever mic audio was already
/// captured ([`drain_tx_mic_frames`] + [`flush_tx_pipeline`]), THEN sends
/// every resulting frame in order, with the LAST one always carrying the
/// `end` bit — a bare [`NULL_AMBE`] EOT frame if nothing was ever queued at
/// all. This is UNCONDITIONAL: however this branch is reached, and however
/// far the flush actually got, it always ends by sending exactly one
/// terminating DSVT voice frame (with retries, see [`send_terminating_frame`])
/// — see the module docs' TX-safety section.
fn apply_ptt_edge(ctx: &mut TxCtx<'_>, tx: &mut TxState, want_key: bool) -> bool {
    if want_key {
        key_down(ctx, tx)
    } else {
        key_up(ctx, tx);
        false
    }
}

/// [`apply_ptt_edge`]'s key-down branch — see its doc for the ordering and
/// the two refusal cases (returns `false` for either).
fn key_down(ctx: &mut TxCtx<'_>, tx: &mut TxState) -> bool {
    tx.pending_pcm.clear();
    tx.ready.clear();
    tx.next_due = None;
    let mut stale = 0usize;
    while ctx.ambe.poll_encoded().is_some() {
        stale += 1;
    }
    if stale > 0 {
        tracing::warn!(
            "dstar: discarded {stale} stale encoded frame(s) left over from the previous \
             transmission's truncated flush"
        );
    }
    let stream = TxStream::new(generate_stream_id());
    if !send_packet(
        ctx.socket,
        &stream.header_packet(ctx.header).encode(),
        "the RF header packet",
    ) {
        tracing::error!(
            "dstar: refusing to key — the RF header could not be sent, so no receiver would \
             attribute this stream"
        );
        return false;
    }
    tx.stream = Some(stream);
    ctx.shared.ptt.store(true, Ordering::Relaxed);
    true
}

/// [`apply_ptt_edge`]'s key-up branch — see its doc.
fn key_up(ctx: &mut TxCtx<'_>, tx: &mut TxState) {
    ctx.shared.ptt.store(false, Ordering::Relaxed);
    drain_tx_mic_frames(ctx, tx);
    let mut encoded: Vec<[u8; 9]> = tx.ready.drain(..).collect();
    encoded.extend(flush_tx_pipeline(ctx.ambe, &mut tx.pending_pcm));
    tx.next_due = None;
    let Some(mut stream) = tx.stream.take() else {
        // No stream was ever opened, so there is nothing to terminate:
        // fabricating one here would put an orphan EOT frame — under a
        // stream id no header ever announced — on the wire, which no
        // reflector acts on (they look streams up by id, exactly as this
        // module's own RX does). Report the broken invariant instead.
        tracing::warn!(
            "dstar: unkey with no open TX stream — nothing to terminate (this should not happen)"
        );
        return;
    };
    if encoded.is_empty() {
        // Not all-zero: an all-zero AMBE payload is not silence to a
        // decoder, it is a frame whose voice parameters are all zero (see
        // NULL_AMBE).
        send_terminating_frame(ctx.socket, &stream.unkey(NULL_AMBE).encode());
        return;
    }
    let last = encoded.len() - 1;
    for (i, ambe) in encoded.into_iter().enumerate() {
        if i == last {
            send_terminating_frame(ctx.socket, &stream.unkey(ambe).encode());
        } else {
            send_packet(
                ctx.socket,
                &stream.next_voice_frame(ambe, false).encode(),
                "a flushed DSVT voice frame",
            );
        }
    }
}

/// Run-loop step: apply a pending PTT edge, if `want_key` differs from
/// `keyed`. Split out of [`run_loop`] to keep that function under clippy's
/// line-count limit (`clippy::too_many_lines`) — see [`apply_ptt_edge`] for
/// the actual key/unkey behavior this wraps.
///
/// On a key-DOWN edge specifically, this ALSO performs the half-duplex
/// handoff (see the module docs): flushes/discards whatever RX audio is
/// still queued or in flight in `pending`/`tracker`/`ambe` BEFORE
/// [`apply_ptt_edge`] ever submits the first mic frame to the encoder — the
/// `ThumbDV` is one physical link, and this session never decodes and
/// encodes at once.
#[allow(clippy::too_many_arguments)]
fn apply_pending_ptt_edge(
    socket: &UdpSocket,
    ambe: &mut dyn AmbeStream,
    call_audio: &CallAudio,
    shared: &SharedState,
    header: RfHeader,
    shutdown: &AtomicBool,
    pending: &mut VecDeque<[u8; 9]>,
    tracker: &mut StreamTracker,
    slow_rx: &mut SlowDataRx,
    tx: &mut TxState,
    want_key: bool,
) -> bool {
    let keyed = {
        let mut ctx = TxCtx {
            socket,
            ambe,
            call_audio,
            shared,
            header,
        };
        apply_ptt_edge(&mut ctx, tx, want_key)
    };
    if keyed {
        // Half-duplex handoff, AFTER the header has gone out: this flush can
        // sleep in DRAIN_POLL_INTERVAL steps up to FLUSH_DEADLINE against a
        // slow/wedged vocoder, and anything the operator says during that
        // window keeps accumulating in the lane's channel (the gate is
        // already open — `ConsoleSession::set_ptt` opened it) rather than
        // being lost. Nothing is submitted to the ENCODER until the run
        // loop's next `run_tx_pump_step`, which happens after this returns —
        // so the one physical ThumbDV link still never decodes and encodes at
        // the same time.
        let mut rx = RxState {
            ambe,
            pending,
            tracker,
            slow_rx,
            call_audio,
            shared,
            shutdown,
        };
        flush_pipeline(&mut rx, true);
        rx.tracker.end();
    }
    keyed
}

/// Run-loop shutdown branch: if still `keyed`, flush the SAME unkey path a
/// normal key-up would — reusing [`apply_ptt_edge`]'s unkey branch — BEFORE
/// sending the unlink. Without this, disconnecting while keyed would leave
/// the reflector's own stream open (no EOT bit ever seen) until ITS OWN
/// stream-idle timeout closed it — mirrors
/// [`crate::m17::send_disc_flushing_eos_if_keyed`] exactly, adapted to
/// D-Star's unlink instead of M17's `DISC`. [`DstarSession::disconnect`] and
/// `Drop` both route through this same shutdown branch (see the module
/// docs), so both are covered by the same guarantee.
fn unlink_flushing_eot_if_keyed(
    ctx: &mut TxCtx<'_>,
    tx: &mut TxState,
    fsm: &mut DextraFsm,
    keyed: bool,
) {
    if keyed {
        let _ = apply_ptt_edge(ctx, tx, false);
    }
    let unlink_bytes = fsm.unlink(Instant::now());
    send_packet(ctx.socket, &unlink_bytes, "the unlink packet");
}

/// Run-loop step: decide what to do about PTT this pass ([`PttGate`]),
/// perform it, and report the outcome back to the gate. Split out of
/// [`run_loop`] to keep that function under clippy's line-count limit; the
/// policy itself (and why each rule exists) lives on [`PttGate`].
#[allow(clippy::too_many_arguments)]
fn run_ptt_step(
    socket: &UdpSocket,
    ambe: &mut dyn AmbeStream,
    call_audio: &CallAudio,
    shared: &SharedState,
    header: RfHeader,
    shutdown: &AtomicBool,
    pending: &mut VecDeque<[u8; 9]>,
    tracker: &mut StreamTracker,
    slow_rx: &mut SlowDataRx,
    tx: &mut TxState,
    ptt: &mut PttGate,
    want_key: bool,
    link: LinkState,
) {
    let action = ptt.decide(want_key, link, Instant::now());
    match action {
        PttAction::None => return,
        PttAction::Refused(link) => {
            tracing::warn!(
                ?link,
                "dstar: refusing PTT — the reflector link is not up; release and re-key once it is"
            );
            return;
        }
        PttAction::Forced(ForcedUnkey::LinkLost(link)) => {
            tracing::warn!(
                ?link,
                "dstar: reflector link lost while transmitting — unkeying"
            );
        }
        PttAction::Forced(ForcedUnkey::Timeout) => {
            tracing::warn!(
                "dstar: transmission exceeded the {MAX_TX_DURATION:?} time-out timer — unkeying \
                 (release PTT to re-arm)"
            );
        }
        PttAction::Key | PttAction::Unkey => {}
    }
    let key = action == PttAction::Key;
    let keyed = apply_pending_ptt_edge(
        socket, ambe, call_audio, shared, header, shutdown, pending, tracker, slow_rx, tx, key,
    );
    if key && !keyed {
        // Refused downstream (the RF header could not be sent): latch PTT off
        // until it is released, rather than retrying — and failing — every
        // pass.
        ptt.block();
    }
    ptt.applied(keyed, Instant::now());
}

/// The "iax-dstar" run-loop: the ONE thread that owns the socket, the
/// [`CallAudio`] channel ends and the [`AmbeStream`] pipeline for this
/// session. Single poll cadence (the socket's 50 ms read timeout) drives
/// everything: PTT edges, TX framing, RX decode/forward, and the FSM's
/// keepalive tick. No router, no gate, no meters — those are the lane's, and
/// `ConsoleSession` reads and drives them there.
fn run_loop(p: RunLoopParams) {
    let RunLoopParams {
        socket,
        mut fsm,
        mut ambe,
        call_audio,
        header,
        shutdown,
        ptt_request,
        shared,
    } = p;

    let now = Instant::now();
    let connect_bytes = fsm.connect(now);
    send_packet(&socket, &connect_bytes, "the link (connect) request");
    shared
        .link
        .store(link_to_u8(fsm.state()), Ordering::Relaxed);

    let mut buf = [0u8; 2_048];
    let mut tracker = StreamTracker::new();
    let mut slow_rx = SlowDataRx::new();
    let mut pending: VecDeque<[u8; 9]> = VecDeque::new();
    let mut fast_socket = false;
    let mut ptt = PttGate::new();
    let mut tx = TxState::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            let mut ctx = TxCtx {
                socket: &socket,
                ambe: ambe.as_mut(),
                call_audio: &call_audio,
                shared: &shared,
                header,
            };
            unlink_flushing_eot_if_keyed(&mut ctx, &mut tx, &mut fsm, ptt.is_keyed());
            break;
        }

        // Apply a pending PTT edge (set_ptt only requests; this is where it
        // actually takes effect), plus the forced-unkey rules PttGate owns
        // (link lost mid-transmission, time-out timer, refusals).
        run_ptt_step(
            &socket,
            ambe.as_mut(),
            &call_audio,
            &shared,
            header,
            &shutdown,
            &mut pending,
            &mut tracker,
            &mut slow_rx,
            &mut tx,
            &mut ptt,
            ptt_request.load(Ordering::Relaxed),
            fsm.state(),
        );
        let keyed = ptt.is_keyed();

        // Poll fast whenever anything is waiting on either direction: an RX
        // backlog the vocoder still owes us, or ANY transmit activity — a
        // keyed session must come back around on the ~2 ms cadence its 20 ms
        // TX pacer needs, and `pending` is empty by construction while keyed
        // (inbound DSVT is dropped, see `poll_socket`), so it can never speak
        // for the TX side.
        let want_fast = !pending.is_empty() || keyed || tx.is_busy();
        if want_fast != fast_socket {
            let timeout = if want_fast {
                BACKLOG_POLL_TIMEOUT
            } else {
                SOCKET_POLL_TIMEOUT
            };
            let _ = socket.set_read_timeout(Some(timeout));
            fast_socket = want_fast;
        }

        run_rx_poll_step(
            &socket,
            &mut buf,
            &mut fsm,
            ambe.as_mut(),
            &mut pending,
            &mut tracker,
            &mut slow_rx,
            &call_audio,
            &shared,
            &shutdown,
            keyed,
        );

        if keyed {
            run_tx_pump_step(
                &socket,
                ambe.as_mut(),
                &call_audio,
                &shared,
                header,
                &mut tx,
            );
        } else {
            // Not transmitting: drain and DROP. This is the ONLY discard on
            // the TX path — `key_down` deliberately keeps what it finds. The
            // gate is `ConsoleSession::set_ptt`'s and it can be open while
            // this loop is not keyed: it stays open after a run-loop-forced
            // unkey (link lost, time-out timer) until the operator releases
            // PTT, and without this the lane would pile audio into
            // `tx_frames` unbounded and the next transmission would open with
            // somebody's stale speech.
            //
            // It cannot eat the pre-roll: `set_ptt` stores the PTT request
            // within nanoseconds of opening the gate, whereas the lane's
            // flush waits for its next capture callback (up to a frame, ~20
            // ms) — so any pass that can see flushed frames has already read
            // `ptt_request` as true and taken the `if` arm.
            while call_audio.tx_frames.try_recv().is_ok() {}
        }

        // Keepalive tick: DextraFsm::tick only ever returns
        // FsmAction::None/Timeout (never Send — unlike SessionFsm, a
        // DExtra client answers keepalives from `on_packet`, not `tick`),
        // so there is nothing to send here; `state()` (read below) already
        // reflects a timeout transition either way.
        let _ = fsm.tick(Instant::now());
        shared
            .link
            .store(link_to_u8(fsm.state()), Ordering::Relaxed);
    }
    // `socket` and the `CallAudio` channel ends drop here; the lane's streams
    // belong to the station router and outlive this thread.
}

/// Run-loop step: poll the socket (bounded by [`SOCKET_POLL_TIMEOUT`]) and
/// react to whatever arrived. A buffer starting with the 4-byte `"DSVT"`
/// magic is a voice/header packet — handled by [`handle_dsvt`] — anything
/// else is fed to the [`DextraFsm`] (connect ACK/NAK, keepalive, unlink
/// ack). This split by magic bytes is safe: none of `DextraFsm`'s own
/// wire shapes (11/14/9-byte fixed-length packets, or the literal
/// `"DISCONNECTED"`) can collide with `"DSVT"` at offset 0..4.
///
/// `keyed`: half-duplex mute (see the module docs) — while transmitting,
/// inbound `"DSVT"` traffic (header/voice) is dropped outright rather than
/// handed to [`handle_dsvt`], so it never reaches the decoder while the
/// encoder is using the one physical `ThumbDV` link. Link-level control
/// packets (keepalive/ACK/NAK/unlink) are answered regardless of `keyed` —
/// those never touch the vocoder.
fn poll_socket(
    socket: &UdpSocket,
    buf: &mut [u8],
    fsm: &mut DextraFsm,
    rx: &mut RxState<'_>,
    keyed: bool,
) {
    match socket.recv(buf) {
        Ok(n) => {
            let data = &buf[..n];
            if data.len() >= DSVT_MAGIC.len() && data[..DSVT_MAGIC.len()] == DSVT_MAGIC {
                if keyed {
                    tracing::trace!("dstar: dropping inbound DSVT traffic while transmitting");
                    return;
                }
                match DsvtPacket::parse(data) {
                    Ok(pkt) => handle_dsvt(pkt, rx),
                    Err(e) => {
                        // Nothing to react to for a bad frame — still
                        // dropped — but silently swallowing this would make
                        // a wrong CRC byte order (or any other framing
                        // mismatch) on a first live listen present as dead
                        // silence with zero signal of why. `DsvtError::Header`
                        // calls out an RF-header CRC failure specifically
                        // (the likeliest real-world cause); every other
                        // variant is a length/magic/type-byte mismatch. Debug
                        // level, rate-unlimited: a malformed-DSVT flood is
                        // itself a signal worth seeing, not something to
                        // rate-limit away.
                        tracing::debug!(error = ?e, "dstar: dropping unparsable DSVT packet");
                    }
                }
            } else if let FsmAction::Send(bytes) = fsm.on_packet(data, Instant::now()) {
                send_packet(socket, &bytes, "a link-level reply");
            }
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Err(_) => {}
    }
}

/// Run-loop step, called every pass: builds an [`RxState`], polls the
/// socket, and (while not keyed) keeps the decode pipeline moving/checks for
/// an abandoned stream. Split out of [`run_loop`] purely to keep that
/// function under clippy's line-count limit — see [`poll_socket`]/[`pump`]/
/// [`flush_pipeline`] for the actual behavior; the half-duplex skip while
/// `keyed` is documented on [`poll_socket`]'s own `keyed` parameter.
#[allow(clippy::too_many_arguments)]
fn run_rx_poll_step(
    socket: &UdpSocket,
    buf: &mut [u8],
    fsm: &mut DextraFsm,
    ambe: &mut dyn AmbeStream,
    pending: &mut VecDeque<[u8; 9]>,
    tracker: &mut StreamTracker,
    slow_rx: &mut SlowDataRx,
    call_audio: &CallAudio,
    shared: &SharedState,
    shutdown: &AtomicBool,
    keyed: bool,
) {
    let mut rx = RxState {
        ambe,
        pending,
        tracker,
        slow_rx,
        call_audio,
        shared,
        shutdown,
    };
    poll_socket(socket, buf, fsm, &mut rx, keyed);
    if !keyed {
        // Keep the pipeline moving even when nothing arrived this pass.
        pump(&mut rx);
        // Abandoned stream (its `end` frame was lost, or the talker's link
        // dropped): flush its tail out and reset tracking so the next
        // `Header` starts clean rather than inheriting frames.
        if rx.tracker.is_abandoned(Instant::now()) {
            tracing::debug!("dstar: stream went quiet, flushing and resetting tracking");
            flush_pipeline(&mut rx, true);
            rx.tracker.end();
        }
    }
}

/// Folds one parsed [`DsvtPacket`] into the session's talker/slow-data/audio
/// state — see the module docs' "Talker / slow-data tracking" and "Pipelined
/// AMBE decode" sections for the full policy.
fn handle_dsvt(pkt: DsvtPacket, rx: &mut RxState<'_>) {
    match pkt {
        DsvtPacket::Header { stream_id, header } => {
            // A fresh transmission. Whatever is still queued or in flight
            // belongs to the PREVIOUS talker — routine, because D-Star
            // streams often end without their `end`-flagged frame (last
            // frame lost, talker's link dropped, reflector switching
            // talkers mid-stream). Discard it: playing it out now would emit
            // talker A's audio while the snapshot reports talker B, and
            // would also push B's own first frames past the pipeline's
            // in-flight bound.
            flush_pipeline(rx, false);
            rx.tracker.start(stream_id, Instant::now());
            *rx.slow_rx = SlowDataRx::new();
            *rx.shared.talker.lock().expect("talker mutex") = Some(header.my_callsign());
        }
        DsvtPacket::Voice {
            stream_id,
            seq,
            end,
            ambe: frame,
            slow_data,
        } => {
            if rx.tracker.current_stream != Some(stream_id) {
                // Orphan voice frame: no header seen for this stream (or a
                // stray frame from an abandoned/unrelated one) — drop
                // rather than guess at a talker. Not submitted to the
                // pipeline either, exactly as it was never decoded before.
                return;
            }
            rx.tracker.note_voice_arrival(Instant::now());
            // Queue on receipt and let `pump` feed the device as it has
            // room (spec §2), so a burst of arrivals is absorbed here rather
            // than colliding with the vocoder's in-flight bound — see the
            // module docs.
            if rx.pending.len() >= MAX_PENDING_FRAMES {
                tracing::warn!(
                    "dstar: {MAX_PENDING_FRAMES} undecoded frames already queued, dropping newest"
                );
            } else {
                rx.pending.push_back(frame);
            }
            pump(rx);
            if let Some(text) = rx.slow_rx.feed(seq, &slow_data) {
                *rx.shared.slow_text.lock().expect("slow_text mutex") = Some(text);
            }
            if end {
                // The stream ended: flush whatever's still queued/in flight
                // so the tail isn't clipped (spec §2) BEFORE resetting
                // tracking — `talker`/`slow_text` deliberately persist past
                // this point (last-heard semantics — see the module docs).
                flush_pipeline(rx, true);
                rx.tracker.end();
            }
        }
    }
}

#[cfg(test)]
mod tx_tests {
    //! Isolated unit coverage of the TX run-loop steps (iax-2f6b), mirroring
    //! [`crate::m17`]'s own bottom `#[cfg(test)] mod tests`: no reflector, no
    //! session thread, no real mic/output devices, and — since the session
    //! stopped owning its audio — no router at all: [`apply_ptt_edge`] and
    //! friends are called directly against a hand-built [`CallAudio`] and a
    //! real loopback `UdpSocket` pair, so the exact wire bytes can be
    //! inspected. This is where the error/teardown-path coverage this
    //! milestone calls out lives: the higher-level
    //! `tests/dstar_session_pipeline.rs` suite covers the same guarantees
    //! end-to-end through a real run-loop thread and reflector.

    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// A trivial, always-immediately-ready [`AmbeStream`] double: `encode`
    /// maps a PCM frame's first sample into a 2-byte "AMBE" payload (the
    /// same convention `dstar_session_pipeline.rs`'s `FakeVocoder` uses),
    /// with no latency modelling — `submit_encode` immediately makes the
    /// result available to `poll_encoded`. The decode side is never
    /// exercised by these tests; its methods are inert stubs only to satisfy
    /// the trait.
    struct FakeEncoder {
        queue: VecDeque<[u8; 9]>,
    }

    impl FakeEncoder {
        fn new() -> Self {
            FakeEncoder {
                queue: VecDeque::new(),
            }
        }
    }

    impl AmbeStream for FakeEncoder {
        fn submit_decode(&mut self, _frame: astar_codec::ambe::ChannelFrame) {}
        fn poll_decoded(&mut self) -> Option<[i16; 160]> {
            None
        }
        fn in_flight(&self) -> usize {
            0
        }
        fn submit_encode(&mut self, pcm: [i16; 160]) {
            let b = pcm[0].to_be_bytes();
            self.queue.push_back([b[0], b[1], 0, 0, 0, 0, 0, 0, 0]);
        }
        fn poll_encoded(&mut self) -> Option<astar_codec::ambe::ChannelFrame> {
            self.queue
                .pop_front()
                .map(astar_codec::ambe::ChannelFrame::Dstar)
        }
        fn in_flight_encoded(&self) -> usize {
            self.queue.len()
        }
    }

    /// An [`AmbeStream`] double whose encode side never answers — the
    /// unit-test-level twin of `dstar_session_pipeline.rs`'s
    /// `FakeVocoder::wedged_encode`, for exercising [`flush_tx_pipeline`]'s
    /// deadline directly without a real run-loop thread/timing.
    struct WedgedEncoder {
        in_flight: usize,
    }

    impl AmbeStream for WedgedEncoder {
        fn submit_decode(&mut self, _frame: astar_codec::ambe::ChannelFrame) {}
        fn poll_decoded(&mut self) -> Option<[i16; 160]> {
            None
        }
        fn in_flight(&self) -> usize {
            0
        }
        fn submit_encode(&mut self, _pcm: [i16; 160]) {
            self.in_flight += 1;
        }
        fn poll_encoded(&mut self) -> Option<astar_codec::ambe::ChannelFrame> {
            None
        }
        fn in_flight_encoded(&self) -> usize {
            self.in_flight
        }
    }

    /// Build a [`CallAudio`] by hand — no router involved — so a test can
    /// push raw frames straight onto the exact channel
    /// [`drain_tx_mic_frames`]/[`apply_ptt_edge`] read from. Mirrors
    /// `crate::m17::tests::fake_call_audio` exactly.
    fn fake_call_audio() -> (CallAudio, Sender<Vec<i16>>, Receiver<Vec<i16>>) {
        let (tx_tx, tx_rx) = channel::<Vec<i16>>();
        let (rx_tx, rx_rx) = channel::<Vec<i16>>();
        let call_audio = CallAudio {
            tx_frames: tx_rx,
            rx_frames: rx_tx,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        };
        (call_audio, tx_tx, rx_rx)
    }

    /// A connected loopback `UdpSocket` pair: `send_sock` is what `TxCtx`
    /// sends through; `recv_sock` is where the test reads the exact wire
    /// bytes back from.
    fn loopback_pair() -> (UdpSocket, UdpSocket) {
        let recv_sock = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        recv_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let recv_addr = recv_sock.local_addr().expect("recv addr");
        let send_sock = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send_sock.connect(recv_addr).expect("connect send socket");
        (send_sock, recv_sock)
    }

    fn test_header() -> RfHeader {
        general_call_header(BLANK_RPT, BLANK_RPT, *b"N0CALL  ")
    }

    /// The gate is `ConsoleSession::set_ptt`'s and it opens up to one poll
    /// tick BEFORE the run loop observes the edge, so whatever is sitting in
    /// `tx_frames` at key-down is the VOX pre-roll ring and the speech
    /// onset — the operator's first syllable. Draining it here is exactly
    /// what silently turns pre-roll off, which is why key-down must not.
    #[test]
    fn key_down_keeps_the_queued_preroll_and_sends_the_header_first() {
        let (send_sock, recv_sock) = loopback_pair();
        let (call_audio, push, _rx) = fake_call_audio();
        push.send(vec![1_i16; 160]).unwrap();
        push.send(vec![2_i16; 160]).unwrap();

        let mut ambe = FakeEncoder::new();
        let mut tx = TxState::new();
        let mut ctx = TxCtx {
            socket: &send_sock,
            ambe: &mut ambe,
            call_audio: &call_audio,
            shared: &SharedState::new(),
            header: test_header(),
        };

        let keyed = apply_ptt_edge(&mut ctx, &mut tx, true);
        assert!(keyed);

        assert_eq!(
            call_audio.tx_frames.try_recv().ok(),
            Some(vec![1_i16; 160]),
            "key_down must leave the lane's queued pre-roll alone — it is this transmission's \
             first audio, not somebody else's residue"
        );
        assert_eq!(call_audio.tx_frames.try_recv().ok(), Some(vec![2_i16; 160]));
        assert!(tx.pending_pcm.is_empty());
        assert!(tx.stream.is_some(), "key-down must start a fresh TxStream");

        let mut buf = [0u8; 128];
        let (n, _) = recv_sock
            .recv_from(&mut buf)
            .expect("key-down must send the header packet");
        let pkt = DsvtPacket::parse(&buf[..n]).expect("valid DsvtPacket");
        assert!(
            matches!(pkt, DsvtPacket::Header { .. }),
            "the FIRST thing key-down sends must be the header, got {pkt:?}"
        );
    }

    #[test]
    fn unkey_with_nothing_ever_queued_still_sends_one_bare_eot_frame() {
        let (send_sock, recv_sock) = loopback_pair();
        let (call_audio, _push, _rx) = fake_call_audio();
        let mut ambe = FakeEncoder::new();
        let mut tx = TxState::new();
        let shared = SharedState::new();
        let mut ctx = TxCtx {
            socket: &send_sock,
            ambe: &mut ambe,
            call_audio: &call_audio,
            shared: &shared,
            header: test_header(),
        };

        assert!(apply_ptt_edge(&mut ctx, &mut tx, true));
        let mut buf = [0u8; 128];
        let (n, _) = recv_sock.recv_from(&mut buf).expect("header packet");
        assert!(matches!(
            DsvtPacket::parse(&buf[..n]).expect("valid packet"),
            DsvtPacket::Header { .. }
        ));

        // Unkey immediately: no mic audio was ever pushed, and the encoder
        // never had anything submitted either.
        assert!(!apply_ptt_edge(&mut ctx, &mut tx, false));
        assert!(!shared.ptt.load(Ordering::Relaxed));
        assert!(
            tx.stream.is_none(),
            "unkey must clear the TxState back to not-keyed"
        );

        let (n, _) = recv_sock
            .recv_from(&mut buf)
            .expect("unkey must send a terminating frame even with nothing queued");
        let pkt = DsvtPacket::parse(&buf[..n]).expect("valid packet");
        let DsvtPacket::Voice { end, ambe: a, .. } = pkt else {
            panic!("expected a voice frame, got {pkt:?}");
        };
        assert!(end, "the bare EOT frame must carry the end bit");
        assert_eq!(
            a, NULL_AMBE,
            "with nothing ever encoded, the bare EOT frame must carry D-Star's NULL codeword —              an all-zero payload is not silence to an AMBE decoder"
        );

        // Nothing else was sent.
        recv_sock
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(
            recv_sock.recv_from(&mut buf).is_err(),
            "exactly one terminating frame must be sent, not more"
        );
    }

    #[test]
    fn unkey_drains_queued_mic_frames_before_the_eot_and_marks_only_the_last_frame() {
        let (send_sock, recv_sock) = loopback_pair();
        let (call_audio, push, _rx) = fake_call_audio();
        // Three frames sitting in `tx_frames` — audio the (real) mic lane
        // would have queued in the ~50ms since the run-loop's last drain —
        // when the unkey edge is observed.
        push.send(vec![10_i16; 160]).unwrap();
        push.send(vec![20_i16; 160]).unwrap();
        push.send(vec![30_i16; 160]).unwrap();

        let mut ambe = FakeEncoder::new();
        let mut tx = TxState::new();
        tx.stream = Some(TxStream::new(0xBEEF));
        let shared = SharedState::new();
        let mut ctx = TxCtx {
            socket: &send_sock,
            ambe: &mut ambe,
            call_audio: &call_audio,
            shared: &shared,
            header: test_header(),
        };

        let keyed = apply_ptt_edge(&mut ctx, &mut tx, false);
        assert!(!keyed);

        // Every queued frame must have been drained (none left for a future
        // transmission), and the flush must have produced exactly 3 voice
        // packets sharing the stream id, with `end` set ONLY on the last.
        assert!(call_audio.tx_frames.try_recv().is_err());
        assert!(tx.pending_pcm.is_empty());

        let mut buf = [0u8; 128];
        let mut seen = Vec::new();
        recv_sock
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        while let Ok((n, _)) = recv_sock.recv_from(&mut buf) {
            seen.push(DsvtPacket::parse(&buf[..n]).expect("valid packet"));
        }
        assert_eq!(
            seen.len(),
            3,
            "all three queued frames must be flushed, got {seen:?}"
        );
        for (i, pkt) in seen.iter().enumerate() {
            let DsvtPacket::Voice { stream_id, end, .. } = pkt else {
                panic!("expected a voice frame, got {pkt:?}");
            };
            assert_eq!(*stream_id, 0xBEEF);
            assert_eq!(
                *end,
                i == seen.len() - 1,
                "only the LAST frame may carry the EOT bit"
            );
        }
    }

    #[test]
    fn unkey_never_hangs_even_when_the_encoder_never_answers() {
        let (send_sock, recv_sock) = loopback_pair();
        let (call_audio, push, _rx) = fake_call_audio();
        push.send(vec![5_i16; 160]).unwrap();

        let mut ambe = WedgedEncoder { in_flight: 0 };
        let mut tx = TxState::new();
        tx.stream = Some(TxStream::new(0xCAFE));
        let shared = SharedState::new();
        let mut ctx = TxCtx {
            socket: &send_sock,
            ambe: &mut ambe,
            call_audio: &call_audio,
            shared: &shared,
            header: test_header(),
        };

        // Bounded by FLUSH_DEADLINE (250ms): must return, not hang forever.
        let started = Instant::now();
        let keyed = apply_ptt_edge(&mut ctx, &mut tx, false);
        assert!(!keyed);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "apply_ptt_edge's unkey branch must be bounded by FLUSH_DEADLINE, took {:?}",
            started.elapsed()
        );

        let mut buf = [0u8; 128];
        let (n, _) = recv_sock
            .recv_from(&mut buf)
            .expect("a bailout EOT frame must still be sent");
        let pkt = DsvtPacket::parse(&buf[..n]).expect("valid packet");
        let DsvtPacket::Voice { end, ambe: a, .. } = pkt else {
            panic!("expected a voice frame, got {pkt:?}");
        };
        assert!(end);
        assert_eq!(a, NULL_AMBE);
    }

    #[test]
    fn unlink_flushing_eot_if_keyed_sends_the_eot_before_the_unlink() {
        let (send_sock, recv_sock) = loopback_pair();
        let (call_audio, _push, _rx) = fake_call_audio();
        let mut ambe = FakeEncoder::new();
        let mut tx = TxState::new();
        tx.stream = Some(TxStream::new(0x1234));
        let shared = SharedState::new();
        let mut fsm = DextraFsm::new("N0CALL", b'A').expect("valid callsign");
        let _ = fsm.connect(Instant::now());
        let mut ctx = TxCtx {
            socket: &send_sock,
            ambe: &mut ambe,
            call_audio: &call_audio,
            shared: &shared,
            header: test_header(),
        };

        // Simulates the run-loop's shutdown branch observing `keyed == true`
        // (a session dropped/disconnected mid-transmission) — must flush the
        // EOT BEFORE sending the unlink, so the reflector never sees an
        // unlink while it still thinks a stream is open.
        unlink_flushing_eot_if_keyed(&mut ctx, &mut tx, &mut fsm, true);

        let mut buf = [0u8; 128];
        let (n1, _) = recv_sock.recv_from(&mut buf).expect("EOT frame first");
        let eot = DsvtPacket::parse(&buf[..n1]).expect("valid packet");
        let DsvtPacket::Voice { end, .. } = eot else {
            panic!("expected the EOT voice frame first, got {eot:?}");
        };
        assert!(end);

        let (n2, _) = recv_sock.recv_from(&mut buf).expect("unlink second");
        // The 11-byte connect/unlink wire shape (see DextraFsm's module
        // docs) — NOT a "DSVT"-prefixed packet.
        assert_eq!(n2, 11);
        assert_ne!(&buf[..4], b"DSVT");
    }

    /// The vocoder double for the stale-frame test: `poll_encoded` hands
    /// back frames that were "already in the queue" before this test ever
    /// submitted anything — exactly the state a deadline-truncated unkey
    /// leaves the real chip in.
    struct PreloadedEncoder {
        queue: VecDeque<[u8; 9]>,
    }

    impl AmbeStream for PreloadedEncoder {
        fn submit_decode(&mut self, _frame: astar_codec::ambe::ChannelFrame) {}
        fn poll_decoded(&mut self) -> Option<[i16; 160]> {
            None
        }
        fn in_flight(&self) -> usize {
            0
        }
        fn submit_encode(&mut self, pcm: [i16; 160]) {
            let b = pcm[0].to_be_bytes();
            self.queue.push_back([b[0], b[1], 0, 0, 0, 0, 0, 0, 0]);
        }
        fn poll_encoded(&mut self) -> Option<astar_codec::ambe::ChannelFrame> {
            self.queue
                .pop_front()
                .map(astar_codec::ambe::ChannelFrame::Dstar)
        }
        fn in_flight_encoded(&self) -> usize {
            self.queue.len()
        }
    }

    /// iax-2f6b review's Critical finding, at the unit level: key-down must
    /// drain the encoder's OUTPUT queue, not just the mic PCM queues. Frames
    /// abandoned by a previous unkey's deadline-truncated flush are still
    /// owed by the chip; they arrive afterwards, unpolled, and would
    /// otherwise be transmitted as voice frames 0..n of the NEXT stream id.
    #[test]
    fn key_down_drains_encoded_frames_left_over_from_a_truncated_unkey() {
        let (send_sock, recv_sock) = loopback_pair();
        let (call_audio, _push, _rx) = fake_call_audio();
        // Four frames of the PREVIOUS transmission, still in the encoder.
        let mut ambe = PreloadedEncoder {
            queue: VecDeque::from(vec![[0xAA; 9]; 4]),
        };
        let mut tx = TxState::new();
        let shared = SharedState::new();
        let mut ctx = TxCtx {
            socket: &send_sock,
            ambe: &mut ambe,
            call_audio: &call_audio,
            shared: &shared,
            header: test_header(),
        };

        assert!(apply_ptt_edge(&mut ctx, &mut tx, true));
        assert_eq!(
            ctx.ambe.in_flight_encoded(),
            0,
            "key-down must leave the encoder's output queue empty — anything left in it belongs \
             to the previous transmission and would go out under the new stream id"
        );

        // Only the header went out: no stale frame was transmitted.
        let mut buf = [0u8; 128];
        let (n, _) = recv_sock.recv_from(&mut buf).expect("header packet");
        assert!(matches!(
            DsvtPacket::parse(&buf[..n]).expect("valid packet"),
            DsvtPacket::Header { .. }
        ));
        recv_sock
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(
            recv_sock.recv_from(&mut buf).is_err(),
            "key-down must send the header and nothing else"
        );
    }

    // ---- PttGate: the forced-unkey / refusal policy ----------------------

    #[test]
    fn ptt_gate_applies_an_ordinary_key_and_unkey_while_linked() {
        let mut gate = PttGate::new();
        let t0 = Instant::now();
        assert_eq!(gate.decide(false, LinkState::Linked, t0), PttAction::None);
        assert_eq!(gate.decide(true, LinkState::Linked, t0), PttAction::Key);
        gate.applied(true, t0);
        // Still held: nothing to do.
        assert_eq!(gate.decide(true, LinkState::Linked, t0), PttAction::None);
        assert_eq!(gate.decide(false, LinkState::Linked, t0), PttAction::Unkey);
        gate.applied(false, t0);
        assert!(!gate.is_keyed());
    }

    #[test]
    fn ptt_gate_forces_an_unkey_when_the_link_drops_and_latches_until_release() {
        let mut gate = PttGate::new();
        let t0 = Instant::now();
        assert_eq!(gate.decide(true, LinkState::Linked, t0), PttAction::Key);
        gate.applied(true, t0);

        assert_eq!(
            gate.decide(true, LinkState::Failed, t0),
            PttAction::Forced(ForcedUnkey::LinkLost(LinkState::Failed)),
            "losing the link while keyed must force an unkey"
        );
        gate.applied(false, t0);
        // PTT is still held down: it must NOT re-key, and must not spam.
        assert_eq!(gate.decide(true, LinkState::Failed, t0), PttAction::None);
        assert_eq!(
            gate.decide(true, LinkState::Linked, t0),
            PttAction::None,
            "even once the link comes back, a held-down PTT must not auto-re-key"
        );
        // Release re-arms.
        assert_eq!(gate.decide(false, LinkState::Linked, t0), PttAction::None);
        assert_eq!(gate.decide(true, LinkState::Linked, t0), PttAction::Key);
    }

    #[test]
    fn ptt_gate_refuses_a_key_down_while_the_link_is_not_up_and_reports_it_once() {
        let mut gate = PttGate::new();
        let t0 = Instant::now();
        assert_eq!(
            gate.decide(true, LinkState::Linking, t0),
            PttAction::Refused(LinkState::Linking),
            "keying before the reflector has ACKed must be refused"
        );
        // Reported once, not once per 2 ms poll.
        assert_eq!(gate.decide(true, LinkState::Linking, t0), PttAction::None);
        assert_eq!(gate.decide(false, LinkState::Linked, t0), PttAction::None);
        assert_eq!(gate.decide(true, LinkState::Linked, t0), PttAction::Key);
    }

    #[test]
    fn ptt_gate_times_out_a_transmission_that_never_ends() {
        let mut gate = PttGate::new();
        let t0 = Instant::now();
        assert_eq!(gate.decide(true, LinkState::Linked, t0), PttAction::Key);
        gate.applied(true, t0);

        // Just short of the TOT: still transmitting.
        let nearly = t0 + MAX_TX_DURATION / 2;
        assert_eq!(
            gate.decide(true, LinkState::Linked, nearly),
            PttAction::None
        );

        // A lost key-up event: PTT is still "held" but the operator is long
        // gone. The engine — which owns the wire — cuts it.
        let over = t0 + MAX_TX_DURATION;
        assert_eq!(
            gate.decide(true, LinkState::Linked, over),
            PttAction::Forced(ForcedUnkey::Timeout)
        );
        gate.applied(false, over);
        assert_eq!(gate.decide(true, LinkState::Linked, over), PttAction::None);
        // Release, then key again: a fresh TOT window.
        assert_eq!(gate.decide(false, LinkState::Linked, over), PttAction::None);
        assert_eq!(gate.decide(true, LinkState::Linked, over), PttAction::Key);
        gate.applied(true, over);
        assert_eq!(
            gate.decide(true, LinkState::Linked, over + MAX_TX_DURATION / 2),
            PttAction::None,
            "the time-out timer must restart with each transmission"
        );
    }

    // ---- TX RF header identity / destination -----------------------------

    #[test]
    fn a_reflector_host_name_fills_the_destination_repeater_fields() {
        let (rpt1, rpt2) = tx_repeater_fields(None, "xrf757.openquad.net", b'A');
        assert_eq!(&rpt2, b"XRF757 A", "RPT2 names the reflector and module");
        assert_eq!(&rpt1, b"XRF757 G", "RPT1 names the gateway");
        // An explicit callsign wins over the host derivation.
        let (_, rpt2) = tx_repeater_fields(Some("XLX458"), "1.2.3.4", b'B');
        assert_eq!(
            &rpt2, b"XRF458 B",
            "an XLX reflector is addressed by its DExtra (XRF) callsign"
        );
    }

    /// The bug this pins: an XLX reflector reached by its published hostname
    /// must be addressed as `XRF###`, which is what the one real capture this
    /// project owns shows arriving FROM XLX458 (`astar_dstar::dsvt`'s
    /// `parses_a_real_xlx_header_with_a_zero_crc` → `RPT2 = "XRF458 A"`).
    /// Deriving `XLX836` from the hostname and packing it verbatim addressed
    /// a reflector nobody on that wire answers to.
    #[test]
    fn an_xlx_host_is_addressed_by_its_dextra_callsign() {
        let (rpt1, rpt2) = tx_repeater_fields(None, "xlx836.example.org", b'A');
        assert_eq!(&rpt2, b"XRF836 A");
        assert_eq!(&rpt1, b"XRF836 G");
    }

    /// A reflector's three-character suffix is ALPHANUMERIC, not always
    /// digits — `XLXARG` and friends are ~15% of the published XLX list, and
    /// a digits-only derivation dropped every one of them onto the blank-RPT
    /// fallback: transmitting with no destination identity at all.
    #[test]
    fn a_letter_suffixed_reflector_host_still_names_the_destination() {
        assert_eq!(
            reflector_callsign_from_host("xlxarg.example.org").as_deref(),
            Some("XLXARG")
        );
        let (rpt1, rpt2) = tx_repeater_fields(None, "xlxarg.example.org", b'C');
        assert_ne!(rpt2, BLANK_RPT, "a letter suffix must not blank the header");
        assert_eq!(&rpt2, b"XRFARG C");
        assert_eq!(&rpt1, b"XRFARG G");
    }

    /// The other half of widening the suffix to alphanumerics: an ordinary
    /// six-letter hostname label must NOT become a callsign. Requiring a
    /// reflector-family prefix is what keeps `server.example.com` from
    /// transmitting `RPT2 = "SERVER A"`.
    #[test]
    fn an_ordinary_six_letter_hostname_is_not_mistaken_for_a_reflector() {
        for host in [
            "server.example.com",
            "router.example.com",
            "gw.example.com",
            "mumble.example.com",
        ] {
            assert_eq!(
                reflector_callsign_from_host(host),
                None,
                "{host} is not a reflector callsign"
            );
        }
    }

    #[test]
    fn a_bare_ip_host_falls_back_to_blank_repeater_fields() {
        // Nothing to derive a callsign from, and guessing one would be
        // inventing protocol — blank, with a warning (see tx_repeater_fields).
        let (rpt1, rpt2) = tx_repeater_fields(None, "127.0.0.1", b'A');
        assert_eq!(rpt1, BLANK_RPT);
        assert_eq!(rpt2, BLANK_RPT);
    }

    #[test]
    fn reflector_callsign_derivation_is_narrow() {
        assert_eq!(
            reflector_callsign_from_host("xlx458.example.org").as_deref(),
            Some("XLX458")
        );
        assert_eq!(
            reflector_callsign_from_host("REF030").as_deref(),
            Some("REF030")
        );
        assert_eq!(
            reflector_callsign_from_host("dcs019.example.org").as_deref(),
            Some("DCS019")
        );
        assert_eq!(reflector_callsign_from_host("127.0.0.1"), None);
        assert_eq!(reflector_callsign_from_host("example.com"), None);
        assert_eq!(reflector_callsign_from_host("xrf7570.net"), None);
        assert_eq!(reflector_callsign_from_host("xrf75.net"), None);
        assert_eq!(reflector_callsign_from_host("xyz757.net"), None);
        assert_eq!(reflector_callsign_from_host(""), None);
    }

    #[test]
    fn the_tx_header_carries_the_operators_own_callsign() {
        // `general_call_header` takes three consecutive [u8; 8] arguments, so
        // a transposition is a one-character mistake that would transmit an
        // unidentified stream. Pin the mapping.
        let fsm = DextraFsm::new("AJ7HR", b'A').expect("valid callsign");
        let (rpt1, rpt2) = tx_repeater_fields(Some("XRF757"), "1.2.3.4", b'A');
        let header = general_call_header(rpt2, rpt1, fsm.callsign());
        assert_eq!(&header.my, b"AJ7HR   ", "MY must be the operator");
        assert_eq!(&header.rpt2, b"XRF757 A");
        assert_eq!(&header.rpt1, b"XRF757 G");
        assert_eq!(&header.ur, b"CQCQCQ  ");
    }

    #[test]
    fn unlink_flushing_eot_if_keyed_is_a_pure_passthrough_when_not_keyed() {
        // The common case (a session that was never keyed, or already
        // cleanly unkeyed): no EOT frame is fabricated, just the unlink.
        let (send_sock, recv_sock) = loopback_pair();
        let (call_audio, _push, _rx) = fake_call_audio();
        let mut ambe = FakeEncoder::new();
        let mut tx = TxState::new();
        let shared = SharedState::new();
        let mut fsm = DextraFsm::new("N0CALL", b'A').expect("valid callsign");
        let _ = fsm.connect(Instant::now());
        let mut ctx = TxCtx {
            socket: &send_sock,
            ambe: &mut ambe,
            call_audio: &call_audio,
            shared: &shared,
            header: test_header(),
        };

        unlink_flushing_eot_if_keyed(&mut ctx, &mut tx, &mut fsm, false);

        let mut buf = [0u8; 128];
        let (n, _) = recv_sock.recv_from(&mut buf).expect("unlink");
        assert_eq!(n, 11);
        assert_ne!(&buf[..4], b"DSVT");
        recv_sock
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(
            recv_sock.recv_from(&mut buf).is_err(),
            "nothing but the unlink may be sent when not keyed"
        );
    }
}
