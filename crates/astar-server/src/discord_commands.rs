// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The node's side of the Discord slash commands: the adapters, the pump
//! hook, and the admin entry points.
//!
//! **No policy lives here.** Every rule — who may dial, who may take the
//! mic, what the master switch stops, what each refusal says — is in
//! `astar_discord::Commander`, which is a pure state machine with no
//! network under it. This module only moves data: gateway events in,
//! station calls out, and one `AdminView` back for the admin page. If a
//! decision ever appears in this file it is in the wrong place.
//!
//! The one thing that IS decided here is the shape of the call into the
//! station, and it is the safety property of the whole feature — see
//! [`ControllerNode`].

#[cfg(not(feature = "discord"))]
use crate::{
    command::{NodeError, NodeReply},
    controller::NodeController,
};

/// The refusal for a build compiled without the `discord` feature, and for
/// a node that has not configured approvers. Both are "there is nothing
/// here to talk to", and both are worth saying plainly rather than 404ing.
#[cfg(not(feature = "discord"))]
const NO_FEATURE: &str = "discord: this build has no discord feature";

#[cfg(feature = "discord")]
mod with_discord {
    use std::sync::Mutex;

    use astar_discord::{AdminAction, Commander, GatewayOps, LinkRef, Network, NodeOps};

    use crate::{
        command::{LinkAction, M17Action, NodeError, NodeReply},
        controller::NodeController,
    };

    /// What a node says when the slash-command half was never turned on.
    /// Named rather than inlined because three entry points say it and the
    /// admin page matches on the wording.
    const NOT_CONFIGURED: &str =
        "discord: slash commands are not configured (set [discord] approvers)";

    /// The prefix the HTTP layer maps to 400. Everything else prefixed
    /// `discord: ` is a refusal about state, which is a 409.
    const BAD_REQUEST: &str = "discord: bad request: ";

    /// What an admin call gets when the commander is checked out by the
    /// pump. Rare — the pump holds it for microseconds — except behind a
    /// `/dial`, which can sit in DNS or a portal token mint for seconds.
    ///
    /// **The `discord: busy: ` prefix is the wire contract**, exactly as
    /// `BAD_REQUEST` is: the HTTP layer maps it to 503 + `Retry-After`, and
    /// classifying on a prefix rather than on the whole sentence is what
    /// keeps a rewording of the tail from silently becoming a 409. Reword
    /// after the colon freely; do not touch what is before it.
    const BUSY: &str = "discord: busy: a dial may be in progress; try again";

    /// `hangup` of a D-Star link. Nothing on this node can hold one yet, so
    /// the rule engine never names one, but the trait can. A request the
    /// node cannot serve gets the same honest words `/dial dstar` does.
    const DSTAR_NOT_ON_NODE: &str = "D-Star isn't wired into this node's conference yet.";

    /// One public line, as the node's own log records it.
    ///
    /// A function rather than an inline `format!` so that the thing the
    /// journal will be grepped for — the prefix, and the fact that the id
    /// survives into it — is pinned by a test instead of being whatever the
    /// format string happened to say.
    fn audit_line(text: &str) -> String {
        format!("astar-server: discord: {text}")
    }

    fn bad_request(what: &str) -> NodeError {
        NodeError {
            message: format!("{BAD_REQUEST}{what}"),
        }
    }

    // -----------------------------------------------------------------------
    // The commander, checked out
    // -----------------------------------------------------------------------

    /// The commander, taken out of its mutex for the duration of a run and
    /// put back when this value drops — **including when the run panics**.
    ///
    /// Why a guard rather than a plain take-and-put-back: everything the
    /// commander is asked to do calls back out of it, through `NodeOps`
    /// into the station and through `GatewayOps` onto the gateway's queue.
    /// None of that may happen under a controller mutex, so the commander
    /// cannot simply be borrowed in place — it has to leave the mutex. And
    /// once it has left, an unwind between the take and the put-back would
    /// lose it: the node would keep running with no rule engine at all, no
    /// approvals, no mic timer, and nothing to say why.
    ///
    /// So the put-back happens in `Drop`, and a panic is made loud rather
    /// than quiet. The state it puts back may be half-applied — a panic
    /// mid-`handle` is a bug, and this is damage control, not recovery.
    ///
    /// The panic itself is deliberately NOT caught, and what that costs is
    /// worth stating exactly: it kills the **pump thread**, permanently.
    /// `run.rs` spawns that thread as a bare loop and only `join()`s it at
    /// shutdown, so the process goes on serving HTTP while audio, DTMF,
    /// announcements and SSE stop for good — and the gateway's events
    /// channel, which nothing drains any more, grows without bound. That is
    /// a worse failure than a crash and it belongs on the backlog (a dead
    /// pump thread should take the process down); it is not, however, a
    /// reason for the Discord commander to be the one subsystem that
    /// swallows its own bugs.
    pub(crate) struct CheckedOutCommander<'a> {
        slot: &'a Mutex<Option<Commander>>,
        commander: Option<Commander>,
    }

    impl<'a> CheckedOutCommander<'a> {
        /// Take the commander out, or `None` when somebody else already
        /// has it. Holds the mutex for exactly the `take` — the whole point
        /// is that nothing else runs under it.
        pub(crate) fn take(slot: &'a Mutex<Option<Commander>>) -> Option<Self> {
            let commander = slot
                .lock()
                .expect("discord commander mutex poisoned")
                .take()?;
            Some(Self {
                slot,
                commander: Some(commander),
            })
        }

        pub(crate) fn get(&mut self) -> &mut Commander {
            self.commander
                .as_mut()
                .expect("the commander is taken only in Drop")
        }
    }

    impl Drop for CheckedOutCommander<'_> {
        fn drop(&mut self) {
            let Some(commander) = self.commander.take() else {
                return;
            };
            if std::thread::panicking() {
                eprintln!(
                    "astar-server: discord: the rule engine panicked while checked out — its \
                     state is being restored as it stands and may be half-applied. This is a \
                     bug; the approvals file is the durable copy."
                );
            }
            // The one place in this crate that recovers from a poisoned
            // mutex instead of propagating it, and it is not swallowing a
            // fault: this lock is held for a `take` and a store with no
            // user code between them, so the only way it is poisoned is
            // that some OTHER panic is already unwinding — and panicking
            // again here would abort the process while losing the
            // commander anyway. The panic that poisoned it is still
            // propagating on its own thread.
            match self.slot.lock() {
                Ok(mut slot) => *slot = Some(commander),
                Err(poisoned) => *poisoned.into_inner() = Some(commander),
            }
        }
    }

    // -----------------------------------------------------------------------
    // GatewayOps
    // -----------------------------------------------------------------------

    /// The commander's gateway, which may not exist yet.
    ///
    /// `Offline` is not a silent failure mode: with no gateway the bot is
    /// not in Discord at all, so there is no channel to post into and
    /// nobody to mute. It behaves exactly as a live `GatewayHandle` whose
    /// runtime thread has gone — that handle also drops every send — which
    /// is why the commander needs no concept of an offline gateway.
    ///
    /// It exists so the mic timers and the link watch keep running while
    /// the bot is down, and so the admin page still works before the token
    /// has ever arrived.
    enum CommanderGateway {
        Live(astar_discord::GatewayHandle),
        Offline,
    }

    impl GatewayOps for CommanderGateway {
        fn reply(&mut self, interaction: u64, text: &str) {
            if let Self::Live(h) = self {
                h.reply(interaction, text);
            }
        }
        fn announce(&mut self, text: &str) {
            // The node's own log gets it FIRST, and gets it whether or not
            // there is a gateway to post through.
            //
            // The spec asks for both records, and the reason is what
            // happens without this one: the only trace of who pointed this
            // node at which AllStar node would be a message in a Discord
            // channel — deletable by any moderator, invisible to whoever
            // runs the node, and never written at all when the bot is
            // offline or missing Send Messages. A node's log is the
            // operator's own copy.
            //
            // **Nothing secret can arrive here.** Every call site is in
            // `Commander` and every one builds its string from fixed text
            // plus a Discord user id (`<@id>`), a display name, or an
            // AllStar node number — all of them things this same line is
            // about to post publicly. The two error-bearing paths
            // deliberately do NOT come through here: a state-file failure
            // becomes `last_error` and a fixed `WRITE_FAILED` suffix on the
            // caller's reply, and a mute or audio failure's reason string
            // goes to `last_error`/`audio_error` for `/status`, never into
            // a public line.
            eprintln!("{}", audit_line(text));
            if let Self::Live(h) = self {
                h.announce(text);
            }
        }
        fn set_mute(&mut self, user: u64, mute: bool) {
            if let Self::Live(h) = self {
                h.set_mute(user, mute);
            }
        }
        fn panel(&mut self, talking: Option<&str>) {
            if let Self::Live(h) = self {
                h.panel(talking);
            }
        }
    }

    // -----------------------------------------------------------------------
    // NodeOps
    // -----------------------------------------------------------------------

    /// The node, as the rule engine is allowed to see it.
    ///
    /// **This adapter is the safety property of the feature.** Every call
    /// it makes into the node is written out below with its action fixed
    /// as a literal. They are fixed so that no chain of slash commands —
    /// from an approved user, an admin or an approver — can produce a
    /// transceive link, transmit toward a reflector, or aim this node at an
    /// arbitrary host. `astar_discord::NodeOps` cannot express any of those;
    /// this is where that promise is cashed.
    pub(crate) struct ControllerNode<'a>(pub(crate) &'a NodeController);

    impl NodeOps for ControllerNode<'_> {
        fn bridge_joined(&self) -> bool {
            self.0.discord_bridge_joined()
        }

        fn links(&self) -> Vec<LinkRef> {
            self.0.discord_link_refs()
        }

        fn dial_allstar_monitor(&mut self, node: &str) -> Result<(), String> {
            // SAFETY PROPERTY. `LinkAction::Monitor` is a literal and
            // `addr` is the literal `None`, and both must stay literals:
            // `Connect` would make a slash command key this node's audio
            // onto a far end, and a `Some(addr)` would let one point it at
            // any host:port on the internet. The caller has no way to ask
            // for either — `NodeOps` has no parameter for them — and this
            // is the line that keeps it that way.
            self.0
                .handle_link(LinkAction::Monitor, node, None)
                .map(|_| ())
                .map_err(|e| e.message)
        }

        fn dial_m17_monitor(
            &mut self,
            reflector: &str,
            module: char,
            callsign: &str,
        ) -> Result<(), String> {
            // SAFETY PROPERTY. `M17Action::Monitor` is a literal and must
            // stay one. The monitor it reaches has no transmit path at all,
            // and this line is where a transmitting action would become
            // reachable from a slash command if `M17Action` ever grew one.
            // There is no address argument either: `handle_m17` resolves the
            // name from `[m17.reflectors]` and the operator's directory,
            // never from anything a Discord user typed.
            self.0
                .handle_m17(
                    M17Action::Monitor,
                    Some(reflector),
                    Some(module),
                    Some(callsign),
                )
                .map(|_| ())
                .map_err(|e| e.message)
        }

        fn hangup(&mut self, link: &LinkRef) -> Result<(), String> {
            // SAFETY PROPERTY, same literals, same reason. AllStar gets
            // `Disconnect` and `None`: hanging up is the safe direction, but
            // it reaches the same `handle_link`, and an `addr` here would be
            // as much an arbitrary-host channel as it is above. M17 gets
            // `Disconnect` and no name at all: the node holds one M17 leg,
            // and hanging up takes whichever it is.
            match link.network {
                Network::Allstar => self
                    .0
                    .handle_link(LinkAction::Disconnect, &link.target, None)
                    .map(|_| ())
                    .map_err(|e| e.message),
                Network::M17 => self
                    .0
                    .handle_m17(M17Action::Disconnect, None, None, None)
                    .map(|_| ())
                    .map_err(|e| e.message),
                Network::Dstar => Err(DSTAR_NOT_ON_NODE.to_string()),
            }
        }

        fn link_end_reason(&self, link: &LinkRef) -> Option<String> {
            match link.network {
                Network::M17 => self.0.m17_end_reason(&link.target),
                // An AllStar link ends in the roster; the IAX side keeps no
                // reason the channel could be told.
                Network::Allstar | Network::Dstar => None,
            }
        }

        fn last_heard(&self, link: &LinkRef) -> Option<(String, u64)> {
            match link.network {
                Network::M17 => self.0.m17_last_heard(),
                Network::Allstar | Network::Dstar => None,
            }
        }
    }

    // -----------------------------------------------------------------------
    // The controller's half
    // -----------------------------------------------------------------------

    impl NodeController {
        /// The union roster the rule engine and the hourly heartbeat both
        /// read: the `AllStar` link roster, then the M17 leg while it is
        /// linked.
        ///
        /// One function for both callers, so "what is this node linked to"
        /// cannot mean one thing to `/hangup` and another to the channel's
        /// hourly line.
        pub(crate) fn discord_link_refs(&self) -> Vec<LinkRef> {
            let mut refs: Vec<LinkRef> = self
                .link_roster_node_ids()
                .iter()
                .map(String::as_str)
                .map(LinkRef::allstar)
                .collect();
            if let Some((reflector, module)) = self.m17_linked() {
                refs.push(LinkRef::m17(&reflector, module));
            }
            refs
        }

        /// A `GatewayOps` onto the live gateway, or `Offline`.
        ///
        /// The gateway mutex is held for exactly one cheap clone (a channel
        /// sender and a number) and released at the end of the statement.
        fn discord_gateway_ops(&self) -> CommanderGateway {
            let Some(channel) = self.discord_channel else {
                return CommanderGateway::Offline;
            };
            self.discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned")
                .as_ref()
                .map_or(CommanderGateway::Offline, |g| {
                    CommanderGateway::Live(g.ops(channel))
                })
        }

        /// Move everything the gateway has sent into the pending queue.
        ///
        /// Two short, non-overlapping critical sections: drain the receiver
        /// into a local, release, then append under the queue's own lock.
        /// Nothing is held across the other, so there is no ordering
        /// between these two mutexes for anything else to deadlock on.
        fn drain_discord_events(&self) {
            let drained: Vec<astar_discord::GatewayEvent> = {
                let guard = self
                    .discord_events
                    .lock()
                    .expect("discord events mutex poisoned");
                match guard.as_ref() {
                    // `try_iter` stops at the first empty, which is what a
                    // pump wants: never block, take what is there.
                    Some(rx) => rx.try_iter().collect(),
                    None => Vec::new(),
                }
            };
            if drained.is_empty() {
                return;
            }
            self.discord_pending_events
                .lock()
                .expect("discord pending events mutex poisoned")
                .extend(drained);
        }

        /// The oldest queued event, if any. One event per lock: the
        /// commander must never run with this mutex held, and popping one
        /// at a time means a panic mid-`handle` loses only the event that
        /// was in flight rather than the whole batch.
        fn next_pending_discord_event(&self) -> Option<astar_discord::GatewayEvent> {
            self.discord_pending_events
                .lock()
                .expect("discord pending events mutex poisoned")
                .pop_front()
        }

        fn cache_discord_view(&self, commander: &Commander) {
            *self
                .discord_view
                .lock()
                .expect("discord view mutex poisoned") = Some(commander.view());
        }

        /// Whether a bare `tick` — one that handled no event — could have
        /// moved anything the cached view shows.
        ///
        /// `Commander::view()` deep-clones five `Vec<UserEntry>` and the
        /// present roster: thousands of `String` allocations a second if it
        /// is run on every 50 ms pump, all of them thrown away on an idle
        /// node. `tick` only has two jobs — the mic timers and the link
        /// watch — and it early-returns when neither applies, so when the
        /// last view had no mic holder and no link a tick provably changed
        /// nothing and the clone is pure waste.
        ///
        /// No cached view at all means "seed it", so a fresh commander is
        /// never left invisible.
        fn a_bare_tick_could_move_the_view(&self) -> bool {
            self.discord_view
                .lock()
                .expect("discord view mutex poisoned")
                .as_ref()
                .is_none_or(|v| v.mic_holder.is_some() || v.link.is_some())
        }

        /// A new gateway has been installed in place of an old one: tell
        /// the rule engine, so the audio fault that belonged to the gateway
        /// that died does not outlive it.
        ///
        /// **Applied now, or on the next pump.** The caller
        /// (`ensure_discord_gateway`) holds `discord_op`, and a `Commander`
        /// method must never run under a controller mutex — the rule that
        /// makes the whole check-out dance necessary. `gateway_replaced`
        /// itself is pure (it clears two fields and calls nothing back out),
        /// so running it here when the engine is free is safe; when an HTTP
        /// admin call has it checked out, the fact is remembered in a flag
        /// instead and the next pump applies it, 50 ms later at worst.
        /// Waiting for the checkout is the one thing that is not allowed:
        /// behind a `/dial` it can be seconds, and this runs while a
        /// gateway is being connected.
        pub(crate) fn note_discord_gateway_replaced(&self) {
            if self.discord_channel.is_none() {
                return;
            }
            let Some(mut checked_out) = CheckedOutCommander::take(&self.discord_commander) else {
                self.discord_gateway_replaced
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            };
            checked_out.get().gateway_replaced();
            self.cache_discord_view(checked_out.get());
        }

        /// Pump step: give the rule engine everything the gateway has said,
        /// then run its timers.
        ///
        /// **`tick` runs on EVERY pump, not on a timer of its own.** The
        /// pump is 50 ms and the two things `tick` drives are a 5-second
        /// silence window and a link watch measured in whole seconds, so
        /// the resolution is three orders of magnitude finer than anything
        /// that depends on it — and a separate deadline would be one more
        /// piece of state that can be left armed after the thing it
        /// watches has gone.
        ///
        /// **Lock discipline.** The commander leaves its mutex before it is
        /// run and goes back after (see [`CheckedOutCommander`]), so no
        /// controller mutex is held across `handle`/`tick`. This is not
        /// theoretical: `handle` can reach `dial_allstar_monitor` →
        /// `handle_link`, which resolves DNS and, for a `wt-guest` target,
        /// makes a blocking HTTPS call to mint a portal token. A `/dial`
        /// handled here therefore stalls this tick exactly as a
        /// DTMF-initiated dial already does — a known, pre-existing-shaped
        /// cost, not a new one, and the Discord interaction was deferred by
        /// the gateway glue long before it reached here, so the 3-second
        /// interaction deadline is not at risk.
        ///
        /// A commander that is checked out by an HTTP admin call means this
        /// tick does nothing at all: the drained events stay queued, in
        /// order, and are handled on the next one 50 ms later.
        pub(crate) fn pump_discord_commands(&self) {
            // Cheapest possible gate for the overwhelming majority of
            // nodes, which configure no approvers: one `Option` and one
            // `is_empty`, no locks taken at all.
            if !self.discord_gateway_is_persistent() {
                return;
            }
            self.drain_discord_events();
            let Some(mut checked_out) = CheckedOutCommander::take(&self.discord_commander) else {
                return;
            };
            let mut gw = self.discord_gateway_ops();
            let mut node = ControllerNode(self);
            // Before the queued events, because one of those events may be
            // the new gateway's own `Ready` — and because a fault that
            // belonged to a gateway which no longer exists must not survive
            // into anything this tick decides.
            let replaced = self
                .discord_gateway_replaced
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            if replaced {
                checked_out.get().gateway_replaced();
            }
            let mut handled_any = false;
            while let Some(ev) = self.next_pending_discord_event() {
                checked_out.get().handle(ev, &mut gw, &mut node);
                handled_any = true;
            }
            let could_move = self.a_bare_tick_could_move_the_view();
            checked_out.get().tick(&mut gw, &node);
            // Refresh when something may have moved, and only then — see
            // `a_bare_tick_could_move_the_view`. `could_move` is read
            // BEFORE the tick on purpose: a tick that releases the mic
            // leaves a view with no holder behind it, and asking
            // afterwards would decide not to publish the very change that
            // just happened.
            // `replaced` counts as a change of its own: it clears the audio
            // fault the page and `/status` are reading, and it can arrive on
            // a tick with no events and no mic, which is exactly the shape
            // `could_move` answers `false` to.
            if handled_any || could_move || replaced {
                self.cache_discord_view(checked_out.get());
            }
        }

        /// Ask the rule engine to undo its server mutes, before the gateway
        /// that would carry them is dropped.
        ///
        /// **Runs exactly once.** Two callers reach here and either may be
        /// first — the explicit `NodeCommand::Shutdown`, and
        /// [`NodeController::teardown_discord`] at the end of the run — so
        /// the second one must not re-issue the whole sweep against a
        /// gateway that is already closing.
        ///
        /// Best effort by construction: the unmutes are enqueued, and the
        /// gateway's `Shutdown` — which `teardown_discord` guarantees by
        /// dropping the `Gateway` itself — waits (bounded) for them to
        /// land. A commander that is checked out at this moment is left
        /// alone rather than waited for: the node is going away, and
        /// `Approvals` remembers who was muted by us, so the mark is lifted
        /// the next time that person appears in voice.
        pub(crate) fn shutdown_discord_commander(&self) {
            let mut gw = self.discord_gateway_ops();
            self.shutdown_discord_commander_with(&mut gw);
        }

        /// The sweep itself, over whatever `GatewayOps` it is given.
        ///
        /// Split out for exactly one reason: the gateway this node really
        /// has is either a queue into a runtime thread or `Offline`, and
        /// neither can be read back, so a test of the shipped path could
        /// only assert that the commander came back out of its mutex —
        /// which stays true when the sweep is deleted. Handed a recorder,
        /// the same code says which members it unmuted and how often.
        pub(crate) fn shutdown_discord_commander_with(&self, gw: &mut dyn GatewayOps) {
            if self.discord_channel.is_none() {
                return;
            }
            // The engine FIRST, the once-only flag second — and the order is
            // the whole point. The pump can hold the engine for seconds
            // behind a `/dial`. Claiming the flag before the checkout meant
            // a Shutdown that landed in that window burnt it, failed the
            // take and returned; `teardown_discord` — which runs after the
            // pump is joined, with the engine guaranteed free — then saw the
            // flag and swept nothing. The fallback was switched off by
            // exactly the condition that makes it necessary.
            let Some(mut checked_out) = CheckedOutCommander::take(&self.discord_commander) else {
                eprintln!(
                    "astar-server: discord: the rule engine was busy at shutdown; \
                     the unmute sweep is left for the end of the run"
                );
                return;
            };
            // Still exactly once: two callers cannot hold the engine at the
            // same time, so whoever is here is alone, and `swap` returns the
            // prior value. A second caller finds `true`, and its checkout
            // puts the engine back untouched as it drops.
            if self
                .discord_shutdown_done
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return;
            }
            checked_out.get().shutdown(gw);
            self.cache_discord_view(checked_out.get());
        }

        /// One action from the admin page.
        ///
        /// # Errors
        /// Four shapes, and the HTTP layer maps each by its prefix:
        ///
        /// * `discord: bad request: ` — an unknown action, or a missing or
        ///   unusable `user_id`. **400.**
        /// * `discord: busy: ` — the rule engine is checked out by the
        ///   pump, which behind a `/dial` can be seconds. **503**, with a
        ///   `Retry-After`; the caller should come back, not give up.
        /// * `discord: ` plus the rule engine's own sentence, when the
        ///   action is well-formed and does not apply. **409.**
        /// * the not-configured refusal, when `[discord] approvers` is
        ///   empty. **500**, the same as every other route's answer to a
        ///   Discord feature that was never turned on.
        pub(crate) fn handle_discord_admin(
            &self,
            action: &str,
            user_id: Option<&str>,
        ) -> Result<NodeReply, NodeError> {
            self.require_discord_commands()?;
            let action = parse_admin_action(action, user_id)?;
            let Some(mut checked_out) = CheckedOutCommander::take(&self.discord_commander) else {
                return Err(NodeError {
                    message: BUSY.to_string(),
                });
            };
            let mut gw = self.discord_gateway_ops();
            let mut node = ControllerNode(self);
            let result = checked_out.get().admin(action, &mut gw, &mut node);
            // Refreshed whether the action applied or not: a refusal can
            // still have moved `last_error`, and the page reads the cache.
            self.cache_discord_view(checked_out.get());
            match result {
                Ok(()) => Ok(NodeReply::Ok),
                // Not a bad request — the request was well-formed and the
                // engine refused it (banned user, approver, nothing to
                // revoke). The HTTP layer turns this into a 409.
                Err(why) => Err(NodeError {
                    message: format!("discord: {why}"),
                }),
            }
        }

        /// Everything the admin page renders.
        ///
        /// Answered from the same cache `/status` reads, and for the same
        /// reason: the page must never queue behind a dial. The cache is
        /// refreshed by every pump (50 ms) and by every admin action before
        /// it returns, so the page always reads back its own change.
        ///
        /// # Errors
        /// [`NodeError`] when `[discord] approvers` is empty — there is no
        /// rule engine to describe.
        pub(crate) fn handle_discord_state(&self) -> Result<NodeReply, NodeError> {
            self.require_discord_commands()?;
            let view = self
                .discord_view
                .lock()
                .expect("discord view mutex poisoned")
                .clone()
                .ok_or_else(not_configured)?;
            Ok(NodeReply::DiscordState(Box::new(view)))
        }

        /// `Ok` only when a rule engine exists to act on.
        fn require_discord_commands(&self) -> Result<(), NodeError> {
            if self.discord_gateway_is_persistent() && self.discord_channel.is_some() {
                Ok(())
            } else {
                Err(not_configured())
            }
        }
    }

    fn not_configured() -> NodeError {
        NodeError {
            message: NOT_CONFIGURED.to_string(),
        }
    }

    /// The admin page's vocabulary, mapped onto the rule engine's.
    ///
    /// The six actions that name a user REQUIRE one; the four that act on
    /// the node itself ignore whatever was sent. Nothing here decides
    /// anything — a well-formed action is handed to the commander, which
    /// owns every rule about whether it applies.
    fn parse_admin_action(action: &str, user_id: Option<&str>) -> Result<AdminAction, NodeError> {
        let with_user = |action: &str| -> Result<u64, NodeError> {
            let raw = user_id
                .ok_or_else(|| bad_request(&format!("action {action:?} needs a user_id")))?;
            let id: u64 = raw
                .parse()
                .map_err(|_| bad_request(&format!("user_id {raw:?} is not a Discord user id")))?;
            // Same rule as `[discord] approvers`: `0` parses and is never
            // anybody, so accepting it would record a decision about a user
            // who cannot exist.
            if id == 0 {
                return Err(bad_request(&format!(
                    "user_id {raw:?} is not a Discord user id"
                )));
            }
            Ok(id)
        };
        match action {
            "approve" => Ok(AdminAction::Approve(with_user(action)?)),
            "revoke" => Ok(AdminAction::Revoke(with_user(action)?)),
            "ban" => Ok(AdminAction::Ban(with_user(action)?)),
            "unban" => Ok(AdminAction::Unban(with_user(action)?)),
            "make_admin" => Ok(AdminAction::MakeAdmin(with_user(action)?)),
            "remove_admin" => Ok(AdminAction::RemoveAdmin(with_user(action)?)),
            "release_mic" => Ok(AdminAction::ReleaseMic),
            "enable" => Ok(AdminAction::Enable),
            "disable" => Ok(AdminAction::Disable),
            "hangup" => Ok(AdminAction::Hangup),
            other => Err(bad_request(&format!("unknown action {other:?}"))),
        }
    }

    // =======================================================================
    // Tests
    // =======================================================================

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::command::{NodeCommand, NodeReply};
        use astar_discord::{
            Approvals, BotCommand, BotCommandKind, GatewayEvent, UserRef, VoiceEvent,
        };

        const APPROVER: &str = "333";
        const CHANNEL: u64 = 222;

        /// A controller with slash commands configured, over a hardware-free
        /// station. No gateway is ever started: `Gateway::connect_with`
        /// reaches Discord's real servers with any non-empty token, so no
        /// test in this module may supply a `discord` secret while approvers
        /// are configured — that is exactly what `ProvideSecret` now retries
        /// the gateway on.
        fn commander_controller(approvers: &[&str]) -> NodeController {
            let secrets = crate::secrets::SecretProvider::new();
            let station = astar_station::Station::with_backend_factory(
                astar_station::StationConfig::default(),
                Box::new(|| Box::new(astar_audio::NullBackend::new())),
            );
            let mut c = NodeController::new(station, secrets);
            c.set_discord_config(Some(crate::config::DiscordCfg {
                guild_id: "111".into(),
                channel_id: CHANNEL.to_string(),
                approvers: approvers.iter().map(|a| (*a).to_string()).collect(),
                ..crate::config::DiscordCfg::default()
            }));
            if !approvers.is_empty() {
                c.set_discord_approvals(Approvals::in_memory())
                    .expect("the channel_id above is a snowflake");
            }
            c
        }

        /// The same, with the two mic timers set to values that are neither
        /// the defaults nor each other — see
        /// `the_configured_mic_timers_reach_the_rule_engine`.
        fn commander_controller_with_timers(timeout: u64, silence: u64) -> NodeController {
            let secrets = crate::secrets::SecretProvider::new();
            let station = astar_station::Station::with_backend_factory(
                astar_station::StationConfig::default(),
                Box::new(|| Box::new(astar_audio::NullBackend::new())),
            );
            let mut c = NodeController::new(station, secrets);
            c.set_discord_config(Some(crate::config::DiscordCfg {
                guild_id: "111".into(),
                channel_id: CHANNEL.to_string(),
                approvers: vec![APPROVER.to_string()],
                mic_timeout_secs: Some(timeout),
                mic_silence_secs: Some(silence),
                ..crate::config::DiscordCfg::default()
            }));
            c.set_discord_approvals(Approvals::in_memory())
                .expect("the channel_id above is a snowflake");
            c
        }

        /// The rule engine's own view, taken the way the admin page will:
        /// pump first, so what comes back reflects the live commander and
        /// not merely whatever was last cached.
        fn state_after_pump(c: &NodeController) -> serde_json::Value {
            c.pump();
            match c.execute(NodeCommand::DiscordState) {
                Ok(NodeReply::DiscordState(view)) => {
                    serde_json::to_value(&*view).expect("an AdminView serialises")
                }
                other => panic!("expected a DiscordState reply, got {other:?}"),
            }
        }

        fn ids(view: &serde_json::Value, list: &str) -> Vec<String> {
            view[list]
                .as_array()
                .unwrap_or_else(|| panic!("{list} is an array: {view}"))
                .iter()
                .map(|e| {
                    e["user_id"]
                        .as_str()
                        .expect("user_id is a string")
                        .to_string()
                })
                .collect()
        }

        fn admin(c: &NodeController, action: &str, user_id: Option<&str>) -> Result<(), String> {
            c.execute(NodeCommand::DiscordAdmin {
                action: action.to_string(),
                user_id: user_id.map(ToString::to_string),
            })
            .map(|_| ())
            .map_err(|e| e.message)
        }

        fn request_from(id: u64, name: &str, interaction: u64) -> GatewayEvent {
            GatewayEvent::Command(BotCommand {
                user: UserRef {
                    id,
                    name: name.to_string(),
                },
                kind: BotCommandKind::Request {
                    // Each member needs their own callsign, because /register
                    // refuses one another member already holds.
                    callsign: format!("K{id}ABC"),
                },
                interaction,
            })
        }

        // -- persistence ----------------------------------------------------

        #[test]
        fn the_gateway_is_persistent_exactly_when_approvers_are_configured() {
            assert!(
                commander_controller(&[APPROVER]).discord_gateway_is_persistent(),
                "approvers means slash commands, which means the bot stays online"
            );
            assert!(
                !commander_controller(&[]).discord_gateway_is_persistent(),
                "a [discord] section with no approvers is the audio-only bridge"
            );

            let secrets = crate::secrets::SecretProvider::new();
            let station = astar_station::Station::with_backend_factory(
                astar_station::StationConfig::default(),
                Box::new(|| Box::new(astar_audio::NullBackend::new())),
            );
            let bare = NodeController::new(station, secrets);
            assert!(
                !bare.discord_gateway_is_persistent(),
                "no [discord] section at all is not persistent either"
            );
        }

        // -- the NodeOps adapter --------------------------------------------

        #[test]
        fn node_ops_reports_an_idle_controller_as_unjoined_with_no_links() {
            let c = commander_controller(&[APPROVER]);
            let ops = ControllerNode(&c);
            assert!(
                !ops.bridge_joined(),
                "nothing has joined the voice channel yet"
            );
            assert!(
                ops.links().is_empty(),
                "and an idle node has nothing in its link roster"
            );
        }

        /// `bridge_joined` must read the bridge, not a constant. Installed
        /// the same way a real Join does — through `add_bridge_member` and
        /// `Bridge::join` — but over astar-discord's network-free doubles,
        /// so no gateway and no Discord are involved.
        #[test]
        fn node_ops_sees_the_bridge_once_one_is_installed() {
            let c = commander_controller(&[APPROVER]);
            assert!(!ControllerNode(&c).bridge_joined());

            c.station_for_test()
                .enable_inbound(astar_station::InboundConfig {
                    bind: "127.0.0.1:0".parse().expect("loopback:0 parses"),
                    ..astar_station::InboundConfig::default()
                })
                .expect("the inbound listener starts and builds the engine");
            c.station_for_test()
                .set_bridge_config(astar_iax::BridgeConfig {
                    mode: astar_iax::BridgeMode::Bridge,
                    mix_minus: true,
                    include_local_radio: false,
                    ..Default::default()
                })
                .expect("bridge mode is valid");
            let handle = c
                .station_for_test()
                .add_bridge_member()
                .expect("bridge mode enrolls");
            let rate = handle.sample_rate;
            let mut bridge = astar_discord::Bridge::new(
                Box::new(astar_discord::ConferenceLink::new(handle)),
                Box::new(std::sync::Arc::new(astar_discord::RecordingLink::new())),
                Box::new(astar_discord::Bypass::new(rate)),
                "node 12345".to_string(),
            );
            bridge
                .join(1, CHANNEL)
                .expect("a RecordingLink always joins");
            c.set_discord_bridge_for_test(bridge);

            assert!(
                ControllerNode(&c).bridge_joined(),
                "a bridge in the mutex is what 'joined' means"
            );
        }

        // -- action mapping ---------------------------------------------------

        #[test]
        fn every_documented_action_maps_to_the_rule_engine() {
            // Not "every action succeeds" — several correctly refuse on an
            // idle node (nothing is linked, nobody has the mic, nobody is
            // approved to revoke). What this pins is that each one REACHED
            // the rule engine: a mapping gap shows up as the bad-request
            // prefix, which is the HTTP layer's 400.
            let c = commander_controller(&[APPROVER]);
            for action in [
                "approve",
                "revoke",
                "ban",
                "unban",
                "make_admin",
                "remove_admin",
                "release_mic",
                "enable",
                "disable",
                "hangup",
            ] {
                if let Err(message) = admin(&c, action, Some("444")) {
                    assert!(
                        !message.starts_with("discord: bad request:"),
                        "{action:?} must map to an AdminAction, not be refused as malformed: \
                         {message}"
                    );
                    assert!(
                        message.starts_with("discord: "),
                        "every refusal is prefixed for the HTTP layer: {message}"
                    );
                }
            }
        }

        #[test]
        fn an_unknown_action_is_a_bad_request_and_changes_nothing() {
            let c = commander_controller(&[APPROVER]);
            let before = state_after_pump(&c);
            let err = admin(&c, "delete_everything", Some("444"))
                .expect_err("an action nobody designed must be refused");
            assert_eq!(
                err, "discord: bad request: unknown action \"delete_everything\"",
                "the message names the action, and carries the 400 prefix"
            );
            assert_eq!(state_after_pump(&c), before, "and nothing moved");
        }

        #[test]
        fn the_six_targeted_actions_need_a_usable_user_id() {
            let c = commander_controller(&[APPROVER]);
            let before = state_after_pump(&c);
            for action in [
                "approve",
                "revoke",
                "ban",
                "unban",
                "make_admin",
                "remove_admin",
            ] {
                let missing = admin(&c, action, None)
                    .expect_err("an action about a user needs a user to be about");
                assert_eq!(
                    missing,
                    format!("discord: bad request: action {action:?} needs a user_id")
                );
                let unparseable = admin(&c, action, Some("not-a-snowflake"))
                    .expect_err("a user_id that is not a snowflake is malformed");
                assert_eq!(
                    unparseable,
                    "discord: bad request: user_id \"not-a-snowflake\" is not a Discord user id"
                );
                // `0` parses as a u64 and is nobody. Accepted, it would
                // record a decision about a user who cannot exist.
                let zero = admin(&c, action, Some("0")).expect_err("0 is not a user id");
                assert_eq!(
                    zero,
                    "discord: bad request: user_id \"0\" is not a Discord user id"
                );
            }
            assert_eq!(
                state_after_pump(&c),
                before,
                "not one of those eighteen refusals may have touched the state"
            );
        }

        #[test]
        fn the_four_node_actions_ignore_a_user_id_rather_than_refusing_it() {
            // `enable`/`disable` are the two that apply unconditionally, so
            // they are the ones that can prove "ignored" rather than merely
            // "tolerated".
            let c = commander_controller(&[APPROVER]);
            admin(&c, "disable", Some("444")).expect("disable takes no target and ignores one");
            assert_eq!(state_after_pump(&c)["enabled"], serde_json::json!(false));
            admin(&c, "enable", None).expect("and enable turns it back on");
            assert_eq!(state_after_pump(&c)["enabled"], serde_json::json!(true));
        }

        // -- the actions themselves -------------------------------------------

        #[test]
        fn approving_a_user_shows_them_in_the_state() {
            let c = commander_controller(&[APPROVER]);
            assert!(
                ids(&state_after_pump(&c), "approved").is_empty(),
                "nobody is approved to begin with"
            );
            admin(&c, "approve", Some("444")).expect("approving a fresh user applies");
            assert_eq!(
                ids(&state_after_pump(&c), "approved"),
                vec!["444".to_string()],
                "and the admin page can see it"
            );
        }

        #[test]
        fn banning_an_approved_user_moves_them_out_of_approved() {
            let c = commander_controller(&[APPROVER]);
            admin(&c, "approve", Some("444")).expect("approve applies");
            admin(&c, "ban", Some("444")).expect("ban applies");
            let view = state_after_pump(&c);
            assert_eq!(ids(&view, "banned"), vec!["444".to_string()]);
            assert!(
                ids(&view, "approved").is_empty(),
                "a ban does not leave them approved as well: {view}"
            );
        }

        #[test]
        fn approving_a_banned_user_is_refused_with_the_engines_own_reason() {
            let c = commander_controller(&[APPROVER]);
            admin(&c, "ban", Some("444")).expect("ban applies");
            let before = state_after_pump(&c);
            let err = admin(&c, "approve", Some("444"))
                .expect_err("a banned user must be unbanned first");
            assert!(
                !err.starts_with("discord: bad request:"),
                "this is a well-formed request the engine refused — a 409, not a 400: {err}"
            );
            assert!(
                err.contains("banned"),
                "and it carries the engine's own reason: {err}"
            );
            assert_eq!(
                state_after_pump(&c),
                before,
                "a refused approve leaves the ban exactly where it was"
            );
        }

        // -- events across a checked-out commander -----------------------------

        #[test]
        fn events_survive_a_checked_out_commander_and_are_handled_in_order() {
            let c = commander_controller(&[APPROVER]);
            // The controller's own receiver, replaced by one this test can
            // send on. No gateway exists, so nothing else can write to it.
            let (tx, rx) = std::sync::mpsc::channel();
            *c.discord_events
                .lock()
                .expect("discord events mutex poisoned") = Some(rx);

            // Checked out by hand, exactly as an HTTP admin call holds it.
            let held = c
                .discord_commander
                .lock()
                .expect("discord commander mutex poisoned")
                .take()
                .expect("the commander was built at construction");

            tx.send(request_from(7, "seven", 1))
                .expect("receiver lives");
            tx.send(request_from(8, "eight", 2))
                .expect("receiver lives");
            c.pump();

            assert_eq!(
                c.discord_pending_events
                    .lock()
                    .expect("discord pending events mutex poisoned")
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                vec![request_from(7, "seven", 1), request_from(8, "eight", 2)],
                "a pump that cannot reach the commander must keep every event, in order"
            );
            assert!(
                held.view().pending.is_empty(),
                "and must not have handled either of them"
            );

            *c.discord_commander
                .lock()
                .expect("discord commander mutex poisoned") = Some(held);
            c.pump();

            assert!(
                c.discord_pending_events
                    .lock()
                    .expect("discord pending events mutex poisoned")
                    .is_empty(),
                "the next pump drains the queue"
            );
            assert_eq!(
                ids(&state_after_pump(&c), "pending"),
                vec!["7".to_string(), "8".to_string()],
                "both requests were handled, and in the order they arrived"
            );
        }

        #[test]
        fn a_voice_event_reaches_the_commander_on_the_pump() {
            // The same path without the checkout, so the test above cannot
            // pass merely because nothing ever works.
            let c = commander_controller(&[APPROVER]);
            let (tx, rx) = std::sync::mpsc::channel();
            *c.discord_events
                .lock()
                .expect("discord events mutex poisoned") = Some(rx);
            tx.send(GatewayEvent::Voice(VoiceEvent {
                user: UserRef {
                    id: 9,
                    name: "nine".into(),
                },
                channel: Some(CHANNEL),
                server_muted: false,
                is_bot: false,
            }))
            .expect("receiver lives");

            let view = state_after_pump(&c);
            let present: Vec<String> = view["present"]
                .as_array()
                .expect("present is an array")
                .iter()
                .map(|p| p["user_id"].as_str().expect("a string").to_string())
                .collect();
            assert_eq!(present, vec!["9".to_string()], "{view}");
        }

        // -- /status -----------------------------------------------------------

        #[test]
        fn status_reports_the_cached_view_while_the_commander_is_checked_out() {
            let c = commander_controller(&[APPROVER]);
            // Something distinctive in the cache, so "reports the cache" is
            // distinguishable from "reports a default".
            admin(&c, "disable", Some("444")).expect("disable applies");

            let held = c
                .discord_commander
                .lock()
                .expect("discord commander mutex poisoned")
                .take()
                .expect("the commander was built at construction");

            let snap = c.snapshot();
            let d = snap
                .discord
                .expect("a [discord] section means a status object");
            assert!(
                !d.enabled,
                "the master switch is read from the cache, not skipped"
            );
            assert!(
                !d.commands,
                "no gateway is up, so commands are configured but not live"
            );
            assert!(d.mic_holder.is_none());
            assert!(d.link.is_none());

            *c.discord_commander
                .lock()
                .expect("discord commander mutex poisoned") = Some(held);
        }

        #[test]
        fn the_commander_goes_back_into_its_mutex_when_a_run_panics() {
            // The whole reason `CheckedOutCommander` is a guard and not a
            // take-and-put-back pair: an unwind between the two would leave
            // the node with no rule engine at all, silently.
            let c = commander_controller(&[APPROVER]);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _held = CheckedOutCommander::take(&c.discord_commander)
                    .expect("the commander was built at construction");
                panic!("a bug inside the rule engine");
            }));
            assert!(result.is_err(), "the panic is propagated, not swallowed");
            // Read through the poison if there is any. `Drop` holds this
            // mutex for one store with no user code under it, so it should
            // not be poisoned — but the assertion worth making here is that
            // the commander came BACK, and a poisoned lock must not be
            // allowed to dress a lost commander up as an unrelated failure.
            let slot = match c.discord_commander.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert!(
                slot.is_some(),
                "the guard must put the commander back on unwind"
            );
        }

        #[test]
        fn the_configured_mic_timers_reach_the_rule_engine() {
            // 77 and 9: neither is a default (180 / 5) and they differ from
            // each other, so swapping the two setter arguments — the one
            // mistake this wiring invites — fails this test.
            let c = commander_controller_with_timers(77, 9);
            let v = state_after_pump(&c);
            assert_eq!(
                v["mic_timeout_secs"],
                serde_json::json!(77),
                "[discord] mic_timeout_secs must reach the commander: {v}"
            );
            assert_eq!(
                v["mic_silence_secs"],
                serde_json::json!(9),
                "[discord] mic_silence_secs must reach the commander: {v}"
            );
        }

        #[test]
        fn an_admin_call_is_refused_while_the_commander_is_checked_out() {
            let c = commander_controller(&[APPROVER]);
            let held = c
                .discord_commander
                .lock()
                .expect("discord commander mutex poisoned")
                .take()
                .expect("the commander was built at construction");

            let err = admin(&c, "enable", None).expect_err("a checked-out commander is busy");
            assert_eq!(
                err, "discord: busy: a dial may be in progress; try again",
                "the `discord: busy: ` prefix is what the HTTP layer maps to 503"
            );

            *c.discord_commander
                .lock()
                .expect("discord commander mutex poisoned") = Some(held);
        }

        #[test]
        fn a_new_events_receiver_drops_the_old_queue_and_keeps_its_own() {
            // What a reconnect must do: forget the dead session's tail (it
            // describes a channel state that no longer exists) without
            // losing what the NEW session has already sent — its `Ready`
            // and the voice sweep that re-mutes everyone.
            let c = commander_controller(&[APPROVER]);
            c.discord_pending_events
                .lock()
                .expect("discord pending events mutex poisoned")
                .push_back(request_from(1, "the corpse", 1));

            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(request_from(7, "seven", 2))
                .expect("receiver lives");
            c.install_discord_events(rx);

            assert_eq!(
                ids(&state_after_pump(&c), "pending"),
                vec!["7".to_string()],
                "the old queue is gone and the new receiver's events are not"
            );
        }

        #[test]
        fn the_cached_view_follows_the_mic_even_on_a_tick_that_handles_nothing() {
            // The pump skips the view clone on an idle tick (it is five Vec
            // clones, 20x a second). It must not skip while a mic is held —
            // that is precisely when the view changes without an event.
            let c = commander_controller(&[APPROVER]);
            let (tx, rx) = std::sync::mpsc::channel();
            c.install_discord_events(rx);
            let boss = UserRef {
                id: 333,
                name: "boss".into(),
            };
            tx.send(GatewayEvent::Voice(VoiceEvent {
                user: boss.clone(),
                channel: Some(CHANNEL),
                server_muted: false,
                is_bot: false,
            }))
            .expect("receiver lives");
            tx.send(GatewayEvent::Command(BotCommand {
                user: boss,
                kind: BotCommandKind::Mic { on: true },
                interaction: 3,
            }))
            .expect("receiver lives");

            let v = state_after_pump(&c);
            assert_eq!(
                v["mic_holder"],
                serde_json::json!("333"),
                "an approver in the channel who took the mic holds it: {v}"
            );
            // And a pump that handles nothing at all leaves that standing
            // rather than publishing a stale view over it.
            c.pump();
            let again = state_after_pump(&c);
            assert_eq!(again["mic_holder"], serde_json::json!("333"));
        }

        #[test]
        fn status_says_commands_are_live_only_when_the_bot_is_online() {
            let c = commander_controller(&[APPROVER]);
            assert!(
                !c.snapshot().discord.expect("status object").commands,
                "approvers alone are not a connected bot"
            );
            // An orphan spawns no thread and reaches no network; it stands
            // in for "a gateway is installed and healthy".
            *c.discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned") = Some(astar_discord::Gateway::orphan());
            assert!(
                c.snapshot().discord.expect("status object").commands,
                "configured plus online is what makes commands live"
            );
            // And a corpse is not online.
            *c.discord_gateway
                .lock()
                .expect("discord gateway mutex poisoned") =
                Some(astar_discord::Gateway::orphan_failed());
            assert!(
                !c.snapshot().discord.expect("status object").commands,
                "a gateway whose runtime thread died cannot answer a command"
            );
        }

        // -- not configured -----------------------------------------------------

        #[test]
        fn both_admin_entry_points_refuse_when_no_approvers_are_configured() {
            let c = commander_controller(&[]);
            let admin_err =
                admin(&c, "enable", None).expect_err("a node with no approvers has no rule engine");
            assert_eq!(
                admin_err,
                "discord: slash commands are not configured (set [discord] approvers)"
            );
            let state_err = c
                .execute(NodeCommand::DiscordState)
                .expect_err("and nothing to describe");
            assert_eq!(state_err.message, admin_err);
        }

        #[test]
        fn shutdown_runs_the_commander_and_gives_it_back() {
            // `Commander::shutdown` enqueues its unmutes through the
            // gateway, which is offline here, so what is observable is the
            // checkout: the commander must be back in its mutex afterwards
            // rather than consumed by the shutdown that borrowed it.
            let c = commander_controller(&[APPROVER]);
            admin(&c, "approve", Some("444")).expect("approve applies");

            c.execute(NodeCommand::Shutdown)
                .expect("shutdown always succeeds");

            assert!(c.should_stop(), "the stop flag is still set");
            let restored = c
                .discord_commander
                .lock()
                .expect("discord commander mutex poisoned")
                .as_ref()
                .map(astar_discord::Commander::view)
                .expect("the commander is back in its mutex");
            assert_eq!(
                restored
                    .approved
                    .iter()
                    .map(|e| e.user_id.clone())
                    .collect::<Vec<_>>(),
                vec!["444".to_string()],
                "with its state intact"
            );
        }

        #[test]
        fn a_replaced_gateway_clears_the_audio_fault_even_while_the_engine_is_busy() {
            let c = commander_controller(&[APPROVER]);
            let (tx, rx) = std::sync::mpsc::channel();
            c.install_discord_events(rx);
            tx.send(GatewayEvent::AudioFailed {
                reason: "the audio track ended".into(),
            })
            .expect("receiver lives");
            let failed = state_after_pump(&c);
            assert!(
                !failed["last_error"].is_null(),
                "the fault is recorded first: {failed}"
            );

            // The ordinary path: nothing has the engine, so the new
            // gateway's arrival is applied there and then.
            c.note_discord_gateway_replaced();
            let v = state_after_pump(&c);
            assert!(
                v["last_error"].is_null(),
                "a gateway that was replaced cannot still have a dead track: {v}"
            );

            // And the busy path. A new gateway can be installed while an
            // HTTP admin call has the engine checked out, and the
            // commander must never be run under a controller mutex — so
            // the fact is remembered and the next pump applies it.
            tx.send(GatewayEvent::AudioFailed {
                reason: "the audio track ended".into(),
            })
            .expect("receiver lives");
            let failed = state_after_pump(&c);
            assert!(!failed["last_error"].is_null());
            let held = CheckedOutCommander::take(&c.discord_commander)
                .expect("nothing else has the engine in a test");
            c.note_discord_gateway_replaced();
            drop(held);
            let v = state_after_pump(&c);
            assert!(
                v["last_error"].is_null(),
                "the deferred note must be applied by the next pump: {v}"
            );
        }

        #[test]
        fn every_public_line_is_written_to_the_nodes_own_log_with_the_user_id() {
            // The spec says the audit trail is BOTH the channel and the
            // node's log. Only the channel was implemented, which makes the
            // sole record of who pointed the node where a Discord message
            // that a moderator can delete — and that never exists at all if
            // the bot is missing Send Messages, or is offline.
            //
            // The line is asserted whole rather than "contains": the id is
            // the point of it, and a prefix is what makes it greppable next
            // to every other `astar-server:` line in the journal.
            assert_eq!(
                audit_line("<@444> → AllStar 44444 (receive-only)"),
                "astar-server: discord: <@444> → AllStar 44444 (receive-only)"
            );
            assert_eq!(
                audit_line("<@444> was approved by <@333>."),
                "astar-server: discord: <@444> was approved by <@333>."
            );
        }

        #[test]
        fn shutdown_lifts_the_mutes_this_node_applied_and_lifts_them_once() {
            // The previous version of this test could not fail: its gateway
            // was `Offline`, so deleting the `shutdown()` call it exists to
            // pin changed nothing it looked at. This one watches the ops.
            let c = commander_controller(&[APPROVER]);
            let (tx, rx) = std::sync::mpsc::channel();
            c.install_discord_events(rx);
            // Somebody walks into the voice channel unmuted. The one-mic
            // rule mutes them and records that WE are the ones who did —
            // which is precisely the debt a shutdown owes.
            tx.send(GatewayEvent::Voice(VoiceEvent {
                user: UserRef {
                    id: 444,
                    name: "visitor".into(),
                },
                channel: Some(CHANNEL),
                server_muted: false,
                is_bot: false,
            }))
            .expect("receiver lives");
            c.pump();
            let before = state_after_pump(&c);
            assert_eq!(
                before["present"][0]["server_muted"],
                serde_json::json!(true),
                "the arrival is muted by this node before the shutdown: {before}"
            );

            let mut gw = astar_discord::RecordingOps::default();
            c.shutdown_discord_commander_with(&mut gw);
            assert_eq!(
                gw.mutes,
                vec![(444_u64, false)],
                "shutdown unmutes exactly the member this node muted, and nobody else"
            );

            // And exactly once. Both the signal path and the end-of-run
            // teardown call this; a second sweep would re-issue every PATCH
            // against a gateway that is already closing.
            let mut again = astar_discord::RecordingOps::default();
            c.shutdown_discord_commander_with(&mut again);
            assert!(
                again.mutes.is_empty(),
                "the second shutdown is a no-op, not a second sweep: {:?}",
                again.mutes
            );
        }

        #[test]
        fn a_shutdown_that_found_the_engine_busy_leaves_the_sweep_for_the_teardown() {
            // The signal path and the end-of-run teardown both call the
            // sweep, and the once-only flag used to be claimed BEFORE the
            // checkout. The pump holds the rule engine for seconds behind a
            // `/dial`; a Shutdown landing in that window burnt the flag,
            // failed the take, and returned — and then `teardown_discord`,
            // which runs after the pump is joined with the engine guaranteed
            // free, saw the flag and swept nothing. The fallback was disabled
            // by exactly the condition that makes it necessary, and everyone
            // this node had muted stayed server-muted guild-wide.
            let c = commander_controller(&[APPROVER]);
            let (tx, rx) = std::sync::mpsc::channel();
            c.install_discord_events(rx);
            tx.send(GatewayEvent::Voice(VoiceEvent {
                user: UserRef {
                    id: 444,
                    name: "visitor".into(),
                },
                channel: Some(CHANNEL),
                server_muted: false,
                is_bot: false,
            }))
            .expect("receiver lives");
            c.pump();

            // The pump's side of the race: the engine is checked out.
            let held = CheckedOutCommander::take(&c.discord_commander)
                .expect("the engine is there to be held");
            let mut busy = astar_discord::RecordingOps::default();
            c.shutdown_discord_commander_with(&mut busy);
            assert!(
                busy.mutes.is_empty(),
                "a sweep that could not get the engine lifts nothing: {:?}",
                busy.mutes
            );
            drop(held);

            // The teardown's turn, with the engine free again.
            let mut later = astar_discord::RecordingOps::default();
            c.shutdown_discord_commander_with(&mut later);
            assert_eq!(
                later.mutes,
                vec![(444_u64, false)],
                "the sweep the busy shutdown could not run is still owed, and is paid here"
            );
        }

        #[test]
        fn a_node_without_approvers_runs_no_commander_on_the_pump() {
            // The cheap gate at the top of `pump_discord_commands`: with no
            // approvers there is no commander, and a pump must not invent
            // one or panic looking for it.
            let c = commander_controller(&[]);
            for _ in 0..5 {
                c.pump();
            }
            assert!(
                c.discord_commander
                    .lock()
                    .expect("discord commander mutex poisoned")
                    .is_none(),
                "no approvers, no rule engine"
            );
        }

        // -- the M17 half of the NodeOps adapter ---------------------------

        use crate::test_support::{FakeReply, RecordingReflector, wait_until};

        /// A slash-command controller whose `[m17.reflectors]` table names
        /// one reflector, `M17-TEST`, at `reflector`, which is always a
        /// loopback fake. It runs over a live engine in Bridge mode, which
        /// gives the monitor a conference to feed.
        fn m17_controller(reflector: std::net::SocketAddr) -> NodeController {
            let mut c = commander_controller(&[APPROVER]);
            c.set_m17_config(Some(crate::m17_directory::M17Settings {
                enabled: true,
                reflectors: std::collections::BTreeMap::from([(
                    "M17-TEST".to_string(),
                    (reflector.ip().to_string(), reflector.port()),
                )]),
                directory: None,
            }));
            c.station_for_test()
                .enable_inbound(astar_station::InboundConfig {
                    bind: "127.0.0.1:0".parse().expect("loopback:0 parses"),
                    ..astar_station::InboundConfig::default()
                })
                .expect("the inbound listener starts and builds the engine");
            c.station_for_test()
                .set_bridge_config(astar_iax::BridgeConfig {
                    mode: astar_iax::BridgeMode::Bridge,
                    mix_minus: true,
                    include_local_radio: false,
                    ..Default::default()
                })
                .expect("bridge mode is valid");
            c
        }

        #[test]
        fn links_leaves_out_an_m17_leg_that_is_still_linking() {
            let silent = RecordingReflector::start(FakeReply::Silent);
            let c = m17_controller(silent.addr);
            ControllerNode(&c)
                .dial_m17_monitor("M17-TEST", 'A', "AJ7HR")
                .expect("a table entry passes the pre-flight");
            assert!(
                wait_until(
                    || {
                        c.pump();
                        !silent.datagrams().is_empty()
                    },
                    5_000
                ),
                "the monitor's CONN reaches the reflector, which will never answer it"
            );
            c.pump();
            assert_eq!(
                c.snapshot().m17.map(|m| m.state),
                Some("linking".to_string()),
                "the leg exists and is waiting for its ACKN"
            );
            assert!(
                ControllerNode(&c).links().is_empty(),
                "a leg that is only linking must not be in links(): {:?}",
                ControllerNode(&c).links()
            );
            c.teardown_m17();
            silent.stop();
        }

        #[test]
        fn links_lists_the_m17_leg_once_it_is_linked() {
            let refl = RecordingReflector::start(FakeReply::Ackn);
            let c = m17_controller(refl.addr);
            ControllerNode(&c)
                .dial_m17_monitor("M17-TEST", 'A', "AJ7HR")
                .expect("a table entry passes the pre-flight");
            assert!(
                wait_until(
                    || {
                        c.pump();
                        !ControllerNode(&c).links().is_empty()
                    },
                    5_000
                ),
                "an ACKN'd leg appears in links()"
            );
            assert_eq!(
                ControllerNode(&c).links(),
                vec![LinkRef::m17("M17-TEST", 'A')]
            );
            assert!(
                c.link_roster_node_ids().is_empty(),
                "and it is not an AllStar roster entry: links() is the union"
            );
            c.teardown_m17();
            refl.stop();
        }

        #[test]
        fn dial_m17_monitor_reaches_handle_m17_as_a_monitor() {
            let refl = RecordingReflector::start(FakeReply::Ackn);
            let c = m17_controller(refl.addr);
            ControllerNode(&c)
                .dial_m17_monitor("M17-TEST", 'A', "AJ7HR")
                .expect("a table entry passes the pre-flight");
            assert!(
                wait_until(|| !refl.datagrams().is_empty(), 5_000),
                "the monitor sent its CONN"
            );
            assert_eq!(
                astar_m17::ControlPacket::parse(&refl.datagrams()[0]),
                Some(astar_m17::ControlPacket::Conn {
                    callsign: astar_m17::encode_callsign("AJ7HR").expect("AJ7HR encodes"),
                    module: b'A',
                }),
                "the reflector is asked for module A by AJ7HR, and by nobody else"
            );
            let m = c.snapshot().m17.expect("an accepted dial records the leg");
            assert_eq!(
                (m.reflector.as_str(), m.module.as_str(), m.callsign.as_str()),
                ("M17-TEST", "A", "AJ7HR")
            );
            assert_eq!(
                ControllerNode(&c).dial_m17_monitor("M17-TEST", 'A', "AJ7HR"),
                Err("Already listening to M17 M17-TEST A. `/hangup` first.".to_string()),
                "the node's own refusal comes back verbatim"
            );
            assert_eq!(
                ControllerNode(&c).dial_m17_monitor("M17-NOPE", 'A', "AJ7HR"),
                Err(
                    "I don't know a reflector called M17-NOPE. The node's operator can add it \
                     under `[m17.reflectors]`."
                        .to_string()
                )
            );
            c.teardown_m17();
            assert!(
                refl.datagrams()
                    .iter()
                    .all(|d| astar_m17::ControlPacket::parse(d).is_some()),
                "only control packets ever went to the reflector"
            );
            refl.stop();
        }

        #[test]
        fn hangup_goes_to_the_network_the_link_is_on() {
            let refl = RecordingReflector::start(FakeReply::Ackn);
            let c = m17_controller(refl.addr);
            let (_guard, dead) = crate::test_support::dead_port();
            c.handle_link(LinkAction::Monitor, "1999", Some(&dead))
                .expect("an AllStar monitor to a silent loopback port is accepted");
            ControllerNode(&c)
                .dial_m17_monitor("M17-TEST", 'A', "AJ7HR")
                .expect("a table entry passes the pre-flight");
            assert!(
                wait_until(
                    || {
                        c.pump();
                        ControllerNode(&c).links().len() == 2
                    },
                    5_000
                ),
                "both legs are in the union roster"
            );
            assert_eq!(
                ControllerNode(&c).links(),
                vec![LinkRef::allstar("1999"), LinkRef::m17("M17-TEST", 'A')]
            );

            ControllerNode(&c)
                .hangup(&LinkRef::m17("M17-TEST", 'A'))
                .expect("the M17 leg hangs up");
            assert!(c.snapshot().m17.is_none(), "the M17 record is gone");
            assert_eq!(
                c.link_roster_node_ids(),
                vec!["1999".to_string()],
                "and the AllStar link was not touched"
            );
            assert_eq!(
                ControllerNode(&c).hangup(&LinkRef::m17("M17-TEST", 'A')),
                Err("No M17 link is up.".to_string())
            );

            ControllerNode(&c)
                .hangup(&LinkRef::allstar("1999"))
                .expect("the AllStar link hangs up");
            assert!(
                wait_until(
                    || {
                        c.pump();
                        c.link_roster_node_ids().is_empty()
                    },
                    5_000
                ),
                "the AllStar link left the roster"
            );
            assert_eq!(
                ControllerNode(&c).hangup(&LinkRef {
                    network: Network::Dstar,
                    target: "XRF234 B".to_string(),
                }),
                Err("D-Star isn't wired into this node's conference yet.".to_string())
            );
            refl.stop();
        }

        #[test]
        fn a_redial_clears_the_end_reason_a_nack_left_behind() {
            let refl = RecordingReflector::start(FakeReply::Nack);
            let mut c = m17_controller(refl.addr);
            let leg = LinkRef::m17("M17-TEST", 'A');
            ControllerNode(&c)
                .dial_m17_monitor("M17-TEST", 'A', "AJ7HR")
                .expect("a table entry passes the pre-flight");
            assert!(
                wait_until(
                    || {
                        c.pump();
                        ControllerNode(&c).link_end_reason(&leg).is_some()
                    },
                    5_000
                ),
                "the NACK is reaped by the pump and its reason kept"
            );
            assert_eq!(
                ControllerNode(&c).link_end_reason(&leg),
                Some("reflector refused the link".to_string())
            );
            assert_eq!(
                ControllerNode(&c).link_end_reason(&LinkRef::m17("M17-TEST", 'B')),
                None,
                "the reason belongs to the leg that ended, not to any M17 target"
            );
            assert_eq!(
                ControllerNode(&c).link_end_reason(&LinkRef::allstar("1999")),
                None
            );
            assert!(
                ControllerNode(&c).links().is_empty(),
                "a refused leg never enters links()"
            );

            // The operator fixes the table (here: a reflector that accepts)
            // and the same person dials the same name again.
            let ackn = RecordingReflector::start(FakeReply::Ackn);
            c.set_m17_config(Some(crate::m17_directory::M17Settings {
                enabled: true,
                reflectors: std::collections::BTreeMap::from([(
                    "M17-TEST".to_string(),
                    (ackn.addr.ip().to_string(), ackn.addr.port()),
                )]),
                directory: None,
            }));
            ControllerNode(&c)
                .dial_m17_monitor("M17-TEST", 'A', "AJ7HR")
                .expect("the refused leg was reaped, so a re-dial is accepted");
            assert_eq!(
                ControllerNode(&c).link_end_reason(&leg),
                None,
                "a new dial must not inherit the last leg's ending, or tick_link would end it \
                 on its first tick"
            );
            assert!(
                wait_until(
                    || {
                        c.pump();
                        ControllerNode(&c).links() == vec![leg.clone()]
                    },
                    5_000
                ),
                "the re-dialled leg links: {:?}",
                ControllerNode(&c).links()
            );
            assert_eq!(ControllerNode(&c).link_end_reason(&leg), None);
            c.teardown_m17();
            ackn.stop();
            refl.stop();
        }

        /// `last_heard` answers from the monitor's heard list for the M17
        /// leg, and `None` for any other link.
        #[test]
        fn last_heard_names_the_far_talker_only_for_the_m17_leg() {
            use crate::test_support::far_talker_packets;
            use astar_m17::{ControlPacket, Reflector, encode_callsign};

            let reflector = Reflector::bind("127.0.0.1:0".parse().expect("loopback"))
                .expect("bind the in-process reflector");
            let addr = reflector.local_addr();
            let handle = reflector.run();
            let c = m17_controller(addr);
            let leg = LinkRef::m17("M17-TEST", 'A');
            ControllerNode(&c)
                .dial_m17_monitor("M17-TEST", 'A', "AJ7HR")
                .expect("a table entry passes the pre-flight");
            assert!(
                wait_until(
                    || {
                        c.pump();
                        !ControllerNode(&c).links().is_empty()
                    },
                    5_000
                ),
                "the monitor links"
            );
            assert_eq!(
                ControllerNode(&c).last_heard(&leg),
                None,
                "nobody has talked yet"
            );

            // A far talker on 127.0.0.1 links to module A and keys up,
            // paced at the real 40 ms per packet.
            let far = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind the far talker");
            let conn = ControlPacket::Conn {
                callsign: encode_callsign("KC1ABC").expect("far callsign"),
                module: b'A',
            };
            far.send_to(&conn.to_bytes(), addr).expect("far CONN");
            assert!(
                wait_until(|| handle.client_count() == 2, 2_000),
                "the far talker links"
            );
            for pkt in far_talker_packets("KC1ABC", 10) {
                far.send_to(&pkt, addr).expect("far stream packet");
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            assert!(
                wait_until(
                    || ControllerNode(&c).last_heard(&leg).map(|(cs, _)| cs)
                        == Some("KC1ABC".to_string()),
                    5_000
                ),
                "last_heard names the far talker on the M17 leg"
            );
            assert_eq!(
                ControllerNode(&c).last_heard(&LinkRef::allstar("1999")),
                None,
                "an AllStar link has no M17 heard list, even while the M17 leg has one"
            );
            c.teardown_m17();
            handle.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Without the feature
// ---------------------------------------------------------------------------

#[cfg(not(feature = "discord"))]
impl NodeController {
    /// The verbs exist in every build so a client gets a clear answer
    /// rather than a 404 that reads like a typo — the same shape as
    /// `handle_discord`'s no-feature arm.
    ///
    /// # Errors
    /// Always: this build has no `discord` feature.
    #[allow(clippy::unused_self)]
    pub(crate) fn handle_discord_admin(
        &self,
        _action: &str,
        _user_id: Option<&str>,
    ) -> Result<NodeReply, NodeError> {
        Err(NodeError {
            message: NO_FEATURE.to_string(),
        })
    }

    /// # Errors
    /// Always: this build has no `discord` feature.
    #[allow(clippy::unused_self)]
    pub(crate) fn handle_discord_state(&self) -> Result<NodeReply, NodeError> {
        Err(NodeError {
            message: NO_FEATURE.to_string(),
        })
    }
}

#[cfg(all(test, not(feature = "discord")))]
mod no_feature_tests {
    use crate::{
        command::{NodeCommand, NodeReply},
        controller::NodeController,
        secrets::SecretProvider,
    };

    fn test_controller() -> NodeController {
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        NodeController::new(station, SecretProvider::new())
    }

    /// Both new verbs exist in a build without the feature and say so,
    /// rather than deserialising fine and then doing nothing — the same
    /// contract `POST /discord` already has.
    #[test]
    fn the_admin_verbs_refuse_without_the_discord_feature() {
        let c = test_controller();
        let admin_err = c
            .execute(NodeCommand::DiscordAdmin {
                action: "approve".into(),
                user_id: Some("444".into()),
            })
            .expect_err("there is no rule engine in this build");
        assert_eq!(
            admin_err.message,
            "discord: this build has no discord feature"
        );

        let state_err = c
            .execute(NodeCommand::DiscordState)
            .expect_err("and nothing to describe");
        assert_eq!(
            state_err.message,
            "discord: this build has no discord feature"
        );

        // And the refusal is the ONLY outcome: a status call still works
        // and still reports no Discord at all.
        let snap = match c.execute(NodeCommand::Status).expect("status always works") {
            NodeReply::Snapshot(s) => s,
            NodeReply::Ok => panic!("status returns a snapshot"),
        };
        assert!(
            snap.discord.is_none(),
            "a build with no discord feature reports no discord object"
        );
    }

    /// `user_id` is optional on the wire: the four node-level actions send
    /// no target at all, and a missing field must not fail deserialisation
    /// before the command ever reaches a handler.
    #[test]
    fn discord_admin_deserialises_without_a_user_id() {
        let c: NodeCommand =
            serde_json::from_str(r#"{"cmd":"discord_admin","action":"enable"}"#).expect("parses");
        assert!(matches!(
            c,
            NodeCommand::DiscordAdmin {
                ref action,
                user_id: None
            } if action == "enable"
        ));
        let with_user: NodeCommand =
            serde_json::from_str(r#"{"cmd":"discord_admin","action":"approve","user_id":"444"}"#)
                .expect("parses");
        assert!(matches!(
            with_user,
            NodeCommand::DiscordAdmin {
                user_id: Some(ref u),
                ..
            } if u == "444"
        ));
        let state: NodeCommand =
            serde_json::from_str(r#"{"cmd":"discord_state"}"#).expect("parses");
        assert!(matches!(state, NodeCommand::DiscordState));

        // A snowflake does not survive a JSON number — 64 bits do not fit in
        // a double — so `user_id` is a string and a number must not quietly
        // coerce into one. This fails at deserialisation, which means the
        // HTTP layer answers 400 through its malformed-body path rather than
        // through the rule engine's `discord: bad request:`.
        assert!(
            serde_json::from_str::<NodeCommand>(
                r#"{"cmd":"discord_admin","action":"approve","user_id":444}"#
            )
            .is_err(),
            "a numeric user_id must not deserialise"
        );
    }
}
