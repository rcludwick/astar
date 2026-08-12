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

use astar_station::{InboundConfig, RegisterConfig, Station, StationError, StationEvent};

use crate::{
    command::{LinkAction, NodeCommand, NodeError, NodeEvent, NodeReply, NodeSnapshot},
    config::{AnnounceCfg, LinkShape},
    dtmf_commands::DtmfCommandAssembler,
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

/// The literal placeholder token substituted in the join greeting template.
pub(crate) const JOIN_NODE_TOKEN: &str = "{server-node-number}";

/// Default inter-digit gap (iax-d254) that finalizes a pending DTMF `*`
/// command when `[dtmf] inter_digit_timeout_ms` is unset. Matches
/// [`crate::config::NodeFileConfig::dtmf_inter_digit_timeout_ms`]'s default.
#[allow(clippy::duration_suboptimal_units)]
const DEFAULT_DTMF_INTER_DIGIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(3000);

/// Core in-process controller that wraps a [`Station`].
///
/// All adapters (HTTP, stdin, TUI) call into this struct; it never leaks
/// credentials — `NodeError`, `NodeReply`, `NodeEvent`, and `NodeSnapshot` are
/// all secret-free.
pub struct NodeController {
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
        }
    }

    /// Access the secret provider (e.g. to `resolve` in tests or to bulk-load env).
    pub fn secrets(&self) -> &SecretProvider {
        &self.secrets
    }

    /// Execute one command synchronously.  Returns `Ok(NodeReply)` on success
    /// or `Err(NodeError)` on failure.  Secret-free — no credential ever appears
    /// in the return types.
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
                // D-Star session must never be keyable from here.
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
                if let Some(refusal) = key_refusal(self.station.snapshot().dstar_active) {
                    return Err(refusal);
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
                self.secrets.put(username, secret);
                Ok(NodeReply::Ok)
            }
            NodeCommand::Status => Ok(NodeReply::Snapshot(self.snapshot())),
            NodeCommand::Shutdown => {
                self.stop.store(true, Ordering::Relaxed);
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
        }
    }

    /// Apply a `POST /link` command (iax-d829.1): connect (transceive) /
    /// monitor (RX-only) / disconnect a node-to-node link. `addr`, when
    /// present, dials an explicit `host:port` (harness / localhost), bypassing
    /// `AllStar` DNS.
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
    fn handle_link(
        &self,
        action: LinkAction,
        node: &str,
        addr: Option<&str>,
    ) -> Result<NodeReply, NodeError> {
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

    /// Current point-in-time view — secret-free.
    pub fn snapshot(&self) -> NodeSnapshot {
        let state = self.station.snapshot();
        NodeSnapshot {
            node_id: self.node_id.clone(),
            listening: self.station.is_listening(),
            registered: self.station.is_registered(),
            calls: state.calls,
            links: self.station.link_roster().links,
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
        // Drain received DTMF digits and execute any finalized `*` commands.
        self.drain_dtmf_commands();
        // iax-7d51: key/unkey transceive links to follow member PTT — a far-end
        // node mutes relayed audio from an unkeyed sender.
        self.station.sync_link_keying();
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

    /// Broadcast one `NodeEvent` to all live subscribers, pruning dead senders.
    fn broadcast(&self, ev: &NodeEvent) {
        let mut senders = self.senders.lock().expect("senders mutex poisoned");
        senders.retain(|tx| tx.send(ev.clone()).is_ok());
    }

    /// `true` after a `Shutdown` command has been received.
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Current conference-bridge configuration (iax-647d). Used by adapters/tests
    /// to read back the effect of `POST /bridge`.
    #[must_use]
    pub fn bridge_config_for_test(&self) -> astar_iax::BridgeConfig {
        self.station.bridge_config()
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

/// Whether a key-down must be refused, given the snapshot's `dstar_active`
/// flag (iax-d9f4). `Some(err)` refuses; `None` lets the key through.
///
/// Pure, so the policy is testable without a `ThumbDV` and a live reflector —
/// the only way to make a real `Station` report `dstar_active`.
///
/// D-Star is the one network this crate must never key. Everything else
/// reachable from here (IAX2, M17) is remotely keyable by design; see
/// `Station::set_ptt`'s "Remote-control surfaces" section for why the check
/// lives at the caller rather than inside the station.
fn key_refusal(dstar_active: bool) -> Option<NodeError> {
    dstar_active.then(|| NodeError {
        message: "refusing to key: a D-Star session is active and D-Star transmit is not \
                  remotely keyable"
            .into(),
    })
}

/// Map a `StationError` to a secret-free `NodeError`.
/// `StationError`'s `Display` impl is already secret-free (it never includes
/// passwords or tokens).
fn station_err(e: &StationError) -> NodeError {
    NodeError {
        message: e.to_string(),
    }
}

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
        LinkShape::Standard => (
            node_id.unwrap_or("").to_string(),
            secrets.link_secret(node),
            astar_iax::CallMode::Standard,
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
        ctl.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "1999".into(),
            addr: Some("127.0.0.1:4569".into()),
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
        ctl.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "1999".into(),
            addr: Some("127.0.0.1:4569".into()),
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

        c.execute(NodeCommand::Link {
            action: LinkAction::Connect,
            node: "55553".into(),
            addr: Some("127.0.0.1:4569".into()),
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
        c.execute(NodeCommand::Link {
            action: LinkAction::Monitor,
            node: "42".into(),
            addr: Some("127.0.0.1:4569".into()),
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
        c.execute(NodeCommand::Link {
            action: LinkAction::Monitor,
            node: "55553".into(),
            addr: Some("127.0.0.1:4569".into()),
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

    // -- iax-5029: per-target dial shape + secret selection -----------------

    #[test]
    fn link_dial_params_standard_resolves_link_namespace_secret() {
        let secrets = SecretProvider::new();
        secrets.put("link:1999", "outbound-pw");
        let (caller_id, secret, mode) =
            link_dial_params(LinkShape::Standard, "1999", Some("77777"), None, &secrets);
        assert_eq!(caller_id, "77777");
        assert_eq!(secret, "outbound-pw");
        assert_eq!(mode, astar_iax::CallMode::Standard);
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
            tts: None,
            events: None,
        });
        let register_cfg = Some(RegisterConfig {
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

    // --- iax-d9f4: D-Star is never remotely keyable ---

    /// A live D-Star session refuses `NodeCommand::Key`. This crate exposes
    /// keying over HTTP (`POST /key`) and a TUI keystroke; D-Star transmit
    /// must stay a local, deliberate act.
    #[test]
    fn keying_is_refused_while_a_dstar_session_is_active() {
        let refusal = key_refusal(true).expect("an active D-Star session must refuse the key");
        assert!(
            refusal.message.contains("D-Star"),
            "the refusal must say why, so an operator is not left guessing: {:?}",
            refusal.message
        );
    }

    /// Every other network stays remotely keyable — the guard must not have
    /// turned `POST /key` off wholesale.
    #[test]
    fn keying_is_allowed_when_no_dstar_session_is_active() {
        assert!(
            key_refusal(false).is_none(),
            "IAX2 and M17 keying must be unaffected by the D-Star guard"
        );
    }
}
