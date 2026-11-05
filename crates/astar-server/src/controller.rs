// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `NodeController` — the in-process API over [`Station`] that all adapters use.
//!
//! Adapters (HTTP+SSE, stdin, TUI) call `execute()` / `subscribe()` / `pump()`.
//! The controller is the single owner of the [`Station`] and the [`SecretProvider`];
//! secrets never escape through any outbound type.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use astar_station::{
    InboundConfig, M17MonitorEdge, M17MonitorRequest, RegisterConfig, Station, StationError,
    StationEvent,
};

use crate::{
    command::{
        EchoLinkAction, LinkAction, M17Action, M17LinkStatus, NodeCommand, NodeError, NodeEvent,
        NodeReply, NodeSnapshot,
    },
    config::{AnnounceCfg, EchoLinkCfg, LinkShape},
    dtmf_commands::DtmfCommandAssembler,
    echolink_node::{self, EchoLinkNodeState},
    m17_directory::{
        M17Settings, ReflectorEntry, module_list, validate_m17_callsign, validate_module,
        validate_reflector_name,
    },
    m17_relay::RelayCounts,
    secrets::SecretProvider,
};

/// Default on-join greeting template (iax-c4ea) used when `[announce]
/// join_template` is unset. The `{server-node-number}` token is replaced at
/// announce time with this node's id expanded into space-separated digits.
pub(crate) const DEFAULT_JOIN_TEMPLATE: &str = "Connected to node {server-node-number}";

/// Default spoken phrase before a link dial (iax-9e02). `{node}` is replaced
/// with the target node number as space-separated digits.
pub(crate) const DEFAULT_LINK_CONNECT_TEMPLATE: &str = "Connecting to node {node}";

/// Default spoken phrase after a link is torn down (iax-9e02).
pub(crate) const DEFAULT_LINK_DISCONNECT_TEMPLATE: &str = "Disconnected from node {node}";

/// Token replaced with the TARGET node number in the link templates.
pub(crate) const LINK_NODE_TOKEN: &str = "{node}";

/// Default spoken phrase once an M17 monitor has been set up. Inbound and
/// handset users get the same disclosure an `AllStar` dial gives them: the
/// conference is about to carry a far network. `{node}` (`LINK_NODE_TOKEN`)
/// is replaced by `m17_announce_text` with the spelled-out reflector and
/// module.
pub(crate) const DEFAULT_M17_CONNECT_TEMPLATE: &str = "Monitoring M17 reflector {node}";

/// Default spoken phrase once an M17 monitor is gone, for any reason.
pub(crate) const DEFAULT_M17_DISCONNECT_TEMPLATE: &str = "Stopped monitoring M17 reflector {node}";

/// The `NodeEvent::AnnouncementStarted` kind for an M17 monitor
/// announcement. A named function rather than inline literals so the exact
/// SSE strings a dashboard keys on are pinned by a unit test.
const fn m17_announcement_kind(connecting: bool) -> &'static str {
    if connecting {
        "m17_connect"
    } else {
        "m17_disconnect"
    }
}

/// The literal placeholder token substituted in the join greeting template.
pub(crate) const JOIN_NODE_TOKEN: &str = "{server-node-number}";

/// Spoken into the channel on join. Disclosure is a requirement of relaying
/// a radio net into a voice channel, not a nicety.
///
/// This is the VOICE notice and `announce_on_join` still governs it. The text
/// status lines (`astar_discord::BridgeStatus`) are in addition to it, never
/// instead of it: spoken disclosure is heard once by whoever is in the channel
/// at that moment, written disclosure is there for anyone scrolling back.
#[cfg(feature = "discord")]
const JOIN_NOTICE: &str = "This channel is bridged to a live amateur radio net. Audio here is \
     received from the air and this bridge does not transmit.";

/// How often a joined bridge with a link up reposts "still linked" in the
/// channel. Nothing is posted on this timer while the link roster is empty.
///
/// **Fixed, and deliberately not a config key.** Keeping `[discord]` small is
/// a stated promise of this design, and an operator who wants a different
/// cadence for a reassurance line is not a need anyone has expressed. Making
/// it configurable is a deliberate non-goal; if that ever changes it is a
/// design decision, not a drive-by addition.
///
/// Distinct from `[announce] id_interval_secs`, which drives the SPOKEN
/// periodic ID onto the bridge member's leg: that one is a station-ID
/// obligation on a real interval, this one is a written state reassertion.
#[cfg(feature = "discord")]
const DISCORD_STATUS_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(3600);

/// How a status line names a node that has no `[register]` username — the
/// bridge still has to say what it is bridging.
#[cfg(feature = "discord")]
const DISCORD_UNREGISTERED_LABEL: &str = "this astar node";

/// Default inter-digit gap (iax-d254) that finalizes a pending DTMF `*`
/// command when `[dtmf] inter_digit_timeout_ms` is unset. Matches
/// [`crate::config::NodeFileConfig::dtmf_inter_digit_timeout_ms`]'s default.
#[allow(clippy::duration_suboptimal_units)]
const DEFAULT_DTMF_INTER_DIGIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(3000);

/// Refusal when `[m17]` is absent or `enabled = false`. M17 is off until the
/// operator opts in: an upgrade alone must not let anyone present a callsign
/// to a third-party reflector.
const M17_NOT_ENABLED: &str =
    "M17 isn't enabled on this node — its operator can add an `[m17]` section.";

/// Refusal when a Monitor is missing one of its three fields. The control
/// port has no callsign default on purpose: a one-line POST must not be able
/// to put the operator's callsign on somebody else's reflector.
const M17_NEEDS_ALL: &str = "An M17 monitor needs a reflector, a module and a callsign.";

/// Refusal when no Codec 2 decoder can be loaded. Said up front, rather than
/// after a link that could only ever carry silence.
const M17_NO_CODEC: &str = "this node has no Codec 2 decoder — its operator needs to install libcodec2 (or build astar-server with codec2-static).";

/// Refusal for a Disconnect with no leg up.
const M17_NOTHING_UP: &str = "No M17 link is up.";

/// How long [`NodeController::teardown_echolink`] waits for the directory
/// logoff before leaving it to finish (or not) on its own thread.
const ECHOLINK_LOGOFF_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// End reason for an explicit Disconnect (`POST /m17`, `/hangup`).
const M17_END_HUNG_UP: &str = "hung up";

/// End reason when the node itself is going away.
const M17_END_SHUTDOWN: &str = "node shutting down";

/// The node's M17 monitor state.
///
/// Behind one short-held mutex that is only ever taken to clone or take a
/// field, never across a `Station` call: the station call can join a thread,
/// and `snapshot()` reads this mutex on every `/status`.
#[derive(Default)]
pub(crate) struct M17NodeState {
    /// `[m17]`, validated at start-up. `None` ⇒ M17 is off.
    pub(crate) settings: Option<M17Settings>,
    /// The leg this controller placed, from Monitor until it ends.
    pub(crate) leg: Option<M17LegRecord>,
    /// How the most recent leg ended on its own (NACK, reflector DISC,
    /// silence). Cleared by the next Monitor, so a re-dial never inherits
    /// the last leg's reason.
    pub(crate) last_ended: Option<M17Ended>,
}

/// One monitor leg as the controller recorded it. Names are uppercase.
#[derive(Debug, Clone)]
pub(crate) struct M17LegRecord {
    pub(crate) reflector: String,
    pub(crate) module: char,
    /// The identity presented in CONN, shown on `/status`.
    pub(crate) callsign: String,
    /// When ACKN arrived. `None` while linking.
    pub(crate) linked_at: Option<std::time::Instant>,
}

/// Why a leg ended without being asked to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct M17Ended {
    pub(crate) reflector: String,
    pub(crate) module: char,
    pub(crate) reason: String,
}

/// A Monitor request that passed every pre-flight check: uppercase names,
/// and where CONN goes, from operator-owned data only.
#[derive(Debug)]
struct M17Target {
    reflector: String,
    module: char,
    callsign: String,
    entry: ReflectorEntry,
}

/// The Monitor pre-flight, in its fixed order. Every refusal is one plain,
/// secret-free sentence, and every one happens before a socket opens:
///
/// 1. `[m17]` absent or `enabled = false`.
/// 2. A missing field, then the reflector, module and callsign shapes.
/// 3. A name the table and directory do not know. There is no other source
///    of an address, so an unknown name can open nothing.
/// 4. A module the directory says that reflector does not offer (a plain
///    refusal instead of a 30 s "did not answer").
/// 5. No Codec 2 decoder. `codec_available` loads a library on its first
///    call, so it is a closure and runs only once 1–4 have passed.
/// 6. A leg already up. One monitor per node.
///
/// Pure (given the closure), so the order is testable without a reflector.
fn m17_preflight(
    settings: Option<&M17Settings>,
    reflector: Option<&str>,
    module: Option<char>,
    callsign: Option<&str>,
    codec_available: impl FnOnce() -> bool,
    live: Option<&M17LegRecord>,
) -> Result<M17Target, NodeError> {
    let refuse = |message: String| NodeError { message };
    let Some(settings) = settings.filter(|s| s.enabled) else {
        return Err(refuse(M17_NOT_ENABLED.to_string()));
    };
    let (Some(reflector), Some(module), Some(callsign)) = (reflector, module, callsign) else {
        return Err(refuse(M17_NEEDS_ALL.to_string()));
    };
    let reflector = validate_reflector_name(reflector).map_err(refuse)?;
    let module = validate_module(module).map_err(refuse)?;
    let callsign = validate_m17_callsign(callsign).map_err(refuse)?;
    let Some(entry) = settings.resolve(&reflector) else {
        return Err(refuse(format!(
            "I don't know a reflector called {reflector}. The node's operator can add it under `[m17.reflectors]`."
        )));
    };
    if let Some(mods) = entry.modules.as_deref()
        && !mods.contains(&module)
    {
        let list = module_list(mods);
        return Err(refuse(if mods.len() == 1 {
            format!("{reflector} has module {list}.")
        } else {
            format!("{reflector} has modules {list}.")
        }));
    }
    if !codec_available() {
        return Err(refuse(M17_NO_CODEC.to_string()));
    }
    if let Some(leg) = live {
        return Err(refuse(format!(
            "Already listening to M17 {} {}. `/hangup` first.",
            leg.reflector, leg.module
        )));
    }
    Ok(M17Target {
        reflector,
        module,
        callsign,
        entry,
    })
}

/// Core in-process controller that wraps a [`Station`].
///
/// All adapters (HTTP, stdin, TUI) call into this struct; it never leaks
/// credentials — `NodeError`, `NodeReply`, `NodeEvent`, and `NodeSnapshot` are
/// all secret-free.
pub struct NodeController {
    /// **Private, and it has to stay private.** `execute()` is the only
    /// way in, and `execute()` is where `key_refusal` lives — the guard
    /// that refuses `Key` while D-Star is active. A sibling module holding
    /// `&self.station` could call `set_ptt(true)` straight past it. The
    /// Discord adapter is given narrow doors instead
    /// (`link_roster_node_ids`, `discord_bridge_joined`, `handle_link`,
    /// `handle_m17`, `m17_linked`, `m17_end_reason`, `m17_last_heard`).
    station: Station,
    secrets: SecretProvider,
    /// Inbound-listener config; defaults to loopback+ephemeral port in tests.
    inbound_cfg: InboundConfig,
    /// Registration config — `None` until set from config (Task 5 / `from_config`).
    register_cfg: Option<RegisterConfig>,
    /// Announcement config — `None` until set via `with_configs`.
    announce_cfg: Option<AnnounceCfg>,
    /// This node's own ID string for voice/CW ID announcements.
    node_id: Option<String>,
    /// Per-target link dial shape (iax-5029), keyed by node number. Empty in
    /// [`NodeController::new`] (every target dials [`LinkShape::Standard`]);
    /// populated by [`NodeController::with_configs`] from the `[links]` table.
    link_shapes: std::collections::HashMap<String, LinkShape>,
    /// DTMF `*`-command execution (iax-d254): gate + per-call assembler.
    /// Commands only execute when `[dtmf] enabled = true` — new authority,
    /// off by default.
    dtmf_enabled: bool,
    dtmf_assembler: Mutex<DtmfCommandAssembler>,
    /// Set to `true` by `Shutdown`.
    stop: Arc<AtomicBool>,
    /// Active event subscribers. We hold one `Sender` per `subscribe()` call.
    senders: Arc<Mutex<Vec<std::sync::mpsc::Sender<NodeEvent>>>>,
    /// Next periodic-ID deadline (monotonic). `None` on first pump (not yet armed).
    /// Interior mutability keeps `pump(&self)` as a shared-ref method.
    next_id_at: Mutex<Option<std::time::Instant>>,
    /// Test-only record of every M17 announcement `fire_m17_announcement`
    /// was asked to speak, as `(connecting, rendered_text)`. With no
    /// conference member the reach gate hides whether a call site fired at
    /// all, so the tests read this instead. Absent from non-test builds.
    #[cfg(test)]
    m17_announce_log: Mutex<Vec<(bool, String)>>,
    /// The public listener's TLS and DNS state, once the public side has
    /// started (it starts after the controller is wrapped in its `Arc`).
    /// Admin snapshot only: the public router never reads it.
    public_status: Mutex<Option<Arc<Mutex<crate::public::serve::PublicTlsStatus>>>>,
    /// The sign-in service, once `serve` has opened the auth store. Both
    /// routers read it through [`NodeController::auth`]; nothing in it ever
    /// reaches a snapshot, an event or the `Station`.
    #[cfg(feature = "auth")]
    auth: std::sync::OnceLock<Arc<crate::auth::AuthService>>,
    /// The node logger, once `serve` has started one (`[recording]`). No
    /// feature gate: recording needs neither `auth` nor `stream`.
    recordings: std::sync::OnceLock<Arc<crate::recording::logger::NodeLogger>>,
    /// M17 monitor state. Held only to clone or take a field, and never
    /// across a `Station` call, so `/status` never waits on a join.
    pub(crate) m17: Mutex<M17NodeState>,
    /// Serializes a whole `handle_m17` / `teardown_m17` body. `server.rs`
    /// runs one thread per request, and without this two concurrent Monitors
    /// could both pass the one-leg check. `drain_m17` only `try_lock`s it
    /// (it skips the M17 poll for one tick while a body runs), so an edge is
    /// never reported in the middle of a Monitor or Disconnect body (no
    /// `disconnected` before `linking`). The Discord commander, which runs on
    /// the pump, reaches `handle_m17` through the adapter and takes it
    /// blocking. That wait is bounded by one DISC plus a ≤ 50 ms join, and no
    /// body waits on anything the pump holds. `snapshot()` never takes it.
    m17_op: Mutex<()>,
    /// `[echolink]` and why the last login never started. Held only to
    /// clone or set a field, and never across a `Station` call.
    echolink: Mutex<EchoLinkNodeState>,
    /// The `[discord]` section, if configured. `None` ⇒ the verb refuses.
    discord_cfg: Option<crate::config::DiscordCfg>,
    /// The settings page's store, when `main` installed one. `None` on a
    /// test or embedded controller, and the settings routes answer 503 for
    /// it rather than inventing a path to write to.
    settings: Option<crate::settings_store::SettingsStore>,
    /// Set by `NodeCommand::Restart` beside `stop`: the shutdown under way
    /// is to end in a re-exec, not an exit. `run_serve` reads it to arm the
    /// watchdog and `run_serve_mode` to exec. Never set by `Shutdown`.
    restart: AtomicBool,
    /// The live bridge while joined. `None` ⇒ not joined.
    ///
    /// `pub(crate)` from here down through `discord_view`: the
    /// slash-command wiring lives in [`crate::discord_commands`], which is a
    /// sibling module rather than a second `impl` block in this file
    /// (controller.rs is already 3.7k lines). Nothing outside this crate can
    /// reach any of it.
    #[cfg(feature = "discord")]
    discord: Mutex<Option<astar_discord::Bridge>>,
    /// The rule engine, when slash commands are configured.
    ///
    /// **Checked OUT rather than borrowed.** Every call into it
    /// (`handle`/`tick`/`admin`/`shutdown`) calls back into the station
    /// through `NodeOps` — `dial_allstar_monitor` reaches `handle_link`,
    /// which can block on DNS or a portal HTTPS mint — so it must never run
    /// with a controller mutex held. `Option::take` under this lock, run,
    /// then put back: see [`crate::discord_commands::CheckedOutCommander`],
    /// which also puts it back when the run panics.
    ///
    /// Declared between `discord` and `discord_gateway` so the drop order
    /// below still holds, with the commander in the middle where it belongs.
    #[cfg(feature = "discord")]
    pub(crate) discord_commander: Mutex<Option<astar_discord::Commander>>,
    /// Where the gateway delivers events, when it was started with
    /// `GatewayOptions`. Replaced wholesale whenever a new gateway is
    /// connected — the old session's queue describes a reality that no
    /// longer exists, and the new session opens with its own `Ready`.
    #[cfg(feature = "discord")]
    pub(crate) discord_events:
        Mutex<Option<std::sync::mpsc::Receiver<astar_discord::GatewayEvent>>>,
    /// Events drained from the receiver but not yet given to the commander,
    /// because an HTTP admin call had it checked out. Kept in arrival order
    /// and retried on the next pump: dropping a `Voice` event would leave
    /// the channel roster wrong until the next reconnect.
    #[cfg(feature = "discord")]
    pub(crate) discord_pending_events:
        Mutex<std::collections::VecDeque<astar_discord::GatewayEvent>>,
    /// The last view the commander produced, refreshed after every
    /// `handle`/`tick`/`admin`/`shutdown`.
    ///
    /// `snapshot()` reads THIS and never the commander itself, so `/status`
    /// cannot queue behind a checked-out commander (and, through it, behind
    /// a dial waiting on DNS). The pump refreshes it every 50 ms, so it is
    /// never meaningfully stale, and every admin action refreshes it before
    /// returning, so the page never reads back its own change as missing.
    #[cfg(feature = "discord")]
    pub(crate) discord_view: Mutex<Option<astar_discord::AdminView>>,
    /// The bridged voice channel id, parsed once when the commander was
    /// built. This is where public lines and the PTT panel go.
    #[cfg(feature = "discord")]
    pub(crate) discord_channel: Option<u64>,
    /// The Discord gateway — the bot being online — while one is up.
    ///
    /// Separate from `discord` because the two have different lifetimes: a
    /// gateway owns the runtime thread, the serenity client and the
    /// reconnect supervisor, and a voice session is one `Join`/`Leave` pair
    /// over it. `None` ⇒ the bot is offline.
    ///
    /// **Declared after `discord` on purpose.** Struct fields drop in
    /// declaration order, so when the controller itself goes away the
    /// `Bridge` — and the voice handle inside it, which asks to leave the
    /// channel — is dropped before the `Gateway` that would carry that ask,
    /// rather than after it.
    ///
    /// **And after `discord_commander`**, so that the unmutes a shutdown
    /// enqueued through a `GatewayHandle` are still carried by a live
    /// gateway when the controller is torn down right afterwards.
    ///
    /// **But a dropped controller is not what the daemon does.** In `serve`
    /// this struct is never dropped: `main` wraps it in an `Arc` and the
    /// ctrlc handler's closure holds a clone for the life of the process.
    /// The order above is therefore a backstop for embedders, and the real
    /// teardown is [`NodeController::teardown_discord`], which performs the
    /// same three steps explicitly at the end of the run — sweep, bridge,
    /// gateway — and is what actually sends `Gateway::drop`'s `Shutdown`.
    ///
    /// Neither buys anything on a route out that runs no destructors at
    /// all. `Commander` has no `Drop`, so a panicking `main` or a `SIGKILL`
    /// enqueues no unmutes: members stay server-muted until they next
    /// appear in voice with the node up, which is the documented limit of
    /// server muting, not a bug in this order.
    #[cfg(feature = "discord")]
    pub(crate) discord_gateway: Mutex<Option<astar_discord::Gateway>>,
    /// The bridge's conference member, for announcing the join notice and the
    /// periodic ID onto its leg. `None` ⇒ not joined.
    #[cfg(feature = "discord")]
    discord_member: Mutex<Option<astar_audio::MemberId>>,
    /// Set once the rule engine has been asked to lift its server mutes, so
    /// it is asked exactly once.
    ///
    /// Two callers reach that sweep and either may be first: the explicit
    /// `NodeCommand::Shutdown` (a signal, the TUI, `POST /shutdown`) and
    /// `teardown_discord` at the end of the run. A second sweep would
    /// re-issue a PATCH per member against a gateway that is already
    /// closing, and race the wait that exists to let the first ones land.
    #[cfg(feature = "discord")]
    pub(crate) discord_shutdown_done: AtomicBool,
    /// Set when a new gateway was installed while the rule engine was
    /// checked out, so the next pump can tell it. See
    /// `note_discord_gateway_replaced`: a `Commander` method may never run
    /// under a controller mutex, and `ensure_discord_gateway` holds
    /// `discord_op` when it installs one.
    #[cfg(feature = "discord")]
    pub(crate) discord_gateway_replaced: AtomicBool,
    /// Serializes an entire Join or Leave operation. `server.rs` spawns one
    /// worker thread per HTTP request against a shared `Arc<NodeController>`,
    /// so a plain check-then-act on `discord` would let two concurrent Joins
    /// both observe "not joined" and both enrol — the second store would
    /// silently drop the first `Bridge`, stranding its member. Held for the
    /// whole `handle_discord` body; deliberately NOT taken by `snapshot()`,
    /// which reads only the (much shorter-held) `discord` mutex, so a Join
    /// in progress never blocks `/status`.
    #[cfg(feature = "discord")]
    discord_op: Mutex<()>,
    /// Next deadline for the periodic Discord-channel ID (separate from the
    /// on-air `next_id_at` timer — this one is spoken only onto the bridge
    /// member's own leg, never the air). `None` ⇒ not yet armed.
    #[cfg(feature = "discord")]
    discord_id_due: Mutex<Option<std::time::Instant>>,
    /// The phrase spoken for the periodic Discord ID, built once at
    /// construction from the node's registered id: `"{node_id} bridge"`, or
    /// `"astar bridge"` when no node id is configured.
    #[cfg(feature = "discord")]
    discord_id_text: String,
    /// Next deadline for the hourly "still connected" line. `None` ⇒ not
    /// armed; `pump()` arms it from the presence of a bridge and disarms it
    /// from its absence, so it cannot outlive a join by any route.
    #[cfg(feature = "discord")]
    discord_status_due: Mutex<Option<std::time::Instant>>,
    /// Noun phrase naming this node in every status line posted into the
    /// Discord channel — `"node 12345"`, or a generic stand-in when no node
    /// id is registered. Built once, from the same node id as the on-air ID.
    #[cfg(feature = "discord")]
    discord_node_label: String,
}

impl NodeController {
    /// Build a controller over `station` using `secrets` as the credential store.
    ///
    /// `inbound_cfg` defaults to binding `127.0.0.1:0` (ephemeral loopback) so
    /// that in-process tests can call `EnableInbound` without touching `0.0.0.0:4569`.
    /// A real daemon should call `from_config` (Task 5) which sets the real bind.
    pub fn new(station: Station, secrets: SecretProvider) -> Self {
        // Install the secret resolver so the Station can look up passwords.
        station.set_secret_resolver(secrets.resolver());

        // Default inbound to loopback:ephemeral — safe for tests; Task 5 overrides.
        let mut inbound_cfg = InboundConfig {
            bind: "127.0.0.1:0".parse().expect("static loopback:0 is valid"),
            ..InboundConfig::default()
        };
        // Wire the SAME secret store into inbound auth (iax-99cd): inbound
        // `auth=Required`/`Optional` resolve credentials from the provider that
        // `ALLSTAR_PEER_*` env + `POST /secrets` feed, not just outbound register.
        inbound_cfg.policy.credential_resolver = Some(secrets.resolver_arc());

        Self {
            station,
            secrets,
            inbound_cfg,
            register_cfg: None,
            announce_cfg: None,
            node_id: None,
            link_shapes: std::collections::HashMap::new(),
            dtmf_enabled: false,
            dtmf_assembler: Mutex::new(DtmfCommandAssembler::new(DEFAULT_DTMF_INTER_DIGIT_TIMEOUT)),
            stop: Arc::new(AtomicBool::new(false)),
            senders: Arc::new(Mutex::new(Vec::new())),
            next_id_at: Mutex::new(None),
            #[cfg(test)]
            m17_announce_log: Mutex::new(Vec::new()),
            public_status: Mutex::new(None),
            #[cfg(feature = "auth")]
            auth: std::sync::OnceLock::new(),
            recordings: std::sync::OnceLock::new(),
            m17: Mutex::new(M17NodeState::default()),
            m17_op: Mutex::new(()),
            echolink: Mutex::new(EchoLinkNodeState::default()),
            discord_cfg: None,
            settings: None,
            restart: AtomicBool::new(false),
            #[cfg(feature = "discord")]
            discord: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_commander: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_events: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_pending_events: Mutex::new(std::collections::VecDeque::new()),
            #[cfg(feature = "discord")]
            discord_view: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_channel: None,
            #[cfg(feature = "discord")]
            discord_gateway: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_member: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_shutdown_done: AtomicBool::new(false),
            #[cfg(feature = "discord")]
            discord_gateway_replaced: AtomicBool::new(false),
            #[cfg(feature = "discord")]
            discord_op: Mutex::new(()),
            #[cfg(feature = "discord")]
            discord_id_due: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_id_text: "astar bridge".to_string(),
            #[cfg(feature = "discord")]
            discord_status_due: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_node_label: DISCORD_UNREGISTERED_LABEL.to_string(),
        }
    }

    /// Build a controller with explicit inbound and optional register configs.
    ///
    /// Use this from the real daemon entry-point after loading [`crate::config::NodeFileConfig`]:
    /// ```ignore
    /// let nc = NodeController::with_configs(
    ///     station, secrets, inbound_cfg, register_cfg, announce_cfg, link_shapes,
    ///     dtmf_enabled, dtmf_inter_digit_timeout_ms,
    /// );
    /// ```
    /// Tests should use [`NodeController::new`] which defaults to a safe loopback bind.
    #[allow(clippy::too_many_arguments)]
    pub fn with_configs(
        station: Station,
        secrets: SecretProvider,
        mut inbound_cfg: InboundConfig,
        register_cfg: Option<RegisterConfig>,
        announce_cfg: Option<AnnounceCfg>,
        link_shapes: std::collections::HashMap<String, LinkShape>,
        dtmf_enabled: bool,
        dtmf_inter_digit_timeout_ms: u64,
    ) -> Self {
        station.set_secret_resolver(secrets.resolver());
        // Wire the SAME secret store into inbound auth (iax-99cd). A caller may
        // have set a static `policy.credentials` map; the resolver is the
        // dynamic fallback consulted on a miss, so both coexist.
        inbound_cfg.policy.credential_resolver = Some(secrets.resolver_arc());
        // Push the announce config into the Station (and its ConsoleSession) so
        // it is available immediately — and replayed whenever the Manager is
        // (re)built inside ensure_engine.
        if let Some(ref acfg) = announce_cfg {
            station.set_announce_config(acfg.to_service_config());
        }
        // Source the node id from the registration config's username field,
        // which holds the node's numeric ID (e.g. "77777").
        let node_id = register_cfg.as_ref().map(|r| r.username.clone());
        // Built once here (not on every announce) from the same node id used
        // for the on-air ID, e.g. "77777 bridge", or "astar bridge" when no
        // node id is configured.
        #[cfg(feature = "discord")]
        let discord_id_text = node_id
            .as_deref()
            .map_or_else(|| "astar bridge".to_string(), |id| format!("{id} bridge"));
        // Same source, different shape: the ID is spoken ("77777 bridge")
        // and this is read ("node 77777"). Both fall back rather than
        // omitting the node, so a status line always names something.
        #[cfg(feature = "discord")]
        let discord_node_label = node_id.as_deref().map_or_else(
            || DISCORD_UNREGISTERED_LABEL.to_string(),
            |id| format!("node {id}"),
        );
        Self {
            station,
            secrets,
            inbound_cfg,
            register_cfg,
            announce_cfg,
            node_id,
            link_shapes,
            dtmf_enabled,
            dtmf_assembler: Mutex::new(DtmfCommandAssembler::new(
                std::time::Duration::from_millis(dtmf_inter_digit_timeout_ms),
            )),
            stop: Arc::new(AtomicBool::new(false)),
            senders: Arc::new(Mutex::new(Vec::new())),
            next_id_at: Mutex::new(None),
            #[cfg(test)]
            m17_announce_log: Mutex::new(Vec::new()),
            public_status: Mutex::new(None),
            #[cfg(feature = "auth")]
            auth: std::sync::OnceLock::new(),
            recordings: std::sync::OnceLock::new(),
            m17: Mutex::new(M17NodeState::default()),
            m17_op: Mutex::new(()),
            echolink: Mutex::new(EchoLinkNodeState::default()),
            discord_cfg: None,
            settings: None,
            restart: AtomicBool::new(false),
            #[cfg(feature = "discord")]
            discord: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_commander: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_events: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_pending_events: Mutex::new(std::collections::VecDeque::new()),
            #[cfg(feature = "discord")]
            discord_view: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_channel: None,
            #[cfg(feature = "discord")]
            discord_gateway: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_member: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_shutdown_done: AtomicBool::new(false),
            #[cfg(feature = "discord")]
            discord_gateway_replaced: AtomicBool::new(false),
            #[cfg(feature = "discord")]
            discord_op: Mutex::new(()),
            #[cfg(feature = "discord")]
            discord_id_due: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_id_text,
            #[cfg(feature = "discord")]
            discord_status_due: Mutex::new(None),
            #[cfg(feature = "discord")]
            discord_node_label,
        }
    }

    /// Access the secret provider (e.g. to `resolve` in tests or to bulk-load env).
    pub fn secrets(&self) -> &SecretProvider {
        &self.secrets
    }

    /// Install the `[discord]` section. Called from `main.rs` after
    /// construction and before the controller is wrapped in an `Arc`.
    pub fn set_discord_config(&mut self, cfg: Option<crate::config::DiscordCfg>) {
        self.discord_cfg = cfg;
    }

    /// Install `[m17]`. Called from `main.rs` after construction and before
    /// the controller is wrapped in an `Arc`. `None` ⇒ M17 is off: the
    /// operator's section is the opt-in, so an upgrade alone never lets
    /// anyone present a callsign to a third-party reflector.
    pub fn set_m17_config(&mut self, settings: Option<M17Settings>) {
        self.m17
            .get_mut()
            .expect("m17 state mutex poisoned")
            .settings = settings;
    }

    /// Install `[echolink]`. Called from `main.rs` after construction and
    /// before the controller is wrapped in an `Arc`. `None` ⇒ `EchoLink` is
    /// dormant: every `POST /echolink` is refused and nothing logs in. The
    /// location goes to the station now (no network), so the login that
    /// [`NodeController::start_echolink_login`] starts carries it.
    pub fn set_echolink_config(&mut self, cfg: Option<EchoLinkCfg>) {
        if let Some(c) = cfg.as_ref() {
            self.station.echolink_set_location(&c.location);
        }
        let st = self
            .echolink
            .get_mut()
            .expect("echolink state mutex poisoned");
        st.cfg = cfg;
        st.setup_error = None;
    }

    /// Start logging in to the `EchoLink` directory as `[echolink]`'s
    /// callsign, with the `echolink:<CALL>` secret. A no-op without the
    /// section. Returns at once: the login runs on the station's own thread
    /// and `/status` shows how it went.
    ///
    /// Logging in only puts the node in the station list. It calls nobody:
    /// every station on the leg is one a person dialled.
    ///
    /// With no password held nothing starts, and the card says which secret
    /// to set. The password is read here, moved into the station, and never
    /// kept or printed.
    pub fn start_echolink_login(&self) {
        let Some(cfg) = self
            .echolink
            .lock()
            .expect("echolink state mutex poisoned")
            .cfg
            .clone()
        else {
            return;
        };
        let password = self.secrets.echolink_password(&cfg.callsign);
        let setup_error = if password.is_empty() {
            Some(echolink_node::no_password(&cfg.callsign))
        } else if !astar_station::echolink_password_is_sendable(&password) {
            // Names the secret's problem, never its value.
            Some(echolink_node::unsendable_password(&cfg.callsign))
        } else {
            self.station
                .echolink_login(&cfg.callsign, password, &cfg.servers)
                .err()
                .map(|e| e.to_string())
        };
        if let Some(why) = setup_error.as_deref() {
            eprintln!("astar-server: echolink: not logging in: {why}");
        }
        self.echolink
            .lock()
            .expect("echolink state mutex poisoned")
            .setup_error = setup_error;
    }

    /// `POST /secrets` just stored `username`. When it is this node's
    /// `echolink:<CALL>` (any case) and the node is not logged in, start the
    /// login now rather than waiting for a restart.
    ///
    /// A node that is already logged in keeps its login: a new password
    /// stored while logged in takes effect at the next restart (or the next
    /// login after a failure), not at once. Re-logging in here would log the
    /// node off and on again under every station on its leg.
    fn echolink_secret_arrived(&self, username: &str) {
        let Some(call) = username.strip_prefix(crate::secrets::ECHOLINK_PREFIX) else {
            return;
        };
        let ours = self
            .echolink
            .lock()
            .expect("echolink state mutex poisoned")
            .cfg
            .as_ref()
            .is_some_and(|c| c.callsign.eq_ignore_ascii_case(call.trim()));
        if ours
            && !self
                .station
                .echolink_state()
                .is_some_and(|s| s.login.state == astar_station::LoginState::LoggedIn)
        {
            self.start_echolink_login();
        }
    }

    /// The settings live tier's door to `[m17]`: `enabled` and the
    /// `[m17.reflectors]` table, which the next dial reads. `None` is a
    /// section removed from the file: M17 off, as on a node that never had
    /// one. The running directory is kept (see
    /// [`M17Settings::live_from`]), no library is loaded, and a monitor that
    /// is already up is left alone: turning M17 off refuses the next dial,
    /// it does not hang anyone up. The state mutex is held only for the
    /// swap, never across a `Station` call.
    ///
    /// # Errors
    /// The loader's sentence for a bad `[m17]`, which the settings oracle
    /// has already refused, so in practice none.
    pub fn apply_m17_settings(&self, cfg: Option<&crate::config::M17Cfg>) -> Result<(), NodeError> {
        let mut st = self.m17.lock().expect("m17 state mutex poisoned");
        st.settings = match cfg {
            None => None,
            Some(c) => Some(
                M17Settings::live_from(c, st.settings.as_ref())
                    .map_err(|message| NodeError { message })?,
            ),
        };
        Ok(())
    }

    /// Install the settings store. Called from `main.rs` after
    /// construction and before the controller is wrapped in an `Arc`, at
    /// the same moment as [`NodeController::set_discord_config`] and for
    /// the same reason: it is built from the `cfg` every other consumer
    /// was built from, so the running mirror it carries is what this
    /// process actually loaded.
    pub fn set_settings_store(&mut self, store: crate::settings_store::SettingsStore) {
        self.settings = Some(store);
    }

    /// The settings store, when one was installed.
    #[must_use]
    pub fn settings_store(&self) -> Option<&crate::settings_store::SettingsStore> {
        self.settings.as_ref()
    }

    /// `true` once a `NodeCommand::Restart` has been accepted. Read after
    /// the accept loop returns, when the only question left is whether this
    /// process exits or replaces itself. `Acquire` pairs with the `Release`
    /// store in the `Restart` arm; a caller on another thread must first
    /// see the stop flag with an `Acquire` load (see `run_serve`), which
    /// then guarantees this load sees the restart flag stored before it.
    #[must_use]
    pub fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::Acquire)
    }

    /// Why this node must not be restarted right now — `"2 calls, 1 link,
    /// 1 EchoLink station, Discord joined"`, only the non-zero parts, in
    /// that order — or `None` when it is idle. A node that is on a net —
    /// calls up, links up, `EchoLink` stations on its leg, a Discord channel
    /// listening — is never restarted on a bare request;
    /// the page shows this sentence and asks before it insists.
    #[must_use]
    pub fn busy_on_a_net(&self) -> Option<String> {
        let snap = self.snapshot();
        // A link is a call in the pool too: `connect_link` dials through
        // `dial_with_identity`, which inserts into `Manager.calls`, and the
        // snapshot copies that pool straight into `calls`. Take the link
        // legs out of the call count, or one link reads "1 call, 1 link".
        // `LinkSnapshot.call` is `CallId::as_raw()` by construction.
        let link_calls: std::collections::HashSet<u64> =
            snap.links.iter().map(|l| l.call).collect();
        let calls = snap
            .calls
            .iter()
            .filter(|c| !link_calls.contains(&c.id.as_raw()))
            .count();
        #[cfg(feature = "discord")]
        let discord_joined = self.discord_bridge_joined();
        #[cfg(not(feature = "discord"))]
        let discord_joined = false;
        let echolink = self.station.echolink_node_peers().len();
        busy_reason(calls, snap.links.len(), echolink, discord_joined)
    }

    /// The settings live tier's one door to the bridge: the whole
    /// `[bridge]`/`[parrot]` shape as `to_bridge_config` builds it, applied
    /// as `main` applies it at start-up. Narrow on purpose — it reaches
    /// `set_bridge_config` and nothing else on the station, so nothing that
    /// edits a file can key a transmitter.
    pub fn apply_bridge_config(&self, cfg: astar_iax::BridgeConfig) -> Result<(), NodeError> {
        self.station
            .set_bridge_config(cfg)
            .map_err(|e| station_err(&e))
    }

    /// What `Shutdown` does, shared with `Restart` so the two can never
    /// drift: the best-effort unmute sweep is queued first, then the stop
    /// flag is raised. The long comment on the `Shutdown` arm is the
    /// reasoning.
    fn shutdown(&self) {
        // DISC the M17 reflector now, while the pump is still running
        // to carry the event. Shutdown and Restart both come through here;
        // `run_serve` calls this again at the end of the run, and the
        // second call is a no-op.
        self.teardown_m17();
        // Hang up on every EchoLink station now, while the pump still runs.
        // The directory logoff can wait on an exchange in flight (about
        // 80 s at worst), so it is left to `teardown_echolink` at the end of
        // the run rather than done on whichever thread asked to stop.
        self.hang_up_echolink_stations();
        #[cfg(feature = "discord")]
        self.shutdown_discord_commander();
        // `Release`: a thread that sees `stop` with an `Acquire` load also
        // sees everything stored before it — the restart flag in particular.
        self.stop.store(true, Ordering::Release);
    }

    /// Install the approval state and build the rule engine. Called from
    /// `main.rs` AFTER [`NodeController::set_discord_config`] and before the
    /// controller is wrapped in an `Arc`, and only when `[discord]` names at
    /// least one approver.
    ///
    /// **Built eagerly, here, rather than lazily when the gateway first
    /// starts.** The commander is pure state with no network under it, and
    /// the two things that would go wrong with a lazy build are both real:
    /// the admin page would be dead until the bot connected — exactly when
    /// an operator most wants to approve someone — and the approvals loaded
    /// at start-up would sit in a field waiting for a gateway that may never
    /// come (no token yet, Discord down). A commander that exists from
    /// start-up has neither problem, and `ensure_discord_gateway` is then a
    /// single concern: the connection.
    ///
    /// # Errors
    /// When `channel_id` is not a snowflake. That is fatal rather than
    /// logged because approvers being configured means slash commands ARE
    /// the point of this section, and every public line and the PTT panel
    /// need somewhere to go.
    #[cfg(feature = "discord")]
    pub fn set_discord_approvals(
        &mut self,
        approvals: astar_discord::Approvals,
    ) -> Result<(), String> {
        let cfg = self
            .discord_cfg
            .as_ref()
            .ok_or_else(|| "discord: approvals set with no [discord] section".to_string())?;
        let channel: u64 = cfg
            .channel_id
            .parse()
            .map_err(|_| "discord: channel_id is not a snowflake".to_string())?;
        // Already validated at config load; parsed again rather than
        // assumed, because this is a `pub` entry point and an embedder is
        // not obliged to have come through `from_toml_str`.
        let approvers = cfg.approver_ids()?;
        let mut commander = astar_discord::Commander::new(approvals, approvers, channel);
        commander.set_mic_timeout(cfg.mic_timeout());
        commander.set_mic_silence(cfg.mic_silence());
        *self
            .discord_view
            .lock()
            .expect("discord view mutex poisoned") = Some(commander.view());
        *self
            .discord_commander
            .lock()
            .expect("discord commander mutex poisoned") = Some(commander);
        self.discord_channel = Some(channel);
        Ok(())
    }

    /// Execute one command synchronously.  Returns `Ok(NodeReply)` on success
    /// or `Err(NodeError)` on failure.  Secret-free — no credential ever appears
    /// in the return types.
    #[allow(clippy::too_many_lines)] // flat command match — splitting hurts readability
    pub fn execute(&self, cmd: NodeCommand) -> Result<NodeReply, NodeError> {
        match cmd {
            NodeCommand::Dial { node } => {
                self.station
                    .connect_wt(&node)
                    .map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::Hangup => {
                self.station.disconnect();
                Ok(NodeReply::Ok)
            }
            NodeCommand::Key => {
                // iax-d9f4: this crate exposes remote keying (`POST /key`, a
                // TUI keystroke) and `Station::set_ptt` is deliberately
                // network-agnostic — it keys whatever session is live. A
                // digital-voice session must never be keyable from here.
                //
                // The guard reads the snapshot's feature-INDEPENDENT
                // `dstar_active` flag rather than being `#[cfg]`-gated, so it
                // holds however this crate was compiled. That matters: Cargo
                // unifies features across a build, so a workspace build that
                // enables `astar-station/dstar` for another crate (the
                // C-ABI in `astar-sys` does exactly that) turns the
                // `dstar` feature on here too, whatever this crate's own
                // manifest asks for. Before this guard, that unification
                // alone would have made `POST /key` a remote D-Star transmit
                // trigger.
                let snap = self.station.snapshot();
                if let Some(e) = key_refusal_from(&snap) {
                    return Err(e);
                }
                self.station.set_ptt(true).map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::Unkey => {
                self.station.set_ptt(false).map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::Answer => {
                self.station.answer().map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::Reject => {
                self.station.reject().map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::EnableInbound => {
                self.station
                    .enable_inbound(self.inbound_cfg.clone())
                    .map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::DisableInbound => {
                self.station.disable_inbound();
                Ok(NodeReply::Ok)
            }
            NodeCommand::Register => {
                let cfg = self.register_cfg.clone().ok_or_else(|| NodeError {
                    message: "no register config set".into(),
                })?;
                self.station.register(cfg).map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::Deregister => {
                self.station.deregister();
                Ok(NodeReply::Ok)
            }
            NodeCommand::SetDevices { input, output } => {
                self.station.set_devices(input, output);
                Ok(NodeReply::Ok)
            }
            NodeCommand::ProvideSecret { username, secret } => {
                // Secret goes ONLY into the provider — never logged, echoed, or
                // returned.
                self.secrets.put(username.as_str(), secret);
                // The node's EchoLink password may be what just arrived; the
                // card told the operator to set it.
                self.echolink_secret_arrived(&username);
                // With `[secrets] source = "control"` the bot token does not
                // exist when the node starts, so the start-up attempt to
                // bring the gateway online failed at "no token". This is the
                // moment it becomes possible — and the only one, short of a
                // restart. A failure here is logged, never returned: the
                // command that arrived was "store this secret", and it
                // succeeded. Non-persistent nodes return immediately, so
                // this is free for everyone else.
                #[cfg(feature = "discord")]
                if username == "discord"
                    && let Err(e) = self.start_discord_gateway()
                {
                    eprintln!(
                        "astar-server: discord: gateway did not start after the token arrived: {}",
                        e.message
                    );
                }
                Ok(NodeReply::Ok)
            }
            NodeCommand::Status => Ok(NodeReply::Snapshot(self.snapshot())),
            NodeCommand::Shutdown => {
                // `Commander::shutdown` enqueues one best-effort unmute per
                // member this bridge muted. They ride the gateway's queue
                // ahead of the `Shutdown` that `Gateway::drop` sends, and
                // the command loop waits for them — bounded by one
                // `OP_TIMEOUT` for the whole set — before the gateway
                // closes.
                //
                // What actually drops that gateway is
                // `teardown_discord`, at the end of the run: in `serve`
                // this controller is never dropped at all (the ctrlc
                // handler holds an `Arc` clone for the life of the
                // process). Calling the sweep here as well as there is not
                // redundancy — it is what puts the unmutes on the queue
                // before anything starts tearing down — and it runs once,
                // whichever caller gets there first.
                //
                // Honest limits: best effort, and a member who is not in a
                // voice channel right now cannot be unmuted by anybody, so
                // their mark stays in `muted_by_us` and is lifted the next
                // time they appear in voice with this node running.
                self.shutdown();
                Ok(NodeReply::Ok)
            }
            NodeCommand::Announce {
                text,
                sample,
                destination,
                mixunder,
                gain_db,
            } => {
                let req = Self::build_announce_request(
                    text,
                    sample,
                    destination.as_deref(),
                    mixunder,
                    gain_db,
                )
                .ok_or_else(|| NodeError {
                    message: "announce needs text or sample".into(),
                })?;
                self.station.announce(req).map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::IdNow => {
                self.station
                    .announce(self.id_request())
                    .map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::SetBridge {
                mode,
                mix_minus,
                include_local_radio,
            } => self.set_bridge(mode, mix_minus, include_local_radio),
            NodeCommand::Link { action, node, addr } => {
                self.handle_link(action, &node, addr.as_deref())
            }
            NodeCommand::M17 {
                action,
                reflector,
                module,
                callsign,
            } => self.handle_m17(action, reflector.as_deref(), module, callsign.as_deref()),
            NodeCommand::EchoLink { action, target } => {
                self.handle_echolink(action, target.as_deref())
            }
            NodeCommand::Discord { action } => self.handle_discord(action),
            NodeCommand::DiscordAdmin { action, user_id } => {
                self.handle_discord_admin(&action, user_id.as_deref())
            }
            NodeCommand::DiscordState => self.handle_discord_state(),
            NodeCommand::PortalTest => {
                // Mint and drop with the credentials the station already
                // holds. No dial, no UDP socket; blocking HTTPS, which is
                // why the HTTP layer runs this on its per-request thread.
                self.station
                    .test_mint_token()
                    .map_err(|e| station_err(&e))?;
                Ok(NodeReply::Ok)
            }
            NodeCommand::Restart { disconnect } => {
                if !disconnect && let Some(why) = self.busy_on_a_net() {
                    return Err(NodeError {
                        message: format!("restart refused: this node is on a net ({why})"),
                    });
                }
                // The flag goes up BEFORE the stop flag, and the pairing is
                // release/acquire: this `Release` store is followed by the
                // `Release` store of `stop` in `shutdown()`, and `run_serve`
                // (and `run_serve_mode`) load `stop` with `Acquire` before
                // `restart_requested()` loads this flag with `Acquire`. So
                // the accept-loop thread, which sees `stop` set, is
                // guaranteed to see this flag set too — on arm64 as on x86.
                // `Relaxed` would give no such guarantee: a stop seen
                // without the flag would be an ordinary exit.
                self.restart.store(true, Ordering::Release);
                self.shutdown();
                Ok(NodeReply::Ok)
            }
        }
    }

    /// Apply a `POST /discord` command. Join enrolls a receive-only conference
    /// member and connects the voice link; leave withdraws both.
    #[cfg(feature = "discord")]
    fn handle_discord(
        &self,
        action: crate::command::DiscordAction,
    ) -> Result<NodeReply, NodeError> {
        use crate::command::DiscordAction;
        // Serialize the ENTIRE Join or Leave operation. `server.rs` spawns
        // one worker thread per HTTP request against a shared
        // `Arc<NodeController>`, so a plain check-then-act on `discord`
        // (take the lock, check `is_some()`, release it, enrol, re-take the
        // lock to store) would let two concurrent Joins both observe "not
        // joined" and both enrol — the second store silently drops the
        // first `Bridge`, stranding its member. Deliberately NOT taken by
        // `snapshot()`, which only ever touches the shorter-held `discord`
        // mutex, so a Join in progress never blocks `/status`.
        let _op_guard = self.discord_op.lock().expect("discord op mutex poisoned");
        match action {
            DiscordAction::Join => {
                // Refuse a second Join outright rather than trying to be
                // self-healing: a silent leave-then-rejoin would hide the
                // fact that something was already connected, and it would
                // still need the explicit-refusal branch for any other
                // failure. Checked BEFORE add_bridge_member so a refused
                // Join enrolls nothing — a leaked member is worse than a
                // refused command. `Leave` is always available to recover,
                // and `/status`'s `connected` field makes the state
                // discoverable.
                if self
                    .discord
                    .lock()
                    .expect("discord bridge mutex poisoned")
                    .is_some()
                {
                    return Err(NodeError {
                        message: "discord: already joined; leave first".into(),
                    });
                }
                let cfg = self.discord_cfg.as_ref().ok_or_else(|| NodeError {
                    message: "discord: no [discord] section".into(),
                })?;
                // `SecretProvider::resolve` returns "" for an unknown username
                // (secrets.rs:49) — it is NOT an Option.
                let token = self.secrets.resolve("discord");
                if token.is_empty() {
                    return Err(NodeError {
                        message: "discord: no bot token (set it on the settings page, or POST \
                                  /secrets with username \"discord\")"
                            .into(),
                    });
                }
                // `None` has TWO causes and the Option cannot distinguish them:
                // the engine is not built yet (ConsoleSession builds `Manager`
                // lazily in `ensure_engine`, session.rs:3254 — a node that has not
                // started inbound/connected has none), or the topology is handset/
                // parrot. `DiscordError::NoConference`'s Display names both, so the
                // wording lives in one place.
                let handle = self.station.add_bridge_member().ok_or_else(|| NodeError {
                    message: astar_discord::DiscordError::NoConference.to_string(),
                })?;
                // Capture the id immediately: from here on, EVERY failure
                // must withdraw it — there is no `Drop` impl on
                // `BridgeMemberHandle` that would do it for us, and an
                // operator with a typo'd snowflake or a revoked token would
                // otherwise strand one member on every retry. The fallible
                // remainder is isolated in `complete_discord_join` so every
                // one of its early returns funnels through the single `Err`
                // arm below, instead of needing a `remove_bridge_member`
                // call copied next to each `?`.
                let member_id = handle.id;
                // The token goes no further than this check: the gateway
                // resolves its own from the same store when it needs one, so
                // nothing below this line handles a credential.
                drop(token);
                let result = self.complete_discord_join(handle, cfg);
                match result {
                    Ok(bridge) => {
                        *self
                            .discord_member
                            .lock()
                            .expect("discord member mutex poisoned") = Some(member_id);
                        *self.discord.lock().expect("discord bridge mutex poisoned") = Some(bridge);
                        Ok(NodeReply::Ok)
                    }
                    Err(e) => {
                        self.station.remove_bridge_member(member_id);
                        // **The status-line race.** `Bridge::join` posted
                        // "Not connected — ...: {reason}" into the channel
                        // microseconds ago, and `complete_discord_join`
                        // dropped the `Bridge` — and the voice handle inside
                        // it — the instant the join returned `Err`. That post
                        // is a REST call still in flight on the runtime
                        // thread, and nothing else waits for it: the
                        // `Shutdown` that `Gateway::drop` sends is what makes
                        // it land, because the command loop's `Shutdown` arm
                        // waits (bounded) for the last status post before the
                        // gateway closes. So the gateway must go down HERE,
                        // at the same moment the bridge did — not later, and
                        // not never.
                        //
                        // On the persistent path there is no teardown to race
                        // at all: the runtime keeps running, the post lands in
                        // its own time, and the bot stays online to answer
                        // commands. Either way the line is reliable; only the
                        // reason differs.
                        //
                        // A gateway is also simply not wanted once a join has
                        // failed on the non-persistent path — it exists to
                        // serve a voice session that does not exist.
                        //
                        // Taken OUT of the mutex into a local so the guard is
                        // released at the end of that statement: `Gateway`'s
                        // `Drop` sends `Shutdown` and joins the runtime
                        // thread, up to ~30 s worst case, and `pump()` runs
                        // every 50 ms. Nothing may block behind a teardown
                        // while holding a lock the pump takes.
                        if !self.discord_gateway_is_persistent() {
                            let gateway = self
                                .discord_gateway
                                .lock()
                                .expect("discord gateway mutex poisoned")
                                .take();
                            drop(gateway);
                        }
                        Err(e)
                    }
                }
            }
            DiscordAction::Leave => {
                // Take the bridge OUT of the lock into a local first, and let
                // the guard drop at the end of this statement. Under edition
                // 2024 an `if let Some(mut b) = <guard>.take()` keeps the
                // `MutexGuard` temporary alive until the end of the `if`
                // body, so `b.leave()` — which now waits for the runtime
                // thread's verdict on the disconnect, because the status line
                // it posts asserts one — would run INSIDE the critical
                // section. That is the same mutex `pump()` takes on the
                // node's 50 ms tick, so a slow Discord leave would stall
                // station-event draining and link keying for seconds. The
                // gateway teardown below is out of the lock for the same
                // reason, only more so: it joins a thread.
                let taken = self
                    .discord
                    .lock()
                    .expect("discord bridge mutex poisoned")
                    .take();
                if let Some(mut b) = taken {
                    let _ = b.leave();
                }
                // Withdraw the conference member whether or not `b.leave()`
                // succeeded above — a failed Discord disconnect must not
                // leave a member stranded in the conference's member table,
                // receiving mix frames forever with no visible trace beyond
                // a slowly growing member count.
                // Same shape as above, and for the same reason: take, drop
                // the guard, then call into the station. `pump()` reads this
                // mutex too (the periodic channel ID), and it already uses
                // this take-then-act form.
                let member = self
                    .discord_member
                    .lock()
                    .expect("discord member mutex poisoned")
                    .take();
                if let Some(member_id) = member {
                    self.station.remove_bridge_member(member_id);
                }
                // A non-persistent gateway exists only to serve a voice
                // session, so the Leave that ends the session ends the
                // gateway too and the bot goes offline — exactly what
                // happened when the runtime lived inside the voice link.
                // Persistent, and it stays up: the bot remains online and
                // answerable while it sits in no channel at all. That single
                // branch is the whole behaviour change here.
                //
                // Dropped AFTER the bridge above, and that ordering is the
                // status line: `b.leave()` posted "Disconnected — ..." and
                // the post is still in flight on the runtime thread, where
                // `Gateway::drop`'s `Shutdown` waits for it before closing.
                // Dropping the gateway first would abandon the line this
                // command exists to make true.
                //
                // Same take-into-a-local shape as everything above: under
                // edition 2024 the guard temporary would otherwise live to
                // the end of the enclosing expression, and `Gateway::drop`
                // joins the runtime thread for up to ~30 s. `pump()` must
                // never queue behind that.
                if !self.discord_gateway_is_persistent() {
                    let gateway = self
                        .discord_gateway
                        .lock()
                        .expect("discord gateway mutex poisoned")
                        .take();
                    drop(gateway);
                }
                Ok(NodeReply::Ok)
            }
        }
    }

    /// Bring the bot online if it is not already. Idempotent.
    ///
    /// The gateway is the connection, not the voice session: it owns the
    /// runtime thread, the serenity client and the reconnect supervisor, and
    /// a `Join` merely takes a handle onto it. Starting it is therefore
    /// cheap to ask for repeatedly and safe to ask for from anywhere that
    /// needs the bot reachable.
    ///
    /// The token is resolved here, from the same secret store `POST
    /// /secrets` and `DISCORD_BOT_TOKEN` feed, and handed straight to
    /// `Gateway::connect`, which keeps it on the runtime thread alone: it is
    /// never stored on this controller, never logged, and never reaches a
    /// `NodeError`. Resolving it here rather than taking it as an argument
    /// is what lets anything that needs the bot online — a join today, a
    /// start-up path tomorrow — ask for it without handling a credential.
    ///
    /// # Errors
    /// [`NodeError`] when no token has been supplied, or when the runtime
    /// thread cannot be spawned. The message is `DiscordError`'s own and
    /// carries no credential.
    #[cfg(feature = "discord")]
    fn ensure_discord_gateway(&self) -> Result<(), NodeError> {
        // **Installed is not the same as working.** A `Gateway` whose runtime
        // thread has died — a panic caught by its `catch_unwind`, or the
        // supervisor task found gone — can never recover on its own: the
        // thread that would do the reconnecting is the one that died, and
        // every send from every handle it produced already fails. Treating
        // it as "a gateway is up" would leave the bot offline until the
        // daemon restarts, which is harmless while a Leave tears the gateway
        // down anyway and a permanent wedge once it is persistent. So a dead
        // one is taken out and replaced rather than reused.
        //
        // Decided inside one critical section so nothing can change between
        // the check and the take; the corpse leaves as a local and is
        // dropped BELOW, outside the guard, because `Gateway::drop` joins
        // the runtime thread.
        let dead = {
            let mut guard = self
                .discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned");
            match guard.as_ref() {
                // Up and working: this call has nothing to do. Callers reach
                // here under `discord_op`, which is what makes the rest of
                // this function's check-then-act safe rather than merely
                // likely.
                Some(g) if !g.is_failed() => return Ok(()),
                Some(_) => guard.take(),
                None => None,
            }
        };
        drop(dead);
        // `SecretProvider::resolve` returns "" for an unknown username
        // (secrets.rs:49) — it is NOT an Option — and `Gateway::connect`
        // turns an empty token into `NotConfigured` without spawning
        // anything, so the emptiness check is its, not ours.
        // A commanding gateway needs its guild BEFORE anything is spawned:
        // the same invariant `complete_discord_join` keeps for the join
        // path, for the same reason — a typo must fail network-free, and
        // this is the only failure here a test can reach without touching
        // Discord's real servers.
        let opts = if self.discord_gateway_is_persistent() {
            let cfg = self.discord_cfg.as_ref().ok_or_else(|| NodeError {
                message: "discord: no [discord] section".into(),
            })?;
            let guild: u64 = cfg.guild_id.parse().map_err(|_| NodeError {
                message: "discord: guild_id is not a snowflake".into(),
            })?;
            let (tx, rx) = std::sync::mpsc::channel();
            Some((
                astar_discord::GatewayOptions {
                    guild,
                    commands: true,
                    events: tx,
                },
                rx,
            ))
        } else {
            // Byte-identical to what shipped: no commands registered, no
            // events, and the gateway dies with the voice session.
            None
        };
        let token = self.secrets.resolve("discord");
        let connected =
            astar_discord::Gateway::connect_with(&token, opts.as_ref().map(|(o, _)| o.clone()));
        drop(token);
        let gateway = connected.map_err(|e| NodeError {
            message: e.to_string(),
        })?;
        // Only now that a gateway exists does its receiver become the live
        // one. This also REPLACES the receiver of a gateway that just died:
        // whatever was still queued on it describes a channel state from
        // before the disconnect, and the new session opens with its own
        // `Ready` — which clears the commander's roster, releases the mic
        // and re-sweeps — so replaying the corpse's tail would be applying
        // a reality that no longer exists. The commander itself is NOT
        // rebuilt: approvals, admins, bans and the link survive a reconnect.
        //
        // Clearing and installing happen in ONE critical section, holding
        // the events lock across the clear. Two sections would race the
        // pump, which does not take `discord_op`: a tick landing between
        // them drains the NEW receiver into the pending queue and the
        // `clear()` then wipes it — losing the new session's `Ready` and
        // its voice-state sweep, which is exactly the re-mute of everyone
        // after a reconnect.
        //
        // LOCK ORDER: `discord_events` → `discord_pending_events`. The only
        // other place that takes both is `drain_discord_events`, which
        // takes them sequentially (the events guard is dropped at the end
        // of its `let` block, before the queue is locked) and never in the
        // opposite nesting, so this is the only nesting in the crate and
        // there is nothing for it to deadlock against.
        if let Some((_, rx)) = opts {
            self.install_discord_events(rx);
            // The new gateway's `TrackSupervisor` starts with fresh audio
            // health, so the fault the OLD one recorded can never be
            // cleared by an `AudioRecovered` that will now never come. Told
            // here, beside the events channel, because this is the one
            // place that knows a replacement happened.
            self.note_discord_gateway_replaced();
        }
        // `replace`, and the displaced value dropped OUTSIDE the guard's
        // statement. Under `discord_op` there can be nothing to displace —
        // but if there ever were, `Gateway::drop` joins the runtime thread
        // for up to ~30 s, and that must never happen while this mutex is
        // held.
        let displaced = self
            .discord_gateway
            .lock()
            .expect("discord gateway mutex poisoned")
            .replace(gateway);
        drop(displaced);
        Ok(())
    }

    /// Bring the bot online at node start, when `[discord] approvers` makes
    /// the gateway persistent. A node with no approvers is left exactly as
    /// it was: its gateway still starts lazily on the first join.
    ///
    /// Idempotent, and it must be: `main.rs` runs `join_at_start` first, and
    /// that path already reaches `ensure_discord_gateway`. Calling this
    /// afterwards finds a healthy gateway and returns without connecting a
    /// second one. Also called when a `discord` secret arrives over
    /// `ProvideSecret`, because with `[secrets] source = "control"` the
    /// token does not exist at start-up at all.
    ///
    /// # Errors
    /// [`NodeError`] when no token has been supplied yet, when `guild_id` is
    /// not a snowflake, or when the runtime thread cannot be spawned. Every
    /// caller logs it and carries on: Discord being unreachable must never
    /// stop a node coming up on the air.
    #[cfg(feature = "discord")]
    pub fn start_discord_gateway(&self) -> Result<(), NodeError> {
        if !self.discord_gateway_is_persistent() {
            return Ok(());
        }
        // The same lock `handle_discord` takes around a whole Join/Leave.
        // `ensure_discord_gateway`'s check-then-act is only safe under it,
        // and this is the one caller that does not arrive through
        // `handle_discord`.
        let _op_guard = self.discord_op.lock().expect("discord op mutex poisoned");
        self.ensure_discord_gateway()
    }

    /// Tear the Discord side down on the way out of a run, in the order the
    /// dropped-controller path would have used — and would never actually
    /// run.
    ///
    /// **Why this exists at all.** `serve` wraps the controller in an `Arc`
    /// and moves a clone into the ctrlc handler's closure, which lives for
    /// the life of the process, so the controller is never dropped: no
    /// `Bridge` drop, no `Gateway::drop`, and therefore no
    /// `GatewayCmd::Shutdown` — which is the one thing that makes the
    /// runtime thread wait for the queued unmutes before it closes. The
    /// field-order comments above describe a teardown the shipped daemon
    /// had no way to reach. This is that teardown, made explicit.
    ///
    /// **Where it is called from, and on which thread.** At the END of the
    /// run, on the MAIN thread, after the pump thread has been joined — not
    /// from the signal handler. The signal thread races `main` returning,
    /// and `Gateway::drop` joins a thread that can take seconds: a teardown
    /// the process exits out from under is the bug being fixed, not a fix
    /// for it. After the pump join because nothing may hand the rule engine
    /// new work while its mutes are being swept up.
    ///
    /// The order below is the drop order the struct documents:
    ///
    /// 1. the rule engine's sweep, which ENQUEUES one unmute per member it
    ///    muted;
    /// 2. the conference member and the `Bridge`, whose voice handle asks to
    ///    leave the channel;
    /// 3. the `Gateway`, whose `Drop` sends the `Shutdown` that those
    ///    unmutes are now queued in front of — and the command loop waits
    ///    for them, bounded, before the gateway closes.
    ///
    /// Each value is taken out of its mutex into a local and the guard
    /// released BEFORE the drop. Under edition 2024 an
    /// `if let Some(x) = m.lock()…take()` keeps the guard alive to the end
    /// of the `if` body, which would run a thread join inside a critical
    /// section the pump also takes.
    ///
    /// No "Disconnected" line is posted: `Bridge::leave` is what posts one,
    /// and dropping the bridge — which is what the controller's own drop
    /// would have done — does not.
    ///
    /// Best effort, and honestly so: a member who is not in a voice channel
    /// cannot be unmuted by anyone, so their mark stays in `muted_by_us` for
    /// the next start.
    #[cfg(feature = "discord")]
    pub fn teardown_discord(&self) {
        // Idempotent, and cheap when an explicit `Shutdown` already ran it.
        self.shutdown_discord_commander();

        // Withdraw the conference member before the bridge that feeds it
        // goes away, exactly as the `Leave` arm does, so nothing is left in
        // the member table taking mix frames with nobody to hand them to.
        let member = self
            .discord_member
            .lock()
            .expect("discord member mutex poisoned")
            .take();
        if let Some(member_id) = member {
            self.station.remove_bridge_member(member_id);
        }

        let bridge = self
            .discord
            .lock()
            .expect("discord bridge mutex poisoned")
            .take();
        drop(bridge);

        let gateway = self
            .discord_gateway
            .lock()
            .expect("discord gateway mutex poisoned")
            .take();
        drop(gateway);
    }

    /// The same entry point in a build without the feature, so `run.rs`
    /// needs no `cfg` of its own. There is nothing to tear down.
    #[cfg(not(feature = "discord"))]
    #[allow(clippy::unused_self)]
    pub fn teardown_discord(&self) {}

    /// True when the gateway must outlive a `Leave` — when the bot has to
    /// stay online with no voice session under it.
    ///
    /// **`[discord] approvers` is the whole condition.** A node that names
    /// approvers is running slash commands, and a bot that goes offline the
    /// moment it leaves the voice channel cannot answer one; a node that
    /// names none is the audio-only bridge that shipped first, where every
    /// gateway is torn down with the session that started it. So this is
    /// also the master switch for registering commands at all, for the
    /// events channel, and for the commander running on the pump — one
    /// config key, one expression, read everywhere the difference matters.
    #[cfg(feature = "discord")]
    pub(crate) fn discord_gateway_is_persistent(&self) -> bool {
        self.discord_cfg
            .as_ref()
            .is_some_and(|c| !c.approvers.is_empty())
    }

    /// Make `rx` the live events receiver, dropping whatever the previous
    /// session left queued.
    ///
    /// Its own function so the clear and the install cannot drift apart —
    /// see the block comment above the call site for why they are one
    /// critical section and what the lock order is.
    #[cfg(feature = "discord")]
    pub(crate) fn install_discord_events(
        &self,
        rx: std::sync::mpsc::Receiver<astar_discord::GatewayEvent>,
    ) {
        let mut events = self
            .discord_events
            .lock()
            .expect("discord events mutex poisoned");
        self.discord_pending_events
            .lock()
            .expect("discord pending events mutex poisoned")
            .clear();
        *events = Some(rx);
    }

    /// The node numbers currently in the link roster.
    ///
    /// One of the narrow doors the Discord adapter is given onto the station
    /// (see the `station` field's doc for the list). It exists so `station`
    /// itself can stay private.
    #[cfg(feature = "discord")]
    pub(crate) fn link_roster_node_ids(&self) -> Vec<String> {
        self.station
            .link_roster()
            .links
            .into_iter()
            .map(|l| l.node)
            .collect()
    }

    /// `(reflector, module)` of the node's M17 leg while it is LINKED
    /// (`linked_at` set by the pump when the ACKN edge arrived), and `None`
    /// while it is still linking or there is none.
    ///
    /// Linked-only is the rule the Discord `tick_link` rests on: a leg
    /// enters the roster at ACKN, so "Connected" means the reflector
    /// accepted. The mutex is held for one clone and released.
    #[cfg(feature = "discord")]
    pub(crate) fn m17_linked(&self) -> Option<(String, char)> {
        self.m17
            .lock()
            .expect("m17 state mutex poisoned")
            .leg
            .as_ref()
            .filter(|l| l.linked_at.is_some())
            .map(|l| (l.reflector.clone(), l.module))
    }

    /// Why the last M17 leg ended, when `target` (`"M17-USA C"`) names that
    /// leg. A successful Monitor clears `last_ended`, so a re-dial never
    /// inherits the previous leg's ending.
    #[cfg(feature = "discord")]
    pub(crate) fn m17_end_reason(&self, target: &str) -> Option<String> {
        self.m17
            .lock()
            .expect("m17 state mutex poisoned")
            .last_ended
            .as_ref()
            .filter(|e| format!("{} {}", e.reflector, e.module) == target)
            .map(|e| e.reason.clone())
    }

    /// Whether a voice bridge is installed — all the rule engine is allowed
    /// to know about it.
    #[cfg(feature = "discord")]
    pub(crate) fn discord_bridge_joined(&self) -> bool {
        self.discord
            .lock()
            .expect("discord bridge mutex poisoned")
            .is_some()
    }

    /// The station, for tests in sibling modules that have to set up real
    /// audio state (enrolling a conference member, say). Test-only: the
    /// production path has no business holding this.
    #[cfg(test)]
    pub(crate) fn station_for_test(&self) -> &Station {
        &self.station
    }

    /// Install a voice bridge without going through a Join. Test-only, for
    /// the same reason as above.
    #[cfg(all(test, feature = "discord"))]
    pub(crate) fn set_discord_bridge_for_test(&self, bridge: astar_discord::Bridge) {
        *self.discord.lock().expect("discord bridge mutex poisoned") = Some(bridge);
    }

    /// The fallible remainder of a Join once a conference member has
    /// already been enrolled by `add_bridge_member`: parse the snowflakes,
    /// connect the voice link, build the `Bridge`, join the channel, and
    /// announce. Isolated into its own function so `handle_discord` has
    /// exactly ONE place that must withdraw the member on failure — the
    /// `Err` arm of this function's caller — rather than needing the
    /// withdrawal call copied next to every `?` in here.
    ///
    /// Snowflake parsing runs BEFORE `ensure_discord_gateway` on purpose: a
    /// typo'd `guild_id`/`channel_id` must fail before any network thread
    /// is spawned, not after — both for a faster, network-free failure and
    /// because it is the only failure in this function that can be
    /// exercised by a test without touching Discord's real servers. That
    /// ordering is load-bearing and pinned by
    /// `a_failed_join_leaves_no_gateway_behind_when_not_persistent`.
    #[cfg(feature = "discord")]
    fn complete_discord_join(
        &self,
        handle: astar_iax::BridgeMemberHandle,
        cfg: &crate::config::DiscordCfg,
    ) -> Result<astar_discord::Bridge, NodeError> {
        let rate = handle.sample_rate;
        // Keep the member id: the join notice and the periodic ID are
        // announced onto this member's leg (Task 8), not spoken by the
        // voice link — astar-discord has no TTS and must not grow one.
        let member_id = handle.id;
        let guild: u64 = cfg.guild_id.parse().map_err(|_| NodeError {
            message: "discord: guild_id is not a snowflake".into(),
        })?;
        let channel: u64 = cfg.channel_id.parse().map_err(|_| NodeError {
            message: "discord: channel_id is not a snowflake".into(),
        })?;
        // Only now, with both ids known good, is the bot brought online. A
        // gateway may already be up — it outlives voice sessions — in which
        // case this does nothing.
        self.ensure_discord_gateway()?;
        // A voice handle is two clones (a channel sender and a failure
        // flag), so the gateway mutex is held for exactly that and released
        // at the end of this statement. Nothing here calls into the runtime
        // thread, and nothing may: `pump()` takes locks on a 50 ms tick.
        let link = self
            .discord_gateway
            .lock()
            .expect("discord gateway mutex poisoned")
            .as_ref()
            .map(astar_discord::Gateway::voice)
            .ok_or_else(|| NodeError {
                message: "discord: the gateway went offline before the voice link was taken".into(),
            })?;
        let mut bridge = astar_discord::Bridge::new(
            Box::new(astar_discord::ConferenceLink::new(handle)),
            Box::new(link),
            Box::new(astar_discord::Bypass::new(rate)),
            self.discord_node_label.clone(),
        );
        // `Bridge::join` posts the text status line itself — "connected" on
        // success, "not connected" with the reason on failure — because the
        // `Bridge` is what owns the voice link and therefore the only thing
        // that can post at all. Nothing here needs to remember to do it, and
        // a failure that returns through the `?` below has already said so in
        // the channel.
        //
        // The member above is enrolled BEFORE this join, and unavoidably so:
        // `handle.sample_rate` is what the `PostFilter` and `FramePipeline`
        // are built from, and the handle only exists once the conference has
        // taken the member. The conference then feeds it a frame every 20 ms
        // through an unbounded channel while this call sits through a
        // multi-second gateway handshake — so `Bridge::join` discards that
        // accumulation on success, before the first `pump()`. Without it the
        // channel would run a join-duration behind the net permanently.
        bridge.join(guild, channel).map_err(|e| NodeError {
            message: e.to_string(),
        })?;
        if cfg.announce_on_join {
            // Onto the bridge member's leg only. This reaches Discord
            // through the ordinary audio path and CANNOT reach the air: no
            // mic lane is keyed and the conference mix is untouched.
            let _ = self.station.announce_to_bridge_member(
                member_id,
                astar_iax::AnnounceRequest::say(JOIN_NOTICE),
                std::time::Duration::ZERO,
            );
        }
        Ok(bridge)
    }

    /// Without the `discord` feature the verb exists and always refuses, so a
    /// client gets a clear answer rather than a 404 that looks like a typo.
    #[cfg(not(feature = "discord"))]
    #[allow(clippy::unused_self)] // `&self` is intentional: kept feature-independent so call sites never need their own `#[cfg]`.
    fn handle_discord(
        &self,
        _action: crate::command::DiscordAction,
    ) -> Result<NodeReply, NodeError> {
        Err(NodeError {
            message: "discord: this build has no discord feature".into(),
        })
    }

    /// Apply a `POST /link` command (iax-d829.1): connect (transceive) /
    /// monitor (RX-only) / disconnect a node-to-node link. `addr`, when
    /// present, dials an explicit loopback `ip:port` (harness / localhost),
    /// bypassing `AllStar` DNS; anything else is refused before the roster is
    /// touched (see [`link_addr_is_loopback`]).
    ///
    /// `AllStar` `ilink` semantics: `*3`/`*2` on an EXISTING link switches its
    /// mode in place (`link_set_mode`) instead of dialing again — the roster is
    /// consulted BEFORE any resolution, so upgrading a link never touches DNS.
    ///
    /// Per-target shape and secret are resolved via `link_dial_params`
    /// (iax-5029): `Standard` presents `node_id` as caller-id with the
    /// `link:<node>` secret (`""` on miss); `WtGuest` uses the fixed `AllStar`
    /// guest identity and the WT wire shape, with a freshly minted portal
    /// token as `CALLING_NAME` when `[portal]` is configured (iax-b7f2).
    /// Resolution (and the mint) happens only on the dial path — mode
    /// switches and disconnects touch neither secrets nor the portal.
    pub(crate) fn handle_link(
        &self,
        action: LinkAction,
        node: &str,
        addr: Option<&str>,
    ) -> Result<NodeReply, NodeError> {
        if addr.is_some_and(|a| !link_addr_is_loopback(a)) {
            return Err(NodeError {
                message: LINK_ADDR_NOT_LOOPBACK.to_string(),
            });
        }
        if let Some(mode) = link_mode_for(action) {
            {
                let already_linked = self
                    .station
                    .link_roster()
                    .links
                    .iter()
                    .any(|l| l.node == node);
                if already_linked {
                    self.station
                        .link_set_mode(node, mode)
                        .map_err(|e| station_err(&e))?;
                } else {
                    // iax-9e02: announce BEFORE the dial so the operator hears
                    // which node is being reached even when the dial fails.
                    self.fire_link_announcement(true, node);
                    let shape = self
                        .link_shapes
                        .get(node)
                        .copied()
                        .unwrap_or(LinkShape::Standard);
                    // wt-guest: the ASL3 `[allstar-public]` dialplan validates
                    // `CALLING_NAME` server-side (authwebphone.pl) and clears
                    // tokenless calls ~1 s after answer — mint a fresh portal
                    // token per dial (iax-b7f2). No portal config / mint
                    // failure falls back to the node-id name (pre-b7f2
                    // behavior, rejected by WT contexts but harmless).
                    // Blocking HTTPS; runs on the HTTP worker (or, for
                    // DTMF-initiated dials, the pump — see iax-b7f2 notes).
                    let wt_token = if shape == LinkShape::WtGuest {
                        self.station.mint_wt_token().ok()
                    } else {
                        None
                    };
                    let (caller_id, secret, call_mode) = link_dial_params(
                        shape,
                        node,
                        self.node_id.as_deref(),
                        wt_token.as_deref(),
                        &self.secrets,
                    );
                    match addr {
                        Some(a) => self
                            .station
                            .link_connect_at(node, a, mode, &caller_id, &secret, call_mode, false),
                        None => self
                            .station
                            .link_connect(node, mode, &caller_id, &secret, call_mode, false),
                    }
                    .map_err(|e| station_err(&e))?;
                }
            }
        } else {
            // iax-9e02: tear the link down FIRST, then announce — so the
            // announcement is never transmitted over the link being dropped
            // (operator's explicit ordering, 2026-08-03).
            self.station
                .link_disconnect(node)
                .map_err(|e| station_err(&e))?;
            self.fire_link_announcement(false, node);
        }
        Ok(NodeReply::Ok)
    }

    /// Apply a `POST /m17` command (and, through the Discord adapter,
    /// `/dial m17` and `/hangup`).
    ///
    /// Monitor runs [`m17_preflight`] and only then asks the station for a
    /// leg. The station resolves the host, loads the decoder and sends CONN
    /// on the monitor's own thread, so nothing here blocks on DNS.
    /// Disconnect takes the record and joins the monitor holding no
    /// controller mutex but `m17_op`. `drain_m17` only ever `try_lock`s it
    /// and `snapshot()` never takes it, so neither waits on a join.
    ///
    /// There is no transmit verb to dispatch: [`M17Action`] has two variants
    /// and neither reaches `set_ptt`.
    pub(crate) fn handle_m17(
        &self,
        action: M17Action,
        reflector: Option<&str>,
        module: Option<char>,
        callsign: Option<&str>,
    ) -> Result<NodeReply, NodeError> {
        let _op = self.m17_op.lock().expect("m17 op mutex poisoned");
        match action {
            M17Action::Monitor => self.m17_monitor(reflector, module, callsign),
            M17Action::Disconnect => self.m17_hang_up(),
        }
    }

    /// Monitor, under `m17_op`.
    fn m17_monitor(
        &self,
        reflector: Option<&str>,
        module: Option<char>,
        callsign: Option<&str>,
    ) -> Result<NodeReply, NodeError> {
        let (settings, live) = {
            let st = self.m17.lock().expect("m17 state mutex poisoned");
            (st.settings.clone(), st.leg.clone())
        };
        let target = m17_preflight(
            settings.as_ref(),
            reflector,
            module,
            callsign,
            || self.station.m17_available(),
            live.as_ref(),
        )?;
        // Recorded BEFORE the station call, so a concurrent /status (snapshot()
        // never takes m17_op) shows the leg as linking from the moment its monitor
        // exists; drain_m17 cannot stamp or reap it until this body releases
        // m17_op. A refusal below puts everything back as it was.
        let prev_ended = {
            let mut st = self.m17.lock().expect("m17 state mutex poisoned");
            st.leg = Some(M17LegRecord {
                reflector: target.reflector.clone(),
                module: target.module,
                callsign: target.callsign.clone(),
                linked_at: None,
            });
            st.last_ended.take()
        };
        let req = M17MonitorRequest {
            host: target.entry.host,
            port: target.entry.port,
            module: target.module,
            callsign: target.callsign,
        };
        if let Err(e) = self.station.m17_monitor_connect(req) {
            let mut st = self.m17.lock().expect("m17 state mutex poisoned");
            st.leg = None;
            st.last_ended = prev_ended;
            return Err(station_err(&e));
        }
        self.broadcast_m17("linking", &target.reflector, target.module, None);
        // Tell the people on this node that the conference now carries a
        // reflector. Only once m17_monitor_connect has returned Ok: a dial
        // rejected by pre-flight or by the station announces nothing. A later
        // reflector refusal is announced by drain_m17's Ended arm.
        self.fire_m17_announcement(true, &target.reflector, target.module);
        Ok(NodeReply::Ok)
    }

    /// Disconnect, under `m17_op`.
    fn m17_hang_up(&self) -> Result<NodeReply, NodeError> {
        let leg = self
            .m17
            .lock()
            .expect("m17 state mutex poisoned")
            .leg
            .take();
        let Some(leg) = leg else {
            return Err(NodeError {
                message: M17_NOTHING_UP.to_string(),
            });
        };
        // DISC to the reflector, member withdrawn, thread joined, with the
        // state mutex already released.
        self.station.m17_monitor_disconnect();
        self.broadcast_m17(
            "disconnected",
            &leg.reflector,
            leg.module,
            Some(M17_END_HUNG_UP),
        );
        // After the monitor is gone, so the phrase describes what already
        // happened.
        self.fire_m17_announcement(false, &leg.reflector, leg.module);
        Ok(NodeReply::Ok)
    }

    /// Tear the M17 leg down because the node is going away. Idempotent:
    /// `fn shutdown` (so both `NodeCommand::Shutdown` and
    /// `NodeCommand::Restart`) and the end of `run_serve` call it, and
    /// either may be first. The reflector always gets DISC. `Hangup` does not
    /// send one, because `Station::disconnect` leaves the monitor alone on
    /// purpose.
    pub fn teardown_m17(&self) {
        let _op = self.m17_op.lock().expect("m17 op mutex poisoned");
        let leg = self
            .m17
            .lock()
            .expect("m17 state mutex poisoned")
            .leg
            .take();
        self.station.m17_monitor_disconnect();
        if let Some(leg) = leg {
            self.broadcast_m17(
                "disconnected",
                &leg.reflector,
                leg.module,
                Some(M17_END_SHUTDOWN),
            );
        }
    }

    /// Pump step: turn the monitor's lifecycle edges into `NodeEvent::M17`.
    ///
    /// `Station::m17_monitor_poll` also reaps a leg that failed (member
    /// withdrawn, thread joined off the session lock), so this is where a
    /// NACK, a reflector DISC or a silent reflector becomes a torn-down leg.
    /// There is no reconnect: a reflector that refused or dropped the node
    /// must not keep receiving CONNs carrying someone's callsign.
    ///
    /// A `handle_m17` or `teardown_m17` body broadcasts its own `linking` or
    /// `disconnected` after its station call returns. If the pump reported
    /// an edge in that window, a subscriber could see `disconnected` for a
    /// fast failure before the `linking` that started it. So the pump only
    /// polls when `m17_op` is free, and otherwise leaves the edge queued in
    /// the session for the next tick (50 ms). `try_lock` never waits, so
    /// `drain_m17` never blocks on a DISC or a join. A poisoned `m17_op` also
    /// skips, which leaves the edge queued rather than panicking the pump.
    fn drain_m17(&self) {
        let Ok(op_guard) = self.m17_op.try_lock() else {
            return;
        };
        let Some(edge) = self.station.m17_monitor_poll() else {
            return;
        };
        match edge {
            M17MonitorEdge::Linked => {
                let linked = {
                    let mut st = self.m17.lock().expect("m17 state mutex poisoned");
                    st.leg.as_mut().map(|leg| {
                        leg.linked_at = Some(std::time::Instant::now());
                        (leg.reflector.clone(), leg.module)
                    })
                };
                if let Some((reflector, module)) = linked {
                    // The reflector's audio now reaches every member the
                    // conference sends to. Say so in the log, with how many.
                    let reach = self.m17_relay_counts();
                    tracing::warn!(
                        reflector = %reflector,
                        module = %module,
                        allstar = reach.allstar,
                        echolink = reach.echolink,
                        "m17 monitor linked: relaying {reflector} {module} to {} AllStar connection(s) and {} EchoLink station(s); a linked node with a radio transmits it",
                        reach.allstar,
                        reach.echolink,
                    );
                    self.broadcast_m17("connected", &reflector, module, None);
                }
            }
            M17MonitorEdge::Ended(reason) => {
                let ended = {
                    let mut st = self.m17.lock().expect("m17 state mutex poisoned");
                    let ended = st.leg.take().map(|leg| M17Ended {
                        reflector: leg.reflector,
                        module: leg.module,
                        reason,
                    });
                    if let Some(e) = &ended {
                        st.last_ended = Some(e.clone());
                    }
                    ended
                };
                if let Some(e) = ended {
                    // One journald line per leg that ended on its own: which
                    // reflector dropped the node, and why. Secret-free, and
                    // the callsign is not repeated here.
                    eprintln!(
                        "astar-server: m17: {} {} ended: {}",
                        e.reflector, e.module, e.reason
                    );
                    self.broadcast_m17("disconnected", &e.reflector, e.module, Some(&e.reason));
                    // The edge is reported; release m17_op before speaking so
                    // a slow TTS synth never holds it on the pump (a POST /m17
                    // or /hangup would otherwise wait on the synth).
                    drop(op_guard);
                    // A reflector refusal, silence or a lost conference ends
                    // the leg without anyone asking; members still hear that
                    // it went.
                    self.fire_m17_announcement(false, &e.reflector, e.module);
                }
            }
        }
    }

    /// One `NodeEvent::M17` edge followed by a fresh `Snapshot`, the pair
    /// every SSE subscriber gets for every M17 transition.
    fn broadcast_m17(&self, kind: &str, reflector: &str, module: char, reason: Option<&str>) {
        self.broadcast(&NodeEvent::M17 {
            kind: kind.to_string(),
            reflector: reflector.to_string(),
            module: module.to_string(),
            reason: reason.map(str::to_string),
        });
        self.broadcast(&NodeEvent::Snapshot(self.snapshot()));
    }

    /// Who the M17 monitor's audio reaches right now: the IAX2 legs and
    /// `EchoLink` stations the conference sends to. See [`crate::m17_relay`].
    #[must_use]
    pub(crate) fn m17_relay_counts(&self) -> RelayCounts {
        let s = self.snapshot();
        RelayCounts::from_parts(&s.calls, &s.links, s.echolink.as_ref())
    }

    /// The newest station the monitor heard, as (callsign, seconds ago).
    /// Used by the snapshot and by the Discord adapter's "last heard".
    #[must_use]
    pub(crate) fn m17_last_heard(&self) -> Option<(String, u64)> {
        self.station
            .m17_monitor_heard()
            .into_iter()
            .next()
            .map(|h| (h.callsign, h.age_ms / 1000))
    }

    /// `/status`'s view of a leg: the record (a clone, lock already
    /// released) plus the monitor's own poll-cheap status.
    fn m17_link_status(
        &self,
        leg: M17LegRecord,
        directory_generated: Option<String>,
    ) -> M17LinkStatus {
        let status = self.station.m17_monitor_status();
        let heard = self.m17_last_heard();
        M17LinkStatus {
            target: format!("{} {}", leg.reflector, leg.module),
            module: leg.module.to_string(),
            reflector: leg.reflector,
            callsign: leg.callsign,
            state: status.as_ref().map_or("linking", |s| s.link).to_string(),
            receiving: status.as_ref().is_some_and(|s| s.receiving),
            talker: heard.as_ref().map(|(call, _)| call.clone()),
            talker_secs_ago: heard.map(|(_, secs)| secs),
            up_secs: leg.linked_at.map(|t| t.elapsed().as_secs()),
            dropped_packets: status.map_or(0, |s| s.dropped_packets),
            directory_generated,
        }
    }

    /// Apply a `POST /bridge` partial update (iax-647d): merge onto the current
    /// config (omitted fields keep their value) and re-wire live calls.
    fn set_bridge(
        &self,
        mode: Option<String>,
        mix_minus: Option<bool>,
        include_local_radio: Option<bool>,
    ) -> Result<NodeReply, NodeError> {
        let cur = self.station.bridge_config();
        let mode = match mode {
            None => cur.mode,
            Some(s) => parse_bridge_mode(&s).ok_or_else(|| NodeError {
                message: format!(
                    "bridge.mode: unknown value {s:?} (expected \"handset\", \"bridge\", \"conference\", or \"parrot\")"
                ),
            })?,
        };
        let new_cfg = astar_iax::BridgeConfig {
            mode,
            mix_minus: mix_minus.unwrap_or(cur.mix_minus),
            include_local_radio: include_local_radio.unwrap_or(cur.include_local_radio),
            // iax-feab: no partial-update knob for parrot tuning yet; preserve
            // whatever the current config carries.
            parrot: cur.parrot,
        };
        self.station
            .set_bridge_config(new_cfg)
            .map_err(|e| station_err(&e))?;
        Ok(NodeReply::Ok)
    }

    /// Apply a `POST /echolink` command: dial a listed station into the
    /// conference, or hang up on one (every one, with no target).
    ///
    /// A dial runs [`echolink_node::dial_preflight`] and only then asks the
    /// station. No controller mutex is held across a station call; the
    /// session's own lock makes the leg's one-entry-per-station check
    /// atomic, so two concurrent dials for one station get one call and an
    /// "already connected" refusal.
    pub(crate) fn handle_echolink(
        &self,
        action: EchoLinkAction,
        target: Option<&str>,
    ) -> Result<NodeReply, NodeError> {
        let (cfg, setup_error) = {
            let st = self.echolink.lock().expect("echolink state mutex poisoned");
            (st.cfg.clone(), st.setup_error.clone())
        };
        match action {
            EchoLinkAction::Dial => {
                let login = self.station.echolink_state().map(|s| s.login);
                let target = echolink_node::dial_preflight(
                    cfg.as_ref(),
                    setup_error.as_deref(),
                    login.as_ref(),
                    target,
                    |t| echolink_node::is_listed(&self.station.echolink_stations(), t),
                    || {
                        // Refused inside the 30 s floor; the sentence says
                        // "refreshing" either way, and a list is due soon.
                        let _ = self.station.echolink_refresh_list();
                    },
                )?;
                self.station
                    .echolink_node_connect(&target)
                    .map_err(|e| match e {
                        StationError::AlreadyConnected => NodeError {
                            message: echolink_node::already_on_node(&target),
                        },
                        e => station_err(&e),
                    })?;
                eprintln!("astar-server: echolink: calling {target}");
            }
            EchoLinkAction::Disconnect => {
                if cfg.is_none() {
                    return Err(NodeError {
                        message: echolink_node::ECHOLINK_NOT_ENABLED.to_string(),
                    });
                }
                if let Some(target) = target {
                    // A target that is present must name a station. Only an
                    // absent one means "everyone", so a blank field on a
                    // form can never hang up the whole leg.
                    let target =
                        echolink_node::clean_target(Some(target)).map_err(|_| NodeError {
                            message: echolink_node::ECHOLINK_DISCONNECT_NEEDS_TARGET.to_string(),
                        })?;
                    self.station
                        .echolink_node_disconnect(&target)
                        .map_err(|e| match e {
                            StationError::NotConnected => NodeError {
                                message: echolink_node::not_on_node(&target),
                            },
                            e => station_err(&e),
                        })?;
                    eprintln!("astar-server: echolink: hung up on {target}");
                } else if self.hang_up_echolink_stations() == 0 {
                    return Err(NodeError {
                        message: echolink_node::ECHOLINK_NOBODY_ON.to_string(),
                    });
                }
            }
        }
        self.broadcast(&NodeEvent::Snapshot(self.snapshot()));
        Ok(NodeReply::Ok)
    }

    /// Hang up on every station on the `EchoLink` leg; how many there were.
    /// A BYE goes out to each on the link's next pass. The login stays up.
    fn hang_up_echolink_stations(&self) -> usize {
        self.station
            .echolink_node_peers()
            .iter()
            .filter(|p| self.station.echolink_node_disconnect(&p.call).is_ok())
            .count()
    }

    /// End of the run: hang up on every `EchoLink` station, then log off
    /// the directory, waiting at most [`ECHOLINK_LOGOFF_WAIT`]. A no-op on
    /// a node that never logged in.
    ///
    /// The hang-ups are quick (a BYE each on the link's next pass). The
    /// logoff is not: it can wait on a directory exchange already in flight
    /// and on a DNS lookup with no deadline, 80 s or more at worst. So it
    /// runs on a thread of its own, and after the wait this returns whether
    /// or not it finished, with one warning line when it did not. The
    /// directory then drops the node from its list on its own time-out.
    /// `run_serve` calls this as its very last step, after the Hangup, the
    /// pump join and the Discord teardown, so a slow directory can delay
    /// the exit by the wait and never skips any of those.
    pub fn teardown_echolink(self: &Arc<Self>) {
        self.teardown_echolink_within(ECHOLINK_LOGOFF_WAIT);
    }

    /// [`NodeController::teardown_echolink`] with the wait given; `true`
    /// when the logoff finished within it.
    fn teardown_echolink_within(self: &Arc<Self>, wait: std::time::Duration) -> bool {
        self.hang_up_echolink_stations();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let ctrl = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("echolink-logoff".into())
            .spawn(move || {
                ctrl.station.echolink_logout();
                let _ = done_tx.send(());
            });
        if let Err(e) = spawned {
            eprintln!(
                "astar-server: echolink: logoff thread did not start ({e}); skipping the logoff"
            );
            return false;
        }
        let finished = done_rx.recv_timeout(wait).is_ok();
        if !finished {
            eprintln!(
                "astar-server: echolink: the directory logoff did not finish within {}s; leaving it",
                wait.as_secs()
            );
        }
        finished
    }

    /// Pump step: what happened on the `EchoLink` leg since the last tick.
    ///
    /// `Station::echolink_node_poll` also copies each station's "talking"
    /// into the conference, which keys the `AllStar` links on the
    /// `sync_link_keying` that follows, and reaps the stations whose
    /// contact ended. A hang-up from `POST /echolink` or a logout emits no
    /// edge (the caller already knows), so this reports only what happened
    /// on its own: one log line each, and one fresh snapshot to every
    /// subscriber.
    fn drain_echolink(&self) {
        let edges = self.station.echolink_node_poll();
        if edges.is_empty() {
            return;
        }
        for edge in edges {
            match edge {
                // The callsign comes from the directory: escaped, so a
                // control character in it can't forge a log line.
                astar_station::EchoLinkNodeEdge::Connected(call) => {
                    eprintln!("astar-server: echolink: {} answered", call.escape_debug());
                }
                astar_station::EchoLinkNodeEdge::Ended { call, reason } => {
                    eprintln!(
                        "astar-server: echolink: {} ended: {}",
                        call.escape_debug(),
                        reason.escape_debug()
                    );
                }
                // The 10-minute transmit cap: the net has been sending to
                // this station without a pause. The leg stops until the net
                // pauses; the sentence also shows on the station's row.
                astar_station::EchoLinkNodeEdge::TransmitCapped { notice, .. } => {
                    eprintln!("astar-server: echolink: {}", notice.escape_debug());
                }
            }
        }
        self.broadcast(&NodeEvent::Snapshot(self.snapshot()));
    }

    /// Current point-in-time view — secret-free.
    pub fn snapshot(&self) -> NodeSnapshot {
        let state = self.station.snapshot();
        #[cfg(feature = "discord")]
        let discord = self.discord_cfg.as_ref().map(|cfg| {
            // A poisoned lock here must NOT read as the clean idle state
            // (`connected: false, frames_sent: 0, last_error: None`) — that
            // would hide precisely the fault an operator most needs to see.
            // Matches this file's dominant `.expect(...)` idiom rather than
            // swallowing the poison into a benign-looking default.
            // LOCK ORDER, load-bearing: `discord` is held across the
            // `discord_view` and `discord_gateway` locks taken below, so the
            // order is `discord` → {`discord_view`, `discord_gateway`} and
            // nothing may take them the other way round. All three are held
            // for a clone or an `is_some_and` and nothing calls out under
            // any of them.
            let guard = self.discord.lock().expect("discord bridge mutex poisoned");
            // Four facts, and `connected` is NOT the one an operator wants
            // when the channel has gone quiet: the audio track can die with
            // the `Bridge` still installed and still being handed frames.
            // `audio_ok` is that second question, and it is exactly "the
            // last play did not fail" — `Bridge::last_error` is written by
            // a failed play and cleared by the next successful one.
            let (connected, audio_ok, frames_sent, last_error) =
                guard.as_ref().map_or((false, false, 0, None), |b| {
                    let last_error = b.last_error().map(ToString::to_string);
                    (true, last_error.is_none(), b.frames_sent(), last_error)
                });
            // `resolve` returns "" for an unknown username (secrets.rs:49) —
            // it is NOT an Option. A section with no token is the single
            // most likely misconfiguration this feature has, and reporting
            // `configured: true` at it would send an operator debugging
            // guild/channel ids instead of the missing token. Looked up
            // fresh on every snapshot (not cached): the token can arrive at
            // runtime via `POST /secrets`, and a cached `false` would keep
            // claiming "unconfigured" after the operator just fixed it.
            let has_token = !self.secrets.resolve("discord").is_empty();
            // **The cached view, never the commander itself.** Reading the
            // commander here would mean waiting behind whatever has it
            // checked out — and what has it checked out may be a `/dial`
            // blocking on DNS or a portal HTTPS mint. `/status` is the one
            // call an operator makes when something is already wrong, so it
            // is the last thing that may block. The cache is refreshed after
            // every `handle`/`tick`/`admin`/`shutdown`, which on the pump is
            // every 50 ms.
            let view = self
                .discord_view
                .lock()
                .expect("discord view mutex poisoned")
                .clone();
            // "Commands are live" is two facts, and an operator needs both:
            // approvers are configured AND the bot actually got online.
            // Configured-but-offline is the state that looks like "off" and
            // is not.
            let gateway_up = self
                .discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned")
                .as_ref()
                .is_some_and(|g| !g.is_failed());
            let commands = self.discord_gateway_is_persistent() && gateway_up;
            // The rule engine's error wins when it has one: a failed
            // state-file write or a mute Discord refused outlives — and
            // matters more than — whatever the audio path last reported.
            let last_error = view
                .as_ref()
                .and_then(|v| v.last_error.clone())
                .or(last_error);
            crate::command::DiscordStatus {
                configured: has_token,
                connected,
                audio_ok,
                channel_id: Some(cfg.channel_id.clone()),
                frames_sent,
                last_error,
                commands,
                // No commander means nothing has been turned off.
                enabled: view.as_ref().is_none_or(|v| v.enabled),
                mic_holder: view.as_ref().and_then(|v| v.mic_holder.clone()),
                link: view.as_ref().and_then(|v| v.link.as_ref()).map(Into::into),
            }
        });
        #[cfg(not(feature = "discord"))]
        let discord = None;
        // Clone under the lock, build outside it: `m17_link_status` asks the
        // station, and no controller mutex is ever held across that.
        let (m17_enabled, directory_generated, m17_leg) = {
            let st = self.m17.lock().expect("m17 state mutex poisoned");
            (
                st.settings.as_ref().is_some_and(|s| s.enabled),
                st.settings
                    .as_ref()
                    .and_then(M17Settings::directory_generated)
                    .map(str::to_string),
                st.leg.clone(),
            )
        };
        let m17 = m17_leg.map(|leg| self.m17_link_status(leg, directory_generated));
        // Same discipline: clone under the lock, ask the station outside it.
        let (echolink_cfg, echolink_setup_error) = {
            let st = self.echolink.lock().expect("echolink state mutex poisoned");
            (st.cfg.clone(), st.setup_error.clone())
        };
        let echolink = echolink_cfg.as_ref().map(|cfg| {
            echolink_node::node_status(
                cfg,
                echolink_setup_error.as_deref(),
                self.station.echolink_state().map(|s| s.login).as_ref(),
                &self.station.echolink_node_peers(),
            )
        });
        let links = self.station.link_roster().links;
        let m17_relay = m17_enabled
            .then(|| RelayCounts::from_parts(&state.calls, &links, echolink.as_ref()).into());
        NodeSnapshot {
            node_id: self.node_id.clone(),
            listening: self.station.is_listening(),
            registered: self.station.is_registered(),
            calls: state.calls,
            links,
            discord,
            m17,
            m17_enabled,
            m17_available: self.station.m17_available(),
            m17_relay,
            echolink_enabled: echolink_cfg.is_some(),
            echolink,
            public: self
                .public_status
                .lock()
                .expect("public status poisoned")
                .as_ref()
                .map(|s| s.lock().expect("public status poisoned").clone()),
        }
    }

    /// Subscribe to async events.  Call `pump()` periodically to drain
    /// `Station::next_event` and forward mapped events to all live subscribers.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<NodeEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.senders
            .lock()
            .expect("senders mutex poisoned")
            .push(tx);
        rx
    }

    /// Drain `Station::next_event`, map to `NodeEvent`, and fan-out to all
    /// live subscribers (dead receivers are pruned on send error).
    ///
    /// Also advances in-flight announcements via `poll_announcements`, fires
    /// the periodic station-ID when its deadline has passed, and consults the
    /// event→announcement table for each drained `StationEvent`.
    ///
    /// # Locking
    /// The `senders` mutex is acquired **once per drained `StationEvent`** and
    /// held across the fan-out sends for that event.  This is safe because the
    /// sends are non-blocking (`std::sync::mpsc` bounded only by heap); the lock
    /// is never held while waiting on I/O or a blocking call.
    ///
    /// # Thread safety
    /// `pump()` is intended to be called from a **single thread**.  Concurrent
    /// callers would race on `Station::next_event()` — events could be delivered
    /// out of order or duplicated across callers.  The adapter layer must ensure
    /// only one thread drives the pump loop at a time.
    pub fn pump(&self) {
        // Advance any in-flight announcement (auto-unkey on completion).
        self.station.poll_announcements();
        // Periodic station-ID (fires when deadline passes; no-ops when disabled).
        self.maybe_fire_periodic_id();
        // Forward node-to-node link lifecycle edges onto the SSE stream.
        self.drain_link_events();
        // M17 monitor edges: linked, or ended (and reaped) with its reason.
        self.drain_m17();
        // EchoLink leg edges, and each station's "talking" into the
        // conference, before the link keying below reads it.
        self.drain_echolink();
        // Drain received DTMF digits and execute any finalized `*` commands.
        self.drain_dtmf_commands();
        // iax-7d51: key/unkey transceive links to follow member PTT — a far-end
        // node mutes relayed audio from an unkeyed sender.
        self.station.sync_link_keying();
        // Move any node audio queued for a joined Discord bridge member out
        // into the voice link.
        #[cfg(feature = "discord")]
        if let Some(b) = self
            .discord
            .lock()
            .expect("discord bridge mutex poisoned")
            .as_mut()
        {
            b.pump();
        }
        // Slash commands: drain the gateway's events into the rule engine
        // and run its timers. Every lock discipline note is in that
        // function — nothing it does may run under a controller mutex.
        #[cfg(feature = "discord")]
        self.pump_discord_commands();
        // Periodic ID into the channel, on the same interval AND the same
        // `id_mode` gate as the node's own on-air ID (`id_interval()`) — this
        // reuses that one physical config field, so `id_mode = "off"` must
        // disable both consumers identically, not just the on-air one.
        // Spoken to Discord only — announce_to_bridge_member reaches the
        // bridge member's own leg, never the air.
        #[cfg(feature = "discord")]
        if let Some(interval) = self.id_interval() {
            let now = std::time::Instant::now();
            // Decide + advance the deadline, then release the lock before
            // touching `discord_member` or calling into the station — mirrors
            // `maybe_fire_periodic_id`'s "release before calling station
            // (avoid lock inversion)" pattern. `announce_to_bridge_member`
            // with a `Phrase::Text` synthesizes TTS synchronously (up to
            // `timeout_ms`, default 4s), so holding either lock across it
            // would stall a concurrent Join/Leave for that long.
            let due_now = {
                let mut due = self
                    .discord_id_due
                    .lock()
                    .expect("discord id timer mutex poisoned");
                let fire = due.is_none_or(|d| now >= d);
                if fire {
                    *due = Some(now + interval);
                }
                fire
            };
            if due_now {
                let member = *self
                    .discord_member
                    .lock()
                    .expect("discord member mutex poisoned");
                // Only announce when a member is actually joined — with
                // nobody enrolled there is no leg to speak onto.
                if let Some(member) = member {
                    let _ = self.station.announce_to_bridge_member(
                        member,
                        astar_iax::AnnounceRequest::say(self.discord_id_text.as_str()),
                        std::time::Duration::ZERO,
                    );
                }
            }
        }
        // Hourly "still linked" line into the voice channel's own text
        // chat, posted only while something is in the link roster. Same shape as the periodic channel ID above — decide and
        // advance the deadline inside the timer lock's scope, drop the
        // guard, and only then post — and for a stronger reason: the post
        // travels to a tokio thread, and holding a timer mutex across any
        // hand-off is how a slow Discord makes the node's 50 ms tick late.
        #[cfg(feature = "discord")]
        {
            // The bridge's own existence is the truth about whether anything
            // is joined, so the timer is armed and disarmed FROM it rather
            // than from Join and Leave remembering to do it. A bridge that
            // went away by any route — including one dropped by a failure —
            // takes the timer with it on the very next tick, and the next
            // join re-arms from scratch.
            //
            // The link roster is the other half of the truth. The line is a
            // reassurance that a LINK is still up, so with nothing linked
            // there is nothing to reassert and every hourly post would be a
            // notification with nothing behind it — the idle state of most
            // nodes most of the day. So the timer is armed only while the
            // union roster (AllStar links plus a linked M17 monitor) is
            // non-empty: it starts when the first link appears,
            // which puts the first line an hour after "Connected" rather
            // than wherever the hour since the join happened to fall, and
            // it is dropped the tick the roster empties.
            let joined = self
                .discord
                .lock()
                .expect("discord bridge mutex poisoned")
                .is_some();
            // The union roster: AllStar links plus a LINKED M17 monitor, so
            // a node listening only to a reflector still reasserts its
            // receive-only disclosure every hour.
            let linked = joined
                .then(|| self.discord_link_refs())
                .filter(|refs| !refs.is_empty());
            let now = std::time::Instant::now();
            let due_now = {
                let mut due = self
                    .discord_status_due
                    .lock()
                    .expect("discord status timer mutex poisoned");
                match *due {
                    _ if linked.is_none() => {
                        *due = None;
                        false
                    }
                    // First tick with a joined bridge AND a link: arm, do
                    // NOT fire. The channel was just told "Connected" and
                    // an immediate "still linked" behind it says nothing.
                    None => {
                        *due = Some(now + DISCORD_STATUS_HEARTBEAT);
                        false
                    }
                    Some(deadline) if now >= deadline => {
                        *due = Some(now + DISCORD_STATUS_HEARTBEAT);
                        true
                    }
                    Some(_) => false,
                }
            };
            if due_now
                && let Some(refs) = &linked
                && let Some(b) = self
                    .discord
                    .lock()
                    .expect("discord bridge mutex poisoned")
                    .as_mut()
            {
                // The bridge mutex IS held here, exactly as it is for
                // `b.pump()` above and for the same reason: the `Bridge`
                // lives behind it. The post itself is a non-blocking
                // `std::sync::mpsc` send onto the runtime thread, which
                // does the REST call; nothing here waits on Discord.
                b.post_heartbeat(refs);
            }
        }
        // Drain station events, consult event table, then fan-out to subscribers.
        while let Some(event) = self.station.next_event() {
            self.maybe_fire_event_announcement(&event);
            let node_events = station_event_to_node_events(event, || self.snapshot());
            let mut senders = self.senders.lock().expect("senders mutex poisoned");
            for ev in node_events {
                senders.retain(|tx| tx.send(ev.clone()).is_ok());
            }
        }
    }

    /// Drain the Station's buffered link lifecycle events and fan the edges out
    /// to subscribers (iax-d829.1). `Station::next_link_event` is poll-style
    /// (it drains the session in bursts internally), so no receiver plumbing is
    /// needed here — mirror of the `next_event` loop in [`Self::pump`].
    fn drain_link_events(&self) {
        while let Some(ev) = self.station.next_link_event() {
            for nev in link_event_to_node_events(ev, || self.snapshot()) {
                self.broadcast(&nev);
            }
        }
    }

    /// Pump step (iax-d254): drain received DTMF digits and execute any
    /// finalized `*` commands through the same `handle_link` path HTTP uses.
    fn drain_dtmf_commands(&self) {
        // Drain unconditionally — digits are already detected/squelched by
        // the engine; consuming keeps the pool bounded even when disabled.
        let digits = self.station.drain_dtmf_digits();
        self.apply_dtmf_digits(digits, std::time::Instant::now());
    }

    /// Testable core of [`Self::drain_dtmf_commands`]: feed digits + advance
    /// the inter-digit clock, dispatching finalized commands. Failures are
    /// logged, not surfaced — a bad `*` sequence must not wedge the pump.
    pub(crate) fn apply_dtmf_digits(&self, digits: Vec<(u64, char)>, now: std::time::Instant) {
        // Every received digit is broadcast (iax-2f5e) even when command
        // execution is disabled: it is the only signal that a handset's tones
        // are reaching this node at all.
        if !self.dtmf_enabled {
            for (call, d) in digits {
                self.broadcast(&NodeEvent::Dtmf {
                    call,
                    digit: d.to_string(),
                    command: None,
                });
            }
            return;
        }
        let mut finalized = Vec::new();
        {
            let mut asm = self.dtmf_assembler.lock().expect("dtmf assembler poisoned");
            for (call, d) in digits {
                let cmd = asm.push(call, d, now);
                self.broadcast(&NodeEvent::Dtmf {
                    call,
                    digit: d.to_string(),
                    command: cmd
                        .as_ref()
                        .map(|(a, n)| format!("{} {n}", link_action_str(*a))),
                });
                if let Some(cmd) = cmd {
                    finalized.push(cmd);
                }
            }
            finalized.extend(asm.tick(now));
        }
        for (action, node) in finalized {
            if let Err(e) = self.handle_link(action, &node, None) {
                tracing::warn!(?action, node, error = %e.message, "DTMF link command failed");
            }
        }
    }

    /// Decide whether the periodic ID is due, advance the deadline, and
    /// optionally fire it.  Interior-mutability safe; no sleeps; deterministic
    /// with `id_interval_secs = 0` (fires on the very first pump).
    ///
    /// Returns `true` if the ID fired successfully, `false` otherwise.
    ///
    /// # Scheduler logic
    /// - `id_mode` absent or `"off"` → always returns `false`.
    /// - First call with `next_id_at == None`: arms the timer at
    ///   `now + interval`.  With interval == 0 the deadline equals `now` so
    ///   the condition `now >= next_id_at` is already satisfied on this call —
    ///   the ID fires immediately, which makes `interval_secs = 0` a reliable
    ///   "fire on first pump" probe for tests.
    /// - Subsequent calls: fires when `now >= next_id_at` and resets deadline
    ///   to `now + interval`.
    fn maybe_fire_periodic_id(&self) -> bool {
        let Some(interval) = self.id_interval() else {
            return false;
        };

        let now = std::time::Instant::now();
        let mut guard = self.next_id_at.lock().expect("next_id_at mutex poisoned");

        let due = match *guard {
            None => {
                // Arm the timer. With interval == Duration::ZERO the deadline
                // equals now, so we fall through to the fire check below.
                *guard = Some(now + interval);
                // now >= now + Duration::ZERO  →  true (zero-interval fires now)
                now >= now + interval
            }
            Some(deadline) => now >= deadline,
        };

        if due {
            // Advance the deadline before firing (regardless of announce outcome)
            // so a busy/idle node does not accumulate a backlog of missed IDs.
            *guard = Some(now + interval);
            drop(guard); // release before calling station (avoid lock inversion)

            if self.station.announce(self.id_request()).is_ok() {
                self.broadcast(&NodeEvent::AnnouncementStarted { kind: "id".into() });
                return true;
            }
        }
        false
    }

    /// Pure helper: is the ID due at `now` given `interval`?  Also advances
    /// `next_id_at`.  Exposed for unit-testing the scheduler logic without
    /// requiring a live call.
    ///
    /// Returns `(due, new_deadline)`.
    #[cfg(test)]
    pub(crate) fn id_due_and_advance(
        &self,
        now: std::time::Instant,
        interval: std::time::Duration,
    ) -> (bool, std::time::Instant) {
        let mut guard = self.next_id_at.lock().expect("next_id_at mutex poisoned");
        let deadline = match *guard {
            None => {
                let dl = now + interval;
                *guard = Some(dl);
                dl
            }
            Some(dl) => dl,
        };
        let due = now >= deadline;
        if due {
            *guard = Some(now + interval);
        }
        (due, *guard.as_ref().unwrap())
    }

    /// Return the configured ID interval if the ID feature is enabled, else
    /// `None`.  The feature is disabled when:
    /// - `announce_cfg` is absent, or
    /// - `id_mode` is absent or equals `"off"` (case-insensitive), or
    /// - `id_interval_secs` is absent.
    fn id_interval(&self) -> Option<std::time::Duration> {
        let cfg = self.announce_cfg.as_ref()?;
        let mode = cfg.id_mode.as_deref()?;
        if mode.eq_ignore_ascii_case("off") {
            return None;
        }
        let secs = cfg.id_interval_secs?;
        Some(std::time::Duration::from_secs(secs))
    }

    /// Consult the event→announcement table and fire an announcement if the
    /// event has an enabled entry.
    ///
    /// # `StationEvent` → config-key mapping
    ///
    /// | `StationEvent` variant            | config key      |
    /// |-----------------------------------|-----------------|
    /// | `IncomingCall { .. }`             | `"incoming_call"` |
    /// | `Hangup { .. }`                   | `"hangup"`      |
    /// | `Registered`                      | `"registered"`  |
    /// | `RegisterFailed { .. }`           | `"register_failed"` |
    /// | `Answered`                        | `"answered"`    |
    /// | `RemotePtt(_)` / `ModeChanged(_)` | (not mapped)    |
    ///
    /// The keys were chosen to match the actual `StationEvent` variant names
    /// (`snake_case`).  The config example keys (`"peer_connected"` / `"call_rejected"`)
    /// in the original design doc were illustrative; these are the canonical keys.
    fn maybe_fire_event_announcement(&self, ev: &astar_station::StationEvent) {
        let Some(cfg) = &self.announce_cfg else {
            return;
        };
        let Some(events) = &cfg.events else { return };

        let key = match ev {
            astar_station::StationEvent::IncomingCall { .. } => "incoming_call",
            astar_station::StationEvent::Hangup { .. } => "hangup",
            astar_station::StationEvent::Registered => "registered",
            astar_station::StationEvent::RegisterFailed { .. } => "register_failed",
            astar_station::StationEvent::Answered => "answered",
            // RemotePtt and ModeChanged are not mapped to announcements.
            astar_station::StationEvent::RemotePtt(_)
            | astar_station::StationEvent::ModeChanged(_) => return,
        };

        let entry = match events.get(key) {
            Some(e) if e.enabled => e,
            _ => return,
        };

        // The "answered" event IS the on-join greeting (iax-c4ea): a user has
        // joined, so speak the node-id join template TO THAT JOINING USER'S OWN
        // LEG (per-member), not the literal word "answered" to the whole mix.
        if key == "answered" {
            self.fire_join_greeting();
            return;
        }

        // Build a text announcement describing the event, directed as configured.
        let phrase = astar_iax::Phrase::Text(key.to_string());
        let dest = match entry
            .destination
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("local" | "local_monitor" | "to_monitor") => astar_iax::Destination::LocalMonitor,
            Some("both") => astar_iax::Destination::Both,
            _ => astar_iax::Destination::ToAir,
        };
        let req = astar_iax::AnnounceRequest {
            phrase,
            destination: dest,
            policy: astar_iax::AnnouncePolicyReq::Seize,
            priority: 3,
        };

        if self.station.announce(req).is_ok() {
            self.broadcast(&NodeEvent::AnnouncementStarted {
                kind: "event".into(),
            });
        }
    }

    /// Render the on-join node-id greeting and inject it onto the JOINING
    /// member's own leg (iax-c4ea). The phrase is the configured `join_template`
    /// (default [`DEFAULT_JOIN_TEMPLATE`]) with `{server-node-number}` replaced
    /// by this node's id expanded into space-separated digits so TTS reads it
    /// digit-by-digit. Rendered via the TTS path (`Phrase::Text`) and routed to
    /// the active conference member only — never broadcast to the whole mix.
    fn fire_join_greeting(&self) {
        let text = self.join_greeting_text();
        let req = astar_iax::AnnounceRequest {
            phrase: astar_iax::Phrase::Text(text),
            destination: astar_iax::Destination::ToAir,
            policy: astar_iax::AnnouncePolicyReq::Seize,
            priority: 4,
        };
        if self.station.announce_to_member(req).is_ok() {
            self.broadcast(&NodeEvent::AnnouncementStarted {
                kind: "join".into(),
            });
        }
    }

    /// Compute the rendered join-greeting text: the configured template (or
    /// [`DEFAULT_JOIN_TEMPLATE`]) with the `{server-node-number}` token replaced
    /// by this node's id as space-separated digits (e.g. `"77777"` →
    /// `"7 7 7 7 7"`). Exposed `pub(crate)` for unit-testing the substitution
    /// WITHOUT invoking piper/TTS.
    pub(crate) fn join_greeting_text(&self) -> String {
        let template = self
            .announce_cfg
            .as_ref()
            .and_then(|a| a.join_template.as_deref())
            .unwrap_or(DEFAULT_JOIN_TEMPLATE);
        let id = self.node_id.as_deref().unwrap_or("");
        let spaced: String = id
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        template.replace(JOIN_NODE_TOKEN, &spaced)
    }

    /// Render a link announcement (iax-9e02): the configured template (or the
    /// built-in default) with `{node}` replaced by `node` as space-separated
    /// digits, so TTS reads `55553` as "5 5 5 5 3". Pure — unit-tested without
    /// invoking piper/TTS.
    pub(crate) fn link_announce_text(&self, connecting: bool, node: &str) -> String {
        let cfg = self.announce_cfg.as_ref();
        let template = if connecting {
            cfg.and_then(|a| a.link_connect_template.as_deref())
                .unwrap_or(DEFAULT_LINK_CONNECT_TEMPLATE)
        } else {
            cfg.and_then(|a| a.link_disconnect_template.as_deref())
                .unwrap_or(DEFAULT_LINK_DISCONNECT_TEMPLATE)
        };
        let spaced: String = node
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        template.replace(LINK_NODE_TOKEN, &spaced)
    }

    /// Speak a link connect/disconnect announcement to the web-transceiver
    /// members on this node (iax-9e02) — every conference member that is NOT a
    /// link. Linked nodes are deliberately excluded: link status is for the
    /// people here, not something to transmit at the far end. Best-effort:
    /// with nobody connected there is nobody to announce to, and the link
    /// operation still stands.
    fn fire_link_announcement(&self, connecting: bool, node: &str) {
        let req = astar_iax::AnnounceRequest {
            phrase: astar_iax::Phrase::Text(self.link_announce_text(connecting, node)),
            destination: astar_iax::Destination::ToAir,
            policy: astar_iax::AnnouncePolicyReq::Seize,
            priority: 6,
        };
        if self.station.announce_to_non_link_members(req) > 0 {
            self.broadcast(&NodeEvent::AnnouncementStarted {
                kind: if connecting {
                    "link_connect".into()
                } else {
                    "link_disconnect".into()
                },
            });
        }
    }

    /// Render an M17 monitor announcement: the configured
    /// `[announce] m17_connect_template` / `m17_disconnect_template` (or the
    /// built-in default) with `{node}` replaced by the reflector name spelled
    /// out character by character, then `" module "` and the module letter.
    /// `("M17-USA", 'C')` becomes `"M 1 7 - U S A module C"`, so TTS never
    /// tries to pronounce a reflector name as a word. Pure, so it is
    /// unit-tested without invoking TTS.
    pub(crate) fn m17_announce_text(
        &self,
        connecting: bool,
        reflector: &str,
        module: char,
    ) -> String {
        let cfg = self.announce_cfg.as_ref();
        let template = if connecting {
            cfg.and_then(|a| a.m17_connect_template.as_deref())
                .unwrap_or(DEFAULT_M17_CONNECT_TEMPLATE)
        } else {
            cfg.and_then(|a| a.m17_disconnect_template.as_deref())
                .unwrap_or(DEFAULT_M17_DISCONNECT_TEMPLATE)
        };
        let mut spoken: String = reflector
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        spoken.push_str(" module ");
        spoken.push(module);
        template.replace(LINK_NODE_TOKEN, &spoken)
    }

    /// Speak an M17 monitor start/stop announcement to the conference
    /// members on this node that are not links, through the same path and
    /// policy as `fire_link_announcement` (`Destination::ToAir`,
    /// `AnnouncePolicyReq::Seize`, priority 6). This cannot reach M17:
    /// `announce_to_non_link_members` walks pooled IAX calls only, and the
    /// monitor is a conference source member with no mix receiver.
    /// Best-effort: with nobody connected there is nobody to tell, and the
    /// M17 operation still stands. It takes the station lock, so it must
    /// never be called while `self.m17` is held.
    fn fire_m17_announcement(&self, connecting: bool, reflector: &str, module: char) {
        let text = self.m17_announce_text(connecting, reflector, module);
        #[cfg(test)]
        self.m17_announce_log
            .lock()
            .expect("m17 announce log mutex poisoned")
            .push((connecting, text.clone()));
        let req = astar_iax::AnnounceRequest {
            phrase: astar_iax::Phrase::Text(text),
            destination: astar_iax::Destination::ToAir,
            policy: astar_iax::AnnouncePolicyReq::Seize,
            priority: 6,
        };
        if self.station.announce_to_non_link_members(req) > 0 {
            self.broadcast(&NodeEvent::AnnouncementStarted {
                kind: m17_announcement_kind(connecting).into(),
            });
        }
    }

    /// Broadcast one `NodeEvent` to all live subscribers, pruning dead senders.
    fn broadcast(&self, ev: &NodeEvent) {
        let mut senders = self.senders.lock().expect("senders mutex poisoned");
        senders.retain(|tx| tx.send(ev.clone()).is_ok());
    }

    /// `true` after a `Shutdown` command has been received.
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Install the sign-in service. Once only; a second call is ignored.
    #[cfg(feature = "auth")]
    pub fn set_auth(&self, auth: Arc<crate::auth::AuthService>) {
        let _ = self.auth.set(auth);
    }

    /// The sign-in service, when `serve` installed one.
    #[cfg(feature = "auth")]
    #[must_use]
    pub fn auth(&self) -> Option<&Arc<crate::auth::AuthService>> {
        self.auth.get()
    }

    /// Install the node logger. Once only; a second call is ignored.
    pub fn set_recordings(&self, logger: Arc<crate::recording::logger::NodeLogger>) {
        let _ = self.recordings.set(logger);
    }

    /// The node logger, when `serve` started one.
    #[must_use]
    pub fn recordings(&self) -> Option<&Arc<crate::recording::logger::NodeLogger>> {
        self.recordings.get()
    }

    /// Show the public listener's state on the admin snapshot.
    pub fn attach_public_status(&self, status: Arc<Mutex<crate::public::serve::PublicTlsStatus>>) {
        *self.public_status.lock().expect("public status poisoned") = Some(status);
    }

    /// Enrol the public stream's tap: a NON-contributing conference member
    /// (`Station::add_bridge_member`), so nothing the stream does can reach
    /// the air. `None` in handset or parrot mode — the station refuses both.
    #[must_use]
    pub fn enrol_stream_tap(&self) -> Option<astar_iax::BridgeMemberHandle> {
        if matches!(self.bridge_mode(), astar_iax::BridgeMode::Parrot) {
            return None;
        }
        self.station.add_bridge_member()
    }

    /// The tap's member is still in the conference and the node is not in
    /// parrot mode. A direct Bridge→Parrot switch keeps the conference (and
    /// so the member), and a parrot caller is making a private test, so
    /// parrot counts as "not enrolled" and the tap withdraws.
    #[must_use]
    pub fn stream_tap_enrolled(&self, id: astar_audio::MemberId) -> bool {
        !matches!(self.bridge_mode(), astar_iax::BridgeMode::Parrot)
            && self.station.has_bridge_member(id)
    }

    /// Withdraw the tap's member.
    pub fn withdraw_stream_tap(&self, id: astar_audio::MemberId) {
        self.station.remove_bridge_member(id);
    }

    /// The callsign the M17 monitor heard most recently, if it was heard
    /// within `within`. Off the air: text only. `None` with no monitor up.
    #[must_use]
    pub fn m17_recent_talker(&self, within: std::time::Duration) -> Option<String> {
        let within_ms = u64::try_from(within.as_millis()).unwrap_or(u64::MAX);
        self.station
            .m17_monitor_heard()
            .into_iter()
            .filter(|h| h.age_ms < within_ms)
            .min_by_key(|h| h.age_ms)
            .map(|h| h.callsign)
    }

    /// Install (or remove) the node logger's codec-frame tap on the station.
    pub fn set_logger_frame_tap(
        &self,
        tap: Option<astar_record::tap::Tap<astar_record::frames::FrameEvent>>,
    ) {
        self.station.set_frame_tap(tap);
    }

    /// The bridge mode the station is in now. The public page names it, and
    /// the stream tap reads it to know whether a tap is possible at all.
    #[must_use]
    pub fn bridge_mode(&self) -> astar_iax::BridgeMode {
        self.station.bridge_config().mode
    }

    /// Current conference-bridge configuration (iax-647d). Used by adapters/tests
    /// to read back the effect of `POST /bridge`.
    #[must_use]
    pub fn bridge_config_for_test(&self) -> astar_iax::BridgeConfig {
        self.station.bridge_config()
    }

    /// The station's selected `(input, output)` devices — what the next
    /// connect will use (`Station::selected_devices`). Used by tests to
    /// prove the settings live tier's `SetDevices` reached the station, not
    /// just the file.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn selected_devices_for_test(&self) -> (Option<String>, Option<String>) {
        self.station.selected_devices()
    }

    /// Current conference-bridge member count. Used by tests to prove a
    /// Discord join/leave cycle actually enrolls and withdraws its member
    /// rather than leaking one.
    #[must_use]
    pub fn conference_member_count_for_test(&self) -> usize {
        self.station.conference_member_count()
    }

    /// Return a clone of the internal stop flag.
    ///
    /// The signal handler can call `stop_flag().store(true, Ordering::Relaxed)`
    /// to trigger graceful shutdown without going through `execute(Shutdown)`,
    /// which would require a `&self` reference across thread boundaries.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Build an `AnnounceRequest` from the Announce command fields.
    ///
    /// Returns `None` if neither `text` nor `sample` is provided.
    fn build_announce_request(
        text: Option<String>,
        sample: Option<String>,
        destination: Option<&str>,
        mixunder: Option<bool>,
        gain_db: Option<f32>,
    ) -> Option<astar_iax::AnnounceRequest> {
        let phrase = if let Some(t) = text {
            astar_iax::Phrase::Text(t)
        } else {
            astar_iax::Phrase::Sample(sample?)
        };

        let dest = match destination.map(str::to_ascii_lowercase).as_deref() {
            Some("local" | "local_monitor") => astar_iax::Destination::LocalMonitor,
            Some("both") => astar_iax::Destination::Both,
            _ => astar_iax::Destination::ToAir,
        };

        let policy = if mixunder == Some(true) {
            astar_iax::AnnouncePolicyReq::MixUnder { gain_db }
        } else {
            astar_iax::AnnouncePolicyReq::Seize
        };

        Some(astar_iax::AnnounceRequest {
            phrase,
            destination: dest,
            policy,
            priority: 5,
        })
    }

    /// Build an ID announcement request from the node's configured `id_mode` and `node_id`.
    fn id_request(&self) -> astar_iax::AnnounceRequest {
        let id = self.node_id.as_deref().unwrap_or("node");
        let id_mode = self
            .announce_cfg
            .as_ref()
            .and_then(|a| a.id_mode.as_deref())
            .unwrap_or("voice");

        let (phrase, policy) = if id_mode.eq_ignore_ascii_case("cw") {
            (
                astar_iax::Phrase::Cw(id.to_string()),
                astar_iax::AnnouncePolicyReq::MixUnder { gain_db: None },
            )
        } else {
            (
                astar_iax::Phrase::Text(format!("node {id}")),
                astar_iax::AnnouncePolicyReq::Seize,
            )
        };

        astar_iax::AnnounceRequest {
            phrase,
            destination: astar_iax::Destination::ToAir,
            policy,
            priority: 4,
        }
    }
}

/// Whether a key-down must be refused, given the snapshot's digital-voice
/// flags (iax-d9f4, extended for YSF in astar-e7b3, for NXDN in iax-b9c2 and
/// for DMR in iax-d4f7, and for `EchoLink`). `Some(err)` refuses; `None` lets
/// the key through.
///
/// Pure, so the policy is testable without a `ThumbDV` and a live reflector —
/// the only way to make a real `Station` report any of these flags.
///
/// The four dongle networks are the ones this crate must never key. D-Star
/// can transmit and must not be made to do so from here; YSF, NXDN and DMR
/// cannot transmit at all yet, so keying any of them would be a refusal
/// further down anyway — but a refusal that names the reason beats one that
/// does not, and the day one grows a transmit path this guard is already in
/// front of it rather than needing to be remembered. DMR is the sharpest
/// case: a master routes by the radio ID the link logged in with, so a
/// remotely triggered key would put bursts on a live network under the
/// operator's own registration.
///
/// Every flag is read off the SNAPSHOT, never a `#[cfg]`: enabling `dmr` (or
/// `nxdn`, or `dstar`, or `ysf`) anywhere in a workspace build unifies the
/// feature into this crate too, so a compile-time guard would be exactly as
/// absent as the feature is present.
///
/// `EchoLink` is refused for a different reason: its access policy forbids
/// reaching it from a computer-based `VoIP` client, and a key from a web page
/// or a TUI while a station is on the far end would be exactly that. The
/// stations on the node's leg hear what the conference carries, which is
/// RF and `AllStar`, and nothing keyed from here. `echolink_active` is true
/// while the app's call or the node's leg has a station on the far end.
///
/// IAX2 and the console's own M17 client session are remotely keyable; the node's M17 *monitor* has no transmit path, and `POST /key` cannot reach it.
/// See `Station::set_ptt`'s "Remote-control surfaces" section for why the
/// check lives at the caller rather than inside the station.
// Five bools, and they stay five bools: each is one network's snapshot flag,
// read straight off `ConsoleState`, and the shape must be `#[cfg]`-free (see
// the doc above). Folding them into an enum would mean deciding "which
// network is live" at the call site — which is exactly the decision this
// function exists to own, and exactly what a fifth network would then have to
// remember to update in two places instead of one.
#[allow(clippy::fn_params_excessive_bools)]
fn key_refusal(
    dstar_active: bool,
    ysf_active: bool,
    nxdn_active: bool,
    dmr_active: bool,
    echolink_active: bool,
) -> Option<NodeError> {
    let (article, network) = if dstar_active {
        ("a", "D-Star")
    } else if ysf_active {
        ("a", "System Fusion")
    } else if nxdn_active {
        ("a", "NXDN")
    } else if dmr_active {
        ("a", "DMR")
    } else if echolink_active {
        ("an", "EchoLink")
    } else {
        return None;
    };
    Some(NodeError {
        message: format!(
            "refusing to key: {article} {network} session is active and {network} transmit is not \
             remotely keyable"
        ),
    })
}

/// [`key_refusal`] asked of a live snapshot: the five flags read in one
/// place, so `execute` stays a dispatcher and the policy stays in one pure,
/// separately testable function.
fn key_refusal_from(snap: &astar_station::ConsoleState) -> Option<NodeError> {
    key_refusal(
        snap.dstar_active,
        snap.ysf_active,
        snap.nxdn_active,
        snap.dmr_active,
        snap.echolink_active,
    )
}

/// Map a `StationError` to a secret-free `NodeError`.
/// `StationError`'s `Display` impl is already secret-free (it never includes
/// passwords or tokens).
fn station_err(e: &StationError) -> NodeError {
    NodeError {
        message: e.to_string(),
    }
}

/// `"1 call"` / `"2 links"` onto `parts`; nothing at all for zero, so the
/// joined sentence names only what is actually up.
fn push_count(parts: &mut Vec<String>, n: usize, noun: &str) {
    match n {
        0 => {}
        1 => parts.push(format!("1 {noun}")),
        n => parts.push(format!("{n} {noun}s")),
    }
}

/// The sentence `busy_on_a_net` reports, from counts it has already
/// taken: `"2 calls, 1 link, 1 EchoLink station, Discord joined"`, only the
/// non-zero parts, in that order, or `None` when nothing is up. Pure, so every part — the
/// `call` part included, which no hermetic controller test can reach —
/// is pinned by exact values.
fn busy_reason(
    calls: usize,
    links: usize,
    echolink_stations: usize,
    discord_joined: bool,
) -> Option<String> {
    let mut parts = Vec::new();
    push_count(&mut parts, calls, "call");
    push_count(&mut parts, links, "link");
    push_count(&mut parts, echolink_stations, "EchoLink station");
    if discord_joined {
        parts.push("Discord joined".to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// `POST /link`'s `addr` may only be a loopback `ip:port` literal
/// (127.0.0.0/8 or `::1`). A dial presents the stored `link:<node>` secret's
/// MD5 response to whoever answers, so an arbitrary far end could harvest it
/// for offline cracking; the field exists for the harness and localhost
/// tests, which only ever dial 127.0.0.1. Never resolved: a name is refused.
pub(crate) fn link_addr_is_loopback(addr: &str) -> bool {
    addr.parse::<std::net::SocketAddr>()
        .is_ok_and(|a| a.ip().is_loopback())
}

/// The sentence for an `addr` that [`link_addr_is_loopback`] refuses.
pub(crate) const LINK_ADDR_NOT_LOOPBACK: &str = "addr: only a loopback ip:port literal (127.0.0.0/8 or [::1]) may be dialled directly; leave addr out to dial through AllStar";

/// Resolve the dial identity for a link to `node` (iax-5029): who we claim
/// to be (`caller_id`), the dial-time secret, and the wire shape. Pure —
/// the secret is returned by value and consumed at the dial, never stored.
///
/// - `Standard`: `caller_id` = our node id, secret from the `link:` namespace
///   (`""` on miss — the pre-iax-5029 dial, byte-identical).
/// - `WtGuest`: the fixed `AllStar` guest identity (`allstar-public` /
///   `allstar` — public constants, not secrets) with the WT shape;
///   `CALLING_NUMBER` is the DESTINATION selector per the WT convention,
///   `CALLING_NAME` identifies us in the far end's logs.
fn link_dial_params(
    shape: LinkShape,
    node: &str,
    node_id: Option<&str>,
    wt_name: Option<&str>,
    secrets: &SecretProvider,
) -> (String, String, astar_iax::CallMode) {
    match shape {
        // iax-cnlink: our node number is also the CALLING_NUMBER, as on an
        // app_rpt link dial; app_rpt drops a link whose caller ID is not
        // numeric. Without a node id there is nothing to send.
        LinkShape::Standard => (
            node_id.unwrap_or("").to_string(),
            secrets.link_secret(node),
            node_id
                .filter(|n| !n.is_empty())
                .map_or(astar_iax::CallMode::Standard, |n| {
                    astar_iax::CallMode::NodeLink {
                        node: n.to_string(),
                    }
                }),
        ),
        LinkShape::WtGuest => (
            "allstar-public".to_string(),
            "allstar".to_string(),
            astar_iax::CallMode::WebTransceiver {
                node: node.to_string(),
                // A freshly minted portal token when the caller has one
                // (iax-b7f2) — required by WT contexts; otherwise the
                // node-id fallback identifies us in logs.
                name: wt_name
                    .map_or_else(|| node_id.unwrap_or("astar").to_string(), str::to_string),
            },
        ),
    }
}

/// Human-readable action name for the `command` field of
/// [`NodeEvent::Dtmf`] (iax-2f5e).
fn link_action_str(action: LinkAction) -> &'static str {
    match action {
        LinkAction::Connect => "connect",
        LinkAction::Monitor => "monitor",
        LinkAction::Disconnect => "disconnect",
    }
}

/// Map a [`LinkAction`] to the [`astar_iax::LinkMode`] to dial in, or `None`
/// for [`LinkAction::Disconnect`] (which tears a link down rather than dialing).
/// The `*3`=transceive / `*2`=monitor `AllStar` `ilink` semantics live here.
fn link_mode_for(action: LinkAction) -> Option<astar_iax::LinkMode> {
    match action {
        LinkAction::Connect => Some(astar_iax::LinkMode::Transceive),
        LinkAction::Monitor => Some(astar_iax::LinkMode::Monitor),
        LinkAction::Disconnect => None,
    }
}

/// Map one [`astar_iax::LinkEvent`] to `NodeEvent`s (iax-d829.1): a flattened,
/// secret-free `NodeEvent::Link` plus a fresh `Snapshot` so subscribers refresh
/// the link roster after each edge.
fn link_event_to_node_events(
    ev: astar_iax::LinkEvent,
    snap: impl Fn() -> NodeSnapshot,
) -> Vec<NodeEvent> {
    let link = match ev {
        astar_iax::LinkEvent::Connected { node, call } => NodeEvent::Link {
            kind: "connected".into(),
            node,
            call,
            reason: None,
            keyed: None,
        },
        astar_iax::LinkEvent::Disconnected { node, call, reason } => NodeEvent::Link {
            kind: "disconnected".into(),
            node,
            call,
            reason: Some(reason),
            keyed: None,
        },
        astar_iax::LinkEvent::Keyed { node, call, keyed } => NodeEvent::Link {
            kind: "keyed".into(),
            node,
            call,
            reason: None,
            keyed: Some(keyed),
        },
    };
    vec![link, NodeEvent::Snapshot(snap())]
}

/// Parse a `POST /bridge` mode string into a [`astar_iax::BridgeMode`]
/// (case-insensitive). `None` for an unrecognised value (iax-647d).
fn parse_bridge_mode(s: &str) -> Option<astar_iax::BridgeMode> {
    match s.to_ascii_lowercase().as_str() {
        "handset" => Some(astar_iax::BridgeMode::Handset),
        "bridge" => Some(astar_iax::BridgeMode::Bridge),
        "conference" => Some(astar_iax::BridgeMode::Conference),
        "parrot" => Some(astar_iax::BridgeMode::Parrot),
        _ => None,
    }
}

/// Map one `StationEvent` to zero-or-more `NodeEvent`s.
/// For lifecycle transitions we also emit a `Snapshot` so subscribers always
/// have a fresh view after each transition.
fn station_event_to_node_events(
    ev: StationEvent,
    snap: impl Fn() -> NodeSnapshot,
) -> Vec<NodeEvent> {
    match ev {
        StationEvent::IncomingCall { from } => {
            vec![
                NodeEvent::IncomingCall { from },
                NodeEvent::Snapshot(snap()),
            ]
        }
        StationEvent::Registered => {
            vec![NodeEvent::Registered, NodeEvent::Snapshot(snap())]
        }
        StationEvent::RegisterFailed { reason } => {
            vec![
                NodeEvent::RegisterFailed { reason },
                NodeEvent::Snapshot(snap()),
            ]
        }
        StationEvent::Hangup { reason } => {
            vec![NodeEvent::Hangup { reason }, NodeEvent::Snapshot(snap())]
        }
        // Answered / RemotePtt / ModeChanged — emit a Snapshot so the
        // subscriber can refresh its view.
        StationEvent::Answered | StationEvent::RemotePtt(_) | StationEvent::ModeChanged(_) => {
            vec![NodeEvent::Snapshot(snap())]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{NodeCommand, NodeReply};

    /// Build a test controller backed by `NullBackend` (no audio hardware).
    fn test_controller() -> NodeController {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        // Note: NodeController::new installs the secret resolver itself.
        NodeController::new(station, secrets)
    }

    /// Like [`test_controller`] but with `[dtmf] enabled = true` threaded
    /// through `with_configs` (iax-d254), so `apply_dtmf_digits` executes
    /// finalized `*` commands instead of ignoring them.
    fn test_controller_with_dtmf_enabled() -> NodeController {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            None,
            None,
            std::collections::HashMap::new(),
            true,
            3000,
        )
    }

    // -------------------------------------------------------------------------
    // iax-d254: pump wiring — DTMF `*` commands drive `handle_link`
    // -------------------------------------------------------------------------

    /// End-to-end inside the controller: set a link up over the HTTP path,
    /// then tear it down with digits `*1<node>#` — no DNS.
    #[test]
    fn dtmf_star1_disconnects_an_existing_link() {
        let ctl = test_controller_with_dtmf_enabled();
        // `_guard` and not `_`: a bare `_` drops the socket at once,
        // which closes the port and lets ICMP tear the call down
        // mid-assertion.
        let (_guard, addr) = crate::test_support::dead_port();
        ctl.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "1999".into(),
            addr: Some(addr),
        })
        .expect("link up");
        assert_eq!(ctl.snapshot().links.len(), 1);

        let now = std::time::Instant::now();
        let digits: Vec<(u64, char)> = "*11999#".chars().map(|c| (7, c)).collect();
        ctl.apply_dtmf_digits(digits, now);
        assert!(
            ctl.snapshot().links.is_empty(),
            "*1 1999 # tears the link down"
        );
    }

    /// `[dtmf] enabled` defaults to `false` — digits are drained but ignored.
    #[test]
    fn dtmf_digits_broadcast_on_sse_with_command_annotation() {
        // iax-2f5e: every drained digit surfaces as a NodeEvent::Dtmf, and the
        // digit that completes a sequence carries the resolved command.
        let ctl = test_controller_with_dtmf_enabled();
        let rx = ctl.subscribe();
        let now = std::time::Instant::now();
        let digits: Vec<(u64, char)> = "*11999#".chars().map(|c| (7, c)).collect();
        ctl.apply_dtmf_digits(digits, now);

        let mut seen: Vec<(String, Option<String>)> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let NodeEvent::Dtmf { digit, command, .. } = ev {
                seen.push((digit, command));
            }
        }
        assert_eq!(seen.len(), 7, "one event per digit: {seen:?}");
        assert_eq!(seen[0].0, "*");
        assert!(
            seen[..6].iter().all(|(_, c)| c.is_none()),
            "only the terminator carries the command: {seen:?}"
        );
        assert_eq!(
            seen[6],
            ("#".to_string(), Some("disconnect 1999".to_string())),
            "the # that finalizes reports the mapped command"
        );
    }

    #[test]
    fn dtmf_disabled_ignores_digits() {
        let ctl = test_controller();
        // `_guard` and not `_`: a bare `_` drops the socket at once,
        // which closes the port and lets ICMP tear the call down
        // mid-assertion.
        let (_guard, addr) = crate::test_support::dead_port();
        ctl.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "1999".into(),
            addr: Some(addr),
        })
        .expect("link up");
        let now = std::time::Instant::now();
        let digits: Vec<(u64, char)> = "*11999#".chars().map(|c| (7, c)).collect();
        ctl.apply_dtmf_digits(digits, now);
        assert_eq!(ctl.snapshot().links.len(), 1, "disabled: command ignored");
    }

    #[test]
    fn status_returns_snapshot_not_listening() {
        let c = test_controller();
        let NodeReply::Snapshot(s) = c.execute(NodeCommand::Status).unwrap() else {
            panic!("expected Snapshot reply")
        };
        assert!(!s.listening, "new controller must not be listening");
        assert_eq!(s.calls.len(), 0, "no calls on idle controller");
    }

    // -------------------------------------------------------------------------
    // iax-d829.1 (ported from iax-213f): node-to-node link control
    // -------------------------------------------------------------------------

    /// The `*3`=transceive / `*2`=monitor / `*1`=disconnect mapping is pure.
    #[test]
    fn link_mode_for_maps_connect_and_monitor_and_disconnect() {
        assert_eq!(
            link_mode_for(LinkAction::Connect),
            Some(astar_iax::LinkMode::Transceive)
        );
        assert_eq!(
            link_mode_for(LinkAction::Monitor),
            Some(astar_iax::LinkMode::Monitor)
        );
        assert_eq!(link_mode_for(LinkAction::Disconnect), None);
    }

    /// A connect to an explicit address (no DNS) registers a transceive link in
    /// the snapshot; a disconnect by node clears it.
    #[test]
    fn link_connect_at_addr_then_disconnect_round_trips_through_snapshot() {
        let c = test_controller();
        assert!(
            c.snapshot().links.is_empty(),
            "idle controller has no links"
        );

        // `_guard` and not `_`: a bare `_` drops the socket at once,
        // which closes the port and lets ICMP tear the call down
        // mid-assertion.
        let (_guard, addr) = crate::test_support::dead_port();
        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "55553".into(),
            addr: Some(addr),
        })
        .expect("connect link at addr");

        let links = c.snapshot().links;
        assert_eq!(links.len(), 1, "one link after connect");
        assert_eq!(links[0].node, "55553");
        assert_eq!(links[0].mode, astar_iax::LinkMode::Transceive);

        c.execute(NodeCommand::Link {
            action: LinkAction::Disconnect,
            node: "55553".into(),
            addr: None,
        })
        .expect("disconnect link");
        assert!(c.snapshot().links.is_empty(), "links cleared on disconnect");
    }

    /// `monitor` connects in RX-only mode.
    #[test]
    fn link_monitor_at_addr_registers_a_monitor_link() {
        let c = test_controller();
        // `_guard` and not `_`: a bare `_` drops the socket at once,
        // which closes the port and lets ICMP tear the call down
        // mid-assertion.
        let (_guard, addr) = crate::test_support::dead_port();
        c.execute(NodeCommand::Link {
            action: LinkAction::Monitor,
            node: "42".into(),
            addr: Some(addr),
        })
        .expect("monitor link");
        let links = c.snapshot().links;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].mode, astar_iax::LinkMode::Monitor);
    }

    /// `AllStar` `ilink` semantics: `*3` after `*2` upgrades the EXISTING link's
    /// mode in place (via `link_set_mode`) — no second dial, no DNS, still one
    /// roster entry.
    #[test]
    fn link_connect_on_existing_monitor_link_switches_mode_in_place() {
        let c = test_controller();
        // `_guard` and not `_`: a bare `_` drops the socket at once,
        // which closes the port and lets ICMP tear the call down
        // mid-assertion.
        let (_guard, addr) = crate::test_support::dead_port();
        c.execute(NodeCommand::Link {
            action: LinkAction::Monitor,
            node: "55553".into(),
            addr: Some(addr),
        })
        .expect("monitor link");

        // No addr on purpose: an existing link must NOT trigger a re-dial (or
        // DNS resolution) — just a mode switch.
        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "55553".into(),
            addr: None,
        })
        .expect("upgrade to transceive");

        let links = c.snapshot().links;
        assert_eq!(links.len(), 1, "still exactly one link");
        assert_eq!(links[0].mode, astar_iax::LinkMode::Transceive);
    }

    /// Disconnecting a node with no live link is an error, not a panic.
    #[test]
    fn link_disconnect_unknown_node_is_error() {
        let c = test_controller();
        let r = c.execute(NodeCommand::Link {
            action: LinkAction::Disconnect,
            node: "99999".into(),
            addr: None,
        });
        assert!(r.is_err(), "disconnecting an unknown node must error");
    }

    /// The link-event → node-event mapper flattens each variant and appends a
    /// roster-refresh snapshot.
    #[test]
    fn link_event_maps_to_node_event_link_with_snapshot() {
        let snap = || NodeSnapshot {
            node_id: None,
            listening: false,
            registered: false,
            calls: vec![],
            links: vec![],
            discord: None,
            m17: None,
            m17_enabled: false,
            m17_available: false,
            m17_relay: None,
            echolink: None,
            echolink_enabled: false,
            public: None,
        };
        let connected = link_event_to_node_events(
            astar_iax::LinkEvent::Connected {
                node: "55553".into(),
                call: 7,
            },
            snap,
        );
        assert!(connected.iter().any(|e| matches!(
            e,
            NodeEvent::Link { kind, node, call, .. } if kind == "connected" && node == "55553" && *call == 7
        )));
        assert!(
            connected
                .iter()
                .any(|e| matches!(e, NodeEvent::Snapshot(_))),
            "a roster-refresh snapshot follows the edge"
        );

        let disc = link_event_to_node_events(
            astar_iax::LinkEvent::Disconnected {
                node: "55553".into(),
                call: 7,
                reason: "peer hung up".into(),
            },
            snap,
        );
        assert!(disc.iter().any(|e| matches!(
            e,
            NodeEvent::Link { kind, reason: Some(r), .. } if kind == "disconnected" && r == "peer hung up"
        )));

        let keyed = link_event_to_node_events(
            astar_iax::LinkEvent::Keyed {
                node: "55553".into(),
                call: 7,
                keyed: true,
            },
            snap,
        );
        assert!(keyed.iter().any(|e| matches!(
            e,
            NodeEvent::Link { kind, keyed: Some(true), .. } if kind == "keyed"
        )));
    }

    #[test]
    fn enable_inbound_makes_it_listen() {
        let c = test_controller();
        c.execute(NodeCommand::EnableInbound).unwrap();
        let NodeReply::Snapshot(s) = c.execute(NodeCommand::Status).unwrap() else {
            panic!("expected Snapshot reply")
        };
        assert!(s.listening, "must be listening after EnableInbound");
    }

    #[test]
    fn disable_inbound_stops_listening() {
        let c = test_controller();
        c.execute(NodeCommand::EnableInbound).unwrap();
        c.execute(NodeCommand::DisableInbound).unwrap();
        let NodeReply::Snapshot(s) = c.execute(NodeCommand::Status).unwrap() else {
            panic!("expected Snapshot reply")
        };
        assert!(!s.listening, "must not be listening after DisableInbound");
    }

    #[test]
    fn provide_secret_feeds_provider_and_is_not_echoed() {
        let c = test_controller();
        let r = c
            .execute(NodeCommand::ProvideSecret {
                username: "1234".into(),
                secret: "x".into(),
            })
            .unwrap();
        // Reply is Ok, never echoes the secret.
        assert!(matches!(r, NodeReply::Ok));
        // The value is accessible through the provider.
        assert_eq!(c.secrets().resolve("1234"), "x");
        // Serialised reply carries no credential.
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains('x'),
            "reply JSON must not contain the secret: {json}"
        );
    }

    /// iax-99cd: the controller wires `inbound_cfg.policy.credential_resolver`
    /// from the SAME `SecretProvider` that `ProvideSecret`/env feed, so inbound
    /// auth resolves runtime-provisioned credentials.
    #[test]
    fn controller_wires_inbound_credential_resolver_from_secrets() {
        let c = test_controller();
        let resolver = c
            .inbound_cfg
            .policy
            .credential_resolver
            .as_ref()
            .expect("inbound policy must carry a credential_resolver");
        // Provision AFTER construction → proves the installed resolver shares
        // the controller's live secret store (not a snapshot copy).
        c.execute(NodeCommand::ProvideSecret {
            username: "allstar-public".into(),
            secret: "allstar".into(),
        })
        .unwrap();
        assert_eq!(resolver("allstar-public"), "allstar");
        assert_eq!(resolver("nobody"), "", "unknown user resolves to empty");
    }

    /// iax-99cd: `with_configs` likewise installs the resolver onto the supplied
    /// inbound config's policy.
    #[test]
    fn with_configs_wires_inbound_credential_resolver() {
        let secrets = SecretProvider::new();
        secrets.put("77777", "topsecret");
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let inbound_cfg = InboundConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..InboundConfig::default()
        };
        let c = NodeController::with_configs(
            station,
            secrets,
            inbound_cfg,
            None,
            None,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        let resolver = c
            .inbound_cfg
            .policy
            .credential_resolver
            .as_ref()
            .expect("with_configs must install a credential_resolver");
        assert_eq!(resolver("77777"), "topsecret");
    }

    #[test]
    #[allow(clippy::duration_suboptimal_units)]
    fn snapshot_node_id_defaults_none_and_follows_register_username() {
        assert_eq!(test_controller().snapshot().node_id, None);

        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let inbound_cfg = InboundConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..InboundConfig::default()
        };
        let c = NodeController::with_configs(
            station,
            secrets,
            inbound_cfg,
            Some(RegisterConfig {
                fallbacks: Vec::new(),
                peer: "127.0.0.1:4569".parse().unwrap(),
                username: "77777".into(),
                refresh: std::time::Duration::from_secs(60),
            }),
            None,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        assert_eq!(c.snapshot().node_id.as_deref(), Some("77777"));
    }

    // -- iax-9e02: link announcements ---------------------------------------

    #[test]
    fn link_announce_text_uses_defaults_with_spaced_digits() {
        let ctl = test_controller();
        assert_eq!(
            ctl.link_announce_text(true, "55553"),
            "Connecting to node 5 5 5 5 3",
            "digits are spaced so TTS reads them one by one"
        );
        assert_eq!(
            ctl.link_announce_text(false, "1999"),
            "Disconnected from node 1 9 9 9"
        );
    }

    // -- M17 monitor announcements -----------------------------------------

    /// An `[announce]` section carrying only the two M17 templates, for the
    /// override tests.
    fn announce_cfg_with_m17_templates(
        connect: Option<&str>,
        disconnect: Option<&str>,
    ) -> crate::config::AnnounceCfg {
        crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: None,
            id_mode: None,
            cw_wpm: None,
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: connect.map(str::to_string),
            m17_disconnect_template: disconnect.map(str::to_string),
            tts: None,
            events: None,
        }
    }

    #[test]
    fn m17_announce_text_uses_defaults_with_spelled_reflector() {
        let ctl = test_controller();
        assert_eq!(
            ctl.m17_announce_text(true, "M17-USA", 'C'),
            "Monitoring M17 reflector M 1 7 - U S A module C",
            "the name is spelled so TTS reads it character by character"
        );
        assert_eq!(
            ctl.m17_announce_text(false, "M17-USA", 'C'),
            "Stopped monitoring M17 reflector M 1 7 - U S A module C"
        );
    }

    #[test]
    fn m17_announce_text_default_is_unaffected_by_link_templates() {
        // The link templates are for AllStar dials only; overriding them must
        // not leak into the M17 phrase.
        let mut ctl = test_controller();
        let mut cfg = announce_cfg_with_m17_templates(None, None);
        cfg.link_connect_template = Some("Link up {node}".into());
        cfg.link_disconnect_template = Some("Link down {node}".into());
        ctl.announce_cfg = Some(cfg);
        assert_eq!(
            ctl.m17_announce_text(true, "M17-USA", 'C'),
            "Monitoring M17 reflector M 1 7 - U S A module C"
        );
        assert_eq!(
            ctl.m17_announce_text(false, "M17-USA", 'C'),
            "Stopped monitoring M17 reflector M 1 7 - U S A module C"
        );
    }

    #[test]
    fn m17_announce_text_custom_template_with_node_token() {
        let mut ctl = test_controller();
        ctl.announce_cfg = Some(announce_cfg_with_m17_templates(
            Some("Now hearing {node} on the air"),
            Some("Done with {node}"),
        ));
        assert_eq!(
            ctl.m17_announce_text(true, "M17-M17", 'A'),
            "Now hearing M 1 7 - M 1 7 module A on the air"
        );
        assert_eq!(
            ctl.m17_announce_text(false, "M17-M17", 'A'),
            "Done with M 1 7 - M 1 7 module A"
        );
    }

    #[test]
    fn m17_announce_text_custom_template_without_node_token() {
        // A template with no `{node}` is spoken verbatim: the operator chose
        // not to name the reflector.
        let mut ctl = test_controller();
        ctl.announce_cfg = Some(announce_cfg_with_m17_templates(
            Some("Reflector audio on"),
            Some("Reflector audio off"),
        ));
        assert_eq!(
            ctl.m17_announce_text(true, "M17-USA", 'C'),
            "Reflector audio on"
        );
        assert_eq!(
            ctl.m17_announce_text(false, "M17-USA", 'C'),
            "Reflector audio off"
        );
    }

    #[test]
    fn m17_announce_text_override_one_side_keeps_other_default() {
        let mut ctl = test_controller();
        ctl.announce_cfg = Some(announce_cfg_with_m17_templates(
            Some("Hearing {node}"),
            None,
        ));
        assert_eq!(
            ctl.m17_announce_text(true, "M17-USA", 'C'),
            "Hearing M 1 7 - U S A module C"
        );
        assert_eq!(
            ctl.m17_announce_text(false, "M17-USA", 'C'),
            "Stopped monitoring M17 reflector M 1 7 - U S A module C"
        );
    }

    #[test]
    fn m17_announcement_kind_labels_are_exact() {
        // These are the SSE `announcement_started` kinds a dashboard keys on;
        // they must stay distinct from the link ones.
        assert_eq!(m17_announcement_kind(true), "m17_connect");
        assert_eq!(m17_announcement_kind(false), "m17_disconnect");
    }

    // -- iax-5029: per-target dial shape + secret selection -----------------

    #[test]
    fn handle_link_refuses_an_addr_that_is_not_a_loopback_literal() {
        let c = test_controller();
        for addr in ["192.0.2.1:4569", "localhost:4569", "[::]:4569"] {
            let e = c
                .handle_link(LinkAction::Connect, "55553", Some(addr))
                .expect_err(addr);
            assert!(e.message.contains("loopback"), "{addr}: {}", e.message);
        }
        assert!(c.station.link_roster().links.is_empty(), "nothing dialled");
        assert!(link_addr_is_loopback("127.0.0.1:4569"));
        assert!(link_addr_is_loopback("127.8.9.10:4569"));
        assert!(link_addr_is_loopback("[::1]:4569"));
        assert!(!link_addr_is_loopback("10.0.0.1:4569"));
        assert!(!link_addr_is_loopback("127.0.0.1"));
    }

    #[test]
    fn link_dial_params_standard_resolves_link_namespace_secret() {
        let secrets = SecretProvider::new();
        secrets.put("link:1999", "outbound-pw");
        let (caller_id, secret, mode) =
            link_dial_params(LinkShape::Standard, "1999", Some("77777"), None, &secrets);
        assert_eq!(caller_id, "77777");
        assert_eq!(secret, "outbound-pw");
        // iax-cnlink: app_rpt drops a link whose caller ID is not numeric,
        // so a node link carries our node number as CALLING_NUMBER.
        assert_eq!(
            mode,
            astar_iax::CallMode::NodeLink {
                node: "77777".into()
            }
        );
    }

    #[test]
    fn link_dial_params_standard_miss_keeps_empty_secret() {
        let secrets = SecretProvider::new();
        let (caller_id, secret, mode) =
            link_dial_params(LinkShape::Standard, "1999", None, None, &secrets);
        assert_eq!(caller_id, "");
        assert_eq!(secret, "");
        assert_eq!(mode, astar_iax::CallMode::Standard);
    }

    #[test]
    fn link_dial_params_wt_guest_uses_guest_creds_and_wt_shape() {
        let secrets = SecretProvider::new();
        // Even a configured link secret is IGNORED for wt-guest — guest auth
        // is fixed by the ASL3 contract.
        secrets.put("link:55553", "should-not-be-used");
        let (caller_id, secret, mode) =
            link_dial_params(LinkShape::WtGuest, "55553", Some("77777"), None, &secrets);
        assert_eq!(caller_id, "allstar-public");
        assert_eq!(secret, "allstar");
        assert_eq!(
            mode,
            astar_iax::CallMode::WebTransceiver {
                node: "55553".into(),
                name: "77777".into(),
            }
        );
    }

    #[test]
    fn link_dial_params_wt_guest_prefers_minted_token_name() {
        // iax-b7f2: a freshly minted portal token wins over the node-id
        // fallback — WT contexts validate CALLING_NAME server-side.
        let secrets = SecretProvider::new();
        let (caller_id, secret, mode) = link_dial_params(
            LinkShape::WtGuest,
            "55553",
            Some("77777"),
            Some("tok-3fca90"),
            &secrets,
        );
        assert_eq!(caller_id, "allstar-public");
        assert_eq!(secret, "allstar");
        assert_eq!(
            mode,
            astar_iax::CallMode::WebTransceiver {
                node: "55553".into(),
                name: "tok-3fca90".into(),
            }
        );
    }

    #[test]
    fn link_dial_params_wt_guest_without_node_id_falls_back() {
        let secrets = SecretProvider::new();
        let (_, _, mode) = link_dial_params(LinkShape::WtGuest, "55553", None, None, &secrets);
        assert_eq!(
            mode,
            astar_iax::CallMode::WebTransceiver {
                node: "55553".into(),
                name: "astar".into(),
            }
        );
    }

    /// Guard: `NodeError` Display and JSON carry no credential.
    #[test]
    fn node_error_is_secret_free() {
        let err = NodeError {
            message: "connection refused".into(),
        };
        let display = err.message.clone();
        let json = serde_json::to_string(&err).unwrap();
        for bad in ["secret", "password", "token", "hunter2"] {
            assert!(
                !display.contains(bad),
                "NodeError display contains forbidden word: {bad}"
            );
            assert!(
                !json.contains(bad),
                "NodeError JSON contains forbidden word: {bad}"
            );
        }
    }

    /// Guard: `NodeSnapshot` serialisation carries no credential.
    #[test]
    fn snapshot_is_secret_free() {
        let c = test_controller();
        let snap = c.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        for bad in ["secret", "password", "token"] {
            assert!(
                !json.contains(bad),
                "NodeSnapshot JSON contains forbidden word: {bad}"
            );
        }
    }

    /// `configured` must mean "ready", not "mentioned": a `[discord]`
    /// section with no token supplied is the single most likely
    /// misconfiguration, and reporting `configured: true` at it would send
    /// an operator debugging guild/channel ids instead of the missing
    /// token. Pins the transition, not just one static state — a hardcoded
    /// `false` would pass a test that only checked the pre-token case.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_configured_reflects_token_presence_not_just_the_section() {
        let mut c = test_controller();
        c.set_discord_config(Some(crate::config::DiscordCfg {
            guild_id: "1".into(),
            channel_id: "2".into(),
            ..crate::config::DiscordCfg::default()
        }));

        let before = c.snapshot();
        assert_eq!(
            before.discord.as_ref().map(|d| d.configured),
            Some(false),
            "a [discord] section with no token must not report configured: true"
        );

        c.execute(NodeCommand::ProvideSecret {
            username: "discord".into(),
            secret: "test-token".into(),
        })
        .expect("providing a secret never fails");

        let after = c.snapshot();
        assert_eq!(
            after.discord.as_ref().map(|d| d.configured),
            Some(true),
            "supplying the token must flip configured to true"
        );
    }

    /// Builds a live engine over loopback (no traffic leaves 127.0.0.1) and
    /// enrolls one real conference member — the same
    /// `Station::add_bridge_member` call `handle_discord`'s Join makes —
    /// then installs a `Bridge` built from astar-discord's own
    /// network-free test doubles (`TestLink`/`RecordingLink`/`Bypass`)
    /// rather than a real `SongbirdLink`. This simulates "already joined"
    /// state without ever starting a gateway — `Gateway::connect` reaches
    /// out to Discord's real servers even with a garbage token, which is
    /// forbidden for a test under this workspace's safety rules, and a
    /// voice handle can only come from one. Returns the member
    /// id installed, for callers that need to assert on withdrawal.
    #[cfg(feature = "discord")]
    /// Install a `Bridge` over a `RecordingLink` in the same place a real
    /// Join would put it, and hand back both the enrolled member id and the
    /// link, so a test can read every status line the bridge posts.
    #[cfg(feature = "discord")]
    fn install_simulated_discord_join(
        c: &NodeController,
    ) -> (
        astar_audio::MemberId,
        std::sync::Arc<astar_discord::RecordingLink>,
    ) {
        c.station
            .enable_inbound(InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            })
            .expect("inbound listener starts and builds the engine");
        c.station
            .set_bridge_config(astar_iax::BridgeConfig {
                mode: astar_iax::BridgeMode::Bridge,
                mix_minus: true,
                include_local_radio: false,
                ..Default::default()
            })
            .expect("bridge mode is valid");
        let handle = c.station.add_bridge_member().expect("bridge mode enrolls");
        let member_id = handle.id;
        let rate = handle.sample_rate;
        let voice = std::sync::Arc::new(astar_discord::RecordingLink::new());
        let mut bridge = astar_discord::Bridge::new(
            Box::new(astar_discord::ConferenceLink::new(handle)),
            Box::new(std::sync::Arc::clone(&voice)),
            Box::new(astar_discord::Bypass::new(rate)),
            "node 12345".to_string(),
        );
        // Through `Bridge::join`, not by hand: that is what marks the bridge
        // connected, and the heartbeat is only posted while it is.
        bridge.join(1, 2).expect("RecordingLink always joins");
        *c.discord_member
            .lock()
            .expect("discord member mutex poisoned") = Some(member_id);
        *c.discord.lock().expect("discord bridge mutex poisoned") = Some(bridge);
        (member_id, voice)
    }

    /// `connected` and `audio_ok` are different questions, and the bug
    /// this pins is that `/status` answered only the first: the audio track
    /// into Discord can die while the `Bridge` object is perfectly present,
    /// and for the whole life of the first bridge it did — `connected:
    /// true`, `frames_sent` climbing, the channel hearing silence.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_reports_audio_separately_from_connected() {
        let mut c = test_controller();
        // A `[discord]` section is what makes the snapshot carry a Discord
        // status at all.
        c.set_discord_config(Some(crate::config::DiscordCfg {
            guild_id: "111".into(),
            channel_id: "222".into(),
            approvers: vec!["333".into()],
            ..crate::config::DiscordCfg::default()
        }));

        let idle = c
            .snapshot()
            .discord
            .expect("the fixture has a [discord] section");
        assert!(!idle.connected, "nothing has joined yet");
        assert!(
            !idle.audio_ok,
            "and with no bridge installed no audio is reaching Discord"
        );

        let _ = install_simulated_discord_join(&c);

        let joined = c
            .snapshot()
            .discord
            .expect("the fixture has a [discord] section");
        assert!(joined.connected, "a bridge is installed");
        assert!(
            joined.audio_ok,
            "and nothing has failed to play, so the audio path is good"
        );
        assert_eq!(
            joined.last_error, None,
            "a healthy bridge reports no failure"
        );
    }

    /// **`serve` never drops the controller.** `main` wraps it in an `Arc`
    /// and moves a clone into the ctrlc handler's closure, which lives for
    /// the life of the process — so the field-order teardown the struct's
    /// comments describe has never once run in the shipped daemon: no
    /// `Bridge` drop, no `Gateway::drop`, and therefore no `GatewayCmd::
    /// Shutdown` and no wait for the unmutes behind it. `teardown_discord`
    /// is that missing step, and everything it does it has to do itself.
    #[cfg(feature = "discord")]
    #[test]
    fn teardown_discord_withdraws_the_member_and_lets_go_of_the_gateway() {
        let c = test_controller();
        let _ = install_simulated_discord_join(&c);
        // An orphan spawns no thread and reaches no network; it stands in
        // for "a gateway is installed", which is what must be let go of.
        *c.discord_gateway
            .lock()
            .expect("discord gateway mutex poisoned") = Some(astar_discord::Gateway::orphan());
        assert_eq!(
            c.conference_member_count_for_test(),
            1,
            "the simulated join enrolled exactly one member"
        );

        c.teardown_discord();

        assert_eq!(
            c.conference_member_count_for_test(),
            0,
            "a node that is going away withdraws its conference member, exactly as Leave does"
        );
        assert!(
            c.discord
                .lock()
                .expect("discord bridge mutex poisoned")
                .is_none(),
            "the bridge is dropped — its voice handle is what asks to leave the channel"
        );
        assert!(
            c.discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned")
                .is_none(),
            "and the gateway after it: dropping it is what sends Shutdown and joins the thread"
        );

        // Idempotent, because it has to be: a run mode calls it at the end
        // of the run whether or not a signal already took the node through
        // a `Shutdown`.
        c.teardown_discord();
        assert_eq!(c.conference_member_count_for_test(), 0);
    }

    /// Regression test for the bug where `Leave` called `voice.leave()` and
    /// dropped the `Bridge` but never called `Station::remove_bridge_member`
    /// — every join/leave cycle stranded a member in the conference's
    /// member table forever.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_leave_withdraws_the_conference_member() {
        let c = test_controller();
        let _ = install_simulated_discord_join(&c);
        assert_eq!(
            c.conference_member_count_for_test(),
            1,
            "the simulated join enrolled exactly one member"
        );

        let r = c.execute(NodeCommand::Discord {
            action: crate::command::DiscordAction::Leave,
        });
        assert!(r.is_ok(), "leave must succeed: {r:?}");

        assert_eq!(
            c.conference_member_count_for_test(),
            0,
            "Leave must withdraw the conference member, not just drop the Bridge"
        );
    }

    /// Regression test for the bug where a second `Join` had no
    /// "already joined" guard: it enrolled another conference member,
    /// overwrote `discord_member` (losing the previous id forever, so it
    /// could never be cleaned up even manually), and overwrote the
    /// `Bridge`. The member-count assertion is the one that actually
    /// catches this — an error-only assertion would still pass even if a
    /// second member leaked in behind the scenes.
    ///
    /// No `[discord] section`/token is configured here on purpose: the
    /// guard in `handle_discord` checks `self.discord.lock().is_some()`
    /// FIRST, before either is read, so this test exercises the exact
    /// production check without needing them. Configuring a real token
    /// would not make the test stronger — it would make it unsafe: once
    /// past the guard, `handle_discord` reaches `ensure_discord_gateway`,
    /// which calls `Gateway::connect` on a non-empty token and starts a
    /// real attempt to reach Discord's servers (an unauthenticated "Get
    /// Gateway" call) before `Bridge::join` is ever invoked. There is no
    /// way to reach the actual second-`add_bridge_member` call this bug
    /// produced without also reaching that network call, so proving the
    /// guard's absence would leak a member cannot be done from a
    /// live-reverted test without violating "no test may contact Discord".
    /// This test instead proves the guard that PREVENTS the leak fires and
    /// stops the command before any of that — the count assertion holds for
    /// the actual shipped check order, not a stand-in for it.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_join_refuses_when_already_joined_and_does_not_leak_a_member() {
        let c = test_controller();
        let _ = install_simulated_discord_join(&c);
        let count_before = c.conference_member_count_for_test();
        assert_eq!(
            count_before, 1,
            "one member enrolled from the simulated join"
        );

        let r = c.execute(NodeCommand::Discord {
            action: crate::command::DiscordAction::Join,
        });
        let err = r.expect_err("a second Join while already joined must be refused");
        assert!(
            err.message.contains("already joined"),
            "unexpected error: {}",
            err.message
        );

        let count_after = c.conference_member_count_for_test();
        assert_eq!(
            count_after, count_before,
            "a refused Join must not enroll another member (before: {count_before}, after: {count_after})"
        );
    }

    /// Regression test for the bug where every failure between
    /// `add_bridge_member` and a successful `Bridge::join` stranded the
    /// enrolled member forever — no concurrency needed, just an operator
    /// with a typo'd snowflake or a revoked token, on every retry. Uses an
    /// unparseable `guild_id` as the trigger: it fails inside
    /// `complete_discord_join` BEFORE `ensure_discord_gateway` is reached
    /// (parsing runs first precisely so this is testable), so this reaches
    /// the real `handle_discord` Join path end to end through `execute()`
    /// without a token ever being used to attempt a network connection.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_join_withdraws_the_member_when_a_later_step_fails() {
        let mut c = test_controller();
        c.station
            .enable_inbound(InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            })
            .expect("inbound listener starts and builds the engine");
        c.station
            .set_bridge_config(astar_iax::BridgeConfig {
                mode: astar_iax::BridgeMode::Bridge,
                mix_minus: true,
                include_local_radio: false,
                ..Default::default()
            })
            .expect("bridge mode is valid");
        c.set_discord_config(Some(crate::config::DiscordCfg {
            guild_id: "not-a-snowflake".into(),
            channel_id: "2".into(),
            ..crate::config::DiscordCfg::default()
        }));
        c.execute(NodeCommand::ProvideSecret {
            username: "discord".into(),
            secret: "unused-because-guild-parse-fails-first".into(),
        })
        .expect("providing a secret never fails");

        let count_before = c.conference_member_count_for_test();
        assert_eq!(count_before, 0, "nothing enrolled before the attempt");

        let r = c.execute(NodeCommand::Discord {
            action: crate::command::DiscordAction::Join,
        });
        let err = r.expect_err("an unparseable guild_id must fail the Join");
        assert!(
            err.message.contains("guild_id is not a snowflake"),
            "unexpected error: {}",
            err.message
        );

        let count_after = c.conference_member_count_for_test();
        assert_eq!(
            count_after, count_before,
            "a failed Join must withdraw the member it enrolled, not strand it \
             (before: {count_before}, after: {count_after})"
        );
    }

    /// The gateway's lifetime is now the controller's to manage, so a Join
    /// that fails must not leave the bot online on the non-persistent path
    /// — and must not leave a runtime thread behind that nothing will ever
    /// join. Sits beside the member-withdrawal test above and uses the same
    /// trigger for the same reason: an unparseable `guild_id` fails inside
    /// `complete_discord_join` while the snowflake parse still runs BEFORE
    /// `ensure_discord_gateway`, so no thread is spawned and Discord is
    /// never contacted.
    ///
    /// A `Gateway::orphan()` is installed first, and that is what gives the
    /// test teeth: without one, "no gateway afterwards" would be true
    /// because none was ever created, and deleting the whole `Err`-arm
    /// teardown would still pass. An orphan spawns no thread and reaches no
    /// network — it is a command channel whose receiver is already gone —
    /// so it stands in for "a gateway is installed" without
    /// `Gateway::connect` and a real token, which would talk to Discord.
    ///
    /// What it still cannot reach is a gateway that was started by THIS
    /// join and then torn down; that needs `connect`. The teardown code it
    /// exercises is the same code either way.
    #[cfg(feature = "discord")]
    #[test]
    fn a_failed_join_leaves_no_gateway_behind_when_not_persistent() {
        let mut c = test_controller();
        c.station
            .enable_inbound(InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            })
            .expect("inbound listener starts and builds the engine");
        c.station
            .set_bridge_config(astar_iax::BridgeConfig {
                mode: astar_iax::BridgeMode::Bridge,
                mix_minus: true,
                include_local_radio: false,
                ..Default::default()
            })
            .expect("bridge mode is valid");
        c.set_discord_config(Some(crate::config::DiscordCfg {
            guild_id: "not-a-snowflake".into(),
            channel_id: "2".into(),
            ..crate::config::DiscordCfg::default()
        }));
        c.execute(NodeCommand::ProvideSecret {
            username: "discord".into(),
            secret: "unused-because-guild-parse-fails-first".into(),
        })
        .expect("providing a secret never fails");

        assert!(
            !c.discord_gateway_is_persistent(),
            "this test is about the non-persistent path; if persistence ever \
             defaults on, the expectation below changes with it"
        );
        // Installed before the attempt: the `Err` arm must take THIS away.
        *c.discord_gateway
            .lock()
            .expect("discord gateway mutex poisoned") = Some(astar_discord::Gateway::orphan());

        let r = c.execute(NodeCommand::Discord {
            action: crate::command::DiscordAction::Join,
        });
        let err = r.expect_err("an unparseable guild_id must fail the Join");
        // Pinned as an equality, not a `contains`: it fixes WHICH step
        // failed. The whole Join must end at the parse, because every step
        // after it in this configuration needs the network. (It cannot by
        // itself prove the parse ran FIRST — a reordered `complete_join`
        // would produce this same message after starting a gateway. What it
        // does is make any other failure, including one that could only
        // come from a reached gateway, a test failure.)
        assert_eq!(
            err.message, "discord: guild_id is not a snowflake",
            "the Join must end at the snowflake parse, before anything reaches the network"
        );

        assert!(
            c.discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned")
                .is_none(),
            "a failed Join must leave no gateway behind on the non-persistent path"
        );
    }

    /// A gateway whose runtime thread has died must be discarded, not
    /// reused: it can never reconnect, because the thread that would do the
    /// reconnecting is the one that died.
    ///
    /// Only the DETECTION half is exercised, and deliberately so. The
    /// replacement itself calls `Gateway::connect`, which reaches Discord
    /// with any non-empty token, so this configures no token at all: the
    /// dead gateway must still be taken out and dropped, and the call must
    /// then fail at the missing token rather than returning `Ok` on the
    /// corpse. Without the `is_failed` check `ensure_discord_gateway`
    /// returns `Ok(())` immediately and the corpse is still installed —
    /// which is exactly what both assertions below would catch.
    #[cfg(feature = "discord")]
    #[test]
    fn a_dead_gateway_is_replaced_not_reused() {
        let c = test_controller();
        *c.discord_gateway
            .lock()
            .expect("discord gateway mutex poisoned") =
            Some(astar_discord::Gateway::orphan_failed());

        let err = c
            .ensure_discord_gateway()
            .expect_err("no token is configured, so no replacement can be made");

        assert_eq!(
            err.message, "discord: no bot token supplied",
            "the call must get as far as trying to connect a replacement"
        );
        assert!(
            c.discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned")
                .is_none(),
            "a dead gateway must be taken out and dropped, never left installed"
        );
    }

    /// A `[discord]` section and no token: the Join stops at the token check,
    /// before anything reaches the network, and the sentence names the ways a
    /// token reaches a node on the generated template. `DISCORD_BOT_TOKEN`
    /// is read only under `source = "env"`, so it is not the advice to give.
    #[cfg(feature = "discord")]
    #[test]
    fn a_join_without_a_token_points_at_the_settings_page() {
        let mut c = test_controller();
        c.set_discord_config(Some(crate::config::DiscordCfg {
            guild_id: "1".into(),
            channel_id: "2".into(),
            ..crate::config::DiscordCfg::default()
        }));

        let err = c
            .execute(NodeCommand::Discord {
                action: crate::command::DiscordAction::Join,
            })
            .expect_err("no token is configured");
        assert_eq!(
            err.message,
            "discord: no bot token (set it on the settings page, or POST /secrets with username \"discord\")"
        );
    }

    /// The other half of the same rule: a working gateway is left exactly
    /// where it is, and no second one is started. `ensure_*` is called twice
    /// with no token configured — an orphan reports healthy, so both calls
    /// must succeed without ever reaching `Gateway::connect` (which, with no
    /// token, would fail).
    #[cfg(feature = "discord")]
    #[test]
    fn a_live_gateway_is_reused_rather_than_restarted() {
        let c = test_controller();
        *c.discord_gateway
            .lock()
            .expect("discord gateway mutex poisoned") = Some(astar_discord::Gateway::orphan());

        c.ensure_discord_gateway()
            .expect("a healthy gateway needs no token to keep");
        c.ensure_discord_gateway()
            .expect("and the second call is the idempotent one");
        assert!(
            c.discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned")
                .is_some(),
            "an idempotent ensure must leave the gateway it found in place"
        );
    }

    // ---- Task 10: periodic Discord ID -----------------------------------------

    /// With no `[discord]` section the ID timer must never fire and must
    /// never panic — the common case for every node that does not use this.
    #[test]
    fn discord_id_is_spoken_on_the_announce_interval() {
        let c = test_controller();
        for _ in 0..10 {
            c.pump();
        }
        let snap = c.snapshot();
        assert!(snap.discord.is_none(), "no section, no status");
    }

    /// Joined to the channel but `[announce] id_interval_secs` is unset
    /// (no `announce_cfg` at all, the default `test_controller()` state):
    /// the timer must never arm. Asserting `discord_id_due` directly (rather
    /// than e.g. conference member count, which the ID timer can never touch
    /// either way) is the assertion that actually discriminates: it would
    /// fail if the whole `#[cfg(feature = "discord")]` ID block in `pump()`
    /// were deleted, because deletion is exactly what this pins against.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_id_never_fires_when_joined_without_interval_configured() {
        let c = test_controller();
        let _ = install_simulated_discord_join(&c);
        for _ in 0..10 {
            c.pump();
        }
        assert!(
            c.discord_id_due
                .lock()
                .expect("discord id timer mutex poisoned")
                .is_none(),
            "with no announce_cfg the ID timer must never arm"
        );
    }

    /// An ID interval and a live `id_mode` are configured but nobody has
    /// joined Discord: `discord_member` is `None`, so there is no leg to
    /// speak the ID onto — but the deadline must still arm and advance
    /// (matching `maybe_fire_periodic_id`'s "advance the deadline regardless
    /// of announce outcome" behaviour, so a later Join does not trigger an
    /// immediate catch-up burst). Asserting `discord_id_due` is `Some` after
    /// one pump is the assertion that discriminates a deleted/broken timer
    /// block from a correct one; `discord_member` staying `None` is checked
    /// too, but only as a secondary guard against a regression that pumps
    /// still call `announce_to_bridge_member` without a member to hand it.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_id_arms_when_configured_but_not_joined() {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let announce_cfg = Some(crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: Some(0),
            id_mode: Some("tts".into()),
            cw_wpm: None,
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: None,
        });
        let c = NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            None,
            announce_cfg,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        c.pump();
        assert!(
            c.discord_id_due
                .lock()
                .expect("discord id timer mutex poisoned")
                .is_some(),
            "a configured interval must arm the timer even with nobody joined"
        );
        assert!(
            c.discord_member
                .lock()
                .expect("discord member mutex poisoned")
                .is_none(),
            "pump must never join anyone on its own — discord_member stays None"
        );
    }

    /// `id_mode = "off"` must disable the Discord ID exactly like it disables
    /// the on-air one — they read the same physical config field
    /// (`id_interval_secs`) through the same `id_interval()` gate, and a
    /// second, differently-gated consumer would be a silent trap for whoever
    /// next edits `id_interval()`'s docstring.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_id_respects_id_mode_off() {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let announce_cfg = Some(crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: Some(0),
            id_mode: Some("off".into()),
            cw_wpm: None,
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: None,
        });
        let c = NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            None,
            announce_cfg,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        for _ in 0..10 {
            c.pump();
        }
        assert!(
            c.discord_id_due
                .lock()
                .expect("discord id timer mutex poisoned")
                .is_none(),
            "id_mode = \"off\" must disable the Discord ID timer, same as the on-air one"
        );
    }

    /// The hourly line must not fire the moment the bridge joins, nor on
    /// every 50 ms tick after it: `pump()` arms the deadline on the first
    /// tick and then leaves it alone. Ten pumps inside a microsecond must
    /// therefore leave the channel with nothing but the join's own
    /// "connected" line — the assertion that a missing `now >= deadline`
    /// check, or an unadvanced deadline, would move immediately.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_heartbeat_does_not_fire_before_its_hour() {
        let c = test_controller();
        let (_member, voice) = install_simulated_discord_join(&c);
        assert_eq!(
            voice.posts().len(),
            1,
            "the simulated join posted its connected line and nothing else"
        );
        let (_guard, addr) = crate::test_support::dead_port();
        link_for_heartbeat(&c, &addr);

        let before = std::time::Instant::now();
        for _ in 0..10 {
            c.pump();
        }
        assert_eq!(
            voice.posts().len(),
            1,
            "no heartbeat is due yet, so the channel must hear nothing more"
        );

        let due = c
            .discord_status_due
            .lock()
            .expect("discord status timer mutex poisoned")
            .expect("a joined bridge must arm the heartbeat timer");
        assert!(
            due >= before + DISCORD_STATUS_HEARTBEAT,
            "the timer must be armed a full interval out, not sooner"
        );
    }

    /// And when the hour is actually up it fires — once — and rearms for the
    /// next one. The deadline is wound back by hand because the alternative
    /// is a test that sleeps for an hour.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_heartbeat_fires_when_its_deadline_passes() {
        let c = test_controller();
        let (_member, voice) = install_simulated_discord_join(&c);
        let (_guard, addr) = crate::test_support::dead_port();
        link_for_heartbeat(&c, &addr);
        c.pump();

        *c.discord_status_due
            .lock()
            .expect("discord status timer mutex poisoned") = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .expect("the monotonic clock is more than a second old"),
        );

        let fired_at = std::time::Instant::now();
        c.pump();
        assert_eq!(
            voice.posts().len(),
            2,
            "a due heartbeat must post exactly one line"
        );
        assert_eq!(
            voice.posts()[1],
            "Still linked — node 12345 is bridging AllStar 1999 into this channel. Audio here \
             is received from amateur radio; this bridge does not transmit.",
            "the hourly line names the node, the link, and repeats the disclosure"
        );

        let rearmed = c
            .discord_status_due
            .lock()
            .expect("discord status timer mutex poisoned")
            .expect("firing must rearm, not disarm");
        assert!(
            rearmed >= fired_at + DISCORD_STATUS_HEARTBEAT,
            "the deadline must advance a full interval, not stay in the past"
        );
        for _ in 0..10 {
            c.pump();
        }
        assert_eq!(
            voice.posts().len(),
            2,
            "the rearmed timer must go quiet again until the next hour"
        );
    }

    /// Put node 1999 in the link roster without a network: a connect to a
    /// port nothing answers on sits in the roster as a link that is still
    /// being set up, which is all `link_roster_node_ids` looks at.
    #[cfg(feature = "discord")]
    fn link_for_heartbeat(c: &NodeController, addr: &str) {
        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "1999".into(),
            addr: Some(addr.to_string()),
        })
        .expect("link up");
        assert_eq!(
            c.link_roster_node_ids(),
            vec!["1999".to_string()],
            "the fixture's link is in the roster"
        );
    }

    /// A joined bridge with nothing linked is the idle state of most nodes
    /// most of the day, and it must be silent: no timer is armed, so no
    /// hourly line is ever posted into a channel with nothing behind it.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_timer_stays_disarmed_while_nothing_is_linked() {
        let c = test_controller();
        let (_member, voice) = install_simulated_discord_join(&c);
        for _ in 0..10 {
            c.pump();
        }
        assert!(
            c.discord_status_due
                .lock()
                .expect("discord status timer mutex poisoned")
                .is_none(),
            "a joined bridge with an empty roster must not arm the heartbeat"
        );
        assert_eq!(
            voice.posts().len(),
            1,
            "the connected line is the only thing an idle channel hears"
        );
    }

    /// The hour counts from the link, not from the join. When the roster
    /// empties the timer goes with it, so a deadline that was nearly due
    /// cannot fire minutes after the NEXT link comes up: that link gets a
    /// full interval before its first "still linked".
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_timer_disarms_when_the_last_link_drops_and_rearms_a_full_hour_later() {
        let c = test_controller();
        let (_member, voice) = install_simulated_discord_join(&c);
        let (_guard, addr) = crate::test_support::dead_port();
        link_for_heartbeat(&c, &addr);
        c.pump();
        // Wind the deadline to a second from now, as a link that has been
        // up for most of an hour would have it.
        *c.discord_status_due
            .lock()
            .expect("discord status timer mutex poisoned") =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(1));

        c.execute(NodeCommand::Link {
            action: LinkAction::Disconnect,
            node: "1999".into(),
            addr: None,
        })
        .expect("link down");
        c.pump();
        assert!(
            c.discord_status_due
                .lock()
                .expect("discord status timer mutex poisoned")
                .is_none(),
            "an empty roster must disarm the timer"
        );

        let relinked_at = std::time::Instant::now();
        link_for_heartbeat(&c, &addr);
        c.pump();
        let due = c
            .discord_status_due
            .lock()
            .expect("discord status timer mutex poisoned")
            .expect("a new link re-arms the timer");
        assert!(
            due >= relinked_at + DISCORD_STATUS_HEARTBEAT,
            "the new link gets a full interval, not the old link's leftover deadline"
        );
        assert_eq!(
            voice.posts().len(),
            1,
            "nothing was posted through any of it"
        );
    }

    /// The timer is armed from the presence of a `Bridge`, not from `Join`
    /// and `Leave` remembering to touch it — so a bridge that has gone away
    /// takes its deadline with it on the next tick, and cannot leave a stale
    /// deadline behind to fire into a channel nobody is bridged to.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_timer_disarms_once_nothing_is_joined() {
        let c = test_controller();
        let (_member, _voice) = install_simulated_discord_join(&c);
        let (_guard, addr) = crate::test_support::dead_port();
        link_for_heartbeat(&c, &addr);
        c.pump();
        assert!(
            c.discord_status_due
                .lock()
                .expect("discord status timer mutex poisoned")
                .is_some(),
            "a joined bridge with a link arms the timer"
        );

        c.execute(NodeCommand::Discord {
            action: crate::command::DiscordAction::Leave,
        })
        .expect("leave must succeed");
        c.pump();

        assert!(
            c.discord_status_due
                .lock()
                .expect("discord status timer mutex poisoned")
                .is_none(),
            "with no bridge the timer must disarm, so the next join starts a fresh hour"
        );
    }

    /// The two doors the Discord adapter has onto the M17 state, read the
    /// way the adapter reads them. A leg is "linked" only once the pump
    /// has seen its ACKN edge, and an end reason belongs only to the leg
    /// that ended.
    #[cfg(feature = "discord")]
    #[test]
    fn m17_linked_and_end_reason_show_the_adapter_only_what_it_may_act_on() {
        let c = test_controller();
        assert_eq!(c.m17_linked(), None, "no leg at all");
        c.m17.lock().expect("m17 state mutex poisoned").leg = Some(M17LegRecord {
            reflector: "M17-USA".into(),
            module: 'C',
            callsign: "AJ7HR".into(),
            linked_at: None,
        });
        assert_eq!(c.m17_linked(), None, "a leg still linking is not linked");
        c.m17
            .lock()
            .expect("m17 state mutex poisoned")
            .leg
            .as_mut()
            .expect("set above")
            .linked_at = Some(std::time::Instant::now());
        assert_eq!(c.m17_linked(), Some(("M17-USA".to_string(), 'C')));

        assert_eq!(c.m17_end_reason("M17-USA C"), None, "nothing has ended");
        c.m17.lock().expect("m17 state mutex poisoned").last_ended = Some(M17Ended {
            reflector: "M17-USA".into(),
            module: 'C',
            reason: "reflector sent DISC".into(),
        });
        assert_eq!(
            c.m17_end_reason("M17-USA C"),
            Some("reflector sent DISC".to_string())
        );
        assert_eq!(c.m17_end_reason("M17-USA D"), None, "another module");
        assert_eq!(c.m17_end_reason("M17-EU C"), None, "another reflector");
    }

    /// A node linked ONLY to an M17 reflector is still linked. The hourly
    /// line must arm from the union roster and name the reflector — the
    /// disclosure matters as much for M17 as for `AllStar`.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_status_heartbeat_arms_and_speaks_for_a_roster_holding_only_m17() {
        let refl =
            crate::test_support::RecordingReflector::start(crate::test_support::FakeReply::Ackn);
        let mut c = test_controller();
        c.set_m17_config(Some(crate::m17_directory::M17Settings {
            enabled: true,
            reflectors: std::collections::BTreeMap::from([(
                "M17-TEST".to_string(),
                (refl.addr.ip().to_string(), refl.addr.port()),
            )]),
            directory: None,
        }));
        let (_member, voice) = install_simulated_discord_join(&c);
        c.handle_m17(
            crate::command::M17Action::Monitor,
            Some("M17-TEST"),
            Some('A'),
            Some("AJ7HR"),
        )
        .expect("the monitor dial is accepted");

        assert!(
            crate::test_support::wait_until(
                || {
                    c.pump();
                    c.discord_status_due
                        .lock()
                        .expect("discord status timer mutex poisoned")
                        .is_some()
                },
                5_000
            ),
            "a linked M17 leg alone must arm the heartbeat"
        );
        assert!(
            c.link_roster_node_ids().is_empty(),
            "no AllStar link exists, so the arming came from the M17 leg"
        );

        *c.discord_status_due
            .lock()
            .expect("discord status timer mutex poisoned") = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .expect("the monotonic clock is more than a second old"),
        );
        c.pump();
        assert_eq!(voice.posts().len(), 2, "the join line, then one heartbeat");
        assert_eq!(
            voice.posts()[1],
            "Still linked — node 12345 is bridging M17 M17-TEST A into this channel. Audio here \
             is received from amateur radio; this bridge does not transmit."
        );

        c.teardown_m17();
        refl.stop();
    }

    /// Dial to an unreachable node returns a `NodeError`, not a panic.
    #[test]
    fn dial_error_path_is_node_error_not_panic() {
        let c = test_controller();
        let result = c.execute(NodeCommand::Dial {
            node: "99999".into(),
        });
        // Must be an Err(NodeError) — not a panic and not an Ok.
        assert!(result.is_err(), "dial without portal config must error");
    }

    /// Shutdown sets `should_stop()`.
    #[test]
    fn shutdown_sets_stop_flag() {
        let c = test_controller();
        assert!(!c.should_stop());
        c.execute(NodeCommand::Shutdown).unwrap();
        assert!(c.should_stop());
    }

    /// M17 is off unless the operator adds `[m17]`. A node without one
    /// refuses in one plain sentence and reports itself off.
    #[test]
    fn m17_monitor_on_a_node_without_an_m17_section_is_refused_in_plain_words() {
        let c = test_controller();
        let err = c
            .execute(NodeCommand::M17 {
                action: crate::command::M17Action::Monitor,
                reflector: Some("M17-USA".into()),
                module: Some('C'),
                callsign: Some("AJ7HR".into()),
            })
            .expect_err("no [m17] section, no monitor");
        assert_eq!(
            err.message,
            "M17 isn't enabled on this node — its operator can add an `[m17]` section."
        );
        let snap = c.snapshot();
        assert!(!snap.m17_enabled, "no section, so M17 reads as off");
        assert!(snap.m17.is_none(), "and no leg exists");
    }

    /// `PortalTest` with no `[portal]` section is the station's own
    /// refusal, verbatim: `mint_wt_token` fails on the missing credentials
    /// before any HTTPS is attempted, so this is hermetic. The sentence is
    /// pinned because the page shows it as-is.
    #[test]
    fn portal_test_without_a_portal_is_the_portal_refusal() {
        let c = test_controller();
        let err = c
            .execute(NodeCommand::PortalTest)
            .expect_err("no [portal] section means no token can be minted");
        assert_eq!(
            err.message,
            "portal/token error: no web-transceiver token in the portal response \
             (bad credentials, no WT access for this node, or markup changed)"
        );
    }

    /// A link is also a call in the pool (`connect_link` dials through
    /// `dial_with_identity`, which inserts into `Manager.calls`), so one
    /// link must read "1 link" and never "1 call, 1 link".
    #[test]
    fn busy_on_a_net_is_none_when_idle_and_counts_links() {
        let c = test_controller();
        assert_eq!(c.busy_on_a_net(), None, "an idle node is not on a net");
        let (_guard, addr) = crate::test_support::dead_port();
        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "55553".into(),
            addr: Some(addr),
        })
        .expect("a link to a silent loopback sink is placed");
        assert_eq!(c.busy_on_a_net().as_deref(), Some("1 link"));
        let (_guard2, addr2) = crate::test_support::dead_port();
        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "55554".into(),
            addr: Some(addr2),
        })
        .expect("a second link");
        assert_eq!(c.busy_on_a_net().as_deref(), Some("2 links"));
    }

    /// The sentence itself, over every part. The `call` part has no
    /// hermetic route through the controller — every test's only way onto a
    /// net is a link, and link legs are subtracted from the call count — so
    /// the counts are fed in directly. Exact values, in the fixed order
    /// calls, links, Discord, with only the non-zero parts named.
    #[test]
    fn busy_reason_names_only_what_is_up_in_a_fixed_order() {
        assert_eq!(busy_reason(0, 0, 0, false), None);
        assert_eq!(busy_reason(1, 0, 0, false).as_deref(), Some("1 call"));
        assert_eq!(busy_reason(2, 0, 0, false).as_deref(), Some("2 calls"));
        assert_eq!(busy_reason(0, 2, 0, false).as_deref(), Some("2 links"));
        assert_eq!(
            busy_reason(2, 1, 0, true).as_deref(),
            Some("2 calls, 1 link, Discord joined")
        );
        assert_eq!(
            busy_reason(0, 0, 0, true).as_deref(),
            Some("Discord joined")
        );
    }

    /// The live tier's second door is `SetDevices`; this is what the http
    /// test reads to prove the station, not just the file, took the devices.
    #[test]
    fn selected_devices_for_test_reads_the_station() {
        let c = test_controller();
        assert_eq!(c.selected_devices_for_test(), (None, None));
        c.execute(NodeCommand::SetDevices {
            input: Some("null-in".into()),
            output: Some("null-out".into()),
        })
        .expect("a mutex write, no hardware");
        assert_eq!(
            c.selected_devices_for_test(),
            (Some("null-in".to_string()), Some("null-out".to_string()))
        );
    }

    /// The guard, and the override. A bare `Restart` on a node with a link
    /// up must change nothing — not the stop flag, not the restart flag —
    /// and `disconnect: true` must set both.
    #[test]
    fn restart_is_refused_while_a_link_is_up_unless_disconnect_is_set() {
        let c = test_controller();
        let (_guard, addr) = crate::test_support::dead_port();
        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "55553".into(),
            addr: Some(addr),
        })
        .expect("a link to a silent loopback sink is placed");

        let err = c
            .execute(NodeCommand::Restart { disconnect: false })
            .expect_err("a node on a net does not restart on a bare request");
        assert_eq!(
            err.message,
            "restart refused: this node is on a net (1 link)"
        );
        assert!(!c.should_stop(), "a refusal must not begin a shutdown");
        assert!(!c.restart_requested(), "nor promise a restart");

        c.execute(NodeCommand::Restart { disconnect: true })
            .expect("disconnect: true is the confirmed override");
        assert!(c.should_stop(), "the restart is a shutdown first");
        assert!(c.restart_requested(), "followed by an exec");
    }

    #[test]
    fn restart_when_idle_sets_both_flags() {
        let c = test_controller();
        c.execute(NodeCommand::Restart { disconnect: false })
            .expect("an idle node restarts on request");
        assert!(c.should_stop());
        assert!(c.restart_requested());
    }

    /// A plain `Shutdown` — Ctrl-C, `POST /shutdown`, the TUI — must exit.
    /// If it ever set the restart flag, every Ctrl-C would re-exec the
    /// daemon and there would be no way to stop it.
    #[test]
    fn shutdown_does_not_request_a_restart() {
        let c = test_controller();
        c.execute(NodeCommand::Shutdown).unwrap();
        assert!(c.should_stop());
        assert!(!c.restart_requested());
    }

    #[test]
    fn apply_bridge_config_rewires_the_bridge() {
        let c = test_controller();
        c.apply_bridge_config(astar_iax::BridgeConfig {
            mode: astar_iax::BridgeMode::Conference,
            mix_minus: false,
            include_local_radio: false,
            parrot: None,
        })
        .expect("conference is a valid mode");
        let cfg = c.bridge_config_for_test();
        assert_eq!(cfg.mode, astar_iax::BridgeMode::Conference);
        assert!(
            !cfg.mix_minus,
            "the whole config is applied, not just the mode"
        );
    }

    #[test]
    fn a_fresh_controller_has_no_settings_store() {
        let c = test_controller();
        assert!(
            c.settings_store().is_none(),
            "only main installs one; tests and embedders have no settings page"
        );
    }

    /// A joined Discord bridge is "on a net" even with no call or link:
    /// people in the channel are listening to whatever the node relays.
    #[cfg(feature = "discord")]
    #[test]
    fn a_joined_discord_bridge_counts_as_being_on_a_net() {
        let c = test_controller();
        let (_member, _voice) = install_simulated_discord_join(&c);
        assert_eq!(c.busy_on_a_net().as_deref(), Some("Discord joined"));
        let err = c
            .execute(NodeCommand::Restart { disconnect: false })
            .expect_err("joined is busy");
        assert_eq!(
            err.message,
            "restart refused: this node is on a net (Discord joined)"
        );
        assert!(!c.should_stop());

        // And the parts are joined in a fixed order: calls, links, Discord.
        let (_guard, addr) = crate::test_support::dead_port();
        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "55553".into(),
            addr: Some(addr),
        })
        .expect("a link beside the joined bridge");
        assert_eq!(c.busy_on_a_net().as_deref(), Some("1 link, Discord joined"));
    }

    /// `subscribe()` + `pump()` deliver events to the receiver.
    #[test]
    fn subscribe_and_pump_deliver_events() {
        let c = test_controller();
        let rx = c.subscribe();

        // EnableInbound triggers a ModeChanged event inside the Station.
        c.execute(NodeCommand::EnableInbound).unwrap();

        // pump() drains next_event and sends to subscriber.
        c.pump();

        // We expect at least one Snapshot event (ModeChanged maps to Snapshot).
        let mut got_snapshot = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, NodeEvent::Snapshot(_)) {
                got_snapshot = true;
            }
        }
        assert!(
            got_snapshot,
            "expected at least one Snapshot event after pump"
        );
    }

    /// Register with no config returns `NodeError` (not panic).
    #[test]
    fn register_without_config_is_node_error() {
        let c = test_controller();
        let result = c.execute(NodeCommand::Register);
        assert!(result.is_err(), "Register without register_cfg must error");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("no register config"),
            "unexpected error message: {}",
            err.message
        );
    }

    /// Guard: a `NodeError` produced via the real `execute()` → `StationError`
    /// conversion path carries no credential in its serialised JSON.
    ///
    /// `Register` with no `register_cfg` triggers the earliest possible error
    /// inside `execute()`, without any network I/O, making this deterministic
    /// offline.  The resulting `NodeError` must not leak "secret", "password",
    /// or "token" into JSON even if the error message path ever changes.
    #[test]
    fn execute_error_json_is_credential_free() {
        let c = test_controller();
        // Store a secret so there is something to leak IF the code ever breaks.
        c.execute(NodeCommand::ProvideSecret {
            username: "testuser".into(),
            secret: "hunter2".into(),
        })
        .unwrap();

        // Trigger a real error through execute() → NodeError via the
        // "no register config" path (offline-deterministic, no network).
        let err = c.execute(NodeCommand::Register).unwrap_err();

        let display = err.message.clone();
        let json = serde_json::to_string(&err).unwrap();

        for bad in ["secret", "password", "token", "hunter2"] {
            assert!(
                !display.contains(bad),
                "NodeError message (from execute) contains forbidden word '{bad}': {display}"
            );
            assert!(
                !json.contains(bad),
                "NodeError JSON (from execute) contains forbidden word '{bad}': {json}"
            );
        }
    }

    /// `build_announce_request` maps `text`→`Phrase::Text`, `sample`→`Phrase::Sample`,
    /// destination `"local"`→`LocalMonitor`, `mixunder+gain`→`MixUnder`.
    #[test]
    fn build_announce_request_maps_fields() {
        // text + to_air (default) + seize
        let req = NodeController::build_announce_request(
            Some("hello".into()),
            None,
            Some("to_air"),
            None,
            None,
        )
        .expect("text should produce a request");
        assert!(
            matches!(req.phrase, astar_iax::Phrase::Text(ref t) if t == "hello"),
            "phrase should be Text"
        );
        assert_eq!(req.destination, astar_iax::Destination::ToAir);
        assert!(matches!(req.policy, astar_iax::AnnouncePolicyReq::Seize));

        // sample + local + mixunder with gain
        let req2 = NodeController::build_announce_request(
            None,
            Some("beep".into()),
            Some("local"),
            Some(true),
            Some(-6.0),
        )
        .expect("sample should produce a request");
        assert!(
            matches!(req2.phrase, astar_iax::Phrase::Sample(ref s) if s == "beep"),
            "phrase should be Sample"
        );
        assert_eq!(req2.destination, astar_iax::Destination::LocalMonitor);
        assert!(
            matches!(
                req2.policy,
                astar_iax::AnnouncePolicyReq::MixUnder { gain_db: Some(g) } if (g - -6.0).abs() < 1e-6
            ),
            "policy should be MixUnder with gain"
        );

        // neither text nor sample → None
        let none = NodeController::build_announce_request(None, None, None, None, None);
        assert!(none.is_none(), "no text/sample should return None");
    }

    /// `execute(IdNow)` on an idle (no active call) controller returns `Err(NodeError)`.
    #[test]
    fn id_now_without_active_call_returns_error() {
        let c = test_controller();
        let result = c.execute(NodeCommand::IdNow);
        assert!(
            result.is_err(),
            "IdNow on an idle controller must return Err (station not connected)"
        );
    }

    /// `execute(Announce{..})` with no text or sample returns a descriptive error.
    #[test]
    fn announce_without_text_or_sample_returns_error() {
        let c = test_controller();
        let result = c.execute(NodeCommand::Announce {
            text: None,
            sample: None,
            destination: None,
            mixunder: None,
            gain_db: None,
        });
        assert!(result.is_err(), "Announce with no text/sample must error");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("announce needs text or sample"),
            "unexpected error: {}",
            err.message
        );
    }

    // -------------------------------------------------------------------------
    // Task 4.4: Periodic ID timer + event→announcement table
    // -------------------------------------------------------------------------

    /// Build a controller with an `AnnounceCfg` that enables the periodic ID.
    fn test_controller_with_id_cfg(interval_secs: u64, mode: &str) -> NodeController {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let announce_cfg = Some(crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: Some(interval_secs),
            id_mode: Some(mode.into()),
            cw_wpm: None,
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: None,
        });
        NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            None,
            announce_cfg,
            std::collections::HashMap::new(),
            false,
            3000,
        )
    }

    /// Pure scheduler test: `id_due_and_advance` with interval=0 fires
    /// immediately on first call (deadline == now, so now >= deadline).
    /// This validates the scheduler logic without requiring a live call.
    #[test]
    fn id_due_and_advance_fires_on_zero_interval() {
        let c = test_controller_with_id_cfg(0, "cw");
        let now = std::time::Instant::now();
        let (due, next) = c.id_due_and_advance(now, std::time::Duration::ZERO);
        assert!(due, "zero-interval should be due on first call");
        // Next deadline should be now + 0 = now (may be equal or slightly after).
        assert!(
            next <= now + std::time::Duration::from_millis(1),
            "next deadline should not be far in the future"
        );
    }

    /// Pure scheduler test: `id_due_and_advance` with a large interval does NOT
    /// fire on the first call (deadline is set in the future).
    #[test]
    fn id_due_and_advance_does_not_fire_on_first_call_with_large_interval() {
        let c = test_controller_with_id_cfg(3600, "cw");
        let now = std::time::Instant::now();
        let (due, next) = c.id_due_and_advance(now, std::time::Duration::from_hours(1));
        assert!(!due, "large interval should not be due on first call");
        // Next deadline should be approximately now + 1h.
        assert!(
            next > now,
            "next deadline must be in the future for large interval"
        );
    }

    /// Pure scheduler test: second call to `id_due_and_advance` after advancing
    /// time past the deadline returns due=true.
    #[test]
    fn id_due_and_advance_fires_when_past_deadline() {
        let c = test_controller_with_id_cfg(1, "cw");
        let t0 = std::time::Instant::now();
        let interval = std::time::Duration::from_secs(1);
        // First call: arms the timer.
        let (due0, _) = c.id_due_and_advance(t0, interval);
        assert!(!due0, "should not be due on arm");
        // Second call with t0 + 2s (past deadline): should be due.
        let t1 = t0 + std::time::Duration::from_secs(2);
        let (due1, _) = c.id_due_and_advance(t1, interval);
        assert!(due1, "should be due after interval elapsed");
    }

    /// `pump()` with a zero-interval ID config must not panic on an idle station
    /// (the announce call will fail with `NotConnected`, which is silently ignored).
    /// The existing event-delivery behaviour must be undisturbed.
    #[test]
    fn pump_with_id_cfg_does_not_panic_on_idle() {
        let c = test_controller_with_id_cfg(0, "cw");
        let rx = c.subscribe();
        // EnableInbound triggers a ModeChanged event inside the Station.
        c.execute(NodeCommand::EnableInbound).unwrap();
        // This pump will: call poll_announcements (no-op), attempt to fire the ID
        // (fails silently on idle), then drain + deliver the ModeChanged→Snapshot.
        c.pump();
        let mut got_snapshot = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, NodeEvent::Snapshot(_)) {
                got_snapshot = true;
            }
        }
        assert!(
            got_snapshot,
            "pump with ID config must still deliver Snapshot after ModeChanged"
        );
    }

    /// `AnnouncementStarted{kind:"id"}` is broadcast when the ID fires.
    /// We use `interval_secs=0` so the deadline is immediately due on first pump.
    /// The announce will fail on an idle station (`NotConnected`) — that's the
    /// expected path; the broadcast should NOT happen because the error is silently
    /// swallowed. This test asserts that no announcement event leaks on failure.
    #[test]
    fn pump_with_zero_interval_id_does_not_broadcast_on_idle() {
        let c = test_controller_with_id_cfg(0, "cw");
        let rx = c.subscribe();
        c.pump();
        // The ID fires but the station has no active call → announce() returns
        // Err(NotConnected) → no AnnouncementStarted broadcast.
        let mut saw_announcement = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, NodeEvent::AnnouncementStarted { .. }) {
                saw_announcement = true;
            }
        }
        assert!(
            !saw_announcement,
            "no AnnouncementStarted should be broadcast when announce() fails"
        );
    }

    /// `id_interval()` returns `None` when `id_mode = "off"`.
    #[test]
    fn id_interval_returns_none_when_mode_is_off() {
        let c = test_controller_with_id_cfg(60, "off");
        assert!(
            c.id_interval().is_none(),
            "id_mode=off must disable the ID timer"
        );
    }

    /// `id_interval()` returns `None` when no `announce_cfg` is set.
    #[test]
    fn id_interval_returns_none_when_no_cfg() {
        let c = test_controller(); // no announce_cfg
        assert!(
            c.id_interval().is_none(),
            "missing announce_cfg must disable the ID timer"
        );
    }

    /// `id_interval()` returns `Some(Duration)` when fully configured.
    #[test]
    fn id_interval_returns_duration_when_configured() {
        let c = test_controller_with_id_cfg(600, "cw");
        assert_eq!(
            c.id_interval(),
            Some(std::time::Duration::from_secs(600)),
            "id_interval must reflect configured value"
        );
    }

    /// `maybe_fire_event_announcement` is a no-op when the event is not in the
    /// table (even if the `announce_cfg` is present).
    #[test]
    fn event_table_no_op_for_unmapped_event() {
        use std::collections::HashMap;
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let mut events = HashMap::new();
        // Only "hangup" is enabled; "registered" is absent.
        events.insert(
            "hangup".into(),
            crate::config::EventCfg {
                enabled: true,
                destination: Some("to_air".into()),
            },
        );
        let announce_cfg = Some(crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: None,
            id_mode: None,
            cw_wpm: None,
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: Some(events),
        });
        let ctrl = NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            None,
            announce_cfg,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        let rx = ctrl.subscribe();
        // Fire a "registered" event — not in table → no AnnouncementStarted.
        ctrl.maybe_fire_event_announcement(&astar_station::StationEvent::Registered);
        let mut saw_announcement = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, NodeEvent::AnnouncementStarted { .. }) {
                saw_announcement = true;
            }
        }
        assert!(
            !saw_announcement,
            "unmapped event must not trigger an AnnouncementStarted broadcast"
        );
    }

    /// `maybe_fire_event_announcement` with an enabled entry for the event
    /// attempts `station.announce()`. On an idle station the announce fails
    /// (`NotConnected`), so NO `AnnouncementStarted` is broadcast. This confirms
    /// the table lookup and the announce path work without requiring a live call.
    #[test]
    fn event_table_matched_entry_attempts_announce() {
        use std::collections::HashMap;
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let mut events = HashMap::new();
        events.insert(
            "incoming_call".into(),
            crate::config::EventCfg {
                enabled: true,
                destination: None,
            },
        );
        let announce_cfg = Some(crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: None,
            id_mode: None,
            cw_wpm: None,
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: Some(events),
        });
        let ctrl = NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            None,
            announce_cfg,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        let rx = ctrl.subscribe();
        // Fire matching event; station is idle so announce() → Err; no broadcast.
        ctrl.maybe_fire_event_announcement(&astar_station::StationEvent::IncomingCall {
            from: "55553".into(),
        });
        let mut saw_announcement = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, NodeEvent::AnnouncementStarted { .. }) {
                saw_announcement = true;
            }
        }
        // On an idle station announce fails; AnnouncementStarted should NOT be sent.
        assert!(
            !saw_announcement,
            "no AnnouncementStarted when announce() fails on idle station"
        );
    }

    // -------------------------------------------------------------------------
    // Task 4.5: Startup wiring — config-flow assertions
    // -------------------------------------------------------------------------

    /// `AnnounceCfg::to_service_config` correctly maps CW params and produces a
    /// `ServiceConfig` whose `resolver` carries the configured wpm/tone, and
    /// whose `tts` is the result of `to_tts_config()` (disabled by default when
    /// no `[announce.tts]` sub-section is present).
    #[test]
    fn announce_cfg_to_service_config_carries_cw_and_tts_params() {
        let acfg = crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: Some(-6.0),
            id_interval_secs: None,
            id_mode: Some("cw".into()),
            cw_wpm: Some(15),
            cw_tone_hz: Some(600.0),
            cw_keys_when_idle: Some(false),
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: None,
        };
        let svc = acfg.to_service_config();

        assert_eq!(svc.resolver.cw_wpm, 15, "cw_wpm should be 15");
        assert!(
            (svc.resolver.cw_tone_hz - 600.0).abs() < 1e-3,
            "cw_tone_hz should be 600.0"
        );
        assert!(
            (svc.mixunder_default_gain_db - -6.0).abs() < 1e-3,
            "mixunder_default_gain_db should be -6.0"
        );
        assert!(!svc.cw_keys_when_idle, "cw_keys_when_idle should be false");
        // No [announce.tts] → disabled by default.
        assert!(
            !svc.tts.enabled,
            "tts should be disabled when no [announce.tts] section"
        );
    }

    /// `with_configs` pushes the announce config into the Station so it is
    /// available before any call is made. We verify the push does not panic and
    /// that the controller's `id_interval` still reflects the config.
    #[test]
    fn with_configs_pushes_announce_config_to_station() {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let acfg = Some(crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: Some(600),
            id_mode: Some("cw".into()),
            cw_wpm: Some(20),
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: None,
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: None,
        });
        // Should not panic even though the Manager has not been built yet.
        let ctrl = NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            None,
            acfg,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        // Verify the announce config was stored (id_interval reflects it).
        assert_eq!(
            ctrl.id_interval(),
            Some(std::time::Duration::from_secs(600)),
            "id_interval should reflect the pushed announce config"
        );
    }

    /// `AnnouncementStarted` and `AnnouncementFinished` serialize correctly via serde.
    #[test]
    fn announcement_events_serialize() {
        let started = NodeEvent::AnnouncementStarted { kind: "id".into() };
        let finished = NodeEvent::AnnouncementFinished {
            kind: "event".into(),
        };
        let s_json = serde_json::to_string(&started).unwrap();
        let f_json = serde_json::to_string(&finished).unwrap();
        assert!(
            s_json.contains("\"event\":\"announcement_started\""),
            "wrong tag: {s_json}"
        );
        assert!(s_json.contains("\"kind\":\"id\""), "missing kind: {s_json}");
        assert!(
            f_json.contains("\"event\":\"announcement_finished\""),
            "wrong tag: {f_json}"
        );
        assert!(
            f_json.contains("\"kind\":\"event\""),
            "missing kind: {f_json}"
        );
    }

    // -------------------------------------------------------------------------
    // iax-c4ea: on-join node-id greeting template substitution (TTS stubbed —
    // these assert the RENDERED phrase text only; piper is never invoked).
    // -------------------------------------------------------------------------

    /// Build a controller whose node id is `node_id` and whose `[announce]`
    /// section carries the given optional `join_template`. No TTS is configured.
    fn test_controller_with_join(node_id: &str, join_template: Option<&str>) -> NodeController {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let announce_cfg = Some(crate::config::AnnounceCfg {
            enabled: true,
            sounds_dir: None,
            mixunder_default_gain_db: None,
            id_interval_secs: None,
            id_mode: None,
            cw_wpm: None,
            cw_tone_hz: None,
            cw_keys_when_idle: None,
            join_template: join_template.map(str::to_string),
            link_connect_template: None,
            link_disconnect_template: None,
            m17_connect_template: None,
            m17_disconnect_template: None,
            tts: None,
            events: None,
        });
        let register_cfg = Some(RegisterConfig {
            fallbacks: Vec::new(),
            peer: "127.0.0.1:4569".parse().unwrap(),
            username: node_id.to_string(),
            refresh: std::time::Duration::from_secs(60),
        });
        NodeController::with_configs(
            station,
            secrets,
            InboundConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..InboundConfig::default()
            },
            register_cfg,
            announce_cfg,
            std::collections::HashMap::new(),
            false,
            3000,
        )
    }

    /// Default template + node id `77777` renders the node id digit-by-digit:
    /// `"Connected to node 7 7 7 7 7"` (so TTS reads each digit).
    #[test]
    fn join_greeting_default_template_expands_node_id_to_spaced_digits() {
        let c = test_controller_with_join("77777", None);
        assert_eq!(c.join_greeting_text(), "Connected to node 7 7 7 7 7");
    }

    /// A different node id substitutes its own digits — no hardcoded number.
    #[test]
    fn join_greeting_default_template_uses_configured_node_id() {
        let c = test_controller_with_join("777771", None);
        assert_eq!(c.join_greeting_text(), "Connected to node 7 7 7 7 7 1");
    }

    /// A custom template round-trips and substitutes the `{server-node-number}`
    /// token (also digit-spaced).
    #[test]
    fn join_greeting_custom_template_substitutes_token() {
        let c = test_controller_with_join("42", Some("welcome to {server-node-number} repeater"));
        assert_eq!(c.join_greeting_text(), "welcome to 4 2 repeater");
    }

    /// A template with no token is returned verbatim.
    #[test]
    fn join_greeting_template_without_token_is_verbatim() {
        let c = test_controller_with_join("77777", Some("hello there"));
        assert_eq!(c.join_greeting_text(), "hello there");
    }

    /// The constant default template carries the documented token.
    #[test]
    fn default_join_template_contains_token() {
        assert!(DEFAULT_JOIN_TEMPLATE.contains(JOIN_NODE_TOKEN));
    }

    // --- iax-d9f4 / astar-e7b3: the dongle networks are never remotely
    // keyable ---

    /// A live D-Star session refuses `NodeCommand::Key`. This crate exposes
    /// keying over HTTP (`POST /key`) and a TUI keystroke; D-Star transmit
    /// must stay a local, deliberate act.
    #[test]
    fn keying_is_refused_while_a_dstar_session_is_active() {
        let refusal = key_refusal(true, false, false, false, false)
            .expect("an active D-Star session must refuse the key");
        assert!(
            refusal.message.contains("D-Star"),
            "the refusal must say why, so an operator is not left guessing: {:?}",
            refusal.message
        );
    }

    /// And so does a live YSF link. YSF cannot transmit at all today, so this
    /// is not what stops a transmission — it is what makes the refusal name
    /// its reason, and what will already be in front of a YSF transmit path
    /// on the day one exists.
    #[test]
    fn keying_is_refused_while_a_ysf_link_is_active() {
        let refusal = key_refusal(false, true, false, false, false)
            .expect("an active YSF link must refuse the key");
        assert!(
            refusal.message.contains("System Fusion"),
            "the refusal must name the network: {:?}",
            refusal.message
        );
    }

    /// And so does a live NXDN link. The flag is read off the SNAPSHOT, never
    /// a `#[cfg]`: enabling `nxdn` on astar-sys unifies the feature into this
    /// crate too, so a compile-time guard would be absent exactly when the
    /// network is present.
    #[test]
    fn keying_is_refused_while_an_nxdn_link_is_active() {
        let refusal = key_refusal(false, false, true, false, false)
            .expect("an active NXDN link must refuse the key");
        assert!(
            refusal.message.contains("NXDN"),
            "the refusal must name the network: {:?}",
            refusal.message
        );
        assert!(
            refusal.message.contains("refusing to key"),
            "and say what it refused: {:?}",
            refusal.message
        );
    }

    /// And so does a live DMR link.
    #[test]
    fn key_is_refused_while_a_dmr_link_is_active() {
        // adding-a-network.md §2.8: on the SNAPSHOT flag, never a #[cfg] —
        // enabling `dmr` on astar-sys unifies it into astar-server too.
        let refusal = key_refusal(false, false, false, true, false).expect("a refusal");
        assert!(refusal.message.contains("DMR"));
        assert!(refusal.message.contains("refusing to key"));
    }

    #[test]
    fn key_is_allowed_when_no_digital_session_holds_the_dongle() {
        assert!(key_refusal(false, false, false, false, false).is_none());
    }

    /// Every other network stays remotely keyable — the guard must not have
    /// turned `POST /key` off wholesale.
    #[test]
    fn keying_is_allowed_when_no_digital_voice_session_is_active() {
        assert!(
            key_refusal(false, false, false, false, false).is_none(),
            "IAX2 and the console's own M17 client keying must be unaffected by the dongle-network guard; the node's M17 monitor has no transmit path"
        );
    }
}

#[cfg(test)]
mod m17_tests {
    //! The node's receive-only M17 leg, end to end on loopback. Every
    //! reflector here is `astar_m17::Reflector` or a `RecordingReflector`
    //! on `127.0.0.1:0`, reached only through a `[m17.reflectors]` entry.
    //! Nothing is resolved by name and nothing leaves the machine.

    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::net::{SocketAddr, UdpSocket};
    use std::time::Duration;

    use astar_m17::{ControlPacket, Reflector, StreamPacket, encode_callsign};

    use super::*;
    use crate::m17_directory::Directory;
    use crate::test_support::{FakeReply, RecordingReflector, far_talker_packets, wait_until};

    const NOT_ENABLED: &str =
        "M17 isn't enabled on this node — its operator can add an `[m17]` section.";
    const NO_CODEC: &str = "this node has no Codec 2 decoder — its operator needs to install libcodec2 (or build astar-server with codec2-static).";
    const LINKING: &str = r#"{"event":"m17","kind":"linking","reflector":"M17-TEST","module":"A"}"#;
    const CONNECTED: &str =
        r#"{"event":"m17","kind":"connected","reflector":"M17-TEST","module":"A"}"#;
    const HUNG_UP: &str = r#"{"event":"m17","kind":"disconnected","reflector":"M17-TEST","module":"A","reason":"hung up"}"#;
    const REFUSED: &str = r#"{"event":"m17","kind":"disconnected","reflector":"M17-TEST","module":"A","reason":"reflector refused the link"}"#;
    const SHUTTING_DOWN: &str = r#"{"event":"m17","kind":"disconnected","reflector":"M17-TEST","module":"A","reason":"node shutting down"}"#;

    /// A directory file with one four-module and one one-module reflector.
    /// Its hosts are never contacted: every use of it is either refused
    /// before a socket exists, or resolves `M17-TEST` from the table first.
    const DIRECTORY_JSON: &str = r#"{"generated":"2026-09-15","license":"CC BY 4.0","license_url":"https://creativecommons.org/licenses/by/4.0/","attribution":"test rows","source":"test","reflectors":[{"id":"M17-DIR","host":"127.0.0.1","port":40404,"modules":["A","B","C","D"]},{"id":"M17-ONE","host":"127.0.0.1","port":40404,"modules":["C"]}]}"#;

    /// `[m17] enabled = true` with `[m17.reflectors] "M17-TEST" = "<addr>"`.
    fn table_settings(addr: SocketAddr) -> M17Settings {
        let mut reflectors = BTreeMap::new();
        reflectors.insert("M17-TEST".to_string(), (addr.ip().to_string(), addr.port()));
        M17Settings {
            enabled: true,
            reflectors,
            directory: None,
        }
    }

    /// `settings` plus [`DIRECTORY_JSON`] as its directory.
    fn with_directory(mut settings: M17Settings) -> M17Settings {
        settings.directory =
            Some(Directory::from_json(DIRECTORY_JSON).expect("a well-formed directory"));
        settings
    }

    /// Settings no socket is ever opened for (pure pre-flight tests).
    fn pure_settings() -> M17Settings {
        with_directory(table_settings(
            "127.0.0.1:40404".parse().expect("a loopback literal"),
        ))
    }

    fn live_leg() -> M17LegRecord {
        M17LegRecord {
            reflector: "M17-TEST".into(),
            module: 'A',
            callsign: "AJ7HR".into(),
            linked_at: None,
        }
    }

    /// The refusal text for one pre-flight input.
    fn refusal(
        settings: Option<&M17Settings>,
        reflector: Option<&str>,
        module: Option<char>,
        callsign: Option<&str>,
        codec: bool,
        live: Option<&M17LegRecord>,
    ) -> String {
        m17_preflight(settings, reflector, module, callsign, || codec, live)
            .expect_err("this request must be refused")
            .message
    }

    /// Every refusal is its exact sentence, and where two apply the earlier
    /// check wins: enabled → presence → shape → directory → module offered
    /// → decoder → one leg.
    #[test]
    fn preflight_refusals_are_exact_and_in_order() {
        let s = pure_settings();
        let off = M17Settings {
            enabled: false,
            ..pure_settings()
        };
        let ok = (Some("M17-TEST"), Some('A'), Some("AJ7HR"));

        assert_eq!(refusal(None, ok.0, ok.1, ok.2, true, None), NOT_ENABLED);
        assert_eq!(
            refusal(Some(&off), ok.0, ok.1, ok.2, true, None),
            NOT_ENABLED
        );
        assert_eq!(
            refusal(Some(&off), None, None, None, false, None),
            NOT_ENABLED,
            "off wins over everything"
        );
        assert_eq!(
            refusal(Some(&s), ok.0, ok.1, None, true, None),
            "An M17 monitor needs a reflector, a module and a callsign."
        );
        assert_eq!(
            refusal(Some(&s), ok.0, ok.1, None, false, None),
            "An M17 monitor needs a reflector, a module and a callsign.",
            "presence is checked before the decoder"
        );
        assert_eq!(
            refusal(Some(&s), None, ok.1, ok.2, true, None),
            "An M17 monitor needs a reflector, a module and a callsign."
        );
        assert_eq!(
            refusal(Some(&s), Some("M17!"), ok.1, ok.2, true, None),
            "A reflector name is letters, digits and '-' only."
        );
        assert_eq!(
            refusal(Some(&s), Some("AB"), ok.1, ok.2, true, None),
            "A reflector name is 3 to 8 characters."
        );
        assert_eq!(
            refusal(Some(&s), Some("AB"), ok.1, ok.2, false, None),
            "A reflector name is 3 to 8 characters.",
            "shape is checked before the decoder"
        );
        assert_eq!(
            refusal(Some(&s), ok.0, Some('1'), ok.2, true, None),
            "A module is a single letter, A to Z."
        );
        assert_eq!(
            refusal(Some(&s), ok.0, ok.1, Some("AJ7 HR"), true, None),
            "\"AJ7 HR\" isn't an M17 callsign — use 1 to 9 letters, digits, '-', '/' or '.'."
        );
        assert_eq!(
            refusal(Some(&s), Some("M17-XYZ"), ok.1, Some("AJ7 HR"), true, None),
            "\"AJ7 HR\" isn't an M17 callsign — use 1 to 9 letters, digits, '-', '/' or '.'.",
            "shape is checked before the directory"
        );
        assert_eq!(
            refusal(Some(&s), Some("m17-xyz"), ok.1, ok.2, true, None),
            "I don't know a reflector called M17-XYZ. The node's operator can add it under `[m17.reflectors]`."
        );
        assert_eq!(
            refusal(Some(&s), Some("m17-xyz"), ok.1, ok.2, false, None),
            "I don't know a reflector called M17-XYZ. The node's operator can add it under `[m17.reflectors]`.",
            "the directory is checked before the decoder"
        );
        assert_eq!(
            refusal(Some(&s), Some("m17-dir"), Some('e'), ok.2, true, None),
            "M17-DIR has modules A–D."
        );
        assert_eq!(
            refusal(Some(&s), Some("M17-DIR"), Some('E'), ok.2, false, None),
            "M17-DIR has modules A–D.",
            "the module offer is checked before the decoder"
        );
        assert_eq!(
            refusal(Some(&s), Some("M17-ONE"), Some('A'), ok.2, true, None),
            "M17-ONE has module C."
        );
        assert_eq!(refusal(Some(&s), ok.0, ok.1, ok.2, false, None), NO_CODEC);
        assert_eq!(
            refusal(Some(&s), ok.0, ok.1, ok.2, false, Some(&live_leg())),
            NO_CODEC,
            "the decoder is checked before the one-leg rule"
        );
        assert_eq!(
            refusal(Some(&s), ok.0, Some('B'), ok.2, true, Some(&live_leg())),
            "Already listening to M17 M17-TEST A. `/hangup` first."
        );
    }

    /// Names, modules and callsigns are uppercased before anything uses
    /// them; the table wins over the directory and carries no module list.
    #[test]
    fn preflight_uppercases_and_resolves_from_the_table() {
        let s = pure_settings();
        let t = m17_preflight(
            Some(&s),
            Some("m17-test"),
            Some('a'),
            Some("aj7hr"),
            || true,
            None,
        )
        .expect("a table entry passes");
        assert_eq!(t.reflector, "M17-TEST");
        assert_eq!(t.module, 'A');
        assert_eq!(t.callsign, "AJ7HR");
        assert_eq!(
            t.entry,
            ReflectorEntry {
                host: "127.0.0.1".into(),
                port: 40404,
                modules: None,
            }
        );
        let d = m17_preflight(
            Some(&s),
            Some("M17-DIR"),
            Some('c'),
            Some("AJ7HR"),
            || true,
            None,
        )
        .expect("an offered module on a directory entry passes");
        assert_eq!(d.entry.modules, Some(vec!['A', 'B', 'C', 'D']));
    }

    /// The decoder probe loads a library. It must not run for a request the
    /// directory has already refused.
    #[test]
    fn preflight_probes_the_codec_only_after_the_directory_answers() {
        let s = pure_settings();
        let probed = Cell::new(false);
        let err = m17_preflight(
            Some(&s),
            Some("M17-XYZ"),
            Some('A'),
            Some("AJ7HR"),
            || {
                probed.set(true);
                true
            },
            None,
        )
        .expect_err("unknown reflector");
        assert_eq!(
            err.message,
            "I don't know a reflector called M17-XYZ. The node's operator can add it under `[m17.reflectors]`."
        );
        assert!(!probed.get(), "no decoder probe for a refused name");
    }

    /// A `NullBackend` node in Bridge mode with its inbound listener on
    /// `127.0.0.1:0` (the topology a monitor joins) and `[m17]` from
    /// `settings`.
    fn m17_node(
        config: astar_station::StationConfig,
        settings: Option<M17Settings>,
    ) -> NodeController {
        let station = Station::with_backend_factory(
            config,
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let mut c = NodeController::new(station, SecretProvider::new());
        c.set_m17_config(settings);
        c.execute(NodeCommand::EnableInbound)
            .expect("inbound on 127.0.0.1:0 builds the engine");
        c.execute(NodeCommand::SetBridge {
            mode: Some("bridge".into()),
            mix_minus: Some(true),
            include_local_radio: Some(false),
        })
        .expect("bridge mode is valid");
        c
    }

    fn monitor(
        c: &NodeController,
        reflector: &str,
        module: char,
        callsign: &str,
    ) -> Result<NodeReply, NodeError> {
        c.execute(NodeCommand::M17 {
            action: M17Action::Monitor,
            reflector: Some(reflector.into()),
            module: Some(module),
            callsign: Some(callsign.into()),
        })
    }

    fn hang_up(c: &NodeController) -> Result<NodeReply, NodeError> {
        c.execute(NodeCommand::M17 {
            action: M17Action::Disconnect,
            reflector: None,
            module: None,
            callsign: None,
        })
    }

    /// ACKN arrived AND the pump has stamped it.
    fn linked(c: &NodeController) -> bool {
        c.snapshot()
            .m17
            .is_some_and(|m| m.state == "linked" && m.up_secs.is_some())
    }

    fn pump_until(
        c: &NodeController,
        pred: impl Fn(&NodeController) -> bool,
        timeout_ms: u64,
    ) -> bool {
        wait_until(
            || {
                c.pump();
                pred(c)
            },
            timeout_ms,
        )
    }

    /// The M17 events among `events`, as the SSE stream would carry them.
    fn m17_json(events: &[NodeEvent]) -> Vec<String> {
        events
            .iter()
            .filter(|e| matches!(e, NodeEvent::M17 { .. }))
            .map(|e| serde_json::to_string(e).expect("events serialize"))
            .collect()
    }

    /// The load-bearing check: everything the node sent toward M17 is CONN,
    /// PONG or DISC, and nothing parses as a stream packet.
    fn assert_only_control(sent: &[Vec<u8>]) {
        assert!(!sent.is_empty(), "the recorder saw nothing at all");
        for d in sent {
            assert!(
                StreamPacket::parse(d).is_none(),
                "a stream packet left toward M17: {d:?}"
            );
            assert!(
                matches!(
                    ControlPacket::parse(d),
                    Some(
                        ControlPacket::Conn { .. }
                            | ControlPacket::Pong { .. }
                            | ControlPacket::Disc { .. }
                    )
                ),
                "only CONN, PONG and DISC may be sent toward M17: {d:?}"
            );
        }
    }

    fn cs(callsign: &str) -> [u8; 6] {
        encode_callsign(callsign).expect("a valid base-40 callsign")
    }

    #[test]
    fn monitor_links_and_the_event_stream_sees_linking_then_connected() {
        let reflector = Reflector::bind("127.0.0.1:0".parse().expect("loopback"))
            .expect("bind the in-process reflector");
        let addr = reflector.local_addr();
        let handle = reflector.run();
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(addr)),
        );
        let rx = c.subscribe();
        let before = c.conference_member_count_for_test();

        let reply = monitor(&c, "m17-test", 'a', "aj7hr")
            .expect("the pre-flight passes and the station spawns the monitor");
        assert!(matches!(reply, NodeReply::Ok), "{reply:?}");
        assert_eq!(
            c.conference_member_count_for_test(),
            before + 1,
            "the monitor joins the conference as one contributing member"
        );
        assert!(
            pump_until(&c, linked, 3_000),
            "the in-process reflector ACKNs: {:?}",
            c.snapshot().m17
        );

        let m = c.snapshot().m17.expect("a leg is up");
        assert_eq!(
            m,
            M17LinkStatus {
                reflector: "M17-TEST".into(),
                module: "A".into(),
                target: "M17-TEST A".into(),
                callsign: "AJ7HR".into(),
                state: "linked".into(),
                receiving: false,
                talker: None,
                talker_secs_ago: None,
                up_secs: m.up_secs,
                dropped_packets: 0,
                directory_generated: None,
            }
        );
        assert!(m.up_secs.is_some(), "up_secs starts counting at ACKN");

        let events: Vec<NodeEvent> = rx.try_iter().collect();
        assert_eq!(
            m17_json(&events),
            vec![LINKING.to_string(), CONNECTED.to_string()]
        );
        for (i, ev) in events.iter().enumerate() {
            if matches!(ev, NodeEvent::M17 { .. }) {
                assert!(
                    matches!(events.get(i + 1), Some(NodeEvent::Snapshot(_))),
                    "each M17 edge is followed by a Snapshot, got {:?}",
                    events.get(i + 1)
                );
            }
        }
        hang_up(&c).expect("hang up");
        handle.shutdown();
    }

    #[test]
    fn a_16k_bridge_member_hears_the_far_talker_in_320_sample_frames() {
        let reflector = Reflector::bind("127.0.0.1:0".parse().expect("loopback"))
            .expect("bind the in-process reflector");
        let addr = reflector.local_addr();
        let handle = reflector.run();
        let slin16 = astar_station::StationConfig {
            codec_policy: astar_station::CodecPolicy::PreferSlin16,
            ..astar_station::StationConfig::default()
        };
        let c = m17_node(slin16, Some(table_settings(addr)));
        let listener = c
            .station_for_test()
            .add_bridge_member()
            .expect("bridge mode enrols a listener");
        assert_eq!(listener.sample_rate, 16_000, "PreferSlin16 pins 16 kHz");

        monitor(&c, "M17-TEST", 'A', "AJ7HR").expect("monitor");
        assert!(pump_until(&c, linked, 3_000), "{:?}", c.snapshot().m17);

        let far = UdpSocket::bind("127.0.0.1:0").expect("bind the far talker");
        far.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set the far talker's read timeout");
        let conn = ControlPacket::Conn {
            callsign: cs("KC1ABC"),
            module: b'A',
        }
        .to_bytes();
        far.send_to(&conn, addr)
            .expect("CONN to the loopback reflector");
        let mut buf = [0u8; 64];
        assert!(
            wait_until(
                || far
                    .recv_from(&mut buf)
                    .is_ok_and(|(n, _)| buf[..n] == *b"ACKN"),
                2_000
            ),
            "the far talker is linked to module A"
        );
        for pkt in far_talker_packets("KC1ABC", 10) {
            far.send_to(&pkt, addr)
                .expect("stream packet to the loopback reflector");
            // A radio's pace (one 40 ms packet every 40 ms), inside the
            // monitor's flood gate. Pacing a sender, not waiting on a result.
            std::thread::sleep(Duration::from_millis(40));
        }

        let mut frames: Vec<Vec<i16>> = Vec::new();
        let heard = wait_until(
            || {
                c.pump();
                frames.extend(listener.from_conference.try_iter());
                frames
                    .iter()
                    .any(|f| f.iter().any(|s| s.unsigned_abs() > 1_000))
            },
            3_000,
        );
        assert!(
            heard,
            "the far talker's tone reaches the bridge member ({} frames, all quiet)",
            frames.len()
        );
        assert!(
            frames.iter().all(|f| f.len() == 320),
            "every frame is 20 ms at 16 kHz: {:?}",
            frames.iter().map(Vec::len).collect::<Vec<_>>()
        );
        assert!(
            pump_until(
                &c,
                |c: &NodeController| c
                    .snapshot()
                    .m17
                    .is_some_and(|m| m.talker.as_deref() == Some("KC1ABC")),
                2_000
            ),
            "the snapshot names the far talker: {:?}",
            c.snapshot().m17
        );

        hang_up(&c).expect("hang up");
        c.station_for_test().remove_bridge_member(listener.id);
        handle.shutdown();
    }

    /// `POST /key` cannot reach the monitor: with only a monitor up there is
    /// no call to key, and the recorder proves nothing but control left.
    #[test]
    fn key_with_only_a_monitor_up_is_no_active_call_and_only_control_leaves() {
        let rec = RecordingReflector::start(FakeReply::Ackn);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(rec.addr)),
        );
        monitor(&c, "m17-test", 'a', "aj7hr").expect("monitor");
        assert!(pump_until(&c, linked, 3_000), "{:?}", c.snapshot().m17);
        assert!(
            wait_until(
                || rec
                    .datagrams()
                    .iter()
                    .any(|d| matches!(ControlPacket::parse(d), Some(ControlPacket::Pong { .. }))),
                2_000
            ),
            "the monitor answers the recorder's PINGs"
        );

        let err = c
            .execute(NodeCommand::Key)
            .expect_err("nothing keyable is up");
        assert_eq!(err.message, "no active call");

        hang_up(&c).expect("hang up");
        let disc = ControlPacket::Disc {
            callsign: Some(cs("AJ7HR")),
        };
        assert!(
            wait_until(
                || rec.datagrams().last().and_then(|d| ControlPacket::parse(d)) == Some(disc),
                2_000
            ),
            "hang-up ends with DISC: {:?}",
            rec.datagrams()
        );
        let sent = rec.datagrams();
        assert_only_control(&sent);
        assert_eq!(
            ControlPacket::parse(&sent[0]),
            Some(ControlPacket::Conn {
                callsign: cs("AJ7HR"),
                module: b'A',
            }),
            "CONN carries the uppercased callsign and module"
        );
        rec.stop();
    }

    #[test]
    fn disconnect_withdraws_the_member_and_reports_hung_up() {
        let reflector = Reflector::bind("127.0.0.1:0".parse().expect("loopback"))
            .expect("bind the in-process reflector");
        let addr = reflector.local_addr();
        let handle = reflector.run();
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(addr)),
        );
        let before = c.conference_member_count_for_test();
        monitor(&c, "M17-TEST", 'A', "AJ7HR").expect("monitor");
        assert!(pump_until(&c, linked, 3_000), "{:?}", c.snapshot().m17);
        assert!(
            wait_until(|| handle.client_count() == 1, 2_000),
            "the reflector lists the monitor"
        );

        let rx = c.subscribe();
        hang_up(&c).expect("a leg is up");
        assert_eq!(
            c.conference_member_count_for_test(),
            before,
            "the source member is withdrawn"
        );
        assert!(c.snapshot().m17.is_none(), "no leg after a hang-up");
        assert!(
            c.station_for_test().m17_monitor_status().is_none(),
            "and no monitor"
        );
        let events: Vec<NodeEvent> = rx.try_iter().collect();
        assert_eq!(m17_json(&events), vec![HUNG_UP.to_string()]);
        assert!(
            wait_until(|| handle.client_count() == 0, 2_000),
            "the reflector got DISC and dropped the client"
        );
        assert_eq!(
            hang_up(&c).expect_err("nothing is up now").message,
            "No M17 link is up."
        );
        handle.shutdown();
    }

    /// No reconnect: a refused leg is torn down once, with its reason, and
    /// the reflector never hears the callsign again.
    #[test]
    fn a_nack_tears_the_leg_down_with_its_reason() {
        let rec = RecordingReflector::start(FakeReply::Nack);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(rec.addr)),
        );
        let rx = c.subscribe();
        let before = c.conference_member_count_for_test();
        monitor(&c, "M17-TEST", 'A', "AJ7HR")
            .expect("the pre-flight cannot know the reflector will refuse");
        assert!(
            pump_until(&c, |c: &NodeController| c.snapshot().m17.is_none(), 3_000),
            "a NACK tears the leg down: {:?}",
            c.snapshot().m17
        );
        assert_eq!(c.conference_member_count_for_test(), before);
        assert_eq!(
            c.m17.lock().expect("m17 state mutex poisoned").last_ended,
            Some(M17Ended {
                reflector: "M17-TEST".into(),
                module: 'A',
                reason: "reflector refused the link".into(),
            })
        );
        let events: Vec<NodeEvent> = rx.try_iter().collect();
        assert_eq!(
            m17_json(&events),
            vec![LINKING.to_string(), REFUSED.to_string()]
        );
        let conns = || {
            rec.datagrams()
                .iter()
                .filter(|d| matches!(ControlPacket::parse(d), Some(ControlPacket::Conn { .. })))
                .count()
        };
        // Pumped, and longer than the FSM's 1 s CONN resend interval, so a
        // re-dial added anywhere in the pump would have time to send.
        assert!(
            !pump_until(&c, |_c: &NodeController| conns() > 1, 1_500),
            "no reconnect: exactly one CONN, got {}",
            conns()
        );
        assert!(c.snapshot().m17.is_none(), "and no new leg");
        assert_only_control(&rec.datagrams());
        rec.stop();
    }

    /// `drain_m17` only `try_lock`s `m17_op`: while a `handle_m17` body holds
    /// it, an ended leg's edge stays queued in the session, so no
    /// `disconnected` can be broadcast ahead of that body's own `linking`.
    #[test]
    fn the_pump_leaves_an_edge_queued_while_an_m17_body_runs() {
        let rec = RecordingReflector::start(FakeReply::Nack);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(rec.addr)),
        );
        monitor(&c, "M17-TEST", 'A', "AJ7HR").expect("the pre-flight passes");
        let held = c.m17_op.lock().expect("m17 op mutex poisoned");
        assert!(
            wait_until(
                || c.station_for_test()
                    .m17_monitor_status()
                    .is_some_and(|s| s.link == "failed"),
                3_000
            ),
            "the NACK failed the monitor"
        );
        for _ in 0..5 {
            c.pump();
        }
        assert!(
            c.snapshot().m17.is_some(),
            "no reap while an M17 body holds m17_op"
        );
        drop(held);
        assert!(
            pump_until(&c, |c: &NodeController| c.snapshot().m17.is_none(), 3_000),
            "the queued Ended edge is reported once m17_op is free"
        );
        rec.stop();
    }

    /// The control port takes the callsign as typed: lowercase and edge
    /// whitespace reach the wire as AJ7HR; a space inside or a non-ASCII
    /// letter is refused before any socket opens.
    #[test]
    fn a_control_port_callsign_is_normalised_or_refused_before_the_wire() {
        let rec = RecordingReflector::start(FakeReply::Ackn);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(rec.addr)),
        );
        c.handle_m17(
            M17Action::Monitor,
            Some("M17-TEST"),
            Some('a'),
            Some(" aj7hr"),
        )
        .expect("lowercase and edge whitespace are accepted");
        assert!(
            wait_until(|| !rec.datagrams().is_empty(), 2_000),
            "the monitor sent CONN"
        );
        assert_eq!(
            rec.datagrams()[0],
            ControlPacket::Conn {
                callsign: cs("AJ7HR"),
                module: b'A',
            }
            .to_bytes()
        );
        c.handle_m17(M17Action::Disconnect, None, None, None)
            .expect("hang up");
        let disc = ControlPacket::Disc {
            callsign: Some(cs("AJ7HR")),
        };
        assert!(
            wait_until(
                || rec.datagrams().last().and_then(|d| ControlPacket::parse(d)) == Some(disc),
                2_000
            ),
            "the hang-up sent DISC"
        );

        let seen = rec.datagrams().len();
        let members = c.conference_member_count_for_test();
        for typed in ["AJ7 HR", "ÄJ7HR"] {
            let err = c
                .handle_m17(M17Action::Monitor, Some("M17-TEST"), Some('A'), Some(typed))
                .expect_err("refused before any socket opens");
            assert_eq!(
                err.message,
                format!(
                    "\"{typed}\" isn't an M17 callsign — use 1 to 9 letters, digits, '-', '/' or '.'."
                )
            );
        }
        assert!(
            !wait_until(|| rec.datagrams().len() > seen, 300),
            "a refused callsign sends nothing"
        );
        assert_eq!(c.conference_member_count_for_test(), members);
        assert!(c.snapshot().m17.is_none());
        assert_only_control(&rec.datagrams());
        rec.stop();
    }

    #[test]
    fn refusals_through_execute_leave_no_leg_and_no_member() {
        let off = m17_node(astar_station::StationConfig::default(), None);
        assert_eq!(
            monitor(&off, "M17-TEST", 'A', "AJ7HR")
                .expect_err("M17 is off")
                .message,
            NOT_ENABLED
        );

        let rec = RecordingReflector::start(FakeReply::Ackn);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(rec.addr)),
        );
        let before = c.conference_member_count_for_test();

        assert_eq!(
            monitor(&c, "m17-xyz", 'A', "AJ7HR")
                .expect_err("unknown name")
                .message,
            "I don't know a reflector called M17-XYZ. The node's operator can add it under `[m17.reflectors]`."
        );
        assert_eq!(
            c.conference_member_count_for_test(),
            before,
            "no member enrolled"
        );
        assert!(
            c.station_for_test().m17_monitor_status().is_none(),
            "no monitor spawned, so no socket opened"
        );
        assert!(c.snapshot().m17.is_none());

        let err = c
            .execute(NodeCommand::M17 {
                action: M17Action::Monitor,
                reflector: Some("M17-TEST".into()),
                module: Some('A'),
                callsign: None,
            })
            .expect_err("no callsign");
        assert_eq!(
            err.message,
            "An M17 monitor needs a reflector, a module and a callsign."
        );

        monitor(&c, "M17-TEST", 'A', "AJ7HR").expect("first monitor");
        assert!(pump_until(&c, linked, 3_000), "{:?}", c.snapshot().m17);
        let with_leg = c.conference_member_count_for_test();
        assert_eq!(with_leg, before + 1);
        assert_eq!(
            monitor(&c, "M17-TEST", 'B', "KC1ABC")
                .expect_err("one leg per node")
                .message,
            "Already listening to M17 M17-TEST A. `/hangup` first."
        );
        assert_eq!(
            c.conference_member_count_for_test(),
            with_leg,
            "the refused second Monitor enrolled nothing"
        );
        hang_up(&c).expect("hang up");

        c.execute(NodeCommand::SetBridge {
            mode: Some("handset".into()),
            mix_minus: None,
            include_local_radio: None,
        })
        .expect("handset mode is valid");
        assert_eq!(
            monitor(&c, "M17-TEST", 'A', "AJ7HR")
                .expect_err("handset has no conference to join")
                .message,
            "m17 error: no conference: bridge topology is handset/parrot"
        );
        assert!(
            c.snapshot().m17.is_none(),
            "a Monitor the station refused leaves no leg behind"
        );
        assert!(
            c.m17
                .lock()
                .expect("m17 state mutex poisoned")
                .leg
                .is_none(),
            "and no record"
        );
        rec.stop();
    }

    /// `NodeCommand::Shutdown` DISCs even a leg that is still linking, so
    /// the reflector never keeps a ghost client carrying someone's callsign.
    #[test]
    fn shutdown_sends_disc_even_to_a_reflector_that_never_answered() {
        let rec = RecordingReflector::start(FakeReply::Silent);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(rec.addr)),
        );
        let rx = c.subscribe();
        monitor(&c, "M17-TEST", 'A', "AJ7HR").expect("monitor");
        let conn = ControlPacket::Conn {
            callsign: cs("AJ7HR"),
            module: b'A',
        };
        assert!(
            wait_until(
                || rec
                    .datagrams()
                    .iter()
                    .any(|d| ControlPacket::parse(d) == Some(conn)),
                2_000
            ),
            "the monitor thread sent its CONN"
        );

        let started = std::time::Instant::now();
        c.execute(NodeCommand::Shutdown).expect("shutdown");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "Shutdown must not block on a leg that is still linking: took {:?}",
            started.elapsed()
        );
        assert!(c.should_stop());
        let disc = ControlPacket::Disc {
            callsign: Some(cs("AJ7HR")),
        };
        assert!(
            wait_until(
                || rec.datagrams().last().and_then(|d| ControlPacket::parse(d)) == Some(disc),
                2_000
            ),
            "Shutdown DISCs a leg that is still linking: {:?}",
            rec.datagrams()
        );
        assert!(c.snapshot().m17.is_none());
        assert!(c.station_for_test().m17_monitor_status().is_none());

        c.teardown_m17();
        let events: Vec<NodeEvent> = rx.try_iter().collect();
        assert_eq!(
            m17_json(&events),
            vec![LINKING.to_string(), SHUTTING_DOWN.to_string()],
            "a second teardown is a no-op"
        );
        assert_only_control(&rec.datagrams());
        rec.stop();
    }

    #[test]
    fn status_json_carries_the_leg_and_no_secret_words() {
        let rec = RecordingReflector::start(FakeReply::Ackn);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(with_directory(table_settings(rec.addr))),
        );
        monitor(&c, "M17-TEST", 'A', "AJ7HR").expect("monitor");
        assert!(pump_until(&c, linked, 3_000), "{:?}", c.snapshot().m17);

        let r = crate::http::handle_request(&c, "GET", "/status", b"");
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).expect("UTF-8 JSON");
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(v["m17"]["target"], "M17-TEST A");
        assert_eq!(v["m17"]["callsign"], "AJ7HR");
        assert_eq!(v["m17"]["state"], "linked");
        assert_eq!(v["m17"]["directory_generated"], "2026-09-15");
        assert_eq!(v["m17_enabled"], true);
        assert_eq!(v["m17_available"], true);
        let lower = body.to_lowercase();
        for bad in ["secret", "password", "token"] {
            assert!(
                !lower.contains(bad),
                "/status must not contain {bad:?}: {body}"
            );
        }
        hang_up(&c).expect("hang up");
        rec.stop();
    }

    #[test]
    fn m17_enabled_follows_the_section_and_available_follows_the_decoder() {
        let none = m17_node(astar_station::StationConfig::default(), None);
        assert!(!none.snapshot().m17_enabled, "no section");
        let off = m17_node(
            astar_station::StationConfig::default(),
            Some(M17Settings {
                enabled: false,
                reflectors: BTreeMap::new(),
                directory: None,
            }),
        );
        assert!(!off.snapshot().m17_enabled, "enabled = false");
        let on = m17_node(
            astar_station::StationConfig::default(),
            Some(M17Settings {
                enabled: true,
                reflectors: BTreeMap::new(),
                directory: None,
            }),
        );
        let snap = on.snapshot();
        assert!(snap.m17_enabled, "an enabled section");
        assert!(
            snap.m17_available,
            "test builds link codec2-static through the dev-dependency"
        );
    }

    // -- M17 monitor announcements (Task 13) --------------------------------

    /// Every `AnnouncementStarted` kind among `events`, in order.
    fn announcement_kinds(events: &[NodeEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|ev| match ev {
                NodeEvent::AnnouncementStarted { kind } => Some(kind.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every M17 announcement `fire_m17_announcement` was asked to speak.
    fn m17_announce_log(c: &NodeController) -> Vec<(bool, String)> {
        c.m17_announce_log
            .lock()
            .expect("m17 announce log mutex poisoned")
            .clone()
    }

    #[test]
    fn m17_announce_gate_monitor_and_hangup_with_no_members() {
        let refl = RecordingReflector::start(FakeReply::Ackn);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(refl.addr)),
        );
        let rx = c.subscribe();

        monitor(&c, "m17-test", 'a', "aj7hr").expect("monitor starts");
        assert!(
            pump_until(&c, linked, 5_000),
            "the monitor links to the recording reflector: {:?}",
            c.snapshot().m17
        );
        hang_up(&c).expect("disconnect with a leg up");

        let events: Vec<NodeEvent> = rx.try_iter().collect();
        assert_eq!(
            announcement_kinds(&events),
            Vec::<String>::new(),
            "announce_to_non_link_members reached nobody, so no AnnouncementStarted"
        );
        assert_eq!(
            m17_announce_log(&c),
            vec![
                (
                    true,
                    "Monitoring M17 reflector M 1 7 - T E S T module A".to_string()
                ),
                (
                    false,
                    "Stopped monitoring M17 reflector M 1 7 - T E S T module A".to_string()
                ),
            ],
            "Monitor announced the uppercased leg once, then Disconnect announced it once"
        );
        assert_only_control(&refl.datagrams());
        refl.stop();
    }

    #[test]
    fn m17_announce_gate_refused_link_with_no_members() {
        let refl = RecordingReflector::start(FakeReply::Nack);
        let c = m17_node(
            astar_station::StationConfig::default(),
            Some(table_settings(refl.addr)),
        );
        let rx = c.subscribe();

        monitor(&c, "m17-test", 'a', "aj7hr").expect("monitor starts; the NACK arrives later");
        assert!(
            pump_until(&c, |c: &NodeController| c.snapshot().m17.is_none(), 5_000),
            "drain_m17 reaps the refused leg: {:?}",
            c.snapshot().m17
        );

        let events: Vec<NodeEvent> = rx.try_iter().collect();
        let ended: Vec<Option<String>> = events
            .iter()
            .filter_map(|ev| match ev {
                NodeEvent::M17 { kind, reason, .. } if kind == "disconnected" => {
                    Some(reason.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            ended,
            vec![Some("reflector refused the link".to_string())],
            "the Ended arm (where the disconnect announcement fires) ran exactly once"
        );
        assert_eq!(
            announcement_kinds(&events),
            Vec::<String>::new(),
            "no member reached, so no AnnouncementStarted"
        );
        assert_eq!(
            m17_announce_log(&c),
            vec![
                (
                    true,
                    "Monitoring M17 reflector M 1 7 - T E S T module A".to_string()
                ),
                (
                    false,
                    "Stopped monitoring M17 reflector M 1 7 - T E S T module A".to_string()
                ),
            ],
            "Monitor announced the leg, then the Ended arm announced its end, once each"
        );
        assert_only_control(&refl.datagrams());
        refl.stop();
    }
}

#[cfg(test)]
mod echolink_tests {
    //! The node's `EchoLink` leg, the key guard and the snapshot, end to end
    //! on loopback. Every station here is a `FakeStation` and the directory
    //! a `FakeDirectory`, both on 127.0.0.1, and every test build of this
    //! crate is loopback-only (`test-support` turns on
    //! `echolink-loopback-only`), so nothing here can reach a real `EchoLink`
    //! server or station.

    use super::*;

    fn test_controller() -> NodeController {
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        NodeController::new(station, SecretProvider::new())
    }

    const EL_CALL: &str = "KX7ABC-L";
    const EL_PASSWORD: &str = "el-pw-SENTINEL-4471";
    const EL_NODE: u32 = 9999;
    const EL_ECHO: &str = "*ECHOTEST*";

    fn el_wait(mut f: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if f() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Loopback binds on ephemeral ports, the fake's node mapped to the
    /// fake's own two ports, and fast clocks.
    fn el_network(
        fake: &astar_echolink::fake::FakeStation,
    ) -> astar_console::echolink::EchoLinkNetwork {
        use astar_console::echolink::{EchoLinkNetwork, PeerAddr, StationEntry, Timings};
        let addr = fake.addr();
        EchoLinkNetwork {
            audio_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            peer_addr: Arc::new(move |e: &StationEntry| {
                if e.node == EL_NODE {
                    addr
                } else {
                    PeerAddr::fixed(e.ip)
                }
            }),
            match_by_ip: false,
            timings: Timings {
                keepalive: std::time::Duration::from_millis(100),
                connection_timeout: std::time::Duration::from_secs(2),
                give_up_after: 20,
                talk_hold: std::time::Duration::from_millis(100),
                // Short, so a logoff a test abandons on a silent directory
                // ends soon after the test does.
                directory_read_timeout: std::time::Duration::from_secs(2),
                // Short, so a dial that misses can be seen to ask for a
                // fresh list.
                list_min_interval: std::time::Duration::from_millis(50),
                ..Timings::default()
            },
        }
    }

    struct ElRig {
        c: Arc<NodeController>,
        fake: astar_echolink::fake::FakeStation,
        dir: astar_echolink::fake::FakeDirectory,
    }

    /// A controller with `[echolink]`, the station pointed at the fakes, in
    /// Bridge mode with the inbound listener up on 127.0.0.1:0 (the
    /// conference the leg's stations join). `password` seeds the secret;
    /// `None` leaves it unset. The login is started the way `main` starts
    /// it, and NOT waited for.
    fn el_controller(password: Option<&str>) -> ElRig {
        use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
        let fake = FakeStation::start(EL_ECHO);
        fake.set_echo(false);
        let dir = FakeDirectory::start(FakeLogin::Ok, vec![fake.entry(EL_NODE)]);
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        station.set_echolink_network(el_network(&fake));
        let secrets = SecretProvider::new();
        if let Some(pw) = password {
            secrets.put(format!("echolink:{EL_CALL}"), pw);
        }
        let mut c = NodeController::new(station, secrets);
        c.set_echolink_config(Some(crate::config::EchoLinkCfg {
            callsign: EL_CALL.into(),
            servers: vec![dir.addr().to_string()],
            location: "Denver".into(),
        }));
        c.execute(NodeCommand::SetBridge {
            mode: Some("bridge".into()),
            mix_minus: None,
            include_local_radio: None,
        })
        .expect("bridge mode");
        c.execute(NodeCommand::EnableInbound)
            .expect("inbound listener on 127.0.0.1:0");
        c.start_echolink_login();
        ElRig {
            c: Arc::new(c),
            fake,
            dir,
        }
    }

    /// [`el_controller`] with the password set, logged in with the list in
    /// hand.
    fn el_logged_in() -> ElRig {
        let r = el_controller(Some(EL_PASSWORD));
        assert!(
            el_wait(|| r
                .c
                .snapshot()
                .echolink
                .is_some_and(|e| e.login == "logged_in")
                && !r.c.station.echolink_stations().is_empty()),
            "never logged in: {:?}",
            r.c.snapshot().echolink
        );
        r
    }

    fn el(action: crate::command::EchoLinkAction, target: Option<&str>) -> NodeCommand {
        NodeCommand::EchoLink {
            action,
            target: target.map(str::to_string),
        }
    }

    /// Dial the fake and pump until it reports connected.
    fn el_dial_connected(r: &ElRig) {
        r.c.execute(el(crate::command::EchoLinkAction::Dial, Some("9999")))
            .expect("the dial is placed");
        assert!(
            el_wait(|| {
                r.c.pump();
                r.c.snapshot().echolink.is_some_and(|e| {
                    e.peers
                        .iter()
                        .any(|p| p.call == EL_ECHO && p.state == "connected")
                })
            }),
            "the fake never answered: {:?}",
            r.c.snapshot().echolink
        );
    }

    #[test]
    fn key_is_refused_while_an_echolink_session_is_live() {
        let refusal = key_refusal(false, false, false, false, true).expect("a refusal");
        assert!(
            refusal.message.contains("EchoLink"),
            "{:?}",
            refusal.message
        );
        assert!(
            refusal.message.contains("refusing to key"),
            "{:?}",
            refusal.message
        );
    }

    #[test]
    fn post_key_is_refused_while_the_node_leg_has_a_station() {
        let r = el_logged_in();
        el_dial_connected(&r);
        assert!(r.c.station.snapshot().echolink_active);
        let err =
            r.c.execute(NodeCommand::Key)
                .expect_err("a station on the EchoLink leg refuses the key");
        assert_eq!(
            err.message,
            "refusing to key: an EchoLink session is active and EchoLink transmit is not \
             remotely keyable",
            "the guard refused it, not the station"
        );
        assert!(!r.c.station.snapshot().ptt, "nothing was keyed");
        r.c.teardown_echolink();
    }

    #[test]
    fn echolink_dial_is_refused_without_the_section() {
        use crate::command::EchoLinkAction;
        let c = test_controller();
        for cmd in [
            el(EchoLinkAction::Dial, Some("9999")),
            el(EchoLinkAction::Disconnect, Some(EL_ECHO)),
            el(EchoLinkAction::Disconnect, None),
        ] {
            assert_eq!(
                c.execute(cmd).unwrap_err().message,
                "EchoLink isn't enabled on this node — its operator can add an [echolink] section."
            );
        }
        let snap = c.snapshot();
        assert!(snap.echolink.is_none() && !snap.echolink_enabled);
        assert!(c.station.echolink_state().is_none(), "nothing logged in");
    }

    #[test]
    fn echolink_refuses_a_password_the_directory_cannot_take() {
        for bad in ["pass\rword", "pass\nword", "pass¬word"] {
            let r = el_controller(Some(bad));
            let snap = r.c.snapshot();
            let card = snap.echolink.expect("the card is there");
            let why = card.error.expect("a setup error");
            assert!(why.contains("can't take"), "{why}");
            assert!(
                !why.contains("pass"),
                "the sentence echoes the password: {why}"
            );
            assert!(r.c.station.echolink_state().is_none(), "no login started");
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(r.dir.exchanges(), 0, "nothing reached the directory");
        }
    }

    #[test]
    fn echolink_dial_and_disconnect_reach_a_fake_station() {
        use crate::command::EchoLinkAction;
        let r = el_logged_in();
        assert_eq!(
            r.c.execute(el(EchoLinkAction::Dial, None))
                .unwrap_err()
                .message,
            "A dial needs a node number or callsign."
        );
        // Past the list's 30 s floor (50 ms here): a miss asks for a fresh
        // list as it refuses.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let before = r.dir.exchanges();
        assert_eq!(
            r.c.execute(el(EchoLinkAction::Dial, Some("1234")))
                .unwrap_err()
                .message,
            "Not in the station list yet — refreshing…"
        );
        assert!(
            el_wait(|| r.dir.exchanges() > before),
            "the miss never asked for a fresh list"
        );
        assert_eq!(r.fake.sdes_count(), 0, "a refused dial sends nothing");

        el_dial_connected(&r);
        assert!(r.fake.sdes_count() > 0, "the fake heard the SDES");
        assert_eq!(
            r.c.execute(el(EchoLinkAction::Dial, Some(EL_ECHO)))
                .unwrap_err()
                .message,
            "EchoLink station *ECHOTEST* is already connected to this node."
        );
        assert_eq!(
            r.c.execute(el(EchoLinkAction::Disconnect, Some("W1AW")))
                .unwrap_err()
                .message,
            "No EchoLink station W1AW is connected to this node."
        );

        r.c.execute(el(EchoLinkAction::Disconnect, Some("*echotest*")))
            .expect("the hang-up");
        assert!(
            el_wait(|| !r.fake.bye_times().is_empty()),
            "the fake never heard the BYE"
        );
        assert!(
            r.c.snapshot().echolink.is_some_and(|e| e.peers.is_empty()),
            "the station left the leg at once"
        );
        assert_eq!(
            r.c.execute(el(EchoLinkAction::Disconnect, None))
                .unwrap_err()
                .message,
            "No EchoLink station is connected to this node."
        );

        // A second dial. A target that is present but blank names nobody and
        // is refused; only an absent one hangs up everyone.
        el_dial_connected(&r);
        for blank in ["", "   "] {
            assert_eq!(
                r.c.execute(el(EchoLinkAction::Disconnect, Some(blank)))
                    .unwrap_err()
                    .message,
                "A disconnect needs a callsign; leave the target out to hang up on everyone."
            );
        }
        assert_eq!(
            r.c.snapshot().echolink.map(|e| e.peers.len()),
            Some(1),
            "a blank target hung nobody up"
        );
        r.c.execute(el(EchoLinkAction::Disconnect, None))
            .expect("hang up everyone");
        assert!(r.c.snapshot().echolink.is_some_and(|e| e.peers.is_empty()));
        r.c.teardown_echolink();
    }

    #[test]
    fn busy_on_a_net_counts_echolink_stations() {
        let r = el_logged_in();
        assert_eq!(r.c.busy_on_a_net(), None);
        el_dial_connected(&r);
        assert_eq!(r.c.busy_on_a_net().as_deref(), Some("1 EchoLink station"));
        let refused =
            r.c.execute(NodeCommand::Restart { disconnect: false })
                .unwrap_err();
        assert_eq!(
            refused.message,
            "restart refused: this node is on a net (1 EchoLink station)"
        );
        r.c.teardown_echolink();
        assert_eq!(
            busy_reason(0, 0, 2, false).as_deref(),
            Some("2 EchoLink stations")
        );
        assert_eq!(
            busy_reason(1, 1, 1, true).as_deref(),
            Some("1 call, 1 link, 1 EchoLink station, Discord joined")
        );
    }

    #[test]
    fn the_snapshot_carries_the_echolink_card_but_no_password() {
        let r = el_logged_in();
        el_dial_connected(&r);
        let snap = r.c.snapshot();
        assert!(snap.echolink_enabled);
        let card = snap.echolink.clone().expect("a card");
        assert_eq!(card.login, "logged_in");
        assert_eq!(card.callsign, EL_CALL);
        assert_eq!(card.error, None);
        assert_eq!(card.peers.len(), 1);
        assert_eq!(card.peers[0].node, EL_NODE);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains(r#""echolink_enabled":true"#), "{json}");
        assert!(json.contains(r#""login":"logged_in""#), "{json}");
        assert!(!json.contains(EL_PASSWORD), "{json}");
        assert!(!json.contains("SENTINEL"), "{json}");
        r.c.teardown_echolink();
    }

    #[test]
    fn a_node_without_the_password_says_so_and_logs_nothing_in() {
        let r = el_controller(None);
        let card = r.c.snapshot().echolink.expect("a card");
        assert_eq!(card.login, "logged_out");
        assert_eq!(
            card.error.as_deref(),
            Some("no password: set the echolink:KX7ABC-L secret")
        );
        assert_eq!(
            r.c.execute(el(crate::command::EchoLinkAction::Dial, Some("9999")))
                .unwrap_err()
                .message,
            "EchoLink isn't logged in yet: no password: set the echolink:KX7ABC-L secret"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(r.dir.exchanges(), 0, "nothing reached the directory");
        assert!(r.c.station.echolink_state().is_none());

        // The secret arriving over POST /secrets (any case) starts the login.
        r.c.execute(NodeCommand::ProvideSecret {
            username: "echolink:kx7abc-l".into(),
            secret: EL_PASSWORD.into(),
        })
        .expect("stored");
        assert!(
            el_wait(|| r
                .c
                .snapshot()
                .echolink
                .is_some_and(|e| e.login == "logged_in")),
            "the login never started: {:?}",
            r.c.snapshot().echolink
        );
        assert_eq!(r.c.snapshot().echolink.unwrap().error, None);
        r.c.teardown_echolink();
    }

    /// A directory that has gone silent cannot hold the end of the run: the
    /// stations are hung up, and the logoff is abandoned after the wait.
    #[test]
    fn teardown_returns_within_its_bound_against_a_silent_directory() {
        let r = el_logged_in();
        el_dial_connected(&r);
        r.dir.set_reply(astar_echolink::fake::FakeLogin::Silent);
        let started = std::time::Instant::now();
        let finished =
            r.c.teardown_echolink_within(std::time::Duration::from_millis(300));
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(1),
            "teardown took {took:?} with a 300 ms bound"
        );
        assert!(
            el_wait(|| !r.fake.bye_times().is_empty()),
            "the stations were hung up before the logoff"
        );
        // Silent means the logoff is still waiting on its read time-out
        // (2 s here), so it must not have been reported as finished.
        assert!(
            !finished,
            "the logoff cannot have finished on a silent directory"
        );
        assert_eq!(ECHOLINK_LOGOFF_WAIT, std::time::Duration::from_secs(10));
    }

    #[test]
    fn teardown_hangs_up_and_logs_off() {
        let r = el_logged_in();
        el_dial_connected(&r);
        r.c.teardown_echolink();
        assert!(
            el_wait(|| !r.fake.bye_times().is_empty()),
            "the fake never heard the BYE"
        );
        let logoff = astar_echolink::directory::logoff_line(EL_CALL, EL_PASSWORD, "Denver");
        assert!(
            r.dir.lines().iter().any(|l| l[..] == logoff[..]),
            "the directory never heard the logoff"
        );
        assert!(r.c.station.echolink_state().is_none());
    }
}
