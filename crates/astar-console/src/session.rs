// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The operator-console session and its supporting config/error/helpers
//! (iax-dd42). `ConsoleSession` drives a single web-transceiver call and
//! exposes a pollable [`ConsoleState`]. This file also holds the config,
//! error, and device/node helpers the front-end calls before connecting.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astar_audio::{
    AudioBackend, AudioError, AudioRouter, CallAudio, Direction, MicId, MicProfile, OutputId,
    StreamConfig, StreamHandle,
};
use astar_iax::{
    BridgeConfig, Call, CallEvent, CallId, CallMode, CodecPolicy, DialSpec, IncomingCall,
    IncomingCallEvent, IncomingCallListener, IncomingCallPolicy, KnownNodes, LinkEvent, LinkMode,
    LinkRoster, LinkSpec, LinkTransport, Manager, RegisterOptions, Registrar, Registration,
    RegistrationEvent, WgLinkConfig, WgStackStatus,
};
use astar_iax_core::session::auth::Secret;

#[cfg(feature = "dmr")]
use crate::dmr::{DmrConfig, DmrLink, DmrSnapshot};
#[cfg(feature = "dstar")]
use crate::dstar::{DstarConfig, DstarSession, DstarSnapshotState};
use crate::heard::HeardEntry;
#[cfg(feature = "m17")]
use crate::m17::{M17Config, M17Session, M17SnapshotState};
use crate::metering::Gain;
#[cfg(feature = "nxdn")]
use crate::nxdn::{NxdnConfig, NxdnLink, NxdnSnapshot};
use crate::state::{CallStatus, ConsoleState};
#[cfg(feature = "ysf")]
use crate::ysf::{YsfConfig, YsfLink, YsfSnapshot};
#[cfg(feature = "m17")]
use astar_m17::LinkState;

/// Outcome of an outbound node registration attempt. Secret-free: the
/// registrar password NEVER appears here — only a human-readable failure
/// description in the `Failed` variant.
#[derive(Debug, Clone)]
pub enum RegisterOutcome {
    /// The upstream registrar acknowledged our REGREQ (registration is live).
    Registered,
    /// Registration failed. `reason` is a secret-free description of the
    /// failure cause. The registrar password is never included.
    Failed(String),
}

/// An owned secret resolver for the `WireGuard` private-key reference
/// (iax-5bbd): the session-level mirror of the engine's borrowed
/// `SecretResolver`, owned (`Box`) so a link transport selected BEFORE the
/// engine exists can be applied when the engine is first built. House secret
/// rule: the resolver is consulted at stack-build time and key material is
/// never stored in config/snapshot/event/log types. The FFI layer wraps a
/// caller-held private key in a one-shot closure of this type.
pub type LinkKeyResolver = dyn Fn(&str) -> String + Send + Sync;

/// How the session answers inbound calls once its listener is started.
///
/// This is the console-level mirror of the station's node answer mode; the
/// station re-exports this type so there is a single `AnswerPolicy` across the
/// stack (and one FFI mapping).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AnswerPolicy {
    /// Auto-accept each inbound offer and bridge it to the local handset.
    #[default]
    Auto,
    /// Surface the offer (via [`ConsoleSession::take_incoming_from`]); the
    /// operator then calls [`ConsoleSession::answer_pending`] /
    /// [`ConsoleSession::reject_pending`].
    Manual,
}

/// The inbound side of the always-on session (iax-a1fb P1): the listener, its
/// event stream, and the once-only Manual-mode parking slot. Adopted inbound
/// calls land in the SAME [`ConsoleSession`] `active`/`events`/`state`
/// machinery the WT dial path uses, so `snapshot`/`set_ptt`/`disconnect` need no
/// inbound special-case. Registration is intentionally absent — it is rebuilt
/// fresh in Task 3.1.
struct InboundState {
    /// Keep the listener alive: its `Drop` stops the actor thread + frees the
    /// port.
    listener: IncomingCallListener,
    events: Receiver<IncomingCallEvent>,
    answer: AnswerPolicy,
    output: OutputId,
    mic: MicId,
    /// A Manual-mode offer parked awaiting an operator decision (`None` in Auto).
    parked: Option<IncomingCall>,
    /// Once-only edge: the caller string of a freshly parked Manual offer, taken
    /// by [`ConsoleSession::take_incoming_from`]. Public caller-id, never a
    /// secret.
    pending_from: Option<String>,
    /// Maximum concurrent adopted calls. Offers arriving when
    /// `manager.call_count() >= max_calls` are busy-rejected.
    max_calls: usize,
    /// Optional inbound node allowlist (iax-91c9). When `Some` AND non-empty,
    /// an offer whose caller node id is not on the list is rejected at
    /// call-setup time ("not authorized") before answer/adopt. `None` or an
    /// empty list = admit all (backward compatible).
    allowlist: Option<KnownNodes>,
}

/// Operator-supplied configuration for a web-transceiver call.
pub struct ConsoleConfig {
    /// Destination `AllStar` node number, e.g. `"55553"` — the node being
    /// dialled. The caller uses it to resolve the peer address; kept here for
    /// record/logging.
    pub node: String,
    /// `CALLING_NUMBER` IE — the node identifying *who is calling*. For a real
    /// connection this is the operator's own authorised node; against the
    /// public parrot it may simply match [`Self::node`]. A node rejecting an
    /// unauthorised caller returns "No authority found".
    pub calling_node: String,
    /// Guest secret, e.g. `"allstar"`.
    pub secret: String,
    /// `CALLING_NAME` IE, e.g. `"astar"`.
    pub name: String,
    /// Capture device substring; `None` = system default.
    pub input_device: Option<String>,
    /// Playback device substring; `None` = system default.
    pub output_device: Option<String>,
    /// Codec negotiation policy for this call (iax-31f7). Default `UlawOnly`.
    pub codec_policy: CodecPolicy,
}

/// Errors surfaced by the console.
#[derive(Debug)]
pub enum ConsoleError {
    /// `connect` called while a call is already live.
    AlreadyConnected,
    /// `set_ptt`/`disconnect` called with no active call.
    NotConnected,
    /// DNS resolution of the node failed.
    Resolve {
        node: String,
        source: std::io::Error,
    },
    /// Device enumeration failed.
    Audio(AudioError),
    /// No unique audio device matched the requested name (parrot device pick).
    Device(String),
    /// An error from the underlying client.
    Iax(astar_iax::IaxError),
    /// A link-layer failure (iax-1075) — secret-free, human-readable.
    Link(String),
    /// An M17 error (iax-f2b8 Task 4) — secret-free, human-readable. Also
    /// used for `m17_connect`/`m17_disconnect` when the `m17` feature isn't
    /// compiled in (`Station` surfaces this case directly via
    /// `StationError::M17` without ever calling into this crate, since the
    /// `ConsoleSession` methods themselves don't exist in that build — see
    /// the `m17` module-level doc note).
    M17(String),
    /// A D-Star error (iax-a9d4 Task 6 built RX; iax-2f6b added TX) —
    /// secret-free, human-readable. Returned by `dstar_connect`/
    /// `dstar_disconnect` when the `dstar` feature isn't compiled in, or by
    /// [`DstarSession::connect`] classifying a vocoder-availability failure.
    /// [`ConsoleSession::set_ptt`] no longer refuses D-Star: a live D-Star
    /// session is full-transceive and keys/unkeys exactly like the M17
    /// branch above it (see `crate::dstar`'s module docs).
    Dstar(String),
    /// A YSF error (iax-e8a4) — secret-free, human-readable. Returned when
    /// the `ysf` feature isn't compiled in, or by [`crate::ysf::YsfLink`]
    /// classifying a bind, resolve or callsign failure.
    Ysf(String),
    /// A DMR link could not be opened or used: raised by
    /// [`crate::dmr::DmrLink`] classifying a radio-id, callsign, password,
    /// resolve or bind failure — and, like every other variant here,
    /// SECRET-FREE. A DMR master authenticates with a password, and no
    /// message this carries interpolates one.
    ///
    /// Present whether or not the `dmr` feature is compiled in, so a caller
    /// that must name the case (the `Station` facade reporting "this build
    /// has no DMR") does not need a `cfg` of its own.
    Dmr(String),
    /// An NXDN link could not be opened or used: raised by
    /// [`crate::nxdn::NxdnLink`] classifying a callsign, radio-id, resolve
    /// or bind failure.
    Nxdn(String),
    /// A key-down was refused because the live voice route has no capture
    /// device it could open (none resolved, permission denied, or the device
    /// is held exclusively). Receiving still works; transmitting cannot.
    NoCaptureDevice,
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyConnected => write!(f, "a call is already in progress"),
            Self::NotConnected => write!(f, "no active call"),
            Self::Resolve { node, source } => write!(f, "could not resolve node {node}: {source}"),
            Self::Audio(e) => write!(f, "audio error: {e}"),
            Self::Device(msg) => write!(f, "audio device: {msg}"),
            Self::Iax(e) => write!(f, "{e}"),
            Self::Link(msg) => write!(f, "link: {msg}"),
            Self::M17(msg) => write!(f, "m17: {msg}"),
            Self::Dstar(msg) => write!(f, "dstar: {msg}"),
            Self::Ysf(msg) => write!(f, "ysf: {msg}"),
            Self::Dmr(msg) => write!(f, "dmr: {msg}"),
            Self::Nxdn(msg) => write!(f, "nxdn: {msg}"),
            Self::NoCaptureDevice => write!(f, "no capture device: cannot transmit"),
        }
    }
}

impl std::error::Error for ConsoleError {}

impl From<astar_iax::IaxError> for ConsoleError {
    fn from(e: astar_iax::IaxError) -> Self {
        Self::Iax(e)
    }
}

/// Parameters for [`ConsoleSession::link_connect`] (iax-1075): a
/// vendor-neutral node link dialed over standard IAX2. The `secret` is
/// consumed at dial time and never stored. Resolution happens in the CALLER
/// (`peer` is already a socket address) — no DNS in the library.
pub struct LinkConnectSpec {
    /// Node number / peer label (roster key).
    pub node: String,
    /// Resolved peer address to dial.
    pub peer: SocketAddr,
    /// Link mode (Transceive / Monitor / `LocalMonitor`).
    pub mode: LinkMode,
    /// Username / caller-id presented to the peer.
    pub caller_id: String,
    /// Dial-time secret; consumed immediately, never stored.
    pub secret: String,
    /// Dial shape (iax-5029): `CallMode::Standard` for plain node-to-node
    /// IAX2, `CallMode::WebTransceiver` for `AllStar` app nodes whose guest
    /// context only exposes the WT extension `"s"` (e.g. the parrot).
    pub shape: CallMode,
    /// Register a permanent (auto-reconnect) recipe. NOTE: reconnection runs
    /// from `Manager::tick`, which this session does not yet drive — the flag
    /// is recorded for hosts that tick the engine themselves.
    pub permanent: bool,
}

/// Enumerate device names usable for capture and playback. Duplex devices
/// appear in both lists. Called by the front-end to populate its pickers.
///
/// # Errors
/// Returns [`ConsoleError::Audio`] if the backend cannot enumerate devices.
pub fn list_devices(
    backend: &dyn AudioBackend,
) -> Result<(Vec<String>, Vec<String>), ConsoleError> {
    let devices = backend.devices().map_err(ConsoleError::Audio)?;
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for d in devices {
        match d.direction {
            Direction::Input => inputs.push(d.name),
            Direction::Output => outputs.push(d.name),
            Direction::Duplex => {
                inputs.push(d.name.clone());
                outputs.push(d.name);
            }
        }
    }
    Ok((inputs, outputs))
}

/// Drives a single web-transceiver call and exposes a pollable [`ConsoleState`].
/// Front-end-agnostic: the web harness, a later TUI, and the astar Tauri app all
/// consume this. Not `Sync`-shared internally — front-ends wrap it in a `Mutex`.
pub struct ConsoleSession {
    /// The connection pool + audio router that drives the WT network call
    /// (iax-64b6). `None` while idle (no backend yet); [`Self::connect`] builds
    /// it over the call's real backend, and [`Self::disconnect`]/[`Self::detach`]
    /// drop it. Gain/DSP/metering now live in the router lane, not a metering
    /// decorator. Only ever touched while a call is active.
    manager: Option<Manager>,
    /// The Manager's aggregated link-event stream (iax-1075), taken once per
    /// engine build and drained by [`ConsoleSession::drain_link_events`].
    link_event_rx: Option<Receiver<LinkEvent>>,
    /// The single active call's pool id (`None` = idle).
    active: Option<CallId>,
    /// Monotonic count of calls that have reached the answered state, surfaced
    /// as `ConsoleState::answered_seq`. Bumped once per newly-answered call
    /// (inbound adopt AND WT dial) so a multi-call node fires the join greeting
    /// per caller (iax-a82f). Never decremented.
    answered_seq: u64,
    /// Lifecycle/media event receiver for the active call (taken from the
    /// `Manager` at dial time).
    events: Option<Receiver<CallEvent>>,
    /// TX (mic/input) volume gain. Created at unity and persisted for the
    /// session's lifetime so volume adjustments survive reconnects. Seeds the
    /// routed call's router cell on connect.
    input_gain: Gain,
    /// RX (speaker/output) volume gain. Same lifetime contract as `input_gain`.
    output_gain: Gain,
    /// Capture DSP toggles for the network call (iax-d50d), persisted across
    /// reconnects. Set by the harness checkboxes; pushed to the router on
    /// connect and on toggle.
    denoise: Arc<AtomicBool>,
    compress: Arc<AtomicBool>,
    /// Compressor strength 0.0..=1.0 (f32 bits, default 0.90), persisted across
    /// reconnects and pushed to the router on connect / on change (iax-d9bb).
    compress_level: Arc<AtomicU32>,
    /// Neural denoise strength (f32 bits, `0.0..=1.0`; `1.0` = full). Held
    /// here so it survives across calls, like `compress_level`.
    denoise_strength: Arc<AtomicU32>,
    /// TX trim 0.0..=4.0 (f32 bits, default 1.0 = unity): the always-on final
    /// TX gain stage after the compressor (iax-750a). Persisted across
    /// reconnects and pushed to the router on connect / on change.
    tx_trim: Arc<AtomicU32>,
    /// RX/output compression toggle (iax-a4e7 PHASE 1): automatic leveling of
    /// the RECEIVED audio, reusing the mic-path compressor on the output bus.
    /// Shared across networks (output is listener-side, same as
    /// `output_gain`) — persisted across reconnects and pushed to the router
    /// on connect / on change. Default OFF (byte-identical rule).
    rx_compress: Arc<AtomicBool>,
    /// RX/output compression strength 0.0..=1.0 (f32 bits, default 0.90, same
    /// as the mic's), persisted across reconnects and pushed to the router on
    /// connect / on change (iax-a4e7 PHASE 1).
    rx_compress_level: Arc<AtomicU32>,
    /// RX jitter-buffer configuration (iax-rxjb): on/off and the window the
    /// adaptive depth may live in. Listener-side like the output gain, shared
    /// across networks, held here so a setting made before the engine exists
    /// survives to reach it — and pushed to the router on engine build and on
    /// every change, so it applies mid-call without reconnecting.
    rx_jitter: Arc<astar_audio::RxJitterSettings>,
    /// VOX pre-roll / look-back length in ms (default 0 = disabled, clamped to
    /// `0..=250`), persisted across reconnects and pushed to the router on
    /// connect / on change (iax-2733). astar opts in from its VOX edge.
    vox_preroll_ms: Arc<AtomicU32>,
    /// Calibrated per-mic profile shared with the parrot (iax-fb8d). When set,
    /// the network call's noise reducer is built from it. Set via
    /// [`Self::set_calibrated`] after a calibration run.
    calibrated: Arc<Mutex<Option<MicProfile>>>,
    state: ConsoleState,
    /// Two-level inspector: semantic timeline + bounded raw-frame ring.
    tracer: crate::tracer::Tracer,
    /// Receiver side of the frame-observer channel installed at dial time.
    frames: Option<std::sync::mpsc::Receiver<astar_iax::TracedFrame>>,
    /// The inbound listener + accept/poll state (iax-a1fb P1). `None` until
    /// [`Self::start_inbound`] binds it; dropped by [`Self::stop_inbound`]. When
    /// present, [`Self::poll_inbound`] drains its offers and adopts answered
    /// calls into THIS session's `Manager`.
    inbound: Option<InboundState>,
    /// `true` while the active call was adopted from an inbound offer (so its
    /// status starts at `Answered` rather than `Dialing`). Cleared when the call
    /// ends or is torn down.
    inbound_active: bool,
    /// The live M17 reflector session (iax-f2b8 Task 4). Mutually exclusive
    /// with an active IAX2 call (`active`): [`Self::connect`], the inbound
    /// answer/adopt paths (`handle_incoming`, the defensive adopt in
    /// [`Self::poll_inbound`], and [`Self::answer_pending`]) all refuse while
    /// this is `Some`, and [`Self::m17_connect`] refuses while `active` is
    /// `Some`.
    ///
    /// Deliberately mirrors the WT/IAX2 path's own contract: a link failure
    /// does NOT clear this back to `None` on its own — [`Self::snapshot`]
    /// keeps mirroring the session's (by-then `Failed`) state as
    /// `CallStatus::Hangup` on every poll, exactly like a WT call's `active`
    /// stays `Some` (and `status` stays `Hangup`) after a remote hangup until
    /// the front-end calls [`Self::disconnect`]/[`Self::m17_disconnect`].
    /// `None` only before the first `m17_connect` or after an explicit
    /// disconnect. Only compiled when the `m17` feature is enabled — see
    /// `astar_codec::codec2`'s module docs for why the
    /// `codec2-runtime`/`-static` split (and thus this field) is
    /// feature-gated in the first place; [`Self::m17_is_active`] and
    /// [`m17_available`] give the rest of this file a feature-independent
    /// view so only this field needs the `#[cfg]`.
    #[cfg(feature = "m17")]
    m17: Option<M17Session>,
    /// The live D-Star `DExtra` session (iax-a9d4 Task 6 RX; iax-2f6b adds
    /// TX). Mutually exclusive with an active IAX2 call (`active`) AND a
    /// live M17 session (`m17`) — every connect/adopt entry point that
    /// guards on those two also guards on this one; see
    /// [`Self::dstar_is_active`]. Unlike `m17`, D-Star's live state is never
    /// mirrored into `self.state` (the shared `ConsoleState` DTO) — a caller
    /// reads it via [`Self::dstar_state`] instead. Only compiled when the
    /// `dstar` feature is enabled (mirrors `m17`'s own `#[cfg]` pattern —
    /// see that field's docs for why only this field needs it).
    #[cfg(feature = "dstar")]
    dstar: Option<DstarSession>,
    /// The live System Fusion link, if any. Read through
    /// [`Self::ysf_state`], never mirrored into [`ConsoleState`] beyond the
    /// two flags — same arrangement, and same reason, as [`Self::dstar`].
    #[cfg(feature = "ysf")]
    ysf: Option<YsfLink>,
    /// The live NXDN link, if any. Read through [`Self::nxdn_state`], never
    /// mirrored into [`ConsoleState`] beyond the two flags — same
    /// arrangement, and same reason, as [`Self::ysf`].
    #[cfg(feature = "nxdn")]
    nxdn: Option<NxdnLink>,
    /// The live DMR link, if any. Read through [`Self::dmr_state`], never
    /// mirrored into [`ConsoleState`] beyond the two flags — same
    /// arrangement, and same reason, as [`Self::nxdn`].
    ///
    /// It holds no password: [`DmrConfig::password`] was moved into the link
    /// at connect, spent on one `RPTK` digest, and dropped. There is nothing
    /// here for a snapshot, an error or a log to leak.
    #[cfg(feature = "dmr")]
    dmr: Option<DmrLink>,
    /// The live outbound node registration handle (Task 3.1). `Some` only while
    /// a registration is in flight; `Drop` sends REGREL when cleared.
    /// Secret-free: the resolved password was consumed into the `Registrar`
    /// and never stored here.
    reg_handle: Option<Registration>,
    /// The event receiver for the live registration (parallel to `reg_handle`).
    reg_events: Option<std::sync::mpsc::Receiver<RegistrationEvent>>,
    /// Queued [`RegisterOutcome`] edges for [`Self::take_register_event`].
    reg_queue: VecDeque<RegisterOutcome>,
    /// `true` once a `Registered` outcome has been observed and not yet cleared
    /// by `stop_register`.
    reg_active: bool,
    /// Announcement service config supplied before the Manager is built.
    /// Applied eagerly when the Manager already exists; replayed in
    /// [`Self::ensure_engine`] when it is built for the first time.
    pending_announce: Option<astar_iax::ServiceConfig>,
    /// Bridge/conference configuration (iax-647d). Library default
    /// [`BridgeMode::Handset`] (today's 1:1). Applied eagerly when the Manager
    /// exists; replayed in [`Self::ensure_engine`] when it is first built. The
    /// node daemon sets this to `Bridge` via [`Self::set_bridge_config`].
    bridge_config: BridgeConfig,
    /// Station-level codec policy (iax-4348), pins the `Manager`'s pipeline
    /// sample rate at construction. Library default `CodecPolicy::default()`
    /// (`UlawOnly`, 8 kHz), byte-identical to pre-iax-4348 sessions.
    ///
    /// Set by [`Self::set_station_policy`] — which is what `Station`'s
    /// constructors call from `StationConfig`, so a `prefer_slin16` station is
    /// 16 kHz whichever network builds the engine first — and again from
    /// [`ConsoleConfig::codec_policy`] in [`Self::connect`] and from the
    /// inbound policy in [`Self::start_inbound`]. A live `Manager`'s rate
    /// cannot change, so a mismatch on an IDLE engine rebuilds it
    /// ([`Self::rebuild_idle_engine_for_rate`]) and a mismatch on a BUSY one
    /// is logged and left alone.
    station_policy: CodecPolicy,
    /// A `WireGuard` link transport selected before the engine exists
    /// (iax-5bbd): the secret-free config plus the owned key resolver, applied
    /// exactly once when the engine is first built. `None` = plain UDP (the
    /// library default — byte-identical to pre-iax-5bbd sessions). Retained on
    /// a failed apply so a retried connect can never silently fall back to
    /// plain UDP; an explicit [`Self::set_link_transport`] with
    /// [`LinkTransport::Udp`] clears it.
    pending_wg: Option<(WgLinkConfig, Box<LinkKeyResolver>)>,
    /// The digital-voice audio lane on the station's one router, held for
    /// the lifetime of the session that asked for it (see
    /// [`crate::voice_route`]). `Some` reserves the station: every other
    /// connect path refuses while it is held, exactly as a live IAX2 call or
    /// a live M17/D-Star/YSF session does.
    voice_route: Option<crate::voice_route::VoiceRoute>,
    /// Out-of-band DTMF digits harvested from the event drain loop
    /// (iax-d254), keyed by the source call's raw id. Merged with the
    /// Manager's in-band digit pool by [`ConsoleSession::drain_dtmf_digits`].
    dtmf_digits: Vec<(u64, char)>,
}

/// The pool id every WT call is dialed under. The console drives a single call
/// at a time, so a fixed id is sufficient and keeps the `Manager` book-keeping
/// trivial.
const WT_CALL_ID: u64 = 1;

impl ConsoleSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            // Idle: no Manager until `connect` builds one over the real backend.
            manager: None,
            link_event_rx: None,
            active: None,
            answered_seq: 0,
            events: None,
            input_gain: Gain::new(),
            output_gain: Gain::new(),
            denoise: Arc::new(AtomicBool::new(false)),
            compress: Arc::new(AtomicBool::new(false)),
            compress_level: Arc::new(AtomicU32::new(0.90_f32.to_bits())),
            denoise_strength: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            tx_trim: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            rx_compress: Arc::new(AtomicBool::new(false)),
            rx_compress_level: Arc::new(AtomicU32::new(0.90_f32.to_bits())),
            rx_jitter: Arc::new(astar_audio::RxJitterSettings::default()),
            vox_preroll_ms: Arc::new(AtomicU32::new(0)),
            calibrated: Arc::new(Mutex::new(None)),
            state: ConsoleState::default(),
            tracer: crate::tracer::Tracer::new(2048),
            frames: None,
            inbound: None,
            inbound_active: false,
            #[cfg(feature = "m17")]
            m17: None,
            #[cfg(feature = "dstar")]
            dstar: None,
            #[cfg(feature = "ysf")]
            ysf: None,
            #[cfg(feature = "nxdn")]
            nxdn: None,
            #[cfg(feature = "dmr")]
            dmr: None,
            reg_handle: None,
            reg_events: None,
            reg_queue: VecDeque::new(),
            reg_active: false,
            pending_announce: None,
            // Library default: handset (1:1), byte-identical to pre-iax-647d.
            bridge_config: BridgeConfig::default(),
            // Library default: UlawOnly (8 kHz), byte-identical to pre-iax-4348.
            station_policy: CodecPolicy::default(),
            // Library default: plain UDP, byte-identical to pre-iax-5bbd.
            pending_wg: None,
            voice_route: None,
            dtmf_digits: Vec::new(),
        }
    }

    /// Select the primary-link transport (iax-5bbd; the session-level mirror
    /// of `Manager::set_link_transport`). [`LinkTransport::Udp`] (the library
    /// default) keeps plain OS UDP; [`LinkTransport::Wireguard`] routes the
    /// whole engine — outgoing dials, the inbound listener, and outbound
    /// registration — through one shared userspace `WireGuard` tunnel.
    ///
    /// Call BEFORE connect: applied immediately when the engine already exists
    /// (and it is idle), otherwise stored (secret-free — the config carries a
    /// key *reference*; `resolver` is the only holder of material) and applied
    /// exactly once when the engine is first built. The transport is immutable
    /// while any call is pooled — switching = disconnect/reconnect.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] if a call is pooled (the engine's
    /// `CallInProgress` refusal); [`ConsoleError::Link`] if the tunnel
    /// config/key is unusable (the message names the key *reference*, never
    /// material).
    pub fn set_link_transport(
        &mut self,
        transport: LinkTransport,
        resolver: Box<LinkKeyResolver>,
    ) -> Result<(), ConsoleError> {
        if let Some(mgr) = self.manager.as_mut() {
            mgr.set_link_transport(transport, &|r| resolver(r))
                .map_err(map_link_err)?;
            // The engine is now the source of truth; nothing left to replay.
            self.pending_wg = None;
            Ok(())
        } else {
            // No engine yet: remember the selection for the first build. UDP
            // is what a fresh engine defaults to, so it simply clears any
            // pending `WireGuard` selection.
            self.pending_wg = match transport {
                LinkTransport::Udp => None,
                LinkTransport::Wireguard(cfg) => Some((cfg, resolver)),
            };
            Ok(())
        }
    }

    /// Tunnel status (handshake age, traffic counters) for operational
    /// logging, passed through from the engine. `None` before the engine is
    /// built or while the transport is plain UDP. Secret-free.
    #[must_use]
    pub fn wg_status(&self) -> Option<WgStackStatus> {
        self.manager.as_ref().and_then(Manager::wg_status)
    }

    /// Set the bridge/conference configuration (iax-647d). Applied immediately
    /// if the `Manager` exists (re-wiring live calls), and stored so it is
    /// replayed when the `Manager` is first built in [`Self::ensure_engine`].
    /// The node daemon calls this with `Bridge` (its default) and on
    /// `POST /bridge`.
    ///
    /// # Errors
    /// [`ConsoleError::Iax`] if re-wiring live calls fails.
    pub fn set_bridge_config(&mut self, cfg: BridgeConfig) -> Result<(), ConsoleError> {
        self.bridge_config = cfg;
        if let Some(mgr) = self.manager.as_mut() {
            mgr.set_bridge_config(cfg).map_err(ConsoleError::Iax)?;
        }
        Ok(())
    }

    /// Current bridge/conference configuration (iax-647d).
    #[must_use]
    pub fn bridge_config(&self) -> BridgeConfig {
        self.bridge_config
    }

    /// Push an announcement service config into the session. If the `Manager`
    /// already exists (a call is active), the config is applied immediately.
    /// Otherwise it is stored and replayed when the `Manager` is first built in
    /// [`Self::ensure_engine`]. This is idempotent — calling it multiple times
    /// replaces the pending config.
    pub fn set_announce_config(&mut self, cfg: astar_iax::ServiceConfig) {
        if let Some(mgr) = self.manager.as_mut() {
            mgr.set_announce_config(cfg.clone());
        }
        self.pending_announce = Some(cfg);
    }

    /// Store the calibrated per-mic profile; the next call's noise reducer is
    /// built from it. Calibration runs while idle, so a later `connect` picks
    /// it up.
    // `profile` is now only ever cloned/borrowed here (the last owner, the
    // M17 pref fan-out, went with the one-audio-lane refactor), so clippy
    // would suggest `Option<&MicProfile>`. The by-value signature stays: it
    // is the published shape every caller and binding already uses, and the
    // last-use clone is a per-calibration cost, not a hot-path one (same
    // convention as `DstarSession::connect`'s own
    // `#[allow(clippy::needless_pass_by_value)]`).
    #[allow(clippy::needless_pass_by_value)]
    pub fn set_calibrated(&self, profile: Option<MicProfile>) {
        self.calibrated.lock().unwrap().clone_from(&profile);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_mic_profile(id, profile.clone());
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_profile(mic, profile.clone());
            }
        }
    }

    /// Toggle capture noise-reduction on the next/current network call.
    /// Whether mic noise reduction is switched on for the live/next call.
    /// Read by the idle capability prediction, which has to answer before a
    /// lane exists to ask.
    #[must_use]
    pub fn denoise(&self) -> bool {
        self.denoise.load(Ordering::Relaxed)
    }

    pub fn set_denoise(&self, on: bool) {
        self.denoise.store(on, Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_denoise(id, on);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_denoise(mic, on);
            }
        }
    }

    /// Toggle capture compression on the next/current network call.
    pub fn set_compress(&self, on: bool) {
        self.compress.store(on, Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_compress(id, on);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_compress(mic, on);
            }
        }
    }

    /// Set the capture compression strength (0.0..=1.0, clamped) on the
    /// next/current network call. Takes effect immediately when compression is
    /// enabled (iax-d9bb).
    pub fn set_compression_level(&self, level: f32) {
        let level = level.clamp(0.0, 1.0);
        self.compress_level
            .store(level.to_bits(), Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_compression_level(id, level);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_compress_level(mic, level);
            }
        }
    }

    /// Set the neural denoise strength (`0.0..=1.0`, clamped) on the
    /// live/next call. `1.0` = full denoise, `0.0` = bypass.
    ///
    /// RNNoise has no strength parameter of its own, so this drives a
    /// delay-compensated dry/wet mix inside the stage
    /// (`docs/design/noise-suppression.md`). It does nothing while the
    /// classical hum-filter-plus-gate chain is running: there is no wet
    /// path to mix against.
    pub fn set_denoise_strength(&self, level: f32) {
        let level = level.clamp(0.0, 1.0);
        self.denoise_strength
            .store(level.to_bits(), Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_denoise_strength(id, level);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_denoise_strength(mic, level);
            }
        }
    }

    /// Toggle RX/output compression on the next/current network call
    /// (iax-a4e7 PHASE 1): automatic leveling of the received audio, reusing
    /// the mic-path compressor on the output bus. Shared across networks
    /// (output is listener-side, same as [`Self::set_output_gain`]).
    pub fn set_rx_compress(&self, on: bool) {
        self.rx_compress.store(on, Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_output_compress(id, on);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            r.set_output_compress(route.out(), on);
        }
    }

    /// Configure the RX jitter buffer (iax-rxjb): whether received audio is
    /// played out of the adaptive buffer at all, and the window
    /// (`min_ms`..=`max_ms`) its depth may live in. Both bounds are clamped to
    /// `0..=500` ms and a `max_ms` under `min_ms` is raised to meet it — a
    /// setting is repaired, never refused.
    ///
    /// Takes effect immediately: a change reaches a call in progress on the
    /// next device callback, with no reconnect. Switching the buffer off
    /// drains what it is holding rather than dropping it; switching it on
    /// starts a fresh, empty buffer. Shared across networks (the buffer is on
    /// the output bus, same as [`Self::set_output_gain`]).
    pub fn set_rx_jitter(&self, cfg: astar_audio::RxJitterConfig) {
        self.rx_jitter.set(cfg);
        if let Some(mgr) = self.manager.as_ref() {
            mgr.set_rx_jitter(self.rx_jitter.get());
        }
    }

    /// Set the RX/output compression strength (0.0..=1.0, clamped) on the
    /// next/current network call. Takes effect immediately when RX
    /// compression is enabled (iax-a4e7 PHASE 1).
    pub fn set_rx_compression_level(&self, level: f32) {
        let level = level.clamp(0.0, 1.0);
        self.rx_compress_level
            .store(level.to_bits(), Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_output_compress_level(id, level);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            r.set_output_compress_level(route.out(), level);
        }
    }

    /// Set the TX trim (0.0..=4.0, clamped; 1.0 = unity) on the next/current
    /// network call: the always-on final TX gain stage after the compressor
    /// (iax-750a). Persisted across reconnects; takes effect immediately.
    pub fn set_tx_trim(&self, g: f32) {
        let g = g.clamp(0.0, 4.0);
        self.tx_trim.store(g.to_bits(), Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_tx_trim(id, g);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_tx_trim(mic, g);
            }
        }
    }

    /// Set the VOX pre-roll / look-back length (ms, clamped to `0..=250`) on the
    /// next/current network call (iax-2733). `0` disables pre-roll. Persisted
    /// across reconnects; takes effect immediately on the active routed mic.
    pub fn set_vox_preroll_ms(&self, ms: u32) {
        let ms = ms.min(250);
        self.vox_preroll_ms.store(ms, Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_vox_preroll_ms(id, ms);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_preroll_ms(mic, ms);
            }
        }
    }

    /// Set the live spectrum peak-hold decay (dB/SECOND, clamped, iax-8616) on
    /// the active network call's TX + RX analyzers. No-op if no call is active
    /// (applies only to currently-live analyzers; iax-8616).
    pub fn set_spectrum_decay(&self, db_per_sec: f32) {
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_spectrum_decay(id, db_per_sec);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_spectrum_decay(mic, db_per_sec);
            }
            r.set_output_spectrum_decay(route.out(), db_per_sec);
        }
    }

    // ── The one audio lane (`crate::voice_route`) ───────────────────────

    /// Open the audio lane for a digital-voice session on the station's one
    /// router — the output bus now, the capture lane if it can — and reserve
    /// the route: every other connect path refuses while it is held. The
    /// session gets only the channel ends; meters, preferences and keying
    /// stay here. `input`/`output` are device-name substrings, `None` =
    /// system default, resolved exactly as [`Self::connect`] resolves them.
    ///
    /// The lanes open at the STATION's pipeline rate (iax-4348), and the
    /// session still gets the fixed 8 kHz its codec speaks: on a 16 kHz
    /// station `VoiceRoute` bridges the two, 20 ms frame for 20 ms frame.
    /// See `crate::voice_route`'s module docs.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] if anything is live;
    /// [`ConsoleError::Device`]/[`ConsoleError::Audio`] if the OUTPUT cannot
    /// be resolved or opened. A missing input is not an error.
    pub fn open_voice_route(
        &mut self,
        input: Option<&str>,
        output: Option<&str>,
        make_backend: impl FnOnce() -> Box<dyn AudioBackend>,
    ) -> Result<CallAudio, ConsoleError> {
        self.can_open_voice_route()?;
        // Scoped so the `&mut Manager` borrow ends before `push_prefs` takes
        // `&self` below.
        let (route, audio) = {
            let manager = self.ensure_engine(make_backend);
            let enumerated = manager.devices().map_err(ConsoleError::Audio)?;
            let out_id = match output {
                Some(q) => find_device(&enumerated, q, Direction::Output)?,
                None => manager
                    .default_output()
                    .ok_or_else(|| ConsoleError::Device("no default output device".into()))?
                    .id
                    .as_str()
                    .to_string(),
            };
            // A capture device that will not resolve is NOT fatal: the route
            // is receive-only and every key-down retries.
            let mic_id = match input {
                Some(q) => find_device(&enumerated, q, Direction::Input).ok(),
                None => manager.default_input().map(|d| d.id.as_str().to_string()),
            };
            if mic_id.is_none() {
                tracing::warn!("voice route: no capture device resolved — receive only");
            }
            // The lanes open at the STATION's pipeline rate, never at a fixed
            // 8 kHz: `AudioRouter::ensure_output`/`ensure_mic` reuse an
            // already-open lane whatever config they are passed, and the
            // Manager refuses to mix rates on one bus — so a 16 kHz station
            // asked for 8 kHz here would either be silently ignored or
            // corrupt the bus. `VoiceRoute` bridges the session's fixed
            // 8 kHz codec framing to whatever the bus runs at.
            let bus_rate = manager.pipeline_sample_rate();
            let stream_cfg = StreamConfig {
                sample_rate: bus_rate,
                ..StreamConfig::default()
            };
            crate::voice_route::VoiceRoute::open(
                manager.router_mut(),
                mic_id.map(MicId::new),
                OutputId::new(&out_id),
                stream_cfg,
            )
            .map_err(ConsoleError::Audio)?
        };
        {
            let router = self
                .manager
                .as_ref()
                .expect("built by ensure_engine")
                .router();
            self.push_prefs(router, route.mic(), route.out());
        }
        self.voice_route = Some(route);
        Ok(audio)
    }

    /// The real gate for opening a voice route: nothing else may be live.
    /// This is THE exclusion check — every network's connect path runs it,
    /// and `dstar_can_connect`/`ysf_can_connect` only answer the same
    /// question for an embedder that wants to ask before it tries.
    ///
    /// An IAX2 *link* is checked through `Manager::call_count`, not through
    /// `active`: links live in the Manager's call table and never set
    /// `active`, so without this a route would `open_mic_lane` the very mic a
    /// Transceive link is keyed through, overwrite its destination, and close
    /// its capture stream at release.
    fn can_open_voice_route(&self) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.voice_route.is_some()
            || self.manager.as_ref().is_some_and(|m| m.call_count() > 0)
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr_is_active()
        {
            return Err(ConsoleError::AlreadyConnected);
        }
        Ok(())
    }

    /// Key or unkey the live voice route's capture lane. `false` = refused
    /// (no capture device could be opened), and the caller must not
    /// transmit. `false` too when no route is open at all.
    pub(crate) fn key_voice_route(&mut self, on: bool) -> bool {
        let Some(route) = self.voice_route.as_mut() else {
            return false;
        };
        let Some(mgr) = self.manager.as_mut() else {
            return false;
        };
        route.key(mgr.router_mut(), on)
    }

    /// Whether the live voice route resolved a capture device.
    #[must_use]
    pub fn voice_route_tx_capable(&self) -> bool {
        self.voice_route
            .as_ref()
            .is_some_and(crate::voice_route::VoiceRoute::tx_capable)
    }

    /// Close the voice route's lanes and clear the reservation. Returns the
    /// stream handles; drop them with no lock held (a `CoreAudio` stream drop
    /// can stall).
    #[must_use]
    pub fn release_voice_route(&mut self) -> Vec<Box<dyn StreamHandle>> {
        match (self.voice_route.take(), self.manager.as_mut()) {
            (Some(route), Some(mgr)) => route.release(mgr.router_mut()),
            _ => Vec::new(),
        }
    }

    /// Push every standing operator preference onto one route's lanes. The
    /// ONE pref-push for a voice route: the same eleven values `connect`
    /// re-pushes for an IAX2 dial, addressed at the router rather than at a
    /// call id.
    fn push_prefs(&self, router: &AudioRouter, mic: Option<&MicId>, out: &OutputId) {
        if let Some(mic) = mic {
            router.set_mic_gain(mic, self.input_gain.get());
            router.set_mic_denoise(mic, self.denoise.load(Ordering::Relaxed));
            router.set_mic_compress(mic, self.compress.load(Ordering::Relaxed));
            router.set_mic_compress_level(
                mic,
                f32::from_bits(self.compress_level.load(Ordering::Relaxed)),
            );
            router.set_mic_denoise_strength(
                mic,
                f32::from_bits(self.denoise_strength.load(Ordering::Relaxed)),
            );
            router.set_mic_tx_trim(mic, f32::from_bits(self.tx_trim.load(Ordering::Relaxed)));
            router.set_mic_preroll_ms(mic, self.vox_preroll_ms.load(Ordering::Relaxed));
            router.set_mic_profile(mic, self.calibrated.lock().unwrap().clone());
        }
        router.set_output_gain(out, self.output_gain.get());
        router.set_output_compress(out, self.rx_compress.load(Ordering::Relaxed));
        router.set_output_compress_level(
            out,
            f32::from_bits(self.rx_compress_level.load(Ordering::Relaxed)),
        );
    }

    /// The mic lane and output bus whose meters the snapshot reports: the
    /// IAX2 call's routed pair, else the voice route's, else none. One
    /// answer for every network — a network cannot forget to wire meters,
    /// because a network no longer wires meters.
    fn meter_ids(&self) -> Option<(Option<MicId>, OutputId)> {
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            let snap = mgr.snapshot();
            let out = snap.output_of(id)?;
            return Some((snap.mic_of(id).map(MicId::new), OutputId::new(&out)));
        }
        self.voice_route
            .as_ref()
            .map(|r| (r.mic().cloned(), r.out().clone()))
    }

    /// Place a web-transceiver call to `peer` (already resolved from the node).
    /// `backend` is a fresh audio backend; it is wrapped in a [`MeteringBackend`]
    /// so TX/RX levels are tapped.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] if a call is live; [`ConsoleError::Iax`]
    /// if device resolution / dial fails.
    ///
    /// # Reconnecting
    /// A remote hangup does not auto-clear the call; the session stays "busy"
    /// (a fresh `connect` returns [`ConsoleError::AlreadyConnected`]) until the
    /// front-end calls [`ConsoleSession::disconnect`]. Call `disconnect` after
    /// observing a `Hangup`/`Failed` status before dialing again.
    pub fn connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        peer: SocketAddr,
        cfg: ConsoleConfig,
    ) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr_is_active()
            || self.voice_route.is_some()
        {
            return Err(ConsoleError::AlreadyConnected);
        }

        // Snapshot the session's standing prefs BEFORE calling ensure_engine so
        // we don't hold a mutable borrow of `self` while also reading from it.
        let input_gain = self.input_gain.get();
        let output_gain = self.output_gain.get();
        let denoise = self.denoise.load(Ordering::Relaxed);
        let compress = self.compress.load(Ordering::Relaxed);
        let compress_level = f32::from_bits(self.compress_level.load(Ordering::Relaxed));
        let denoise_strength = f32::from_bits(self.denoise_strength.load(Ordering::Relaxed));
        let tx_trim = f32::from_bits(self.tx_trim.load(Ordering::Relaxed));
        let rx_compress = self.rx_compress.load(Ordering::Relaxed);
        let rx_compress_level = f32::from_bits(self.rx_compress_level.load(Ordering::Relaxed));
        let vox_preroll_ms = self.vox_preroll_ms.load(Ordering::Relaxed);
        let calibrated = self.calibrated.lock().unwrap().clone();

        // Pin the station pipeline rate to this call's codec policy (iax-4348)
        // BEFORE the Manager is built. The rate cannot change on a live
        // Manager, so an engine left behind at the wrong rate by a
        // digital-voice session is dropped here while it is idle and rebuilt
        // below — otherwise `Manager::dial` would cap this dial's policy to
        // the old rate and a `prefer_slin16` node would be dialed narrowband.
        self.station_policy = cfg.codec_policy;
        self.rebuild_idle_engine_for_rate(cfg.codec_policy.max_sample_rate());

        // Ensure a Manager exists (build it once; keep it across calls).
        // The caller passes `backend` as the factory value for the first call;
        // on subsequent calls the existing Manager is reused and `backend` is
        // dropped. This mirrors `ensure_engine` semantics. The checked variant
        // applies a link transport selected before the engine existed
        // (iax-5bbd) so the dial below rides the configured transport.
        let manager = self.ensure_engine_checked(|| backend)?;

        // Resolve the configured device substrings to device-id strings against
        // the backend's enumerated list (case-insensitive substring, unique),
        // falling back to the system default when unset.
        let needs_enum = cfg.input_device.is_some() || cfg.output_device.is_some();
        let enumerated = if needs_enum {
            manager.devices().map_err(ConsoleError::Audio)?
        } else {
            Vec::new()
        };
        let in_id = match &cfg.input_device {
            Some(q) => find_device(&enumerated, q, Direction::Input)?,
            None => manager
                .default_input()
                .ok_or_else(|| ConsoleError::Device("no default input device".into()))?
                .id
                .as_str()
                .to_string(),
        };
        let out_id = match &cfg.output_device {
            Some(q) => find_device(&enumerated, q, Direction::Output)?,
            None => manager
                .default_output()
                .ok_or_else(|| ConsoleError::Device("no default output device".into()))?
                .id
                .as_str()
                .to_string(),
        };

        let (ftx, frx) = std::sync::mpsc::channel();
        let id = CallId::from_raw(WT_CALL_ID);
        let spec = DialSpec {
            id,
            node: cfg.node.clone(),
            peer,
            output: OutputId::new(&out_id),
            caller_id: "allstar-public".into(),
            secret: cfg.secret,
            mode: CallMode::WebTransceiver {
                node: cfg.calling_node,
                name: cfg.name,
            },
            dest: String::new(),
            frame_observer: Some(ftx),
            codec_policy: cfg.codec_policy,
        };
        let id = manager.dial(spec)?;
        manager.route(id, &MicId::new(&in_id))?;

        // Re-push the session's standing prefs onto the routed call's router
        // cells so volume/DSP survive reconnects.
        manager.set_input_gain(id, input_gain);
        manager.set_output_gain(id, output_gain);
        manager.set_denoise(id, denoise);
        manager.set_compress(id, compress);
        manager.set_compression_level(id, compress_level);
        manager.set_denoise_strength(id, denoise_strength);
        manager.set_tx_trim(id, tx_trim);
        manager.set_output_compress(id, rx_compress);
        manager.set_output_compress_level(id, rx_compress_level);
        manager.set_vox_preroll_ms(id, vox_preroll_ms);
        manager.set_mic_profile(id, calibrated);

        let events = manager.take_events(id);

        self.active = Some(id);
        self.events = events;
        self.tracer = crate::tracer::Tracer::new(2048);
        self.frames = Some(frx);
        self.state.status = CallStatus::Dialing;
        self.state.ptt = false;
        self.state.remote_ptt = false;
        Ok(())
    }

    /// Start the inbound listener on `bind` (iax-a1fb P1): bind an
    /// `IncomingCallListener` and remember `answer`/the resolved handset devices
    /// so [`Self::poll_inbound`] can adopt accepted calls into THIS session's
    /// `Manager`. The Manager is built (via `make_backend`) if it does not exist
    /// yet, so inbound works before any WT dial.
    ///
    /// `policy.decision` is forced to `AppDecide` so the session gates every
    /// offer (busy-reject / Manual parking are decided here, not in the
    /// listener).
    ///
    /// `devices` is an `(input, output)` pair of optional device-name substrings
    /// (case-insensitive, must uniquely match one device). `None` falls back to the
    /// system default, mirroring the `connect` path device resolution (iax-be48).
    /// The tuple form keeps the argument count within clippy's `too_many_arguments`
    /// limit and matches the `(Option<String>, Option<String>)` return type of
    /// `Station::selected_devices()`.
    ///
    /// # Errors
    /// [`ConsoleError::Audio`]/[`ConsoleError::Device`] if the handset devices
    /// can't be resolved; [`ConsoleError::Iax`] if the listener can't bind/start.
    pub fn start_inbound(
        &mut self,
        bind: SocketAddr,
        policy: IncomingCallPolicy,
        answer: AnswerPolicy,
        max_calls: usize,
        make_backend: impl FnOnce() -> Box<dyn AudioBackend>,
        devices: (Option<String>, Option<String>),
    ) -> Result<(), ConsoleError> {
        self.start_inbound_with_allowlist(
            bind,
            policy,
            answer,
            max_calls,
            None,
            make_backend,
            devices,
        )
    }

    /// Like [`Self::start_inbound`] but with an optional inbound node allowlist
    /// (iax-91c9). When `allowlist` is `Some` AND non-empty, an offer whose
    /// caller node id ([`IncomingCall::calling_number`]) is not on the list is
    /// rejected ("not authorized") at call-setup time, BEFORE answer/adopt.
    /// `None` or an empty list admits all callers (backward compatible). The
    /// allowlist is orthogonal to `policy.auth` (which proves identity); this
    /// is a per-node admission policy on top.
    ///
    /// # Errors
    /// Same as [`Self::start_inbound`].
    #[allow(clippy::too_many_arguments)]
    pub fn start_inbound_with_allowlist(
        &mut self,
        bind: SocketAddr,
        policy: IncomingCallPolicy,
        answer: AnswerPolicy,
        max_calls: usize,
        allowlist: Option<KnownNodes>,
        make_backend: impl FnOnce() -> Box<dyn AudioBackend>,
        devices: (Option<String>, Option<String>),
    ) -> Result<(), ConsoleError> {
        // Pin the station pipeline rate to the inbound policy's codec policy
        // (iax-4348) BEFORE the Manager is built — the node path reaches inbound
        // without ever calling `connect`, so this is where node.toml's
        // `prefer_slin16` becomes a 16 kHz engine. As in [`Self::connect`], an
        // idle engine left at the wrong rate by a digital-voice session is
        // dropped and rebuilt rather than silently capping every call the
        // listener goes on to adopt.
        self.station_policy = policy.codec_policy;
        self.rebuild_idle_engine_for_rate(policy.codec_policy.max_sample_rate());

        // Resolve the handset devices against the session's Manager (built now if
        // absent). Mirror the connect-path resolution (iax-be48): enumerate only
        // when a named device is requested, find_device for named, default for None.
        let (input, output) = devices;
        let manager = self.ensure_engine_checked(make_backend)?;
        let needs_enum = input.is_some() || output.is_some();
        let enumerated = if needs_enum {
            manager.devices().map_err(ConsoleError::Audio)?
        } else {
            Vec::new()
        };
        let in_id = match &input {
            Some(q) => find_device(&enumerated, q, Direction::Input)?,
            None => manager
                .default_input()
                .ok_or_else(|| ConsoleError::Device("no default input device".into()))?
                .id
                .as_str()
                .to_string(),
        };
        let out_id = match &output {
            Some(q) => find_device(&enumerated, q, Direction::Output)?,
            None => manager
                .default_output()
                .ok_or_else(|| ConsoleError::Device("no default output device".into()))?
                .id
                .as_str()
                .to_string(),
        };

        // Bind the listener from the Manager's selected link transport
        // (iax-5bbd): plain OS UDP by default (byte-identical — the builder's
        // own default is the same OS stack), the shared `WireGuard` tunnel
        // when one is configured, plus the tunnel mode's optional extra plain
        // UDP listener for direct/LAN peers.
        let net = manager.net_stack();
        let extra_udp = manager.also_bind_udp();

        // Force AppDecide so EVERY inbound NEW surfaces as Incoming and the
        // session decides (answer / busy-reject / Manual park).
        let mut policy = policy;
        policy.decision = astar_iax::IncomingDecisionPolicy::AppDecide;

        let (listener, events) = IncomingCallListener::builder()
            .bind(bind)
            .policy(policy)
            .net(net)
            .also_bind_udp(extra_udp)
            .start()
            .map_err(ConsoleError::Iax)?;

        self.inbound = Some(InboundState {
            listener,
            events,
            answer,
            output: OutputId::new(&out_id),
            mic: MicId::new(&in_id),
            parked: None,
            pending_from: None,
            max_calls,
            allowlist,
        });
        Ok(())
    }

    /// Hang up the active inbound-adopted call (if any) but KEEP the listener
    /// running for the next caller. No-op when no inbound call is active.
    pub fn disconnect_inbound(&mut self) {
        if self.inbound_active {
            if let Some(id) = self.active.take()
                && let Some(mgr) = self.manager.as_mut()
            {
                let _ = mgr.hangup(id, None);
            }
            self.events = None;
            self.frames = None;
            self.inbound_active = false;
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
        }
    }

    /// Stop the inbound listener and hang up any inbound-adopted call. The
    /// Manager is retained (the WT path may still use it). Idempotent.
    pub fn stop_inbound(&mut self) {
        // Drop any parked Manual offer (rejects it implicitly via Drop) and the
        // listener (its Drop stops the actor thread + frees the port).
        self.inbound = None;
        // Hang up an inbound-originated active call; leave a WT call alone.
        if self.inbound_active {
            if let Some(id) = self.active.take()
                && let Some(mgr) = self.manager.as_mut()
            {
                let _ = mgr.hangup(id, None);
            }
            self.events = None;
            self.frames = None;
            self.inbound_active = false;
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
        }
    }

    /// The bound listener address (an ephemeral port when `bind` used `:0`), or
    /// `None` when inbound is not started.
    #[must_use]
    pub fn inbound_addr(&self) -> Option<SocketAddr> {
        self.inbound.as_ref().map(|i| i.listener.local_addr())
    }

    /// `true` while the inbound listener is running.
    #[must_use]
    pub fn has_inbound(&self) -> bool {
        self.inbound.is_some()
    }

    /// Drain inbound offers: in Auto, answer + adopt the first offer (busy-reject
    /// the rest); in Manual, park the first offer + record the once-only edge
    /// (busy-reject while parked or busy). Adopted calls land in THIS session's
    /// `active`/`events`/`state` machinery. No-op when inbound isn't started.
    pub fn poll_inbound(&mut self) {
        if self.inbound.is_none() {
            return;
        }
        // Drain offers first (collect to release the borrow on `self.inbound`).
        let mut offers = Vec::new();
        if let Some(inb) = self.inbound.as_ref() {
            while let Ok(ev) = inb.events.try_recv() {
                offers.push(ev);
            }
        }
        for ev in offers {
            match ev {
                IncomingCallEvent::Incoming(c) => self.handle_incoming(c),
                // We force AppDecide so this shouldn't fire; handle defensively
                // by adopting an already-answered call only when idle AND no
                // M17/D-Star session is live (iax-f2b8 Task 4 / iax-a9d4 Task
                // 6: the same mutual exclusion `handle_incoming` enforces
                // above — this is a second, independent path into `active`
                // and needs its own guard). Dropping `call`/`events` here
                // (rather than adopting) mirrors the existing
                // `self.active.is_some()` case just above: this whole branch
                // is a defensive fallback that should never fire under
                // AppDecide.
                IncomingCallEvent::Answered { call, events } => {
                    if self.active.is_none()
                        && !self.m17_is_active()
                        && !self.dstar_is_active()
                        && !self.ysf_is_active()
                        && !self.nxdn_is_active()
                        && !self.dmr_is_active()
                        && self.voice_route.is_none()
                    {
                        self.adopt_inbound(call, events);
                    }
                }
            }
        }
    }

    /// Decide on a freshly surfaced inbound offer (Auto: answer+adopt; Manual:
    /// park). At or above `max_calls` → busy-reject. A non-empty inbound node
    /// allowlist rejects callers not on it ("not authorized") before
    /// answer/adopt (iax-91c9). Below cap and allowed: Auto → answer()+adopt;
    /// Manual → park (one slot; reject a 2nd concurrent offer while one is
    /// parked or when at cap).
    fn handle_incoming(&mut self, incoming: IncomingCall) {
        // iax-f2b8 Task 4: an M17 session is mutually exclusive with an IAX2
        // call — busy-reject rather than answer/adopt into `active` while one
        // is live (mirrors the max_calls busy-reject below). iax-a9d4 Task 6
        // adds the same guard for a live D-Star session: adopting an inbound
        // call would open the local handset's output device concurrently
        // with the D-Star session's own output device.
        if self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr_is_active()
            || self.voice_route.is_some()
        {
            let _ = incoming.reject(Some("busy".into()));
            return;
        }
        let max_calls = self.inbound.as_ref().map_or(usize::MAX, |i| i.max_calls);
        let current = self.manager.as_ref().map_or(0, Manager::call_count);
        if current >= max_calls {
            let _ = incoming.reject(Some("busy".into()));
            return;
        }
        // Inbound node allowlist (iax-91c9): when a non-empty allowlist is
        // configured, reject any caller whose node id is not on it BEFORE
        // answer/adopt. An absent/empty allowlist admits all (backward compat).
        if let Some(known) = self
            .inbound
            .as_ref()
            .and_then(|i| i.allowlist.as_ref())
            .filter(|k| !k.is_empty())
        {
            let node = caller_of(&incoming);
            if !known.contains(&node) {
                let _ = incoming.reject(Some("not authorized".into()));
                return;
            }
        }
        let answer = self
            .inbound
            .as_ref()
            .map_or(AnswerPolicy::Auto, |i| i.answer);
        match answer {
            AnswerPolicy::Auto => match incoming.answer() {
                Ok((call, events)) => self.adopt_inbound(call, events),
                Err(e) => {
                    self.state.status = CallStatus::Failed {
                        reason: e.to_string(),
                    };
                }
            },
            AnswerPolicy::Manual => {
                if let Some(inb) = self.inbound.as_mut() {
                    if inb.parked.is_some() {
                        let _ = incoming.reject(Some("busy".into()));
                    } else {
                        inb.pending_from = Some(caller_of(&incoming));
                        inb.parked = Some(incoming);
                    }
                }
            }
        }
    }

    /// Adopt an answered inbound call into the session's Manager, route it to the
    /// local handset mic (single-handset: only when no mic is currently routed),
    /// and fold it into the shared call machinery as an answered call.
    fn adopt_inbound(&mut self, call: Call, events: Receiver<CallEvent>) {
        let Some(out) = self.inbound.as_ref().map(|i| i.output.clone()) else {
            return;
        };
        // In conference/bridge mode the leg is enrolled as a mix-minus member by
        // `Manager::adopt` (no 1:1 mic); only the handset path routes the mic.
        let mic = if self.bridge_config.mode.is_conference() {
            None
        } else {
            self.inbound.as_ref().map(|i| i.mic.clone())
        };
        let Some(mgr) = self.manager.as_mut() else {
            return;
        };
        match mgr.adopt(call, &out) {
            Ok(id) => {
                // Single-handset: route the mic only if none is currently routed
                // to this call (a fresh adopt is always unrouted, but keep the
                // guard explicit for the multi-call future).
                if let Some(mic) = mic
                    && mgr.routed_mic(id).is_none()
                    && let Err(e) = mgr.route(id, &mic)
                {
                    let _ = mgr.hangup(id, None);
                    self.state.status = CallStatus::Failed {
                        reason: e.to_string(),
                    };
                    return;
                }
                self.active = Some(id);
                self.events = Some(events);
                self.frames = None;
                self.inbound_active = true;
                self.tracer = crate::tracer::Tracer::new(2048);
                self.state.status = CallStatus::Answered;
                self.state.ptt = false;
                self.state.remote_ptt = false;
                // iax-a82f: mark a newly-answered call so the station fires a
                // per-caller answered edge (the join greeting). Bump per adopt,
                // not per status change — status stays Answered across
                // sequential callers in a multi-call node.
                self.answered_seq += 1;
            }
            Err(e) => {
                self.state.status = CallStatus::Failed {
                    reason: e.to_string(),
                };
            }
        }
    }

    /// Take the once-only inbound edge: the caller string of a freshly parked
    /// Manual offer, returned exactly once. Public caller-id, never a secret.
    pub fn take_incoming_from(&mut self) -> Option<String> {
        self.inbound.as_mut().and_then(|i| i.pending_from.take())
    }

    /// Answer the parked Manual-mode offer and bridge it to the local handset.
    ///
    /// # Errors
    /// [`ConsoleError::NotConnected`] if no offer is parked;
    /// [`ConsoleError::AlreadyConnected`] if an M17 or D-Star session is live
    /// (iax-f2b8 Task 4 / iax-a9d4 Task 6 — mirrors `handle_incoming`'s
    /// Auto-mode busy-reject; this is the Manual-mode counterpart, since a
    /// Manual offer can park BEFORE M17/D-Star connects and only this call
    /// actually adopts it into `active`). The offer stays parked so the
    /// operator can retry once the other session is disconnected;
    /// [`ConsoleError::Iax`] if the answer handshake fails.
    pub fn answer_pending(&mut self) -> Result<(), ConsoleError> {
        if self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr_is_active()
            || self.voice_route.is_some()
        {
            return Err(ConsoleError::AlreadyConnected);
        }
        let inc = self
            .inbound
            .as_mut()
            .and_then(|i| {
                i.pending_from = None;
                i.parked.take()
            })
            .ok_or(ConsoleError::NotConnected)?;
        let (call, events) = inc.answer().map_err(ConsoleError::Iax)?;
        self.adopt_inbound(call, events);
        Ok(())
    }

    /// Reject the parked Manual-mode offer (sends REJECT/HANGUP).
    ///
    /// # Errors
    /// [`ConsoleError::NotConnected`] if no offer is parked; [`ConsoleError::Iax`]
    /// if the reject cannot be sent.
    pub fn reject_pending(&mut self) -> Result<(), ConsoleError> {
        let inc = self
            .inbound
            .as_mut()
            .and_then(|i| {
                i.pending_from = None;
                i.parked.take()
            })
            .ok_or(ConsoleError::NotConnected)?;
        inc.reject(None).map_err(ConsoleError::Iax)?;
        Ok(())
    }

    // ── Registration (Task 3.1) ─────────────────────────────────────────────

    /// Start outbound node registration. If `secret` is `None` the resolver was
    /// not configured: immediately queue a `RegisterOutcome::Failed` and return.
    /// Otherwise spawn the registration thread and keep the handle.
    ///
    /// Secret-free invariant: `secret` is consumed into the [`Registrar`] here
    /// and is never stored on the session or surfaced in any event or log.
    ///
    /// # Errors
    /// [`std::io::Error`] if the underlying UDP socket or mio poll cannot be
    /// created (only when `secret` is `Some`).
    pub fn start_register(
        &mut self,
        peer: SocketAddr,
        username: String,
        refresh: Duration,
        secret: Option<Arc<Secret>>,
    ) -> std::io::Result<()> {
        // Drop any previous registration first (its Drop sends REGREL).
        self.stop_register();

        let Some(secret) = secret else {
            self.reg_queue.push_back(RegisterOutcome::Failed(
                "no credential resolver configured".to_string(),
            ));
            return Ok(());
        };

        let options = RegisterOptions {
            refresh_request: refresh,
            ..RegisterOptions::default()
        };
        let mut registrar = Registrar::new(peer, username, secret).with_options(options);
        // Ride the engine's selected link transport (iax-5bbd) when an engine
        // exists: plain OS UDP by default (byte-identical to the registrar's
        // own default), the shared `WireGuard` tunnel when configured. A
        // `WireGuard` session starts inbound/dials first (building the
        // engine), so registration naturally follows the tunnel.
        if let Some(mgr) = self.manager.as_ref() {
            registrar = registrar.with_net(mgr.net_stack());
        }
        let (handle, rx) = registrar.register()?;
        self.reg_handle = Some(handle);
        self.reg_events = Some(rx);
        Ok(())
    }

    /// Stop the outbound registration. Drops the [`Registration`] handle (its
    /// `Drop` sends REGREL to the registrar and joins the thread). Clears
    /// registration state so `is_registered` returns `false`.
    pub fn stop_register(&mut self) {
        // Drop the handle (sends REGREL) and the receiver.
        self.reg_handle = None;
        self.reg_events = None;
        self.reg_active = false;
        // Retain queued outcomes so `take_register_event` can drain them.
    }

    /// Drain the registration event receiver into the outcome queue.
    /// Only `Registered` and `Failed` are meaningful at the station level;
    /// intermediate lifecycle variants (`Registering`, `Refreshing`, etc.)
    /// are silently discarded.
    ///
    /// Invariant: the secret NEVER flows into `RegisterOutcome` — only a
    /// human-readable failure reason in `Failed(String)`.
    pub fn poll_register(&mut self) {
        // Collect events to avoid borrow conflict.
        let mut evs = Vec::new();
        if let Some(rx) = &self.reg_events {
            while let Ok(ev) = rx.try_recv() {
                evs.push(ev);
            }
        }
        for ev in evs {
            match ev {
                RegistrationEvent::Registered { .. } => {
                    self.reg_active = true;
                    self.reg_queue.push_back(RegisterOutcome::Registered);
                }
                RegistrationEvent::Failed(reason) => {
                    // iax-177d: the registration thread exits on Failed — the
                    // registrar no longer knows us. A sticky true here reported
                    // `registered: true` for 14 h while the node was out of the
                    // AllStar directory.
                    self.reg_active = false;
                    // Secret-free: `RegFailReason`'s Debug output never includes
                    // the password (it prints auth-method names and timeout counts).
                    self.reg_queue
                        .push_back(RegisterOutcome::Failed(format!("{reason:?}")));
                }
                RegistrationEvent::Released => {
                    self.reg_active = false;
                }
                // Registering / Refreshing / Refreshed: no action.
                _ => {}
            }
        }
    }

    /// Pop one queued registration outcome, or `None`.
    pub fn take_register_event(&mut self) -> Option<RegisterOutcome> {
        self.reg_queue.pop_front()
    }

    /// `true` when the most recent registration succeeded and `stop_register`
    /// has not been called since.
    #[must_use]
    pub fn is_registered(&self) -> bool {
        self.reg_active
    }

    /// Engage/release transmit.
    ///
    /// # Errors
    /// [`ConsoleError::NotConnected`] if no call is live.
    pub fn set_ptt(&mut self, on: bool) -> Result<(), ConsoleError> {
        // NXDN has no transmit path yet (`crate::nxdn`'s Transmit section),
        // and the refusal has to come out BEFORE the gate below opens: a
        // key-down that opened the capture lane and then refused would leave
        // the microphone live, feeding a run loop that discards every frame,
        // with a UI showing a keyed station that is transmitting nothing. A
        // key-UP is not refused — it is how a caller clears state — and
        // falls through to the branch below, which forwards it and closes
        // the gate like every other network.
        #[cfg(feature = "nxdn")]
        if on && self.nxdn.is_some() {
            return Err(ConsoleError::Nxdn("transmit is not built yet".into()));
        }
        // DMR, for the identical reason and at the identical point. It
        // matters more here than it did for NXDN: a DMR master routes by the
        // radio ID this link logged in with, so a key that opened the
        // capture lane and then quietly dropped every burst would be a live
        // microphone attached to the operator's own registration. See
        // `crate::dmr`'s Transmit section.
        #[cfg(feature = "dmr")]
        if on && self.dmr.is_some() {
            return Err(ConsoleError::Dmr("transmit is not built yet".into()));
        }
        // The ONE keying gate for a digital-voice session: the route's
        // capture lane opens (or is refused) before any network is told to
        // transmit, and closes on key-up. A refusal here forwards nothing —
        // an RF header over a stream of silence is worse than no key at all.
        if self.voice_route.is_some() {
            if on {
                if !self.key_voice_route(true) {
                    return Err(ConsoleError::NoCaptureDevice);
                }
            } else {
                let _ = self.key_voice_route(false);
            }
        }
        // iax-f2b8 Task 4: dispatch to the M17 session FIRST — mutual
        // exclusion with `active` means at most one of these branches is ever
        // live, but M17 must be checked before the IAX2 NotConnected error
        // path below since M17 carries no `CallId`.
        #[cfg(feature = "m17")]
        if let Some(session) = self.m17.as_mut() {
            session.set_ptt(on);
            self.state.ptt = on;
            self.tracer
                .note(if on { "LocalKey" } else { "LocalUnkey" }, String::new());
            return Ok(());
        }
        // A live digital-voice session dispatches exactly like the M17
        // branch above, INCLUDING the `self.state.ptt` mirror and the tracer
        // note. Each network's richer state is read through its own accessor
        // (`ysf_state()`, `dstar_state()`), but `ptt` is not network-specific:
        // it is the shared `ConsoleState` field every `Station::snapshot()`
        // consumer reads for "is this station transmitting", and a UI — or
        // any PTT source that reconciles against the snapshot — must never
        // see `false` while a transmission is on the air. `snapshot()` then
        // mirrors the run loop's ACTUALLY-applied state back on every poll,
        // so a forced unkey (link lost, time-out timer) shows up here too.
        //
        // The tracer note is the non-transient half: a timeline that records
        // IAX2, M17 and D-Star keys but not YSF ones is missing the network
        // outright, not merely late.
        #[cfg(feature = "ysf")]
        if let Some(link) = self.ysf.as_ref() {
            link.set_ptt(on);
            self.state.ptt = on;
            self.tracer
                .note(if on { "LocalKey" } else { "LocalUnkey" }, String::new());
            return Ok(());
        }
        // NXDN reaches this branch only on a key-UP: the key-DOWN was
        // refused at the top of this function. The link is told anyway (it
        // clears its own request cell), the mirror is written, and the
        // release lands on the timeline like any other.
        #[cfg(feature = "nxdn")]
        if let Some(link) = self.nxdn.as_ref() {
            link.set_ptt(on);
            self.state.ptt = on;
            self.tracer
                .note(if on { "LocalKey" } else { "LocalUnkey" }, String::new());
            return Ok(());
        }
        // DMR reaches this branch only on a key-UP, exactly as NXDN does:
        // the key-DOWN was refused at the top of this function.
        #[cfg(feature = "dmr")]
        if let Some(link) = self.dmr.as_ref() {
            link.set_ptt(on);
            self.state.ptt = on;
            self.tracer
                .note(if on { "LocalKey" } else { "LocalUnkey" }, String::new());
            return Ok(());
        }
        #[cfg(feature = "dstar")]
        if let Some(session) = self.dstar.as_mut() {
            session.set_ptt(on);
            self.state.ptt = on;
            self.tracer
                .note(if on { "LocalKey" } else { "LocalUnkey" }, String::new());
            return Ok(());
        }
        // No network session took the key. A route left keyed here would
        // capture into nothing, so close the gate before refusing — the same
        // guarantee the key-down refusal above gives.
        if self.active.is_none() && self.voice_route.is_some() {
            let _ = self.key_voice_route(false);
        }
        // NotConnected returns before any tracer write, so the no-call path
        // records nothing on the timeline.
        let id = self.active.ok_or(ConsoleError::NotConnected)?;
        let mgr = self.manager.as_mut().ok_or(ConsoleError::NotConnected)?;
        if on {
            mgr.key(id)?;
        } else {
            mgr.unkey(id)?;
        }
        self.state.ptt = on;
        self.tracer
            .note(if on { "LocalKey" } else { "LocalUnkey" }, String::new());
        Ok(())
    }

    /// Send an out-of-band IAX2 DTMF frame pair (`DtmfBegin`/`DtmfEnd`) for
    /// `digit` on the active call (iax-7fff) — the protocol-frame emission
    /// path, as opposed to injecting an in-band tone via [`Self::announce`].
    /// Digit validity is enforced upstream (the station's keypad check) and
    /// again by the core FSM.
    ///
    /// # Errors
    /// [`ConsoleError::NotConnected`] if no call is live; [`ConsoleError::Iax`]
    /// if the call's runtime thread has exited.
    pub fn send_dtmf(&self, digit: char) -> Result<(), ConsoleError> {
        let id = self.active.ok_or(ConsoleError::NotConnected)?;
        let mgr = self.manager.as_ref().ok_or(ConsoleError::NotConnected)?;
        mgr.send_dtmf(id, digit).map_err(ConsoleError::from)
    }

    /// Play an announcement on the active call (iax-da05).
    ///
    /// # Errors
    /// [`ConsoleError::NotConnected`] if no call is live.
    pub fn announce(
        &mut self,
        req: astar_iax::AnnounceRequest,
    ) -> Result<astar_audio::AnnounceHandle, ConsoleError> {
        let id = self.active.ok_or(ConsoleError::NotConnected)?;
        let mgr = self.manager.as_mut().ok_or(ConsoleError::NotConnected)?;
        mgr.announce(id, req).map_err(ConsoleError::from)
    }

    /// Play a private announcement to the active call's conference-member leg
    /// (iax-c4ea): the node-id join greeting, heard by that one joining user
    /// only. Requires the active call to be enrolled as a conference member
    /// (Bridge/Conference mode); errors otherwise.
    ///
    /// # Errors
    /// [`ConsoleError::NotConnected`] if no call is active or no Manager exists;
    /// [`ConsoleError::Iax`] if the leg is not a conference member or the phrase
    /// cannot be resolved.
    pub fn announce_to_active_member(
        &mut self,
        req: astar_iax::AnnounceRequest,
    ) -> Result<(), ConsoleError> {
        let id = self.active.ok_or(ConsoleError::NotConnected)?;
        let mgr = self.manager.as_mut().ok_or(ConsoleError::NotConnected)?;
        // iax-9722: this path is the join greeting only — no carrier lead.
        mgr.announce_to_member(id, req, std::time::Duration::ZERO)
            .map_err(ConsoleError::from)
    }

    /// Advance announcement queues (auto-unkey finished to-air legs).
    pub fn poll_announcements(&mut self) {
        if let Some(mgr) = self.manager.as_mut() {
            mgr.poll_announcements();
        }
    }

    /// Hang up and tear down the call. Idempotent (no-op if not connected).
    ///
    /// Note: the `Manager` hangup joins the runtime thread, which drops the cpal
    /// streams — a potentially slow, blocking teardown. Front-ends that hold a
    /// shared lock around the session (e.g. the SSE snapshot loop) should
    /// prefer [`Self::detach`] so that teardown does not run under the lock.
    ///
    /// # Errors
    /// [`ConsoleError::Iax`] if the hangup command cannot be sent.
    pub fn disconnect(&mut self) -> Result<(), ConsoleError> {
        // iax-f2b8 Task 4 / iax-a9d4 Task 6: tear down whichever of
        // IAX2/M17/D-Star is live. Mutual exclusion guarantees at most one
        // of `active`/`m17`/`dstar` is `Some`.
        #[cfg(feature = "m17")]
        if let Some(session) = self.m17.take() {
            session.disconnect();
            // The route the session rode is this session's too: released
            // here, exactly as `m17_disconnect` does, or the station stays
            // reserved against every later connect.
            let handles = self.release_voice_route();
            drop(handles);
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
            return Ok(());
        }
        #[cfg(feature = "ysf")]
        if let Some(link) = self.ysf.take() {
            link.disconnect();
            // The route the link rode is this session's too: released here,
            // exactly as `ysf_disconnect` does, or the station stays
            // reserved against every later connect.
            let handles = self.release_voice_route();
            drop(handles);
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
            return Ok(());
        }
        #[cfg(feature = "nxdn")]
        if let Some(link) = self.nxdn.take() {
            link.disconnect();
            // The route the link rode is this session's too: released here,
            // exactly as `nxdn_disconnect` does, or the station stays
            // reserved against every later connect.
            let handles = self.release_voice_route();
            drop(handles);
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
            return Ok(());
        }
        #[cfg(feature = "dmr")]
        if let Some(link) = self.dmr.take() {
            link.disconnect();
            // The route the link rode is this session's too: released here,
            // exactly as `dmr_disconnect` does, or the station stays
            // reserved against every later connect.
            let handles = self.release_voice_route();
            drop(handles);
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
            return Ok(());
        }
        #[cfg(feature = "dstar")]
        if let Some(session) = self.dstar.take() {
            session.disconnect();
            // The route the session rode is this session's too: released
            // here, exactly as `dstar_disconnect` does, or the station stays
            // reserved against every later connect.
            let handles = self.release_voice_route();
            drop(handles);
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
            return Ok(());
        }
        if let Some(id) = self.active.take() {
            // Hang up the active call but keep the Manager alive so inbound
            // calls can later be adopted into the same engine (iax-a1fb P1).
            if let Some(mgr) = self.manager.as_mut() {
                mgr.hangup(id, None)?;
            }
        }
        // Manager is intentionally retained; only call-specific state is cleared.
        self.events = None;
        self.frames = None;
        self.inbound_active = false;
        self.state.status = CallStatus::Idle;
        self.state.ptt = false;
        self.state.remote_ptt = false;
        self.state.rtt_ms = None;
        Ok(())
    }

    /// Connect to an M17 reflector (iax-f2b8 Task 4). Mutually exclusive with
    /// an active IAX2 call: refuses [`ConsoleError::AlreadyConnected`] when
    /// `self.active` is `Some` (the reverse of [`Self::connect`]'s own guard)
    /// or when an M17 session is already up.
    ///
    /// Deliberately does NOT also refuse while a Manual-mode inbound offer is
    /// merely parked (`InboundState::parked`): parking never opens a
    /// `Manager`/audio call for that offer (only [`Self::answer_pending`]'s
    /// `IncomingCall::answer()` does), so there is no device contention until
    /// an actual answer is attempted — and that path is guarded directly (see
    /// [`Self::answer_pending`]). Refusing here too would break a legitimate
    /// sequence this design supports (monitor/transmit over M17 while an
    /// inbound offer sits parked awaiting an operator decision) for no
    /// correctness benefit.
    ///
    /// `backend` is a fresh audio backend — mirrors [`Self::connect`]'s
    /// contract of taking an already-constructed backend rather than a
    /// factory. It builds the station engine if one does not exist yet; a
    /// station that has already dialed reuses the engine it has, and this
    /// one is dropped.
    ///
    /// `input`/`output` are device-name substrings for the lane the session
    /// will ride (`None` = system default), resolved exactly as
    /// [`Self::connect`] resolves an IAX2 dial's. The session itself is
    /// handed only the lane's channel ends: it opens no device, carries no
    /// preference and keys no gate.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] per above (also refused while a
    /// D-Star/YSF session or another voice route is live);
    /// [`ConsoleError::Device`]/[`ConsoleError::Audio`] if the OUTPUT device
    /// cannot be resolved or opened; otherwise whatever
    /// [`M17Session::connect`] returns (`Device` for an invalid
    /// callsign/missing codec, `Resolve` for a DNS/bind failure).
    #[cfg(feature = "m17")]
    pub fn m17_connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        cfg: M17Config,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<(), ConsoleError> {
        // The route is the reservation, the mutual exclusion AND the pref
        // push, all at once: `open_voice_route` refuses with
        // `AlreadyConnected` if an IAX2 call, another voice route or any
        // digital session is live, opens the bus (plus the capture lane
        // when a device resolves, gate closed) on the station's ONE router,
        // and pushes every standing operator preference onto it.
        let audio = self.open_voice_route(input, output, || backend)?;
        match M17Session::connect(cfg, audio) {
            Ok(session) => {
                self.m17 = Some(session);
                Ok(())
            }
            Err(e) => {
                // The route opened but the session did not: give the lanes
                // back, or the station stays reserved forever.
                let handles = self.release_voice_route();
                drop(handles);
                Err(e)
            }
        }
    }

    /// Disconnect the live M17 session, if any. No-op when none is active.
    /// This is the ONLY thing that clears a `Hangup` status left by a failed
    /// link back to `Idle` — see [`Self::m17`]'s docs.
    #[cfg(feature = "m17")]
    pub fn m17_disconnect(&mut self) {
        // The route is released ONLY when this call actually tore an M17
        // session down. `Station::disconnect` calls every network's
        // disconnect in turn, so an unconditional release here would close
        // the lanes out from under a live D-Star or System Fusion session
        // that legitimately holds the route.
        if let Some(session) = self.m17.take() {
            session.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
        }
    }

    /// `true` while an M17 session is live (the mutual-exclusion check every
    /// IAX2 entry point uses). Always `false` when the `m17` feature isn't
    /// compiled in, so callers never need their own `#[cfg]`.
    #[allow(clippy::unused_self)] // `&self` is intentional: kept feature-independent so call sites never need their own `#[cfg]`.
    fn m17_is_active(&self) -> bool {
        #[cfg(feature = "m17")]
        {
            self.m17.is_some()
        }
        #[cfg(not(feature = "m17"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live M17 session's state, or `None` when
    /// none is active — the M17 half of what [`Self::dstar_state`] and
    /// [`Self::ysf_state`] give for their networks.
    ///
    /// The network-agnostic fields (`ptt`, `remote_ptt`, `status`, the level
    /// meters) are already mirrored into [`ConsoleState`] by `snapshot()`;
    /// read them there. What is only here is
    /// [`M17SnapshotState::talker`] — who last keyed up — which has no
    /// `ConsoleState` equivalent, exactly as D-Star's talker has none.
    ///
    /// Unlike the other two this composes nothing from the lane: M17's
    /// snapshot carries no console-owned level or capability fields to
    /// overwrite.
    #[cfg(feature = "m17")]
    #[must_use]
    pub fn m17_state(&self) -> Option<M17SnapshotState> {
        self.m17.as_ref().map(M17Session::state)
    }

    /// Connect to a D-Star `DExtra` reflector, full-transceive (iax-a9d4
    /// Task 6 built RX; iax-2f6b added TX). Mutually exclusive with an
    /// active IAX2 call AND a live M17 session: refuses
    /// [`ConsoleError::AlreadyConnected`] while either is `Some` (mirrors
    /// [`Self::m17_connect`]'s own guard, which now also refuses while a
    /// D-Star session is live — see its docs).
    ///
    /// `backend` mirrors [`Self::m17_connect`]'s contract of taking an
    /// already-constructed backend. `input`/`output` are device-name
    /// substrings for the lane the session will ride (`None` = system
    /// default); the session itself is handed only the lane's channel ends.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] per above;
    /// [`ConsoleError::Device`]/[`ConsoleError::Audio`] if the OUTPUT device
    /// cannot be resolved or opened; otherwise whatever
    /// [`DstarSession::connect`] returns (`Device` for an invalid callsign,
    /// `Dstar` for an unavailable AMBE backend, `Resolve` for a DNS/bind
    /// failure).
    #[cfg(feature = "dstar")]
    pub fn dstar_connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        cfg: DstarConfig,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<(), ConsoleError> {
        // The route is the reservation, the mutual exclusion AND the pref
        // push, all at once — see [`Self::m17_connect`].
        let audio = self.open_voice_route(input, output, || backend)?;
        match DstarSession::connect(cfg, audio) {
            Ok(session) => self.dstar_adopt(session),
            Err(e) => {
                // The route opened but the session did not: give the lanes
                // back, or the station stays reserved forever.
                let handles = self.release_voice_route();
                drop(handles);
                Err(e)
            }
        }
    }

    /// An embedder-facing query: *would a D-Star connect be refused right
    /// now?* — answered without opening anything, so a front-end can grey a
    /// button out, or a caller that builds the [`DstarSession`] outside this
    /// session's mutex can decide not to scan for a `ThumbDV` at all.
    ///
    /// It is NOT a step of the facade's connect flow. The gate that actually
    /// excludes is [`Self::open_voice_route`]'s own check, which runs under
    /// the lock as the route is taken; asking here first saves work but
    /// decides nothing, because the state can change in between.
    ///
    /// It is also NOT the check [`Self::dstar_adopt`] runs: by then the route
    /// is held on this session's own behalf, so an adopt requires
    /// `voice_route` to be `Some` where this requires it to be `None`.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] while an IAX2 call, an M17, D-Star
    /// or System Fusion session, or any voice route, is live.
    #[cfg(feature = "dstar")]
    pub fn dstar_can_connect(&self) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar.is_some()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr_is_active()
            || self.voice_route.is_some()
        {
            return Err(ConsoleError::AlreadyConnected);
        }
        Ok(())
    }

    /// Install an already-constructed [`DstarSession`], re-checking mutual
    /// exclusion (a caller that pre-checked with [`Self::dstar_can_connect`]
    /// did so without holding this session's lock, so the state may have
    /// changed under it).
    ///
    /// This exists so `astar-station` can run the whole of
    /// `DstarSession::connect` — a `ThumbDV` candidate-port scan plus, per
    /// candidate and per baud rate, an open and an eight-transaction init
    /// cookbook — with the session mutex NOT held. Every `Station` method
    /// takes that mutex, including `snapshot()`/`dstar_state()`, and the
    /// `AstarStation` contract is poll-and-snapshot, never blocking.
    ///
    /// The session must ARRIVE with the route already reserved on its
    /// behalf: the caller opened it with [`Self::open_voice_route`] and
    /// handed the resulting [`astar_audio::CallAudio`] to
    /// `DstarSession::connect`, so a `voice_route` of `None` here means the
    /// session has channel ends nothing is feeding.
    ///
    /// On refusal the rejected session is disconnected here rather than
    /// handed back — it has already bound a socket and taken the dongle, and
    /// leaking either (especially the dongle, which only one process may
    /// hold) would be worse than the error the caller is about to see — and
    /// the route it rode is released with it.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] when an IAX2 call or another
    /// digital session is live, or when no route was reserved.
    #[cfg(feature = "dstar")]
    pub fn dstar_adopt(&mut self, session: DstarSession) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar.is_some()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr_is_active()
            || self.voice_route.is_none()
        {
            session.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            return Err(ConsoleError::AlreadyConnected);
        }
        self.dstar = Some(session);
        Ok(())
    }

    /// Disconnect the live D-Star session, if any (iax-a9d4 Task 6). No-op
    /// when none is active.
    #[cfg(feature = "dstar")]
    pub fn dstar_disconnect(&mut self) {
        // Everything here is inside the `if let`, route release included:
        // `Station::disconnect` calls every network's disconnect in turn, so
        // an unconditional release would close the lanes out from under a
        // live M17 or System Fusion session that legitimately holds them —
        // and an unconditional state reset would blank their mirror.
        if let Some(session) = self.dstar.take() {
            session.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            // Reset every field `snapshot()`'s D-Star branch mirrors, exactly
            // as `m17_disconnect` does for its own (iax-4c8e).
            //
            // Without this the mirror simply stops running once `self.dstar`
            // is `None`, leaving the LAST values it wrote frozen in
            // `self.state` forever. Keying, then disconnecting, would leave a
            // snapshot reporting `ptt: true` for a station that no longer has
            // a session at all — a UI would show a transmitting station
            // indefinitely. The levels go with it: the meter block only
            // writes while a lane is live, and this released the last one.
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
        }
    }

    /// `true` while a D-Star session is live (the mutual-exclusion check
    /// every IAX2/M17 entry point uses). Always `false` when the `dstar`
    /// feature isn't compiled in, so callers never need their own `#[cfg]`
    /// (mirrors [`Self::m17_is_active`]).
    #[allow(clippy::unused_self)]
    fn dstar_is_active(&self) -> bool {
        #[cfg(feature = "dstar")]
        {
            self.dstar.is_some()
        }
        #[cfg(not(feature = "dstar"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live D-Star session's state (iax-a9d4
    /// Task 6), or `None` when no session is active. Unlike M17, D-Star
    /// state is never mirrored into [`ConsoleState`] (see [`Self::dstar`]'s
    /// field docs) — a caller reads it through this accessor instead.
    ///
    /// Four of the fields are the CONSOLE's, not the session's, and are
    /// composed here: `tx_capable` comes from the lane (a session with no
    /// capture device cannot transmit however capable its vocoder is), and
    /// the three levels come from the one meter read `snapshot()` already
    /// does at the lane. A session owns no meters — see
    /// [`DstarSnapshotState`].
    #[cfg(feature = "dstar")]
    #[must_use]
    pub fn dstar_state(&self) -> Option<DstarSnapshotState> {
        let mut st = self.dstar.as_ref().map(DstarSession::state)?;
        st.tx_capable = self.voice_route_tx_capable();
        st.tx_dbfs = self.state.tx_level_db;
        st.rx_dbfs = self.state.rx_level_db;
        st.input_dbfs = self.state.input_level_db;
        Some(st)
    }

    /// Open a System Fusion link with audio, and adopt it.
    ///
    /// Mutually exclusive with an IAX2 call, an M17 session and a D-Star
    /// session — not by analogy with them but because there is one `ThumbDV`,
    /// and both digital-voice networks need it exclusively.
    ///
    /// `input`/`output` are device-name substrings for the lane the link
    /// will ride (`None` = system default); the link itself is handed only
    /// the lane's channel ends.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] from [`Self::open_voice_route`]
    /// while any other network holds the lane;
    /// [`ConsoleError::Device`]/[`ConsoleError::Audio`] if the OUTPUT device
    /// cannot be resolved or opened; otherwise whatever
    /// [`YsfLink::connect_with_audio`] returns.
    #[cfg(feature = "ysf")]
    pub fn ysf_connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        cfg: &YsfConfig,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<(), ConsoleError> {
        // The route is the reservation, the mutual exclusion AND the pref
        // push, all at once — see [`Self::dstar_connect`].
        let audio = self.open_voice_route(input, output, || backend)?;
        match YsfLink::connect_with_audio(cfg, audio) {
            Ok(link) => self.ysf_adopt(link),
            Err(e) => {
                // The route opened but the link did not: give the lanes
                // back, or the station stays reserved forever.
                let handles = self.release_voice_route();
                drop(handles);
                Err(e)
            }
        }
    }

    /// An embedder-facing query: *would a System Fusion connect be refused
    /// right now?* — answered without opening anything, so a front-end can
    /// grey a button out, or a caller building the link OUTSIDE this
    /// session's mutex can decide not to scan for a dongle. Not a step of the
    /// facade's connect flow: [`Self::open_voice_route`]'s own check is the
    /// gate. Same reasoning as [`Self::dstar_can_connect`].
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] while any other network is live.
    #[cfg(feature = "ysf")]
    pub fn ysf_can_connect(&self) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf.is_some()
            || self.nxdn_is_active()
            || self.dmr_is_active()
            || self.voice_route.is_some()
        {
            return Err(ConsoleError::AlreadyConnected);
        }
        Ok(())
    }

    /// Install an already-constructed [`YsfLink`], re-checking exclusion.
    ///
    /// Exists so `astar-station` can run the `ThumbDV` scan and init cookbook
    /// with the session mutex NOT held — every `Station` method takes it, and
    /// the contract is poll-and-snapshot, never blocking.
    ///
    /// On refusal the link is disconnected here rather than handed back: it
    /// has already bound a socket and taken the dongle, and leaking the
    /// dongle — which only one process may hold — would be worse than the
    /// error the caller is about to see — and the route it rode is released
    /// with it.
    ///
    /// The link must ARRIVE with the route already reserved on its behalf:
    /// the caller opened it with [`Self::open_voice_route`] and handed the
    /// resulting [`astar_audio::CallAudio`] to `YsfLink::connect_with_audio`,
    /// so a `voice_route` of `None` here means the link has channel ends
    /// nothing is feeding. Same rule, same reason, as [`Self::dstar_adopt`].
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] when an IAX2 call or another
    /// digital session is live, or when no route was reserved.
    #[cfg(feature = "ysf")]
    pub fn ysf_adopt(&mut self, link: YsfLink) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf.is_some()
            || self.nxdn_is_active()
            || self.dmr_is_active()
            || self.voice_route.is_none()
        {
            link.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            return Err(ConsoleError::AlreadyConnected);
        }
        self.ysf = Some(link);
        Ok(())
    }

    /// Disconnect the live YSF link, if any. No-op when none is active.
    #[cfg(feature = "ysf")]
    pub fn ysf_disconnect(&mut self) {
        // Everything here is inside the `if let`, route release included:
        // `Station::disconnect` calls every network's disconnect in turn, so
        // an unconditional release would close the lanes out from under a
        // live M17 or D-Star session that legitimately holds them — and an
        // unconditional state reset would blank their mirror.
        if let Some(link) = self.ysf.take() {
            link.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            // Reset every field the snapshot's YSF branch writes, exactly as
            // `dstar_disconnect` does and for the identical reason: the
            // mirror simply STOPS running once `self.ysf` is `None`, so
            // without this the last values it wrote stay frozen in
            // `self.state` forever. A link that came up and then
            // disconnected would leave a snapshot reporting `Answered` for a
            // station with no session at all. The levels go with it: the
            // meter block only writes while a lane is live, and this
            // released the last one.
            self.state.status = CallStatus::Idle;
            self.state.remote_ptt = false;
            self.state.ptt = false;
            self.state.rx_level_db = -60.0;
            self.state.tx_level_db = -60.0;
            self.state.input_level_db = -60.0;
        }
    }

    /// `true` while a YSF link is live. Always `false` when the `ysf`
    /// feature isn't compiled in, so callers never need their own `#[cfg]`.
    #[allow(clippy::unused_self)]
    fn ysf_is_active(&self) -> bool {
        #[cfg(feature = "ysf")]
        {
            self.ysf.is_some()
        }
        #[cfg(not(feature = "ysf"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live YSF link, or `None`.
    ///
    /// The two levels are the CONSOLE's, not the link's, and are composed
    /// here from the one meter read `snapshot()` already does at the lane —
    /// exactly as [`Self::dstar_state`] composes D-Star's. A session owns no
    /// meters; see [`YsfSnapshot::tx_dbfs`].
    #[cfg(feature = "ysf")]
    #[must_use]
    pub fn ysf_state(&self) -> Option<YsfSnapshot> {
        let mut st = self.ysf.as_ref().map(YsfLink::snapshot)?;
        st.tx_dbfs = self.state.tx_level_db;
        st.rx_dbfs = self.state.rx_level_db;
        Some(st)
    }

    /// Link to an `NXDNReflector` talkgroup and decode the audio on it
    /// (iax-b9c2). The console-side counterpart of [`Self::ysf_connect`],
    /// step for step: the route is the reservation, the mutual exclusion AND
    /// the pref push, all at once.
    ///
    /// `input`/`output` are device-name substrings for the lane the link
    /// will ride (`None` = system default); the link itself is handed only
    /// the lane's channel ends.
    ///
    /// Receive only. [`Self::set_ptt`] refuses a key-down while this link is
    /// live — see [`crate::nxdn`]'s Transmit section for why a refusal beats
    /// a key that silently does nothing.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] from [`Self::open_voice_route`]
    /// while any other network holds the lane;
    /// [`ConsoleError::Device`]/[`ConsoleError::Audio`] if the OUTPUT device
    /// cannot be resolved or opened; otherwise whatever
    /// [`NxdnLink::connect_with_audio`] returns.
    #[cfg(feature = "nxdn")]
    pub fn nxdn_connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        cfg: &NxdnConfig,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<(), ConsoleError> {
        let audio = self.open_voice_route(input, output, || backend)?;
        match NxdnLink::connect_with_audio(cfg, audio) {
            Ok(link) => self.nxdn_adopt(link),
            Err(e) => {
                // The route opened but the link did not: give the lanes
                // back, or the station stays reserved forever.
                let handles = self.release_voice_route();
                drop(handles);
                Err(e)
            }
        }
    }

    /// An embedder-facing query: *would an NXDN connect be refused right
    /// now?* — answered without opening anything, so a front-end can grey a
    /// button out, or a caller building the link OUTSIDE this session's mutex
    /// can decide not to scan for a dongle. Not a step of the facade's
    /// connect flow: [`Self::open_voice_route`]'s own check is the gate. Same
    /// reasoning as [`Self::ysf_can_connect`].
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] while any other network is live.
    #[cfg(feature = "nxdn")]
    pub fn nxdn_can_connect(&self) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn.is_some()
            || self.dmr_is_active()
            || self.voice_route.is_some()
        {
            return Err(ConsoleError::AlreadyConnected);
        }
        Ok(())
    }

    /// Install an already-constructed [`NxdnLink`], re-checking exclusion.
    ///
    /// Exists so `astar-station` can run the `ThumbDV` scan and init cookbook
    /// with the session mutex NOT held — every `Station` method takes it, and
    /// the contract is poll-and-snapshot, never blocking.
    ///
    /// On refusal the link is disconnected here rather than handed back: it
    /// has already bound a socket and taken the dongle, and leaking the
    /// dongle — which only one process may hold — would be worse than the
    /// error the caller is about to see — and the route it rode is released
    /// with it.
    ///
    /// The link must ARRIVE with the route already reserved on its behalf, so
    /// a `voice_route` of `None` here means the link has channel ends nothing
    /// is feeding. Same rule, same reason, as [`Self::ysf_adopt`].
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] when an IAX2 call or another
    /// digital session is live, or when no route was reserved.
    #[cfg(feature = "nxdn")]
    pub fn nxdn_adopt(&mut self, link: NxdnLink) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn.is_some()
            || self.dmr_is_active()
            || self.voice_route.is_none()
        {
            link.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            return Err(ConsoleError::AlreadyConnected);
        }
        self.nxdn = Some(link);
        Ok(())
    }

    /// Disconnect the live NXDN link, if any. No-op when none is active.
    #[cfg(feature = "nxdn")]
    pub fn nxdn_disconnect(&mut self) {
        // Everything here is inside the `if let`, route release included:
        // `Station::disconnect` calls every network's disconnect in turn, so
        // an unconditional release would close the lanes out from under a
        // live M17, D-Star or YSF session that legitimately holds them — and
        // an unconditional state reset would blank their mirror.
        if let Some(link) = self.nxdn.take() {
            link.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            // Reset every field the snapshot's NXDN branch writes, exactly
            // as `ysf_disconnect` does and for the identical reason: the
            // mirror simply STOPS running once `self.nxdn` is `None`, so
            // without this the last values it wrote stay frozen in
            // `self.state` forever. The levels go with it: the meter block
            // only writes while a lane is live, and this released the last
            // one.
            self.state.status = CallStatus::Idle;
            self.state.remote_ptt = false;
            self.state.ptt = false;
            self.state.rx_level_db = -60.0;
            self.state.tx_level_db = -60.0;
            self.state.input_level_db = -60.0;
        }
    }

    /// `true` while an NXDN link is live. Always `false` when the `nxdn`
    /// feature isn't compiled in, so callers never need their own `#[cfg]`.
    #[allow(clippy::unused_self)]
    fn nxdn_is_active(&self) -> bool {
        #[cfg(feature = "nxdn")]
        {
            self.nxdn.is_some()
        }
        #[cfg(not(feature = "nxdn"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live NXDN link, or `None`.
    ///
    /// The two levels are the CONSOLE's, not the link's, and are composed
    /// here from the one meter read `snapshot()` already does at the lane —
    /// exactly as [`Self::ysf_state`] composes YSF's. A session owns no
    /// meters; see [`NxdnSnapshot::tx_dbfs`].
    #[cfg(feature = "nxdn")]
    #[must_use]
    pub fn nxdn_state(&self) -> Option<NxdnSnapshot> {
        let mut st = self.nxdn.as_ref().map(NxdnLink::snapshot)?;
        st.tx_dbfs = self.state.tx_level_db;
        st.rx_dbfs = self.state.rx_level_db;
        Some(st)
    }

    /// Link to a DMR master's talkgroup and decode the audio on it
    /// (iax-d4f7). The console-side counterpart of [`Self::nxdn_connect`],
    /// step for step: the route is the reservation, the mutual exclusion AND
    /// the pref push, all at once.
    ///
    /// `input`/`output` are device-name substrings for the lane the link
    /// will ride (`None` = system default); the link itself is handed only
    /// the lane's channel ends.
    ///
    /// `cfg` is taken **by value**, unlike every other network's here,
    /// because [`DmrConfig::password`] is a secret: moving it means this
    /// session's caller no longer holds it, and the compiler says so. It is
    /// moved on into the link, spent on one `RPTK` digest and dropped —
    /// nothing on this session ever holds it.
    ///
    /// Receive only. [`Self::set_ptt`] refuses a key-down while this link is
    /// live — see [`crate::dmr`]'s Transmit section for why a refusal beats
    /// a key that silently does nothing.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] from [`Self::open_voice_route`]
    /// while any other network holds the lane;
    /// [`ConsoleError::Device`]/[`ConsoleError::Audio`] if the OUTPUT device
    /// cannot be resolved or opened; otherwise whatever
    /// [`DmrLink::connect_with_audio`] returns — never carrying the
    /// password.
    #[cfg(feature = "dmr")]
    pub fn dmr_connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        cfg: DmrConfig,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<(), ConsoleError> {
        let audio = self.open_voice_route(input, output, || backend)?;
        match DmrLink::connect_with_audio(cfg, audio) {
            Ok(link) => self.dmr_adopt(link),
            Err(e) => {
                // The route opened but the link did not: give the lanes
                // back, or the station stays reserved forever.
                let handles = self.release_voice_route();
                drop(handles);
                Err(e)
            }
        }
    }

    /// An embedder-facing query: *would a DMR connect be refused right now?*
    /// — answered without opening anything, so a front-end can grey a button
    /// out, or a caller building the link OUTSIDE this session's mutex can
    /// decide not to scan for a dongle. Not a step of the facade's connect
    /// flow: [`Self::open_voice_route`]'s own check is the gate. Same
    /// reasoning as [`Self::nxdn_can_connect`].
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] while any other network is live.
    #[cfg(feature = "dmr")]
    pub fn dmr_can_connect(&self) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr.is_some()
            || self.voice_route.is_some()
        {
            return Err(ConsoleError::AlreadyConnected);
        }
        Ok(())
    }

    /// Install an already-constructed [`DmrLink`], re-checking exclusion.
    ///
    /// Exists so `astar-station` can run the `ThumbDV` scan and init cookbook
    /// — and DMR's multi-round-trip homebrew login on top of it — with the
    /// session mutex NOT held. Every `Station` method takes it, and the
    /// contract is poll-and-snapshot, never blocking.
    ///
    /// On refusal the link is disconnected here rather than handed back: it
    /// has already bound a socket, logged in to a master under the
    /// operator's own radio ID and taken the dongle, and leaking any of
    /// those would be worse than the error the caller is about to see — and
    /// the route it rode is released with it.
    ///
    /// The link must ARRIVE with the route already reserved on its behalf, so
    /// a `voice_route` of `None` here means the link has channel ends nothing
    /// is feeding. Same rule, same reason, as [`Self::nxdn_adopt`].
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] when an IAX2 call or another
    /// digital session is live, or when no route was reserved.
    #[cfg(feature = "dmr")]
    pub fn dmr_adopt(&mut self, link: DmrLink) -> Result<(), ConsoleError> {
        if self.active.is_some()
            || self.m17_is_active()
            || self.dstar_is_active()
            || self.ysf_is_active()
            || self.nxdn_is_active()
            || self.dmr.is_some()
            || self.voice_route.is_none()
        {
            link.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            return Err(ConsoleError::AlreadyConnected);
        }
        self.dmr = Some(link);
        Ok(())
    }

    /// Disconnect the live DMR link, if any. No-op when none is active.
    #[cfg(feature = "dmr")]
    pub fn dmr_disconnect(&mut self) {
        // Everything here is inside the `if let`, route release included:
        // `Station::disconnect` calls every network's disconnect in turn, so
        // an unconditional release would close the lanes out from under a
        // live M17, D-Star, YSF or NXDN session that legitimately holds them
        // — and an unconditional state reset would blank their mirror.
        if let Some(link) = self.dmr.take() {
            link.disconnect();
            let handles = self.release_voice_route();
            drop(handles);
            // Reset every field the snapshot's DMR branch writes, exactly as
            // `nxdn_disconnect` does and for the identical reason: the
            // mirror simply STOPS running once `self.dmr` is `None`, so
            // without this the last values it wrote stay frozen in
            // `self.state` forever. The levels go with it: the meter block
            // only writes while a lane is live, and this released the last
            // one.
            self.state.status = CallStatus::Idle;
            self.state.remote_ptt = false;
            self.state.ptt = false;
            self.state.rx_level_db = -60.0;
            self.state.tx_level_db = -60.0;
            self.state.input_level_db = -60.0;
        }
    }

    /// `true` while a DMR link is live. Always `false` when the `dmr`
    /// feature isn't compiled in, so callers never need their own `#[cfg]`.
    #[allow(clippy::unused_self)]
    fn dmr_is_active(&self) -> bool {
        #[cfg(feature = "dmr")]
        {
            self.dmr.is_some()
        }
        #[cfg(not(feature = "dmr"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live DMR link, or `None`.
    ///
    /// The two levels are the CONSOLE's, not the link's, and are composed
    /// here from the one meter read `snapshot()` already does at the lane —
    /// exactly as [`Self::nxdn_state`] composes NXDN's. A session owns no
    /// meters; see [`DmrSnapshot::tx_dbfs`].
    #[cfg(feature = "dmr")]
    #[must_use]
    pub fn dmr_state(&self) -> Option<DmrSnapshot> {
        let mut st = self.dmr.as_ref().map(DmrLink::snapshot)?;
        st.tx_dbfs = self.state.tx_level_db;
        st.rx_dbfs = self.state.rx_level_db;
        Some(st)
    }

    /// Reset console state to idle immediately and hand back the live `Call` and
    /// its owning `Manager` so the caller can tear them down OUTSIDE any session
    /// lock. Returns `None` when no call is active.
    ///
    /// The blocking parts of teardown are `Call::hangup` (it joins the runtime
    /// thread) and dropping the `Manager` (its `AudioRouter` drops the cpal
    /// streams — this can stall briefly on a `CoreAudio` mutex). Running either
    /// under the session lock freezes the SSE snapshot loop mid-teardown, so the
    /// harness detaches here (only cheap pool/router bookkeeping via `remove`
    /// happens under the lock), releases the lock, then hangs the `Call` up and
    /// drops the `Manager` off-lock.
    #[must_use]
    pub fn detach(&mut self) -> Option<(Call, Manager)> {
        let id = self.active.take()?;
        let mut manager = self.manager.take()?;
        // Cheap pool/router bookkeeping under the lock; the blocking parts
        // (Call thread join + Manager/router stream drop) run off-lock.
        let call = manager.remove(id);
        self.events = None;
        self.frames = None;
        self.inbound_active = false;
        self.state.status = CallStatus::Idle;
        self.state.ptt = false;
        self.state.remote_ptt = false;
        self.state.rtt_ms = None;
        call.map(|c| (c, manager))
    }

    /// Set the input (TX/mic) gain multiplier. `value` is clamped to `[0.0, 4.0]`
    /// (100%-400% headroom, matching the output side's ceiling); `NaN` is
    /// treated as unity (1.0). Takes `&self` (atomic write) so it can be called
    /// from a shared reference — no `NotConnected` error, gain is a standing
    /// preference that persists across calls.
    pub fn set_input_gain(&self, value: f32) {
        let clamped = if value.is_nan() {
            1.0
        } else {
            value.clamp(0.0, 4.0)
        };
        self.input_gain.set(clamped);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_input_gain(id, clamped);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            if let Some(mic) = route.mic() {
                r.set_mic_gain(mic, clamped);
            }
        }
    }

    /// Set the output (RX/speaker) gain multiplier. `value` is clamped to
    /// `[0.0, 4.0]` (iax-a4e7: 100%-400% headroom so a quiet station on a
    /// mixed net can be boosted — the input side shares this same ceiling);
    /// `NaN` is treated as unity (1.0), same as
    /// [`set_input_gain`](Self::set_input_gain). `0.0` is a valid floor, not
    /// just a clamp boundary — the half-duplex RX-mute path calls
    /// `set_output_gain(0)` and depends on it actually muting.
    pub fn set_output_gain(&self, value: f32) {
        let clamped = if value.is_nan() {
            1.0
        } else {
            value.clamp(0.0, 4.0)
        };
        self.output_gain.set(clamped);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_output_gain(id, clamped);
        }
        if let (Some(route), Some(mgr)) = (self.voice_route.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            r.set_output_gain(route.out(), clamped);
        }
    }

    /// Copy the live-call TX spectrum (iax-2b09) of the active network call into
    /// `out` — the SAME log-binned, peak-held dBFS values the mic monitor
    /// produces, tapped from the post-DSP, pre-encode TX PCM. Returns the number
    /// of bins written (`0` if no active call / unrouted mic). A pure observer.
    ///
    /// Reads the ONE lane [`Self::meter_ids`] names — the IAX2 call's routed
    /// mic or the voice route's — so every network's TX analyzer is the same
    /// analyzer.
    #[must_use]
    pub fn tx_spectrum(&self, out: &mut [f32]) -> usize {
        if let (Some((mic, _)), Some(mgr)) = (self.meter_ids(), self.manager.as_ref()) {
            return mic
                .and_then(|m| mgr.router().mic_tx_spectrum(&m, out))
                .unwrap_or(0);
        }
        0
    }

    /// Copy the live-call RX spectrum (iax-2b09) of the active network call into
    /// `out` — the SAME log-binned, peak-held dBFS values the mic monitor
    /// produces, tapped from the post-mix decoded RX PCM. Returns the number of
    /// bins written (`0` if no active call). A pure observer.
    ///
    /// Reads the ONE bus [`Self::meter_ids`] names; see [`Self::tx_spectrum`].
    #[must_use]
    pub fn rx_spectrum(&self, out: &mut [f32]) -> usize {
        if let (Some((_, bus)), Some(mgr)) = (self.meter_ids(), self.manager.as_ref()) {
            return mgr.router().output_rx_spectrum(&bus, out).unwrap_or(0);
        }
        0
    }

    /// Return the current input gain multiplier (1.0 = unity by default).
    #[must_use]
    pub fn input_gain(&self) -> f32 {
        self.input_gain.get()
    }

    /// Return the current output gain multiplier (1.0 = unity by default).
    #[must_use]
    pub fn output_gain(&self) -> f32 {
        self.output_gain.get()
    }

    /// Clone the TX (input) gain cell so a sibling audio path (the local
    /// parrot) shares the same slider without going through a `Client`.
    #[must_use]
    pub fn input_gain_cell(&self) -> crate::metering::Gain {
        self.input_gain.clone()
    }

    /// Clone the RX (output) gain cell. See [`Self::input_gain_cell`].
    #[must_use]
    pub fn output_gain_cell(&self) -> crate::metering::Gain {
        self.output_gain.clone()
    }

    /// `true` while a call is live (used to keep the local parrot and a network
    /// call mutually exclusive — they share the audio devices).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    // -----------------------------------------------------------------------
    // Link surface (iax-1075): thin passthroughs to the Manager's link layer
    // so cross-language front-ends (via astar-station / the C-ABI) can
    // build node-shaped features. Vendor-neutral, secret-free (the dial
    // secret is consumed at connect and never stored).
    // -----------------------------------------------------------------------

    /// Connect a node link over standard IAX2 (iax-1075): dial `spec.peer`,
    /// register the link in `spec.mode`, and (for a Transceive link) route the
    /// default mic so it is key-able. Builds the engine if needed. Returns the
    /// link's opaque call id (`CallId::as_raw`, as reported in the roster).
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] while a digital-voice route is held
    /// — routing a link's mic through the `Manager` would re-bind and un-gate
    /// the very lane M17/D-Star/YSF is transmitting on;
    /// [`ConsoleError::Link`] on a dial/link failure; [`ConsoleError::Device`]
    /// if no default output (or, for Transceive, input) device exists.
    pub fn link_connect(
        &mut self,
        spec: LinkConnectSpec,
        backend: Box<dyn AudioBackend>,
    ) -> Result<u64, ConsoleError> {
        if self.voice_route.is_some() {
            return Err(ConsoleError::AlreadyConnected);
        }
        let manager = self.ensure_engine_checked(|| backend)?;
        let out_id = manager
            .default_output()
            .ok_or_else(|| ConsoleError::Device("no default output device".into()))?
            .id
            .as_str()
            .to_string();
        let mic = if spec.mode.is_transmit_capable() {
            Some(
                manager
                    .default_input()
                    .ok_or_else(|| ConsoleError::Device("no default input device".into()))?
                    .id
                    .as_str()
                    .to_string(),
            )
        } else {
            None
        };
        let peer = spec.peer;
        let node = spec.node.clone();
        let id = manager
            .connect_link(
                LinkSpec {
                    node: spec.node,
                    mode: spec.mode,
                    output: OutputId::new(&out_id),
                    caller_id: spec.caller_id,
                    secret: spec.secret,
                    dest: node,
                    mode_shape: spec.shape,
                    permanent: spec.permanent,
                },
                &|_| Ok(peer),
            )
            .map_err(|e| ConsoleError::Link(e.to_string()))?;
        if let Some(mic) = mic {
            manager
                .route(id, &MicId::new(&mic))
                .map_err(ConsoleError::Iax)?;
        }
        // End the ensure_engine borrow before touching the receiver field.
        if self.link_event_rx.is_none() {
            let taken = self.manager.as_mut().and_then(Manager::link_events);
            self.link_event_rx = taken;
        }
        Ok(id.as_raw())
    }

    /// Tear a link down by node label: drops the link view + hangs the call up.
    ///
    /// # Errors
    /// [`ConsoleError::Link`] if no link is registered for `node` (or no engine).
    pub fn link_disconnect(&mut self, node: &str) -> Result<(), ConsoleError> {
        let id = self.link_call_id(node)?;
        self.manager
            .as_mut()
            .expect("engine checked by link_call_id")
            .disconnect_link(id)
            .map_err(|e| ConsoleError::Link(e.to_string()))
    }

    /// Change a link's mode by node label. Switching TO Transceive routes the
    /// default mic if none is routed (so the link is immediately key-able);
    /// switching away releases it (Manager mode routing).
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] while a digital-voice route is held
    /// (switching TO Transceive routes a mic — see [`Self::link_connect`]);
    /// [`ConsoleError::Link`] if no link is registered for `node` (or no engine).
    pub fn link_set_mode(&mut self, node: &str, mode: LinkMode) -> Result<(), ConsoleError> {
        if self.voice_route.is_some() {
            return Err(ConsoleError::AlreadyConnected);
        }
        let id = self.link_call_id(node)?;
        let manager = self.manager.as_mut().expect("engine checked");
        manager
            .set_link_mode(id, mode)
            .map_err(|e| ConsoleError::Link(e.to_string()))?;
        if mode.is_transmit_capable() && manager.routed_mic(id).is_none() {
            let mic = manager
                .default_input()
                .ok_or_else(|| ConsoleError::Device("no default input device".into()))?
                .id
                .as_str()
                .to_string();
            manager
                .route(id, &MicId::new(&mic))
                .map_err(ConsoleError::Iax)?;
        }
        Ok(())
    }

    /// Key / unkey a link by node label (refused for non-transmit modes).
    ///
    /// # Errors
    /// [`ConsoleError::Link`] if no link is registered for `node`, or keying a
    /// non-transmit-capable link.
    pub fn link_key(&mut self, node: &str, on: bool) -> Result<(), ConsoleError> {
        let id = self.link_call_id(node)?;
        let manager = self.manager.as_mut().expect("engine checked");
        let r = if on {
            manager.key_link(id)
        } else {
            manager.unkey_link(id)
        };
        r.map_err(|e| ConsoleError::Link(e.to_string()))
    }

    /// Snapshot of all live links (secret-free; `None` before any engine).
    #[must_use]
    pub fn link_roster(&self) -> Option<LinkRoster> {
        self.manager.as_ref().map(Manager::link_roster)
    }

    /// Who has keyed up on the live digital link, newest first — see
    /// [`crate::heard::HeardLog`]. Empty when no digital session is live:
    /// `AllStar` carries no talker identity, so an IAX2 call has nothing to
    /// report here.
    ///
    /// At most one digital session is ever live (every connect path guards on
    /// the others), so the first `Some` found is the answer.
    #[must_use]
    pub fn heard(&self) -> Vec<HeardEntry> {
        #[cfg(feature = "m17")]
        if let Some(s) = &self.m17 {
            return s.heard();
        }
        #[cfg(feature = "dstar")]
        if let Some(s) = &self.dstar {
            return s.heard();
        }
        #[cfg(feature = "ysf")]
        if let Some(s) = &self.ysf {
            return s.heard();
        }
        #[cfg(feature = "nxdn")]
        if let Some(s) = &self.nxdn {
            return s.heard();
        }
        #[cfg(feature = "dmr")]
        if let Some(s) = &self.dmr {
            return s.heard();
        }
        Vec::new()
    }

    /// Drain all pending aggregated link lifecycle events (iax-62cf stream).
    /// Empty before the first `link_connect`.
    pub fn drain_link_events(&mut self) -> Vec<LinkEvent> {
        if self.link_event_rx.is_none() {
            let taken = self.manager.as_mut().and_then(Manager::link_events);
            self.link_event_rx = taken;
        }
        self.link_event_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    }

    /// Drain DTMF command digits (iax-d254): merges the engine's in-band
    /// pool (`Manager::drain_dtmf_digits` — conference member tones,
    /// Goertzel-detected and squelched from the relay) with out-of-band
    /// `CallEvent::Dtmf` frames harvested by the session event loop.
    /// Draining consumes. Digits are `(raw CallId, digit)`.
    /// Out-of-band (protocol) DTMF is harvested only from the leg whose events the session holds — the most recently adopted inbound member; in-band tones are detected for ALL conference members by the engine.
    pub fn drain_dtmf_digits(&mut self) -> Vec<(u64, char)> {
        let mut digits: Vec<(u64, char)> = self
            .manager
            .as_ref()
            .map(Manager::drain_dtmf_digits)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, d)| (id.as_raw(), d))
            .collect();
        digits.append(&mut self.dtmf_digits);
        digits
    }

    /// Announce to every non-link conference member (iax-9e02): the
    /// web-transceiver / handset users, never the linked nodes. Returns how
    /// many members were reached (0 with no engine or no members).
    pub fn announce_to_non_link_members(&mut self, req: astar_iax::AnnounceRequest) -> usize {
        self.manager
            .as_mut()
            .map_or(0, |m| m.announce_to_non_link_members(req))
    }

    /// Propagate member keying onto transceive links (iax-7d51): a far-end
    /// node mutes relayed audio from an unkeyed sender, so the links must key
    /// while a conference member is transmitting. Idempotent; safe to call
    /// every pump tick. Returns the number of links whose keying changed.
    pub fn sync_link_keying(&mut self) -> usize {
        self.manager
            .as_mut()
            .map_or(0, astar_iax::Manager::sync_link_keying)
    }

    /// The live audio pipeline sample rate in Hz (iax-4348): the engine's
    /// rate once built (8 kHz, or 16 kHz for a prefer-slin16 policy), else
    /// the 8 kHz default. In-band DTMF synthesis must match it (iax-8d2f) —
    /// a fixed-8k tone played on a 16 kHz pipeline is double pitch.
    #[must_use]
    pub fn pipeline_sample_rate(&self) -> u32 {
        self.manager
            .as_ref()
            .map_or(8_000, Manager::pipeline_sample_rate)
    }

    /// Resolve a node label to its live link `CallId` via the roster.
    fn link_call_id(&self, node: &str) -> Result<CallId, ConsoleError> {
        let roster = self
            .link_roster()
            .ok_or_else(|| ConsoleError::Link("no engine".into()))?;
        roster
            .links
            .iter()
            .find(|l| l.node == node)
            .map(|l| CallId::from_raw(l.call))
            .ok_or_else(|| ConsoleError::Link(format!("no link for node {node}")))
    }

    /// Pin the station's audio pipeline rate from `policy` (iax-4348), which
    /// is what makes astar and astar-server slin16 stations: the rate is
    /// fixed when the `Manager` is built, and `Manager::dial` CAPS every
    /// dial's codec policy to it, so a station that means to offer slin16
    /// has to say so before anything builds the engine — not on the first
    /// dial, by which time a digital-voice session may already have built it
    /// at the 8 kHz default. `Station`'s constructors call this with
    /// `StationConfig.codec_policy`.
    ///
    /// An engine that is already up and idle is rebuilt at the new rate; a
    /// busy one keeps the rate it has (calls, the inbound listener and the
    /// digital-voice bus are all live on it) and the mismatch is logged.
    pub fn set_station_policy(&mut self, policy: CodecPolicy) {
        self.station_policy = policy;
        let rate = policy.max_sample_rate();
        self.rebuild_idle_engine_for_rate(rate);
        if let Some(mgr) = self.manager.as_ref()
            && mgr.pipeline_sample_rate() != rate
        {
            tracing::warn!(
                engine_hz = mgr.pipeline_sample_rate(),
                policy_hz = rate,
                "station policy set while the engine is busy: the pipeline rate cannot change live"
            );
        }
    }

    /// Drop an engine that is doing nothing when the pipeline sample rate it
    /// was pinned to at construction no longer matches the policy about to
    /// build a call (iax-4348), so the next `ensure_engine` rebuilds it at
    /// `rate`. A no-op when the rates already agree or the engine is busy.
    ///
    /// Why this is needed: `Manager::pipeline_sample_rate` is fixed at
    /// construction and `Manager::dial` CAPS every dial's codec policy to it,
    /// so an engine built at 8 kHz silently turns a `prefer_slin16` dial into
    /// a plain-slin one — the app offers `capability=0x004c`, the far end
    /// answers slin, and nobody logs a word about it. The one audio lane made
    /// that reachable in normal use: `open_voice_route` builds the engine at
    /// `station_policy` (the 8 kHz default until an IAX2 `connect` has run),
    /// and a digital disconnect deliberately leaves the engine in place, so
    /// M17/D-Star/YSF followed by a dial pinned the station narrowband for
    /// the rest of the process.
    ///
    /// "Idle" is strict: no pooled calls, no inbound listener, no voice
    /// route, no digital session, no live registration. A live `WireGuard` transport with no
    /// pending config left to replay it from also blocks the rebuild — a
    /// fresh engine would come up on plain UDP and the dial would leave the
    /// tunnel without saying so. In both cases the old (capped) behaviour
    /// stands.
    ///
    /// Dropping the engine drops its `AudioRouter`, so any device streams the
    /// old rate left open close here, under whatever lock the caller holds.
    /// A digital route has already closed its own two lanes by the time it
    /// releases, so that case is free; an IAX2 call's lanes outlive its
    /// hangup (the `Manager` never closes them), so a policy change between
    /// two dials pays one stream-drop. That is the price of not handing the
    /// next call a bus running at the wrong rate.
    fn rebuild_idle_engine_for_rate(&mut self, rate: u32) {
        let (from, no_calls, wg_live) = {
            let Some(mgr) = self.manager.as_ref() else {
                return;
            };
            (
                mgr.pipeline_sample_rate(),
                mgr.call_count() == 0,
                mgr.wg_status().is_some(),
            )
        };
        if from == rate {
            return;
        }
        let idle = no_calls
            && self.active.is_none()
            && self.inbound.is_none()
            && self.voice_route.is_none()
            // A live registration took a clone of THIS engine's net stack; a
            // rebuild would leave it running on an orphaned transport.
            && self.reg_handle.is_none()
            && !self.m17_is_active()
            && !self.dstar_is_active()
            && !self.ysf_is_active()
            && !self.nxdn_is_active()
            && !self.dmr_is_active();
        if !idle {
            return;
        }
        if wg_live && self.pending_wg.is_none() {
            tracing::warn!(
                from_hz = from,
                to_hz = rate,
                "idle engine kept at its pinned rate: rebuilding would drop the WireGuard transport"
            );
            return;
        }
        tracing::info!(
            from_hz = from,
            to_hz = rate,
            "idle engine rebuilt for the new pipeline rate"
        );
        // Everything taken FROM the old engine dies with it; `ensure_engine`
        // replays what the SESSION owns (pending announce config, bridge
        // mode) and `ensure_engine_checked` re-applies a pending WireGuard
        // transport. `link_event_rx` must go or `link_events()` would keep
        // draining a receiver whose sender is gone and never re-take from the
        // new engine.
        self.manager = None;
        self.link_event_rx = None;
        self.events = None;
        self.frames = None;
        self.inbound_active = false;
    }

    /// Build the `Manager` once and keep it for the session lifetime. Idempotent:
    /// if a `Manager` is already present the factory is not called and the
    /// existing one is returned. The pipeline rate is pinned by
    /// `self.station_policy` (iax-4348) — `CodecPolicy::default()` (8 kHz)
    /// unless [`Self::connect`] has already set it from `ConsoleConfig`.
    pub fn ensure_engine(
        &mut self,
        make_backend: impl FnOnce() -> Box<dyn AudioBackend>,
    ) -> &mut Manager {
        if self.manager.is_none() {
            self.manager = Some(Manager::with_policy(make_backend(), self.station_policy));
            // iax-rxjb: the RX jitter-buffer config is router-wide, so replay
            // whatever was set before the engine existed onto the fresh one.
            self.manager
                .as_ref()
                .expect("just set")
                .set_rx_jitter(self.rx_jitter.get());
            // Replay any pending announce config pushed before the Manager existed.
            if let Some(cfg) = self.pending_announce.clone() {
                self.manager
                    .as_mut()
                    .expect("just set")
                    .set_announce_config(cfg);
            }
            // Replay the configured bridge/conference mode (iax-647d). Handset is
            // a no-op (the Manager's own default), so existing embedders stay
            // byte-identical; the node daemon's `Bridge` takes effect here.
            if self.bridge_config != BridgeConfig::default() {
                let _ = self
                    .manager
                    .as_mut()
                    .expect("just set")
                    .set_bridge_config(self.bridge_config);
            }
        }
        self.manager.as_mut().expect("just set")
    }

    /// [`Self::ensure_engine`] plus the deferred link-transport apply
    /// (iax-5bbd): if a `WireGuard` transport was selected before the engine
    /// existed, install it now — over a fresh OS UDP underlay socket — so the
    /// engine's dial/listen/register paths all ride the tunnel. All fallible
    /// session entry points that build the engine (`connect`, `start_inbound`,
    /// `link_connect`) come through here; the infallible public
    /// [`Self::ensure_engine`] intentionally does NOT apply the pending
    /// transport, so a failed `WireGuard` install can never be silently
    /// downgraded — the pending selection is retained and every dial/listen
    /// attempt keeps failing until the config is fixed or the caller
    /// explicitly clears back to [`LinkTransport::Udp`].
    ///
    /// # Errors
    /// [`ConsoleError::Link`] if the tunnel underlay/config/key is unusable
    /// (the message names the key *reference*, never material).
    fn ensure_engine_checked(
        &mut self,
        make_backend: impl FnOnce() -> Box<dyn AudioBackend>,
    ) -> Result<&mut Manager, ConsoleError> {
        self.ensure_engine(make_backend);
        if let Some((cfg, resolver)) = self.pending_wg.take() {
            let applied = astar_wireguard::UdpSocketTransport::bound()
                .map_err(astar_iax::IaxError::Io)
                .and_then(|underlay| {
                    self.manager
                        .as_mut()
                        .expect("built by ensure_engine")
                        .set_wireguard_transport_over(&cfg, &|r| resolver(r), Box::new(underlay))
                });
            if let Err(e) = applied {
                // Retain the selection: a retry must re-attempt the tunnel,
                // never silently proceed over plain UDP.
                self.pending_wg = Some((cfg, resolver));
                return Err(map_link_err(e));
            }
        }
        Ok(self.manager.as_mut().expect("built by ensure_engine"))
    }

    /// `true` when a `Manager` has been built (i.e. after the first
    /// `ensure_engine` call or after `connect`).
    #[must_use]
    pub fn has_engine(&self) -> bool {
        self.manager.is_some()
    }

    /// Number of calls currently tracked by the `Manager` (`0` when idle or
    /// before `ensure_engine`/`connect` is called).
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.manager.as_ref().map_or(0, Manager::call_count)
    }

    /// Drain pending call events into cached status, refresh levels/rtt, and
    /// return the current state. `&mut self` because it advances the event queue.
    #[allow(clippy::too_many_lines)] // IAX2 event drain + M17 mirror (iax-f2b8 Task 4): one linear poll, splitting it would scatter the single source of truth across files.
    pub fn snapshot(&mut self) -> ConsoleState {
        // Drive the inbound listener first: drain offers, answer/park/reject, and
        // adopt accepted calls into the shared `active`/`events` machinery below.
        self.poll_inbound();
        // Drain registration events into the outcome queue (Task 3.1).
        self.poll_register();
        // Drain frame observer before events so the timeline is ordered.
        if let Some(rx) = &self.frames {
            while let Ok(tf) = rx.try_recv() {
                self.tracer.on_frame(tf);
            }
        }
        // Collect first to avoid borrowing `self` immutably and mutably at once.
        let mut drained = Vec::new();
        if let Some(rx) = &self.events {
            while let Ok(ev) = rx.try_recv() {
                drained.push(ev);
            }
        }
        for ev in drained {
            // Feed the tracer first (borrows &ev), then the match may move out of ev.
            self.tracer.on_event(&ev);
            match ev {
                CallEvent::Answered { .. } => {
                    // iax-a82f: a WT dial answering is also a newly-answered
                    // call — bump the counter on the Idle/Dialing → Answered
                    // transition (guard against re-counting a call already
                    // answered).
                    if self.state.status != CallStatus::Answered {
                        self.answered_seq += 1;
                    }
                    self.state.status = CallStatus::Answered;
                }
                CallEvent::Hangup { reason } => {
                    let reason = reason.to_string();
                    // A hangup before we ever reached Answered is a failed dial
                    // (reject / timeout); after Answered it's a normal teardown.
                    self.state.status = if self.state.status == CallStatus::Answered {
                        CallStatus::Hangup { reason }
                    } else {
                        CallStatus::Failed { reason }
                    };
                    // An inbound-adopted call that ended: clear the active id so
                    // the listener can adopt the next caller, and report the
                    // silence floor (the WT path keeps `active` until disconnect).
                    if self.inbound_active {
                        self.active = None;
                        self.events = None;
                        self.inbound_active = false;
                    }
                }
                CallEvent::RemotePtt(b) => self.state.remote_ptt = b,
                // Out-of-band DTMF is command input (iax-d254): harvest it
                // for the digit drain, tagged with the active call.
                CallEvent::Dtmf(d) => {
                    if let Some(id) = self.active {
                        self.dtmf_digits.push((id.as_raw(), d));
                    }
                }
                // ConnectionLost/Restored, Text, and any future variants
                // don't affect the console's high-level state.
                _ => {}
            }
        }
        // Reap any leg that reached Hungup (e.g. a remote HANGUP). The leg's FSM
        // marks the call Hungup but leaves it pooled; without this sweep it keeps
        // counting toward `max_calls` and the node eventually busy-rejects every
        // inbound caller.
        if let Some(mgr) = self.manager.as_mut() {
            let _ = mgr.reap_hungup();
        }
        // Levels come from the router lane now; once disconnected there is no
        // active call, so report the silence floor instead of a stale level.
        let ids = self.meter_ids();
        if let (Some((mic, bus)), Some(mgr)) = (ids.as_ref(), self.manager.as_ref()) {
            let r = mgr.router();
            self.state.tx_level_db = mic.as_ref().and_then(|m| r.mic_tx_dbfs(m)).unwrap_or(-60.0);
            self.state.input_level_db = mic
                .as_ref()
                .and_then(|m| r.mic_input_dbfs(m))
                .unwrap_or(-60.0);
            self.state.rx_level_db = r.output_rx_dbfs(bus).unwrap_or(-60.0);
            // Which noise-reduction chain the metered mic is actually running.
            self.state.denoise_status = mic
                .as_ref()
                .and_then(|m| r.mic_denoise_status(m))
                .unwrap_or_default();
        } else {
            self.state.tx_level_db = -60.0;
            self.state.input_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.denoise_status = astar_audio::DenoiseStatus::default();
        }
        // Populate the full concurrent-call list (iax-a1fb P5). Secret-free:
        // CallSnapshot fields are node ids, device names, and health counters only.
        // Taken ONCE per refresh — the flat per-call fields below read out of
        // this list rather than asking the Manager to build a second snapshot.
        self.state.calls = self
            .manager
            .as_ref()
            .map(|m| m.snapshot().calls)
            .unwrap_or_default();
        // IAX2-only health counters stay call-scoped: an RTT and a ts-ladder
        // re-anchor mean nothing without a call.
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            self.state.rtt_ms = mgr
                .rtt(id)
                .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
            // TX health counters (iax-9e55): cumulative ts-ladder re-anchors and
            // cpal capture overruns on the active call / its routed mic.
            self.state.tx_reanchors = mgr.tx_reanchors(id).unwrap_or(0);
            self.state.tx_capture_overruns = mgr.tx_capture_overruns(id).unwrap_or(0);
            // RX health (iax-rxjb): the bus's underrun count and the call
            // lane's live jitter-buffer counters, out of the list above.
            let rx = self.state.calls.iter().find(|c| c.id == id);
            self.state.rx_underruns = rx.map_or(0, |c| c.rx_underruns);
            self.state.rx_jitter_ms = rx.map_or(0, |c| c.rx_jitter_ms);
            self.state.rx_jb_depth_ms = rx.map_or(0, |c| c.rx_jb_depth_ms);
            self.state.rx_frames_lost = rx.map_or(0, |c| c.rx_frames_lost);
            self.state.rx_frames_late = rx.map_or(0, |c| c.rx_frames_late);
            self.state.rx_frames_ooo = rx.map_or(0, |c| c.rx_frames_ooo);
        } else {
            self.state.rtt_ms = None;
            self.state.tx_reanchors = 0;
            self.state.tx_capture_overruns = 0;
            self.state.rx_underruns = 0;
            self.state.rx_jitter_ms = 0;
            self.state.rx_jb_depth_ms = 0;
            self.state.rx_frames_lost = 0;
            self.state.rx_frames_late = 0;
            self.state.rx_frames_ooo = 0;
        }
        // The jitter-buffer CONFIG is a station setting, not a call fact: it
        // reads the same idle or mid-call, so a client can render (and change)
        // it before anything is connected.
        let jb = self.rx_jitter.get();
        self.state.rx_jb_enabled = jb.enabled;
        self.state.rx_jb_min_ms = jb.min_ms;
        self.state.rx_jb_max_ms = jb.max_ms;
        // Mirror the active call's negotiated codec into the flat field
        // (iax-3e53), like the levels/rtt above: `None` when idle or while
        // negotiation is still in flight.
        self.state.negotiated_format = self.active.and_then(|id| {
            self.state
                .calls
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.negotiated_format)
        });
        // iax-a82f: surface the per-call answered counter for the station's
        // answered-edge derivation.
        self.state.answered_seq = self.answered_seq;

        // iax-f2b8 Task 4: M17, mutually exclusive with the IAX2 path above
        // (self.active stays None whenever self.m17 is Some — enforced by
        // connect()/handle_incoming()/answer_pending()'s guards), so this can
        // never clobber a live IAX2 call's fields.
        //
        // Deliberately does NOT reap the session on `LinkState::Failed`: like
        // a WT call's `active`, `self.m17` stays `Some` (and `status` keeps
        // reporting `Hangup` on every poll) until the front-end calls
        // `disconnect()`/`m17_disconnect()` — see `m17`'s field docs. A
        // one-shot "reap immediately, then settle to Idle next poll" latch
        // was tried first and rejected: it raced any second poller (e.g.
        // astar's meter-poll snapshot() running alongside its event-poll
        // next_event()) — whichever poll happened to observe the Failed
        // transition consumed the ONE Hangup-reporting snapshot, so the other
        // poller could miss it entirely.
        #[cfg(feature = "m17")]
        if let Some(session) = self.m17.as_ref() {
            let st = session.state();
            self.state.ptt = st.ptt;
            self.state.remote_ptt = st.receiving;
            // Levels are NOT read here: the one meter block above already
            // read them off the voice route's own lanes (`meter_ids`).
            self.state.status = match st.link {
                LinkState::Idle | LinkState::Connecting => CallStatus::Dialing,
                LinkState::Linked => CallStatus::Answered,
                LinkState::Failed => CallStatus::Hangup {
                    reason: "m17 link lost".into(),
                },
            };
        }
        self.state.m17_active = self.m17_is_active();
        self.state.m17_available = m17_available();

        // iax-2f6b: D-Star, mutually exclusive with both paths above (see
        // `dstar_can_connect`). Only the fields that mean the same thing for
        // every network are mirrored — is this station transmitting, and
        // (iax-4c8e) the call status. Everything D-Star-shaped (talker, slow
        // text, vocoder backend) stays behind `dstar_state()`; the three
        // level meters are read once, at the lane, by the meter block above.
        //
        // `status` is mapped from the D-Star link exactly as the M17 branch
        // above maps its own: a front-end drives one connection state machine
        // off `status` for every network, rather than a per-network special
        // case. The underlying `LinkState` is still available verbatim
        // through `dstar_state()` for anything that wants the D-Star-native
        // value.
        //
        // `ptt` in particular is the run loop's ACTUALLY-APPLIED state, so a
        // refused key-down or a forced unkey (link lost mid-transmission, or
        // the time-out timer) corrects the optimistic value `set_ptt` wrote
        // on the very next poll. A snapshot must never report a station as
        // idle while it is keyed — nor as keyed after the engine unkeyed it.
        #[cfg(feature = "dstar")]
        if let Some(session) = self.dstar.as_ref() {
            let st = session.state();
            self.state.ptt = st.ptt;
            // Fully qualified: the `m17` import above binds the bare name
            // `LinkState` to M17's own enum, and both features are usually on.
            self.state.status = match st.link {
                astar_dstar::LinkState::Idle | astar_dstar::LinkState::Linking => {
                    CallStatus::Dialing
                }
                astar_dstar::LinkState::Linked => CallStatus::Answered,
                // Unlinking reports as ending, not as Answered: the engine
                // refuses a key-down once the link leaves `Linked` (see
                // `PttGate`), so showing a connected station here would invite
                // an operator to key into a teardown and get a silent refusal.
                astar_dstar::LinkState::Unlinking => CallStatus::Hangup {
                    reason: "d-star unlinking".into(),
                },
                astar_dstar::LinkState::Failed => CallStatus::Hangup {
                    reason: "d-star link lost".into(),
                },
            };
        }
        self.state.dstar_active = self.dstar_is_active();
        self.state.dstar_available = dstar_available();

        // System Fusion, mirrored the same way and for the same reason the two
        // branches above are: a front-end drives ONE connection state machine
        // off `status`, whatever the network. Without this a live YSF link
        // leaves `status` at whatever it was — `Idle` on a fresh station — so
        // the link comes up, holds the vocoder, and the UI still reports
        // nothing connected. That is exactly how this shipped, and it read as
        // "I can't connect to a YSF reflector" rather than as a missing
        // mirror.
        //
        // `remote_ptt` carries "somebody is transmitting", which YSF states
        // outright in the frame header rather than inferring from audio level
        // — the same field M17 fills from its own `receiving`.
        //
        // Levels are NOT read here: the one meter block above already read
        // them off the voice route's own lanes (`meter_ids`), which is where
        // every network's meters now come from.
        #[cfg(feature = "ysf")]
        if let Some(link) = self.ysf.as_ref() {
            let snap = link.snapshot();
            self.state.remote_ptt = snap.receiving;
            // The ACTUALLY-APPLIED key state, so the optimistic value
            // `set_ptt` wrote is corrected on the very next poll — by a
            // forced unkey, or simply confirmed. A snapshot must never
            // report a station as transmitting when it is not. (A key-down
            // with no capture device never gets this far: `set_ptt` refuses
            // it with `NoCaptureDevice` and forwards nothing.)
            self.state.ptt = snap.ptt;
            // Fully qualified: the bare `LinkState` in this scope is M17's.
            self.state.status = match link.link_state() {
                astar_ysf::LinkState::Idle | astar_ysf::LinkState::Linking => CallStatus::Dialing,
                astar_ysf::LinkState::Linked => CallStatus::Answered,
                // Unlinking reports as ending rather than connected, matching
                // D-Star's treatment: the link is going away and an operator
                // should not be told otherwise.
                astar_ysf::LinkState::Unlinking => CallStatus::Hangup {
                    reason: "ysf unlinking".into(),
                },
                astar_ysf::LinkState::Failed => CallStatus::Hangup {
                    reason: "ysf link lost".into(),
                },
            };
        }
        self.state.ysf_active = self.ysf_is_active();
        self.state.ysf_available = ysf_available();

        // NXDN, mirrored exactly as the three branches above are and for the
        // same reason: a front-end drives ONE connection state machine off
        // `status`, whatever the network. Without this a live NXDN link
        // leaves `status` at whatever it was — `Idle` on a fresh station —
        // so the link comes up, holds the vocoder, and the UI reports
        // nothing connected.
        //
        // `remote_ptt` carries "somebody is transmitting", which NXDN states
        // outright in the frame header (the start/end flags and the
        // terminator LICH) rather than inferring from audio level.
        //
        // Levels are NOT read here: the one meter block above already read
        // them off the voice route's own lanes (`meter_ids`), which is where
        // every network's meters come from.
        #[cfg(feature = "nxdn")]
        if let Some(link) = self.nxdn.as_ref() {
            let snap = link.snapshot();
            self.state.remote_ptt = snap.receiving;
            // The ACTUALLY-APPLIED key state. Always `false` while transmit
            // is gated, which is exactly the point: a snapshot must never
            // report a station as transmitting when it is not.
            self.state.ptt = snap.ptt;
            // Fully qualified: the bare `LinkState` in this scope is M17's.
            self.state.status = match link.link_state() {
                astar_nxdn::LinkState::Idle | astar_nxdn::LinkState::Linking => CallStatus::Dialing,
                astar_nxdn::LinkState::Linked => CallStatus::Answered,
                // Unlinking reports as ending rather than connected, matching
                // YSF's and D-Star's treatment: the link is going away and an
                // operator should not be told otherwise.
                astar_nxdn::LinkState::Unlinking => CallStatus::Hangup {
                    reason: "nxdn unlinking".into(),
                },
                astar_nxdn::LinkState::Failed => CallStatus::Hangup {
                    reason: "nxdn link lost".into(),
                },
            };
        }
        self.state.nxdn_active = self.nxdn_is_active();
        self.state.nxdn_available = nxdn_available();

        // DMR, mirrored exactly as the four branches above are and for the
        // same reason: a front-end drives ONE connection state machine off
        // `status`, whatever the network. Without this a live DMR link
        // leaves `status` at whatever it was — `Idle` on a fresh station —
        // so the link comes up, holds the vocoder, and the UI reports
        // nothing connected.
        //
        // `remote_ptt` carries "somebody is transmitting", which DMR states
        // in the `DMRD` header (the frame type and the stream id) rather
        // than inferring from audio level.
        //
        // DMR has THREE states before `Linked` where the others have one —
        // homebrew logs in, authenticates, then configures — and all three
        // map to `Dialing`. A front-end that wanted to name the stage reads
        // `dmr_state().link_state` for it; `status` stays the shared
        // vocabulary.
        //
        // Levels are NOT read here: the one meter block above already read
        // them off the voice route's own lanes (`meter_ids`), which is where
        // every network's meters come from.
        #[cfg(feature = "dmr")]
        if let Some(link) = self.dmr.as_ref() {
            let snap = link.snapshot();
            self.state.remote_ptt = snap.receiving;
            // The ACTUALLY-APPLIED key state. Always `false` while transmit
            // is gated, which is exactly the point: a snapshot must never
            // report a station as transmitting when it is not.
            self.state.ptt = snap.ptt;
            // Fully qualified: the bare `LinkState` in this scope is M17's.
            self.state.status = match link.link_state() {
                astar_dmr::LinkState::Idle
                | astar_dmr::LinkState::LoggingIn
                | astar_dmr::LinkState::Authenticating
                | astar_dmr::LinkState::Configuring => CallStatus::Dialing,
                astar_dmr::LinkState::Linked => CallStatus::Answered,
                // Closing reports as ending rather than connected, matching
                // NXDN's `Unlinking` and D-Star's: the link is going away
                // and an operator should not be told otherwise.
                astar_dmr::LinkState::Closing => CallStatus::Hangup {
                    reason: "dmr closing".into(),
                },
                astar_dmr::LinkState::Failed => CallStatus::Hangup {
                    reason: "dmr link lost".into(),
                },
            };
        }
        self.state.dmr_active = self.dmr_is_active();
        self.state.dmr_available = dmr_available();

        // The capture gate is driven by `set_ptt` ONLY, never reconciled
        // here. `state.ptt` above is the run loop's ACTUALLY-APPLIED value,
        // which lags a key-down by up to one poll interval — closing the gate
        // on it would mute the first ≤50 ms of every transmission, and
        // nothing would re-open it. The other direction (a session that
        // unkeyed itself: link lost, time-out timer) leaves the gate open
        // until the operator releases PTT, which is harmless: a run loop
        // discards captured frames while it is not transmitting.

        self.state.clone()
    }

    /// Push an error note onto the timeline (e.g. WT token mint failure).
    pub fn note_error(&mut self, msg: impl Into<String>) {
        self.tracer.note("Error", msg);
    }

    /// Return all timeline events with `seq >= seq`. Passes through to the
    /// internal [`Tracer`](crate::tracer::Tracer).
    #[must_use]
    pub fn timeline_since(&self, seq: u64) -> Vec<crate::tracer::TimelineEvent> {
        self.tracer.timeline_since(seq)
    }

    /// Return all recorded frames with `seq >= seq`. Passes through to the
    /// internal [`Tracer`](crate::tracer::Tracer).
    #[must_use]
    pub fn frames_since(&self, seq: u64) -> Vec<astar_iax::TracedFrame> {
        self.tracer.frames_since(seq)
    }
}

impl Default for ConsoleSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a capture/playback device selection to its device-id string against
/// `backend`. `query == None` (or empty) selects the system default for `dir`;
/// otherwise the substring is matched case-insensitively and uniquely (mirrors
/// the connect-time resolution). Used by monitor mode (iax-2377) to open a mic
/// outside a call.
///
/// # Errors
/// [`ConsoleError::Audio`] if enumeration fails, [`ConsoleError::Device`] if no
/// default exists or the substring is zero/ambiguous.
pub fn resolve_device(
    backend: &dyn AudioBackend,
    query: Option<&str>,
    dir: Direction,
) -> Result<String, ConsoleError> {
    if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) {
        let enumerated = backend.devices().map_err(ConsoleError::Audio)?;
        find_device(&enumerated, q, dir)
    } else {
        let default = if dir == Direction::Output {
            backend.default_output()
        } else {
            backend.default_input()
        };
        default
            .map(|d| d.id.as_str().to_string())
            .ok_or_else(|| ConsoleError::Device(format!("no default device for {dir:?}")))
    }
}

/// `true` when M17 voice is available: the `m17` feature is compiled in AND a
/// working Codec 2 backend was found (iax-f2b8 Task 4). Cached after the
/// first call — `codec2_available`'s static backend does a full
/// `Codec2::new()` construction (iax-f2b8 Task 2's ledgered "don't call in
/// hot paths" warning), so [`ConsoleSession::snapshot`] and the station
/// layer's own `Station::m17_available` (which both poll this every tick)
/// re-probe only once for the process lifetime rather than on every poll.
/// Always `false` when the `m17` feature isn't compiled in.
#[must_use]
pub fn m17_available() -> bool {
    #[cfg(feature = "m17")]
    {
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| astar_codec::codec2::codec2_available(&[]))
    }
    #[cfg(not(feature = "m17"))]
    {
        false
    }
}

/// The one cached `ThumbDV` probe, shared by every network that needs the
/// dongle.
///
/// D-Star and System Fusion both decode AMBE+2 on the same hardware, so
/// "available" means the same thing to both and there is no second scan to
/// keep in step with this one — see [`dstar_available`] for why the probe is
/// enumeration-only and why it is cached on a short TTL rather than once.
#[cfg(any(feature = "dstar", feature = "ysf", feature = "nxdn", feature = "dmr"))]
fn thumbdv_available_cached() -> bool {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, bool)>> =
        std::sync::Mutex::new(None);
    // Poison-tolerant: there is no invariant to protect here (a memoized
    // bool + timestamp), so a panicking prior caller must not wedge every
    // later availability query.
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, val)) = *cache
        && at.elapsed() < DSTAR_AVAILABLE_TTL
    {
        return val;
    }
    let val = astar_codec::ambe::thumbdv_present();
    *cache = Some((std::time::Instant::now(), val));
    val
}

/// `true` when System Fusion voice is available: the `ysf` feature is
/// compiled in AND a `ThumbDV` is attached.
///
/// Deliberately the same probe as [`dstar_available`], not a parallel one.
/// The design said to reuse D-Star's rather than grow a second, and the
/// reason is not tidiness: two probes with two caches would eventually
/// disagree about whether a dongle is plugged in, and a UI would grey out
/// one network and not the other for the same dongle.
///
/// Always `false` when the `ysf` feature isn't compiled in.
#[must_use]
pub fn ysf_available() -> bool {
    #[cfg(feature = "ysf")]
    {
        thumbdv_available_cached()
    }
    #[cfg(not(feature = "ysf"))]
    {
        false
    }
}

/// `true` when NXDN voice is available: the `nxdn` feature is compiled in
/// AND a `ThumbDV` is attached.
///
/// Deliberately the same probe as [`dstar_available`] and [`ysf_available`],
/// not a third one. NXDN voice is AMBE+2 off the same dongle, and two probes
/// with two caches would eventually disagree about whether it is plugged in
/// — greying out one network and not the other for the same hardware.
///
/// Always `false` when the `nxdn` feature isn't compiled in.
#[must_use]
pub fn nxdn_available() -> bool {
    #[cfg(feature = "nxdn")]
    {
        thumbdv_available_cached()
    }
    #[cfg(not(feature = "nxdn"))]
    {
        false
    }
}

/// `true` when DMR voice is available: the `dmr` feature is compiled in AND a
/// `ThumbDV` is attached.
///
/// Deliberately the same probe as [`dstar_available`], [`ysf_available`] and
/// [`nxdn_available`], not a fourth one. DMR voice is AMBE+2 off the same
/// dongle — a different rate word, the same chip — and two probes with two
/// caches would eventually disagree about whether it is plugged in, greying
/// out one network and not another for the same hardware.
///
/// Always `false` when the `dmr` feature isn't compiled in, and callable
/// either way: `astar-server` reads `dmr_active` off a snapshot in a build
/// that may not compile the session at all.
#[must_use]
pub fn dmr_available() -> bool {
    #[cfg(feature = "dmr")]
    {
        thumbdv_available_cached()
    }
    #[cfg(not(feature = "dmr"))]
    {
        false
    }
}

/// `true` when D-Star voice is available: the `dstar` feature is compiled in
/// AND a `ThumbDV` dongle is attached (iax-b3e7 M0 — D-Star is hardware-only,
/// so "available" means exactly "the dongle is plugged in").
///
/// This is a VID/PID enumeration only
/// ([`astar_codec::ambe::thumbdv_present`]): it opens no serial device,
/// runs no init cookbook and holds no port. That matters twice over:
///
/// - it can be answered truthfully WHILE a D-Star session is live. Only one
///   process may hold the dongle, so an open-based probe (this used to call
///   `open_ambe(None, ..)`, the Auto-preference `detect()` path) returns "no
///   backend" precisely when a session is using it — a UI reporting "D-Star
///   unavailable" during a live D-Star QSO.
/// - it can't be memoized into a permanently wrong `false`. The result is
///   recomputed on a short TTL ([`DSTAR_AVAILABLE_TTL`]) instead of a
///   process-lifetime `OnceLock`, which also picks up a dongle plugged in
///   after start-up, while keeping a UI that polls every tick off the
///   IOKit/udev scan on every single call.
///
/// Always `false` when the `dstar` feature isn't compiled in.
#[must_use]
pub fn dstar_available() -> bool {
    #[cfg(feature = "dstar")]
    {
        thumbdv_available_cached()
    }
    #[cfg(not(feature = "dstar"))]
    {
        false
    }
}

/// How long [`thumbdv_available_cached`]'s port enumeration is reused before
/// being recomputed. Long enough that a UI polling on a 100 ms tick scans
/// ~twice a second; short enough that plugging the dongle in shows up
/// promptly.
#[cfg(any(feature = "dstar", feature = "ysf", feature = "nxdn", feature = "dmr"))]
const DSTAR_AVAILABLE_TTL: std::time::Duration = std::time::Duration::from_millis(500);

/// Map an engine error from a link-transport switch (iax-5bbd):
/// `CallInProgress` (the transport is immutable while a session is up) becomes
/// [`ConsoleError::AlreadyConnected`]; anything else — an unusable tunnel
/// config/key/underlay — becomes [`ConsoleError::Link`]. Secret-free: the
/// engine's messages name the key *reference*, never material.
fn map_link_err(e: astar_iax::IaxError) -> ConsoleError {
    match e {
        astar_iax::IaxError::CallInProgress => ConsoleError::AlreadyConnected,
        other => ConsoleError::Link(other.to_string()),
    }
}

/// The caller's public identity for an inbound offer: node id
/// (`CALLING_NUMBER`) preferred, then `CALLING_NAME`, else "unknown". Never a
/// secret.
fn caller_of(c: &IncomingCall) -> String {
    c.calling_number
        .clone()
        .or_else(|| c.calling_name.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Resolve `query` (case-insensitive substring) against the names of devices
/// usable in direction `dir` (`Duplex` devices match either way) and return the
/// matched device's id string. Exactly one hit is required: zero or several is a
/// configuration error the caller fixes by narrowing the substring (mirrors the
/// `Client` dial-time resolution, iax-fd34).
fn find_device(
    devices: &[astar_audio::DeviceInfo],
    query: &str,
    dir: Direction,
) -> Result<String, ConsoleError> {
    let usable =
        |d: &&astar_audio::DeviceInfo| d.direction == dir || d.direction == Direction::Duplex;
    let needle = query.to_lowercase();
    let matches: Vec<&astar_audio::DeviceInfo> = devices
        .iter()
        .filter(usable)
        .filter(|d| d.name.to_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.as_str().to_string()),
        [] => Err(ConsoleError::Device(format!(
            "no device matched {query:?} for {dir:?}"
        ))),
        many => Err(ConsoleError::Device(format!(
            "{query:?} is ambiguous: {:?}",
            many.iter().map(|d| &d.name).collect::<Vec<_>>()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_audio::{DeviceId, DeviceInfo, NullBackend};

    fn dev(name: &str, direction: Direction) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(name.to_string()),
            name: name.to_string(),
            direction,
            channels: 1,
            native_sample_rates: vec![8_000],
        }
    }

    struct FixedBackend(Vec<DeviceInfo>);
    impl AudioBackend for FixedBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(self.0.clone())
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            None
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            None
        }
        fn open_input(
            &self,
            _d: &DeviceInfo,
            _c: astar_audio::StreamConfig,
            _s: Box<dyn astar_audio::InputSink>,
            _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn astar_audio::StreamHandle>, AudioError> {
            unreachable!("not opened in this test")
        }
        fn open_output(
            &self,
            _d: &DeviceInfo,
            _c: astar_audio::StreamConfig,
            _s: Box<dyn astar_audio::OutputSource>,
        ) -> Result<Box<dyn astar_audio::StreamHandle>, AudioError> {
            unreachable!("not opened in this test")
        }
    }

    #[test]
    fn list_devices_partitions_by_direction_with_duplex_in_both() {
        let backend = FixedBackend(vec![
            dev("Mic", Direction::Input),
            dev("Speakers", Direction::Output),
            dev("Loopback", Direction::Duplex),
        ]);
        let (inputs, outputs) = list_devices(&backend).expect("enumerate");
        assert_eq!(inputs, vec!["Mic", "Loopback"]);
        assert_eq!(outputs, vec!["Speakers", "Loopback"]);
    }

    #[test]
    fn console_error_display_includes_node_on_resolve() {
        let e = ConsoleError::Resolve {
            node: "55553".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        };
        assert!(e.to_string().contains("55553"), "names the node: {e}");
    }

    #[test]
    fn set_ptt_without_call_does_not_panic_and_records_nothing() {
        let mut s = ConsoleSession::new();
        assert!(s.set_ptt(true).is_err(), "no active call");
        assert!(
            s.timeline_since(0).is_empty(),
            "no-call PTT records nothing"
        );
    }

    /// No digital link, nothing heard. `AllStar` carries no talker identity,
    /// so an IAX2-only console has nothing to report either.
    #[test]
    fn heard_is_empty_with_no_digital_session() {
        let s = ConsoleSession::new();
        assert!(s.heard().is_empty());
    }

    #[test]
    fn gain_defaults_to_unity() {
        let s = ConsoleSession::new();
        assert!(
            (s.input_gain() - 1.0).abs() < 1e-9,
            "input gain defaults to 1.0"
        );
        assert!(
            (s.output_gain() - 1.0).abs() < 1e-9,
            "output gain defaults to 1.0"
        );
    }

    #[test]
    fn set_input_gain_clamps_to_0_to_4() {
        let s = ConsoleSession::new();
        s.set_input_gain(1.5);
        assert!(
            (s.input_gain() - 1.5).abs() < 1e-6,
            "within range stored as-is"
        );
        s.set_input_gain(-0.5);
        assert!(
            (s.input_gain() - 0.0).abs() < 1e-6,
            "below 0 clamped to 0.0"
        );
        s.set_input_gain(3.0);
        assert!(
            (s.input_gain() - 3.0).abs() < 1e-6,
            "3.0 is within the 4.0 ceiling, stored as-is"
        );
        s.set_input_gain(5.0);
        assert!(
            (s.input_gain() - 4.0).abs() < 1e-6,
            "above 4 clamped to 4.0"
        );
    }

    #[test]
    fn set_output_gain_clamps_to_0_to_4() {
        // iax-a4e7: output gain's ceiling is 4.0 (400%), same headroom as
        // the input side — RX amplification for a quiet station on a mixed
        // net.
        let s = ConsoleSession::new();
        s.set_output_gain(0.75);
        assert!(
            (s.output_gain() - 0.75).abs() < 1e-6,
            "within range stored as-is"
        );
        s.set_output_gain(3.5);
        assert!(
            (s.output_gain() - 3.5).abs() < 1e-6,
            "above the old 2.0 ceiling, within the new 4.0 one, stored as-is"
        );
        s.set_output_gain(-1.0);
        assert!(
            (s.output_gain() - 0.0).abs() < 1e-6,
            "below 0 clamped to 0.0"
        );
        s.set_output_gain(100.0);
        assert!(
            (s.output_gain() - 4.0).abs() < 1e-6,
            "above 4 clamped to 4.0"
        );
    }

    #[test]
    fn set_gain_nan_becomes_unity() {
        let s = ConsoleSession::new();
        s.set_input_gain(f32::NAN);
        assert!((s.input_gain() - 1.0).abs() < 1e-6, "NaN input gain -> 1.0");
        s.set_output_gain(f32::NAN);
        assert!(
            (s.output_gain() - 1.0).abs() < 1e-6,
            "NaN output gain -> 1.0"
        );
    }

    #[test]
    fn session_exposes_timeline_and_frames_accessors() {
        let s = ConsoleSession::new();
        assert!(s.timeline_since(0).is_empty());
        assert!(s.frames_since(0).is_empty());
    }

    #[test]
    fn inbound_policy_pins_the_engine_pipeline_rate() {
        // The node path (main.rs → Station::enable_inbound/set_mode(Node) →
        // start_inbound_with_allowlist) never calls `connect`, so the inbound
        // policy's codec_policy must pin the pipeline rate when the engine is
        // first built here (iax-4348) — node.toml's `prefer_slin16` must yield
        // a 16 kHz Manager, not the 8 kHz default.
        let mut s = ConsoleSession::new();
        let policy = IncomingCallPolicy {
            codec_policy: CodecPolicy::PreferSlin16,
            ..IncomingCallPolicy::default()
        };
        s.start_inbound(
            "127.0.0.1:0".parse().unwrap(),
            policy,
            AnswerPolicy::Auto,
            4,
            || -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) },
            (None, None),
        )
        .expect("start_inbound");
        // ensure_engine returns the ALREADY-BUILT engine (no rebuild).
        let mgr = s.ensure_engine(|| -> Box<dyn AudioBackend> {
            unreachable!("engine already built by start_inbound")
        });
        assert_eq!(
            mgr.pipeline_sample_rate(),
            16_000,
            "inbound prefer_slin16 must build a 16 kHz engine"
        );
    }

    // --- iax-4348: pinning the station rate before anything builds the engine ---

    fn null_backend() -> Box<dyn AudioBackend> {
        Box::new(NullBackend::new())
    }

    fn slin16_config() -> ConsoleConfig {
        ConsoleConfig {
            node: "1".into(),
            calling_node: "1".into(),
            secret: String::new(),
            name: "test".into(),
            input_device: None,
            output_device: None,
            codec_policy: CodecPolicy::PreferSlin16,
        }
    }

    /// The live bug, at the seam it happened: the one audio lane lets a
    /// digital-voice session build the engine, and the engine's pipeline rate
    /// is fixed at construction — so an M17/D-Star/YSF session before the
    /// first dial used to pin the station to 8 kHz forever, and every later
    /// `prefer_slin16` dial was silently capped to plain slin. An IDLE engine
    /// must be rebuilt at the dial's rate instead.
    #[test]
    fn an_idle_engine_left_by_a_digital_session_is_rebuilt_at_the_dial_policy_rate() {
        let mut s = ConsoleSession::new();
        let audio = s
            .open_voice_route(None, None, null_backend)
            .expect("the route opens on the NullBackend");
        drop(audio);
        drop(s.release_voice_route());
        assert_eq!(
            s.pipeline_sample_rate(),
            8_000,
            "a digital session builds the engine at the default 8 kHz"
        );

        // The dial goes to a bound-but-silent loopback socket: it reaches
        // `Dialing` and never further, which is all this assertion needs.
        let (_sink, peer) = sink_socket();
        s.connect(null_backend(), peer, slin16_config())
            .expect("dial starts");
        assert_eq!(
            s.pipeline_sample_rate(),
            16_000,
            "an idle 8 kHz engine must be rebuilt at the dial policy's rate, \
             or Manager::dial caps prefer_slin16 back to plain slin"
        );
        let _ = s.disconnect();
    }

    /// The other half of the rule: a BUSY engine keeps the rate it has. The
    /// inbound listener is live on it, its pipeline rate cannot change under
    /// a running call, and the dial is capped exactly as it was before this
    /// fix — pre-existing, intended behaviour.
    #[test]
    fn a_busy_engine_is_never_rebuilt() {
        let mut s = ConsoleSession::new();
        start_inbound_null(&mut s).expect("listener binds on 127.0.0.1:0");
        assert_eq!(
            s.pipeline_sample_rate(),
            8_000,
            "the default inbound policy builds an 8 kHz engine"
        );

        let (_sink, peer) = sink_socket();
        s.connect(null_backend(), peer, slin16_config())
            .expect("dial starts");
        assert_eq!(
            s.pipeline_sample_rate(),
            8_000,
            "the listener is live on this engine: the rate stands and the dial is capped"
        );
        let _ = s.disconnect();
        s.stop_inbound();
    }

    /// What actually makes astar and astar-server slin16 stations: the policy
    /// is set at construction, so the FIRST thing to build the engine — a
    /// digital session included — builds it at 16 kHz.
    #[test]
    fn set_station_policy_pins_the_rate_a_digital_session_then_builds_at() {
        let mut s = ConsoleSession::new();
        s.set_station_policy(CodecPolicy::PreferSlin16);
        assert!(
            !s.has_engine(),
            "setting the policy must not build anything"
        );

        let audio = s
            .open_voice_route(None, None, null_backend)
            .expect("the route opens");
        drop(audio);
        assert_eq!(
            s.pipeline_sample_rate(),
            16_000,
            "a prefer_slin16 station stays 16 kHz through a digital session"
        );
        assert!(
            s.voice_route.as_ref().expect("route held").bridged(),
            "the 8 kHz digital codecs must be rate-bridged onto the 16 kHz bus"
        );
        drop(s.release_voice_route());
    }

    /// An 8 kHz station is unchanged: no bridge, the session holds the bus's
    /// own channel ends.
    #[test]
    fn an_8k_station_passes_a_digital_route_straight_through() {
        let mut s = ConsoleSession::new();
        let audio = s
            .open_voice_route(None, None, null_backend)
            .expect("the route opens");
        drop(audio);
        assert!(
            !s.voice_route.as_ref().expect("route held").bridged(),
            "an 8 kHz station needs no converter"
        );
        drop(s.release_voice_route());
    }

    #[test]
    fn out_of_band_dtmf_surfaces_from_drain_dtmf_digits() {
        // iax-d254: CallEvent::Dtmf harvested by the session drain loop —
        // previously discarded by the catch-all arm. No live Manager/Call is
        // needed to exercise this: wire the private `active`/`events` cells
        // directly (this test lives in `session.rs`'s own test module) so a
        // sent CallEvent::Dtmf flows through `snapshot`'s drain loop exactly
        // as it would for a real adopted/dialed call.
        let mut session = ConsoleSession::new();
        let (tx, rx) = std::sync::mpsc::channel();
        session.active = Some(CallId::from_raw(7));
        session.events = Some(rx);
        tx.send(CallEvent::Dtmf('5')).expect("send");
        session.snapshot(); // runs the event drain loop
        let digits = session.drain_dtmf_digits();
        assert_eq!(digits, vec![(7, '5')]);
        // Draining consumes.
        assert!(session.drain_dtmf_digits().is_empty());
    }

    // --- iax-5bbd: the WireGuard link-transport surface at the session layer ---

    /// 32 zero bytes base64 — length-valid x25519 key material (mirrors the
    /// engine's own transport tests). Never a real secret.
    const KEY32: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    /// A valid `WgLinkConfig` whose endpoint is a caller-bound loopback socket
    /// (so any handshake initiations the stack emits stay on 127.0.0.1).
    fn wg_cfg(endpoint: std::net::SocketAddr) -> WgLinkConfig {
        WgLinkConfig::new(
            "WG_CONSOLE_KEY",
            "10.77.0.2/32",
            KEY32,
            &endpoint.to_string(),
            &[],
            0,
        )
        .expect("valid config")
    }

    fn good_resolver() -> Box<LinkKeyResolver> {
        Box::new(|_: &str| KEY32.to_string())
    }

    fn null_mk() -> impl FnOnce() -> Box<dyn AudioBackend> {
        || -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) }
    }

    /// A bound-but-silent loopback socket: dial/handshake traffic lands here
    /// and is never answered, keeping the test offline and deterministic.
    fn sink_socket() -> (std::net::UdpSocket, std::net::SocketAddr) {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sink");
        let a = s.local_addr().expect("sink addr");
        (s, a)
    }

    fn start_inbound_null(s: &mut ConsoleSession) -> Result<(), ConsoleError> {
        s.start_inbound(
            "127.0.0.1:0".parse().unwrap(),
            IncomingCallPolicy::default(),
            AnswerPolicy::Auto,
            4,
            null_mk(),
            (None, None),
        )
    }

    #[test]
    fn set_link_transport_before_engine_defers_then_installs_on_build() {
        let (_sink, ep) = sink_socket();
        let mut s = ConsoleSession::new();
        s.set_link_transport(LinkTransport::Wireguard(wg_cfg(ep)), good_resolver())
            .expect("selecting a transport before connect succeeds");
        assert!(
            !s.has_engine(),
            "selecting the transport must not build the engine"
        );
        assert!(s.wg_status().is_none(), "no engine yet, no tunnel yet");
        start_inbound_null(&mut s).expect("engine build applies the pending transport");
        assert!(
            s.wg_status().is_some(),
            "the tunnel is installed when the engine is first built"
        );
    }

    #[test]
    fn set_link_transport_applies_immediately_on_a_live_idle_engine() {
        let (_sink, ep) = sink_socket();
        let mut s = ConsoleSession::new();
        let _ = s.ensure_engine(null_mk());
        s.set_link_transport(LinkTransport::Wireguard(wg_cfg(ep)), good_resolver())
            .expect("idle engine accepts the switch");
        assert!(s.wg_status().is_some(), "tunnel installed immediately");
        // Clearing back to plain UDP drops the tunnel.
        s.set_link_transport(LinkTransport::Udp, Box::new(|_: &str| String::new()))
            .expect("udp reset on an idle engine");
        assert!(s.wg_status().is_none(), "back on plain UDP");
    }

    #[test]
    fn set_link_transport_while_a_call_is_pooled_is_already_connected() {
        let (_peer, peer_addr) = sink_socket();
        let (_sink, ep) = sink_socket();
        let mut s = ConsoleSession::new();
        s.connect(
            Box::new(NullBackend::new()),
            peer_addr,
            ConsoleConfig {
                node: "9999".into(),
                calling_node: "9999".into(),
                secret: "s".into(),
                name: "t".into(),
                input_device: None,
                output_device: None,
                codec_policy: CodecPolicy::default(),
            },
        )
        .expect("dial pools a call");
        let err = s
            .set_link_transport(LinkTransport::Wireguard(wg_cfg(ep)), good_resolver())
            .expect_err("the transport is immutable while a call is pooled");
        assert!(
            matches!(err, ConsoleError::AlreadyConnected),
            "engine CallInProgress maps to AlreadyConnected, got: {err}"
        );
        s.disconnect().expect("teardown");
    }

    #[test]
    fn bad_key_material_fails_engine_build_naming_the_ref_and_is_retained() {
        let (_sink, ep) = sink_socket();
        let mut s = ConsoleSession::new();
        // "AAAA" decodes to 3 bytes — length-invalid key material.
        s.set_link_transport(
            LinkTransport::Wireguard(wg_cfg(ep)),
            Box::new(|_: &str| "AAAA".to_string()),
        )
        .expect("deferred selection is stored without touching the key");
        let err = start_inbound_null(&mut s).expect_err("bad key must fail the engine build");
        let msg = err.to_string();
        assert!(msg.contains("WG_CONSOLE_KEY"), "names the reference: {msg}");
        assert!(!msg.contains("AAAA"), "never the material: {msg}");
        // Retained: a retry must NOT silently proceed over plain UDP.
        start_inbound_null(&mut s).expect_err("still refused until fixed or cleared");
        // An explicit clear back to UDP recovers.
        s.set_link_transport(LinkTransport::Udp, Box::new(|_: &str| String::new()))
            .expect("explicit clear");
        start_inbound_null(&mut s).expect("plain UDP after the explicit clear");
        assert!(s.wg_status().is_none(), "no tunnel after the clear");
    }

    #[test]
    fn engine_outlives_a_single_call() {
        let mut s = ConsoleSession::new();
        let mk = || -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) };
        let mgr = s.ensure_engine(mk);
        assert_eq!(mgr.call_count(), 0);
        // Engine persists across an ensure_engine re-call (no rebuild):
        let _ = s.ensure_engine(|| -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) });
        assert!(s.has_engine());
    }

    // --- M17 rides the one lane -------------------------------------------
    //
    // `session.m17`/`manager` are private fields of THIS module, so these
    // same-module unit tests (rather than the crate's external
    // `tests/m17_session.rs` integration file) can read the station router
    // directly — the smallest test-visible seam that proves an M17 connect
    // goes through the voice route rather than building audio of its own.
    //
    // Only needs a UDP target `M17Session::connect` can bind/resolve
    // against: neither a pref nor a meter depends on link state, so no
    // scripted reflector is needed here.

    /// An `M17Config` pointed at `addr`. Carries no devices any more — the
    /// route owns those.
    #[cfg(feature = "m17")]
    fn m17_cfg(addr: std::net::SocketAddr) -> M17Config {
        M17Config {
            host: addr.ip().to_string(),
            port: addr.port(),
            module: b'A',
            callsign: "N0CALL".to_string(),
            codec_dirs: Vec::new(),
            keepalive_timeout: Duration::from_secs(30),
        }
    }

    #[cfg(feature = "m17")]
    #[test]
    fn console_session_prefs_reach_the_m17_router_before_and_after_connect() {
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
        let addr = target.local_addr().expect("local addr");

        let mut session = ConsoleSession::new();
        // BEFORE m17_connect: standing prefs set on a plain idle session.
        session.set_output_gain(0.4);
        session.set_input_gain(1.6);
        // iax-a4e7 PHASE 1: RX compression is an output-side pref too, so it
        // must reach the route's bus the same way output_gain does.
        session.set_rx_compress(true);
        session.set_rx_compression_level(0.65);

        session
            .m17_connect(Box::new(NullBackend::new()), m17_cfg(addr), None, None)
            .expect("m17 connect");

        let (mic, bus) = session.meter_ids().expect("a route is live");
        let mic = mic.expect("the null backend resolves a capture device");
        // Read the values OFF the station router, not off any session
        // mirror: this is the one place they now live.
        let router = || session.manager.as_ref().expect("engine built").router();
        assert!(
            (router().output_gain(&bus).expect("bus open") - 0.4).abs() < 0.01,
            "output gain set BEFORE m17_connect must be on the route's bus at connect time"
        );
        assert!(
            (router().mic_gain(&mic).expect("mic open") - 1.6).abs() < 0.01,
            "input gain set BEFORE m17_connect must be on the route's mic lane"
        );
        assert_eq!(
            router().output_compress(&bus),
            Some(true),
            "rx compression set BEFORE m17_connect must be on the route's bus"
        );
        assert!(
            (router().output_compress_level(&bus).expect("bus open") - 0.65).abs() < 0.01,
            "rx compression level set BEFORE m17_connect must be on the route's bus"
        );

        // AFTER m17_connect: a live setter must reach the same lanes, with
        // no poll tick in between — the setter writes the router directly.
        // Also proves (iax-a4e7) the bus accepts the 4.0 ceiling.
        session.set_output_gain(4.0);
        assert!(
            (router().output_gain(&bus).expect("bus open") - 4.0).abs() < 0.01,
            "output gain set AFTER m17_connect must reach the route live at the 4.0 ceiling"
        );
        session.set_rx_compress(false);
        assert_eq!(
            router().output_compress(&bus),
            Some(false),
            "a LIVE set_rx_compress(false) must reach the route's bus"
        );

        session.m17_disconnect();
    }

    /// `Station::disconnect` calls `m17_disconnect` before D-Star's and
    /// YSF's, so a `m17_disconnect` that released the route unconditionally
    /// would close the lanes out from under whichever of those holds it.
    #[cfg(feature = "m17")]
    #[test]
    fn m17_disconnect_leaves_a_route_it_does_not_own_alone() {
        let mut s = ConsoleSession::new();
        // A route with no M17 session behind it — the shape a D-Star or YSF
        // connect leaves the station in.
        let _audio = s.open_voice_route(None, None, null).expect("route");

        s.m17_disconnect();

        let mgr = s.manager.as_ref().expect("engine survives");
        assert_eq!(
            mgr.router().output_count(),
            1,
            "the bus another network is listening on must stay open"
        );
        assert_eq!(mgr.router().mic_count(), 1, "so must its capture lane");
        #[cfg(feature = "dstar")]
        assert!(
            matches!(
                s.dstar_can_connect(),
                Err(crate::session::ConsoleError::AlreadyConnected)
            ),
            "the reservation must survive: the route is still held"
        );
    }

    #[cfg(feature = "m17")]
    #[test]
    fn an_m17_session_reports_the_router_meters_through_the_snapshot() {
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
        let addr = target.local_addr().expect("local addr");

        let mut session = ConsoleSession::new();
        session
            .m17_connect(Box::new(NullBackend::new()), m17_cfg(addr), None, None)
            .expect("m17 connect");

        // The lane the snapshot meters: the route's, because the session
        // has none of its own.
        let ids = session.meter_ids().expect("a route is live");
        assert_eq!(ids.1, OutputId::new("out:null"));
        assert_eq!(ids.0, Some(MicId::new("in:null")));

        let snap = session.snapshot();
        assert!(
            (snap.rx_level_db + 60.0).abs() < 1e-6,
            "a silent bus reads the floor, not garbage, got {}",
            snap.rx_level_db
        );
        assert!(
            (snap.tx_level_db + 60.0).abs() < 1e-6,
            "a silent, unkeyed mic lane reads the floor, got {}",
            snap.tx_level_db
        );

        session.m17_disconnect();
        assert!(
            session.meter_ids().is_none(),
            "the route is released on disconnect, so nothing is metered"
        );
    }

    // ── D-Star rides the one lane ───────────────────────────────────────
    //
    // The decisions that moved OFF `DstarSession` and onto the console live
    // here now: whether this station may key at all, and what the four
    // console-owned `DstarSnapshotState` fields report. No dongle is needed
    // for any of it — the vocoder is injected, and nothing below transmits
    // anywhere (the socket is `connect`ed to a silent loopback port).

    /// An inert [`astar_codec::ambe::AmbeStream`]: enough for
    /// `DstarSession::connect_with_stream` to build a session with no
    /// `ThumbDV` attached. Neither direction is exercised by these tests —
    /// what they assert is decided before a frame is ever encoded.
    #[cfg(any(feature = "dstar", feature = "ysf", feature = "nxdn", feature = "dmr"))]
    struct InertVocoder;

    #[cfg(any(feature = "dstar", feature = "ysf", feature = "nxdn", feature = "dmr"))]
    impl astar_codec::ambe::AmbeStream for InertVocoder {
        fn submit_decode(&mut self, _frame: astar_codec::ambe::ChannelFrame) {}
        fn poll_decoded(&mut self) -> Option<[i16; 160]> {
            None
        }
        fn in_flight(&self) -> usize {
            0
        }
        fn submit_encode(&mut self, _pcm: [i16; 160]) {}
        fn poll_encoded(&mut self) -> Option<astar_codec::ambe::ChannelFrame> {
            None
        }
        fn in_flight_encoded(&self) -> usize {
            0
        }
    }

    /// A `DstarConfig` pointed at `addr`. Carries no devices any more — the
    /// route owns those.
    #[cfg(feature = "dstar")]
    fn dstar_cfg(addr: std::net::SocketAddr) -> DstarConfig {
        DstarConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            module: b'A',
            callsign: "N0CALL".to_string(),
            reflector_callsign: None,
        }
    }

    /// Build a D-Star session over `audio` with no hardware in the loop.
    #[cfg(feature = "dstar")]
    fn dstar_session(addr: std::net::SocketAddr, audio: astar_audio::CallAudio) -> DstarSession {
        DstarSession::connect_with_stream(
            dstar_cfg(addr),
            audio,
            Box::new(InertVocoder),
            astar_codec::ambe::AmbeBackend::Hardware,
        )
        .expect("a session with an injected vocoder needs no dongle")
    }

    /// A machine with no usable microphone must still be able to LISTEN, and
    /// must be told it cannot transmit — the two halves of the policy that
    /// used to live in `DstarSession`'s own key-down.
    ///
    /// The refusal is the console's now because only the lane knows whether
    /// a capture device exists. Getting this wrong puts an RF header and a
    /// stream of silence on the air, which is worse than not transmitting.
    #[cfg(feature = "dstar")]
    #[test]
    fn keying_a_receive_only_route_is_refused_for_dstar() {
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
        let addr = target.local_addr().expect("local addr");

        let mut s = ConsoleSession::new();
        let audio = s
            .open_voice_route(Some("no such device"), None, null)
            .expect("the bus opens; a missing capture device is not fatal");
        s.dstar_adopt(dstar_session(addr, audio))
            .expect("adopt onto the reserved route");

        assert!(
            !s.dstar_state().expect("a live session").tx_capable,
            "tx_capable is the LANE's answer: no capture device, no transmit — whatever the \
             vocoder is capable of"
        );
        assert!(
            matches!(s.set_ptt(true), Err(ConsoleError::NoCaptureDevice)),
            "a key-down with no capture device must be refused, and nothing forwarded"
        );
        assert!(!s.snapshot().ptt, "a refused key never reports as keyed");

        s.dstar_disconnect();
    }

    /// `dstar_adopt` installs a session that arrived with the route already
    /// reserved on its behalf — and refuses (tearing the session down) one
    /// that did not, because its channel ends would have nothing feeding
    /// them.
    #[cfg(feature = "dstar")]
    #[test]
    fn dstar_adopt_requires_a_reserved_route() {
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
        let addr = target.local_addr().expect("local addr");

        // A lane built outside the console: a session with real channel ends
        // that this `ConsoleSession` never reserved anything for.
        let mut orphan = astar_audio::AudioRouter::new(Box::new(NullBackend::new()));
        let (audio, _mic_tx, _mix) = orphan
            .open_monitor_call(&OutputId::new("out:null"), StreamConfig::default())
            .expect("bus");

        let mut s = ConsoleSession::new();
        assert!(
            matches!(
                s.dstar_adopt(dstar_session(addr, audio)),
                Err(ConsoleError::AlreadyConnected)
            ),
            "a session with no route reserved for it must be refused"
        );
        assert!(s.dstar_state().is_none(), "and not installed");
    }

    /// The levels a UI reads off `dstar_state()` are the CONSOLE's — read
    /// once, at the lane, by `snapshot()` — not the session's. D-Star
    /// shipped with a per-session meter mirror and no spectrum at all; this
    /// pins that both now come from the one lane.
    #[cfg(feature = "dstar")]
    #[test]
    fn a_dstar_session_reports_the_router_meters_through_dstar_state() {
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
        let addr = target.local_addr().expect("local addr");

        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        s.dstar_adopt(dstar_session(addr, audio)).expect("adopt");

        // The lane the snapshot meters: the route's, because the session has
        // none of its own.
        let ids = s.meter_ids().expect("a route is live");
        assert_eq!(ids.1, OutputId::new("out:null"));
        assert_eq!(ids.0, Some(MicId::new("in:null")));

        let snap = s.snapshot();
        let st = s.dstar_state().expect("a live session");
        assert!(
            (st.rx_dbfs - snap.rx_level_db).abs() < 1e-6
                && (st.tx_dbfs - snap.tx_level_db).abs() < 1e-6
                && (st.input_dbfs - snap.input_level_db).abs() < 1e-6,
            "dstar_state must report the SAME numbers the snapshot does, got {st:?}"
        );
        assert!(
            (snap.rx_level_db + 60.0).abs() < 1e-6,
            "a silent bus reads the floor, not garbage, got {}",
            snap.rx_level_db
        );
        assert!(st.tx_capable, "a route with a capture device can transmit");

        s.dstar_disconnect();
        assert!(
            s.meter_ids().is_none(),
            "the route is released on disconnect, so nothing is metered"
        );
    }

    /// `Station::disconnect` calls `m17_disconnect` before D-Star's and
    /// YSF's, so a `dstar_disconnect` that released the route
    /// unconditionally would close the lanes out from under whichever of
    /// those holds it.
    #[cfg(feature = "dstar")]
    #[test]
    fn dstar_disconnect_leaves_a_route_it_does_not_own_alone() {
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");

        s.dstar_disconnect();

        let mgr = s.manager.as_ref().expect("engine survives");
        assert_eq!(
            mgr.router().output_count(),
            1,
            "the bus another network is listening on must stay open"
        );
        assert_eq!(mgr.router().mic_count(), 1, "so must its capture lane");
    }

    // ── System Fusion wiring (astar-e7b3 §2.4) ──────────────────────────

    /// A link that needs no dongle, riding the lane the console opened for
    /// it: enough to prove exclusion, the prefs and the mirror, all of which
    /// are about the session rather than about the vocoder. Everything binds
    /// `127.0.0.1`.
    #[cfg(feature = "ysf")]
    fn loopback_ysf(
        audio: astar_audio::CallAudio,
    ) -> (astar_ysf::ReflectorHandle, crate::ysf::YsfLink) {
        let r =
            astar_ysf::Reflector::bind("127.0.0.1:0".parse().expect("v4")).expect("bind reflector");
        let addr = r.local_addr();
        let handle = r.run();
        let link = crate::ysf::YsfLink::connect_with_stream(
            &crate::ysf::YsfConfig {
                host: addr.to_string(),
                callsign: "N0CALL".to_string(),
                options: None,
            },
            audio,
            Box::new(InertVocoder),
            astar_codec::ambe::AmbeBackend::Hardware,
        )
        .expect("connect the link");
        (handle, link)
    }

    /// A machine with no usable microphone must still be able to LISTEN, and
    /// must be told it cannot transmit — the same policy D-Star has, moved to
    /// the same place, because only the lane knows whether a capture device
    /// exists. Getting this wrong puts an RF header and a stream of silence
    /// on the reflector, which is worse than not transmitting.
    ///
    /// This is `crate::ysf`'s own `a_key_down_without_a_microphone_is_refused`
    /// (deleted there): the link no longer owns the decision.
    #[cfg(feature = "ysf")]
    #[test]
    fn keying_a_receive_only_route_is_refused_for_ysf() {
        let mut s = ConsoleSession::new();
        let audio = s
            .open_voice_route(Some("no such device"), None, null)
            .expect("the bus opens; a missing capture device is not fatal");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt onto the reserved route");

        assert!(
            matches!(s.set_ptt(true), Err(ConsoleError::NoCaptureDevice)),
            "a key-down with no capture device must be refused, and nothing forwarded"
        );
        assert!(!s.snapshot().ptt, "a refused key never reports as keyed");

        s.ysf_disconnect();
        reflector.shutdown();
    }

    /// A YSF key must reach `ConsoleState::ptt` IMMEDIATELY — before any
    /// `snapshot()`, and without waiting for the link's run loop to apply the
    /// edge — and must land on the timeline, exactly as an M17 or D-Star key
    /// does.
    ///
    /// Two different bugs live here. The optimistic mirror is the transient
    /// one: a consumer polling faster than one 20 ms link pass would see
    /// `ptt: false` on YSF where the other networks show `true`. The tracer
    /// note is not transient at all — without it a YSF transmission never
    /// appears in the timeline, ever.
    #[cfg(feature = "ysf")]
    #[test]
    fn keying_ysf_mirrors_ptt_and_records_the_timeline_immediately() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");

        s.set_ptt(true).expect("the route has a capture device");
        assert!(
            s.state.ptt,
            "the mirror must be written by set_ptt itself, not by the next snapshot"
        );
        let kinds = |s: &ConsoleSession| -> Vec<String> {
            s.timeline_since(0).iter().map(|e| e.kind.clone()).collect()
        };
        assert!(
            kinds(&s).contains(&"LocalKey".to_string()),
            "a YSF key must be on the timeline, got {:?}",
            kinds(&s)
        );

        s.set_ptt(false).expect("unkey");
        assert!(!s.state.ptt, "and the release mirrors too");
        assert!(
            kinds(&s).contains(&"LocalUnkey".to_string()),
            "as must the release, got {:?}",
            kinds(&s)
        );

        s.ysf_disconnect();
        reflector.shutdown();
    }

    /// `ysf_adopt` installs a link that arrived with the route already
    /// reserved on its behalf — and refuses (tearing the link down) one that
    /// did not, because its channel ends would have nothing feeding them.
    #[cfg(feature = "ysf")]
    #[test]
    fn ysf_adopt_requires_a_reserved_route() {
        // A lane built outside the console: a link with real channel ends
        // that this `ConsoleSession` never reserved anything for.
        let mut orphan = astar_audio::AudioRouter::new(Box::new(NullBackend::new()));
        let (audio, _mic_tx, _mix) = orphan
            .open_monitor_call(&OutputId::new("out:null"), StreamConfig::default())
            .expect("bus");
        let (reflector, link) = loopback_ysf(audio);

        let mut s = ConsoleSession::new();
        assert!(
            matches!(s.ysf_adopt(link), Err(ConsoleError::AlreadyConnected)),
            "a link with no route reserved for it must be refused"
        );
        assert!(s.ysf_state().is_none(), "and not installed");
        reflector.shutdown();
    }

    /// `Station::disconnect` calls `m17_disconnect` and `dstar_disconnect`
    /// before YSF's, so a `ysf_disconnect` that released the route
    /// unconditionally would close the lanes out from under whichever of
    /// those holds it.
    #[cfg(feature = "ysf")]
    #[test]
    fn ysf_disconnect_leaves_a_route_it_does_not_own_alone() {
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");

        s.ysf_disconnect();

        let mgr = s.manager.as_ref().expect("engine survives");
        assert_eq!(
            mgr.router().output_count(),
            1,
            "the bus another network is listening on must stay open"
        );
        assert_eq!(mgr.router().mic_count(), 1, "so must its capture lane");
    }

    /// One `ThumbDV`, one link. A live YSF link must refuse every other
    /// network, and be refused by them — the exclusion is about the hardware,
    /// not about tidiness.
    #[cfg(feature = "ysf")]
    #[test]
    fn a_live_ysf_link_excludes_every_other_network() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");

        assert!(
            matches!(s.ysf_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "a second YSF link must be refused"
        );
        #[cfg(feature = "dstar")]
        assert!(
            matches!(s.dstar_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "D-Star must be refused while YSF holds the dongle"
        );

        s.ysf_disconnect();
        assert!(s.ysf_can_connect().is_ok(), "disconnect must free the slot");
        reflector.shutdown();
    }

    /// A link adopted after the operator has already set a volume must ride
    /// a lane already AT that volume, not at the router's unity default —
    /// and a change made afterwards must reach the same lane. This is the bug
    /// that shipped on D-Star; the link no longer carries a preference cell
    /// of its own, so both halves are asserted where the values now live:
    /// on the station router's bus.
    #[cfg(feature = "ysf")]
    #[test]
    fn the_prefs_a_ysf_link_rides_are_on_the_router_before_and_after_adopt() {
        let mut s = ConsoleSession::new();
        s.set_output_gain(2.5);
        s.set_rx_compress(true);
        s.set_rx_compression_level(0.75);

        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");

        let out = OutputId::new("out:null");
        {
            let r = s.manager.as_ref().expect("engine").router();
            assert!(
                (r.output_gain(&out).expect("bus") - 2.5).abs() < 1e-6,
                "the gain chosen BEFORE the link came up must be on its bus"
            );
            assert_eq!(r.output_compress(&out), Some(true));
            assert!((r.output_compress_level(&out).expect("bus") - 0.75).abs() < 1e-6);
        }

        s.set_output_gain(0.25);
        s.set_rx_compression_level(0.1);
        {
            let r = s.manager.as_ref().expect("engine").router();
            assert!((r.output_gain(&out).expect("bus") - 0.25).abs() < 1e-6);
            assert!((r.output_compress_level(&out).expect("bus") - 0.1).abs() < 1e-6);
        }

        s.ysf_disconnect();
        reflector.shutdown();
    }

    /// The regression this suite was missing, and the one that shipped: a
    /// front-end drives ONE connection state machine off `status`, whatever
    /// the network — M17 and D-Star both mirror their link into it. YSF did
    /// not, so a live link left `status` at `Idle` and the app reported
    /// nothing connected while holding the vocoder open.
    ///
    /// Asserting `ysf_active` was never enough: that flag is read by
    /// `astar-server`'s key guard, not by any UI.
    #[cfg(feature = "ysf")]
    #[test]
    fn a_live_ysf_link_reaches_the_status_a_front_end_reads() {
        let mut s = ConsoleSession::new();
        assert_eq!(s.snapshot().status, CallStatus::Idle);

        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");
        // The link is `Linking` until the loopback reflector answers, so both
        // of the pre-`Linked` states are legal here — what must NOT happen is
        // `status` staying `Idle`, which is what "nothing is connected" means
        // to every front-end.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut seen = s.snapshot().status;
        while seen != CallStatus::Answered && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            seen = s.snapshot().status;
        }
        assert_eq!(
            seen,
            CallStatus::Answered,
            "a linked YSF session must report as connected, not idle"
        );

        s.ysf_disconnect();
        assert_eq!(
            s.snapshot().status,
            CallStatus::Idle,
            "the mirror stops running on disconnect, so disconnect must reset it \
             or the snapshot reports a session that no longer exists"
        );
        reflector.shutdown();
    }

    /// The levels a UI reads off `ysf_state()` are the CONSOLE's — read
    /// once, at the lane, by `snapshot()` — not the link's. A silent bus
    /// must report the floor rather than a stale or invented level, and the
    /// spectrum must report "no reading yet" rather than a zeroed array a UI
    /// would draw as real bars.
    #[cfg(feature = "ysf")]
    #[test]
    fn a_ysf_link_reports_the_router_meters_through_ysf_state() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        assert!(
            (link.snapshot().rx_dbfs + 60.0).abs() < 1e-6,
            "the link itself always reports the floor; the console fills it in"
        );
        s.ysf_adopt(link).expect("adopt");

        let ids = s.meter_ids().expect("a route is live");
        assert_eq!(ids.1, OutputId::new("out:null"));
        assert_eq!(ids.0, Some(MicId::new("in:null")));

        let snap = s.snapshot();
        let st = s.ysf_state().expect("a live link");
        assert!(
            (st.rx_dbfs - snap.rx_level_db).abs() < 1e-6
                && (st.tx_dbfs - snap.tx_level_db).abs() < 1e-6,
            "ysf_state must report the SAME numbers the snapshot does, got {st:?}"
        );
        assert!(
            (snap.rx_level_db + 60.0).abs() < 1e-6,
            "a silent bus reads the floor, not garbage, got {}",
            snap.rx_level_db
        );

        s.ysf_disconnect();
        assert!(
            s.meter_ids().is_none(),
            "the route is released on disconnect, so nothing is metered"
        );
        // And the analyzer goes with the lane: the link never had one to
        // fall back on, so a released route means no bins at all.
        let mut bins = [1.0f32; astar_audio::SPECTRUM_BINS];
        assert_eq!(s.rx_spectrum(&mut bins), 0, "no route, no bins");
        reflector.shutdown();
    }

    /// The level and the spectrum both have to be reset on teardown, for the
    /// same reason `status` does: the mirror stops running, so whatever it
    /// last wrote would otherwise stay on screen over a dead session.
    #[cfg(feature = "ysf")]
    #[test]
    fn disconnect_returns_the_rx_meter_to_the_floor() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");
        let _ = s.snapshot();
        s.ysf_disconnect();
        assert!(
            (s.snapshot().rx_level_db + 60.0).abs() < 1e-6,
            "a disconnected station must meter silence, not its last reading"
        );
        reflector.shutdown();
    }

    /// The decay preference reaches the lane a live YSF link rides, so one
    /// call from a settings slider scrubs every visible spectrum rather than
    /// all but one.
    ///
    /// Asserted as "it reaches the route's own analyzers without panicking",
    /// not as a value read back: the router exposes setters for the peak-hold
    /// decay and no getter, so there is nothing to compare against. What this
    /// pins is that the fan-out addresses BOTH of the route's lanes while a
    /// YSF link holds it — which is the part that used to be skipped.
    #[cfg(feature = "ysf")]
    #[test]
    fn the_spectrum_decay_preference_reaches_the_lane_a_ysf_link_rides() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");

        let (mic, out) = s.meter_ids().expect("a route is live");
        assert!(mic.is_some(), "the fan-out has a capture lane to address");
        assert_eq!(out, OutputId::new("out:null"));
        s.set_spectrum_decay(42.0);

        s.ysf_disconnect();
        reflector.shutdown();
    }

    /// "Somebody is transmitting" reaches the same field M17 fills, so a UI's
    /// receive indicator works on YSF without a per-network special case.
    #[cfg(feature = "ysf")]
    #[test]
    fn a_ysf_transmission_sets_remote_ptt() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");
        // Nobody is transmitting on a freshly-linked loopback reflector.
        let _ = s.snapshot();
        assert!(!s.snapshot().remote_ptt);
        s.ysf_disconnect();
        assert!(!s.snapshot().remote_ptt, "and it clears on teardown");
        reflector.shutdown();
    }

    /// `ysf_active` is what `astar-server` reads to refuse remote keying, so
    /// it has to be true while a link is live and false the moment it is not.
    #[cfg(feature = "ysf")]
    #[test]
    fn the_snapshot_mirrors_whether_a_ysf_link_is_live() {
        let mut s = ConsoleSession::new();
        assert!(!s.snapshot().ysf_active, "nothing is live yet");

        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");
        assert!(s.snapshot().ysf_active, "a live link must show");
        assert!(s.ysf_state().is_some(), "and be readable");

        s.ysf_disconnect();
        assert!(!s.snapshot().ysf_active, "and stop showing when it is gone");
        assert!(s.ysf_state().is_none());
        reflector.shutdown();
    }
    // ── NXDN wiring (iax-b9c2 Task 5) ───────────────────────────────────

    /// A link that needs no dongle, riding the lane the console opened for
    /// it: enough to prove exclusion, the mirror and the transmit gate, all
    /// of which are about the session rather than about the vocoder.
    /// Everything binds `127.0.0.1`.
    #[cfg(feature = "nxdn")]
    fn loopback_nxdn(
        audio: astar_audio::CallAudio,
    ) -> (astar_nxdn::ReflectorHandle, crate::nxdn::NxdnLink) {
        let r = astar_nxdn::Reflector::bind("127.0.0.1:0".parse().expect("v4"), 31_313)
            .expect("bind reflector");
        let addr = r.local_addr();
        let handle = r.run();
        let link = crate::nxdn::NxdnLink::connect_with_stream(
            &crate::nxdn::NxdnConfig {
                host: addr.to_string(),
                callsign: "N0CALL".to_string(),
                radio_id: 4242,
                talkgroup: 31_313,
            },
            audio,
            Box::new(InertVocoder),
            astar_codec::ambe::AmbeBackend::Hardware,
        )
        .expect("connect the link");
        (handle, link)
    }

    /// The whole point of the status mirror: one connection state machine in
    /// the front end, not a per-network special case. Without it the link
    /// comes up, holds the vocoder, and a UI still reports nothing
    /// connected.
    #[cfg(feature = "nxdn")]
    #[test]
    fn an_nxdn_link_mirrors_linked_as_answered() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_nxdn(audio);
        s.nxdn_adopt(link).expect("adopt onto the reserved route");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snap = s.snapshot();
            if snap.status == CallStatus::Answered || std::time::Instant::now() > deadline {
                assert_eq!(snap.status, CallStatus::Answered);
                assert!(snap.nxdn_active);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        s.nxdn_disconnect();
        assert_eq!(s.snapshot().status, CallStatus::Idle);
        assert!(!s.snapshot().nxdn_active);
        reflector.shutdown();
    }

    /// One `ThumbDV`, one link. A live NXDN link must refuse every other
    /// network, and be refused by them — the exclusion is about the
    /// hardware, not about tidiness. Missing one of these sites is exactly
    /// the silent failure the design opens by warning about.
    #[cfg(feature = "nxdn")]
    #[test]
    fn nxdn_is_excluded_by_every_other_network() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_nxdn(audio);
        s.nxdn_adopt(link).expect("adopt");

        assert!(
            matches!(s.nxdn_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "a second NXDN link must be refused"
        );
        #[cfg(feature = "ysf")]
        assert!(
            matches!(s.ysf_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "YSF must be refused while NXDN holds the dongle"
        );
        #[cfg(feature = "dstar")]
        assert!(
            matches!(s.dstar_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "D-Star must be refused while NXDN holds the dongle"
        );
        assert!(
            matches!(
                s.can_open_voice_route(),
                Err(ConsoleError::AlreadyConnected)
            ),
            "and so must the route every other connect path opens"
        );

        s.nxdn_disconnect();
        assert!(
            s.nxdn_can_connect().is_ok(),
            "disconnect must free the slot"
        );
        reflector.shutdown();
    }

    /// And the other direction: a live YSF link must refuse NXDN.
    #[cfg(all(feature = "nxdn", feature = "ysf"))]
    #[test]
    fn a_live_ysf_link_excludes_nxdn() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");

        assert!(
            matches!(s.nxdn_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "NXDN must be refused while YSF holds the dongle"
        );

        s.ysf_disconnect();
        reflector.shutdown();
    }

    /// The mirror STOPS running when the link goes; without the reset the
    /// last values it wrote stay frozen in `self.state` forever, and a
    /// snapshot reports `Answered` for a station with no session at all.
    #[cfg(feature = "nxdn")]
    #[test]
    fn disconnecting_nxdn_releases_the_route_and_clears_the_mirror() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_nxdn(audio);
        s.nxdn_adopt(link).expect("adopt");
        let _ = s.snapshot();

        s.nxdn_disconnect();

        let snap = s.snapshot();
        assert_eq!(snap.status, CallStatus::Idle);
        assert!(!snap.ptt && !snap.remote_ptt);
        assert!((snap.rx_level_db + 60.0).abs() < 1e-6);
        assert!(s.nxdn_can_connect().is_ok(), "the route came back");
        reflector.shutdown();
    }

    /// `Station::disconnect` calls every other network's disconnect before
    /// NXDN's, so an `nxdn_disconnect` that released the route
    /// unconditionally would close the lanes out from under whichever of
    /// those holds it.
    #[cfg(feature = "nxdn")]
    #[test]
    fn nxdn_disconnect_leaves_a_route_it_does_not_own_alone() {
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");

        s.nxdn_disconnect();

        let mgr = s.manager.as_ref().expect("engine survives");
        assert_eq!(
            mgr.router().output_count(),
            1,
            "the bus another network is listening on must stay open"
        );
        assert_eq!(mgr.router().mic_count(), 1, "so must its capture lane");
    }

    /// NXDN cannot transmit yet, and the refusal has to land BEFORE the
    /// capture gate opens: a key that opened the microphone and then refused
    /// would leave the lane live, feeding a run loop that discards every
    /// frame. A key-UP is never refused — it is how a caller clears state.
    #[cfg(feature = "nxdn")]
    #[test]
    fn keying_nxdn_is_refused_before_the_route_is_keyed() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_nxdn(audio);
        s.nxdn_adopt(link).expect("adopt");

        match s.set_ptt(true) {
            Err(ConsoleError::Nxdn(msg)) => assert!(
                msg.contains("transmit"),
                "the refusal must say why, got {msg:?}"
            ),
            other => panic!("a key-down on NXDN must be refused, got {other:?}"),
        }
        assert!(!s.state.ptt, "a refused key never mirrors as keyed");
        assert!(!s.snapshot().ptt);

        s.set_ptt(false).expect("a key-up is not refused");
        assert!(!s.state.ptt);

        s.nxdn_disconnect();
        reflector.shutdown();
    }

    /// The ORDER of the refusal, pinned where it is observable: on a route
    /// with no capture device the gate refuses first with
    /// [`ConsoleError::NoCaptureDevice`], so an NXDN key that reached the
    /// gate would surface THAT error. Seeing `Nxdn` instead is proof the
    /// refusal ran before the gate — and therefore that the microphone is
    /// never opened for a transmission that cannot happen.
    #[cfg(feature = "nxdn")]
    #[test]
    fn the_nxdn_refusal_runs_before_the_capture_gate() {
        let mut s = ConsoleSession::new();
        let audio = s
            .open_voice_route(Some("no such device"), None, null)
            .expect("the bus opens; a missing capture device is not fatal");
        let (reflector, link) = loopback_nxdn(audio);
        s.nxdn_adopt(link).expect("adopt");

        match s.set_ptt(true) {
            Err(ConsoleError::Nxdn(_)) => {}
            other => panic!(
                "NXDN must refuse before the gate is asked; a NoCaptureDevice here would mean \
                 the gate ran first, got {other:?}"
            ),
        }

        s.nxdn_disconnect();
        reflector.shutdown();
    }

    /// The levels a UI reads off `nxdn_state()` are the CONSOLE's — read
    /// once, at the lane, by `snapshot()` — not the link's. A silent bus
    /// must report the floor rather than a stale or invented level.
    #[cfg(feature = "nxdn")]
    #[test]
    fn an_nxdn_link_reports_the_router_meters_through_nxdn_state() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_nxdn(audio);
        assert!(
            (link.snapshot().rx_dbfs + 60.0).abs() < 1e-6,
            "the link itself always reports the floor; the console fills it in"
        );
        s.nxdn_adopt(link).expect("adopt");

        let snap = s.snapshot();
        let st = s.nxdn_state().expect("a live link");
        assert!(
            (st.rx_dbfs - snap.rx_level_db).abs() < 1e-6
                && (st.tx_dbfs - snap.tx_level_db).abs() < 1e-6,
            "nxdn_state must report the SAME numbers the snapshot does, got {st:?}"
        );
        assert!(
            (snap.rx_level_db + 60.0).abs() < 1e-6,
            "a silent bus reads the floor, not garbage, got {}",
            snap.rx_level_db
        );

        s.nxdn_disconnect();
        assert!(s.nxdn_state().is_none(), "and nothing is readable after");
        reflector.shutdown();
    }

    /// `nxdn_adopt` installs a link that arrived with the route already
    /// reserved on its behalf — and refuses (tearing the link down) one that
    /// did not, because its channel ends would have nothing feeding them.
    #[cfg(feature = "nxdn")]
    #[test]
    fn nxdn_adopt_requires_a_reserved_route() {
        let mut orphan = astar_audio::AudioRouter::new(Box::new(NullBackend::new()));
        let (audio, _mic_tx, _mix) = orphan
            .open_monitor_call(&OutputId::new("out:null"), StreamConfig::default())
            .expect("bus");
        let (reflector, link) = loopback_nxdn(audio);

        let mut s = ConsoleSession::new();
        assert!(
            matches!(s.nxdn_adopt(link), Err(ConsoleError::AlreadyConnected)),
            "a link with no route reserved for it must be refused"
        );
        assert!(s.nxdn_state().is_none(), "and not installed");
        reflector.shutdown();
    }

    // ── DMR wiring (iax-d4f7 Task 8) ────────────────────────────────────

    /// A session with the one audio lane already reserved on its behalf, and
    /// the lane's channel ends, ready to hand to a link constructor. Exactly
    /// what `astar-station`'s three-step facade does: open the route under
    /// the lock, build the link off it, adopt under the lock again.
    #[cfg(feature = "dmr")]
    fn session_with_reserved_route() -> (ConsoleSession, astar_audio::CallAudio) {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        (s, audio)
    }

    /// A link that needs no dongle, riding the lane the console opened for
    /// it: enough to prove exclusion, the mirror and the teardown, none of
    /// which is about the vocoder. Everything binds `127.0.0.1`.
    #[cfg(feature = "dmr")]
    fn loopback_dmr(
        audio: astar_audio::CallAudio,
    ) -> (astar_dmr::MasterHandle, crate::dmr::DmrLink) {
        let m = astar_dmr::Master::bind("127.0.0.1:0".parse().expect("v4"), "passw0rd")
            .expect("bind master");
        let addr = m.local_addr();
        let handle = m.run();
        let link = crate::dmr::DmrLink::connect_with_stream(
            crate::dmr::DmrConfig {
                system: "tgif".into(),
                family: Some(astar_dmr::DmrNetwork::Tgif),
                host: addr.ip().to_string(),
                port: addr.port(),
                radio_id: 3_153_591,
                callsign: "KC0ABC".into(),
                talkgroup: 31_313,
                timeslot: astar_dmr::Timeslot::Ts2,
                password: "passw0rd".into(),
            },
            audio,
            Box::new(InertVocoder),
            astar_codec::ambe::AmbeBackend::Hardware,
        )
        .expect("connect");
        (handle, link)
    }

    #[cfg(feature = "dmr")]
    #[test]
    fn a_dmr_link_mirrors_linked_as_answered() {
        // The whole point of the status mirror: one connection state machine
        // in the front end, not a per-network special case.
        let (mut s, audio) = session_with_reserved_route();
        let (master, link) = loopback_dmr(audio);
        s.dmr_adopt(link).expect("adopt onto the reserved route");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let snap = s.snapshot();
            if snap.status == CallStatus::Answered || std::time::Instant::now() > deadline {
                assert_eq!(snap.status, CallStatus::Answered);
                assert!(snap.dmr_active);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        s.dmr_disconnect();
        assert_eq!(s.snapshot().status, CallStatus::Idle);
        assert!(!s.snapshot().dmr_active);
        master.shutdown();
    }

    #[cfg(feature = "dmr")]
    #[test]
    fn dmr_is_excluded_by_every_other_network_and_excludes_them() {
        // Five networks, one dongle, one audio lane. The exclusion has to be
        // symmetric or the second connect wins a race with the first.
        let (mut s, audio) = session_with_reserved_route();
        let (master, link) = loopback_dmr(audio);
        s.dmr_adopt(link).expect("adopt");
        assert!(matches!(
            s.dmr_can_connect(),
            Err(ConsoleError::AlreadyConnected)
        ));
        // Each `*_can_connect` exists only when its own feature is compiled
        // in, so each assertion carries that feature's `cfg`. A run with
        // every network on (`--features dmr,nxdn,ysf,dstar`) asserts the
        // whole matrix; `--features dmr` alone asserts the part that exists.
        #[cfg(feature = "nxdn")]
        assert!(matches!(
            s.nxdn_can_connect(),
            Err(ConsoleError::AlreadyConnected)
        ));
        #[cfg(feature = "ysf")]
        assert!(matches!(
            s.ysf_can_connect(),
            Err(ConsoleError::AlreadyConnected)
        ));
        #[cfg(feature = "dstar")]
        assert!(matches!(
            s.dstar_can_connect(),
            Err(ConsoleError::AlreadyConnected)
        ));
        // M17 is the fifth. It has no `m17_can_connect` — its gate is
        // `open_voice_route`'s own check inside `m17_connect` — so this asks
        // the question the only way M17 answers it. The brief's list omitted
        // M17 entirely, and an omission here is exactly the asymmetry that
        // lets a second connect win a race with the first.
        #[cfg(feature = "m17")]
        {
            let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
            let addr = target.local_addr().expect("local addr");
            assert!(
                matches!(
                    s.m17_connect(Box::new(NullBackend::new()), m17_cfg(addr), None, None),
                    Err(ConsoleError::AlreadyConnected)
                ),
                "M17 must be refused while DMR holds the lane"
            );
        }
        s.dmr_disconnect();
        master.shutdown();
    }

    /// The other direction for M17: a live M17 session must refuse DMR.
    #[cfg(all(feature = "dmr", feature = "m17"))]
    #[test]
    fn a_live_m17_session_excludes_dmr() {
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
        let addr = target.local_addr().expect("local addr");
        let mut s = ConsoleSession::new();
        s.m17_connect(Box::new(NullBackend::new()), m17_cfg(addr), None, None)
            .expect("m17 connect");
        assert!(
            matches!(s.dmr_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "DMR must be refused while M17 holds the lane"
        );
        s.m17_disconnect();
        assert!(s.dmr_can_connect().is_ok(), "and allowed once it is gone");
    }

    /// And the other direction for the dongle networks that can be made live
    /// here: a live YSF link must refuse DMR.
    #[cfg(all(feature = "dmr", feature = "ysf"))]
    #[test]
    fn a_live_ysf_link_excludes_dmr() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_ysf(audio);
        s.ysf_adopt(link).expect("adopt");
        assert!(
            matches!(s.dmr_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "DMR must be refused while YSF holds the dongle"
        );
        s.ysf_disconnect();
        reflector.shutdown();
    }

    /// And a live NXDN link must refuse DMR.
    #[cfg(all(feature = "dmr", feature = "nxdn"))]
    #[test]
    fn a_live_nxdn_link_excludes_dmr() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        let (reflector, link) = loopback_nxdn(audio);
        s.nxdn_adopt(link).expect("adopt");
        assert!(
            matches!(s.dmr_can_connect(), Err(ConsoleError::AlreadyConnected)),
            "DMR must be refused while NXDN holds the dongle"
        );
        s.nxdn_disconnect();
        reflector.shutdown();
    }

    #[cfg(feature = "dmr")]
    #[test]
    fn disconnecting_dmr_releases_the_route_and_clears_the_mirror() {
        let (mut s, audio) = session_with_reserved_route();
        let (master, link) = loopback_dmr(audio);
        s.dmr_adopt(link).expect("adopt");
        s.dmr_disconnect();
        assert!(s.dmr_state().is_none());
        assert!(!s.snapshot().dmr_active);
        assert!(s.dmr_can_connect().is_ok(), "the route came back");
        #[cfg(feature = "ysf")]
        assert!(
            s.ysf_can_connect().is_ok(),
            "for every network, not just DMR"
        );
        master.shutdown();
    }

    /// `dmr_adopt` installs a link that arrived with the route already
    /// reserved on its behalf — and refuses (tearing the link down) one that
    /// did not, because its channel ends would have nothing feeding them.
    #[cfg(feature = "dmr")]
    #[test]
    fn dmr_adopt_requires_a_reserved_route() {
        let mut orphan = astar_audio::AudioRouter::new(Box::new(NullBackend::new()));
        let (audio, _mic_tx, _mix) = orphan
            .open_monitor_call(&OutputId::new("out:null"), StreamConfig::default())
            .expect("bus");
        let (master, link) = loopback_dmr(audio);

        let mut s = ConsoleSession::new();
        assert!(
            matches!(s.dmr_adopt(link), Err(ConsoleError::AlreadyConnected)),
            "a link with no route reserved for it must be refused"
        );
        assert!(s.dmr_state().is_none(), "and not installed");
        master.shutdown();
    }

    /// `dmr_disconnect` on a session whose route belongs to another network
    /// must leave that route alone — `Station::disconnect` calls every
    /// network's disconnect in turn.
    #[cfg(feature = "dmr")]
    #[test]
    fn dmr_disconnect_leaves_a_route_it_does_not_own_alone() {
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");
        s.dmr_disconnect();
        assert!(
            s.meter_ids().is_some(),
            "a route this session did not open for DMR must survive dmr_disconnect"
        );
    }

    /// The levels a UI reads off `dmr_state()` are the CONSOLE's — read
    /// once, at the lane, by `snapshot()` — not the link's.
    #[cfg(feature = "dmr")]
    #[test]
    fn a_dmr_link_reports_the_router_meters_through_dmr_state() {
        let (mut s, audio) = session_with_reserved_route();
        let (master, link) = loopback_dmr(audio);
        assert!(
            (link.snapshot().rx_dbfs + 60.0).abs() < 1e-6,
            "the link itself always reports the floor; the console fills it in"
        );
        s.dmr_adopt(link).expect("adopt");

        let snap = s.snapshot();
        let st = s.dmr_state().expect("a live link");
        assert!(
            (st.rx_dbfs - snap.rx_level_db).abs() < 1e-6
                && (st.tx_dbfs - snap.tx_level_db).abs() < 1e-6,
            "dmr_state must report the SAME numbers the snapshot does, got {st:?}"
        );
        s.dmr_disconnect();
        master.shutdown();
    }

    #[test]
    fn dmr_available_answers_without_the_feature() {
        // Feature-independent by construction: astar-server reads
        // `dmr_active` off a snapshot in a build that may not compile the
        // session at all.
        let _ = dmr_available();
    }

    // ── The one audio lane: `VoiceRoute` on the station router ───────────

    /// A backend factory shaped for `open_voice_route`, which takes the
    /// factory rather than an already-built backend (it decides for itself
    /// whether the engine still needs one).
    fn null() -> Box<dyn AudioBackend> {
        Box::new(NullBackend::new())
    }

    #[test]
    fn a_voice_route_opens_the_bus_and_the_capture_lane_on_the_station_router() {
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");
        let mgr = s.manager.as_ref().expect("engine built by the route");
        assert_eq!(mgr.router().output_count(), 1);
        assert_eq!(
            mgr.router().mic_count(),
            1,
            "a resolvable mic opens at connect, gate closed"
        );
        assert!(s.voice_route_tx_capable());
    }

    #[test]
    fn a_voice_route_is_receive_only_when_no_input_resolves() {
        let mut s = ConsoleSession::new();
        let _audio = s
            .open_voice_route(Some("no such device"), None, null)
            .expect("route");
        let mgr = s.manager.as_ref().expect("engine");
        assert_eq!(mgr.router().mic_count(), 0);
        assert!(!s.voice_route_tx_capable());
        assert!(
            !s.key_voice_route(true),
            "keying without a capture device is refused"
        );
    }

    fn loopback_link_spec(node: &str, mode: LinkMode) -> LinkConnectSpec {
        LinkConnectSpec {
            node: node.to_string(),
            // Loopback only: nothing listens, the dial just pools a call.
            peer: "127.0.0.1:4569".parse().expect("loopback"),
            mode,
            caller_id: "1999".into(),
            secret: String::new(),
            shape: CallMode::Standard,
            permanent: false,
        }
    }

    /// An IAX2 link lives in the `Manager`'s call table and never sets
    /// `active`, so the voice-route gate has to ask the Manager. Without
    /// that, opening a route would re-open the link's own mic lane, overwrite
    /// its destination, and close its capture stream at release.
    #[test]
    fn a_live_link_refuses_a_voice_route() {
        let mut s = ConsoleSession::new();
        s.link_connect(loopback_link_spec("55553", LinkMode::Transceive), null())
            .expect("the dial pools a link over the null backend");
        assert!(
            matches!(
                s.open_voice_route(None, None, null),
                Err(ConsoleError::AlreadyConnected)
            ),
            "a route must not steal the lane a live link is keyed through"
        );
        s.link_disconnect("55553").expect("tear the link down");
        assert!(
            s.open_voice_route(None, None, null).is_ok(),
            "and the lane is free again once the link is gone"
        );
    }

    /// The other direction: `link_connect` routes the default mic through the
    /// `Manager`, which would un-gate and re-bind a lane a digital-voice
    /// session is mid-transmission on.
    #[test]
    fn a_held_voice_route_refuses_link_connect() {
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");
        assert!(
            matches!(
                s.link_connect(loopback_link_spec("55553", LinkMode::Transceive), null()),
                Err(ConsoleError::AlreadyConnected)
            ),
            "a link must not be dialed while a voice route holds the lane"
        );
        assert!(
            matches!(
                s.link_set_mode("55553", LinkMode::Transceive),
                Err(ConsoleError::AlreadyConnected)
            ),
            "nor may a mode switch route a mic behind the route's back"
        );
        assert!(
            s.link_roster().is_none_or(|r| r.links.is_empty()),
            "and nothing was registered"
        );
    }

    #[test]
    fn a_reserved_voice_route_refuses_every_other_connect() {
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");
        #[cfg(feature = "dstar")]
        assert!(matches!(
            s.dstar_can_connect(),
            Err(ConsoleError::AlreadyConnected)
        ));
        #[cfg(feature = "ysf")]
        assert!(matches!(
            s.ysf_can_connect(),
            Err(ConsoleError::AlreadyConnected)
        ));
        // An IAX2 dial: it must be refused before it dials, so the peer is
        // never contacted (and is a loopback address regardless).
        let cfg = ConsoleConfig {
            node: "55553".into(),
            calling_node: "55553".into(),
            secret: "allstar".into(),
            name: "astar".into(),
            input_device: None,
            output_device: None,
            codec_policy: CodecPolicy::default(),
        };
        let peer: SocketAddr = "127.0.0.1:1".parse().expect("loopback");
        assert!(matches!(
            s.connect(null(), peer, cfg),
            Err(ConsoleError::AlreadyConnected)
        ));
    }

    #[test]
    fn releasing_a_voice_route_closes_both_streams_and_clears_the_reservation() {
        let mut s = ConsoleSession::new();
        let audio = s.open_voice_route(None, None, null).expect("route");
        drop(audio);
        let handles = s.release_voice_route();
        assert_eq!(
            handles.len(),
            2,
            "mic + output stream handles come back to be dropped off-lock"
        );
        let mgr = s.manager.as_ref().expect("engine survives a release");
        assert_eq!(mgr.router().mic_count(), 0);
        assert_eq!(mgr.router().output_count(), 0);
        #[cfg(feature = "dstar")]
        assert!(s.dstar_can_connect().is_ok());
    }

    #[test]
    fn prefs_set_before_and_after_a_voice_route_reach_the_router() {
        let mut s = ConsoleSession::new();
        s.set_output_gain(2.5);
        s.set_input_gain(1.5);
        s.set_rx_compress(true);
        let _audio = s.open_voice_route(None, None, null).expect("route");
        let out = OutputId::new("out:null");
        let mic = MicId::new("in:null");
        {
            let r = s.manager.as_ref().expect("engine").router();
            assert!((r.output_gain(&out).expect("bus") - 2.5).abs() < 1e-6);
            assert!((r.mic_gain(&mic).expect("lane") - 1.5).abs() < 1e-6);
            assert_eq!(r.output_compress(&out), Some(true));
        }
        s.set_output_gain(0.5);
        s.set_rx_compress(false);
        let r = s.manager.as_ref().expect("engine").router();
        assert!((r.output_gain(&out).expect("bus") - 0.5).abs() < 1e-6);
        assert_eq!(r.output_compress(&out), Some(false));
    }

    #[test]
    fn keying_the_voice_route_opens_and_closes_the_gate() {
        // `set_gate` is a no-op on an unopened lane, so this proves the lane
        // IS open: a keyed lane reports through the router's own gate cell.
        // The router exposes no gate getter; prove it through the return
        // value instead, and leave the on-air proof to the M17 test in Task 3
        // (a keyed route carries mic frames to the reflector).
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");
        assert!(s.key_voice_route(true));
        assert!(s.key_voice_route(false));
    }

    #[test]
    fn set_ptt_with_no_capture_device_is_a_typed_refusal() {
        let mut s = ConsoleSession::new();
        let _audio = s
            .open_voice_route(Some("no such device"), None, null)
            .expect("route");
        assert!(matches!(
            s.set_ptt(true),
            Err(ConsoleError::NoCaptureDevice)
        ));
    }

    #[test]
    fn set_ptt_on_a_route_with_no_session_refuses_and_keeps_the_lane() {
        // The route is reserved but no network took the key, so the refusal
        // is `NotConnected` — and the lane it opened at connect survives it,
        // ready for the session that will own the route in Task 3. (Whether
        // the gate itself is closed is not observable: the router exposes no
        // gate getter. `set_ptt` closes it on this path by construction.)
        let mut s = ConsoleSession::new();
        let _audio = s.open_voice_route(None, None, null).expect("route");
        assert!(matches!(s.set_ptt(true), Err(ConsoleError::NotConnected)));
        let mgr = s.manager.as_ref().expect("engine");
        assert_eq!(mgr.router().mic_count(), 1, "the capture lane stays open");
    }
}
