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

use astar_audio::{AudioBackend, AudioError, Direction, MicId, MicProfile, OutputId};
use astar_iax::{
    BridgeConfig, Call, CallEvent, CallId, CallMode, CodecPolicy, DialSpec, IncomingCall,
    IncomingCallEvent, IncomingCallListener, IncomingCallPolicy, KnownNodes, LinkEvent, LinkMode,
    LinkRoster, LinkSpec, LinkTransport, Manager, RegisterOptions, Registrar, Registration,
    RegistrationEvent, WgLinkConfig, WgStackStatus,
};
use astar_iax_core::session::auth::Secret;

#[cfg(feature = "dstar")]
use crate::dstar::{DstarConfig, DstarSession, DstarSnapshotState};
#[cfg(feature = "m17")]
use crate::m17::{M17Config, M17Prefs, M17Session};
use crate::metering::Gain;
use crate::state::{CallStatus, ConsoleState};
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
    /// TX trim 0.0..=2.0 (f32 bits, default 1.0 = unity): the always-on final
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
    /// (`UlawOnly`, 8 kHz), byte-identical to pre-iax-4348 sessions. Set from
    /// [`ConsoleConfig::codec_policy`] in [`Self::connect`] BEFORE
    /// [`Self::ensure_engine`] builds the `Manager`; has no effect once the
    /// `Manager` already exists (the pipeline rate cannot change live).
    station_policy: CodecPolicy,
    /// A `WireGuard` link transport selected before the engine exists
    /// (iax-5bbd): the secret-free config plus the owned key resolver, applied
    /// exactly once when the engine is first built. `None` = plain UDP (the
    /// library default — byte-identical to pre-iax-5bbd sessions). Retained on
    /// a failed apply so a retried connect can never silently fall back to
    /// plain UDP; an explicit [`Self::set_link_transport`] with
    /// [`LinkTransport::Udp`] clears it.
    pending_wg: Option<(WgLinkConfig, Box<LinkKeyResolver>)>,
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
            tx_trim: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            rx_compress: Arc::new(AtomicBool::new(false)),
            rx_compress_level: Arc::new(AtomicU32::new(0.90_f32.to_bits())),
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
    // `profile` is only ever moved-from (vs. cloned/borrowed) in the
    // `#[cfg(feature = "m17")]` branch below, so a build with `dstar` but
    // without `m17` (iax-a9d4 Task 7: `astar-cli --features dstar` pulls
    // in this crate's own `dstar` feature with `m17` off, unlike every
    // previous caller, which always had `m17` on too) sees `profile` used
    // only by reference and would otherwise suggest `Option<&MicProfile>`
    // here — but the signature must stay identical whether or not `m17` is
    // compiled in (same convention as `DstarSession::connect`'s own
    // `#[allow(clippy::needless_pass_by_value)]`).
    #[allow(clippy::needless_pass_by_value)]
    pub fn set_calibrated(&self, profile: Option<MicProfile>) {
        self.calibrated.lock().unwrap().clone_from(&profile);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_mic_profile(id, profile.clone());
        }
        // iax-f2b8-fix Fix 4: forward every standing pref to a live M17
        // session too — before this, M17Session's own AudioRouter never
        // heard about ANY of these (only the IAX2 Manager did), so e.g. the
        // RX volume slider had no effect on an M17 link.
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_calibrated(profile);
        }
    }

    /// Toggle capture noise-reduction on the next/current network call.
    pub fn set_denoise(&self, on: bool) {
        self.denoise.store(on, Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_denoise(id, on);
        }
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_denoise(on);
        }
    }

    /// Toggle capture compression on the next/current network call.
    pub fn set_compress(&self, on: bool) {
        self.compress.store(on, Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_compress(id, on);
        }
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_compress(on);
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
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_compression_level(level);
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
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_rx_compress(on);
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
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_rx_compression_level(level);
        }
    }

    /// Set the TX trim (0.0..=2.0, clamped; 1.0 = unity) on the next/current
    /// network call: the always-on final TX gain stage after the compressor
    /// (iax-750a). Persisted across reconnects; takes effect immediately.
    pub fn set_tx_trim(&self, g: f32) {
        let g = g.clamp(0.0, 2.0);
        self.tx_trim.store(g.to_bits(), Ordering::Relaxed);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_tx_trim(id, g);
        }
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_tx_trim(g);
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
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_vox_preroll_ms(ms);
        }
    }

    /// Set the live spectrum peak-hold decay (dB/SECOND, clamped, iax-8616) on
    /// the active network call's TX + RX analyzers. No-op if no call is active
    /// (applies only to currently-live analyzers; iax-8616).
    pub fn set_spectrum_decay(&self, db_per_sec: f32) {
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_spectrum_decay(id, db_per_sec);
        }
        // iax-f2b8-fix Fix 6: forward to a live M17 session too — mirrors
        // Fix 4's pref-setter forwarding, but "live-only" (no persisted
        // cell), matching this setter's own no-op-when-idle contract above.
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_spectrum_decay(db_per_sec);
        }
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
        if self.active.is_some() || self.m17_is_active() || self.dstar_is_active() {
            return Err(ConsoleError::AlreadyConnected);
        }

        // Snapshot the session's standing prefs BEFORE calling ensure_engine so
        // we don't hold a mutable borrow of `self` while also reading from it.
        let input_gain = self.input_gain.get();
        let output_gain = self.output_gain.get();
        let denoise = self.denoise.load(Ordering::Relaxed);
        let compress = self.compress.load(Ordering::Relaxed);
        let compress_level = f32::from_bits(self.compress_level.load(Ordering::Relaxed));
        let tx_trim = f32::from_bits(self.tx_trim.load(Ordering::Relaxed));
        let rx_compress = self.rx_compress.load(Ordering::Relaxed);
        let rx_compress_level = f32::from_bits(self.rx_compress_level.load(Ordering::Relaxed));
        let vox_preroll_ms = self.vox_preroll_ms.load(Ordering::Relaxed);
        let calibrated = self.calibrated.lock().unwrap().clone();

        // Pin the station pipeline rate to this call's codec policy (iax-4348)
        // BEFORE the Manager is built; a no-op if the Manager already exists
        // (its pipeline rate cannot change live — set on the first `connect`).
        self.station_policy = cfg.codec_policy;

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
        // `prefer_slin16` becomes a 16 kHz engine. A no-op if the Manager
        // already exists (its pipeline rate cannot change live).
        self.station_policy = policy.codec_policy;

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
                    if self.active.is_none() && !self.m17_is_active() && !self.dstar_is_active() {
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
        if self.m17_is_active() || self.dstar_is_active() {
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
        if self.m17_is_active() || self.dstar_is_active() {
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
        // iax-2f6b: a live D-Star session now transmits — dispatch exactly
        // like the M17 branch above, INCLUDING the `self.state.ptt` mirror.
        // D-Star's richer state (talker/slow text/link/backend) is read
        // through `dstar_state()`, but `ptt` is not D-Star-specific: it is
        // the shared `ConsoleState` field every `Station::snapshot()`
        // consumer reads for "is this station transmitting", and a UI — or
        // any PTT source that reconciles against the snapshot — must never
        // see `false` while a D-Star transmission is on the air. `snapshot()`
        // then mirrors the run loop's ACTUALLY-applied state back on every
        // poll, so a refused key-down or a forced unkey (link lost, time-out
        // timer) shows up here too.
        #[cfg(feature = "dstar")]
        if let Some(session) = self.dstar.as_mut() {
            session.set_ptt(on);
            self.state.ptt = on;
            self.tracer
                .note(if on { "LocalKey" } else { "LocalUnkey" }, String::new());
            return Ok(());
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
            self.state.status = CallStatus::Idle;
            self.state.ptt = false;
            self.state.remote_ptt = false;
            self.state.rtt_ms = None;
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            return Ok(());
        }
        // D-Star never mirrors into `self.state` (see `dstar`'s field docs),
        // so there is nothing to reset here beyond clearing the session
        // itself.
        #[cfg(feature = "dstar")]
        if let Some(session) = self.dstar.take() {
            session.disconnect();
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
    /// factory. [`M17Session::connect`] itself wants a `&dyn Fn() -> Box<dyn
    /// AudioBackend>` (called exactly once); this wraps `backend` in a
    /// `RefCell`-backed adapter satisfying that shape without an extra trait
    /// object allocation.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] per above (also refused while a
    /// D-Star session is live — iax-a9d4 Task 6, the same mutual exclusion
    /// as an IAX2 call); otherwise whatever [`M17Session::connect`] returns
    /// (`Device` for an invalid callsign/missing codec, `Audio` for a
    /// device/stream failure, `Resolve` for a DNS/bind failure).
    #[cfg(feature = "m17")]
    pub fn m17_connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        cfg: M17Config,
    ) -> Result<(), ConsoleError> {
        if self.active.is_some() || self.m17.is_some() || self.dstar_is_active() {
            return Err(ConsoleError::AlreadyConnected);
        }
        // iax-f2b8-fix Fix 4: mirror the standing-pref re-push `Self::connect`
        // does for an IAX2 dial (originally 8 prefs; iax-a4e7 PHASE 1 adds RX
        // compression, a 10-pref re-push) — otherwise a fresh M17 link
        // silently reverted to the router's bare defaults (unity gain, DSP
        // off) no matter what the operator had already dialed in.
        let prefs = M17Prefs {
            input_gain: self.input_gain.get(),
            output_gain: self.output_gain.get(),
            denoise: self.denoise.load(Ordering::Relaxed),
            compress: self.compress.load(Ordering::Relaxed),
            compress_level: f32::from_bits(self.compress_level.load(Ordering::Relaxed)),
            tx_trim: f32::from_bits(self.tx_trim.load(Ordering::Relaxed)),
            rx_compress: self.rx_compress.load(Ordering::Relaxed),
            rx_compress_level: f32::from_bits(self.rx_compress_level.load(Ordering::Relaxed)),
            vox_preroll_ms: self.vox_preroll_ms.load(Ordering::Relaxed),
            calibrated: self.calibrated.lock().unwrap().clone(),
        };
        let slot = std::cell::RefCell::new(Some(backend));
        let make_backend = move || -> Box<dyn AudioBackend> {
            slot.borrow_mut()
                .take()
                .expect("m17 backend factory called exactly once")
        };
        let session = M17Session::connect(cfg, prefs, &make_backend)?;
        self.m17 = Some(session);
        Ok(())
    }

    /// Disconnect the live M17 session, if any. No-op when none is active.
    /// This is the ONLY thing that clears a `Hangup` status left by a failed
    /// link back to `Idle` — see [`Self::m17`]'s docs.
    #[cfg(feature = "m17")]
    pub fn m17_disconnect(&mut self) {
        if let Some(session) = self.m17.take() {
            session.disconnect();
        }
        self.state.status = CallStatus::Idle;
        self.state.ptt = false;
        self.state.remote_ptt = false;
        self.state.tx_level_db = -60.0;
        self.state.rx_level_db = -60.0;
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

    /// Connect to a D-Star `DExtra` reflector, full-transceive (iax-a9d4
    /// Task 6 built RX; iax-2f6b added TX). Mutually exclusive with an
    /// active IAX2 call AND a live M17 session: refuses
    /// [`ConsoleError::AlreadyConnected`] while either is `Some` (mirrors
    /// [`Self::m17_connect`]'s own guard, which now also refuses while a
    /// D-Star session is live — see its docs).
    ///
    /// `backend` mirrors [`Self::m17_connect`]'s contract of taking an
    /// already-constructed backend.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] per above; otherwise whatever
    /// [`DstarSession::connect`] returns (`Device` for an invalid
    /// callsign or unavailable AMBE backend, `Audio` for a device/stream
    /// failure, `Resolve` for a DNS/bind failure).
    #[cfg(feature = "dstar")]
    pub fn dstar_connect(
        &mut self,
        backend: Box<dyn AudioBackend>,
        cfg: DstarConfig,
    ) -> Result<(), ConsoleError> {
        self.dstar_can_connect()?;
        let slot = std::cell::RefCell::new(Some(backend));
        let make_backend = move || -> Box<dyn AudioBackend> {
            slot.borrow_mut()
                .take()
                .expect("dstar backend factory called exactly once")
        };
        let session = DstarSession::connect(cfg, &make_backend)?;
        self.dstar_adopt(session)
    }

    /// The mutual-exclusion guard [`Self::dstar_connect`] applies, on its
    /// own — so a caller that wants to build the [`DstarSession`] OUTSIDE
    /// this session's mutex (see [`Self::dstar_adopt`]) can refuse early,
    /// cheaply, without having opened a `ThumbDV` first.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] while an IAX2 call, an M17 session
    /// or a D-Star session is live.
    #[cfg(feature = "dstar")]
    pub fn dstar_can_connect(&self) -> Result<(), ConsoleError> {
        if self.active.is_some() || self.m17_is_active() || self.dstar.is_some() {
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
    /// On refusal the rejected session is disconnected here rather than
    /// handed back: it has already bound a socket, opened an output stream
    /// and taken the dongle, and leaking any of those (especially the
    /// dongle, which only one process may hold) would be worse than the
    /// error the caller is about to see.
    ///
    /// # Errors
    /// [`ConsoleError::AlreadyConnected`] per [`Self::dstar_can_connect`].
    #[cfg(feature = "dstar")]
    pub fn dstar_adopt(&mut self, session: DstarSession) -> Result<(), ConsoleError> {
        if let Err(e) = self.dstar_can_connect() {
            session.disconnect();
            return Err(e);
        }
        self.dstar = Some(session);
        Ok(())
    }

    /// Disconnect the live D-Star session, if any (iax-a9d4 Task 6). No-op
    /// when none is active.
    #[cfg(feature = "dstar")]
    pub fn dstar_disconnect(&mut self) {
        if let Some(session) = self.dstar.take() {
            session.disconnect();
        }
        // Reset every field `snapshot()`'s D-Star branch mirrors, exactly as
        // `m17_disconnect` does for its own (iax-4c8e).
        //
        // Without this the mirror simply stops running once `self.dstar` is
        // `None`, leaving the LAST values it wrote frozen in `self.state`
        // forever. Keying, then disconnecting, would leave a snapshot
        // reporting `ptt: true` for a station that no longer has a session at
        // all — a UI would show a transmitting station indefinitely.
        //
        // `input_level_db` is reset here but not in `m17_disconnect`: D-Star
        // is the only path that mirrors the continuous mic meter, so it is
        // the only one that can leave it stale.
        self.state.status = CallStatus::Idle;
        self.state.ptt = false;
        self.state.tx_level_db = -60.0;
        self.state.rx_level_db = -60.0;
        self.state.input_level_db = -60.0;
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
    #[cfg(feature = "dstar")]
    #[must_use]
    pub fn dstar_state(&self) -> Option<DstarSnapshotState> {
        self.dstar.as_ref().map(DstarSession::state)
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

    /// Set the input (TX/mic) gain multiplier. `value` is clamped to `[0.0, 2.0]`;
    /// `NaN` is treated as unity (1.0). Takes `&self` (atomic write) so it can be
    /// called from a shared reference — no `NotConnected` error, gain is a
    /// standing preference that persists across calls.
    pub fn set_input_gain(&self, value: f32) {
        let clamped = if value.is_nan() {
            1.0
        } else {
            value.clamp(0.0, 2.0)
        };
        self.input_gain.set(clamped);
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            mgr.set_input_gain(id, clamped);
        }
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_mic_gain(clamped);
        }
    }

    /// Set the output (RX/speaker) gain multiplier. `value` is clamped to
    /// `[0.0, 4.0]` (iax-a4e7: 100%-400% headroom so a quiet station on a
    /// mixed net can be boosted, not just the input side's `[0.0, 2.0]`);
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
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            m17.set_output_gain(clamped);
        }
    }

    /// Copy the live-call TX spectrum (iax-2b09) of the active network call into
    /// `out` — the SAME log-binned, peak-held dBFS values the mic monitor
    /// produces, tapped from the post-DSP, pre-encode TX PCM. Returns the number
    /// of bins written (`0` if no active call / unrouted mic). A pure observer.
    ///
    /// Also reads the M17 router's TX analyzer while an M17 session is live
    /// (iax-f2b8-fix Fix 6) — `self.active`/`self.m17` are mutually
    /// exclusive, so this never has to choose between the two.
    #[must_use]
    pub fn tx_spectrum(&self, out: &mut [f32]) -> usize {
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            return mgr.tx_spectrum(id, out).unwrap_or(0);
        }
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            return m17.tx_spectrum(out);
        }
        0
    }

    /// Copy the live-call RX spectrum (iax-2b09) of the active network call into
    /// `out` — the SAME log-binned, peak-held dBFS values the mic monitor
    /// produces, tapped from the post-mix decoded RX PCM. Returns the number of
    /// bins written (`0` if no active call). A pure observer.
    ///
    /// Also reads the M17 router's RX analyzer while an M17 session is live
    /// (iax-f2b8-fix Fix 6); see [`Self::tx_spectrum`]'s doc.
    #[must_use]
    pub fn rx_spectrum(&self, out: &mut [f32]) -> usize {
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            return mgr.rx_spectrum(id, out).unwrap_or(0);
        }
        #[cfg(feature = "m17")]
        if let Some(m17) = self.m17.as_ref() {
            return m17.rx_spectrum(out);
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
    /// [`ConsoleError::Link`] on a dial/link failure; [`ConsoleError::Device`]
    /// if no default output (or, for Transceive, input) device exists.
    pub fn link_connect(
        &mut self,
        spec: LinkConnectSpec,
        backend: Box<dyn AudioBackend>,
    ) -> Result<u64, ConsoleError> {
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
    /// [`ConsoleError::Link`] if no link is registered for `node` (or no engine).
    pub fn link_set_mode(&mut self, node: &str, mode: LinkMode) -> Result<(), ConsoleError> {
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
        if let (Some(id), Some(mgr)) = (self.active, self.manager.as_ref()) {
            self.state.tx_level_db = mgr.tx_dbfs(id).unwrap_or(-60.0);
            self.state.rx_level_db = mgr.rx_dbfs(id).unwrap_or(-60.0);
            self.state.input_level_db = mgr.input_dbfs(id).unwrap_or(-60.0);
            self.state.rtt_ms = mgr
                .rtt(id)
                .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
            // TX health counters (iax-9e55): cumulative ts-ladder re-anchors and
            // cpal capture overruns on the active call / its routed mic.
            self.state.tx_reanchors = mgr.tx_reanchors(id).unwrap_or(0);
            self.state.tx_capture_overruns = mgr.tx_capture_overruns(id).unwrap_or(0);
        } else {
            self.state.tx_level_db = -60.0;
            self.state.rx_level_db = -60.0;
            self.state.input_level_db = -60.0;
            self.state.rtt_ms = None;
            self.state.tx_reanchors = 0;
            self.state.tx_capture_overruns = 0;
        }
        // Populate the full concurrent-call list (iax-a1fb P5). Secret-free:
        // CallSnapshot fields are node ids, device names, and health counters only.
        self.state.calls = self
            .manager
            .as_ref()
            .map(|m| m.snapshot().calls)
            .unwrap_or_default();
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
            self.state.tx_level_db = st.tx_dbfs;
            self.state.rx_level_db = st.rx_dbfs;
            // iax-f2b8-fix Fix 6: mirror the M17 router's continuous mic
            // input meter, same as tx/rx above — so the input meter (and any
            // future VOX edge) reads correctly during an M17 call instead of
            // sitting at the IAX2-only -60 floor `else` branch below leaves
            // it at whenever no IAX2 call is active.
            self.state.input_level_db = st.input_dbfs;
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
        // every network are mirrored — is this station transmitting, the
        // three level meters a UI's meters/VOX read, and (iax-4c8e) the
        // call status. Everything D-Star-shaped (talker, slow text, vocoder
        // backend) stays behind `dstar_state()`.
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
            self.state.tx_level_db = st.tx_dbfs;
            self.state.rx_level_db = st.rx_dbfs;
            self.state.input_level_db = st.input_dbfs;
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
    #[cfg(not(feature = "dstar"))]
    {
        false
    }
}

/// How long [`dstar_available`]'s port enumeration is reused before being
/// recomputed. Long enough that a UI polling on a 100 ms tick scans ~twice a
/// second; short enough that plugging the dongle in shows up promptly.
#[cfg(feature = "dstar")]
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
    fn set_input_gain_clamps_to_0_to_2() {
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
            (s.input_gain() - 2.0).abs() < 1e-6,
            "above 2 clamped to 2.0"
        );
    }

    #[test]
    fn set_output_gain_clamps_to_0_to_4() {
        // iax-a4e7: output gain's ceiling is 4.0 (400%), double the input
        // side's 2.0 — RX amplification headroom for a quiet station.
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

    // --- iax-f2b8-fix Fix 4: standing prefs reach a live M17 session too ----
    //
    // `session.m17` is a private field of THIS module, so this same-module
    // unit test (rather than the crate's external `tests/m17_session.rs`
    // integration file) can read `M17Session::state()` directly through it —
    // the smallest test-visible seam that proves `ConsoleSession`'s own
    // setters (not just `M17Session`'s passthroughs, already covered in
    // `tests/m17_session.rs`) actually reach the M17 router, both BEFORE and
    // AFTER `m17_connect`. Only needs a UDP target `M17Session::connect` can
    // bind/resolve against — link state is irrelevant to whether a pref
    // reaches the router, so no scripted reflector is needed here.
    #[cfg(feature = "m17")]
    #[test]
    fn console_session_prefs_reach_the_m17_router_before_and_after_connect() {
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind silent target");
        let addr = target.local_addr().expect("local addr");

        let mut session = ConsoleSession::new();
        // BEFORE m17_connect: standing prefs set on plain idle ConsoleSession.
        session.set_output_gain(0.4);
        session.set_input_gain(1.6);
        // iax-a4e7 PHASE 1: RX compression is an output-side pref too, so it
        // must reach the M17 router the same way output_gain does.
        session.set_rx_compress(true);
        session.set_rx_compression_level(0.65);

        session
            .m17_connect(
                Box::new(NullBackend::new()),
                M17Config {
                    host: addr.ip().to_string(),
                    port: addr.port(),
                    module: b'A',
                    callsign: "N0CALL".to_string(),
                    input: None,
                    output: None,
                    codec_dirs: Vec::new(),
                    keepalive_timeout: Duration::from_secs(30),
                },
            )
            .expect("m17 connect");

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let applied = |session: &ConsoleSession| {
            session
                .m17
                .as_ref()
                .expect("m17 session must be up")
                .state()
        };
        let mut st = applied(&session);
        while std::time::Instant::now() < deadline && (st.applied_output_gain - 0.4).abs() >= 0.01 {
            std::thread::sleep(Duration::from_millis(10));
            st = applied(&session);
        }
        assert!(
            (st.applied_output_gain - 0.4).abs() < 0.01,
            "output gain set BEFORE m17_connect must reach the M17 router at connect time, got {}",
            st.applied_output_gain
        );
        assert!(
            (st.applied_mic_gain - 1.6).abs() < 0.01,
            "input gain set BEFORE m17_connect must reach the M17 router at connect time, got {}",
            st.applied_mic_gain
        );
        assert!(
            st.applied_rx_compress,
            "rx compression set BEFORE m17_connect must reach the M17 router at connect time"
        );
        assert!(
            (st.applied_rx_compress_level - 0.65).abs() < 0.01,
            "rx compression level set BEFORE m17_connect must reach the M17 router at connect time, got {}",
            st.applied_rx_compress_level
        );

        // AFTER m17_connect: a live ConsoleSession setter must ALSO forward
        // to the now-active M17 session (Fix 4(b)) — before the fix,
        // set_output_gain only ever reached the IAX2 Manager. Also proves
        // (iax-a4e7) the M17 bus accepts the new 4.0 ceiling, not just the
        // old 2.0 one.
        session.set_output_gain(4.0);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut st = applied(&session);
        while std::time::Instant::now() < deadline && (st.applied_output_gain - 4.0).abs() >= 0.01 {
            std::thread::sleep(Duration::from_millis(10));
            st = applied(&session);
        }
        assert!(
            (st.applied_output_gain - 4.0).abs() < 0.01,
            "output gain set AFTER m17_connect must reach the M17 router live at the new 4.0 ceiling, got {}",
            st.applied_output_gain
        );

        // A live rx-compression toggle AFTER connect must also forward
        // (iax-a4e7 PHASE 1), mirroring output_gain's live-update contract.
        session.set_rx_compress(false);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut st = applied(&session);
        while std::time::Instant::now() < deadline && st.applied_rx_compress {
            std::thread::sleep(Duration::from_millis(10));
            st = applied(&session);
        }
        assert!(
            !st.applied_rx_compress,
            "a LIVE set_rx_compress(false) must reach the router"
        );

        session.m17_disconnect();
    }
}
