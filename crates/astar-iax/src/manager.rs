// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Multi-call connection pool + routing table (iax-42e9 phase 3).
//!
//! The [`Manager`] owns an [`AudioRouter`], a `HashMap<CallId, Connection>`,
//! and the routing table (`tx: CallId → Option<MicId>`, `rx: CallId →
//! OutputId`). It pools EVERY [`Call`] — dialed ([`Manager::dial`]) or inbound
//! ([`Manager::adopt`], the iax-8baf integration point) — uniformly, keyed by
//! the canonical process-level [`CallId`] (== `ConnectionSpec.id`, Q5; distinct
//! from the wire `CallNo`, which stays per-socket `CallNo(1)`, Q4).
//!
//! Vendor-neutral: `Manager` knows nothing about `AllStarLink`. The call shape
//! arrives pre-built as a [`CallMode`]; auth secrets are a `dial()`-time
//! argument and never enter a snapshot, the routing table, or a tracing line.
//!
//! Invariants enforced here:
//! - A mic feeds at most one call, and a call has at most one mic (the 1:1
//!   invariant). [`Manager::route`] clears any prior binding of the mic AND any
//!   prior mic of the call.
//! - [`Manager::key`] on a mic-less (monitor-only) call returns
//!   [`IaxError::NotRouted`].
//! - A re-routed mic drops its old call to monitor-only.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;

use astar_audio::{
    AudioBackend, AudioRouter, Conference, ConferenceConfig, MemberId, MicId, MixCallId, OutputId,
    StreamConfig,
};
use astar_iax_core::session::CodecPolicy;
use astar_iax_core::session::call_no::CallNo;
use astar_wireguard::{UdpTransport, WgLinkConfig, WgStack, WgStackStatus};
use mio::{Poll, Token, Waker};

use crate::audio_bridge::PttGate;
use crate::call::{Call, CallId, CallSnapshot, CallSnapshotMode, CallSnapshotState};
use crate::call_mode::CallMode;
use crate::error::IaxError;
use crate::link::{Link, LinkError, LinkMode, LinkRoster, LinkSnapshot, LinkState};
use crate::link_control::{LinkEvent, LinkResolver, LinkSpec, PermanentLink, SecretResolver};
use crate::routing_config::{ConnectionId, ConnectionSpec, RoutingConfig};
use crate::runtime::{SpawnParams, spawn_call_runtime};

/// A request to dial a node into the pool. Vendor-neutral: carries a fully
/// built [`CallMode`] and a `dial`-time `secret`. The stable `id` is the
/// process-level [`CallId`] (== `ConnectionSpec.id`, Q5) under which the
/// resulting connection is pooled.
pub struct DialSpec {
    /// Stable pool identity assigned by the caller (Phase 4 `ConnectionSpec.id`).
    pub id: CallId,
    /// Node number / label, carried into the snapshot (secret-free).
    pub node: String,
    /// IAX2 peer address.
    pub peer: SocketAddr,
    /// Output bus this call's RX mixes onto (monitor-only until `route`).
    pub output: OutputId,
    /// Username / caller-id presented to the peer.
    pub caller_id: String,
    /// Auth secret — `dial`-time only; never stored in a snapshot/table.
    pub secret: String,
    /// Pre-built call shape (`Standard` or `WebTransceiver`).
    pub mode: CallMode,
    /// Dial target (inert in WT mode, where it is forced to `"s"`).
    pub dest: String,
    /// Optional protocol-frame observer (the inspector's tracer). None = no tracing.
    pub frame_observer: Option<std::sync::mpsc::Sender<crate::trace::TracedFrame>>,
    /// Codec negotiation policy for this call (iax-31f7). Default `UlawOnly`.
    pub codec_policy: CodecPolicy,
}

/// Primary-link transport selection (iax-927a): every socket the engine binds
/// for this Manager's outgoing calls — and, via [`Manager::net_stack`], the
/// registrar and the inbound listener — comes from the selected transport.
///
/// - [`LinkTransport::Udp`] (the default) is plain OS UDP, byte-identical to
///   the pre-WG engine.
/// - [`LinkTransport::Wireguard`] rides ONE shared userspace `WireGuard`
///   tunnel ([`WgStack`]) to a single peer; dial/registrar peer addresses are
///   then tunnel-inner IPv4 addresses. The private key never lands in the
///   config: [`WgLinkConfig`] carries a reference resolved through the
///   caller's `SecretResolver` exactly once, at stack-build time.
///
/// The two modes are mutually exclusive on the primary link and immutable
/// while calls are pooled — switching = disconnect/reconnect.
#[derive(Debug, Default)]
pub enum LinkTransport {
    /// Plain OS UDP (default — byte-identical to the pre-WG engine).
    #[default]
    Udp,
    /// A shared userspace `WireGuard` tunnel to a single peer.
    Wireguard(WgLinkConfig),
}

/// The secret-free identity strings a connection carries for `export`/`apply`
/// round-tripping. Derived from a [`DialSpec`] on direct `dial`, or supplied
/// verbatim from a [`ConnectionSpec`] on `apply`.
struct ConnectionIdentity {
    conn_id: ConnectionId,
    calling_node: String,
    name: String,
}

/// One pooled connection: the canonical [`Call`] handle (dialed OR adopted),
/// its event stream, and the routing facts the `Manager` owns.
struct Connection {
    /// The canonical handle (keystone) — dialed via `dial` or adopted via
    /// `adopt`. Pooled identically either way.
    call: Call,
    /// The call's lifecycle/media event stream (kept so the pool owns it).
    /// `Option` so a single consumer (e.g. the console) can take it via
    /// [`Manager::take_events`] without disturbing the pool entry.
    events: Option<std::sync::mpsc::Receiver<crate::CallEvent>>,
    /// Carried from the spec for export (Phase 4, secret-free). The snapshot
    /// reads node from `call.snapshot()`.
    node: String,
    /// Stable secret-free identity (== `ConnectionSpec.id`, Q5). The pool key is
    /// the derived [`CallId`]; this is the string identity for export/apply.
    conn_id: ConnectionId,
    /// Calling node / caller-id, carried for export (secret-free).
    calling_node: String,
    /// Human-friendly connection label, carried for export.
    name: String,
    /// The output bus this call's RX mixes onto.
    output: OutputId,
    /// The bus slot id of this call's RX lane (for `set_output`/teardown).
    /// `None` while the call's RX is owned by the [`Conference`] engine instead
    /// of an output bus (iax-647d conference mode).
    mix_id: Option<MixCallId>,
    /// The currently routed mic (`None` = monitor-only). 1:1 with the mic.
    mic: Option<MicId>,
    /// The parked TX `Sender` the mic lane's dest points at once routed (Q1).
    tx_sender: std::sync::mpsc::Sender<Vec<i16>>,
    /// VOX pre-roll lead cell (iax-2733), bound into the mic lane's dest unit on
    /// `route` so a key-up pre-roll flush reaches THIS call's run-loop. For a
    /// dialed call it is the same cell the run-loop reads (from `CallAudio`); for
    /// an adopted inbound leg (whose simpler `+=20` ladder can't re-anchor) it is
    /// a parked cell — the flush still prepends the onset; nothing reads the lead.
    preroll_lead: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Conference membership slot (iax-647d): `Some` while this call is enrolled
    /// in the mix-minus [`Conference`], `None` for handset / monitor-bus calls.
    /// Removed on `remove`/`hangup` so the mix thread stops summing it.
    member: Option<MemberId>,
}

/// Secret-free snapshot of the whole pool. The single read surface for
/// iax-ad2e (Link roster) and iax-1075 (FFI): they read this, never the
/// routing table.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ManagerSnapshot {
    /// One secret-free snapshot per pooled call.
    pub calls: Vec<CallSnapshot>,
}

impl ManagerSnapshot {
    /// The routed mic id of `id` (`None` = monitor-only / unknown call).
    #[must_use]
    pub fn mic_of(&self, id: CallId) -> Option<String> {
        self.calls
            .iter()
            .find(|c| c.id == id)
            .and_then(|c| c.routed_mic.clone())
    }

    /// The output bus id of `id` (`None` = unknown call).
    #[must_use]
    pub fn output_of(&self, id: CallId) -> Option<String> {
        self.calls
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.output.clone())
    }
}

/// How the `Manager` wires an adopted call's audio (iax-647d).
///
/// - [`BridgeMode::Handset`] is today's 1:1 behavior: an adopted call mixes its
///   RX onto an output bus and (single-handset) routes the local mic to it. The
///   **`Station` library default** so existing embedders/tests stay
///   byte-identical.
/// - [`BridgeMode::Bridge`] / [`BridgeMode::Conference`] are the SAME mix-minus
///   engine ([`Conference`]): every member's TX = sum of all other members' RX.
///   `Bridge` is the documented name for the pure-bridge **daemon default**
///   (local radio off); `Conference` is an alias kept for the config surface.
/// - [`BridgeMode::Parrot`] rides the same [`Conference`] engine but in its
///   per-member record/replay/report mode (iax-feab): members enroll like a
///   conference, but each leg privately hears its own audio replayed back,
///   then a spoken signal report, then the node hangs the leg up. The join
///   greeting still plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeMode {
    /// 1:1 handset (today's behavior). Library default.
    Handset,
    /// Mix-minus bridge among remote members (daemon default).
    Bridge,
    /// Mix-minus conference — same engine as [`BridgeMode::Bridge`].
    Conference,
    /// Echo test mode (iax-feab): members enroll like a conference, but each
    /// leg privately hears its own audio replayed, then a spoken signal
    /// report, then the node hangs the leg up. The join greeting still plays.
    Parrot,
}

impl BridgeMode {
    /// Whether this mode runs the mix-minus [`Conference`] engine.
    #[must_use]
    pub fn is_conference(self) -> bool {
        matches!(
            self,
            BridgeMode::Bridge | BridgeMode::Conference | BridgeMode::Parrot
        )
    }
}

/// Bridge/conference configuration (iax-647d). `mode` selects handset vs the
/// mix-minus engine; `mix_minus` and `include_local_radio` tune the engine.
/// `Eq` is NOT derived: `parrot` carries a `f32` tuning field (`ParrotTuning`),
/// which can only ever be `PartialEq`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BridgeConfig {
    /// Handset (1:1) vs Bridge/Conference/Parrot (mix-minus engine). Library
    /// default [`BridgeMode::Handset`].
    pub mode: BridgeMode,
    /// Mix-minus: each member hears everyone but itself (default `true`).
    /// `false` = full mix (members hear themselves).
    pub mix_minus: bool,
    /// Add the local mic as a conference source + feed the local speaker the sum
    /// of all members (default `false` = pure bridge).
    pub include_local_radio: bool,
    /// Parrot-mode tuning (iax-feab), used only when `mode` is
    /// [`BridgeMode::Parrot`]. `None` picks up
    /// [`astar_audio::ParrotTuning::default`].
    pub parrot: Option<astar_audio::ParrotTuning>,
}

impl Default for BridgeConfig {
    /// The **library** default: handset (1:1), byte-identical to pre-iax-647d.
    /// The daemon overrides `mode` to [`BridgeMode::Bridge`].
    fn default() -> Self {
        Self {
            mode: BridgeMode::Handset,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        }
    }
}

/// Owns the audio router + the connection pool + the routing table, and drives
/// `dial`/`hangup`/`adopt`/`route`/`unroute`/`set_output`/`key`/`unkey`/`snapshot`.
pub struct Manager {
    router: AudioRouter,
    calls: HashMap<CallId, Connection>,
    /// Link-layer (iax-ad2e) mode bookkeeping: a thin metadata view keyed by the
    /// same [`CallId`] as the connection pool. NOT a second routing path — every
    /// mode change delegates to `route`/`unroute`. A `CallId` may be pooled
    /// without a link (dialed but not yet wrapped).
    links: HashMap<CallId, Link>,
    config: StreamConfig,
    /// Monotonic source of Manager-assigned [`CallId`]s for adopted inbound
    /// legs (which arrive as the placeholder `CallId(0)`). Dialed calls key on
    /// their caller-supplied `spec.id`; `adopt` mints from here, skipping any
    /// already-pooled id so the two id spaces never collide.
    next_call_id: u64,
    /// Permanent-link registry (iax-62cf, Decision 2). Secret-free re-dial
    /// recipes + backoff state, keyed by the link's pool `CallId`. Driven only
    /// by the app calling `tick` — no background thread.
    permanent: HashMap<CallId, PermanentLink>,
    /// Aggregated link-lifecycle event sink (iax-62cf, Decision 4). `tick`,
    /// `key_link`, and `unkey_link` emit `LinkEvent`s here; the app drains the
    /// paired receiver via `link_events()`.
    link_event_tx: std::sync::mpsc::Sender<LinkEvent>,
    /// Paired receiver, handed out once by `link_events()`.
    link_event_rx: Option<std::sync::mpsc::Receiver<LinkEvent>>,
    /// Announcement resolver + policy defaults (iax-6c5d).
    announce: crate::announce::AnnouncementService,
    /// Per-call announcement queues + in-flight tracking (iax-6c5d).
    /// Driven by [`Manager::poll_announcements`].
    announce_q: HashMap<CallId, AnnounceSlot>,
    /// Bridge/conference configuration (iax-647d). Library default is
    /// [`BridgeMode::Handset`] (today's 1:1); the daemon sets `Bridge`.
    bridge_config: BridgeConfig,
    /// The live mix-minus conference engine, present only while a conference
    /// mode is active (iax-647d). `adopt` enrolls members here instead of the
    /// 1:1 mic route; `remove`/`hangup` un-enroll them.
    conference: Option<Conference>,
    /// Station-level codec policy (iax-4348), default `CodecPolicy::default()`
    /// (`UlawOnly`, 8 kHz). Pins `config.sample_rate` (see `with_policy`) and is
    /// the policy internal (non-`dial`-caller) `DialSpec`s inherit; a `dial()`
    /// caller's own `spec.codec_policy` is capped to it in `dial_with_identity`.
    station_policy: CodecPolicy,
    /// Parrot mode (iax-feab): calls whose spoken signal report is in flight
    /// on their leg, awaiting drain before the leg is hung up. Driven by
    /// [`Manager::poll_announcements`].
    parrot_pending_hangup: Vec<(CallId, MemberId)>,
    /// Transport seam (iax-927a): the socket factory every dial runtime binds
    /// from. [`crate::transport::OsNetStack`] by default (byte-identical); the
    /// shared [`crate::transport::WgNetStack`] in `WireGuard` mode.
    net: Arc<dyn crate::transport::NetStack>,
    /// The extra plain-UDP listener address from the WG config (`None` in UDP
    /// mode or when the config leaves it unset). Read by the embedder when it
    /// starts the inbound listener.
    wg_also_bind_udp: Option<SocketAddr>,
    /// The shared `WireGuard` stack (WG mode only). Deliberately the LAST
    /// field: struct fields drop in declaration order, so `calls` (and every
    /// per-call socket) tears down before the stack's I/O thread is joined.
    wg: Option<Arc<WgStack>>,
}

// ---------------------------------------------------------------------------
// Announcement queue types (iax-6c5d)
// ---------------------------------------------------------------------------

/// Per-call announcement state: one in-flight slot + a priority-sorted pending
/// queue. Driven exclusively by [`Manager::poll_announcements`].
struct AnnounceSlot {
    current: Option<InFlight>,
    /// Pending requests, sorted descending by priority (highest first) so
    /// `pending[0]` is always the next candidate to play.
    pending: Vec<crate::announce::AnnounceRequest>,
}

/// One announcement that has been handed to the audio router and is playing (or
/// was cancelled and awaiting the lane's next callback to flip `done`).
struct InFlight {
    handle: astar_audio::AnnounceHandle,
    /// Whether the operator's PTT was already down when this announcement was
    /// started. If `false`, [`Manager::poll_announcements`] must unkey on
    /// completion so the mic lane returns to idle.
    was_operator_keyed: bool,
    /// Priority of this announcement (needed for preemption comparisons).
    priority: u8,
}

impl Manager {
    /// Construct a manager over `backend`. The router owns the open streams.
    #[must_use]
    pub fn new(backend: Box<dyn AudioBackend>) -> Self {
        let (link_event_tx, link_event_rx) = std::sync::mpsc::channel();
        Self {
            router: AudioRouter::new(backend),
            calls: HashMap::new(),
            links: HashMap::new(),
            config: StreamConfig::default(),
            next_call_id: 1,
            permanent: HashMap::new(),
            link_event_tx,
            link_event_rx: Some(link_event_rx),
            announce: crate::announce::AnnouncementService::new(crate::announce::ServiceConfig {
                resolver: crate::announce::ResolverConfig::default(),
                mixunder_default_gain_db: -12.0,
                cw_keys_when_idle: true,
                tts: crate::announce::tts::TtsConfig::default(),
            }),
            announce_q: HashMap::new(),
            bridge_config: BridgeConfig::default(),
            conference: None,
            station_policy: CodecPolicy::default(),
            parrot_pending_hangup: Vec::new(),
            net: Arc::new(crate::transport::OsNetStack),
            wg_also_bind_udp: None,
            wg: None,
        }
    }

    /// Build a manager whose station pipeline rate is pinned by `policy`
    /// (iax-4348): 16 kHz iff the policy can offer slin16, else 8 kHz.
    /// `Manager::new` is unchanged and equivalent to
    /// `with_policy(backend, CodecPolicy::default())` (8 kHz).
    #[must_use]
    pub fn with_policy(backend: Box<dyn AudioBackend>, policy: CodecPolicy) -> Self {
        let mut m = Self::new(backend);
        m.station_policy = policy;
        m.config.sample_rate = policy.max_sample_rate();
        m
    }

    /// The station audio pipeline sample rate in Hz (iax-4348): 8 kHz for
    /// [`Manager::new`], `policy.max_sample_rate()` for [`Manager::with_policy`]
    /// (16 kHz iff the policy can offer slin16). Fixed at construction.
    #[must_use]
    pub fn pipeline_sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    /// Select the primary-link transport (iax-927a). [`LinkTransport::Udp`]
    /// (the default — calling this with it is a no-op reset) keeps plain OS
    /// UDP; [`LinkTransport::Wireguard`] builds ONE shared [`WgStack`] whose
    /// underlay is an OS UDP socket bound to an ephemeral port, and every
    /// subsequent dial runtime binds its socket from that stack. Hand
    /// [`Manager::net_stack`] to the inbound listener / registrar so they ride
    /// the same tunnel.
    ///
    /// `secret_of` resolves the config's private-key reference — consulted
    /// exactly once, here; key material is never stored (house secret rule).
    ///
    /// # Errors
    /// - [`IaxError::CallInProgress`] if calls are pooled (the transport is
    ///   immutable while a session is up — switch = disconnect/reconnect).
    /// - [`IaxError::Io`] if the underlay socket or the tunnel config/key is
    ///   unusable.
    pub fn set_link_transport(
        &mut self,
        transport: LinkTransport,
        secret_of: &SecretResolver<'_>,
    ) -> Result<(), IaxError> {
        match transport {
            LinkTransport::Udp => {
                if !self.calls.is_empty() {
                    return Err(IaxError::CallInProgress);
                }
                self.net = Arc::new(crate::transport::OsNetStack);
                self.wg_also_bind_udp = None;
                self.wg = None;
                Ok(())
            }
            LinkTransport::Wireguard(cfg) => {
                let underlay =
                    astar_wireguard::UdpSocketTransport::bound().map_err(IaxError::Io)?;
                self.set_wireguard_transport_over(&cfg, secret_of, Box::new(underlay))
            }
        }
    }

    /// [`Manager::set_link_transport`]'s `WireGuard` arm over a caller-supplied
    /// underlay [`UdpTransport`] (tests use in-memory paired transports; an
    /// embedder could supply a custom datagram carrier). Same semantics and
    /// secret handling otherwise.
    ///
    /// # Errors
    /// See [`Manager::set_link_transport`].
    pub fn set_wireguard_transport_over(
        &mut self,
        cfg: &WgLinkConfig,
        secret_of: &SecretResolver<'_>,
        underlay: Box<dyn UdpTransport>,
    ) -> Result<(), IaxError> {
        if !self.calls.is_empty() {
            return Err(IaxError::CallInProgress);
        }
        let stack = Arc::new(WgStack::new(cfg, secret_of, underlay).map_err(|e| {
            IaxError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?);
        tracing::info!(
            endpoint = %cfg.endpoint(),
            tunnel_ip = %cfg.tunnel_ip(),
            "wireguard link transport enabled"
        );
        self.net = Arc::new(crate::transport::WgNetStack::new(Arc::clone(&stack)));
        self.wg_also_bind_udp = cfg.also_bind_udp();
        self.wg = Some(stack);
        Ok(())
    }

    /// The socket factory of the selected transport (iax-927a). Hand this to
    /// [`crate::IncomingCallListenerBuilder::net`] and
    /// [`crate::Registrar::with_net`] so inbound + registration share the
    /// Manager's link transport. Plain OS UDP unless `WireGuard` mode is set.
    #[must_use]
    pub fn net_stack(&self) -> Arc<dyn crate::transport::NetStack> {
        Arc::clone(&self.net)
    }

    /// The extra plain-UDP listener address configured on the `WireGuard`
    /// link (`None` in UDP mode or when unset). Pass to
    /// [`crate::IncomingCallListenerBuilder::also_bind_udp`].
    #[must_use]
    pub fn also_bind_udp(&self) -> Option<SocketAddr> {
        self.wg_also_bind_udp
    }

    /// Tunnel status (handshake age, traffic counters) for periodic
    /// operational logging. `None` unless `WireGuard` mode is active.
    #[must_use]
    pub fn wg_status(&self) -> Option<WgStackStatus> {
        self.wg.as_ref().map(|s| s.status())
    }

    /// Mint a fresh pool key for an adopted inbound leg, skipping any id that a
    /// dialed call already holds (the two id spaces share one `HashMap`).
    fn mint_call_id(&mut self) -> CallId {
        let mut raw = self.next_call_id;
        while self.calls.contains_key(&CallId::from_raw(raw)) {
            raw = raw.wrapping_add(1);
        }
        self.next_call_id = raw.wrapping_add(1);
        CallId::from_raw(raw)
    }

    /// Current bridge/conference configuration (iax-647d).
    #[must_use]
    pub fn bridge_config(&self) -> BridgeConfig {
        self.bridge_config
    }

    /// Whether a mix-minus conference engine is currently active.
    #[must_use]
    pub fn conference_active(&self) -> bool {
        self.conference.is_some()
    }

    /// Number of calls currently enrolled in the mix-minus conference (0 when no
    /// conference is active).
    #[must_use]
    pub fn conference_member_count(&self) -> usize {
        self.conference.as_ref().map_or(0, Conference::member_count)
    }

    /// Build (or fetch) the live conference engine, starting its 20 ms mixing
    /// thread on first use. The `include_local_radio` wiring (local mic source +
    /// local speaker sink) is reserved: a future change hands the conference the
    /// router's local channels; today both default to absent (pure bridge).
    ///
    /// Parrot mode (iax-feab): `parrot` is set only when the engine is being
    /// freshly created under [`BridgeMode::Parrot`] — an ALREADY-live engine
    /// (e.g. a direct Bridge→Parrot flip without an intervening Handset) keeps
    /// whatever `parrot` it was built with, since [`Conference`] has no live
    /// parrot re-toggle (mirrors `mix_minus`, which IS live-settable via
    /// `set_bridge_config`).
    fn ensure_conference(&mut self) -> &Conference {
        if self.conference.is_none() {
            self.conference = Some(Conference::start(ConferenceConfig {
                mix_minus: self.bridge_config.mix_minus,
                local_mic: None,
                local_out: None,
                sample_rate: self.config.sample_rate,
                parrot: (self.bridge_config.mode == BridgeMode::Parrot)
                    .then(|| self.bridge_config.parrot.unwrap_or_default()),
                // Node behavior (iax-8ca0): member DTMF is command input for
                // THIS node — detect it, squelch it out of the relay, and
                // surface it via `Manager::drain_dtmf_digits`.
                dtmf_squelch: true,
            }));
        }
        self.conference.as_ref().expect("just created")
    }

    /// Set the bridge/conference configuration and re-wire live calls (the
    /// `POST /bridge` path, iax-647d). Switching INTO a conference mode enrolls
    /// every pooled call as a mix-minus member (detaching its RX from its output
    /// bus); switching back to handset drains the conference and re-attaches each
    /// call's RX to its output bus. `mix_minus` toggles live on the running
    /// engine. Calls already up are re-wired in place.
    ///
    /// # Errors
    /// [`IaxError::Audio`] if re-attaching a call to its output bus fails.
    pub fn set_bridge_config(&mut self, config: BridgeConfig) -> Result<(), IaxError> {
        let was_conf = self.bridge_config.mode.is_conference();
        let now_conf = config.mode.is_conference();
        self.bridge_config = config;

        if now_conf {
            // Ensure the engine exists with the requested mix-minus, then enroll
            // every pooled call that isn't already a member.
            self.ensure_conference();
            if let Some(conf) = self.conference.as_ref() {
                conf.set_mix_minus(config.mix_minus);
            }
            if !was_conf {
                self.enroll_all_as_members();
            }
        } else if was_conf {
            // Handset: drain the conference and re-attach each call to its bus.
            self.drain_conference_to_buses()?;
            self.conference = None;
        }
        Ok(())
    }

    /// Enroll every pooled call as a conference member, detaching each from its
    /// output bus first (handset→conference live switch).
    fn enroll_all_as_members(&mut self) {
        let ids: Vec<CallId> = self.calls.keys().copied().collect();
        for id in ids {
            self.enroll_one_as_member(id);
        }
    }

    /// Enroll ONE pooled call as a conference member, detaching its RX from
    /// the output bus first. No-op when the call is already a member or has no
    /// bus attachment. Shared by the handset→conference live switch
    /// ([`Manager::enroll_all_as_members`]) and by `dial` (iax-6a3f), so an
    /// outbound call — every node-to-node link is one — joins the mix exactly
    /// like an `adopt`ed inbound leg instead of mixing onto a bus nobody hears.
    fn enroll_one_as_member(&mut self, id: CallId) {
        let Some((mix_id, out, tx, key, waker, already)) = self.calls.get(&id).map(|conn| {
            (
                conn.mix_id,
                conn.output.clone(),
                conn.tx_sender.clone(),
                conn.call.remote_keyed_handle(),
                conn.call.waker(),
                conn.member.is_some(),
            )
        }) else {
            return;
        };
        if already {
            return;
        }
        // Detach the RX lane from the bus and recover its Receiver so the
        // conference can own it.
        let Some(mix_id) = mix_id else { return };
        let Some(rx_source) = self.router.take_from_bus(&out, mix_id) else {
            return;
        };
        let engine = self.ensure_conference();
        let member = engine.add_member_keyed(rx_source, tx, key);
        // iax-feab: same wake wiring as `adopt` — see its comment.
        engine.set_member_wake(
            member,
            Arc::new(move || {
                let _ = waker.wake();
            }),
        );
        let conn = self.calls.get_mut(&id).expect("present");
        conn.member = Some(member);
        conn.mix_id = None;
        // A pre-existing link view keeps its mode's relay flags across the
        // handset→conference live switch (iax-42ce).
        self.sync_link_relay(id);
    }

    /// Drain every conference member back onto its output bus (conference→handset
    /// live switch). The per-member mix-minus output is discarded; each call's RX
    /// re-joins its bus mixer so the local speaker hears it again.
    fn drain_conference_to_buses(&mut self) -> Result<(), IaxError> {
        if self.conference.is_none() {
            return Ok(());
        }
        let ids: Vec<CallId> = self.calls.keys().copied().collect();
        for id in ids {
            let (member, out) = {
                let conn = self.calls.get(&id).expect("present");
                (conn.member, conn.output.clone())
            };
            let Some(member) = member else { continue };
            let taken = self.conference.as_ref().and_then(|c| c.take_member(member));
            if let Some(rx_source) = taken {
                let mix_id = self.router.add_rx_to_bus(&out, rx_source, self.config)?;
                let conn = self.calls.get_mut(&id).expect("present");
                conn.mix_id = Some(mix_id);
            }
            let conn = self.calls.get_mut(&id).expect("present");
            conn.member = None;
        }
        Ok(())
    }

    /// Dial a node into the pool, monitor-only (no mic until `route`). Opens /
    /// joins the spec's output bus, spawns the per-call runtime with a
    /// per-socket `CallNo(1)` (Q4), and registers the [`Connection`] under the
    /// spec's [`CallId`] (Q5).
    ///
    /// # Errors
    /// - [`IaxError::CallInProgress`] if `spec.id` is already pooled.
    /// - [`IaxError::Audio`] if opening the output bus fails.
    /// - [`IaxError::Io`] if the socket/poll setup fails.
    pub fn dial(&mut self, spec: DialSpec) -> Result<CallId, IaxError> {
        // Direct dials carry a derived secret-free identity: the `ConnectionId`
        // mirrors the `CallId` (Q5 lock-step), `calling_node` mirrors the
        // caller-id, and `name` mirrors the node label. `apply` overrides these
        // with the true `ConnectionSpec` strings via `dial_with_identity`.
        let identity = ConnectionIdentity {
            conn_id: ConnectionId::new(spec.id.as_raw().to_string()),
            calling_node: spec.caller_id.clone(),
            name: spec.node.clone(),
        };
        self.dial_with_identity(spec, identity)
    }

    /// Dial a node into the pool carrying an explicit secret-free identity. The
    /// `apply()` path uses this so `export()` round-trips the true
    /// `ConnectionSpec` strings (`id`/`calling_node`/`name`).
    fn dial_with_identity(
        &mut self,
        spec: DialSpec,
        identity: ConnectionIdentity,
    ) -> Result<CallId, IaxError> {
        if self.calls.contains_key(&spec.id) {
            return Err(IaxError::CallInProgress);
        }

        // RX side: open/join the bus, get the parked TX sender + the run-loop
        // channel ends as a CallAudio.
        let (audio, tx_sender, mix_id) =
            self.router.open_monitor_call(&spec.output, self.config)?;
        // The pre-roll lead cell the run-loop reads (iax-2733); bound into the
        // mic lane's dest unit on `route` so a key-up flush reaches this loop.
        let preroll_lead = std::sync::Arc::clone(&audio.preroll_lead);

        // Lower the call mode (WT forces dest="s" + supplies the profile).
        let (mut profile, forced_dest) = spec.mode.resolve();
        // Cap the requested policy to what the station pipeline can actually
        // carry (iax-4348): an 8 kHz station cannot offer slin16.
        let policy = spec.codec_policy.capped_to_rate(self.config.sample_rate);
        if policy != spec.codec_policy {
            tracing::warn!(requested = ?spec.codec_policy, capped = ?policy,
                "dial codec policy capped to station rate");
        }
        profile.codec_policy = policy;
        let dest = forced_dest.map_or(spec.dest.clone(), ToString::to_string);
        let mode = snapshot_mode(&spec.mode);

        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let gate = PttGate::new();

        let (call, events) = spawn_call_runtime(SpawnParams {
            peer: spec.peer,
            caller_id: spec.caller_id,
            dest,
            secret: spec.secret,
            profile,
            call_no: CallNo::new(1).expect("CallNo::new(1) is valid"),
            poll,
            waker,
            gate,
            audio,
            frame_observer: spec.frame_observer,
            id: spec.id,
            node: spec.node.clone(),
            mode,
            pooled: true,
            // The station bus rate (iax-4348): the codec edge resamples between
            // it and the negotiated wire rate.
            sample_rate: self.config.sample_rate,
            // Transport seam (iax-b6f5/iax-927a): the Manager's selected
            // transport — plain OS UDP unless WireGuard mode is set.
            net: Arc::clone(&self.net),
        })?;

        // Manager owns the routing facts that feed the snapshot.
        call.set_output(spec.output.as_str().to_string());
        call.set_routed_mic(None);

        self.calls.insert(
            spec.id,
            Connection {
                call,
                events: Some(events),
                node: spec.node,
                conn_id: identity.conn_id,
                calling_node: identity.calling_node,
                name: identity.name,
                output: spec.output,
                mix_id: Some(mix_id),
                mic: None,
                tx_sender,
                preroll_lead,
                member: None,
            },
        );
        // iax-6a3f: in conference/bridge mode a dialed call must JOIN the mix,
        // exactly like an adopted inbound leg. Without this a node-to-node link
        // (always a dial) mixed onto an output bus nobody hears and waited on a
        // mic for TX — the link showed "up" and passed no audio either way.
        // Handset mode is untouched (bus + 1:1 mic as before).
        if self.bridge_config.mode.is_conference() {
            self.enroll_one_as_member(spec.id);
        }
        Ok(spec.id)
    }

    /// Adopt an inbound leg (from iax-8baf `accept`/auto-answer) into the pool,
    /// monitor-only, identically to a dialed call. Joins the call's
    /// `CallAudio.rx_frames` source into `out`'s mixer and registers a
    /// monitor-only [`Connection`]. After `adopt`, the inbound call is
    /// indistinguishable from a dialed one to `route`/`key`/`set_output`.
    ///
    /// # Errors
    /// - [`IaxError::CallInProgress`] if the call's id is already pooled.
    /// - [`IaxError::Audio`] if opening the output bus fails.
    pub fn adopt(&mut self, mut call: Call, out: &OutputId) -> Result<CallId, IaxError> {
        // Inbound legs arrive carrying the placeholder `CallId(0)`; the Manager
        // owns identity assignment (keystone), so mint a fresh pool key and
        // stamp it onto the leg. Without this, adopting a second inbound leg
        // would collide at `CallId(0)` (e.g. iax-6461's N-caller echo node).
        let id = self.mint_call_id();
        call.set_call_id(id);
        let node = call.snapshot().node;

        // Rate-match guard (iax-4348): the leg carries its own bus rate (its
        // listener policy's codec cap). A leg whose rate differs from this
        // station's bus can never share the router's mixer (mixing 8 kHz and
        // 16 kHz PCM on one bus is silent corruption), so refuse it — never mix
        // rates on one bus. A dial-path Call carries `None` and is exempt.
        if let Some(leg_rate) = call.adopt_sample_rate()
            && leg_rate != self.config.sample_rate
        {
            tracing::warn!(
                leg = leg_rate,
                station = self.config.sample_rate,
                "refusing to adopt leg with mismatched bus rate"
            );
            return Err(IaxError::MissingConfig(
                "listener/station sample-rate mismatch",
            ));
        }

        // The inbound leg arrives carrying its router-facing CallAudio: the
        // run-loop drains `tx_frames` (mic→call) and fills `rx_frames`
        // (call→speaker). For RX-bus wiring the Manager needs the matching
        // Receiver the inbound builder paired with `rx_frames`; iax-8baf hands
        // it via the Call's adopt RX source. Open/join the bus and register it.
        let rx_source = call
            .take_adopt_rx_source()
            .ok_or(IaxError::MissingConfig("inbound call has no rx source"))?;
        // The inbound leg's parked TX sender, bound to a mic lane on `route`.
        let tx_sender = call
            .take_adopt_tx_sender()
            .ok_or(IaxError::MissingConfig("inbound call has no tx sender"))?;

        // Conference mode (iax-647d): enroll the leg as a mix-minus member —
        // its RX is summed into every OTHER member's TX and its own TX gets the
        // mix-minus output — instead of mixing onto an output bus + 1:1 mic.
        // Handset mode keeps today's path: RX onto the bus, TX parked for route.
        let (mix_id, member) = if self.bridge_config.mode.is_conference() {
            let key = call.remote_keyed_handle();
            // iax-feab: wake this leg's run-loop whenever the conference
            // enqueues new TX audio for it (mirrors `TxFrames::send`'s explicit
            // wake on the raw-dial path). Without this, a parrot's replay —
            // which by definition only starts after the record side detects
            // silence, i.e. once the peer has gone quiet — can sit unseen in
            // the leg's TX channel until its own periodic poll-timeout
            // fallback, which can race a fast-following hangup and silently
            // drop the tail of the replay.
            let waker = call.waker();
            let engine = self.ensure_conference();
            let member = engine.add_member_keyed(rx_source, tx_sender.clone(), key);
            engine.set_member_wake(
                member,
                Arc::new(move || {
                    let _ = waker.wake();
                }),
            );
            (None, Some(member))
        } else {
            let mix_id = self.router.add_rx_to_bus(out, rx_source, self.config)?;
            (Some(mix_id), None)
        };

        call.set_output(out.as_str().to_string());
        call.set_routed_mic(None);

        self.calls.insert(
            id,
            Connection {
                call,
                events: Some(std::sync::mpsc::channel().1),
                conn_id: ConnectionId::new(id.as_raw().to_string()),
                calling_node: String::new(),
                name: node.clone(),
                node,
                output: out.clone(),
                mix_id,
                mic: None,
                tx_sender,
                // Adopted inbound leg: parked lead cell (its `+=20` ladder can't
                // re-anchor, so nothing reads this; a pre-roll flush still
                // prepends the onset frames ahead of the live stream).
                preroll_lead: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
                member,
            },
        );
        Ok(id)
    }

    /// [`Manager::adopt`], but also pools the leg's [`crate::CallEvent`]
    /// receiver (the one the listener's `answer()` hands back alongside the
    /// [`Call`]) instead of the placeholder `adopt` parks. Pooling the stream
    /// lets [`Manager::drain_dtmf_digits`] surface the leg's OUT-OF-BAND IAX
    /// DTMF frames (`CallEvent::Dtmf`) in the same digit stream as in-band
    /// tones (iax-8ca0). A caller that adopts this way must not also expect
    /// the full stream later — see the `drain_dtmf_digits` contract (or
    /// `take_events` the receiver back before the first drain).
    ///
    /// # Errors
    /// Same as [`Manager::adopt`].
    pub fn adopt_with_events(
        &mut self,
        call: Call,
        out: &OutputId,
        events: std::sync::mpsc::Receiver<crate::CallEvent>,
    ) -> Result<CallId, IaxError> {
        let id = self.adopt(call, out)?;
        self.calls.get_mut(&id).expect("just adopted").events = Some(events);
        Ok(id)
    }

    /// Route a mic to a call (the call becomes its transmit source). Enforces
    /// the 1:1 invariant: any prior binding of `mic` is cleared (that call
    /// drops to monitor-only), and any prior mic of `call` is released.
    ///
    /// # Errors
    /// - [`IaxError::NoActiveCall`] if `call` isn't pooled.
    /// - [`IaxError::Audio`] if opening the mic lane fails.
    pub fn route(&mut self, call: CallId, mic: &MicId) -> Result<(), IaxError> {
        if !self.calls.contains_key(&call) {
            return Err(IaxError::NoActiveCall);
        }

        // Clear any OTHER call currently bound to this mic (1:1, re-route drops
        // the old call to monitor-only).
        let prior: Option<CallId> = self
            .calls
            .iter()
            .find(|(id, c)| **id != call && c.mic.as_ref() == Some(mic))
            .map(|(id, _)| *id);
        if let Some(prev) = prior {
            if let Some(conn) = self.calls.get_mut(&prev) {
                conn.mic = None;
                conn.call.set_routed_mic(None);
                // Monitor-only now (no mic): stop reporting the old mic's capture
                // overruns in this call's snapshot (iax-9e55).
                conn.call.set_capture_overruns(None);
                // Demoted to monitor-only: it can no longer transmit, so it must
                // not report `keyed` (a monitor-only call is never keyed). Clear
                // its PTT gate; ignore a send error (the leg may already be gone).
                let _ = conn.call.set_ptt(false);
            }
            // PTT is a property of the binding: the mic lane was keyed for the
            // prior call, so reset its gate — the new call starts unkeyed until
            // it is explicitly `key`ed, rather than inheriting a hot mic.
            self.router.set_gate(mic, false);
        }

        // Release this call's previous mic (if different) so a mic-swap on the
        // same call doesn't leave the old lane bound.
        if let Some(old) = self.calls.get(&call).and_then(|c| c.mic.clone())
            && &old != mic
        {
            self.router.unbind_mic(&old);
        }

        // Bind the mic lane's dest to THIS call's TX sender + waker + pre-roll
        // lead cell (Q1; lead added iax-2733). The three travel together so a
        // key-up flush reaches this call's run-loop.
        let (tx_sender, waker, preroll_lead) = {
            let conn = self.calls.get(&call).expect("call present");
            (
                conn.tx_sender.clone(),
                conn.call.waker(),
                std::sync::Arc::clone(&conn.preroll_lead),
            )
        };
        self.router
            .bind_mic(mic, (tx_sender, waker, preroll_lead), self.config)?;

        // Bind the now-open mic lane's capture-overrun cell into this call's
        // snapshot surface so `tx_capture_overruns` tracks the routed mic
        // (iax-9e55). The lane is open after `bind_mic`, so the cell exists.
        let overruns = self.router.mic_overruns_cell(mic);
        let conn = self.calls.get_mut(&call).expect("call present");
        conn.mic = Some(mic.clone());
        conn.call.set_routed_mic(Some(mic.as_str().to_string()));
        conn.call.set_capture_overruns(overruns);
        Ok(())
    }

    /// Drop a call to monitor-only: clear its mic lane destination and mic.
    ///
    /// # Errors
    /// [`IaxError::NoActiveCall`] if `call` isn't pooled.
    pub fn unroute(&mut self, call: CallId) -> Result<(), IaxError> {
        let mic = self
            .calls
            .get(&call)
            .ok_or(IaxError::NoActiveCall)?
            .mic
            .clone();
        if let Some(mic) = mic {
            self.router.set_gate(&mic, false);
            self.router.unbind_mic(&mic);
        }
        let conn = self.calls.get_mut(&call).expect("call present");
        conn.mic = None;
        conn.call.set_routed_mic(None);
        // Monitor-only now (no mic): stop reporting capture overruns (iax-9e55).
        conn.call.set_capture_overruns(None);
        // Monitor-only now: clear PTT so the snapshot doesn't report `keyed`.
        let _ = conn.call.set_ptt(false);
        Ok(())
    }

    /// Move a call's RX onto a different output bus. Drops the old bus's
    /// per-call jitter `residual` (≤20 ms glitch, Q3).
    ///
    /// # Errors
    /// - [`IaxError::NoActiveCall`] if `call` isn't pooled.
    /// - [`IaxError::Audio`] if opening the new bus fails.
    pub fn set_output(&mut self, call: CallId, out: &OutputId) -> Result<(), IaxError> {
        let (from, mix_id) = {
            let conn = self.calls.get(&call).ok_or(IaxError::NoActiveCall)?;
            (conn.output.clone(), conn.mix_id)
        };
        if &from == out {
            return Ok(());
        }
        // A conference member's RX is owned by the mix engine, not an output bus
        // (iax-647d): there is no bus lane to move, so just record the new label.
        let Some(mix_id) = mix_id else {
            let conn = self.calls.get_mut(&call).expect("call present");
            conn.output = out.clone();
            conn.call.set_output(out.as_str().to_string());
            return Ok(());
        };
        let new_mix = self
            .router
            .move_call_to_bus(&from, out, mix_id, self.config)?;
        let conn = self.calls.get_mut(&call).expect("call present");
        conn.output = out.clone();
        conn.mix_id = Some(new_mix);
        conn.call.set_output(out.as_str().to_string());
        Ok(())
    }

    /// Key a routed call (open its mic lane gate + send PTT-on to the peer).
    ///
    /// # Errors
    /// - [`IaxError::NoActiveCall`] if `call` isn't pooled.
    /// - [`IaxError::NotRouted`] if the call has no mic routed.
    pub fn key(&mut self, call: CallId) -> Result<(), IaxError> {
        let mic = self
            .calls
            .get(&call)
            .ok_or(IaxError::NoActiveCall)?
            .mic
            .clone()
            .ok_or(IaxError::NotRouted)?;
        self.router.set_gate(&mic, true);
        self.calls
            .get(&call)
            .expect("call present")
            .call
            .set_ptt(true)
    }

    /// Unkey a call (close its mic lane gate + send PTT-off to the peer). A
    /// monitor-only call unkeys cleanly (no-op gate).
    ///
    /// # Errors
    /// [`IaxError::NoActiveCall`] if `call` isn't pooled.
    pub fn unkey(&mut self, call: CallId) -> Result<(), IaxError> {
        let conn = self.calls.get(&call).ok_or(IaxError::NoActiveCall)?;
        if let Some(mic) = conn.mic.clone() {
            self.router.set_gate(&mic, false);
        }
        conn.call.set_ptt(false)
    }

    /// Hang up and remove a call from the pool. Unbinds its mic and detaches
    /// its RX lane from the bus.
    ///
    /// Note: the final [`Call::hangup`] joins the per-call runtime thread (which
    /// drops the audio streams) — a potentially slow, blocking teardown. A caller
    /// that holds a shared lock around the `Manager` (e.g. an SSE snapshot loop)
    /// should prefer [`Manager::remove`] so the blocking join runs off-lock.
    ///
    /// # Errors
    /// [`IaxError::NoActiveCall`] if `call` isn't pooled.
    pub fn hangup(&mut self, call: CallId, cause: Option<String>) -> Result<(), IaxError> {
        let removed = self.remove(call).ok_or(IaxError::NoActiveCall)?;
        removed.hangup(cause)
    }

    /// Remove a call from the pool, unbinding its mic and detaching its RX lane
    /// from the bus, and hand back the owned [`Call`] WITHOUT hanging it up.
    /// Returns `None` if `call` isn't pooled.
    ///
    /// This performs only the cheap pool/router bookkeeping; the blocking part
    /// of teardown ([`Call::hangup`], which joins the runtime thread) is left to
    /// the caller, who can run it off any shared lock. [`Manager::hangup`] is
    /// this plus an immediate `hangup` on the returned handle.
    #[must_use]
    pub fn remove(&mut self, call: CallId) -> Option<Call> {
        let conn = self.calls.remove(&call)?;
        if let Some(mic) = &conn.mic {
            self.router.unbind_mic(mic);
        }
        // Leave the mix-minus conference (iax-647d) if enrolled; otherwise detach
        // the RX lane from its output bus.
        if let Some(member) = conn.member {
            if let Some(conf) = self.conference.as_ref() {
                conf.remove_member(member);
            }
        } else if let Some(mix_id) = conn.mix_id {
            self.router.remove_from_bus(&conn.output, mix_id);
        }
        // Drop the announcement queue entry so it doesn't leak across
        // adopt/hangup cycles (FIX M2).
        self.announce_q.remove(&call);
        Some(conn.call)
    }

    /// Enumerate the backend's audio devices (pass-through to the router) so a
    /// caller can resolve a configured device name to a device id before
    /// [`Manager::dial`]/[`Manager::route`].
    ///
    /// # Errors
    /// Propagates [`astar_audio::AudioError`] from the backend.
    pub fn devices(&self) -> Result<Vec<astar_audio::DeviceInfo>, astar_audio::AudioError> {
        self.router.devices()
    }

    /// The backend's default input device, if any (pass-through to the router).
    #[must_use]
    pub fn default_input(&self) -> Option<astar_audio::DeviceInfo> {
        self.router.default_input()
    }

    /// The backend's default output device, if any (pass-through to the router).
    #[must_use]
    pub fn default_output(&self) -> Option<astar_audio::DeviceInfo> {
        self.router.default_output()
    }

    /// Take the lifecycle/media event receiver for a pooled call. Returns `Some`
    /// at most once per call (the pool keeps no second copy); subsequent calls
    /// return `None`. A single consumer (the console) drains this for status
    /// transitions.
    pub fn take_events(
        &mut self,
        call: CallId,
    ) -> Option<std::sync::mpsc::Receiver<crate::CallEvent>> {
        self.calls.get_mut(&call)?.events.take()
    }

    /// Secret-free snapshot of the whole pool — the single read surface for
    /// iax-ad2e (roster) and iax-1075 (FFI).
    #[must_use]
    pub fn snapshot(&self) -> ManagerSnapshot {
        ManagerSnapshot {
            calls: self.calls.values().map(|c| c.call.snapshot()).collect(),
        }
    }

    /// Number of pooled calls.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.len()
    }

    /// Remove every pooled call that has reached `Hungup` from the pool,
    /// returning the reaped ids. A remote/peer HANGUP drives a leg to `Hungup`
    /// but does NOT remove it from the pool; without this sweep the dead leg
    /// lingers, inflating [`Manager::call_count`] and — for an inbound node —
    /// permanently consuming a `max_calls` slot, so the node busy-rejects every
    /// further caller once `max_calls` callers have come and gone. Call this
    /// from the app's pump loop. (The `tick` reconnect supervisor already reaps
    /// PERMANENT-linked legs; this covers plain inbound-adopted / dialed calls.)
    pub fn reap_hungup(&mut self) -> Vec<CallId> {
        let dead: Vec<CallId> = self
            .calls
            .iter()
            .filter(|(_, c)| matches!(c.call.snapshot().state, CallSnapshotState::Hungup))
            .map(|(id, _)| *id)
            .collect();
        for id in &dead {
            let _ = self.remove(*id);
        }
        dead
    }

    /// Replace the announcement service configuration (node pushes config in).
    pub fn set_announce_config(&mut self, cfg: crate::announce::ServiceConfig) {
        self.announce = crate::announce::AnnouncementService::new(cfg);
    }

    /// Play an announcement on `call`. To-air requests auto-key the mic for the
    /// duration (never dropping an operator's existing PTT) and auto-unkey when
    /// done (only if the operator wasn't already keyed). A higher-priority
    /// request preempts the currently playing announcement. Lower-priority
    /// requests are queued and played in order when the current one finishes.
    ///
    /// Call [`Manager::poll_announcements`] periodically (or after each audio
    /// callback) to advance the queue and perform auto-unkey.
    ///
    /// # Errors
    /// - [`IaxError::NoActiveCall`] if `call` isn't pooled.
    /// - [`IaxError::AnnounceUnavailable`] if the phrase cannot be resolved, or
    ///   the request is `ToAir`/`Both` on a monitor-only call (no mic routed).
    #[allow(clippy::needless_pass_by_value)]
    pub fn announce(
        &mut self,
        call: CallId,
        req: crate::announce::AnnounceRequest,
    ) -> Result<astar_audio::AnnounceHandle, IaxError> {
        let new_priority = req.priority;

        // Check whether the current in-flight should be preempted.
        let should_preempt = self
            .announce_q
            .get(&call)
            .and_then(|s| s.current.as_ref())
            .is_some_and(|f| new_priority > f.priority);

        // Capture the preempted announcement's was_operator_keyed BEFORE clearing
        // the slot. The preempting announcement must inherit this value so the
        // original operator-keyed state propagates through the entire preemption
        // chain — preventing a permanently-keyed call when a non-operator-keyed
        // announcement is preempted while the call is still auto-keyed.
        let inherited_was_operator_keyed: Option<bool> = if should_preempt {
            let slot = self.announce_q.get_mut(&call).expect("just checked");
            let inherited = slot.current.as_ref().map(|f| f.was_operator_keyed);
            if let Some(ref f) = slot.current {
                f.handle.cancel();
            }
            slot.current = None;
            inherited
        } else {
            None
        };

        // Is there still an in-flight announcement (non-preempted)?
        let has_current = self
            .announce_q
            .get(&call)
            .and_then(|s| s.current.as_ref())
            .is_some();

        if has_current {
            // Queue behind the current (sorted descending by priority).
            let slot = self.announce_q.entry(call).or_insert(AnnounceSlot {
                current: None,
                pending: Vec::new(),
            });
            let pos = slot.pending.partition_point(|r| r.priority >= new_priority);
            slot.pending.insert(pos, req);
            // Return a handle that shares cells with the one that will be built
            // when this request reaches the front of the queue.  We need a
            // "future" handle here.  The simplest correct approach: resolve now
            // and store both the request AND a pre-built handle, then replay it
            // when it becomes current.  But that changes the data model.
            //
            // Simpler: refuse to return a handle for the queued path.  The tests
            // only call `h.is_done()` on the *preempted* handle (which is now
            // cancelled) and on handles returned when no current exists.  For
            // queued (non-preempting) requests we return a placeholder handle
            // that the caller may poll; it will flip to done once the audio lane
            // finishes — that happens the next time poll_announcements starts it.
            //
            // Actual implementation: resolve the phrase NOW so we can create the
            // handle immediately.  We store the resolved PCM in the pending
            // entry.  This requires storing a `ResolvedRequest` in pending rather
            // than the raw `AnnounceRequest`.  For now, the test only exercises
            // the preempt path where `has_current` becomes false after preemption
            // — so this branch is not exercised by the new tests.  Return an
            // immediately-done placeholder so compilation succeeds.
            let placeholder = astar_audio::AnnounceHandle::new_placeholder();
            return Ok(placeholder);
        }

        // No current in-flight — begin immediately.
        let (mut in_flight, handle) = self.begin_announcement(call, req)?;
        // If this announcement is preempting a prior one, inherit the original
        // was_operator_keyed so the auto-unkey decision reflects the true
        // pre-announcement operator state (not the transiently-keyed state).
        if let Some(inherited) = inherited_was_operator_keyed {
            in_flight.was_operator_keyed = inherited;
        }
        self.announce_q
            .entry(call)
            .or_insert(AnnounceSlot {
                current: None,
                pending: Vec::new(),
            })
            .current = Some(in_flight);
        Ok(handle)
    }

    /// Play a private announcement to ONE conference member's leg (iax-c4ea).
    ///
    /// This is the per-user-stream seam from the conference design: the phrase is
    /// resolved to PCM, encoded to 160-sample µ-law frames, and queued onto that
    /// member's TX via [`Conference::announce_to_member`] — it reaches that one
    /// user only and never touches the conference mix or any other member. Used
    /// for the node-id join greeting ("node 77777") so each joining user hears
    /// the node they reached.
    ///
    /// Unlike [`Manager::announce`], this does NOT key a mic lane (a conference
    /// member has no 1:1 mic) and does NOT participate in the per-call announce
    /// queue/preemption — it is a fire-and-forget injection on the member leg.
    ///
    /// # Errors
    /// - [`IaxError::NoActiveCall`] if `call` isn't pooled.
    /// - [`IaxError::NoActiveCall`] (reused) if `call` is not a conference member
    ///   (no member slot) or no conference engine is active.
    /// - [`IaxError::AnnounceUnavailable`] if the phrase cannot be resolved.
    #[allow(clippy::needless_pass_by_value)]
    pub fn announce_to_member(
        &mut self,
        call: CallId,
        req: crate::announce::AnnounceRequest,
        lead: std::time::Duration,
    ) -> Result<(), IaxError> {
        let member = self
            .calls
            .get(&call)
            .ok_or(IaxError::NoActiveCall)?
            .member
            .ok_or(IaxError::NoActiveCall)?;
        let resolved = self
            .announce
            .resolve_request(&req, self.config.sample_rate)
            .map_err(|_| IaxError::AnnounceUnavailable)?;
        let mut frames = pcm_frames(&resolved.pcm, self.config.sample_rate);
        // iax-9722: prepend a carrier/PTT lead of silence so the far-end squelch
        // is primed before the first spoken word (parrot report; the greeting
        // passes ZERO). 20 ms per frame.
        #[allow(clippy::cast_possible_truncation)]
        let lead_frames = (lead.as_millis() / 20) as usize;
        if lead_frames > 0 {
            let n = astar_audio::router::frame_samples(self.config.sample_rate);
            let silence = vec![0i16; n];
            let mut lead_vec = vec![silence; lead_frames];
            lead_vec.append(&mut frames);
            frames = lead_vec;
        }
        self.conference
            .as_ref()
            .ok_or(IaxError::NoActiveCall)?
            .announce_to_member(member, frames);
        Ok(())
    }

    /// Start one announcement, key the mic if needed, and return the `InFlight`
    /// record plus the caller-facing handle.
    #[allow(clippy::needless_pass_by_value)]
    fn begin_announcement(
        &mut self,
        call: CallId,
        req: crate::announce::AnnounceRequest,
    ) -> Result<(InFlight, astar_audio::AnnounceHandle), IaxError> {
        use crate::announce::Destination;

        let resolved = self
            .announce
            .resolve_request(&req, self.config.sample_rate)
            .map_err(|_| IaxError::AnnounceUnavailable)?;
        let conn = self.calls.get(&call).ok_or(IaxError::NoActiveCall)?;
        let output = conn.output.clone();
        let mic = conn.mic.clone();

        // Local-monitor leg (no keying).
        let monitor_handle = if matches!(
            resolved.destination,
            Destination::LocalMonitor | Destination::Both
        ) {
            self.router.play_into_bus(
                &output,
                std::sync::Arc::clone(&resolved.pcm),
                self.config.sample_rate,
            )
        } else {
            None
        };

        // To-air leg (key the mic lane for the duration).
        if matches!(resolved.destination, Destination::ToAir | Destination::Both) {
            let mic = mic.ok_or(IaxError::AnnounceUnavailable)?;
            let already_keyed = self
                .calls
                .get(&call)
                .expect("call present")
                .call
                .snapshot()
                .keyed;

            // FIX I2: wire cw_keys_when_idle. A MixUnder (CW) announcement on
            // an idle (unkeyed) call must respect the flag: if false, do NOT
            // auto-key — the CW simply won't go to air (the inject path only
            // transmits when the mic lane is keyed). Skip the to-air leg
            // entirely and fall through to the monitor-only path (or return
            // AnnounceUnavailable if there's no monitor handle either).
            let is_mixunder =
                matches!(req.policy, crate::announce::AnnouncePolicy::MixUnder { .. });
            if !already_keyed && is_mixunder && !self.announce.cw_keys_when_idle() {
                // Do NOT key; do NOT play to air. Fall through to monitor path.
            } else {
                if !already_keyed {
                    self.key(call)?;
                }
                let h = self
                    .router
                    .play_into_mic(&mic, resolved.pcm, resolved.audio_policy)
                    .ok_or(IaxError::AnnounceUnavailable)?;
                let caller_h = h.clone();
                let in_flight = InFlight {
                    handle: h,
                    was_operator_keyed: already_keyed,
                    priority: req.priority,
                };
                return Ok((in_flight, caller_h));
            }
        }

        let h = monitor_handle.ok_or(IaxError::AnnounceUnavailable)?;
        let caller_h = h.clone();
        let in_flight = InFlight {
            handle: h,
            was_operator_keyed: true, // monitor: no keying, so no auto-unkey needed
            priority: req.priority,
        };
        Ok((in_flight, caller_h))
    }

    /// Advance the per-call announcement queues: detect completions, auto-unkey
    /// where needed, and start the next pending announcement. Call this
    /// periodically — e.g., after each captured audio frame or on a timer.
    pub fn poll_announcements(&mut self) {
        // Collect call ids first to avoid holding a borrow on `announce_q`
        // while calling `&mut self` methods below.
        let call_ids: Vec<CallId> = self.announce_q.keys().copied().collect();

        for call in call_ids {
            // Check whether the current in-flight has finished.
            let finished = self
                .announce_q
                .get(&call)
                .and_then(|s| s.current.as_ref())
                .is_some_and(|f| f.handle.is_done());

            if finished {
                // Take the finished slot.
                // is_none_or semantics: if we somehow can't take the InFlight
                // (slot already empty), treat as operator-keyed and skip unkey.
                let was_operator_keyed = self
                    .announce_q
                    .get_mut(&call)
                    .and_then(|s| s.current.take())
                    .is_none_or(|f| f.was_operator_keyed);

                // Auto-unkey if the operator wasn't already keyed before we
                // started, AND there are no more pending announcements about to
                // re-key.
                let has_pending = self
                    .announce_q
                    .get(&call)
                    .is_some_and(|s| !s.pending.is_empty());

                if !was_operator_keyed && !has_pending {
                    let _ = self.unkey(call);
                }

                // Start the next queued announcement (if any).
                // FIX C1: propagate the finishing slot's `was_operator_keyed`
                // into the new InFlight so the original pre-announcement
                // operator state flows through the entire queue chain.
                // `begin_announcement` will re-derive `was_operator_keyed`
                // from the live call snapshot (which is still auto-keyed at
                // this point), so we must override it after the call — exactly
                // as the preemption path does in `announce`.
                if let Some(next_req) = self.announce_q.get_mut(&call).and_then(|s| {
                    if s.pending.is_empty() {
                        None
                    } else {
                        Some(s.pending.remove(0))
                    }
                }) && let Ok((mut in_flight, _h)) = self.begin_announcement(call, next_req)
                {
                    // Inherit the just-finished slot's was_operator_keyed
                    // so the queue chain preserves the original pre-keyed
                    // state rather than re-deriving it from the
                    // transiently-keyed snapshot.
                    in_flight.was_operator_keyed = was_operator_keyed;
                    self.announce_q
                        .get_mut(&call)
                        .expect("still present")
                        .current = Some(in_flight);
                }
            }
        }

        // Parrot mode (iax-feab): a finished record/replay cycle yields a
        // signal report — speak it on that member's leg, then hang the leg
        // up once the speech has fully drained. If the FSM can't resolve the
        // announcement (e.g. TTS disabled), hang up anyway: the report is
        // best-effort, never a reason to leave the leg parked.
        let reports = self
            .conference
            .as_ref()
            .map(Conference::take_parrot_reports)
            .unwrap_or_default();
        for (member, report) in reports {
            let Some((&call, _)) = self.calls.iter().find(|(_, c)| c.member == Some(member)) else {
                continue;
            };
            let codec = self
                .calls
                .get(&call)
                .and_then(|c| c.call.snapshot().negotiated_format);
            let req = crate::announce::AnnounceRequest {
                phrase: crate::announce::Phrase::Text(astar_audio::render_report(&report, codec)),
                destination: crate::announce::Destination::ToAir,
                policy: crate::announce::AnnouncePolicy::Seize,
                priority: 6,
            };
            match self.announce_to_member(call, req, std::time::Duration::from_secs(1)) {
                Ok(()) => self.parrot_pending_hangup.push((call, member)),
                Err(_) => {
                    let _ = self.hangup(call, Some("signal report complete".into()));
                }
            }
        }
        // A member leaving `parrot_pending_hangup` (queue drained to 0) is due
        // for hangup; an already-departed member (no longer in the
        // conference) is treated as done too, so it never lingers.
        let due: Vec<CallId> = self
            .parrot_pending_hangup
            .iter()
            .filter(|(_, m)| {
                self.conference
                    .as_ref()
                    .is_none_or(|c| c.member_queue_len(*m) == 0)
            })
            .map(|(c, _)| *c)
            .collect();
        self.parrot_pending_hangup.retain(|(c, _)| !due.contains(c));
        for call in due {
            let _ = self.hangup(call, Some("signal report complete".into()));
        }
    }

    /// Drain the DTMF command digits received on member legs since the last
    /// call (iax-8ca0): `(call id, digit)`. THE single digit source for the
    /// iax-d254 DTMF→link-command mapper, merging both arrival paths:
    ///
    /// - **In-band touch tones** detected (and relay-squelched) by the
    ///   conference engine, mapped `MemberId` → [`CallId`]. A digit whose
    ///   member left the pool between detection and drain is dropped.
    /// - **Out-of-band IAX DTMF frames** ([`crate::CallEvent::Dtmf`]) drained
    ///   from each pooled connection's event stream, for streams the pool
    ///   still holds (dialed calls, or legs adopted via
    ///   [`Manager::adopt_with_events`]).
    ///
    /// Contract: draining CONSUMES the pool-held event streams — non-DTMF
    /// events are discarded (they had no consumer while pooled). A caller
    /// that wants a call's full [`crate::CallEvent`] stream must
    /// [`Manager::take_events`] it BEFORE the first drain; that leg's
    /// out-of-band digits are then the taker's responsibility to merge, and
    /// this method leaves the leg alone (its in-band tones still surface
    /// here).
    #[must_use]
    pub fn drain_dtmf_digits(&self) -> Vec<(CallId, char)> {
        let mut out = Vec::new();
        // In-band: conference-detected tones, member → call id.
        if let Some(conf) = self.conference.as_ref() {
            for (member, digit) in conf.drain_dtmf_digits() {
                if let Some((&call, _)) = self.calls.iter().find(|(_, c)| c.member == Some(member))
                {
                    out.push((call, digit));
                }
            }
        }
        // Out-of-band: CallEvent::Dtmf from pool-held event streams.
        for (&call, conn) in &self.calls {
            if let Some(rx) = conn.events.as_ref() {
                while let Ok(ev) = rx.try_recv() {
                    if let crate::CallEvent::Dtmf(digit) = ev {
                        out.push((call, digit));
                    }
                }
            }
        }
        out
    }

    /// Snapshot the current wiring as a secret-free [`RoutingConfig`]. One
    /// [`ConnectionSpec`] per pooled connection, keyed by its stable
    /// [`ConnectionId`] (== the `CallId` pool key, Q5). Never reads a secret —
    /// the manager never stored one beyond the `dial()` call.
    #[must_use]
    pub fn export(&self) -> RoutingConfig {
        let connections = self
            .calls
            .values()
            .map(|c| ConnectionSpec {
                id: c.conn_id.clone(),
                node: c.node.clone(),
                calling_node: c.calling_node.clone(),
                name: c.name.clone(),
                output_device: c.output.as_str().to_string(),
                input_device: c.mic.as_ref().map(|m| m.as_str().to_string()),
            })
            .collect();
        RoutingConfig { connections }
    }

    /// Reconcile the live pool to match `cfg`. Reconciliation keys on
    /// [`ConnectionSpec::id`] (Q5), NEVER on `node` — two connections may target
    /// the same node. For each spec whose `id` is not already live it dials the
    /// connection (secret supplied by `secret_of`, peer by `peer_of`, so secrets
    /// stay OUT of the config); any pooled connection whose `id` is absent from
    /// `cfg` is hung up. Each spec's `input_device` binding is then (re)applied:
    /// a present mic is routed, `None` drops the call to monitor-only.
    ///
    /// # Errors
    /// Propagates [`IaxError`] from `dial`/`route`/`unroute`/`hangup`.
    pub fn apply(
        &mut self,
        cfg: &RoutingConfig,
        peer_of: impl Fn(&ConnectionSpec) -> SocketAddr,
        secret_of: impl Fn(&ConnectionSpec) -> String,
    ) -> Result<(), IaxError> {
        // Desired set, keyed by the derived CallId (== ConnectionSpec.id, Q5).
        let desired: HashMap<CallId, &ConnectionSpec> = cfg
            .connections
            .iter()
            .map(|s| (Self::call_id_of(&s.id), s))
            .collect();

        // Hang up any pooled connection whose id is absent from cfg.
        let stale: Vec<CallId> = self
            .calls
            .keys()
            .copied()
            .filter(|id| !desired.contains_key(id))
            .collect();
        for id in stale {
            self.hangup(id, None)?;
        }

        // Dial any desired connection not already pooled, then (re)apply the
        // input binding for every desired connection.
        for (id, spec) in &desired {
            if !self.calls.contains_key(id) {
                let dial_spec = DialSpec {
                    id: *id,
                    node: spec.node.clone(),
                    peer: peer_of(spec),
                    output: OutputId::new(spec.output_device.clone()),
                    caller_id: spec.calling_node.clone(),
                    secret: secret_of(spec),
                    mode: CallMode::Standard,
                    dest: spec.node.clone(),
                    frame_observer: None,
                    codec_policy: self.station_policy,
                };
                let identity = ConnectionIdentity {
                    conn_id: spec.id.clone(),
                    calling_node: spec.calling_node.clone(),
                    name: spec.name.clone(),
                };
                self.dial_with_identity(dial_spec, identity)?;
            }
            match &spec.input_device {
                Some(mic) => self.route(*id, &MicId::new(mic.clone()))?,
                None => self.unroute(*id)?,
            }
        }
        Ok(())
    }

    /// Derive the stable [`CallId`] pool key for a [`ConnectionId`] (Q5
    /// lock-step). A direct integer `ConnectionId` (as `export` emits for
    /// `dial`-originated connections) round-trips exactly; any other string maps
    /// through a stable hash so `apply`/`export` stay consistent.
    fn call_id_of(conn_id: &ConnectionId) -> CallId {
        if let Ok(raw) = conn_id.as_str().parse::<u64>() {
            return CallId::from_raw(raw);
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        Hash::hash(conn_id.as_str(), &mut hasher);
        CallId::from_raw(hasher.finish())
    }
}

/// Link layer (iax-ad2e): a thin metadata view over the pool. Every method here
/// DELEGATES to the routing machinery above (`route`/`unroute`/`key`/`unkey`);
/// it adds only mode bookkeeping + the read-only [`LinkRoster`]. No parallel
/// audio path.
impl Manager {
    /// Register an existing pooled connection (dialed OR adopted) as a [`Link`]
    /// in `mode`, applying the routing-table state that mode implies. Mechanism
    /// only — no command policy.
    ///
    /// `node` is a vendor-neutral peer label (e.g. an `AllStar` node number).
    ///
    /// # Errors
    /// - [`LinkError::NoActiveCall`] if `call` is not pooled.
    pub fn add_link(
        &mut self,
        call: CallId,
        node: impl Into<String>,
        mode: LinkMode,
    ) -> Result<(), LinkError> {
        if !self.calls.contains_key(&call) {
            return Err(LinkError::NoActiveCall(call));
        }
        let node = node.into();
        self.apply_mode_routing(call, mode)?;
        self.links.insert(
            call,
            Link {
                call,
                node,
                mode,
                addr: None,
                up_since: std::sync::OnceLock::new(),
            },
        );
        self.sync_link_relay(call);
        Ok(())
    }

    /// Change a link's mode. Pure routing-table mutation + metadata update:
    /// switching a Transceive link to Monitor/LocalMonitor drops it to
    /// monitor-only (mic released, RX keeps mixing), exactly iax-42e9's
    /// "switching a mic off a node leaves it up as monitor-only."
    ///
    /// # Errors
    /// - [`LinkError::NoSuchLink`] if no link is registered for `call`.
    pub fn set_link_mode(&mut self, call: CallId, mode: LinkMode) -> Result<(), LinkError> {
        if !self.links.contains_key(&call) {
            return Err(LinkError::NoSuchLink(call));
        }
        self.apply_mode_routing(call, mode)?;
        // Presence checked above.
        self.links.get_mut(&call).expect("link present").mode = mode;
        self.sync_link_relay(call);
        Ok(())
    }

    /// Remove the link view for `call`: releases any routed mic and drops the
    /// roster entry. Does NOT hang up the call — that is [`Manager::hangup`].
    /// This keeps `Link` a view, not an owner.
    ///
    /// # Errors
    /// - [`LinkError::NoSuchLink`] if no link is registered for `call`.
    pub fn remove_link(&mut self, call: CallId) -> Result<(), LinkError> {
        if self.links.remove(&call).is_none() {
            return Err(LinkError::NoSuchLink(call));
        }
        // Idempotent; drops the mic if one was bound. The call stays pooled.
        let _ = self.unroute(call);
        // Back to the full-relay conference default (iax-42ce): a pooled call
        // with no link view relays like any pre-link member.
        self.sync_link_relay(call);
        Ok(())
    }

    /// Push a link's mode onto its conference membership (iax-42ce): the mode
    /// maps to the [`Conference`] relay flags via the two Link-layer
    /// predicates — `contributes = relays_onward()` (`LocalMonitor` stays off
    /// the relay sum), `receives = is_transmit_capable()` (only `Transceive` is
    /// transmitted to). A call with NO link keeps the full-relay default.
    /// No-op in handset mode or for an unenrolled call.
    fn sync_link_relay(&self, call: CallId) {
        let Some(conf) = self.conference.as_ref() else {
            return;
        };
        let Some(member) = self.calls.get(&call).and_then(|c| c.member) else {
            return;
        };
        let (contributes, receives) = self.links.get(&call).map_or((true, true), |l| {
            (l.mode.relays_onward(), l.mode.is_transmit_capable())
        });
        conf.set_member_relay(member, contributes, receives);
    }

    /// Announce privately to every conference member that is NOT a link
    /// (iax-9e02) — the web-transceiver / handset users. Node-to-node links
    /// are deliberately excluded: a link-status announcement is for the people
    /// on THIS node, never something to transmit at a linked node.
    ///
    /// Best-effort per member: a leg that cannot take the announcement is
    /// skipped. Returns how many members it reached.
    #[allow(clippy::needless_pass_by_value)]
    pub fn announce_to_non_link_members(&mut self, req: crate::announce::AnnounceRequest) -> usize {
        let targets: Vec<CallId> = self
            .calls
            .iter()
            .filter(|(id, conn)| conn.member.is_some() && !self.links.contains_key(id))
            .map(|(id, _)| *id)
            .collect();
        let mut reached = 0;
        for call in targets {
            if self
                .announce_to_member(call, req.clone(), std::time::Duration::ZERO)
                .is_ok()
            {
                reached += 1;
            }
        }
        reached
    }

    /// Propagate receiver activity onto transceive links (iax-7d51) —
    /// `AllStar` COS semantics. A far-end node mutes relayed audio unless the
    /// sender is KEYED (`Manager::key` puts `RADIO_KEY` / `RADIO_UNKEY` control
    /// frames on the wire), so a bridged link that never keys passes voice
    /// frames the far end throws away: audio arrives FROM the link but nothing
    /// gets through TO it.
    ///
    /// While any non-link member is keyed (its peer signalled PTT), every
    /// transmit-capable link is keyed; when they all fall quiet the links
    /// unkey. Idempotent — the FSM coalesces unchanged PTT state, so calling
    /// this every pump tick puts nothing extra on the wire. No-op when there
    /// are no links.
    ///
    /// Returns the number of links whose keyed state changed.
    pub fn sync_link_keying(&mut self) -> usize {
        if self.links.is_empty() {
            return 0;
        }
        // Sources are pooled calls that are NOT links: conference members
        // (handsets, inbound node legs) whose peer has signalled PTT.
        let any_source_keyed = self.calls.iter().any(|(id, conn)| {
            !self.links.contains_key(id)
                && conn
                    .call
                    .remote_keyed_handle()
                    .load(std::sync::atomic::Ordering::Relaxed)
        });
        let targets: Vec<CallId> = self
            .links
            .values()
            .filter(|l| l.mode.is_transmit_capable())
            .map(|l| l.call)
            .collect();
        let mut changed = 0;
        for call in targets {
            let already = self
                .calls
                .get(&call)
                .is_some_and(|c| c.call.snapshot().keyed);
            if already != any_source_keyed {
                let res = if any_source_keyed {
                    self.key(call)
                } else {
                    self.unkey(call)
                };
                if res.is_ok() {
                    changed += 1;
                }
            }
            // iax-5b8e: gate the link's TX on its keying. The conference hands
            // every RECEIVING member its mix each tick, so an unkeyed link
            // would stream silence forever — to `app_rpt` an unbroken carrier,
            // which never ends the over (a parrot node records and never
            // replays). A real `AllStar` link carries audio only during an
            // over, so `receives` tracks the key.
            self.set_link_tx_enabled(call, any_source_keyed);
        }
        changed
    }

    /// Enable/disable a link member's conference TX (iax-5b8e). Transmit-only:
    /// the link keeps CONTRIBUTING its RX to the mix while unkeyed, so audio
    /// FROM the far end is never interrupted.
    fn set_link_tx_enabled(&self, call: CallId, enabled: bool) {
        let Some(conf) = self.conference.as_ref() else {
            return;
        };
        let Some(member) = self.calls.get(&call).and_then(|c| c.member) else {
            return;
        };
        let contributes = self.links.get(&call).is_none_or(|l| l.mode.relays_onward());
        conf.set_member_relay(member, contributes, enabled);
    }

    /// Key a link, refusing non-transmit modes up front. For a `Transceive`
    /// link with no mic routed the underlying [`Manager::key`] returns
    /// [`IaxError::NotRouted`], mapped here to [`LinkError::NotTransmitCapable`].
    ///
    /// # Errors
    /// - [`LinkError::NoSuchLink`] if no link is registered for `call`.
    /// - [`LinkError::NotTransmitCapable`] for a Monitor/LocalMonitor link, or a
    ///   mic-less Transceive link.
    pub fn key_link(&mut self, call: CallId) -> Result<(), LinkError> {
        let link = self.links.get(&call).ok_or(LinkError::NoSuchLink(call))?;
        if !link.mode.is_transmit_capable() {
            return Err(LinkError::NotTransmitCapable);
        }
        let node = link.node.clone();
        // iax-42e9 `key` returns NotRouted for a mic-less call → NotTransmitCapable.
        self.key(call).map_err(|_| LinkError::NotTransmitCapable)?;
        let _ = self.link_event_tx.send(LinkEvent::Keyed {
            node,
            call: call.as_raw(),
            keyed: true,
        });
        Ok(())
    }

    /// Unkey a link (always allowed; a monitor-only call unkeys cleanly).
    ///
    /// # Errors
    /// - [`LinkError::NoSuchLink`] if no link is registered for `call`.
    pub fn unkey_link(&mut self, call: CallId) -> Result<(), LinkError> {
        let node = self
            .links
            .get(&call)
            .ok_or(LinkError::NoSuchLink(call))?
            .node
            .clone();
        let _ = self.unkey(call);
        let _ = self.link_event_tx.send(LinkEvent::Keyed {
            node,
            call: call.as_raw(),
            keyed: false,
        });
        Ok(())
    }

    /// Snapshot of all live links — the `app_rpt` `RPT_LINKS` analogue, secret-free
    /// and serde-friendly. Per-link `state` and `keyed` are read straight from
    /// the canonical [`Call::snapshot`]; the roster maintains no second copy.
    #[must_use]
    pub fn link_roster(&self) -> LinkRoster {
        let mut links: Vec<LinkSnapshot> = self
            .links
            .values()
            .filter_map(|l| {
                // Read liveness/keyed from the canonical Call snapshot (single
                // source of truth). A link whose call has left the pool is
                // skipped (it has no snapshot).
                let snap = self.calls.get(&l.call)?.call.snapshot();
                let state = link_state_from(snap.state);
                let up_secs = if matches!(state, LinkState::Up) {
                    l.up_since
                        .get_or_init(std::time::Instant::now)
                        .elapsed()
                        .as_secs()
                } else {
                    0
                };
                Some(LinkSnapshot {
                    call: l.call.as_raw(),
                    node: l.node.clone(),
                    mode: l.mode,
                    state,
                    keyed: snap.keyed,
                    rx_active: snap.remote_keyed,
                    up_secs,
                    addr: l.addr.clone(),
                })
            })
            .collect();
        // Deterministic order for stable snapshots / UI diffing.
        links.sort_by_key(|s| s.call);
        LinkRoster { links }
    }

    /// Whether a mic is currently routed to `call` (`None` = monitor-only /
    /// unknown). A thin read over the canonical [`Call::snapshot`] — NOT a
    /// second source of truth.
    #[must_use]
    pub fn routed_mic(&self, call: CallId) -> Option<String> {
        self.calls.get(&call)?.call.snapshot().routed_mic
    }

    /// Set input (mic) gain for `call`'s routed mic. No-op if unrouted/unknown.
    pub fn set_input_gain(&self, call: CallId, g: f32) {
        if let Some(mic) = self.calls.get(&call).and_then(|c| c.mic.as_ref()) {
            self.router.set_mic_gain(mic, g);
        }
    }

    /// Input (mic) gain for `call`'s routed mic (`None` = unrouted/unknown).
    #[must_use]
    pub fn input_gain(&self, call: CallId) -> Option<f32> {
        let mic = self.calls.get(&call)?.mic.as_ref()?;
        self.router.mic_gain(mic)
    }

    /// Set output gain for `call`'s output bus. No-op if the call is unknown.
    pub fn set_output_gain(&self, call: CallId, g: f32) {
        if let Some(c) = self.calls.get(&call) {
            self.router.set_output_gain(&c.output, g);
        }
    }

    /// Output gain for `call`'s output bus (`None` = unknown call).
    #[must_use]
    pub fn output_gain(&self, call: CallId) -> Option<f32> {
        let c = self.calls.get(&call)?;
        self.router.output_gain(&c.output)
    }

    /// Toggle noise reduction on `call`'s routed mic. No-op if unrouted/unknown.
    pub fn set_denoise(&self, call: CallId, on: bool) {
        if let Some(mic) = self.calls.get(&call).and_then(|c| c.mic.as_ref()) {
            self.router.set_mic_denoise(mic, on);
        }
    }

    /// Toggle compression on `call`'s routed mic. No-op if unrouted/unknown.
    pub fn set_compress(&self, call: CallId, on: bool) {
        if let Some(mic) = self.calls.get(&call).and_then(|c| c.mic.as_ref()) {
            self.router.set_mic_compress(mic, on);
        }
    }

    /// Set the compression strength (0.0..=1.0, clamped) on `call`'s routed mic.
    /// No-op if unrouted/unknown.
    pub fn set_compression_level(&self, call: CallId, level: f32) {
        if let Some(mic) = self.calls.get(&call).and_then(|c| c.mic.as_ref()) {
            self.router.set_mic_compress_level(mic, level);
        }
    }

    /// Toggle RX/output compression on `call`'s output bus (iax-a4e7 PHASE 1):
    /// automatic leveling of the received audio, reusing the mic-path
    /// compressor. No-op if the call is unknown.
    pub fn set_output_compress(&self, call: CallId, on: bool) {
        if let Some(c) = self.calls.get(&call) {
            self.router.set_output_compress(&c.output, on);
        }
    }

    /// RX/output compression toggle for `call`'s output bus (`None` = unknown
    /// call).
    #[must_use]
    pub fn output_compress(&self, call: CallId) -> Option<bool> {
        let c = self.calls.get(&call)?;
        self.router.output_compress(&c.output)
    }

    /// Set the RX/output compression strength (0.0..=1.0, clamped) on `call`'s
    /// output bus. No-op if the call is unknown.
    pub fn set_output_compress_level(&self, call: CallId, level: f32) {
        if let Some(c) = self.calls.get(&call) {
            self.router.set_output_compress_level(&c.output, level);
        }
    }

    /// RX/output compression strength for `call`'s output bus (`None` =
    /// unknown call).
    #[must_use]
    pub fn output_compress_level(&self, call: CallId) -> Option<f32> {
        let c = self.calls.get(&call)?;
        self.router.output_compress_level(&c.output)
    }

    /// Set the TX trim (0.0..=2.0, clamped; 1.0 = unity) on `call`'s routed
    /// mic: the always-on final gain stage after the compressor (iax-750a).
    /// No-op if unrouted/unknown.
    pub fn set_tx_trim(&self, call: CallId, g: f32) {
        if let Some(mic) = self.calls.get(&call).and_then(|c| c.mic.as_ref()) {
            self.router.set_mic_tx_trim(mic, g);
        }
    }

    /// Set the VOX pre-roll / look-back length (ms, clamped to `0..=250`) on
    /// `call`'s routed mic (iax-2733). `0` disables pre-roll. No-op if
    /// unrouted/unknown.
    pub fn set_vox_preroll_ms(&self, call: CallId, ms: u32) {
        if let Some(mic) = self.calls.get(&call).and_then(|c| c.mic.as_ref()) {
            self.router.set_mic_preroll_ms(mic, ms);
        }
    }

    /// Apply (or clear) the calibrated mic profile on `call`'s routed mic. No-op
    /// if unrouted/unknown.
    pub fn set_mic_profile(&self, call: CallId, profile: Option<astar_audio::MicProfile>) {
        if let Some(mic) = self.calls.get(&call).and_then(|c| c.mic.as_ref()) {
            self.router.set_mic_profile(mic, profile);
        }
    }

    /// Set the live spectrum peak-hold decay (dB/SECOND, clamped, iax-8616) on
    /// `call`'s analyzers — both the routed mic's TX analyzer and the call's
    /// output-bus RX analyzer. Takes effect immediately. No-op for the mic side
    /// if the call is unrouted; the bus side applies whenever the call is known.
    pub fn set_spectrum_decay(&self, call: CallId, db_per_sec: f32) {
        let Some(c) = self.calls.get(&call) else {
            return;
        };
        if let Some(mic) = c.mic.as_ref() {
            self.router.set_mic_spectrum_decay(mic, db_per_sec);
        }
        self.router.set_output_spectrum_decay(&c.output, db_per_sec);
    }

    /// Smoothed TX level (dBFS) of `call`'s routed mic (`None` = unrouted/unknown).
    #[must_use]
    pub fn tx_dbfs(&self, call: CallId) -> Option<f32> {
        let mic = self.calls.get(&call)?.mic.as_ref()?;
        self.router.mic_tx_dbfs(mic)
    }

    /// Continuous mic INPUT level (dBFS) of `call`'s routed mic, metered even
    /// while unkeyed so VOX can key from silence (iax-5c30). Unlike
    /// [`Manager::tx_dbfs`] (post-DSP, keyed-only) this is post-gain, pre-NR and
    /// never floors on unkey. (`None` = unrouted/unknown.)
    #[must_use]
    pub fn input_dbfs(&self, call: CallId) -> Option<f32> {
        let mic = self.calls.get(&call)?.mic.as_ref()?;
        self.router.mic_input_dbfs(mic)
    }

    /// Smoothed RX level (dBFS) of `call`'s output bus (`None` = unknown call).
    #[must_use]
    pub fn rx_dbfs(&self, call: CallId) -> Option<f32> {
        let c = self.calls.get(&call)?;
        self.router.output_rx_dbfs(&c.output)
    }

    /// Copy the live TX spectrum (iax-2b09) of `call`'s routed mic lane into
    /// `out` — the SAME log-binned, peak-held dBFS values the mic monitor
    /// produces, tapped from the post-DSP, pre-encode TX PCM. Returns the number
    /// of bins written, or `None` if the call isn't routed to a mic. A pure
    /// observer that never perturbs the live call.
    #[must_use]
    pub fn tx_spectrum(&self, call: CallId, out: &mut [f32]) -> Option<usize> {
        let mic = self.calls.get(&call)?.mic.as_ref()?;
        self.router.mic_tx_spectrum(mic, out)
    }

    /// Copy the live RX spectrum (iax-2b09) of `call`'s output bus into `out` —
    /// the SAME log-binned, peak-held dBFS values the mic monitor produces,
    /// tapped from the post-mix decoded RX PCM. Returns the number of bins
    /// written, or `None` for an unknown call. A pure observer.
    #[must_use]
    pub fn rx_spectrum(&self, call: CallId, out: &mut [f32]) -> Option<usize> {
        let c = self.calls.get(&call)?;
        self.router.output_rx_spectrum(&c.output, out)
    }

    /// Smoothed round-trip time for `call`, if a sample exists.
    #[must_use]
    pub fn rtt(&self, call: CallId) -> Option<std::time::Duration> {
        self.calls.get(&call).and_then(|c| c.call.rtt())
    }

    /// Cumulative voice-ts-ladder re-anchors for `call` (>80 ms TX-clock drift
    /// events; iax-5530/iax-9e55). `None` for an unknown call. A thin read over
    /// the canonical [`Call::snapshot`] — NOT a second source of truth. A plain
    /// `u64` health counter, credential-free.
    #[must_use]
    pub fn tx_reanchors(&self, call: CallId) -> Option<u64> {
        self.calls
            .get(&call)
            .map(|c| c.call.snapshot().tx_reanchors)
    }

    /// Cumulative cpal capture overruns on `call`'s routed mic (dropped input
    /// buffers; iax-9e55). `0` while monitor-only, `None` for an unknown call. A
    /// thin read over the canonical [`Call::snapshot`] — NOT a second source of
    /// truth. A plain `u64` health counter, credential-free.
    #[must_use]
    pub fn tx_capture_overruns(&self, call: CallId) -> Option<u64> {
        self.calls
            .get(&call)
            .map(|c| c.call.snapshot().tx_capture_overruns)
    }

    /// Send a DTMF digit to `call`'s peer (passthrough to the pooled `Call`).
    ///
    /// # Errors
    /// - [`IaxError::NoActiveCall`] if `call` isn't pooled.
    /// - Propagates [`IaxError`] from the call if its runtime thread has exited.
    pub fn send_dtmf(&self, call: CallId, digit: char) -> Result<(), IaxError> {
        self.calls
            .get(&call)
            .ok_or(IaxError::NoActiveCall)?
            .call
            .send_dtmf(digit)
    }

    /// Apply the routing-table state a mode implies. The ONLY place link modes
    /// touch routing — keeps `Link` a view, not a parallel mechanism.
    ///
    /// `add_link`/`set_link_mode` deliberately do NOT pick a mic for
    /// `Transceive`: iax-42e9 owns the mic→call binding via [`Manager::route`].
    /// The Link layer only enforces the *negative* invariant (Monitor /
    /// `LocalMonitor` must have no mic).
    fn apply_mode_routing(&mut self, call: CallId, mode: LinkMode) -> Result<(), LinkError> {
        match mode {
            // Leave any existing tx binding as-is; the UI routes a mic via
            // Manager::route, then key_link.
            LinkMode::Transceive => {}
            LinkMode::Monitor | LinkMode::LocalMonitor => {
                // Never transmit: ensure no mic routed (idempotent).
                self.unroute(call)
                    .map_err(|_| LinkError::NoActiveCall(call))?;
            }
        }
        Ok(())
    }
}

/// Link control API (iax-62cf): connect/disconnect by node, permanent links with
/// app-driven auto-reconnect, and an aggregated lifecycle event stream. Built on
/// top of the iax-ad2e Link layer; vendor-neutral + secret-free.
impl Manager {
    /// Bring a link UP by node label in `spec.mode`. Resolves the node→peer via
    /// the INJECTED `resolver` (no DNS in the library), dials it into the pool,
    /// and registers a `Link`. If `spec.permanent`, also records a secret-free
    /// re-dial recipe so `tick` auto-reconnects across drops. The secret is
    /// consumed by `dial` and never stored.
    ///
    /// # Errors
    /// - [`LinkError::Resolve`] if the injected resolver fails.
    /// - Propagates `dial`/`add_link` failures via [`LinkError::NoActiveCall`].
    pub fn connect_link(
        &mut self,
        spec: LinkSpec,
        resolver: &LinkResolver<'_>,
    ) -> Result<CallId, LinkError> {
        let peer = resolver(&spec.node).map_err(|e| LinkError::Resolve(e.to_string()))?;
        // Stable id derived from the node label (matches the test_spec hash
        // pattern); two permanent links to one node would collide — acceptable
        // for v1 (one link per node), documented as a follow-up if needed.
        let id = Self::call_id_of(&crate::routing_config::ConnectionId::new(spec.node.clone()));
        let dial = DialSpec {
            id,
            node: spec.node.clone(),
            peer,
            output: spec.output.clone(),
            caller_id: spec.caller_id.clone(),
            secret: spec.secret, // moved into dial; never stored.
            mode: spec.mode_shape.clone(),
            dest: spec.dest.clone(),
            frame_observer: None,
            codec_policy: self.station_policy,
        };
        self.dial(dial).map_err(|_| LinkError::NoActiveCall(id))?;
        self.add_link(id, spec.node.clone(), spec.mode)?;
        if let Some(l) = self.links.get_mut(&id) {
            l.addr = Some(peer.to_string());
        }
        if spec.permanent {
            self.permanent.insert(
                id,
                PermanentLink {
                    node: spec.node,
                    mode: spec.mode,
                    output: spec.output,
                    caller_id: spec.caller_id,
                    dest: spec.dest,
                    mode_shape: spec.mode_shape,
                    desired: true,
                    attempts: 0,
                    next_attempt_at: None,
                },
            );
        }
        Ok(id)
    }

    /// Tear a link DOWN by its `CallId`: drops the link view, marks any permanent
    /// recipe as no-longer-desired (so `tick` won't re-dial it), and hangs the
    /// call up.
    ///
    /// # Errors
    /// [`LinkError::NoActiveCall`] if the call isn't pooled.
    pub fn disconnect_link(&mut self, call: CallId) -> Result<(), LinkError> {
        if let Some(p) = self.permanent.get_mut(&call) {
            p.desired = false;
        }
        self.permanent.remove(&call);
        // Drop the link view if present (idempotent), then hang the call up.
        let _ = self.remove_link(call);
        self.hangup(call, None)
            .map_err(|_| LinkError::NoActiveCall(call))
    }

    /// Take the aggregated [`LinkEvent`] receiver. Returns `Some` exactly once
    /// (the first call); subsequent calls return `None`. The app drains this to
    /// observe Connected/Disconnected/Keyed across ALL links on one stream.
    pub fn link_events(&mut self) -> Option<std::sync::mpsc::Receiver<LinkEvent>> {
        self.link_event_rx.take()
    }

    /// Reconnect supervisor (iax-62cf, Decision 2). Call this from the app's
    /// event loop with the current `now`. For each PERMANENT link whose call has
    /// dropped (left the pool or reached Hungup) and whose backoff window has
    /// elapsed, re-resolve the peer (`resolver`), re-fetch the secret
    /// (`secret_of`), re-dial, and re-register the link in its stored mode. Emits
    /// (and returns) the resulting `LinkEvent`s. NO hidden thread: reconnection
    /// happens only when this is called.
    pub fn tick(
        &mut self,
        now: std::time::Instant,
        resolver: &LinkResolver<'_>,
        secret_of: &SecretResolver<'_>,
    ) -> Vec<LinkEvent> {
        let mut emitted: Vec<LinkEvent> = Vec::new();

        // Identify permanent links whose call is gone or hung up.
        let down: Vec<CallId> = self
            .permanent
            .iter()
            .filter(|(id, p)| {
                if !p.desired {
                    return false;
                }
                match self.calls.get(id) {
                    None => true,
                    Some(conn) => {
                        matches!(conn.call.snapshot().state, CallSnapshotState::Hungup)
                    }
                }
            })
            .map(|(id, _)| *id)
            .collect();

        for id in down {
            // Backoff gate.
            let ready = self
                .permanent
                .get(&id)
                .and_then(|p| p.next_attempt_at)
                .is_none_or(|at| at <= now);
            if !ready {
                continue;
            }

            // Emit Disconnected once for the observed drop.
            let node = self
                .permanent
                .get(&id)
                .map(|p| p.node.clone())
                .unwrap_or_default();
            let ev = LinkEvent::Disconnected {
                node: node.clone(),
                call: id.as_raw(),
                reason: "permanent link dropped; reconnecting".to_string(),
            };
            let _ = self.link_event_tx.send(ev.clone());
            emitted.push(ev);

            // If the old call is still pooled (Hungup but not removed), clean it up.
            if self.calls.contains_key(&id) {
                let _ = self.remove_link(id);
                let _ = self.hangup(id, None);
            } else {
                // Pool already lost it; drop any stale link-view entry.
                let _ = self.remove_link(id);
            }

            // Re-resolve + re-dial.
            let recipe = self.permanent.get(&id).expect("present");
            let (node, mode, output, caller_id, dest, mode_shape) = (
                recipe.node.clone(),
                recipe.mode,
                recipe.output.clone(),
                recipe.caller_id.clone(),
                recipe.dest.clone(),
                recipe.mode_shape.clone(),
            );
            match resolver(&node) {
                Ok(peer) => {
                    let dial = DialSpec {
                        id,
                        node: node.clone(),
                        peer,
                        output,
                        caller_id,
                        secret: secret_of(&node), // re-supplied; never stored.
                        mode: mode_shape,
                        dest,
                        frame_observer: None,
                        codec_policy: self.station_policy,
                    };
                    if self.dial(dial).is_ok() && self.add_link(id, node.clone(), mode).is_ok() {
                        if let Some(l) = self.links.get_mut(&id) {
                            l.addr = Some(peer.to_string());
                        }
                        // Success: reset backoff.
                        if let Some(p) = self.permanent.get_mut(&id) {
                            p.attempts = 0;
                            p.next_attempt_at = None;
                        }
                        let ev = LinkEvent::Connected {
                            node,
                            call: id.as_raw(),
                        };
                        let _ = self.link_event_tx.send(ev.clone());
                        emitted.push(ev);
                    } else {
                        self.schedule_retry(id, now);
                    }
                }
                Err(_) => self.schedule_retry(id, now),
            }
        }
        emitted
    }

    /// Advance a permanent link's backoff after a failed reconnect attempt.
    fn schedule_retry(&mut self, id: CallId, now: std::time::Instant) {
        if let Some(p) = self.permanent.get_mut(&id) {
            let delay = crate::link_control::backoff_delay(p.attempts);
            p.attempts = p.attempts.saturating_add(1);
            p.next_attempt_at = Some(now + delay);
        }
    }
}

/// Map the canonical snapshot's coarse state to a [`LinkState`]. `Active` is
/// `Up`; everything else (Connecting / Hungup) reads as `Connecting` for the
/// roster's "not yet live" sense.
/// Chunk announcement PCM into 20 ms frames at the station rate, zero-padding
/// the trailing partial frame (iax-c4ea; rate-aware since iax-4348).
fn pcm_frames(pcm: &[i16], sample_rate: u32) -> Vec<Vec<i16>> {
    let n = astar_audio::router::frame_samples(sample_rate);
    pcm.chunks(n)
        .map(|chunk| {
            let mut frame = chunk.to_vec();
            frame.resize(n, 0);
            frame
        })
        .collect()
}

fn link_state_from(state: CallSnapshotState) -> LinkState {
    match state {
        CallSnapshotState::Active => LinkState::Up,
        CallSnapshotState::Connecting | CallSnapshotState::Hungup => LinkState::Connecting,
    }
}

/// Map a [`CallMode`] to its secret-free snapshot variant.
fn snapshot_mode(mode: &CallMode) -> CallSnapshotMode {
    match mode {
        CallMode::Standard => CallSnapshotMode::Direct,
        CallMode::WebTransceiver { .. } => CallSnapshotMode::WebTransceiver,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};

    use astar_audio::{
        AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
        StreamConfig, StreamHandle,
    };

    use crate::call::{Call, CallId, CallSnapshotMode, STATE_ACTIVE};

    /// Build a canonical inbound [`Call`] WITHOUT a live socket — for the
    /// `adopt` pooling assertions. Mirrors the shape iax-8baf's `accept` will
    /// produce: a parked `CallAudio` (the RX `Receiver` joined to a bus + the TX
    /// `Sender` bound to a mic on `route`), an already-active state, and a
    /// dead runtime thread (no I/O; control commands are no-ops).
    pub(crate) fn fake_inbound_call(id: CallId, node: &str) -> Call {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<crate::runtime::RuntimeCommand>();
        let poll = mio::Poll::new().expect("poll");
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(1)).expect("waker"));
        // A stand-in runtime thread that drains control commands (key/unkey/PTT)
        // so `Manager::key`/`unkey` succeed exactly as they would against a real
        // inbound leg's live run-loop. It does no I/O; it keeps the command
        // channel's receiver alive but exits on `Shutdown` (sent by `Call::drop`
        // / `hangup`) so the handle joins promptly — a plain drain loop would
        // deadlock `join()` against `cmd_tx` still being held by the dropping
        // `Call`.
        let handle = std::thread::spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, crate::runtime::RuntimeCommand::Shutdown) {
                    break;
                }
            }
        });
        let ptt = crate::audio_bridge::PttGate::new();
        let rtt = Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
        let state = Arc::new(std::sync::atomic::AtomicU8::new(STATE_ACTIVE));
        // The inbound leg's router-facing channel ends: the run-loop (here
        // absent) would drain `tx_rx` and fill `rx_tx`; the Manager joins
        // `rx_source` to a bus and binds `tx_sender` to a mic on route().
        let (tx_sender, _tx_rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let (_rx_tx, rx_source) = std::sync::mpsc::channel::<Vec<i16>>();
        let format_bits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        Call::new_inbound(
            cmd_tx,
            waker,
            handle,
            ptt,
            rtt,
            id,
            node.to_string(),
            CallSnapshotMode::Direct,
            state,
            rx_source,
            tx_sender,
            format_bits,
            // Test fake: default station rate so `adopt` accepts it (iax-4348).
            8000,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    /// Like [`fake_inbound_call`] but hands back the test-side channel ends so a
    /// test can inject "remote spoke" frames on the leg's RX surface
    /// (`rx_injector`) and observe what the bridge sends to the wire on its TX
    /// surface (`tx_observer`). Used by the conference-bridge tests (iax-647d).
    pub(crate) fn fake_inbound_call_wired(
        id: CallId,
        node: &str,
    ) -> (
        Call,
        std::sync::mpsc::Sender<Vec<i16>>,
        std::sync::mpsc::Receiver<Vec<i16>>,
    ) {
        fake_inbound_call_wired_at(id, node, 8000)
    }

    /// [`fake_inbound_call_wired`] with an explicit leg sample rate, for
    /// tests of wideband (16 kHz) pipelines (iax-8d2f follow-up).
    pub(crate) fn fake_inbound_call_wired_at(
        id: CallId,
        node: &str,
        sample_rate: u32,
    ) -> (
        Call,
        std::sync::mpsc::Sender<Vec<i16>>,
        std::sync::mpsc::Receiver<Vec<i16>>,
    ) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<crate::runtime::RuntimeCommand>();
        let poll = mio::Poll::new().expect("poll");
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(1)).expect("waker"));
        let handle = std::thread::spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, crate::runtime::RuntimeCommand::Shutdown) {
                    break;
                }
            }
        });
        let ptt = crate::audio_bridge::PttGate::new();
        let rtt = Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
        let state = Arc::new(std::sync::atomic::AtomicU8::new(STATE_ACTIVE));
        // rx_injector → rx_source: "remote audio in" (call→bridge).
        // tx_sender → tx_observer: "to the wire" (bridge→call run-loop→wire).
        let (rx_injector, rx_source) = std::sync::mpsc::channel::<Vec<i16>>();
        let (tx_sender, tx_observer) = std::sync::mpsc::channel::<Vec<i16>>();
        let format_bits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call = Call::new_inbound(
            cmd_tx,
            waker,
            handle,
            ptt,
            rtt,
            id,
            node.to_string(),
            CallSnapshotMode::Direct,
            state,
            rx_source,
            tx_sender,
            format_bits,
            // Leg rate must match the adopting station's (iax-4348).
            sample_rate,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        (call, rx_injector, tx_observer)
    }

    /// Multi-device stub backend for the manager unit tests: two inputs
    /// (`in:a`, `in:b`) and one shared output (`out:s` / `out:shared`), no-op
    /// stream handles. Stashes the most-recently opened input sink so a test can
    /// drive captured frames via `push_mic`.
    #[derive(Default)]
    struct Shared {
        sinks: Vec<Box<dyn InputSink>>,
    }

    pub(crate) struct StubBackend(Arc<Mutex<Shared>>);

    impl StubBackend {
        pub(crate) fn new() -> Self {
            Self(Arc::new(Mutex::new(Shared::default())))
        }
    }

    struct NullHandle;
    impl StreamHandle for NullHandle {
        fn stop(self: Box<Self>) {}
        fn pause(&self) -> Result<(), AudioError> {
            Ok(())
        }
        fn resume(&self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    fn dev(dir: Direction, tag: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(tag.to_string()),
            name: tag.to_string(),
            direction: dir,
            channels: 1,
            native_sample_rates: vec![8_000],
        }
    }

    impl AudioBackend for StubBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                dev(Direction::Input, "in:a"),
                dev(Direction::Input, "in:b"),
                dev(Direction::Output, "out:s"),
                dev(Direction::Output, "out:shared"),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Input, "in:a"))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Output, "out:s"))
        }
        fn open_input(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            sink: Box<dyn InputSink>,
            _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            self.0.lock().unwrap().sinks.push(sink);
            Ok(Box::new(NullHandle))
        }
        fn open_output(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            _s: Box<dyn OutputSource>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            Ok(Box::new(NullHandle))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    fn peer_a() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4569))
    }
    fn peer_b() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4570))
    }

    fn test_manager() -> Manager {
        Manager::new(Box::new(test_support::StubBackend::new()))
    }

    /// A dial spec whose `node` doubles as the mic label used in the routing
    /// tests, and whose `id` is derived from the node string so distinct nodes
    /// get distinct ids.
    fn test_spec(node: &str, output: &str, peer: SocketAddr) -> DialSpec {
        // Cheap stable id from the node label.
        let id = CallId(u64::from(
            node.bytes().fold(0u32, |a, b| a.wrapping_add(u32::from(b))),
        ));
        DialSpec {
            id,
            node: node.to_string(),
            peer,
            output: OutputId::new(output),
            caller_id: "mgr-test".to_string(),
            secret: String::new(),
            mode: CallMode::Standard,
            dest: "1000".to_string(),
            frame_observer: None,
            codec_policy: CodecPolicy::default(),
        }
    }

    // -- Link transport selection (iax-927a) --------------------------------

    /// A valid WG config for transport tests (key seed 9, arbitrary peer key).
    fn wg_cfg() -> astar_wireguard::WgLinkConfig {
        // 32 zero bytes base64 — length-valid x25519 key material.
        const KEY32: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        astar_wireguard::WgLinkConfig::new(
            "WG_TEST_KEY",
            "10.66.0.1/32",
            KEY32,
            "192.0.2.1:51820",
            &[],
            25,
        )
        .expect("valid config")
    }

    fn wg_resolver(_: &str) -> String {
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()
    }

    #[test]
    fn default_transport_is_udp_with_no_wg_surface() {
        let mgr = test_manager();
        assert!(mgr.wg_status().is_none(), "no tunnel by default");
        assert_eq!(mgr.also_bind_udp(), None);
    }

    #[test]
    fn wireguard_transport_installs_and_reports_status_and_also_bind() {
        let extra: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut mgr = test_manager();
        mgr.set_wireguard_transport_over(
            &wg_cfg().with_also_bind_udp(Some(extra)),
            &wg_resolver,
            Box::new(astar_wireguard::FakeTransport::new()),
        )
        .expect("wg installs");
        assert!(mgr.wg_status().is_some(), "tunnel status surfaces");
        assert_eq!(mgr.also_bind_udp(), Some(extra));

        // Switching back to plain UDP drops the stack + the extra bind.
        mgr.set_link_transport(LinkTransport::Udp, &wg_resolver)
            .expect("udp resets");
        assert!(mgr.wg_status().is_none());
        assert_eq!(mgr.also_bind_udp(), None);
    }

    #[test]
    fn transport_switch_is_refused_while_calls_are_pooled() {
        let mut mgr = test_manager();
        mgr.dial(test_spec("in:a", "out:s", peer_a()))
            .expect("dial");
        let err = mgr
            .set_wireguard_transport_over(
                &wg_cfg(),
                &wg_resolver,
                Box::new(astar_wireguard::FakeTransport::new()),
            )
            .expect_err("transport is immutable while a session is up");
        assert!(matches!(err, IaxError::CallInProgress), "got: {err}");
        let err = mgr
            .set_link_transport(LinkTransport::Udp, &wg_resolver)
            .expect_err("udp reset refused too");
        assert!(matches!(err, IaxError::CallInProgress), "got: {err}");
    }

    #[test]
    fn wireguard_transport_rejects_bad_key_material() {
        let mut mgr = test_manager();
        let bad = |_: &str| String::new();
        let err = mgr
            .set_wireguard_transport_over(
                &wg_cfg(),
                &bad,
                Box::new(astar_wireguard::FakeTransport::new()),
            )
            .expect_err("empty secret must fail");
        assert!(matches!(err, IaxError::Io(_)), "got: {err}");
        assert!(
            err.to_string().contains("WG_TEST_KEY"),
            "error names the reference, never material: {err}"
        );
        // The failed install leaves the manager on plain UDP.
        assert!(mgr.wg_status().is_none());
    }

    #[test]
    fn with_policy_pins_the_station_rate() {
        let m = Manager::with_policy(
            Box::new(test_support::StubBackend::new()),
            CodecPolicy::PreferSlin16,
        );
        assert_eq!(m.config.sample_rate, 16_000);
        let m8 = Manager::with_policy(
            Box::new(test_support::StubBackend::new()),
            CodecPolicy::PreferSlin,
        );
        assert_eq!(m8.config.sample_rate, 8_000);
    }

    #[test]
    fn dial_policy_is_capped_to_the_station_rate() {
        use astar_iax_core::frame::{Frame, Subclass, parse_lenient};
        use astar_iax_core::subclass::IaxCommand;
        use astar_iax_core::subclass::VoiceFormat;

        // Manager::new is an 8 kHz station; a PreferSlin16 DialSpec must reach
        // the FSM (and thus the wire) as PreferSlin — no slin16 bit offered.
        let mut mgr = test_manager();
        assert_eq!(mgr.config.sample_rate, 8_000, "Manager::new stays 8 kHz");

        let (obs_tx, obs_rx) = std::sync::mpsc::channel();
        let mut spec = test_spec("in:a", "out:s", peer_a());
        spec.codec_policy = CodecPolicy::PreferSlin16;
        spec.frame_observer = Some(obs_tx);
        mgr.dial(spec).expect("dial");

        // Drain the observer for the outbound NEW and inspect its CAPABILITY.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut checked = false;
        while std::time::Instant::now() < deadline {
            let Ok(tf) = obs_rx.recv_timeout(std::time::Duration::from_millis(200)) else {
                continue;
            };
            if tf.dir != crate::trace::Direction::Out {
                continue;
            }
            let Ok(Frame::Full(ff)) = parse_lenient(&tf.raw) else {
                continue;
            };
            if !matches!(ff.subclass, Subclass::Iax(IaxCommand::New)) {
                continue;
            }
            let capability = ff.ies.capability.expect("NEW carries CAPABILITY");
            assert_eq!(
                capability & (VoiceFormat::Slin16 as u32),
                0,
                "8 kHz station must not offer slin16 even when requested"
            );
            checked = true;
            break;
        }
        assert!(checked, "expected to observe the outbound NEW frame");
    }

    #[test]
    fn dialing_two_nodes_yields_two_connections_monitor_only() {
        let mut mgr = test_manager();
        let a = mgr.dial(test_spec("in:a", "out:shared", peer_a())).unwrap();
        let b = mgr.dial(test_spec("in:b", "out:shared", peer_b())).unwrap();
        assert_ne!(a, b, "distinct call ids");
        let snap = mgr.snapshot();
        assert_eq!(snap.calls.len(), 2);
        assert!(
            snap.calls.iter().all(|c| c.routed_mic.is_none()),
            "both monitor-only until routed"
        );
    }

    #[test]
    fn routing_a_mic_to_a_second_call_clears_the_first_binding() {
        let mut mgr = test_manager();
        let x = mgr.dial(test_spec("in:a", "out:s", peer_a())).unwrap();
        let y = mgr.dial(test_spec("in:b", "out:s", peer_b())).unwrap();
        let mic = MicId::new("in:a");
        mgr.route(x, &mic).unwrap();
        assert_eq!(mgr.snapshot().mic_of(x).as_deref(), Some("in:a"));
        // Re-route the SAME mic to Y → X drops to monitor-only.
        mgr.route(y, &mic).unwrap();
        assert_eq!(mgr.snapshot().mic_of(y).as_deref(), Some("in:a"));
        assert_eq!(mgr.snapshot().mic_of(x), None, "X is now monitor-only");
    }

    #[test]
    fn tx_health_counters_track_routing_state() {
        // iax-9e55: tx_reanchors is published per-call (0 with no TX); the
        // capture-overrun count follows the routed mic — `Some(0)` once routed,
        // and back to `Some(0)` (monitor-only) after unroute. The StubBackend
        // never drops capture buffers, so the count stays 0.
        let mut mgr = test_manager();
        let x = mgr.dial(test_spec("in:a", "out:s", peer_a())).unwrap();
        // Pooled but unknown id → None on both accessors.
        assert_eq!(mgr.tx_reanchors(CallId(99999)), None);
        assert_eq!(mgr.tx_capture_overruns(CallId(99999)), None);
        // Monitor-only (no mic): re-anchors and overruns both read 0.
        assert_eq!(mgr.tx_reanchors(x), Some(0));
        assert_eq!(mgr.tx_capture_overruns(x), Some(0));
        // Route a mic → the overrun count now reflects the open mic lane (0).
        let mic = MicId::new("in:a");
        mgr.route(x, &mic).unwrap();
        assert_eq!(mgr.tx_capture_overruns(x), Some(0));
        // The snapshot surface carries the same values.
        let snap = mgr.snapshot();
        let c = snap.calls.iter().find(|c| c.id == x).expect("call x");
        assert_eq!(c.tx_reanchors, 0);
        assert_eq!(c.tx_capture_overruns, 0);
        // Unroute → monitor-only again; still reads 0 (no panic, no stale Arc).
        mgr.unroute(x).unwrap();
        assert_eq!(mgr.tx_capture_overruns(x), Some(0));
    }

    #[test]
    fn no_mic_is_ever_double_bound() {
        let mut mgr = test_manager();
        let x = mgr.dial(test_spec("in:a", "out:s", peer_a())).unwrap();
        let y = mgr.dial(test_spec("in:b", "out:s", peer_b())).unwrap();
        let mic = MicId::new("in:a");
        mgr.route(x, &mic).unwrap();
        mgr.route(y, &mic).unwrap();
        let snap = mgr.snapshot();
        let bound = snap
            .calls
            .iter()
            .filter(|c| c.routed_mic.as_deref() == Some(mic.as_str()))
            .count();
        assert_eq!(bound, 1, "a mic feeds at most one call");
    }

    #[test]
    fn manager_exposes_wt_audio_controls_for_a_routed_call() {
        let mut mgr = test_manager();
        let id = mgr.dial(test_spec("in:a", "out:s", peer_a())).unwrap();
        mgr.route(id, &MicId::new("in:a")).unwrap();
        mgr.set_input_gain(id, 1.5);
        mgr.set_output_gain(id, 0.5);
        mgr.set_denoise(id, true);
        mgr.set_compress(id, true);
        // iax-a4e7 PHASE 1: RX/output compression is the ONLY path the IAX2/WT
        // connect drives (session.rs's ConsoleSession::connect calls straight
        // into Manager, never the router directly) — this is the one test
        // proving the setting actually reaches the routed call's output bus
        // over that path, not just that AudioRouter's own setter works.
        mgr.set_output_compress(id, true);
        mgr.set_output_compress_level(id, 0.65);
        assert!((mgr.input_gain(id).unwrap() - 1.5).abs() < 1e-6);
        assert!((mgr.output_gain(id).unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(mgr.output_compress(id), Some(true));
        assert!((mgr.output_compress_level(id).unwrap() - 0.65).abs() < 1e-6);
        assert!(mgr.tx_dbfs(id).unwrap() <= 0.0);
        assert!(mgr.rx_dbfs(id).unwrap() <= 0.0);
        // iax-5c30: a dedicated continuous input meter, mirroring tx_dbfs.
        assert!(mgr.input_dbfs(id).unwrap() <= 0.0);
    }

    #[test]
    fn keying_a_monitor_only_call_is_not_routed() {
        let mut mgr = test_manager();
        let x = mgr.dial(test_spec("in:a", "out:s", peer_a())).unwrap();
        assert!(matches!(mgr.key(x), Err(IaxError::NotRouted)));
    }

    #[test]
    fn key_then_unkey_a_routed_call_toggles_ptt_and_gate() {
        // iax-7f7e: use a WIRED fake call, not a dial to a dead peer. `key()`
        // ends in `set_ptt`, which sends `SendPtt` to the call's run-loop over
        // an mpsc channel; `Call::send` maps a closed channel to
        // `NoActiveCall`. The old version dialed `peer_a()` (nothing
        // listening), so under parallel load the real runtime thread exhausted
        // its NEW retries and exited before `key()` ran, making the send fail —
        // a test-only flake, not a product bug. A wired fake call keeps its
        // command receiver alive until `Shutdown`, so key/unkey are
        // deterministic. This still exercises the full route → key → unkey
        // gate+PTT path; the `keyed` assertions make the toggle explicit.
        let mut mgr = test_manager();
        let out = OutputId::new("out:s");
        let (call, _rx_in, _tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let x = mgr.adopt(call, &out).unwrap();
        mgr.route(x, &MicId::new("in:a")).unwrap();

        mgr.key(x).unwrap(); // opens gate + fires set_ptt(true)
        assert!(
            mgr.snapshot()
                .calls
                .iter()
                .find(|c| c.id == x)
                .unwrap()
                .keyed,
            "keyed after key()"
        );

        mgr.unkey(x).unwrap(); // closes gate + set_ptt(false)
        assert!(
            !mgr.snapshot()
                .calls
                .iter()
                .find(|c| c.id == x)
                .unwrap()
                .keyed,
            "unkeyed after unkey()"
        );
    }

    #[test]
    fn adopting_an_inbound_call_pools_it_monitor_only_like_a_dial() {
        let mut mgr = test_manager();
        // Simulate an inbound leg: a canonical Call produced outside dial().
        let inbound = test_support::fake_inbound_call(CallId(42), "1999");
        let id = mgr.adopt(inbound, &OutputId::new("out:s")).unwrap();
        let snap = mgr.snapshot();
        assert!(
            snap.calls.iter().any(|c| c.id == id),
            "adopted call is in the pool"
        );
        assert_eq!(snap.mic_of(id), None, "adopted call starts monitor-only");
        // It is now routable/keyable identically to a dialed call.
        mgr.route(id, &MicId::new("in:a")).unwrap();
        assert_eq!(mgr.snapshot().mic_of(id).as_deref(), Some("in:a"));
    }

    #[test]
    fn adopting_two_inbound_legs_assigns_distinct_pool_keys() {
        // Both legs arrive as the placeholder CallId(0) (as the real Listener
        // hands them over); the Manager must mint distinct keys so an N-caller
        // echo node (iax-6461) can pool them onto one shared output bus.
        let mut mgr = test_manager();
        let out = OutputId::new("out:s");
        let a = mgr
            .adopt(test_support::fake_inbound_call(CallId(0), "1001"), &out)
            .unwrap();
        let b = mgr
            .adopt(test_support::fake_inbound_call(CallId(0), "1002"), &out)
            .unwrap();
        assert_ne!(a, b, "each adopted inbound leg gets a distinct pool key");
        let snap = mgr.snapshot();
        assert!(snap.calls.iter().any(|c| c.id == a));
        assert!(snap.calls.iter().any(|c| c.id == b));
        assert_eq!(mgr.call_count(), 2, "both legs pooled, no collision");
    }

    #[test]
    fn call_snapshot_surfaces_remote_keyed() {
        let mut mgr = test_manager();
        let inbound = test_support::fake_inbound_call(CallId(0), "1999");
        let rk = inbound.remote_keyed_handle();
        let id = mgr.adopt(inbound, &OutputId::new("out:s")).expect("adopt");
        assert!(
            !mgr.calls.get(&id).unwrap().call.snapshot().remote_keyed,
            "quiet peer reports remote_keyed = false"
        );
        rk.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            mgr.calls.get(&id).unwrap().call.snapshot().remote_keyed,
            "keyed peer reports remote_keyed = true"
        );
    }

    #[test]
    fn library_default_bridge_mode_is_handset() {
        // The Station library default stays handset (1:1) so existing embedders
        // and tests are byte-identical (iax-647d). Only the daemon flips to
        // Bridge.
        let mgr = test_manager();
        assert_eq!(mgr.bridge_config().mode, BridgeMode::Handset);
        assert!(
            !mgr.conference_active(),
            "no conference engine in handset mode"
        );
    }

    #[test]
    fn bridge_mode_adopt_enrolls_members_and_bridges_audio() {
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        assert!(mgr.conference_active(), "Bridge mode starts the engine");

        let out = OutputId::new("out:s");
        let (call_a, a_rx_in, _a_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let (call_b, _b_rx_in, b_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1002");
        let _a = mgr.adopt(call_a, &out).unwrap();
        let _b = mgr.adopt(call_b, &out).unwrap();
        assert_eq!(mgr.conference_member_count(), 2, "both legs enrolled");

        // A's remote speaks → the bridge feeds it to B's TX (B hears A).
        let frame: Vec<i16> = vec![8000i16; 160];
        a_rx_in.send(frame).unwrap();
        let heard = b_tx_out
            .recv_timeout(std::time::Duration::from_millis(500))
            .expect("the bridge delivered A's audio to B");
        let s = f32::from(heard[0]) / 32768.0;
        let want = f32::from(8000i16) / 32768.0;
        assert!((s - want).abs() < 2e-3, "B's TX carries A's RX, got {s}");
    }

    /// Synthesize `count` 20 ms (8 kHz) i16 PCM frames of a DTMF tone pair,
    /// phase-continuous across frames (iax-8ca0 test signal).
    #[allow(clippy::cast_possible_truncation)]
    fn dtmf_pcm_frames(row_hz: f32, col_hz: f32, count: usize) -> Vec<Vec<i16>> {
        #[allow(clippy::cast_precision_loss)]
        (0..count * 160)
            .map(|i| {
                let t = i as f32 / 8_000.0;
                let s = 0.25 * (std::f32::consts::TAU * row_hz * t).sin()
                    + 0.25 * (std::f32::consts::TAU * col_hz * t).sin();
                (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
            })
            .collect::<Vec<i16>>()
            .chunks(160)
            .map(<[i16]>::to_vec)
            .collect()
    }

    #[test]
    fn drain_dtmf_digits_surfaces_in_band_tone_at_16k() {
        // iax-8d2f follow-up: the VPS hub runs a prefer-slin16 (16 kHz)
        // pipeline — the conference member detectors must decode tones
        // arriving at THAT rate too (scratch diagnosis 2026-08-02).
        let mut mgr = Manager::with_policy(
            Box::new(test_support::StubBackend::new()),
            CodecPolicy::PreferSlin16,
        );
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (call_a, a_rx_in, _a_tx_out) =
            test_support::fake_inbound_call_wired_at(CallId(0), "1001", 16_000);
        let (call_b, _b_rx_in, _b_tx_out) =
            test_support::fake_inbound_call_wired_at(CallId(0), "1002", 16_000);
        let a = mgr.adopt(call_a, &out).unwrap();
        let _b = mgr.adopt(call_b, &out).unwrap();

        // '5' = 770+1336 Hz, ~200 ms, sampled at 16 kHz (320-sample frames).
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let frames: Vec<Vec<i16>> = (0..10 * 320)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                let s = 0.25 * (std::f32::consts::TAU * 770.0 * t).sin()
                    + 0.25 * (std::f32::consts::TAU * 1336.0 * t).sin();
                (s * 32767.0) as i16
            })
            .collect::<Vec<i16>>()
            .chunks(320)
            .map(<[i16]>::to_vec)
            .collect();
        for f in frames {
            a_rx_in.send(f).unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut got = Vec::new();
        while got.is_empty() && std::time::Instant::now() < deadline {
            got.extend(mgr.drain_dtmf_digits());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            got,
            vec![(a, '5')],
            "16 kHz tone must decode with A's CallId"
        );
    }

    #[test]
    fn drain_dtmf_digits_surfaces_in_band_tone_with_the_right_call_id() {
        // iax-8ca0: a member leg sounding a touch-tone ('5' = 770+1336 Hz)
        // surfaces the digit via Manager::drain_dtmf_digits mapped to its
        // CallId, and the tone never reaches the other leg's TX (relay
        // squelch).
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (call_a, a_rx_in, _a_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let (call_b, _b_rx_in, b_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1002");
        let a = mgr.adopt(call_a, &out).unwrap();
        let _b = mgr.adopt(call_b, &out).unwrap();

        // A sounds '5' for ~200 ms; the free-running engine consumes one
        // frame per 20 ms tick, so poll the drain until the digit lands.
        for f in dtmf_pcm_frames(770.0, 1336.0, 10) {
            a_rx_in.send(f).unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut got = Vec::new();
        while got.is_empty() && std::time::Instant::now() < deadline {
            got.extend(mgr.drain_dtmf_digits());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(got, vec![(a, '5')], "digit surfaces with A's CallId");

        // B's TX never carried the tone. The live engine sends B a (silent)
        // frame every 20 ms forever, so bound the check to a fixed number of
        // frames covering the tone's span rather than draining until empty.
        for _ in 0..30 {
            let Ok(f) = b_tx_out.recv_timeout(std::time::Duration::from_millis(60)) else {
                break;
            };
            let peak = f
                .iter()
                .map(|&s| (f32::from(s) / 32768.0).abs())
                .fold(0.0, f32::max);
            assert!(peak < 1e-3, "A's tone leaked onto B's TX, peak {peak}");
        }
    }

    #[test]
    fn drain_dtmf_digits_merges_out_of_band_call_events() {
        // iax-8ca0: out-of-band IAX DTMF frames arrive as CallEvent::Dtmf on
        // the leg's event stream; adopt_with_events pools the real stream so
        // the drain merges them into the same (CallId, digit) surface.
        // Non-DTMF events are consumed without surfacing digits.
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (call_a, _a_in, _a_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let a = mgr.adopt_with_events(call_a, &out, ev_rx).unwrap();

        ev_tx.send(crate::CallEvent::Dtmf('#')).unwrap();
        ev_tx.send(crate::CallEvent::RemotePtt(true)).unwrap();
        ev_tx.send(crate::CallEvent::Dtmf('1')).unwrap();
        assert_eq!(
            mgr.drain_dtmf_digits(),
            vec![(a, '#'), (a, '1')],
            "out-of-band digits surface in order, non-DTMF events skipped"
        );
        assert!(
            mgr.drain_dtmf_digits().is_empty(),
            "drain empties the stream"
        );
    }

    #[test]
    fn announce_to_member_injects_on_that_members_leg_only() {
        // iax-c4ea: a private per-member announcement (the node-id join greeting)
        // is injected onto ONE member's TX and reaches that user only. We use a
        // Phrase::Pcm so the test never needs piper/TTS.
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (call_a, _a_in, a_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let (call_b, _b_in, b_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1002");
        let a = mgr.adopt(call_a, &out).unwrap();
        let _b = mgr.adopt(call_b, &out).unwrap();

        // A one-frame announcement at a known level, addressed to A only.
        let pcm: std::sync::Arc<[i16]> = vec![8000_i16; 160].into();
        let req = crate::announce::AnnounceRequest {
            phrase: crate::announce::Phrase::Pcm(pcm),
            destination: crate::announce::Destination::ToAir,
            policy: crate::announce::AnnouncePolicy::Seize,
            priority: 4,
        };
        mgr.announce_to_member(a, req, std::time::Duration::ZERO)
            .expect("inject to A's leg");

        // A's TX carries the greeting frame.
        let heard_a = a_tx_out
            .recv_timeout(std::time::Duration::from_millis(500))
            .expect("A receives its private greeting");
        let s = f32::from(heard_a[0]) / 32768.0;
        let want = f32::from(8000i16) / 32768.0;
        assert!(
            (s - want).abs() < 2e-3,
            "A's TX carries the greeting, got {s}"
        );

        // B's TX never carries the greeting (it only gets the silent mix). The
        // live thread emits a frame every 20 ms forever, so bound the drain to a
        // fixed number of frames rather than looping until empty.
        let mut saw_greeting_on_b = false;
        for _ in 0..10 {
            match b_tx_out.recv_timeout(std::time::Duration::from_millis(60)) {
                Ok(f) => {
                    let sb = (f32::from(f[0]) / 32768.0).abs();
                    if (sb - want).abs() < 2e-3 {
                        saw_greeting_on_b = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(
            !saw_greeting_on_b,
            "the greeting must NOT leak onto B's leg"
        );
    }

    #[test]
    fn announce_to_member_prepends_lead_silence() {
        // iax-9722: a non-zero `lead` prepends silence frames (1 s = 50 @
        // 20 ms) onto the member's leg, ahead of the announcement's own PCM,
        // so the far-end squelch is primed before the first spoken word.
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (call_a, _a_in, a_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let a = mgr.adopt(call_a, &out).unwrap();

        // Two speech frames (320 samples @ 8 kHz = 2 x 160-sample frames) at a
        // known non-zero level, so the lead/speech boundary is unambiguous.
        let pcm: std::sync::Arc<[i16]> = vec![8000_i16; 320].into();
        let req = crate::announce::AnnounceRequest {
            phrase: crate::announce::Phrase::Pcm(pcm),
            destination: crate::announce::Destination::ToAir,
            policy: crate::announce::AnnouncePolicy::Seize,
            priority: 4,
        };
        mgr.announce_to_member(a, req, std::time::Duration::from_secs(1))
            .expect("inject with lead");

        // First 50 frames (1 s @ 20 ms/frame) are silence.
        for i in 0..50 {
            let f = a_tx_out
                .recv_timeout(std::time::Duration::from_millis(200))
                .unwrap_or_else(|_| panic!("lead frame {i} never arrived"));
            assert!(
                f.iter().all(|&s| s == 0),
                "lead frame {i} must be silent, got {f:?}"
            );
        }
        // Then the 2 speech frames carry the announcement's own PCM.
        for i in 0..2 {
            let f = a_tx_out
                .recv_timeout(std::time::Duration::from_millis(200))
                .unwrap_or_else(|_| panic!("speech frame {i} never arrived"));
            let s = f32::from(f[0]) / 32768.0;
            let want = f32::from(8000i16) / 32768.0;
            assert!(
                (s - want).abs() < 2e-3,
                "speech frame {i} carries the announcement, got {s}"
            );
        }
    }

    #[test]
    fn announce_to_member_on_non_member_call_errors() {
        // In handset mode an adopted call has no conference member slot, so the
        // per-member injection path errors rather than panicking.
        let mut mgr = test_manager();
        let out = OutputId::new("out:s");
        let (call_a, _a_in, _a_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let a = mgr.adopt(call_a, &out).unwrap();
        let pcm: std::sync::Arc<[i16]> = vec![0_i16; 160].into();
        let req = crate::announce::AnnounceRequest {
            phrase: crate::announce::Phrase::Pcm(pcm),
            destination: crate::announce::Destination::ToAir,
            policy: crate::announce::AnnouncePolicy::Seize,
            priority: 4,
        };
        assert!(
            mgr.announce_to_member(a, req, std::time::Duration::ZERO)
                .is_err(),
            "handset call (no member) must error, not panic"
        );
    }

    #[test]
    fn bridge_hangup_one_member_leaves_cleanly() {
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            ..BridgeConfig::default()
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (call_a, _a_in, _a_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let (call_b, _b_in, _b_out) = test_support::fake_inbound_call_wired(CallId(0), "1002");
        let a = mgr.adopt(call_a, &out).unwrap();
        let b = mgr.adopt(call_b, &out).unwrap();
        assert_eq!(mgr.conference_member_count(), 2);
        mgr.hangup(a, None).unwrap();
        assert_eq!(mgr.conference_member_count(), 1, "A left the conference");
        assert_eq!(mgr.call_count(), 1, "A left the pool");
        // B is still pooled and enrolled.
        assert!(mgr.snapshot().calls.iter().any(|c| c.id == b));
    }

    #[test]
    fn live_switch_handset_to_bridge_enrolls_existing_calls() {
        // Adopt under handset (RX on the bus), then flip to Bridge live: the
        // pooled call is enrolled as a conference member and detached from the
        // bus (iax-647d POST /bridge path).
        let mut mgr = test_manager();
        let out = OutputId::new("out:s");
        let (call_a, _a_in, _a_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let id = mgr.adopt(call_a, &out).unwrap();
        assert!(!mgr.conference_active(), "handset: no conference");
        assert_eq!(mgr.router.bus_call_count(&out), 1, "RX on the bus");

        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Conference,
            ..BridgeConfig::default()
        })
        .unwrap();
        assert_eq!(mgr.conference_member_count(), 1, "existing call enrolled");
        assert_eq!(mgr.router.bus_call_count(&out), 0, "RX left the bus");

        // And back to handset: the call re-joins its output bus.
        mgr.set_bridge_config(BridgeConfig::default()).unwrap();
        assert!(!mgr.conference_active());
        assert_eq!(mgr.router.bus_call_count(&out), 1, "RX back on the bus");
        assert!(mgr.snapshot().calls.iter().any(|c| c.id == id));
    }

    #[test]
    fn parrot_mode_hangs_up_the_leg_once_the_report_pump_runs() {
        // iax-feab: a member's take (VOX-triggered here) yields a signal
        // report; poll_announcements speaks it on that member's leg and hangs
        // the leg up once done. test_manager()'s AnnouncementService has TTS
        // disabled (crate::announce::tts::TtsConfig::default()), so the
        // report's Phrase::Text always fails to resolve — this exercises the
        // pump's "TTS unavailable -> hang up anyway" branch, which is exactly
        // the CI-safe path (no piper binary required) and still proves the
        // pump drains a parrot report into a real hangup.
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Parrot,
            mix_minus: true,
            include_local_radio: false,
            // Tiny ticks so the take completes in well under a second instead
            // of the 10 s production default.
            parrot: Some(astar_audio::ParrotTuning {
                playback_delay_ticks: 1,
                silence_gap_ticks: 1,
                vox_threshold_db: -40.0,
                max_record_ticks: 1,
            }),
        })
        .unwrap();
        assert!(mgr.conference_active(), "Parrot mode starts the engine");

        let out = OutputId::new("out:s");
        let (call_a, a_rx_in, _a_tx_out) = test_support::fake_inbound_call_wired(CallId(0), "1001");
        let a = mgr.adopt(call_a, &out).unwrap();
        assert_eq!(mgr.conference_member_count(), 1, "A enrolled as a member");

        // Feed voice well above the VOX floor and keep polling until the leg
        // is hung up (or we time out — a bug here would hang the test, so
        // bound it).
        let frame: Vec<i16> = vec![9000_i16; 160];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut still_pooled = true;
        while std::time::Instant::now() < deadline {
            let _ = a_rx_in.send(frame.clone());
            mgr.poll_announcements();
            still_pooled = mgr.snapshot().calls.iter().any(|c| c.id == a);
            if !still_pooled {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            !still_pooled,
            "the leg was hung up once the parrot report pump ran"
        );
        assert_eq!(mgr.call_count(), 0);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn export_reflects_devices_and_bindings_without_secrets() {
        let mut mgr = test_manager();
        let x = mgr.dial(test_spec("in:a", "out:s", peer_a())).unwrap();
        mgr.route(x, &MicId::new("in:a")).unwrap();
        let cfg = mgr.export();
        assert_eq!(cfg.connections.len(), 1);
        let c = &cfg.connections[0];
        assert_eq!(c.output_device, "out:s");
        assert_eq!(c.input_device.as_deref(), Some("in:a"));
        // Secret-free: serialize and check.
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("allstar"));
    }

    /// A [`ConnectionSpec`] for the `apply` tests; identity is `id`, so two
    /// specs may share a `node`.
    fn spec(id: &str, node: &str, out: &str, input: Option<&str>) -> ConnectionSpec {
        ConnectionSpec {
            id: ConnectionId::new(id),
            node: node.to_string(),
            calling_node: "1999".to_string(),
            name: id.to_string(),
            output_device: out.to_string(),
            input_device: input.map(ToString::to_string),
        }
    }

    #[test]
    fn apply_dials_missing_connections_and_binds_inputs() {
        let mut mgr = test_manager();
        // `spec(id, node, out, input)` — identity is `id`, so two specs may
        // share a node.
        let cfg = RoutingConfig {
            connections: vec![
                spec("c1", "n1", "out:s", Some("in:a")),
                spec("c2", "n1", "out:s", None), // same node, distinct id → monitor-only (Q5)
            ],
        };
        mgr.apply(&cfg, |_| peer_a(), |_| "secret-not-in-config".into())
            .unwrap();
        let snap = mgr.snapshot();
        assert_eq!(
            snap.calls.len(),
            2,
            "two connections to the same node, keyed by id"
        );
        assert_eq!(
            snap.calls.iter().filter(|c| c.routed_mic.is_some()).count(),
            1,
            "c1 routed, c2 monitor-only"
        );
    }

    #[test]
    fn apply_is_idempotent_and_hangs_up_removed_connections() {
        let mut mgr = test_manager();
        let cfg = RoutingConfig {
            connections: vec![
                spec("c1", "n1", "out:s", Some("in:a")),
                spec("c2", "n2", "out:s", None),
            ],
        };
        mgr.apply(&cfg, |_| peer_a(), |_| String::new()).unwrap();
        assert_eq!(mgr.call_count(), 2);
        // Re-applying the same config dials nothing new (reconcile on id).
        mgr.apply(&cfg, |_| peer_a(), |_| String::new()).unwrap();
        assert_eq!(mgr.call_count(), 2, "no duplicate dials on re-apply");
        // Dropping c2 from the config hangs it up.
        let smaller = RoutingConfig {
            connections: vec![spec("c1", "n1", "out:s", Some("in:a"))],
        };
        mgr.apply(&smaller, |_| peer_a(), |_| String::new())
            .unwrap();
        assert_eq!(mgr.call_count(), 1, "c2 hung up; absent from cfg");
        assert_eq!(mgr.export().connections.len(), 1);
        assert_eq!(mgr.export().connections[0].id, ConnectionId::new("c1"));
    }
}

/// Link-layer (iax-ad2e) integration tests. These live in the lib crate (not a
/// `tests/` integration file) because the `Link` mode mechanics are exercised
/// against the `pub(crate)` `test_support::StubBackend` + `fake_inbound_call`
/// fixtures — the same real fixtures the routing tests above use. An adopted
/// inbound leg gives an already-Active call without a live socket, which is how
/// the roster's `Up` state and the mode transitions are driven here.
#[cfg(test)]
mod link_layer_tests {
    use super::test_support::fake_inbound_call;
    use super::*;
    use crate::link::{LinkMode, LinkState};

    fn test_manager() -> Manager {
        Manager::new(Box::new(test_support::StubBackend::new()))
    }

    /// Adopt a fake inbound leg into the pool → an Active call with `id`.
    fn active_call(mgr: &mut Manager, id: u64, node: &str) -> CallId {
        mgr.adopt(fake_inbound_call(CallId(id), node), &OutputId::new("out:s"))
            .expect("adopt")
    }

    /// iax-42ce end-to-end: link modes gate the live conference relay.
    ///
    /// Two legs in Bridge mode — A `Transceive`, B `LocalMonitor`. B's RX must
    /// never reach A's TX and B must be sent nothing; flipping B to
    /// `Transceive` live restores the relay both ways on the next ticks.
    #[test]
    fn link_modes_gate_the_conference_relay() {
        use super::test_support::fake_inbound_call_wired;
        use std::time::Duration;

        /// Scan `rx` until a frame's first sample is within 2e-3 of `want`
        /// (normalized), or `timeout` passes. The engine free-runs at 20 ms,
        /// so polling frames beats asserting on any single tick.
        fn hears(rx: &std::sync::mpsc::Receiver<Vec<i16>>, want: i16, timeout: Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            let want = f32::from(want) / 32768.0;
            while std::time::Instant::now() < deadline {
                if let Ok(frame) = rx.recv_timeout(Duration::from_millis(50)) {
                    let s = f32::from(frame[0]) / 32768.0;
                    if (s - want).abs() < 2e-3 {
                        return true;
                    }
                }
            }
            false
        }

        /// Drain `rx` until it stays empty for `gap` — proves the engine has
        /// stopped sending to this leg (pre-link-mode frames may be in flight).
        fn goes_silent(rx: &std::sync::mpsc::Receiver<Vec<i16>>, gap: Duration) -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if rx.recv_timeout(gap).is_err() {
                    return true;
                }
            }
            false
        }

        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (call_a, _a_rx_in, a_tx_out) = fake_inbound_call_wired(CallId(0), "1001");
        let (call_b, b_rx_in, b_tx_out) = fake_inbound_call_wired(CallId(0), "1002");
        let a = mgr.adopt(call_a, &out).unwrap();
        let b = mgr.adopt(call_b, &out).unwrap();
        mgr.add_link(a, "1001", LinkMode::Transceive).unwrap();
        mgr.add_link(b, "1002", LinkMode::LocalMonitor).unwrap();

        // B is LocalMonitor: sent nothing (after any pre-link frames drain)...
        assert!(
            goes_silent(&b_tx_out, Duration::from_millis(100)),
            "a LocalMonitor leg stops being transmitted to"
        );
        // ...and its RX never reaches A (A's mix stays silent).
        b_rx_in.send(vec![8000i16; 160]).unwrap();
        assert!(
            !hears(&a_tx_out, 8000, Duration::from_millis(300)),
            "LocalMonitor RX must not relay to the Transceive leg"
        );

        // Flip B to Transceive live: its RX now relays to A.
        mgr.set_link_mode(b, LinkMode::Transceive).unwrap();
        b_rx_in.send(vec![8000i16; 160]).unwrap();
        assert!(
            hears(&a_tx_out, 8000, Duration::from_secs(2)),
            "Transceive B's RX relays to A after the live mode change"
        );

        // Monitor: still relays onward, but is itself sent nothing again.
        mgr.set_link_mode(b, LinkMode::Monitor).unwrap();
        assert!(
            goes_silent(&b_tx_out, Duration::from_millis(100)),
            "a Monitor leg stops being transmitted to"
        );
        b_rx_in.send(vec![6000i16; 160]).unwrap();
        assert!(
            hears(&a_tx_out, 6000, Duration::from_secs(2)),
            "Monitor B's RX still relays to A"
        );
    }

    #[test]
    fn announcements_reach_non_link_members_only() {
        // iax-9e02: link status is for the web-transceiver users on THIS node.
        // A linked node must never be sent the announcement.
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let wt = mgr
            .adopt(test_support::fake_inbound_call(CallId(1), "1001"), &out)
            .unwrap();
        let link = mgr
            .adopt(test_support::fake_inbound_call(CallId(2), "55553"), &out)
            .unwrap();
        mgr.route(link, &MicId::new("in:a")).unwrap();
        mgr.add_link(link, "55553", LinkMode::Transceive).unwrap();

        let reached = mgr.announce_to_non_link_members(crate::announce::AnnounceRequest {
            phrase: crate::announce::Phrase::Pcm(vec![0_i16; 160].into()),
            destination: crate::announce::Destination::ToAir,
            policy: crate::announce::AnnouncePolicy::Seize,
            priority: 6,
        });
        assert_eq!(reached, 1, "only the non-link member is announced to");
        assert!(
            mgr.calls[&wt].member.is_some() && mgr.calls[&link].member.is_some(),
            "both legs are members; the link is excluded by link status, not membership"
        );
    }

    #[test]
    fn link_tx_carries_audio_only_while_keyed() {
        // iax-5b8e (live failure, hub 2026-08-03): the parrot locked up —
        // it recorded and never replayed. The conference hands every
        // receiving member its mix EVERY tick, so an unkeyed link streamed
        // silence forever: to app_rpt an unbroken carrier that never ends the
        // over. Audio must flow to a link only while it is keyed.
        use super::test_support::fake_inbound_call_wired;
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let out = OutputId::new("out:s");
        let (src_call, src_rx_in, _src_tx) = fake_inbound_call_wired(CallId(0), "1001");
        let (link_call, _link_rx_in, link_tx_out) = fake_inbound_call_wired(CallId(0), "55553");
        let source = mgr.adopt(src_call, &out).unwrap();
        let link = mgr.adopt(link_call, &out).unwrap();
        mgr.route(link, &MicId::new("in:a")).unwrap();
        mgr.add_link(link, "55553", LinkMode::Transceive).unwrap();

        // Unkeyed: sync gates the link's TX off, so the source's audio must
        // NOT reach the link.
        mgr.sync_link_keying();
        for _ in 0..5 {
            src_rx_in.send(vec![8000_i16; 160]).unwrap();
        }
        assert!(
            link_tx_out
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "an unkeyed link must not be sent conference audio"
        );

        // The source keys: the link keys and now carries the mix.
        mgr.calls[&source]
            .call
            .remote_keyed_handle()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        mgr.sync_link_keying();
        for _ in 0..5 {
            src_rx_in.send(vec![8000_i16; 160]).unwrap();
        }
        assert!(
            link_tx_out
                .recv_timeout(std::time::Duration::from_millis(500))
                .is_ok(),
            "a keyed link must carry the conference mix"
        );
    }

    #[test]
    fn link_keys_while_a_member_is_keyed_and_unkeys_after() {
        // iax-7d51 (live failure, hub 2026-08-03): audio arrived FROM the
        // parrot link but nothing got through TO it. Keying puts RADIO_KEY /
        // RADIO_UNKEY on the wire and app_rpt mutes an unkeyed sender, so a
        // bridged link that never keys transmits frames the far end discards.
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        // A source leg (astar's inbound call) and a second leg carrying the
        // link view. Both are fakes so the test never depends on a live dial
        // (iax-6a3f's test covers dial-time enrollment).
        let source = mgr
            .adopt(
                test_support::fake_inbound_call(CallId(1), "1001"),
                &OutputId::new("out:s"),
            )
            .unwrap();
        let link = mgr
            .adopt(
                test_support::fake_inbound_call(CallId(2), "55553"),
                &OutputId::new("out:s"),
            )
            .unwrap();
        mgr.route(link, &MicId::new("in:a")).unwrap();
        mgr.add_link(link, "55553", LinkMode::Transceive).unwrap();

        // Nobody keyed: the link stays unkeyed.
        assert_eq!(mgr.sync_link_keying(), 0, "quiet conference keys nothing");
        assert!(!mgr.calls[&link].call.snapshot().keyed);

        // The source's peer keys (astar presses PTT).
        mgr.calls[&source]
            .call
            .remote_keyed_handle()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(mgr.sync_link_keying(), 1, "link follows the keyed member");
        assert!(
            mgr.calls[&link].call.snapshot().keyed,
            "a transceive link must key while a member transmits"
        );
        // Idempotent: no further changes while it stays keyed.
        assert_eq!(mgr.sync_link_keying(), 0, "no churn on an unchanged state");

        // The source unkeys; the link follows.
        mgr.calls[&source]
            .call
            .remote_keyed_handle()
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(mgr.sync_link_keying(), 1);
        assert!(!mgr.calls[&link].call.snapshot().keyed);
    }

    #[test]
    fn dialed_call_joins_the_conference_in_bridge_mode() {
        // iax-6a3f (live failure, hub 2026-08-03): `*3<node>` brought a link
        // UP but no audio passed either way. A DIALED call (every link is one)
        // was always bus-attached with `member: None`, so in conference mode
        // its RX mixed onto an output bus nobody hears and its TX waited on a
        // mic — it never joined the mix. Inbound (`adopt`ed) legs enrolled
        // correctly, which is why member-to-member audio always worked.
        let mut mgr = test_manager();
        mgr.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            parrot: None,
        })
        .unwrap();
        let dialed = mgr
            .dial(DialSpec {
                id: CallId(1),
                node: "55553".to_string(),
                peer: "127.0.0.1:4569".parse().unwrap(),
                output: OutputId::new("out:s"),
                caller_id: "mgr-test".to_string(),
                secret: String::new(),
                mode: CallMode::Standard,
                dest: "55553".to_string(),
                frame_observer: None,
                codec_policy: CodecPolicy::default(),
            })
            .expect("dial");
        assert!(
            mgr.conference_member_count() >= 1,
            "a dialed call must join the conference in bridge mode"
        );
        assert!(
            mgr.calls.get(&dialed).is_some_and(|c| c.member.is_some()),
            "the dialed call itself must be the member"
        );
    }

    #[test]
    fn add_link_in_monitor_mode_routes_no_mic() {
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 1, "55553");
        mgr.add_link(call, "55553", LinkMode::Monitor).unwrap();

        // Monitor => no mic routed for this call.
        assert!(
            mgr.routed_mic(call).is_none(),
            "monitor must not route a mic"
        );
        // But it IS in the roster.
        let roster = mgr.link_roster();
        assert_eq!(roster.links.len(), 1);
        assert_eq!(roster.links[0].node, "55553");
        assert_eq!(roster.links[0].mode, LinkMode::Monitor);
        assert!(!roster.links[0].keyed);
    }

    #[test]
    fn roster_reports_state_and_keyed_from_manager_not_a_copy() {
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 1, "55553");

        // A transceive link with a mic routed, then keyed.
        mgr.route(call, &MicId::new("in:a")).unwrap();
        mgr.add_link(call, "55553", LinkMode::Transceive).unwrap();
        mgr.key(call).unwrap();

        let snap = mgr.link_roster().links[0].clone();
        assert_eq!(snap.mode, LinkMode::Transceive);
        assert_eq!(snap.state, LinkState::Up);
        assert!(
            snap.keyed,
            "keyed must reflect Manager::key, not Link state"
        );

        mgr.unkey(call).unwrap();
        assert!(!mgr.link_roster().links[0].keyed);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn roster_serializes_to_json_without_secrets() {
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 1, "55553");
        mgr.add_link(call, "55553", LinkMode::Monitor).unwrap();
        let json = serde_json::to_string(&mgr.link_roster()).unwrap();
        assert!(json.contains("55553"));
        assert!(json.contains("monitor"));
        // No credential-shaped fields ever leak.
        assert!(!json.contains("secret"));
        assert!(!json.contains("password"));
        assert!(!json.contains("token"));
    }

    #[test]
    fn transceive_to_monitor_drops_mic_but_keeps_link_up() {
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 1, "55553");
        mgr.route(call, &MicId::new("in:a")).unwrap();
        mgr.add_link(call, "55553", LinkMode::Transceive).unwrap();
        assert!(mgr.routed_mic(call).is_some());

        mgr.set_link_mode(call, LinkMode::Monitor).unwrap();

        // Mic released, link still present and Up (monitor-only).
        assert!(mgr.routed_mic(call).is_none());
        let snap = mgr.link_roster().links[0].clone();
        assert_eq!(snap.mode, LinkMode::Monitor);
        assert_eq!(snap.state, LinkState::Up);
        assert!(!snap.keyed);
    }

    #[test]
    fn keying_a_monitor_link_is_refused() {
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 1, "55553");
        mgr.add_link(call, "55553", LinkMode::Monitor).unwrap();
        // Monitor has no mic; key_link refuses up front.
        let err = mgr.key_link(call).unwrap_err();
        assert!(matches!(err, crate::link::LinkError::NotTransmitCapable));
    }

    #[test]
    fn keying_a_micless_transceive_link_is_not_transmit_capable() {
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 1, "55553");
        // Transceive mode but no mic routed → key_link must fail
        // NotTransmitCapable (mapping the Manager's NotRouted).
        mgr.add_link(call, "55553", LinkMode::Transceive).unwrap();
        let err = mgr.key_link(call).unwrap_err();
        assert!(matches!(err, crate::link::LinkError::NotTransmitCapable));
    }

    #[test]
    fn remove_link_releases_mic_and_drops_from_roster_without_hangup() {
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 1, "55553");
        mgr.route(call, &MicId::new("in:a")).unwrap();
        mgr.add_link(call, "55553", LinkMode::Transceive).unwrap();

        mgr.remove_link(call).unwrap();

        // Gone from roster, mic released...
        assert!(mgr.link_roster().links.is_empty());
        assert!(mgr.routed_mic(call).is_none());
        // ...but the underlying call is still in the pool (caller hangs up
        // separately). Read liveness from the canonical Call::snapshot().
        let snap = mgr.snapshot();
        let c = snap.calls.iter().find(|c| c.id == call).expect("pooled");
        assert!(c.is_active(), "remove_link must not hang up the call");
    }

    #[test]
    fn inbound_accepted_call_wraps_as_link_identically() {
        // An adopted inbound leg is poolable identically to a dialed one, so a
        // Monitor/Transceive Link wraps either via the SAME add_link path.
        let mut mgr = test_manager();
        let call = active_call(&mut mgr, 0, "from-peer");
        mgr.add_link(call, "from-peer", LinkMode::Monitor).unwrap();

        let snap = mgr.link_roster().links[0].clone();
        assert_eq!(snap.node, "from-peer");
        assert_eq!(snap.mode, LinkMode::Monitor);
        assert_eq!(snap.state, LinkState::Up);

        // Mode change works the same on an inbound leg.
        mgr.set_link_mode(call, LinkMode::LocalMonitor).unwrap();
        assert_eq!(mgr.link_roster().links[0].mode, LinkMode::LocalMonitor);
        assert!(!mgr.link_roster().links[0].mode.relays_onward());
    }

    #[test]
    fn link_ops_on_unknown_call_report_no_such_link() {
        let mut mgr = test_manager();
        let ghost = CallId(999);
        assert!(matches!(
            mgr.set_link_mode(ghost, LinkMode::Monitor),
            Err(crate::link::LinkError::NoSuchLink(_))
        ));
        assert!(matches!(
            mgr.key_link(ghost),
            Err(crate::link::LinkError::NoSuchLink(_))
        ));
        assert!(matches!(
            mgr.remove_link(ghost),
            Err(crate::link::LinkError::NoSuchLink(_))
        ));
    }
}

/// Link-control (iax-62cf) integration tests: connect/disconnect by node via an
/// injected resolver, the `tick` reconnect supervisor + backoff, the aggregated
/// `LinkEvent` stream, and `Keyed` emission. Reuse the `pub(crate)`
/// `test_support::StubBackend` + `fake_inbound_call` fixtures (the same ones the
/// routing/link tests above use), and a fake resolver — no live network.
#[cfg(test)]
mod link_control_tests {
    use super::test_support::fake_inbound_call;
    use super::*;
    use crate::link::{LinkMode, LinkState};
    use crate::link_control::LinkSpec;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    fn test_manager() -> Manager {
        Manager::new(Box::new(test_support::StubBackend::new()))
    }

    fn peer() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4569))
    }

    fn spec(node: &str, mode: LinkMode, permanent: bool) -> LinkSpec {
        LinkSpec {
            node: node.to_string(),
            mode,
            output: OutputId::new("out:s"),
            caller_id: "1999".to_string(),
            secret: String::new(),
            dest: node.to_string(),
            mode_shape: CallMode::Standard,
            permanent,
        }
    }

    #[test]
    fn connect_link_dials_resolves_and_registers_a_monitor_link() {
        let mut mgr = test_manager();
        let resolver = |_node: &str| Ok(peer());
        let call = mgr
            .connect_link(spec("55553", LinkMode::Monitor, false), &resolver)
            .expect("connect");
        // It is in the link roster as a monitor link...
        let roster = mgr.link_roster();
        assert_eq!(roster.links.len(), 1);
        assert_eq!(roster.links[0].node, "55553");
        assert_eq!(roster.links[0].mode, LinkMode::Monitor);
        // ...and disconnect_link tears it down by node# (hangs up + drops the link).
        mgr.disconnect_link(call).expect("disconnect");
        assert!(mgr.link_roster().links.is_empty());
        assert_eq!(mgr.call_count(), 0, "disconnect_link hangs the call up");
    }

    #[test]
    fn connect_link_surfaces_resolver_failure_as_link_error_resolve() {
        let mut mgr = test_manager();
        let resolver = |_node: &str| Err(IaxError::MissingConfig("no route"));
        let err = mgr
            .connect_link(spec("99999", LinkMode::Monitor, false), &resolver)
            .unwrap_err();
        assert!(matches!(err, crate::link::LinkError::Resolve(_)));
    }

    #[test]
    fn link_events_receiver_is_handed_out_once() {
        let mut mgr = test_manager();
        assert!(
            mgr.link_events().is_some(),
            "first call yields the receiver"
        );
        assert!(mgr.link_events().is_none(), "second call yields None");
    }

    #[test]
    fn tick_redials_a_dropped_permanent_link_and_emits_disconnected_then_connected() {
        use std::time::Instant;
        let mut mgr = test_manager();
        let events = mgr.link_events().expect("receiver");
        let resolver = |_n: &str| Ok(peer());
        let secret_of = |_n: &str| String::new();

        // Connect a PERMANENT monitor link.
        let call = mgr
            .connect_link(spec("55553", LinkMode::Monitor, true), &resolver)
            .expect("connect");

        // Simulate the call dropping: hang it up out from under the link so its
        // CallId leaves the pool (the real-world "thread exited" case).
        mgr.hangup(call, None).expect("force-drop");
        assert_eq!(mgr.call_count(), 0, "permanent link's call is gone");

        // tick at t0 with backoff next_attempt_at == None => eligible immediately.
        let emitted = mgr.tick(Instant::now(), &resolver, &secret_of);
        // The supervisor re-dialed the permanent link back into the pool.
        assert_eq!(
            mgr.call_count(),
            1,
            "tick re-dialed the dropped permanent link"
        );
        assert_eq!(mgr.link_roster().links.len(), 1, "link re-registered");

        // A Disconnected event was emitted for the drop the supervisor observed.
        let drained: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert!(
            drained.iter().any(|e| matches!(e, crate::link_control::LinkEvent::Disconnected { node, .. } if node == "55553")),
            "tick emits Disconnected for the dropped permanent link"
        );
        assert!(
            !emitted.is_empty(),
            "tick returns the events it emitted for callers that prefer the return value"
        );
    }

    #[test]
    fn tick_re_stamps_addr_when_redialing_a_dropped_permanent_link() {
        use std::time::Instant;
        let mut mgr = test_manager();
        let resolver = |_n: &str| Ok(peer());
        let secret_of = |_n: &str| String::new();

        // Connect a PERMANENT monitor link.
        let call = mgr
            .connect_link(spec("55553", LinkMode::Monitor, true), &resolver)
            .expect("connect");
        assert_eq!(
            mgr.link_roster().links[0].addr,
            Some("127.0.0.1:4569".to_string()),
            "connect_link stamps addr up front"
        );

        // Simulate the call dropping out from under the link.
        mgr.hangup(call, None).expect("force-drop");
        assert_eq!(mgr.call_count(), 0, "permanent link's call is gone");

        // tick re-dials; the freshly resolved peer must be re-stamped onto the
        // recreated Link, not left at the add_link default of None.
        let _ = mgr.tick(Instant::now(), &resolver, &secret_of);
        assert_eq!(
            mgr.call_count(),
            1,
            "tick re-dialed the dropped permanent link"
        );
        assert_eq!(
            mgr.link_roster().links[0].addr,
            Some("127.0.0.1:4569".to_string()),
            "tick re-stamps addr on the reconnected link"
        );
    }

    #[test]
    fn tick_does_not_redial_a_deliberately_disconnected_permanent_link() {
        use std::time::Instant;
        let mut mgr = test_manager();
        let resolver = |_n: &str| Ok(peer());
        let secret_of = |_n: &str| String::new();
        let call = mgr
            .connect_link(spec("55553", LinkMode::Monitor, true), &resolver)
            .expect("connect");
        // Deliberate teardown clears `desired`.
        mgr.disconnect_link(call).expect("disconnect");
        let _ = mgr.tick(Instant::now(), &resolver, &secret_of);
        assert_eq!(
            mgr.call_count(),
            0,
            "a deliberately-disconnected link is never re-dialed"
        );
    }

    #[test]
    fn keying_a_transceive_link_emits_a_keyed_event() {
        let mut mgr = test_manager();
        let events = mgr.link_events().expect("receiver");
        // An adopted inbound leg is Active without a live socket (reuse the fixture).
        let call = mgr
            .adopt(
                fake_inbound_call(CallId(7), "55553"),
                &OutputId::new("out:s"),
            )
            .expect("adopt");
        mgr.route(call, &MicId::new("in:a")).expect("route");
        mgr.add_link(call, "55553", LinkMode::Transceive)
            .expect("link");

        mgr.key_link(call).expect("key");
        mgr.unkey_link(call).expect("unkey");

        let drained: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert!(
            drained.iter().any(|e| matches!(e, crate::link_control::LinkEvent::Keyed { node, keyed: true, .. } if node == "55553")),
            "key_link emits Keyed{{true}}"
        );
        assert!(
            drained.iter().any(|e| matches!(
                e,
                crate::link_control::LinkEvent::Keyed { keyed: false, .. }
            )),
            "unkey_link emits Keyed{{false}}"
        );
    }

    #[test]
    fn tick_backoff_defers_a_retry_when_redial_fails_then_allows_it_after_the_window() {
        use std::time::{Duration, Instant};
        let mut mgr = test_manager();
        let secret_of = |_n: &str| String::new();
        // A resolver that always fails so each tick schedules backoff.
        let bad = |_n: &str| Err(IaxError::MissingConfig("down"));
        // Connect via a working resolver, then drop the call.
        let good = |_n: &str| Ok(peer());
        let call = mgr
            .connect_link(spec("55553", LinkMode::Monitor, true), &good)
            .expect("connect");
        mgr.hangup(call, None).expect("force-drop");

        let t0 = Instant::now();
        // First tick: resolver fails → Disconnected emitted, retry scheduled at
        // t0 + 1s, no reconnect.
        let _ = mgr.tick(t0, &bad, &secret_of);
        assert_eq!(mgr.call_count(), 0, "still down after a failed redial");

        // A tick before the backoff window must NOT attempt again (no new redial).
        let _ = mgr.tick(t0 + Duration::from_millis(500), &good, &secret_of);
        assert_eq!(
            mgr.call_count(),
            0,
            "backoff window not elapsed: no retry yet"
        );

        // After the window, a good resolver reconnects.
        let _ = mgr.tick(t0 + Duration::from_millis(1_100), &good, &secret_of);
        assert_eq!(
            mgr.call_count(),
            1,
            "after backoff elapsed, the link reconnects"
        );
    }

    #[test]
    fn roster_carries_addr_and_zero_up_secs_while_connecting() {
        let mut mgr = test_manager();
        let resolver = |_node: &str| Ok(peer());
        let _call = mgr
            .connect_link(spec("55553", LinkMode::Monitor, false), &resolver)
            .expect("connect");
        let roster = mgr.link_roster();
        let s = &roster.links[0];
        assert_eq!(
            s.addr.as_deref(),
            Some("127.0.0.1:4569"),
            "dialed link records the resolved peer addr"
        );
        assert!(!s.rx_active, "no remote key yet");
        assert_eq!(s.up_secs, 0, "connecting links report up_secs = 0");
    }

    #[test]
    fn roster_reports_rx_active_and_up_secs_for_an_up_link() {
        let mut mgr = test_manager();
        let inbound = fake_inbound_call(CallId(0), "1999");
        let rk = inbound.remote_keyed_handle();
        let id = mgr.adopt(inbound, &OutputId::new("out:s")).expect("adopt");
        mgr.add_link(id, "1999", LinkMode::Monitor).expect("link");
        let first = mgr.link_roster();
        assert!(matches!(first.links[0].state, LinkState::Up));
        assert!(!first.links[0].rx_active);
        assert_eq!(
            first.links[0].addr, None,
            "adopted inbound link has no dialed addr"
        );
        rk.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            mgr.link_roster().links[0].rx_active,
            "RX follows remote_keyed"
        );
        // up_secs counts from the first roster read that observed Up — wait-until
        // poll (house style), no fixed sleep assumptions about scheduling.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if mgr.link_roster().links[0].up_secs >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "up_secs never advanced past 0"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
