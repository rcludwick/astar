// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The Discord gateway connection, and the runtime it lives on.
//!
//! This file owns the only tokio runtime in the workspace. It runs on a
//! thread of its own and is reached solely through `std::sync::mpsc`, so the
//! blocking `astar-http` control plane never sees an async type.
//!
//! **The gateway outlives any one voice session.** [`Gateway`] is the
//! connection: it owns the runtime thread, the serenity client, the READY
//! gate and the reconnect supervisor, and it stops only when it is dropped.
//! [`Gateway::voice`] hands out a [`SongbirdLink`], a cheap handle that
//! joins and leaves a voice channel over this same connection; dropping one
//! ends a session, never the connection. That split is what lets the bot
//! stay online — answering commands, visible in the member list — while it
//! is in no voice channel at all.
//!
//! ## Listen-only, and what `receive` does and does not change
//!
//! **The channel hears the net; the net never hears the channel.** That has
//! not moved: [`NetLink::send`](crate::NetLink::send) has no production
//! caller, and the conference member this crate enrolls is registered
//! non-contributing. Discord audio is not relayed anywhere because no
//! Discord audio exists.
//!
//! songbird's `receive` feature IS enabled, for one thing: packet TIMING.
//! Discord has no "stopped speaking" event — the speaking opcode fires once,
//! the first time a user transmits — so "the mic holder has gone quiet" can
//! only be inferred from packets not arriving, and that inference needs the
//! driver's 20 ms `VoiceTick`. The rule under which it is enabled is
//! [`voice_config`]: **`DecodeMode::Pass`, so packets are never decrypted
//! and never decoded**. What crosses out of songbird into this file is a set
//! of 32-bit SSRC numbers per tick; what crosses out of this file into the
//! node is [`GatewayEvent::Talking`], two integers and a bool. The
//! ciphertext is never opened, so there is no audio in the process to leak,
//! log, record or relay.
//!
//! **It must fail safe.** Receive is the reverse-engineered half of Discord
//! voice and can stop working without notice — a protocol change, a crypto
//! mode this build does not speak, a driver thread that quietly dies. If
//! talk edges stop arriving the mic must still not stick open, and it does
//! not: the Commander's hard time-out timer (`mic_timeout_secs`, 180 s by
//! default) is a separate clock that depends on none of this and releases
//! the mic regardless. Silence detection shortens that; it is never what
//! keeps it closed.
//!
//! ## Translation, in both directions — and no rules
//!
//! With [`GatewayOptions`] the gateway also carries commands. Inbound,
//! [`Handler`] turns Discord's traffic into the plain values of
//! [`crate::commands`] — a slash command or a button click becomes a
//! `BotCommand`, a voice-state change becomes a `VoiceEvent` — and pushes
//! them down a `std::sync::mpsc` channel. Outbound, [`GatewayHandle`]
//! implements [`GatewayOps`] by enqueuing one [`GatewayCmd`] per call and
//! returning immediately, because the Commander calls those methods on the
//! node's 50 ms pump thread.
//!
//! **Not one rule about who may do what lives in this file**; they are all
//! in [`crate::commander`], where they are tested without a network. The
//! single exception is the guild lock, and it is here because it cannot be
//! anywhere else: a `BotCommand` carries no guild, so an interaction from
//! another server — or from a DM — has to be dropped before it becomes
//! one. See [`is_ours`], which is the first line of every handler below.
//!
//! The interaction TOKEN never leaves this file. It is a fifteen-minute
//! secret that authorises speaking as this bot, it lives inside the
//! serenity value parked in [`PendingMap`], and the only thing that
//! crosses to the node is the interaction's numeric id.
//!
//! ## Panic supervision — what is and is not covered
//!
//! [`Gateway::connect`] wraps `run_runtime`'s entire synchronous body in
//! `catch_unwind`, which catches a panic in the command loop itself and in
//! anything reached via `rt.block_on(...)` from it (join/leave/shutdown).
//! That does **not** cover a panic inside the gateway supervisor task, which
//! runs on a tokio-spawned task (`rt.spawn`, not `block_on`): tokio isolates
//! a spawned task's panic into its `JoinHandle`'s result rather than
//! unwinding into the spawner. `run_runtime` closes that gap itself by
//! polling the supervisor's `JoinHandle` on every pass through the command
//! loop and marking the gateway failed the moment it finds the task gone,
//! whether from a panic or an unexpected clean exit. Every voice handle
//! taken from this gateway shares that one flag, so a handle reports a dead
//! gateway rather than dropping frames into it forever.
//!
//! **The audio TRACK is supervised separately, and it has to be.** The
//! bridge feeds one long-lived songbird track, and a track can die while
//! the gateway, the runtime thread and the voice connection are all
//! perfectly healthy — which is exactly what happened: for the whole life
//! of the first bridge every track died forty milliseconds in with
//! `Errored(Parse(DecodeError("no compatible track found")))`, and nothing
//! noticed. `/status` said `connected: true`, `frames_sent` climbed against
//! a dead track, and the channel heard silence. So every join attaches a
//! [`TrackWatch`] to the track it installs — for `TrackEvent::Error` and
//! `TrackEvent::End`, before the track is handed to the driver — and when
//! the CURRENT track dies (see [`TrackGeneration`] for how a deliberate
//! handover is told apart) it records the reason in [`AudioHealth`], stops
//! the frame feed, and — when there is a Commander to tell — emits
//! [`GatewayEvent::AudioFailed`]. A later join that installs a working
//! track emits [`GatewayEvent::AudioRecovered`], so the outage has an end
//! as well as a beginning. None of this is the `failed` flag, deliberately:
//! the runtime thread is alive and the answer is a rejoin, not a
//! replacement gateway.
//!
//! What remains genuinely uncovered: songbird's own internally-spawned OS
//! threads (the driver's mixer/scheduler thread and its disposal thread —
//! `songbird-0.6.0/src/driver/scheduler/live.rs`,
//! `src/driver/tasks/disposal.rs`). Those are raw `std::thread::spawn` calls
//! owned entirely by songbird's `Call`/`Driver` internals; no handle to them
//! is ever exposed to this module, so a panic there cannot be observed from
//! here at all. Rust's default per-thread panic behaviour (unwind, then that
//! one thread quietly ends) means such a panic will not crash the process,
//! and the track events above are fired BY those threads — so a panic that
//! takes the mixer out stops the audio without firing anything and without
//! setting `failed`. This is a known, intentional limitation of what a
//! caller outside songbird can supervise — not an oversight.

use crate::DiscordError;
use crate::commander::GatewayOps;
use crate::commands::{BotCommand, GatewayEvent, UserRef, VoiceEvent};
use crate::interactions::{
    COMMANDS, OptKind, OptVal, PENDING_TTL, PTT_BUTTON_ID, STATUS_BUTTON_ID, display_name, is_ours,
    parse_command, parse_custom_id, prune_expired,
};
use crate::panel::{PanelAction, PanelState, STATUS_LABEL, panel_view};
use crate::songbird_link::SongbirdLink;
use crate::talk::Talkers;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{
    Receiver as StdReceiver, RecvTimeoutError, Sender as StdSender, SyncSender as StdSyncSender,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the runtime thread may spend on ONE `Join` before it gives up and
/// reports why. Covers waiting for the gateway to reach READY *and* songbird's
/// own connection attempt, so it is a single end-to-end budget rather than a
/// sum of sub-timeouts.
///
/// 10 s is chosen against songbird's own `Config::gateway_timeout`, which also
/// defaults to 10 s: Discord's handshake (identify → READY → voice state /
/// server update → UDP + IP discovery) legitimately takes seconds on a cold
/// start, and anything much shorter would report a local timeout for a join
/// that was still progressing normally.
pub(crate) const JOIN_DEADLINE: Duration = Duration::from_secs(10);

/// The ceiling on any ONE await the command loop performs outside a `Join`:
/// the voice disconnect behind `Leave`, the wait for the last status post to
/// reach Discord, the gateway shutdown behind `Shutdown`, and the reap of the
/// aborted supervisor task.
///
/// These exist so that [`Gateway::drop`] waits on something finite. It
/// joins the runtime thread, and `std::thread::JoinHandle::join` takes no
/// timeout — the only way to bound that wait is for every step the thread
/// might still be executing to be bounded itself. `Join` has its own, longer
/// budget ([`JOIN_DEADLINE`]); everything else gets this one.
///
/// 5 s matches `rt.shutdown_timeout` below. Exceeding it means Discord or
/// the network has stopped answering, at which point abandoning the polite
/// disconnect and tearing the runtime down is strictly better than waiting:
/// the server drops the session on its own once heartbeats stop.
pub(crate) const OP_TIMEOUT: Duration = Duration::from_secs(5);

/// The songbird configuration this crate uses. **The only
/// `songbird::Config` constructed anywhere in it.**
///
/// `DecodeMode::Pass` and nothing else, and that is not a tuning knob.
///
/// * **`Pass`** hands received packets to events with nothing applied:
///   songbird's own `should_decrypt()` and `should_decode()` are both false
///   for it, the cipher is never invoked, and no Opus decoder ever runs.
///   What this crate learns from a tick is which SSRCs produced a packet in
///   the last 20 ms — the timing, and only the timing. That is enough to
///   unkey a silent mic, and it is the entire reason `receive` is enabled.
/// * **`Decrypt`** would open every packet, and **`Decode`** would turn it
///   into PCM. Either one puts other people's Discord audio inside a
///   process whose whole safety story is that it never holds any: this node
///   bridges a ham-radio conference, the conference is a transmitter's
///   input, and "the bridge cannot relay what it does not have" stops being
///   true the moment it has it. Nothing here would relay it — but the
///   guarantee rests on the code being unable to, not on it choosing not
///   to.
///
/// So changing this line is a **design decision for the owner, not a
/// refactor**, and it is not one a later feature can take by itself. If
/// something one day genuinely needs Discord audio, that is a conversation
/// about what this bridge is, with the licence, the privacy statement and
/// the "does not transmit" line in `bridge.rs` all in scope.
///
/// songbird's default is `DecodeMode::Decrypt`, so this is a value that has
/// to be set rather than a default that has to be left alone — which is why
/// it is built in one place and asserted in a test.
pub(crate) fn voice_config() -> songbird::Config {
    songbird::Config::default().decode_mode(songbird::driver::DecodeMode::Pass)
}

/// One voice session's talk-edge state, and the channel its edges leave by.
///
/// The sender lives INSIDE the mutex because `std::sync::mpsc::Sender` is
/// `Send` but not `Sync`, and songbird requires an `EventHandler` to be
/// both. Sending while the lock is held costs nothing and is deliberate:
/// the channel is unbounded, so a send never blocks, and holding the lock
/// across it keeps the edges in the order the ticks produced them.
struct TalkState {
    talkers: Talkers,
    events: StdSender<GatewayEvent>,
}

impl TalkState {
    /// Hand a batch of edges to the node. A failed send means the node has
    /// gone; there is nothing to do about it and nothing worth logging per
    /// tick.
    fn emit(&self, edges: Vec<(u64, bool)>) {
        for (user, talking) in edges {
            let _ = self.events.send(GatewayEvent::Talking { user, talking });
        }
    }
}

/// One voice session's view of who is talking: songbird's event handler at
/// one end, the command loop's session bookkeeping at the other.
///
/// Cloned once per registration and once more to be held by the command
/// loop; every clone shares the one state.
///
/// **Only created when the gateway has somewhere to send events.** An
/// audio-only gateway (`Gateway::connect`, no [`GatewayOptions`]) registers
/// no receive handlers at all — there is no Commander listening, so there
/// is no work worth doing on songbird's driver task.
#[derive(Clone)]
struct TalkSession(Arc<Mutex<TalkState>>);

impl TalkSession {
    fn new(events: StdSender<GatewayEvent>) -> Self {
        Self(Arc::new(Mutex::new(TalkState {
            talkers: Talkers::default(),
            events,
        })))
    }

    /// End this session: tell the node that anyone still marked talking has
    /// stopped, and stop reporting anything further.
    ///
    /// The Commander's hard time-out timer would eventually rescue a holder
    /// left marked mid-word, but that backstop is for the path where
    /// receive has failed. A leave is the normal path, and on it the node
    /// must be told the truth rather than left to time out.
    fn end(&self) -> Vec<u64> {
        self.stop_talkers(Talkers::end)
    }

    /// The voice connection dropped without a `Leave`: tell the node that
    /// anyone still marked talking has stopped, and clear the stream state
    /// so a reconnect starts from nothing.
    ///
    /// **Not an [`Self::end`], deliberately.** songbird retries a dropped
    /// driver itself (`Config::driver_retry`), so the session is expected
    /// back and its handlers must keep working; what must not survive is
    /// the claim that somebody is mid-word, because no further tick can
    /// ever end their spurt. Without this the holder stayed latched
    /// `talking = true`, the silence unkey never fired, and the mic stayed
    /// open until the 180 s hard timer.
    fn interrupt(&self) -> Vec<u64> {
        self.stop_talkers(Talkers::interrupt)
    }

    /// The one implementation behind both: perform `stop`, then tell the
    /// node that everyone it named has stopped talking. Returns them, so a
    /// caller can say whether anything was actually interrupted.
    ///
    /// Shared rather than written twice because the two differ in exactly
    /// one thing — whether the session is over afterwards — and a second
    /// copy of "map to `false`, emit under the lock" is a second place for
    /// the falling edges to go missing.
    fn stop_talkers(&self, stop: fn(&mut Talkers) -> Vec<u64>) -> Vec<u64> {
        let mut state = self.0.lock().expect("talk state mutex poisoned");
        let stopped = stop(&mut state.talkers);
        state.emit(stopped.iter().map(|&u| (u, false)).collect());
        stopped
    }
}

/// Talk edges, from packet timing and nothing else.
///
/// All three arms run on songbird's driver event task, which also drives
/// the 20 ms tick, so everything here is cheap and nothing here blocks: one
/// `HashSet` of `u32`, one `std::sync::Mutex` held for the length of an
/// integer update, and an unbounded `std::sync::mpsc` send. There is no
/// await inside the lock because there is no await at all. The disconnect
/// arm is the rarest and still the cheapest: it drops two small maps.
#[serenity::async_trait]
impl songbird::EventHandler for TalkSession {
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        match ctx {
            songbird::EventContext::SpeakingStateUpdate(speaking) => {
                // The one thing this event is used for: SSRC → user. The
                // opcode's own "speaking" bitfield is NOT read — it fires
                // once, when a user first transmits, and says nothing about
                // when they stop. An update with no user id names nobody
                // and is dropped.
                let user = speaking.user_id?;
                let mut state = self.0.lock().expect("talk state mutex poisoned");
                let edges = state.talkers.map_ssrc(speaking.ssrc, user.0);
                state.emit(edges);
            }
            songbird::EventContext::VoiceTick(tick) => {
                // KEYS ONLY, and visibly so: the SSRCs are collected on the
                // first line and the map's VALUES are never named again.
                // `VoiceData`'s payload fields — `packet` and
                // `decoded_voice` — are not read here, not moved, not
                // logged and not mentioned anywhere else in this crate. In
                // `DecodeMode::Pass` `decoded_voice` is always `None` and
                // `packet` is ciphertext this process has no key applied
                // to, but the reason this handler does not touch them is
                // the stronger one: there is no legitimate use for them
                // here, and the listen-only guarantee should not depend on
                // what a mode setting happens to leave empty.
                let speaking: HashSet<u32> = tick.speaking.keys().copied().collect();
                let mut state = self.0.lock().expect("talk state mutex poisoned");
                let edges = state.talkers.tick(&speaking, &tick.silent);
                state.emit(edges);
            }
            songbird::EventContext::DriverDisconnect(_) => {
                // The driver dropped: kicked from the channel, a network
                // fault, or a leave this node asked for. Whatever the
                // cause, no further tick can end a spurt that is in
                // progress, so everyone still marked talking is reported
                // stopped and the stream bookkeeping is thrown away.
                //
                // Nothing is read out of `DisconnectData`. The reason and
                // the kind would only decide how to WORD a log line, and
                // this handler runs on the driver task: songbird retries
                // on its own (`Config::driver_retry`), the session is not
                // torn down here, and the node is not told the link
                // failed. All that is owed is an honest talk state.
                let stopped = self.interrupt();
                if !stopped.is_empty() {
                    eprintln!(
                        "astar-discord: the voice connection dropped while {n} \
                         user(s) were talking; they are now reported stopped",
                        n = stopped.len()
                    );
                }
            }
            // `EventContext` is `#[non_exhaustive]`, and this handler is
            // registered for exactly three events. Anything else is not
            // ours.
            _ => {}
        }
        // `None` keeps the handler registered for the next event; returning
        // an `Event` here would re-schedule it as a one-shot.
        None
    }
}

/// Which track the bridge is feeding RIGHT NOW, as a number that only goes
/// up.
///
/// **This exists to tell a failure from a handover.** The audio path is one
/// long-lived songbird track, and the events that say it is over —
/// `TrackEvent::End` and `TrackEvent::Error` — cannot say why on their own.
/// A rejoin ends the previous track (`play_only_input` stops it), and so
/// does a `Leave`; reporting either as an outage would announce a dead
/// audio feed into the channel the bridge had just successfully joined. So
/// every handler is tagged with the generation it was registered for, and
/// acts only while that generation is still current. A deliberate stop
/// bumps the number BEFORE the track is stopped, which retires its handler
/// ahead of the event it is about to fire.
///
/// Cheap and `Clone`: the counter is one `AtomicU64` behind an `Arc`, shared
/// with every handler. `Relaxed` is enough because nothing else is ordered
/// against it — the only question ever asked is whether one `u64` equals
/// another, and the compare-exchange that answers it is atomic by itself.
#[derive(Clone, Debug, Default)]
pub(crate) struct TrackGeneration(Arc<AtomicU64>);

impl TrackGeneration {
    /// Retire whatever track is current and name the next one.
    ///
    /// Never returns the starting value, so a fresh `TrackGeneration` —
    /// one no join has played against — matches no handler at all.
    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Whether an event tagged `generation` is about the track being fed
    /// now.
    ///
    /// An observation, not a decision — production asks
    /// [`Self::claim_failure`], which answers the same question and takes
    /// the generation in one atomic step. This exists so that a test can
    /// state the property directly rather than inferring it from a claim
    /// that has already consumed it.
    #[cfg(test)]
    fn is_current(&self, generation: u64) -> bool {
        self.0.load(Ordering::Relaxed) == generation
    }

    /// Claim the death of `generation`: true for the FIRST event about the
    /// track that is current, false for every other event.
    ///
    /// Both halves matter. A superseded generation is a deliberate stop and
    /// must be ignored. And one death arrives as two events — songbird
    /// fires `TrackEvent::Error` and then `TrackEvent::End` for a track
    /// that errored (`songbird-0.6.0/src/tracks/mode.rs:50-69`) — so the
    /// claim retires the generation as it takes it, and the second event
    /// finds nothing left to report.
    fn claim_failure(&self, generation: u64) -> bool {
        self.0
            .compare_exchange(
                generation,
                generation + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
    }
}

/// Whether the audio track into Discord is dead, and why — shared between
/// the runtime thread that learns it and every [`SongbirdLink`] that has to
/// report it.
///
/// **Deliberately NOT the `failed` flag.** `failed` means the runtime
/// thread has died, and the controller answers it by replacing the whole
/// gateway; a dead track is a live gateway with a broken audio path, and
/// the answer to it is a rejoin. Conflating them would take the bot offline
/// for a fault that a rejoin fixes.
///
/// The reason is songbird's own words, carries no credential, and is for
/// the operator: it reaches `/status` through `Bridge::last_error` and the
/// admin view, never a public channel.
#[derive(Clone, Debug, Default)]
pub(crate) struct AudioHealth(Arc<Mutex<Option<String>>>);

impl AudioHealth {
    /// Record why the audio stopped. The FIRST reason wins until a join
    /// clears it: what arrives afterwards is a consequence — the `End` that
    /// follows an `Error`, the next failed rejoin — and the cause is what
    /// the operator needs.
    pub(crate) fn fail(&self, reason: String) {
        let mut slot = self.0.lock().expect("audio health mutex poisoned");
        if slot.is_none() {
            *slot = Some(reason);
        }
    }

    /// Forget any failure. Called by a successful join, which has just
    /// installed a new track: the old one's death has stopped being the
    /// truth about this bridge.
    pub(crate) fn clear(&self) {
        *self.0.lock().expect("audio health mutex poisoned") = None;
    }

    /// Why the audio stopped, if it has.
    pub(crate) fn reason(&self) -> Option<String> {
        self.0.lock().expect("audio health mutex poisoned").clone()
    }
}

/// Why a track event means the audio is over, in songbird's words.
///
/// An errored track carries its `PlayError`, whose `Display` names the
/// stage and the underlying error — that is exactly the line that would
/// have saved the first bridge, which died of
/// `Parse(DecodeError("no compatible track found"))` forty milliseconds
/// into every session while `/status` said everything was fine.
///
/// **Only the `PlayMode` is read.** The event context is not `Debug`-printed
/// and the `TrackHandle` beside it is not touched: a whole songbird event
/// rendered into a log is how something that was never meant to be printed
/// gets printed.
///
/// A track that merely ENDED has no error to quote, and the global-event
/// shape of `EventContext::Track` carries no tracks at all; both get the
/// same fixed sentence.
fn track_failure_reason<'a>(
    states: impl IntoIterator<Item = &'a songbird::tracks::TrackState>,
) -> String {
    for state in states {
        if let songbird::tracks::PlayMode::Errored(e) = &state.playing {
            return e.to_string();
        }
    }
    "the audio track ended".to_string()
}

/// Everything one voice session needs in order to notice that its audio
/// track has died.
///
/// Cloned into each join, which mints the handler for the track it is about
/// to install.
#[derive(Clone)]
struct TrackSupervisor {
    tracks: TrackGeneration,
    health: AudioHealth,
    /// Where `AudioFailed` goes, when there is a Commander to tell. `None`
    /// for an audio-only gateway — which still records the failure, because
    /// [`SongbirdLink::play`] reports it and `/status` shows it.
    ///
    /// The sender is inside a `Mutex` for the same reason [`TalkState`]'s
    /// is: `std::sync::mpsc::Sender` is `Send` and not `Sync`, and songbird
    /// requires an `EventHandler` to be both.
    events: Option<Arc<Mutex<StdSender<GatewayEvent>>>>,
}

impl TrackSupervisor {
    fn new(events: Option<StdSender<GatewayEvent>>) -> Self {
        Self {
            tracks: TrackGeneration::default(),
            health: AudioHealth::default(),
            events: events.map(|tx| Arc::new(Mutex::new(tx))),
        }
    }

    /// A new track is playing: forget the old one's death, and report
    /// whether there WAS one.
    ///
    /// The return is what makes recovery announceable. Every successful
    /// join clears the flag, but only a join that cleared something has
    /// anything to say — otherwise the channel would be told "the audio
    /// feed is back" the first time the bridge ever joined it.
    fn recover(&self) -> bool {
        let had_failed = self.health.reason().is_some();
        self.health.clear();
        had_failed
    }

    /// Retire the track being fed now and produce the handler for the one
    /// about to be installed.
    ///
    /// The retirement happens HERE, on the line before the install,
    /// because installing is what stops the previous track: the `End` it
    /// fires must find its own generation already superseded.
    ///
    /// **Call this only where a track really is about to replace another.**
    /// It was once called when a join was REQUESTED, which retired the
    /// watch on the track that was still playing — so a join that failed
    /// early (never READY, out of time, a refusal) left the previous
    /// session feeding the channel with nothing watching it, and
    /// `apply_join_outcome` returns through `outcome.result?` without
    /// clearing `pcm_tx`, so the frames kept going. The later death of that
    /// track was silent. Same bug class as the one this whole type exists
    /// to fix: a generation that moves when no track moved.
    fn watch_next(&self) -> TrackWatch {
        TrackWatch {
            generation: self.tracks.next(),
            supervisor: self.clone(),
        }
    }
}

/// The handler that notices ONE track dying.
///
/// Attached to the `Track`'s own event store for `TrackEvent::Error` and
/// `TrackEvent::End` BEFORE the track is handed to `play_only`, so the
/// handler is part of the track from the moment it exists rather than being
/// registered through a handle afterwards. That ordering is what makes it
/// survive songbird's own rebuilds of the track (see `add_event`'s limit,
/// recorded on the backlog).
#[derive(Clone)]
struct TrackWatch {
    /// The track this handler belongs to. It acts only while this is the
    /// current generation — see [`TrackGeneration`].
    generation: u64,
    supervisor: TrackSupervisor,
}

impl TrackWatch {
    /// Report that this track has stopped, for `reason` — or do nothing,
    /// because this is not news.
    ///
    /// Everything [`songbird::EventHandler::act`] does once it has the
    /// reason, kept apart from songbird so that it can be driven by a test:
    /// `EventContext::Track` carries a `&TrackHandle`, and a `TrackHandle`
    /// cannot be built outside songbird, so `act` itself is reachable only
    /// from a live driver.
    ///
    /// Returns whether it was reported, which is how the caller knows this
    /// event was the death of the CURRENT track rather than the echo of one
    /// this node stopped on purpose.
    fn report(&self, reason: String) -> bool {
        // A superseded generation is a track this node stopped on purpose,
        // and a second event on the current one is the same death arriving
        // twice. Either way there is nothing to report.
        if !self.supervisor.tracks.claim_failure(self.generation) {
            return false;
        }
        // One line, once per track. This is the line whose absence let the
        // first bridge run for its whole life against a track that had died
        // forty milliseconds in.
        eprintln!("astar-discord: the audio track into Discord stopped: {reason}");
        self.supervisor.health.fail(reason.clone());
        if let Some(events) = &self.supervisor.events {
            let _ = events
                .lock()
                .expect("track events mutex poisoned")
                .send(GatewayEvent::AudioFailed { reason });
        }
        true
    }
}

/// The audio track into Discord has stopped.
///
/// Runs on songbird's driver event task, so it is cheap and never blocks:
/// one compare-exchange, one short `std::sync::Mutex`, one `eprintln!` and
/// one unbounded `std::sync::mpsc` send. No await, so no lock is held
/// across one.
#[serenity::async_trait]
impl songbird::EventHandler for TrackWatch {
    async fn act(&self, ctx: &songbird::EventContext<'_>) -> Option<songbird::Event> {
        let songbird::EventContext::Track(states) = ctx else {
            return None;
        };
        // Only the `PlayMode` is read out of the context, and the
        // `TrackHandle` beside it is not touched: see
        // [`track_failure_reason`].
        self.report(track_failure_reason(states.iter().map(|&(state, _)| state)));
        // `None` keeps the handler registered; there is nothing left for it
        // to report, and the claim inside `report` is what guarantees that.
        None
    }
}

/// Commands crossing from the node's threads into the tokio runtime.
///
/// One channel serves the whole gateway: the [`Gateway`] holds the sending
/// end and every [`SongbirdLink`] it hands out holds a clone of it.
pub(crate) enum GatewayCmd {
    Join {
        guild: u64,
        channel: u64,
        /// Where the runtime thread posts the REAL outcome. Bounded at one
        /// slot and never awaited by the runtime thread, so a caller that has
        /// already given up cannot block it.
        reply: StdSyncSender<Result<(), DiscordError>>,
    },
    /// Leave the voice channel.
    ///
    /// Carries a reply channel exactly as `Join` does, and for the same
    /// reason: `Songbird::remove` can fail or time out, and the
    /// "disconnected" line the caller posts afterwards must report what the
    /// leave DID, not merely that one was asked for.
    ///
    /// A handle's own `Drop` sends this too, and drops the receiving end
    /// immediately — nobody is there to be told. That is why the command
    /// loop answers with `try_send`: the reply is an offer, never an
    /// obligation, and an unheard answer must not block the loop.
    Leave {
        reply: StdSyncSender<Result<(), DiscordError>>,
    },
    Frame(Vec<i16>),
    /// Post one status line into a channel's built-in text chat. Carries the
    /// channel explicitly so a FAILED join — which never reaches
    /// `handle_join`'s success path — can still say where it failed.
    PostText {
        channel: u64,
        text: String,
    },
    /// Answer one deferred interaction, ephemerally.
    ///
    /// Only the interaction's numeric id crosses this channel. The token
    /// that actually authorises the answer never leaves the runtime
    /// thread: it sits inside the interaction parked in the pending map,
    /// which the runtime thread owns. See [`PendingInteraction`].
    Reply {
        interaction: u64,
        text: String,
    },
    /// Server-mute or unmute one member of the configured guild.
    ///
    /// No guild field: the guild is the one this gateway was configured
    /// with, held by the runtime thread. A handle cannot name a different
    /// one, which is the same guild lock the inbound side applies, viewed
    /// from the outbound side.
    SetMute {
        user: u64,
        mute: bool,
    },
    /// Redraw the PTT panel in `channel` for the given mic holder (or for
    /// nobody). Coalesced — see [`crate::panel`].
    Panel {
        channel: u64,
        talking: Option<String>,
    },
    Shutdown,
}

/// What a gateway needs in order to do anything but carry audio.
///
/// Absent (`Gateway::connect`, or `connect_with(token, None)`) the bot is
/// the bridge and nothing else: it joins, it plays, it posts status lines.
/// Present, it also registers slash commands, reports voice state, and
/// turns interactions into [`GatewayEvent`]s on `events`.
#[derive(Debug, Clone)]
pub struct GatewayOptions {
    /// The one guild this bot answers. Everything from anywhere else —
    /// every other guild, and every DM — is dropped without a reply.
    pub guild: u64,
    /// Whether to register the slash commands on READY. When this is
    /// false the registration is an explicit EMPTY list rather than no
    /// call at all, so a node that stops offering commands takes its stale
    /// ones down with it.
    pub commands: bool,
    /// Where gateway events are delivered. The node drains this on its own
    /// thread; a send that fails means the node has gone and is ignored.
    pub events: StdSender<GatewayEvent>,
}

/// The reply a caller gets when their command is a shape this bot never
/// registered. Emitting nothing is the other half: the Commander is never
/// asked to rule on something nobody designed.
const UNPARSEABLE: &str = "I didn't understand that command.";

/// How long between checks for the panel having scrolled out of sight.
///
/// The check is a `GET` of the channel's newest messages, and it happens
/// only when the panel is being redrawn anyway — so this is a ceiling on a
/// rate that is already low. A minute is far shorter than the ~20 messages
/// it takes to bury the panel in a conversational channel, and far longer
/// than a handover storm.
const PANEL_SCAN_INTERVAL: Duration = Duration::from_secs(60);

/// A live Discord gateway connection on its own supervised runtime thread.
///
/// The bot is online for exactly as long as this value exists. It is not a
/// voice session: [`Self::voice`] hands out the handles that join and leave
/// channels, and any number of them may come and go underneath one
/// `Gateway`. Dropping THIS is what takes the bot offline.
pub struct Gateway {
    tx: StdSender<GatewayCmd>,
    /// Set by the runtime thread when it dies. Shared with every handle
    /// [`Self::voice`] has produced, so a `play` on a stale handle reports a
    /// failed gateway instead of silently dropping frames forever.
    failed: Arc<AtomicBool>,
    /// Why the audio track into Discord stopped, if it has. Shared the same
    /// way and for a related reason — a handle must be able to report a
    /// dead audio path — but it is a DIFFERENT fault from `failed`: the
    /// runtime thread is alive and the answer is a rejoin, not a new
    /// gateway. See [`AudioHealth`].
    audio: AudioHealth,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Gateway {
    /// Start the runtime thread and log the bot in.
    ///
    /// `token` is used here and never stored on this struct, never logged and
    /// never returned in an error.
    ///
    /// # Errors
    /// [`DiscordError::NotConfigured`] when `token` is empty — an unset token
    /// is a configuration answer, not a connection attempt, and nothing is
    /// spawned. [`DiscordError::Voice`] when the thread cannot be spawned.
    pub fn connect(token: &str) -> Result<Self, DiscordError> {
        Self::connect_with(token, None)
    }

    /// The same, plus everything a commanding gateway needs: the guild it
    /// is locked to, whether to register the slash commands, and where to
    /// deliver [`GatewayEvent`]s.
    ///
    /// `connect(t)` is exactly `connect_with(t, None)`, which is the
    /// audio-only bridge this crate shipped first.
    ///
    /// `token` is used here and never stored on this struct, never logged
    /// and never returned in an error.
    ///
    /// # Errors
    /// [`DiscordError::NotConfigured`] when `token` is empty — an unset
    /// token is a configuration answer, not a connection attempt, and
    /// nothing is spawned. [`DiscordError::Voice`] when the thread cannot
    /// be spawned.
    pub fn connect_with(token: &str, opts: Option<GatewayOptions>) -> Result<Self, DiscordError> {
        if token.is_empty() {
            return Err(DiscordError::NotConfigured);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&failed);
        let audio = AudioHealth::default();
        let track_health = audio.clone();
        let token = token.to_string();

        let handle = std::thread::Builder::new()
            .name("astar-discord".into())
            .spawn(move || {
                // A panic anywhere in songbird marks the gateway failed and
                // ends the thread; it must never unwind into the node.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_runtime(&token, &rx, &flag, &track_health, opts);
                }));
                drop(token);
                if result.is_err() {
                    flag.store(true, Ordering::Relaxed);
                }
            })
            .map_err(|e| DiscordError::Voice(format!("cannot spawn runtime thread: {e}")))?;

        Ok(Self {
            tx,
            failed,
            audio,
            handle: Some(handle),
        })
    }

    /// True once the runtime thread has died — a panic caught by
    /// `connect`'s `catch_unwind`, or the gateway supervisor task found gone
    /// by `run_runtime`'s poll.
    ///
    /// A `Gateway` that reports this can do nothing at all: every send from
    /// every handle it has produced already fails, and no reconnect will
    /// happen because the thread that would do the reconnecting is the one
    /// that died. **Existing is therefore not the same as working**, and a
    /// caller holding one in an `Option` must ask this before deciding it
    /// has a gateway — otherwise a single panic takes the bot offline until
    /// the daemon restarts, which matters exactly when the gateway is meant
    /// to be persistent.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// A `Gateway` with no runtime thread behind it, for tests in this crate
    /// and in `astar-server`.
    ///
    /// Compiled only under `cfg(test)` or the `test-support` feature, which
    /// nothing in a shipped build enables — the same shape as
    /// `astar-audio`'s `test-backend`. It spawns no thread, reaches no
    /// network, and can do nothing: its command receiver is dropped before
    /// it returns, so every send fails, and its `Drop` has no thread to
    /// join. What it is for is standing in as "a gateway is installed" in a
    /// controller test, which is otherwise unreachable without
    /// [`Self::connect`] and a real token.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn orphan() -> Self {
        Self::orphan_with(false)
    }

    /// The same, but already reporting [`Self::is_failed`] — a gateway whose
    /// runtime thread has died.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn orphan_failed() -> Self {
        Self::orphan_with(true)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn orphan_with(failed: bool) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        // Dropped here on purpose: there is no thread to read the channel,
        // and a live receiver would make sends look like they had been
        // delivered somewhere.
        drop(rx);
        Self {
            tx,
            failed: Arc::new(AtomicBool::new(failed)),
            audio: AudioHealth::default(),
            handle: None,
        }
    }

    /// A voice handle onto this gateway.
    ///
    /// Cheap — a channel sender and a flag, both clones — so a caller may
    /// take one per voice session without thinking about cost. **Dropping
    /// the handle does not stop the gateway**: it asks, best effort, to leave
    /// whatever channel it joined (see `SongbirdLink`'s `Drop`) and nothing
    /// more. The bot stays online until this `Gateway` is dropped.
    #[must_use]
    pub fn voice(&self) -> SongbirdLink {
        SongbirdLink::new(
            self.tx.clone(),
            Arc::clone(&self.failed),
            self.audio.clone(),
        )
    }

    /// A [`GatewayOps`] onto this gateway, aimed at one text/voice channel.
    ///
    /// `channel` is where public lines and the PTT panel go: the bridged
    /// voice channel's own built-in text chat, which is the same id the
    /// bridge joins.
    ///
    /// Cheap and disposable like [`Self::voice`] — a sender clone and a
    /// number. Holding one keeps nothing alive.
    #[must_use]
    pub fn ops(&self, channel: u64) -> GatewayHandle {
        GatewayHandle {
            tx: self.tx.clone(),
            channel,
        }
    }
}

/// The Commander's end of the gateway: every op is an enqueue and nothing
/// else.
///
/// **Why this may not do any work of its own.** Every method below is
/// called synchronously from inside the Commander's rule evaluation, which
/// runs on the node's 50 ms pump thread. A REST round trip there would
/// stall the pump, and behind the pump is the audio path. So each method
/// puts one `GatewayCmd` on an unbounded queue and returns; the runtime
/// thread spawns the actual Discord call, exactly as `post_status_text`
/// does and for exactly the reason given in its docs.
///
/// A send that fails means the gateway's runtime thread has gone. There is
/// nothing useful to say about that here — the node learns it from
/// [`Gateway::is_failed`] — so it is dropped rather than logged once per
/// mute in a sweep.
pub struct GatewayHandle {
    tx: StdSender<GatewayCmd>,
    channel: u64,
}

impl GatewayHandle {
    fn send(&self, cmd: GatewayCmd) {
        let _ = self.tx.send(cmd);
    }
}

impl GatewayOps for GatewayHandle {
    fn reply(&mut self, interaction: u64, text: &str) {
        self.send(GatewayCmd::Reply {
            interaction,
            text: text.to_string(),
        });
    }

    fn announce(&mut self, text: &str) {
        self.send(GatewayCmd::PostText {
            channel: self.channel,
            text: text.to_string(),
        });
    }

    fn set_mute(&mut self, user: u64, mute: bool) {
        self.send(GatewayCmd::SetMute { user, mute });
    }

    fn panel(&mut self, talking: Option<&str>) {
        self.send(GatewayCmd::Panel {
            channel: self.channel,
            talking: talking.map(ToString::to_string),
        });
    }
}

impl Drop for Gateway {
    /// Ask the runtime thread to stop, then wait for it.
    ///
    /// **What this waits on, and what it does not.**
    /// `std::thread::JoinHandle::join` takes no timeout and cannot be given
    /// one, so the only way to bound this wait is for every step the runtime
    /// thread might still be executing to be bounded itself. As long as that
    /// thread is running its command loop, they all are: the command poll is
    /// 200 ms, a `Join` is [`JOIN_DEADLINE`], `Leave`/the last status post/
    /// the outstanding server mutes (one budget for all of them, see
    /// [`InFlight`])/`Shutdown`/the supervisor reap are each [`OP_TIMEOUT`],
    /// and the runtime teardown is `shutdown_timeout`'s 5 s. The `Shutdown`
    /// below therefore
    /// queues behind at most one in-flight command, and the whole of this
    /// `drop` completes inside roughly 30 s worst case — the common case is
    /// milliseconds.
    ///
    /// **It is therefore never run under a lock.** The controller takes the
    /// `Gateway` out of its mutex into a local and lets the guard go before
    /// dropping it, because the node's 50 ms pump must never queue behind
    /// this.
    ///
    /// What is **not** guaranteed: if the runtime thread were ever wedged
    /// inside a `block_on` that never returns, it would never read this
    /// `Shutdown` at all, and this `join` would block forever. Bounding the
    /// teardown cannot help there — a wedged thread never reaches the
    /// teardown. The only defence is that nothing it runs is unbounded, which
    /// is why `join_and_play` keeps every await it performs, down to the
    /// `Call` lock, inside the join deadline. songbird's own
    /// internally-spawned threads stay outside this crate's reach entirely
    /// (see the module docs).
    fn drop(&mut self) {
        let _ = self.tx.send(GatewayCmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// One deferred interaction, waiting for the Commander's answer.
///
/// **This is where the interaction token lives, and it lives nowhere
/// else.** The token is a 15-minute secret that authorises anyone holding
/// it to speak as this bot in that conversation, and it is a field of the
/// serenity value below. That value never leaves the runtime thread: the
/// only thing that crosses into the node is the interaction's numeric id,
/// which is public and authorises nothing. [`GatewayCmd::Reply`] carries
/// that id back, and the answer is found here.
///
/// Both interaction kinds are held because the panel's buttons produce
/// component interactions and `/mic` produces a command interaction, and
/// the Commander answers both through the same `reply`.
enum PendingInteraction {
    Command(Box<serenity::model::application::CommandInteraction>),
    Component(Box<serenity::model::application::ComponentInteraction>),
}

/// The deferred interactions this gateway may still answer, by id.
type PendingMap = HashMap<u64, (Instant, PendingInteraction)>;

impl PendingInteraction {
    fn id(&self) -> u64 {
        match self {
            Self::Command(c) => c.id.get(),
            Self::Component(c) => c.id.get(),
        }
    }

    /// The guild the interaction came from, or `None` for a DM.
    fn guild(&self) -> Option<u64> {
        match self {
            Self::Command(c) => c.guild_id.map(serenity::model::id::GuildId::get),
            Self::Component(c) => c.guild_id.map(serenity::model::id::GuildId::get),
        }
    }

    /// Who invoked it, with the best display name available: the guild
    /// nickname if this came from a guild, then the account's display
    /// name, then the username.
    fn user(&self) -> UserRef {
        let (user, nick) = match self {
            Self::Command(c) => (&c.user, c.member.as_ref().and_then(|m| m.nick.as_deref())),
            Self::Component(c) => (&c.user, c.member.as_ref().and_then(|m| m.nick.as_deref())),
        };
        UserRef {
            id: user.id.get(),
            name: display_name(nick, user.global_name.as_deref(), &user.name),
        }
    }

    /// What was asked for, or `None` for a shape this bot never
    /// registered.
    fn parse(&self) -> Option<crate::commands::BotCommandKind> {
        match self {
            Self::Command(c) => parse_command(&c.data.name, &to_opt_vals(&c.data.options())),
            Self::Component(c) => parse_custom_id(&c.data.custom_id),
        }
    }

    /// Acknowledge, ephemerally, before anything is parsed or decided.
    ///
    /// Discord gives an interaction three seconds to be acknowledged, and
    /// the ephemeral flag has to be chosen at that moment — long before
    /// the node has decided whether the caller may do what they asked. So
    /// everything is deferred ephemerally and the real answer is an edit
    /// of this.
    async fn defer(&self, http: &Arc<serenity::http::Http>) {
        let outcome = match self {
            Self::Command(c) => c.defer_ephemeral(http).await,
            Self::Component(c) => c.defer_ephemeral(http).await,
        };
        if let Err(e) = outcome {
            // Fixed text plus a status code, never the error itself: this
            // request's URL carries the interaction token in its path.
            // serenity's `Display` does not print a URL —
            // `HttpError::Request`'s is the fixed string "Error while
            // sending HTTP request." (`serenity-0.12.5/src/http/error.rs`)
            // — but a log line about a live secret is not the place to rely
            // on someone else's formatter.
            eprintln!(
                "astar-discord: could not acknowledge an interaction{}",
                status_suffix(&e)
            );
        }
    }

    /// Replace the "thinking…" placeholder with the answer.
    async fn answer(&self, http: &Arc<serenity::http::Http>, text: String) {
        let builder = serenity::builder::EditInteractionResponse::new()
            .content(text)
            // Refusals and status lines both mention users by id. Empty
            // allowed-mentions renders `<@id>` as a name and pings nobody.
            .allowed_mentions(serenity::builder::CreateAllowedMentions::new());
        let outcome = match self {
            Self::Command(c) => c.edit_response(http, builder).await,
            Self::Component(c) => c.edit_response(http, builder).await,
        };
        if let Err(e) = outcome {
            // Fixed text plus status, for the same reason as `defer`.
            eprintln!(
                "astar-discord: could not answer an interaction{}",
                status_suffix(&e)
            );
        }
    }
}

/// The HTTP status behind a serenity error, when there was one.
///
/// The status is the only part of a failed REST call this module ever acts
/// on — a 403 is a permission the operator has not granted and will not
/// grant by being asked again; a 404 is a message somebody deleted.
fn http_status(e: &serenity::Error) -> Option<u16> {
    match e {
        serenity::Error::Http(h) => h.status_code().map(|s| s.as_u16()),
        _ => None,
    }
}

/// `" (HTTP 403)"`; `" (no application id)"` for the one status-less
/// failure that is this module's own doing; nothing for the rest.
///
/// Deliberately not the error's own text: see [`PendingInteraction::defer`].
/// The application-id case is spelled out because it is not a network
/// condition and does not clear itself — it means the REST client answering
/// was never told who it is (see [`Handler::ready`]), and every answer will
/// fail the same way until that is fixed.
fn status_suffix(e: &serenity::Error) -> String {
    if matches!(
        e,
        serenity::Error::Http(serenity::http::HttpError::ApplicationIdMissing)
    ) {
        return " (no application id)".to_string();
    }
    http_status(e).map_or_else(String::new, |s| format!(" (HTTP {s})"))
}

/// Convert serenity's resolved options into the plain tree
/// [`parse_command`] reads.
fn to_opt_vals(opts: &[serenity::model::application::ResolvedOption<'_>]) -> Vec<(String, OptVal)> {
    opts.iter()
        .map(|o| (o.name.to_string(), to_opt_val(&o.value)))
        .collect()
}

fn to_opt_val(v: &serenity::model::application::ResolvedValue<'_>) -> OptVal {
    use serenity::model::application::ResolvedValue;
    match v {
        ResolvedValue::String(s) => OptVal::Str((*s).to_string()),
        ResolvedValue::User(u, member) => OptVal::User(UserRef {
            id: u.id.get(),
            name: display_name(
                member.and_then(|m| m.nick.as_deref()),
                u.global_name.as_deref(),
                &u.name,
            ),
        }),
        ResolvedValue::SubCommand(inner) | ResolvedValue::SubCommandGroup(inner) => {
            OptVal::Sub(to_opt_vals(inner))
        }
        // Every other option type, including ones a future serenity adds.
        // Kept as a present-but-unreadable option rather than dropped, so
        // the parser refuses instead of treating it as absent.
        _ => OptVal::Other,
    }
}

/// Everything the event handler needs beyond the READY flag. `None` for an
/// audio-only gateway, which registers nothing and emits nothing.
#[derive(Clone)]
struct HandlerCtx {
    guild: u64,
    commands: bool,
    events: StdSender<GatewayEvent>,
    pending: Arc<Mutex<PendingMap>>,
    /// This bot's own user id, learned from READY. Zero until then, which
    /// is a value no member has — see [`voice_event`] for why this is
    /// carried at all. Shared across reconnects because [`HandlerCtx`] is
    /// cloned per session.
    own_id: Arc<AtomicU64>,
}

impl HandlerCtx {
    /// Hand one event to the node. A failed send means the node has gone;
    /// there is nothing to do about it and nothing worth logging per
    /// event.
    fn emit(&self, ev: GatewayEvent) {
        let _ = self.events.send(ev);
    }
}

/// Sets its flag when the gateway reaches READY, and turns Discord's
/// traffic into plain [`GatewayEvent`]s.
///
/// The READY flag is why this handler exists at all, and it is still the
/// part that cannot be removed. `Songbird::join` panics outright
/// (`"Manager has not been initialised"`) if it is called before serenity
/// has handed songbird the bot's user id, and it returns
/// `JoinError::NoSender` until the shard has registered — so "is the
/// gateway up?" has to be answered before a join is attempted, not
/// inferred from its failure.
///
/// Everything else here is translation, and only translation: an event in,
/// a value out. **No rule about who may do what lives in this file.** The
/// one decision made here is the guild lock ([`is_ours`]), which has to be
/// made here because a `BotCommand` carries no guild to decide it later.
struct Handler {
    ready: Arc<AtomicBool>,
    ctx: Option<HandlerCtx>,
    /// The command loop's REST client — the one that answers interactions,
    /// posts status and edits mutes. serenity's `Client` has an `Http` of
    /// its own and learns the application id for it from READY; this one
    /// is told in [`Handler::ready`], because editing an interaction's
    /// acknowledgement is a webhook call addressed by APPLICATION id, and
    /// without it every answer fails before a request is even built.
    rest: Arc<serenity::http::Http>,
}

#[serenity::async_trait]
impl serenity::client::EventHandler for Handler {
    async fn ready(
        &self,
        ctx: serenity::client::Context,
        data_about_bot: serenity::model::gateway::Ready,
    ) {
        // Before the ready flag: an interaction cannot arrive ahead of
        // READY, so this is what guarantees the client that answers it
        // knows the application id by then. Idempotent across reconnects.
        self.rest.set_application_id(data_about_bot.application.id);
        self.ready.store(true, Ordering::Relaxed);

        let Some(c) = self.ctx.as_ref() else { return };
        // Recorded before anything else is done with this event: from here
        // on every voice state can be checked against it, and the bridge
        // can never be mistaken for a human. See `voice_event` — including
        // why "from here on" is a claim about THIS handler only: serenity
        // runs each handler on its own task, so `guild_create` can read
        // this before the line below has run.
        c.own_id
            .store(data_about_bot.user.id.get(), Ordering::Relaxed);
        let Some(guild) = to_serenity_guild(c.guild) else {
            eprintln!("astar-discord: no guild configured, so no slash commands were registered");
            return;
        };
        // An EMPTY list when commands are switched off, not "no call":
        // registration is a bulk overwrite, so this is what takes a
        // previous run's commands down. Nothing else ever clears them.
        let commands = if c.commands {
            command_builders()
        } else {
            Vec::new()
        };
        let http = Arc::clone(&ctx.http);
        // Spawned, never awaited: READY is what unblocks a pending join
        // (see this handler's docs), and a REST round trip must not stand
        // between the two.
        tokio::spawn(async move {
            if let Err(e) = guild.set_commands(&http, commands).await {
                eprintln!(
                    "astar-discord: could not register the slash commands (is the bot missing \
                     the applications.commands scope in this server?): {e}"
                );
            }
        });
    }

    async fn guild_create(
        &self,
        _ctx: serenity::client::Context,
        guild: serenity::model::guild::Guild,
        _is_new: Option<bool>,
    ) {
        let Some(c) = self.ctx.as_ref() else { return };
        if !is_ours(c.guild, Some(guild.id.get())) {
            return;
        }
        // This, not `ready`, is where the guild's voice states arrive —
        // `ready` carries unavailable guild stubs and nothing else. On a
        // reconnect serenity fires this again, so the node gets another
        // `Ready` and re-sweeps the channel. That is intended: a
        // reconnect is exactly when events were missed.
        let voice = guild
            .voice_states
            .values()
            .map(|vs| {
                voice_event(
                    vs,
                    vs.member
                        .as_ref()
                        .or_else(|| guild.members.get(&vs.user_id)),
                    c.own_id.load(Ordering::Relaxed),
                )
            })
            .collect();
        c.emit(GatewayEvent::Ready { voice });
    }

    async fn voice_state_update(
        &self,
        _ctx: serenity::client::Context,
        _old: Option<serenity::model::voice::VoiceState>,
        new: serenity::model::voice::VoiceState,
    ) {
        let Some(c) = self.ctx.as_ref() else { return };
        if !is_ours(c.guild, new.guild_id.map(serenity::model::id::GuildId::get)) {
            return;
        }
        c.emit(GatewayEvent::Voice(voice_event(
            &new,
            new.member.as_ref(),
            c.own_id.load(Ordering::Relaxed),
        )));
    }

    async fn interaction_create(
        &self,
        ctx: serenity::client::Context,
        interaction: serenity::model::application::Interaction,
    ) {
        use serenity::model::application::Interaction;

        let Some(c) = self.ctx.as_ref() else { return };

        // `Autocomplete`, `Modal` and `Ping` are shapes this bot never
        // produces — it registers no autocomplete and opens no modal — so
        // they are not deferred and not answered.
        let pending = match interaction {
            Interaction::Command(cmd) => PendingInteraction::Command(Box::new(cmd)),
            Interaction::Component(component) => PendingInteraction::Component(Box::new(component)),
            _ => return,
        };

        // FIRST, before the defer and before anything is parsed: an
        // interaction from another guild, or from a DM, gets no reply, no
        // acknowledgement and no log line.
        if !is_ours(c.guild, pending.guild()) {
            return;
        }

        // Then the defer, before the parse: three seconds is not a lot,
        // and a parse that fails still owes the caller an answer.
        pending.defer(&ctx.http).await;

        let Some(kind) = pending.parse() else {
            pending.answer(&ctx.http, UNPARSEABLE.to_string()).await;
            return;
        };

        let id = pending.id();
        let user = pending.user();
        {
            let now = Instant::now();
            let mut map = c
                .pending
                .lock()
                .expect("pending interactions mutex poisoned");
            // Swept on insert: the map only grows when an interaction
            // arrives, so that is the only moment it can need trimming.
            prune_expired(&mut map, now, PENDING_TTL);
            map.insert(id, (now, pending));
        }

        // Parked BEFORE the event is emitted, and that order is
        // load-bearing: the Commander answers on the node's thread, so its
        // `reply` can reach the command loop before this function's next
        // line would have run.
        c.emit(GatewayEvent::Command(BotCommand {
            user,
            kind,
            interaction: id,
        }));
    }
}

/// One voice state as a plain [`VoiceEvent`].
///
/// `member` is whatever the event carried, or the guild's own member list
/// on the initial sweep. With no member at all there is no name and no bot
/// flag to read: the id stands in as the name, and `is_bot` falls back to
/// false — which is the conservative answer in the HUMAN direction, since a
/// human wrongly skipped would be an unmuted stranger in the channel.
///
/// **`own_id` is the other direction, and it is not symmetrical.**
/// `is_bot` is the only thing standing between the Commander and
/// server-muting THIS BRIDGE: `Commander::voice` returns immediately for a
/// bot, and for nobody else. If this bot's own voice state ever arrived
/// with no member attached — a payload serenity could not resolve, a
/// gateway that sent a partial state — the fallback above would read it as
/// a human, the node would mute its own bridge, and there is no path back:
/// the bridge is not approved, cannot be given the mic, and the mute it
/// issued against itself is a mute it recorded as its own. So an id equal
/// to the one READY reported is a bot whatever the member field says. The
/// id is zero until READY has been seen, and zero is not a snowflake any
/// member has, so before then this matches nobody.
///
/// **And "until READY has been seen" is genuinely racy — it is not merely
/// the first microseconds of the connection.** READY precedes `GUILD_CREATE`
/// on the wire, but serenity 0.12.5 dispatches every event handler on its
/// own tokio task (`src/client/dispatch.rs:69`, `spawn_named`), so the
/// `guild_create` handler that reads `own_id` is NOT ordered after the
/// `ready` handler that stores it. The read can see zero for a guild whose
/// voice states have already arrived.
///
/// The consequence is bounded, and that is why it is documented rather than
/// closed. It bites only for THIS BOT'S OWN voice state, delivered in that
/// window, with no member resolvable from either the event or the guild's
/// member list — because a resolvable member still answers `user.bot`, and
/// the bridge's own member is in the guild payload that carries the voice
/// states. Everything else is unaffected: another bot is still a bot, a
/// human is still a human, and the next `voice_state_update` for the bridge
/// reads an `own_id` that is long since stored.
///
/// The obvious closes are both worse. `ctx.cache` does not exist in this
/// build — serenity is taken with `default-features = false` and no
/// `cache` feature, deliberately, because a cache is a copy of a guild this
/// node has no business keeping. And `ctx.http.get_current_user()` is a
/// REST round trip: awaiting it inside `guild_create` would put a network
/// call between a reconnect and the `Ready` the Commander rebuilds its
/// picture from, and spawning it instead just replaces this race with an
/// unordered one that can still lose. If it ever needs closing, the honest
/// fix is to gate the `Ready` emission on `own_id` being non-zero, which
/// costs an ordering this file does not currently need.
fn voice_event(
    state: &serenity::model::voice::VoiceState,
    member: Option<&serenity::model::guild::Member>,
    own_id: u64,
) -> VoiceEvent {
    let user = match member {
        Some(m) => UserRef {
            id: m.user.id.get(),
            name: display_name(
                m.nick.as_deref(),
                m.user.global_name.as_deref(),
                &m.user.name,
            ),
        },
        None => UserRef {
            id: state.user_id.get(),
            name: state.user_id.get().to_string(),
        },
    };
    VoiceEvent {
        user,
        channel: state.channel_id.map(serenity::model::id::ChannelId::get),
        // `mute`, NOT `self_mute`: this is the guild's server mute, the
        // one a bot can set and a member cannot lift for themselves.
        // `self_mute` is the member's own mic button and none of this
        // node's business.
        server_muted: state.mute,
        // The `own_id` half first, deliberately: it must not depend on a
        // member being resolvable, because the case it exists for is the
        // one where it is not.
        is_bot: is_own_bot(state.user_id.get(), own_id) || member.is_some_and(|m| m.user.bot),
    }
}

/// Whether this voice state belongs to this bot itself.
///
/// Split out so the rule can be stated once and tested: `own_id` is zero
/// until READY has been seen, and a zero here must match nobody rather than
/// everybody.
fn is_own_bot(user_id: u64, own_id: u64) -> bool {
    own_id != 0 && user_id == own_id
}

/// [`COMMANDS`] as serenity builders.
///
/// The table is the single source of truth for what Discord is told this
/// bot accepts; `definitions_and_parser_agree` in [`crate::interactions`]
/// is what keeps it in step with the parser.
fn command_builders() -> Vec<serenity::builder::CreateCommand> {
    COMMANDS
        .iter()
        .map(|def| {
            let mut cmd =
                serenity::builder::CreateCommand::new(def.name).description(def.description);
            for opt in def.options {
                cmd = cmd.add_option(option_builder(opt));
            }
            cmd
        })
        .collect()
}

fn option_builder(def: &crate::interactions::OptionDef) -> serenity::builder::CreateCommandOption {
    use serenity::model::application::CommandOptionType;

    let kind = match def.kind {
        OptKind::SubCommand => CommandOptionType::SubCommand,
        OptKind::Str => CommandOptionType::String,
        OptKind::User => CommandOptionType::User,
    };
    let mut opt = serenity::builder::CreateCommandOption::new(kind, def.name, def.description)
        .required(def.required);
    for choice in def.choices {
        opt = opt.add_string_choice(*choice, *choice);
    }
    for sub in def.options {
        opt = opt.add_sub_option(option_builder(sub));
    }
    opt
}

fn to_serenity_guild(raw: u64) -> Option<serenity::model::id::GuildId> {
    NonZeroU64::new(raw).map(serenity::model::id::GuildId::from)
}

fn to_serenity_channel(raw: u64) -> Option<serenity::model::id::ChannelId> {
    NonZeroU64::new(raw).map(serenity::model::id::ChannelId::from)
}

/// A live, unseekable byte source for symphonia: bytes arrive from a
/// `std::sync::mpsc` channel as they are pushed by the command loop, one
/// pushed [`Vec<u8>`] per `read` call once drained. Wrapped in
/// [`songbird::input::RawAdapter`], this becomes the single, long-lived
/// [`songbird::input::Input`] that every `Frame` command streams into — no
/// new track is created per frame.
///
/// `std::sync::mpsc::Receiver` is `Send` but not `Sync`; the `Mutex` exists
/// only to make the whole type `Sync`, which `MediaSource` requires. Exactly
/// one thread (songbird's own mixer thread) ever calls `read`, so there is no
/// real contention.
struct PcmChannelSource {
    rx: Mutex<StdReceiver<Vec<u8>>>,
    pending: Vec<u8>,
    pos: usize,
}

impl PcmChannelSource {
    fn new(rx: StdReceiver<Vec<u8>>) -> Self {
        Self {
            rx: Mutex::new(rx),
            pending: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for PcmChannelSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.pending.len() {
                let n = buf.len().min(self.pending.len() - self.pos);
                buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            let next = self
                .rx
                .lock()
                .expect("PcmChannelSource receiver mutex poisoned")
                .recv();
            match next {
                // The sender side (the command loop) is gone: report EOF
                // rather than blocking the mixer thread forever.
                Err(_) => return Ok(0),
                Ok(bytes) => {
                    self.pending = bytes;
                    self.pos = 0;
                }
            }
        }
    }
}

impl Seek for PcmChannelSource {
    fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "live PCM stream is not seekable",
        ))
    }
}

impl songbird::input::core::io::MediaSource for PcmChannelSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Convert one frame of interleaved i16 PCM to the interleaved little-endian
/// f32 PCM that [`songbird::input::RawAdapter`] expects.
fn pcm_i16_to_raw_f32_bytes(frame: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frame.len() * 4);
    for &sample in frame {
        // Lossless: every i16 value is exactly representable in f32.
        let f = f32::from(sample) / 32_768.0;
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

fn to_guild_id(raw: u64) -> Option<songbird::id::GuildId> {
    NonZeroU64::new(raw).map(songbird::id::GuildId::from)
}

fn to_channel_id(raw: u64) -> Option<songbird::id::ChannelId> {
    NonZeroU64::new(raw).map(songbird::id::ChannelId::from)
}

/// Build one long-lived streaming [`songbird::input::Input`] and its feed
/// channel. Every `Frame` command pushes bytes into the returned sender;
/// songbird pulls them out via [`PcmChannelSource`] on its own schedule.
fn new_streaming_input() -> (StdSender<Vec<u8>>, songbird::input::Input) {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let source = PcmChannelSource::new(rx);
    // 48 kHz stereo: matches FramePipeline's output and VoiceLink::play's contract.
    let input: songbird::input::Input = songbird::input::RawAdapter::new(source, 48_000, 2).into();
    (tx, input)
}

/// Keeps the serenity/songbird gateway connection alive, reconnecting with
/// capped exponential backoff whenever a session ends. Publishes the current
/// `ShardManager` (if any) so the command loop can ask it to shut down
/// cleanly, and stops retrying once `stop` is set.
async fn run_gateway_with_backoff(
    token: String,
    manager: Arc<songbird::Songbird>,
    shard_manager: Arc<Mutex<Option<Arc<serenity::gateway::ShardManager>>>>,
    stop: Arc<AtomicBool>,
    gateway_ready: Arc<AtomicBool>,
    handler_ctx: Option<HandlerCtx>,
    rest: Arc<serenity::http::Http>,
) {
    use serenity::model::gateway::GatewayIntents;

    const MAX_BACKOFF: Duration = Duration::from_secs(60);
    // A session must have stayed up at least this long before a drop resets
    // backoff to the floor. `ClientBuilder::into_future`'s only network call
    // ahead of the gateway handshake is serenity's *unauthenticated* "Get
    // Gateway" endpoint, which succeeds even with a revoked or malformed
    // token — so `builder.await` succeeding is not evidence of a working
    // connection. Without this floor, a persistently bad token would build
    // a "client" every cycle, fail fast in `start()`, and retry at ~1s
    // forever instead of escalating toward `MAX_BACKOFF`.
    const MIN_SESSION_FOR_RESET: Duration = Duration::from_secs(5);

    // GUILDS + GUILD_VOICE_STATES only: no privileged intents. This bot must
    // never be able to read message content. Interactions — slash commands
    // and the panel's button clicks alike — are delivered to the
    // application that owns them and need no intent at all, which is why
    // adding commands does not widen this line.
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;

    let mut backoff = Duration::from_secs(1);

    while !stop.load(Ordering::Relaxed) {
        let builder = serenity::Client::builder(&token, intents).event_handler(Handler {
            ready: Arc::clone(&gateway_ready),
            ctx: handler_ctx.clone(),
            rest: Arc::clone(&rest),
        });
        let builder = songbird::serenity::register_with(builder, Arc::clone(&manager));

        match builder.await {
            Ok(mut client) => {
                *shard_manager.lock().expect("shard manager mutex poisoned") =
                    Some(Arc::clone(&client.shard_manager));

                let session_started = Instant::now();
                let result = client.start().await;

                // The session is over, so READY no longer describes anything.
                // Clearing it here is what makes a join attempted during a
                // reconnect wait for the NEW session instead of walking
                // straight into `Songbird::join` with no shard behind it.
                gateway_ready.store(false, Ordering::Relaxed);
                *shard_manager.lock().expect("shard manager mutex poisoned") = None;

                if stop.load(Ordering::Relaxed) {
                    break;
                }

                // Only a session that actually stayed connected for a while
                // is evidence of a working token/network path; reset so the
                // next transient drop retries promptly. A fast failure
                // leaves backoff exactly where it was, so it keeps growing.
                if session_started.elapsed() >= MIN_SESSION_FOR_RESET {
                    backoff = Duration::from_secs(1);
                }

                if let Err(e) = result {
                    eprintln!("astar-discord: gateway session ended: {e}");
                }
            }
            Err(e) => {
                eprintln!("astar-discord: failed to build the Discord client: {e}");
            }
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(backoff).await;
        backoff = Duration::min(backoff.saturating_mul(2), MAX_BACKOFF);
    }
}

/// The runtime body: build a tokio runtime, keep a serenity/songbird gateway
/// connection alive on it, and service `GatewayCmd`s until `Shutdown` or a
/// closed channel.
///
/// This is the whole life of a [`Gateway`]. Voice sessions come and go
/// inside it as `Join`/`Leave` pairs; neither ends this loop.
///
/// The command loop runs synchronously on this thread; individual async
/// operations (join, leave, shutdown) are dispatched with `rt.block_on`
/// while the gateway supervisor keeps running concurrently as a task spawned
/// on the same multi-thread runtime. `failed` is the same flag every
/// [`SongbirdLink`] taken from this gateway checks before every send: on top
/// of [`Gateway::connect`]'s `catch_unwind` (which only sees panics in this
/// function's own call stack), the loop below polls the supervisor task's
/// `JoinHandle` on every iteration and sets `failed` the moment that task is
/// gone — see the module docs for exactly what this does and does not cover.
#[allow(clippy::too_many_lines)] // flat command match — splitting hurts readability
fn run_runtime(
    token: &str,
    rx: &StdReceiver<GatewayCmd>,
    failed: &Arc<AtomicBool>,
    audio: &AudioHealth,
    opts: Option<GatewayOptions>,
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("astar-discord-rt")
        .enable_all()
        .build()
        .expect("failed to build the astar-discord tokio runtime");

    // `serenity_from_config`, not `serenity()`: the config is the manager's
    // own, cloned into every `Call` it creates (`Songbird::get_or_insert` ->
    // `Call::from_config`) — so `DecodeMode::Pass` is in place before the
    // driver exists, let alone before it connects. Setting it later would be
    // a race against the first packet.
    let manager = songbird::Songbird::serenity_from_config(voice_config());
    let shard_manager: Arc<Mutex<Option<Arc<serenity::gateway::ShardManager>>>> =
        Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    // True only between a READY frame and the end of that session. A join
    // attempted before READY would panic inside songbird (see `ReadyFlag`).
    let gateway_ready = Arc::new(AtomicBool::new(false));

    // Owned by this thread and shared only with the event handler, which
    // runs on this same runtime. The interaction tokens inside it never go
    // anywhere else.
    let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
    // The panel's REST bookkeeping, shared with whichever redraw task is
    // in flight.
    let panel: Arc<Mutex<PanelRest>> = Arc::new(Mutex::new(PanelRest::default()));

    let guild = opts.as_ref().map(|o| o.guild);
    let events = opts.as_ref().map(|o| o.events.clone());
    let handler_ctx = opts.map(|o| HandlerCtx {
        guild: o.guild,
        commands: o.commands,
        events: o.events,
        pending: Arc::clone(&pending),
        own_id: Arc::new(AtomicU64::new(0)),
    });

    // The REST half of the bot: status posts, mute edits, the panel, and
    // the answers to interactions. It is built inside the runtime because
    // its reqwest client wants a reactor, and it holds the token in
    // serenity's `SecretString` — this module never prints it, and nothing
    // derived from it is ever interpolated into a posted message. Built
    // BEFORE the gateway task because the event handler has to hold it:
    // `Handler::ready` is where it learns the application id, without
    // which it can acknowledge an interaction but never answer one.
    let http = rt.block_on(async { Arc::new(serenity::http::Http::new(token)) });

    let mut gateway_task = Some(rt.spawn(run_gateway_with_backoff(
        token.to_string(),
        Arc::clone(&manager),
        Arc::clone(&shard_manager),
        Arc::clone(&stop),
        Arc::clone(&gateway_ready),
        handler_ctx,
        Arc::clone(&http),
    )));

    let mut pcm_tx: Option<StdSender<Vec<u8>>> = None;
    let mut joined_guild: Option<songbird::id::GuildId> = None;
    // The live voice session's talk-edge state, if any. One per successful
    // join, ended on the way out of the session so the node is never left
    // believing somebody is mid-word. `None` for an audio-only gateway:
    // with no events sender there is nobody to tell, so no receive handler
    // is registered at all.
    let mut talk: Option<TalkSession> = None;
    // The audio track's supervision, for the whole life of the gateway
    // rather than of one session: the generation counter has to outlive a
    // session so that the track a `Leave` ends can be retired BEFORE it
    // fires its `End`, and the health flag is the one the node's handles
    // already hold a clone of.
    let tracks = TrackSupervisor {
        health: audio.clone(),
        ..TrackSupervisor::new(events.clone())
    };
    // The most recent status post still in flight, if any. Kept so the
    // teardown can wait for it: see `post_status_text` for why nothing else
    // would.
    let mut last_post: Option<tokio::task::JoinHandle<()>> = None;
    // Every mute/unmute PATCH still in flight. Kept for the same reason as
    // `last_post` and for a bigger stake — see `InFlight`. ALL of them, not
    // just the most recent: a shutdown unmutes everyone this node muted, and
    // each of those is a separate member left muted guild-wide if it is
    // abandoned.
    let mut mutes = InFlight::default();

    // A polling loop rather than a plain `for cmd in rx`: a dead gateway
    // task must be noticed even during a quiet stretch with no commands
    // arriving, not just whenever the next command happens to show up.
    'commands: loop {
        if gateway_died(&rt, &mut gateway_task) {
            failed.store(true, Ordering::Relaxed);
            break 'commands;
        }

        let cmd = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => continue 'commands,
            Err(RecvTimeoutError::Disconnected) => break 'commands,
        };

        match cmd {
            GatewayCmd::Join {
                guild,
                channel,
                reply,
            } => {
                let outcome = handle_join(
                    &rt,
                    &manager,
                    &gateway_ready,
                    guild,
                    channel,
                    events.as_ref(),
                    JoinState {
                        pcm_tx: &mut pcm_tx,
                        joined_guild: &mut joined_guild,
                        talk: &mut talk,
                        tracks: &tracks,
                    },
                );
                if let Err(e) = &outcome {
                    eprintln!("astar-discord: {e}");
                }
                // `try_send`, not `send`: the slot is free unless the caller
                // has already given up and dropped the receiver, and the
                // command loop must never block on a caller.
                let _ = reply.try_send(outcome);
            }
            GatewayCmd::Leave { reply } => {
                // FIRST, before anything else in this arm, and both halves
                // of "before" matter. The disconnect below ends the track —
                // but so does the line under this one: dropping the frame
                // sender makes `PcmChannelSource::read` return `Ok(0)`,
                // which is EOF, which is `TrackEvent::End`. Either way the
                // event must find its own generation already superseded, or
                // a leave the operator asked for would be announced into
                // the channel as the audio feed failing.
                let _retired = tracks.tracks.next();
                pcm_tx = None;
                // Before the disconnect, not after: the node is told that
                // the mic holder stopped talking whether or not songbird
                // manages a clean leave.
                end_talk(&mut talk);
                let outcome = handle_leave(&rt, &manager, &mut joined_guild);
                if let Err(e) = &outcome {
                    eprintln!("astar-discord: {e}");
                }
                // `try_send` for the same reason as `Join`'s: the slot is
                // free unless the caller has already given up, and the
                // command loop must never block on a caller.
                let _ = reply.try_send(outcome);
            }
            GatewayCmd::Frame(pcm) => {
                // The track died and the handler on songbird's driver task
                // recorded it. Nothing downstream can tell by itself — the
                // channel that feeds it is still open, and a send into it
                // still succeeds — so this is where the feed stops. A
                // `Frame` can only be in flight at all because the node's
                // own `play` checks the same flag before sending.
                if pcm_tx.is_some() && audio.reason().is_some() {
                    pcm_tx = None;
                }
                if let Some(tx) = &pcm_tx
                    && tx.send(pcm_i16_to_raw_f32_bytes(&pcm)).is_err()
                {
                    // The track (and its reader) is gone; stop pushing into
                    // a dead channel until the next Join.
                    pcm_tx = None;
                }
            }
            GatewayCmd::PostText { channel, text } => {
                // Replacing the previous handle merely detaches it — dropping
                // a `JoinHandle` does not cancel its task. Only the most
                // recent post is worth holding: it is the one that can still
                // be in flight when a teardown arrives.
                if let Some(handle) = post_status_text(&rt, &http, channel, text) {
                    last_post = Some(handle);
                }
            }
            GatewayCmd::Reply { interaction, text } => {
                answer_interaction(&rt, &http, &pending, interaction, text);
            }
            GatewayCmd::SetMute { user, mute } => {
                if let Some(handle) =
                    set_member_mute(&rt, &http, guild, events.as_ref(), user, mute)
                {
                    mutes.push(handle);
                }
            }
            GatewayCmd::Panel { channel, talking } => {
                draw_panel(&rt, &http, &panel, channel, talking);
            }
            GatewayCmd::Shutdown => {
                // Before `shutdown_gateway` below, not after the loop:
                // closing the gateway takes the voice connection with it
                // and ends the track, and a track dying while the bot goes
                // offline must not be announced into a public channel as a
                // failure. The retire after the loop covers the other ways
                // out; a second bump there is harmless, because no handler
                // carries a generation that was never handed to a track.
                let _retired = tracks.tracks.next();
                // Before the gateway closes and before the runtime is torn
                // down: this is where the "disconnected" line — and, on a
                // failed join, the "not connected" line whose `Bridge` is
                // dropped microseconds after posting it — is won or lost.
                //
                // A `Shutdown` only ever arrives from [`Gateway::drop`], and
                // the controller drops the gateway at exactly that moment on
                // the non-persistent path, so this wait still covers the
                // failed-join line. On the persistent path the `Shutdown`
                // arrives from the node's own teardown
                // (`NodeController::teardown_discord`), which drops the
                // gateway explicitly at the end of the run.
                //
                // The unmutes go first, and they are the reason this arm
                // waits at all now. The node's teardown runs the Commander's
                // shutdown BEFORE it drops the `Gateway`, so by the time
                // this `Shutdown` is read off the queue every one of those
                // `SetMute`s is already behind it on the same queue and has
                // already been spawned — there is nothing still to arrive,
                // only tasks still to land. A status line that is lost is a
                // line nobody reads; a mute that is lost outlives the
                // process.
                await_in_flight(&rt, &mut mutes);
                await_last_post(&rt, &mut last_post);
                shutdown_gateway(&rt, &stop, &shard_manager);
                break 'commands;
            }
        }
    }

    // Every way out of the loop — `Shutdown`, the command sender dropped,
    // the gateway task found dead — ends the voice session, so this is
    // where the last talkers are reported stopped. A no-op after a `Leave`,
    // which has already taken it.
    end_talk(&mut talk);
    // And the same courtesy to the track: the teardown below ends it, and
    // a bot going offline must not announce that its audio feed failed on
    // the way out.
    let _retired = tracks.tracks.next();

    teardown_runtime(rt, &stop, &mut gateway_task, &mut last_post, &mut mutes);
}

/// End the live talk watch, if there is one: report anyone still marked
/// talking as stopped, and stop its handlers reporting anything further.
///
/// Idempotent by construction — it takes the session out — which is what
/// lets both the `Leave` arm and the post-loop drain call it.
fn end_talk(talk: &mut Option<TalkSession>) {
    if let Some(session) = talk.take() {
        // The users it reported stopped are of no use here: `end` has
        // already told the node about each of them. The return exists for
        // the disconnect path, which logs whether anything was interrupted.
        drop(session.end());
    }
}

/// Stop the gateway supervisor and tear the runtime down, bounded at every
/// step.
///
/// Split out of [`run_runtime`]'s tail for length alone; the ordering is
/// exactly as it was, and every step is bounded because
/// [`Gateway::drop`] joins this thread with no timeout of its own.
fn teardown_runtime(
    rt: tokio::runtime::Runtime,
    stop: &AtomicBool,
    gateway_task: &mut Option<tokio::task::JoinHandle<()>>,
    last_post: &mut Option<tokio::task::JoinHandle<()>>,
    mutes: &mut InFlight,
) {
    // Belt and braces: `stop` plus a shutdown_all() already asked the
    // gateway task to end on its own, and `gateway_died` above already
    // reaps it if it ended first. Aborting whatever is left guarantees this
    // function returns even if the task is stuck between attempts (e.g.
    // mid-backoff sleep), and awaiting the handle lets it actually unwind
    // before the runtime tears down. Bounded by `OP_TIMEOUT`: an abort only
    // takes effect at the task's next await point, so this must not be the
    // one step of the teardown that could wait forever.
    // The exits that are not `Shutdown` — the command sender dropped, or the
    // gateway task found dead — can have a status line, or a mute, in flight
    // just the same. Both are no-ops after `Shutdown`, which has already
    // taken what it waited on.
    await_in_flight(&rt, mutes);
    await_last_post(&rt, last_post);

    stop.store(true, Ordering::Relaxed);
    if let Some(handle) = gateway_task.take() {
        handle.abort();
        rt.block_on(async {
            let _ = tokio::time::timeout(OP_TIMEOUT, handle).await;
        });
    }
    rt.shutdown_timeout(Duration::from_secs(5));
}

/// Ask the gateway to close its session, bounded by [`OP_TIMEOUT`].
///
/// Extracted from the command loop's `Shutdown` arm purely to keep
/// `run_runtime` readable; the ordering matters and is unchanged — `stop` is
/// set first so the supervisor does not immediately reconnect the session
/// this is about to close.
fn shutdown_gateway(
    rt: &tokio::runtime::Runtime,
    stop: &AtomicBool,
    shard_manager: &Mutex<Option<Arc<serenity::gateway::ShardManager>>>,
) {
    stop.store(true, Ordering::Relaxed);
    let sm = shard_manager
        .lock()
        .expect("shard manager mutex poisoned")
        .clone();
    if let Some(sm) = sm {
        rt.block_on(async {
            if tokio::time::timeout(OP_TIMEOUT, sm.shutdown_all())
                .await
                .is_err()
            {
                eprintln!("astar-discord: gateway shutdown timed out; tearing down anyway");
            }
        });
    }
}

/// Post one status line into a channel's text chat, best effort. Returns the
/// spawned task's handle so the teardown can wait for it, or `None` when
/// there was nothing to post to.
///
/// **Spawned, not awaited here.** A REST round trip can take a second, and the
/// command loop is single-threaded: blocking it would leave `Frame` commands
/// queueing in the mpsc and then arriving in a burst, which songbird's mixer
/// plays at real time — the channel would sit permanently behind the net, the
/// same failure `Bridge::join`'s backlog discard exists to prevent. A status
/// line is never worth that.
///
/// The consequence is that a post issued immediately before a `Shutdown` races
/// the teardown — the "disconnected" line on a leave, and above all the "not
/// connected" line on a FAILED join, whose `Bridge` the caller drops the
/// instant the join returns `Err` and whose [`Gateway`] the controller drops
/// immediately behind it unless the gateway is persistent.
/// **`rt.shutdown_timeout` is not what saves it.** Tokio is explicit that
/// tasks spawned with `Runtime::spawn` "keep running until they yield. Then
/// they are dropped. They are not guaranteed to run to completion"
/// (`tokio/src/runtime/runtime.rs`); the timeout covers
/// `spawn_blocking` work and worker-thread teardown, not an async task parked
/// on socket I/O — which a REST POST always is.
///
/// So the wait is explicit instead: [`run_runtime`] keeps the handle returned
/// here and blocks on it for at most [`OP_TIMEOUT`] in its `Shutdown` arm,
/// before the gateway closes. The one line that reveals "astar thinks it never
/// joined" is then reliable rather than probable, and it costs nothing on the
/// audio path — the bridge is already going away.
///
/// **A missing Send Messages permission arrives here as an ordinary serenity
/// error** (HTTP 403) and is logged and dropped. Audio is the point of the
/// bridge; status is secondary, and a permission the operator has not granted
/// must never stop the radio reaching the channel.
fn post_status_text(
    rt: &tokio::runtime::Runtime,
    http: &Arc<serenity::http::Http>,
    channel: u64,
    text: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(channel_id) = NonZeroU64::new(channel) else {
        eprintln!("astar-discord: cannot post a status line to a zero channel id");
        return None;
    };
    let http = Arc::clone(http);
    Some(rt.spawn(async move {
        let id = serenity::model::id::ChannelId::from(channel_id);
        // `send_message` rather than `say`, for the one thing `say` cannot
        // express: an empty `CreateAllowedMentions`. Public lines name
        // people as `<@id>` so the channel's audit trail reads as names,
        // and with mentions suppressed that rendering costs nobody a
        // notification. Every message this bot posts carries it.
        let message = serenity::builder::CreateMessage::new()
            .content(text)
            .allowed_mentions(serenity::builder::CreateAllowedMentions::new());
        if let Err(e) = id.send_message(&http, message).await {
            eprintln!(
                "astar-discord: could not post the status line (is the bot missing Send \
                 Messages in this channel?): {e}"
            );
        }
    }))
}

/// Handle `GatewayCmd::Reply`: find the deferred interaction and edit its
/// placeholder into the answer.
///
/// The interaction is REMOVED from the map: an interaction is answered
/// once, and holding a spent token would only keep a secret alive for
/// nothing.
///
/// An id with nothing behind it is logged and dropped. It means the entry
/// was pruned — the Commander took longer than fourteen minutes, which it
/// never does — or that two replies were issued for one interaction. Both
/// are worth a line and neither is worth failing anything for.
fn answer_interaction(
    rt: &tokio::runtime::Runtime,
    http: &Arc<serenity::http::Http>,
    pending: &Arc<Mutex<PendingMap>>,
    interaction: u64,
    text: String,
) {
    let entry = pending
        .lock()
        .expect("pending interactions mutex poisoned")
        .remove(&interaction);
    let Some((_, pending)) = entry else {
        eprintln!(
            "astar-discord: no pending interaction left to answer (it may have already been \
             answered, or expired)"
        );
        return;
    };
    let http = Arc::clone(http);
    // Spawned, not awaited: see `post_status_text`.
    rt.spawn(async move {
        pending.answer(&http, text).await;
    });
}

/// Handle `GatewayCmd::SetMute`: server-mute or unmute one member.
///
/// A refusal — almost always a missing Mute Members permission — comes
/// back as [`GatewayEvent::MuteFailed`], which the Commander turns into
/// ONE line in the channel and a `last_error` on the admin page. The
/// reason string is serenity's `Display`, which for an HTTP error is
/// Discord's own JSON `message` field and nothing else: no URL, no headers,
/// and so no token. (`HttpError::Request`'s `Display` is the fixed string
/// "Error while sending HTTP request." — `serenity-0.12.5/src/http/error.rs`
/// — and the status/JSON arms print Discord's own `message` field.)
///
/// Returns the spawned task's handle so the caller can wait for it. That is
/// not optional politeness: the shutdown unmutes go out through here, and a
/// task nobody holds is a task the teardown cannot wait for — see
/// [`InFlight`] and [`post_status_text`] for why the runtime's own shutdown
/// is not a wait.
fn set_member_mute(
    rt: &tokio::runtime::Runtime,
    http: &Arc<serenity::http::Http>,
    guild: Option<u64>,
    events: Option<&StdSender<GatewayEvent>>,
    user: u64,
    mute: bool,
) -> Option<tokio::task::JoinHandle<()>> {
    let (Some(guild_id), Some(user_id)) = (
        guild.and_then(to_serenity_guild),
        NonZeroU64::new(user).map(serenity::model::id::UserId::from),
    ) else {
        eprintln!("astar-discord: cannot mute without a configured guild and a real user id");
        return None;
    };
    let http = Arc::clone(http);
    let events = events.cloned();
    Some(rt.spawn(async move {
        let builder = serenity::builder::EditMember::new().mute(mute);
        if let Err(e) = guild_id.edit_member(&http, user_id, builder).await {
            eprintln!("astar-discord: could not change a member's server mute: {e}");
            if let Some(events) = events {
                let _ = events.send(GatewayEvent::MuteFailed {
                    user,
                    reason: e.to_string(),
                });
            }
        }
    }))
}

/// The mute/unmute requests this gateway has spawned and not yet seen
/// finish.
///
/// **Why anything keeps them at all.** A `SetMute` is a REST call spawned
/// onto the runtime, and tokio is explicit that a task spawned with
/// `Runtime::spawn` is dropped at its next yield when the runtime shuts
/// down — it is "not guaranteed to run to completion". At shutdown the
/// Commander enqueues one unmute per member it muted; if nothing holds
/// those handles, the process exits with every one of them parked on a
/// socket and the members stay server-muted guild-wide. Holding them is
/// what lets the `Shutdown` arm wait — the same grace the last status post
/// already gets, and for a consequence that outlives the process rather
/// than a line of text that is merely missed.
///
/// The record stays small by pruning on the way in: a gateway that has been
/// muting a busy channel for a week has at most the handles that have not
/// finished yet, which in practice is none.
#[derive(Default)]
struct InFlight {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl InFlight {
    /// Record one spawned mute, dropping whatever has already finished.
    ///
    /// Pruning here rather than on a timer is deliberate: this is the only
    /// place the set grows, so it is the only place it can grow unbounded.
    fn push(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.prune_finished();
        self.handles.push(handle);
    }

    /// Forget every handle whose task has already completed. Dropping a
    /// `JoinHandle` detaches; it never cancels, so a prune can never lose a
    /// mute that is still on the wire.
    fn prune_finished(&mut self) {
        self.handles.retain(|h| !h.is_finished());
    }

    /// Everything still outstanding, leaving the record empty so a second
    /// waiter cannot await the same task twice.
    fn take_all(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(&mut self.handles)
    }

    /// How many handles are being held. Used by the tests, and by nothing
    /// else — the production paths only push and take.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.handles.len()
    }
}

/// Wait for every outstanding mute/unmute, bounded by ONE [`OP_TIMEOUT`]
/// overall.
///
/// One budget for the whole set, not one per task: a shutdown sweep of a
/// busy channel can be dozens of requests, and `OP_TIMEOUT` each would let a
/// rate-limited guild hold the process open for minutes. They are all in
/// flight concurrently anyway, so the useful question is "have they landed
/// yet", once.
///
/// What is abandoned on a timeout stays recorded in the node's
/// `muted_by_us` set, so the next start (or the member's next appearance in
/// voice) lifts it — see [`crate::Commander::shutdown`].
fn await_in_flight(rt: &tokio::runtime::Runtime, mutes: &mut InFlight) {
    let handles = mutes.take_all();
    if handles.is_empty() {
        return;
    }
    let outstanding = handles.len();
    rt.block_on(async move {
        let all = async {
            for handle in handles {
                // A mute task never panics and never returns anything; a
                // `JoinError` here would only mean the runtime is going
                // down under us, which is exactly what we are racing.
                let _ = handle.await;
            }
        };
        if tokio::time::timeout(OP_TIMEOUT, all).await.is_err() {
            eprintln!(
                "astar-discord: {outstanding} server mute change(s) did not finish in time; \
                 anyone this node muted and could not unmute stays muted until they next \
                 appear in voice with the node running"
            );
        }
    });
}

/// The panel's REST-side bookkeeping: the coalescer, the message being
/// maintained, and how recently we checked that it is still visible.
#[derive(Default)]
struct PanelRest {
    state: PanelState,
    /// The panel message this gateway is maintaining, once one exists.
    message: Option<serenity::model::id::MessageId>,
    /// When the channel was last scanned for the panel having scrolled
    /// away. `None` means never.
    last_scan: Option<Instant>,
    /// Set once the channel refuses a history read, so the scan is
    /// abandoned rather than retried into an error loop.
    history_forbidden: bool,
}

/// Handle `GatewayCmd::Panel`: ask the coalescer whether a redraw is due,
/// and start one if it is.
///
/// The channel is checked BEFORE the coalescer is told anything: a `want`
/// that returned an action this function then threw away would leave the
/// panel permanently marked "redraw in flight" with no task ever to
/// finish it.
fn draw_panel(
    rt: &tokio::runtime::Runtime,
    http: &Arc<serenity::http::Http>,
    panel: &Arc<Mutex<PanelRest>>,
    channel: u64,
    talking: Option<String>,
) {
    let Some(channel_id) = to_serenity_channel(channel) else {
        eprintln!("astar-discord: cannot draw the PTT panel in a zero channel id");
        return;
    };
    let action = panel
        .lock()
        .expect("panel mutex poisoned")
        .state
        .want(talking);
    let Some(action) = action else { return };
    // Spawned like every other REST call here; see `post_status_text` for
    // why the command loop must not block on one.
    rt.spawn(run_panel(
        Arc::clone(http),
        channel_id,
        Arc::clone(panel),
        action,
    ));
}

/// Perform one panel redraw, then whatever the coalescer says is still
/// owed.
///
/// A loop rather than a chain of spawns: exactly one of these tasks exists
/// at a time, which is what bounds the edit rate. It ends when the panel
/// on screen matches what was last asked for.
async fn run_panel(
    http: Arc<serenity::http::Http>,
    channel: serenity::model::id::ChannelId,
    shared: Arc<Mutex<PanelRest>>,
    first: PanelAction,
) {
    // Armed for the whole body. On the normal path `done` has already
    // emptied the in-flight slot by the time this drops, so it is a no-op;
    // on a panic it is the only thing that will ever empty it.
    let _guard = PanelGuard {
        shared: &shared,
        channel,
    };
    let mut action = first;
    loop {
        let PanelAction::Render(talking) = action;
        render_panel(&http, channel, &shared, talking.as_deref()).await;
        let next = shared.lock().expect("panel mutex poisoned").state.done();
        match next {
            Some(a) => action = a,
            None => break,
        }
    }
}

/// Frees the panel's in-flight slot if [`run_panel`] is unwound out of.
///
/// **What this is for.** `PanelState::done` is the only thing that empties
/// that slot, and it is reached only when the REST call returns. A panic
/// anywhere in a redraw — in serenity, in the builder, in this file — is
/// contained by tokio inside the task's `JoinHandle` and never unwinds into
/// anybody who would notice; the slot would then stay occupied for the life
/// of the process, and every later `want` would answer "a redraw is already
/// in flight". The panel stops updating, silently and permanently. That is
/// worth one guard.
struct PanelGuard<'a> {
    shared: &'a Arc<Mutex<PanelRest>>,
    channel: serenity::model::id::ChannelId,
}

impl Drop for PanelGuard<'_> {
    fn drop(&mut self) {
        // **A poisoned lock is ignored HERE and nowhere else**, and it is
        // not the swallowed-poison anti-pattern: this runs during
        // unwinding, and a panic inside a `Drop` that is itself running
        // because of a panic aborts the process. Every other lock in this
        // file keeps `.expect("panel mutex poisoned")`, because everywhere
        // else a poisoned panel mutex means the redraw state is unusable
        // and saying so loudly is right. A poisoned lock here means the
        // panic we are unwinding from happened while the redraw state was
        // held, so it is already unusable — and taking the process down
        // over a button message is not the trade.
        if let Ok(mut guard) = self.shared.lock() {
            guard.state.abandon();
        } else {
            eprintln!(
                "astar-discord: a PTT panel redraw in {channel} was interrupted and its state \
                 could not be cleared",
                channel = self.channel
            );
        }
    }
}

/// Post or edit the panel message so it shows `talking`.
///
/// A failure is logged and dropped, exactly like a status line: audio is
/// the point of the bridge, and a permission the operator has not granted
/// must never turn into a retry loop.
async fn render_panel(
    http: &Arc<serenity::http::Http>,
    channel: serenity::model::id::ChannelId,
    shared: &Arc<Mutex<PanelRest>>,
    talking: Option<&str>,
) {
    let view = panel_view(talking);
    let buttons = serenity::builder::CreateActionRow::Buttons(vec![
        serenity::builder::CreateButton::new(PTT_BUTTON_ID)
            .label(view.ptt_label)
            .style(if view.ptt_danger {
                serenity::model::application::ButtonStyle::Danger
            } else {
                serenity::model::application::ButtonStyle::Primary
            }),
        serenity::builder::CreateButton::new(STATUS_BUTTON_ID)
            .label(STATUS_LABEL)
            .style(serenity::model::application::ButtonStyle::Secondary),
    ]);

    let existing = shared.lock().expect("panel mutex poisoned").message;

    // Nothing posted yet, or what was posted has scrolled out of sight.
    if existing.is_none() || panel_is_buried(http, channel, existing, shared).await {
        let message = serenity::builder::CreateMessage::new()
            .content(view.content)
            .components(vec![buttons])
            .allowed_mentions(serenity::builder::CreateAllowedMentions::new());
        match channel.send_message(http, message).await {
            Ok(posted) => {
                let old = shared
                    .lock()
                    .expect("panel mutex poisoned")
                    .message
                    .replace(posted.id);
                // The old panel is this bot's own message, so deleting it
                // needs no Manage Messages permission. Best effort: a
                // leftover panel is untidy, not broken.
                if let Some(old) = old
                    && let Err(e) = channel.delete_message(http, old).await
                {
                    eprintln!("astar-discord: could not delete the old PTT panel: {e}");
                }
            }
            Err(e) => eprintln!(
                "astar-discord: could not post the PTT panel (is the bot missing Send Messages \
                 in this channel?): {e}"
            ),
        }
        return;
    }

    let Some(id) = existing else { return };
    let edit = serenity::builder::EditMessage::new()
        .content(view.content)
        .components(vec![buttons])
        .allowed_mentions(serenity::builder::CreateAllowedMentions::new());
    if let Err(e) = channel.edit_message(http, id, edit).await {
        eprintln!("astar-discord: could not redraw the PTT panel: {e}");
        // 404 is the one failure worth acting on: somebody deleted the
        // panel, so forget it and the next redraw posts a fresh one.
        // Anything else (a missing permission, a network blip) must not
        // turn into a stream of new panels.
        if http_status(&e) == Some(404) {
            let mut guard = shared.lock().expect("panel mutex poisoned");
            guard.message = None;
            // ... and this edit's state never reached the screen, so it
            // must not be recorded as shown. `done` would otherwise book
            // it as displayed and the channel would sit with no panel at
            // all until the next holder change — on an idle node, hours.
            //
            // What `abandon` buys is CONDITIONAL, and the condition is
            // `PanelState::start`: the follow-up `done` renders again only
            // if `desired` still differs from `shown`. Usually it does, and
            // that pass POSTS a fresh panel rather than editing a message
            // that is gone, because the id was forgotten above. But if the
            // wanted state has meanwhile returned to what `shown` already
            // records — holder A took the mic, B took it, A took it back
            // while this edit was in flight — `start` answers `None` and no
            // panel is posted now. The channel then has no button until the
            // next holder change, and that change WILL post one: the id is
            // gone, so the render it triggers cannot be an edit.
            //
            // Either way it is exactly one retry: a post cannot 404, so the
            // pass that does render records itself normally whether it
            // succeeds or fails.
            guard.state.abandon();
        }
    }
}

/// Whether the panel has scrolled far enough up the channel to be worth
/// re-posting.
///
/// **What this reads, and what it does not.** It fetches the channel's
/// newest twenty messages and compares `Message::id` against the id of the
/// panel this gateway posted. No message content, author or attachment is
/// read, logged or kept — only the ids, and only to answer "is mine still
/// near the bottom?". That is message *metadata*, not message content: the
/// bot holds no `MESSAGE_CONTENT` intent and could not read a message body
/// here even if it tried.
///
/// It costs a REST call, so it is rate-limited to [`PANEL_SCAN_INTERVAL`]
/// and only ever happens while a redraw is being performed anyway. A
/// channel that refuses the read (no Read Message History) is logged ONCE
/// and never scanned again for this gateway's lifetime: the panel then
/// simply stays where it was posted, which is a cosmetic loss, and the
/// alternative is a 403 per redraw forever.
async fn panel_is_buried(
    http: &Arc<serenity::http::Http>,
    channel: serenity::model::id::ChannelId,
    existing: Option<serenity::model::id::MessageId>,
    shared: &Arc<Mutex<PanelRest>>,
) -> bool {
    let Some(existing) = existing else {
        return false;
    };
    {
        let mut guard = shared.lock().expect("panel mutex poisoned");
        if guard.history_forbidden {
            return false;
        }
        if let Some(last) = guard.last_scan
            && last.elapsed() < PANEL_SCAN_INTERVAL
        {
            return false;
        }
        // Stamped before the call, not after: a scan that fails must still
        // count as a scan, or a failing channel is scanned on every
        // redraw.
        guard.last_scan = Some(Instant::now());
    }

    let recent = serenity::builder::GetMessages::new().limit(20);
    match channel.messages(http, recent).await {
        // Ids only. See this function's docs.
        Ok(messages) => {
            let ids: Vec<u64> = messages.iter().map(|m| m.id.get()).collect();
            panel_is_buried_in(&ids, existing.get())
        }
        Err(e) => {
            eprintln!(
                "astar-discord: cannot check whether the PTT panel is still visible (is the bot \
                 missing Read Message History in this channel?), so it will not be re-posted: {e}"
            );
            // A refusal is permanent — a permission does not appear on its
            // own — so stop asking, and stop logging. Anything else (a
            // network blip, a rate limit) may pass, and is already limited
            // to one attempt per `PANEL_SCAN_INTERVAL` by the stamp above.
            if http_status(&e) == Some(403) {
                shared
                    .lock()
                    .expect("panel mutex poisoned")
                    .history_forbidden = true;
            }
            false
        }
    }
}

/// The whole decision [`panel_is_buried`] makes once the ids are in hand,
/// as a pure function over them.
///
/// **An empty list is "cannot tell", not "buried".** A bot without Read
/// Message History is not refused with a 403 here — Discord answers `GET
/// /channels/{id}/messages` with **HTTP 200 and an empty array**, so the
/// error arm that sets `history_forbidden` never runs and a plain
/// `!contains(ours)` reads a successful, empty answer as "my panel has
/// scrolled away". The panel would then be deleted and re-posted on every
/// redraw — a new message in the channel each time the mic changes hands —
/// which is worse in every way than leaving it where it is. And the honest
/// reading costs nothing: a channel that really does contain zero messages
/// cannot be hiding a panel among them either.
fn panel_is_buried_in(recent: &[u64], ours: u64) -> bool {
    if recent.is_empty() {
        return false;
    }
    !recent.contains(&ours)
}

/// Wait, bounded by [`OP_TIMEOUT`], for the most recent status post to finish.
///
/// This is the whole grace period a status line gets against a teardown; see
/// [`post_status_text`] for why the runtime's own shutdown does not provide
/// one. A post that has not landed by then is abandoned, which is the accepted
/// behaviour for every status line here — but it is abandoned after a real
/// wait, not after however many microseconds the teardown happened to take.
fn await_last_post(
    rt: &tokio::runtime::Runtime,
    last_post: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let Some(handle) = last_post.take() else {
        return;
    };
    rt.block_on(async {
        if tokio::time::timeout(OP_TIMEOUT, handle).await.is_err() {
            eprintln!("astar-discord: the last status line did not finish posting in time");
        }
    });
}

/// Handle `GatewayCmd::Leave`: disconnect the call, and report whether that is
/// what happened.
///
/// Three outcomes, and only the first is a disconnection:
/// * songbird removed the call — `Ok`;
/// * it answered with a `JoinError` — that reason, bounded by
///   [`join_failure_reason`];
/// * it did not answer inside [`OP_TIMEOUT`] — say so, rather than let the
///   caller post "Disconnected" for a call that may well still be up.
///
/// `joined_guild` is cleared either way. A leave that failed leaves nothing
/// this end can usefully do with the id, and repeating `remove` at `Shutdown`
/// would only repeat the same wait while [`Gateway::drop`] blocks on it.
fn handle_leave(
    rt: &tokio::runtime::Runtime,
    manager: &Arc<songbird::Songbird>,
    joined_guild: &mut Option<songbird::id::GuildId>,
) -> Result<(), DiscordError> {
    // Nothing was joined, so there is nothing to disconnect and no claim to
    // check: `VoiceLink::leave` is documented idempotent.
    let Some(guild_id) = joined_guild.take() else {
        return Ok(());
    };
    rt.block_on(async {
        match tokio::time::timeout(OP_TIMEOUT, manager.remove(guild_id)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(DiscordError::Voice(format!(
                "leave failed: {}",
                join_failure_reason(&e)
            ))),
            Err(_elapsed) => Err(DiscordError::Voice(format!(
                "leaving the voice channel did not complete within {}s",
                OP_TIMEOUT.as_secs()
            ))),
        }
    })
}

/// The single place a `songbird::error::JoinError` becomes text that can reach
/// a public Discord channel.
///
/// `JoinError` is `#[non_exhaustive]` and this crate depends on `songbird`
/// with a caret requirement, so a minor bump can add a variant — and a new
/// variant's `Display` need not be the fixed prose today's five are. Two of
/// the current ones already wrap a foreign error (`Serenity`, `Twilight`);
/// only the variants named below are known to render text this crate has
/// read, so only those are passed through. Anything else — today's wrapped
/// errors, tomorrow's additions — collapses to one fixed sentence.
///
/// Passing `Display` through for the known variants rather than restating
/// songbird's wording keeps the reasons the channel sees byte-identical to
/// what it sees today, with nothing to drift out of step.
fn join_failure_reason(e: &songbird::error::JoinError) -> String {
    use songbird::error::JoinError;
    match e {
        JoinError::Dropped
        | JoinError::NoSender
        | JoinError::NoCall
        | JoinError::TimedOut
        | JoinError::Driver(_) => e.to_string(),
        _ => "the voice connection failed for a reason astar does not recognise".to_string(),
    }
}

/// Check whether the gateway supervisor task has ended, and if so, consume
/// its `JoinHandle` and report why (panic vs. an unexpected clean exit —
/// `stop` being set is handled by the caller before this ever runs).
///
/// Returns `true` exactly when the task is gone, whatever the reason: either
/// way, the link can no longer do anything useful and the caller marks it
/// failed.
fn gateway_died(
    rt: &tokio::runtime::Runtime,
    task: &mut Option<tokio::task::JoinHandle<()>>,
) -> bool {
    let Some(handle) = task else {
        return false;
    };
    if !handle.is_finished() {
        return false;
    }
    let handle = task.take().expect("checked Some above");
    match rt.block_on(handle) {
        Ok(()) => {
            eprintln!("astar-discord: gateway supervisor task ended unexpectedly");
        }
        Err(e) if e.is_panic() => {
            eprintln!("astar-discord: gateway supervisor task panicked");
        }
        Err(_) => {
            eprintln!("astar-discord: gateway supervisor task was cancelled");
        }
    }
    true
}

/// How far one join attempt got, as a fact about the bot's position
/// rather than about the error message.
///
/// The distinction only matters for one thing — whether something must
/// later be able to LEAVE the channel — so it is separated from the
/// reporting and given its own exhaustive rule in
/// [`leaves_bot_in_channel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptEnd {
    /// The gateway never reached READY, so `Songbird::join` was never
    /// called. Nothing was asked of Discord.
    NotReady,
    /// The budget was spent before this attempt could be made. Again,
    /// nothing was asked of Discord on this pass.
    OutOfTime,
    /// songbird answered with a `JoinError` raised BEFORE the voice-state
    /// update went out, so nothing was asked of Discord. See
    /// [`refusal_end`] for which errors those are.
    RefusedBeforeSending,
    /// songbird answered with a `JoinError` raised AFTER the voice-state
    /// update went out: the request that puts the bot in the channel had
    /// already been sent, and songbird does not undo it.
    RefusedAfterSending,
    /// `Songbird::join` was still running when the budget ran out, so what
    /// it had already done is unknown.
    TimedOutMidFlight,
    /// The channel was joined and the `Call` lock or the stream install
    /// then ran out of time.
    StreamInstallFailed,
    /// Joined, and the conference audio is installed as the call's track.
    Playing,
}

/// Whether an attempt that ended this way must be assumed to have left the
/// bot sitting in the voice channel.
///
/// **When in doubt the answer is `true`.** `Songbird::join` (songbird 0.6.0
/// `src/manager.rs:240-256`) is two stages: `call.join(channel_id)` sends
/// the voice-state update that actually puts the bot in the channel and
/// returns in milliseconds, then the returned future waits for Discord to
/// confirm — and it is that second stage which spends the ten-second
/// budget. So an outer timeout firing means, realistically, that the
/// request was already sent; and so does a `JoinError` raised by that
/// second stage, which is why a refusal is not one answer but two (see
/// [`refusal_end`]). (`join_inner`'s `get_or_insert` has registered a
/// `Call` before either stage, too; songbird's own doc on `join` says an
/// `Err` still leaves a `Call` reachable through `get`.)
///
/// Being wrong in the `true` direction costs one `Songbird::remove` that
/// had nothing to remove — `manager.rs:346-350` leaves and drops the call
/// from its map whatever state it was in. Being wrong in the `false`
/// direction is the stranded-bot bug: nothing records the session, so
/// [`handle_leave`] returns early and the bot sits in the channel beside
/// its own "Not connected — ..." line until the whole gateway goes down —
/// and a gateway that outlives the session does not go down.
///
/// Exhaustive on purpose, with no wildcard arm: a new way for an attempt
/// to end must be classified here, not defaulted.
fn leaves_bot_in_channel(end: AttemptEnd) -> bool {
    match end {
        // Nothing reached Discord at all: two ends that never called
        // `Songbird::join`, and one where songbird failed before it could
        // send the voice-state update.
        AttemptEnd::NotReady | AttemptEnd::OutOfTime | AttemptEnd::RefusedBeforeSending => false,
        // Known to be there, or unknown and therefore assumed to be —
        // including a refusal raised after the update was already sent.
        AttemptEnd::RefusedAfterSending
        | AttemptEnd::TimedOutMidFlight
        | AttemptEnd::StreamInstallFailed
        | AttemptEnd::Playing => true,
    }
}

/// Which side of the voice-state update a `JoinError` was raised on.
///
/// **This is the fix for a real stranded-bot bug**, not a tidy-up. "songbird
/// refused it" was treated as "the bot is not in the channel", and for the
/// commonest flaky-voice failure that is false. `Songbird::join`
/// (songbird 0.6.0 `src/manager.rs:240-256`) is two stages:
///
/// * **stage 1** is `Call::join` → `Call::join_inner` (`src/handler.rs:226-245`),
///   which ends in `self.update().await` — the voice-state update that PUTS
///   THE BOT IN THE CHANNEL. **Two** things can fail before that lands: no
///   shard to send on (`NoSender`, raised by `update` itself when the `Call`
///   has no websocket — `src/handler.rs:443-455`), and serenity refusing
///   the send (`Serenity`, from `SerenityShardHandle::send` via
///   `src/shards.rs:186`). `NoCall` is **not** one of them: in songbird
///   0.6.0 it is raised in exactly one place, `Songbird::leave_inner` when
///   the guild has no `Call` at all (`src/manager.rs:328`), and `join` goes
///   through `get_or_insert`, which creates one. It is named in the arm
///   below for completeness — it is pre-send by construction, and a
///   version that started raising it from `join` would have to be
///   pre-send too — not because a join can produce it.
/// * **stage 2** is awaiting Discord's confirmation (`src/join.rs:160-168`),
///   which yields `TimedOut`, `Dropped` or `Driver(_)` — every one of them
///   AFTER the update was sent. `join_inner` does not clean up on `Err`, and
///   songbird says as much itself: the docs on `TimedOut` and `Driver` both
///   read "Users should `leave` the server on the gateway before
///   re-attempting connection", and `JoinError::should_leave_server()`
///   (`src/error.rs:57-64`) is true for `TimedOut`. That predicate is
///   corroboration, not the rule — it is false for `Dropped` and `Driver`,
///   which are equally post-send.
///
/// **The wildcard is deliberate, and it is on the SAFE side.** `JoinError`
/// is `#[non_exhaustive]` and this crate takes songbird with a caret
/// requirement, so a minor bump may add a variant. An unknown variant
/// classified as "never reached the channel" is the stranded-bot bug again;
/// classified as "may be in the channel" it costs one `Songbird::remove`
/// that has nothing to remove. So only the pre-send errors are named, and
/// everything else — today's and tomorrow's — lands on
/// [`AttemptEnd::RefusedAfterSending`].
fn refusal_end(e: &songbird::error::JoinError) -> AttemptEnd {
    use songbird::error::JoinError;
    match e {
        // `Twilight` is not in this build: songbird's `twilight` feature is
        // off, and that variant is `#[cfg(feature = "twilight")]`.
        // `NoCall` cannot come from `join` in 0.6.0 at all (see above); it
        // is listed so that the two errors that CAN and the one that is
        // pre-send by construction read as one rule.
        JoinError::NoSender | JoinError::NoCall | JoinError::Serenity(_) => {
            AttemptEnd::RefusedBeforeSending
        }
        _ => AttemptEnd::RefusedAfterSending,
    }
}

/// What one join reached — which is not the same question as whether it
/// succeeded.
///
/// Two facts, deliberately kept apart. `result` is what the operator is
/// told; `in_channel` is whether the bot is (or may be) in the voice
/// channel now. They diverge whenever an attempt reached Discord and then
/// went wrong: `Songbird::join` can succeed and the stream install behind
/// it time out, and `Songbird::join` can itself be interrupted mid-flight
/// with the voice-state update already sent.
///
/// Recording the session only on a fully successful join left nothing able to
/// leave that channel again — both the explicit `Leave` and a handle's `Drop`
/// reach [`handle_leave`], which returns early when no guild is recorded. The
/// bot stayed put beside its own "Not connected — ..." line, and was removed
/// only as a side effect of the whole gateway going down. A gateway that
/// outlives the session does not go down, so the bookkeeping has to be true
/// instead.
struct JoinOutcome {
    /// Whether something must later be able to leave this channel.
    /// [`leaves_bot_in_channel`] is the whole rule.
    in_channel: bool,
    result: Result<(), DiscordError>,
}

impl JoinOutcome {
    /// One attempt's outcome: what the operator is told, plus where the
    /// bot has to be assumed to be, which follows from `end` alone.
    fn of(end: AttemptEnd, result: Result<(), DiscordError>) -> Self {
        Self {
            in_channel: leaves_bot_in_channel(end),
            result,
        }
    }

    /// The same, for the failures — which is all of them but one.
    fn failed(end: AttemptEnd, reason: String) -> Self {
        Self::of(end, Err(DiscordError::Voice(reason)))
    }
}

/// Fold one join's outcome into the command loop's two pieces of state, and
/// report the outcome.
///
/// Pure, and separate from [`handle_join`] for that reason: the rule it
/// applies is the one that was wrong before, and it is the only part of the
/// join path a test can drive without Discord.
///
/// * `joined_guild` follows `in_channel`, NOT success. A session the bot is
///   in is a session something must be able to leave, and a failed join is
///   exactly when that matters.
/// * `pcm_tx` follows success only, unchanged: a failed join leaves the frame
///   path where it was, so frames are dropped at the command loop rather than
///   queued into a channel nobody reads.
///
/// Neither is ever cleared here. A join that never reached the channel leaves
/// any earlier session's guild alone — `Leave` is what clears it — because
/// the earlier session may still be live.
fn apply_join_outcome(
    outcome: JoinOutcome,
    guild_id: songbird::id::GuildId,
    tx: StdSender<Vec<u8>>,
    pcm_tx: &mut Option<StdSender<Vec<u8>>>,
    joined_guild: &mut Option<songbird::id::GuildId>,
) -> Result<(), DiscordError> {
    if outcome.in_channel {
        *joined_guild = Some(guild_id);
    }
    outcome.result?;
    *pcm_tx = Some(tx);
    Ok(())
}

/// The command loop's per-session state, as one bundle a join may write.
///
/// Three fields that are written together on a join and read together for
/// the rest of the session: where audio frames go, which guild has to be
/// left again, and whose talk edges are being watched.
struct JoinState<'a> {
    pcm_tx: &'a mut Option<StdSender<Vec<u8>>>,
    joined_guild: &'a mut Option<songbird::id::GuildId>,
    talk: &'a mut Option<TalkSession>,
    /// Not written by the join — it belongs to the gateway, not to one
    /// session — but read by it: a join mints the handler that watches the
    /// track it installs, and clears the previous failure on success.
    tracks: &'a TrackSupervisor,
}

/// The handlers one join registers inside the `Call` lock.
///
/// Two of them, and they answer different questions: [`TalkSession`] is who
/// is speaking IN the channel (absent for an audio-only gateway — nobody is
/// listening for it), and [`TrackWatch`] is whether this node is still
/// audible in it, which matters whether or not there is a Commander.
struct SessionHandlers<'a> {
    talk: Option<TalkSession>,
    tracks: &'a TrackSupervisor,
}

/// Handle `GatewayCmd::Join`: build the streaming input once, join the
/// channel, and hand the input to the driver as the call's sole track.
///
/// Returns the real outcome; [`apply_join_outcome`] holds the rule for what
/// that outcome does to the command loop's state.
fn handle_join(
    rt: &tokio::runtime::Runtime,
    manager: &Arc<songbird::Songbird>,
    gateway_ready: &AtomicBool,
    guild: u64,
    channel: u64,
    events: Option<&StdSender<GatewayEvent>>,
    state: JoinState<'_>,
) -> Result<(), DiscordError> {
    let JoinState {
        pcm_tx,
        joined_guild,
        talk,
        tracks,
    } = state;
    // Before anything is attempted, so nothing is in the channel and nothing
    // is recorded.
    let (Some(guild_id), Some(channel_id)) = (to_guild_id(guild), to_channel_id(channel)) else {
        return Err(DiscordError::Voice(
            "join requested with a zero guild or channel id".into(),
        ));
    };

    let (tx, input) = new_streaming_input();
    let deadline = Instant::now() + JOIN_DEADLINE;

    // Built before the join, because `join_and_play` registers its handlers
    // inside the same `Call` lock that installs the audio track. `None` for
    // an audio-only gateway: no events sender means nobody is listening, so
    // no receive handler is registered at all.
    let session = events.cloned().map(TalkSession::new);

    let outcome = rt.block_on(join_and_play(
        manager,
        gateway_ready,
        guild_id,
        channel_id,
        input,
        SessionHandlers {
            talk: session.clone(),
            tracks,
        },
        deadline,
    ));

    let result = apply_join_outcome(outcome, guild_id, tx, pcm_tx, joined_guild);
    // A new track is playing, so the previous one's death has stopped being
    // the truth about this bridge. Cleared only on success: a join that
    // failed installed nothing, and forgetting the reason would leave
    // `/status` with a working-looking bridge and no audio.
    //
    // And when there WAS a death to clear, the node is told — otherwise the
    // rule engine's `last_error` keeps reporting an outage that is over for
    // the life of the process, and the next genuine outage is never
    // announced because the "already said it once" flag is still set.
    // `recover` answers both questions in one step, so an ordinary join —
    // the overwhelming majority — says nothing at all.
    if result.is_ok()
        && tracks.recover()
        && let Some(events) = events
    {
        let _ = events.send(GatewayEvent::AudioRecovered);
    }
    // Installed only on a fully successful join, which is exactly when the
    // handlers were registered. The session it replaces is over: say that
    // its talkers stopped, so the Commander is not left believing one of
    // them is mid-word, and stop the handlers songbird may still hold for
    // it from reporting anything further.
    if result.is_ok()
        && let Some(session) = session
        && let Some(previous) = talk.replace(session)
    {
        drop(previous.end());
    }
    result
}

/// Wait for the gateway, join, and install the streaming input — all inside
/// one `deadline`.
///
/// Two distinct waits, and they are not the same failure:
/// * **before READY** songbird has no user id and `Songbird::join` would
///   panic, so this polls the flag instead of calling in;
/// * **after READY** the shard may still be registering with songbird, which
///   surfaces as `JoinError::NoSender` ("no gateway destination"). That is a
///   retry, not a verdict — it is the failure the first join after a daemon
///   start would otherwise hit every time.
///
/// Any other `JoinError` is final; the text that reaches the caller is
/// whatever [`join_failure_reason`] allows through — `Display` verbatim for
/// the five variants it trusts, one fixed sentence for everything else
/// (including the two variants that wrap a foreign error).
///
/// **Every await in here is inside `deadline`** — the READY poll, each
/// `Songbird::join` attempt, and the `Call` lock that installs the stream —
/// so this function always returns, and `handle_join`'s `rt.block_on` around
/// it always unblocks. That matters beyond reporting a tidy error: the
/// command loop is single-threaded, so an await that never finished here
/// would take `Frame`, `Leave` and `Shutdown` with it.
///
/// **The return says where the bot ended up, not only how it went**, and
/// every return below names the [`AttemptEnd`] that says so rather than
/// asserting a position of its own. Two of them are KNOWN not to have
/// reached the channel because nothing was asked of Discord at all
/// (`NotReady`, `OutOfTime`). A `JoinError` from songbird is NOT one
/// answer: only the errors raised before the voice-state update went out
/// are safe to call "not in the channel", and [`refusal_end`] is the rule
/// that separates them — a timeout inside songbird's own second stage is
/// the commonest flaky-voice failure there is, and the bot is sitting in
/// the channel when it happens. One end is genuinely UNKNOWN — the outer
/// timeout around `Songbird::join` — and is therefore treated as being in
/// the channel; [`leaves_bot_in_channel`] carries the songbird reasoning
/// for that. The two after the join are known to be in it.
///
/// Keep that discipline if this function is ever changed: it is what makes
/// a `Leave` after a failed join real (see [`JoinOutcome`]).
async fn join_and_play(
    manager: &Arc<songbird::Songbird>,
    gateway_ready: &AtomicBool,
    guild_id: songbird::id::GuildId,
    channel_id: songbird::id::ChannelId,
    input: songbird::input::Input,
    handlers: SessionHandlers<'_>,
    deadline: Instant,
) -> JoinOutcome {
    let SessionHandlers { talk, tracks } = handlers;
    // ---- nothing below has been asked of Discord yet: not in the channel --
    while !gateway_ready.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            return JoinOutcome::failed(
                AttemptEnd::NotReady,
                "the Discord gateway did not reach READY in time".to_string(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let call = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // The budget is spent and this pass never called in, so this
            // attempt asked Discord for nothing.
            return JoinOutcome::failed(AttemptEnd::OutOfTime, "join timed out".to_string());
        }
        match tokio::time::timeout(remaining, manager.join(guild_id, channel_id)).await {
            Ok(Ok(call)) => break call,
            // The shard has not finished registering with songbird yet.
            // Retrying is safe with respect to position: this arm is only
            // reached when `Songbird::join` RETURNED, so no earlier pass of
            // this loop can have left a request in flight — the
            // mid-flight-timeout arm below returns rather than looping. If
            // this loop is ever restructured so that it can continue after
            // a timeout, the conservative answer has to survive across
            // attempts.
            Ok(Err(songbird::error::JoinError::NoSender)) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Ok(Err(e)) => {
                // Which END this is decides whether a `Leave` will really
                // be performed later; `refusal_end` carries the whole rule
                // and the songbird reasoning behind it.
                return JoinOutcome::failed(
                    refusal_end(&e),
                    format!("join failed: {}", join_failure_reason(&e)),
                );
            }
            // Interrupted with the outcome unknown. `Songbird::join`'s
            // first stage — the one that actually puts the bot in the
            // channel — completes in milliseconds, so by the time the
            // ten-second budget expires the request has almost certainly
            // been sent. Reported as in-channel for that reason; see
            // `leaves_bot_in_channel`.
            Err(_elapsed) => {
                return JoinOutcome::failed(
                    AttemptEnd::TimedOutMidFlight,
                    "join timed out".to_string(),
                );
            }
        }
    };
    // ---- in the channel from here on, whatever happens next --------------

    // The lock is inside the budget too. songbird holds a `Call`'s mutex
    // only briefly, so in practice this is uncontended — but it is an await
    // like any other, and one that never completed would leave the command
    // loop wedged in `rt.block_on` for good: no `Frame`, no `Leave`, no
    // `Shutdown`, and a `Drop` with nothing bounded to wait on. Bounding it
    // is what makes the join path finite, rather than finite everywhere but
    // its last line.
    let remaining = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(remaining, call.lock()).await {
        Ok(mut handler) => {
            if let Some(talk) = talk {
                // `remove_all_global_events` FIRST, and it matters.
                // songbird keeps ONE `Call` per guild and `Songbird::join`
                // reuses it, so a second join onto the same guild would
                // otherwise leave the previous session's handlers
                // registered alongside the new ones: two handlers over two
                // different `Talkers`, each advancing its own debounce
                // counters from the same ticks and each emitting edges.
                // This crate registers no other global events, and
                // songbird's own machinery does not use the global store
                // for anything of its own (`EventStore::remove_handlers`
                // clears only handlers a caller added), so clearing it is
                // safe as well as necessary.
                handler.remove_all_global_events();
                handler.add_global_event(
                    songbird::Event::Core(songbird::CoreEvent::SpeakingStateUpdate),
                    talk.clone(),
                );
                handler.add_global_event(
                    songbird::Event::Core(songbird::CoreEvent::VoiceTick),
                    talk.clone(),
                );
                // The third one belongs HERE and nowhere else, for the
                // reason above: `remove_all_global_events` clears every
                // global handler, so one registered anywhere but in this
                // block would be silently dropped by the next join onto
                // the same guild and nobody would notice — the symptom is
                // a mic that only sticks open after a rejoin.
                //
                // Without it, a driver that drops with no `Leave` simply
                // stops delivering ticks: the holder stays latched
                // `talking = true`, the silence unkey never fires, and the
                // mic is open until the 180 s hard timer.
                handler.add_global_event(
                    songbird::Event::Core(songbird::CoreEvent::DriverDisconnect),
                    talk,
                );
            }
            // **The events are attached BEFORE the track is handed over,
            // not registered on the handle afterwards**, and the difference
            // is real rather than stylistic.
            //
            // `TrackHandle::add_event` sends a `TrackCommand::AddEvent`
            // that the mixer only drains from `audio_commands_events()` —
            // and the live scheduler calls that at the END of a cycle,
            // after `handle_task_msgs()` has installed the track and after
            // `mix_and_build_packet()` has already mixed it once
            // (`songbird-0.6.0/src/driver/scheduler/live.rs:192-277`,
            // `src/driver/tasks/mixer/mod.rs:417-424`). So a handler
            // registered that way is in place only from the SECOND cycle.
            // `Track::events` is the other door: `decompose_track` hands
            // that store straight to the event task as the track's own
            // (`src/driver/tasks/mixer/track.rs:20-43`,
            // `mixer/mod.rs:389-398`), and that happens inside
            // `handle_task_msgs()` — before the first mix, in the same
            // cycle. It costs nothing and removes the window entirely.
            //
            // The window was probably never fatal — the input is lazy, so
            // the first mix only dispatches the parse to the task pool and
            // returns `Waiting` (`mixer/track.rs:176-217`), which is why
            // the original failure took ~40 ms rather than one tick — but
            // "probably not fatal" is not a thing to build a supervisor on.
            //
            // songbird 0.6.0 has no builder method for this; the public
            // `Track::events` field, documented for exactly this purpose,
            // is the whole API.
            let mut track = songbird::tracks::Track::new(input);
            // Minted HERE, one line above the install, because this is the
            // only place a track is replaced. See `watch_next`.
            let watch = tracks.watch_next();
            // The `Duration` is the store's "now", used only to compute the
            // first firing time of a PERIODIC or DELAYED event
            // (`EventData::compute_activation`, `src/events/data.rs:32-43`).
            // These two are neither, so it is read and discarded; zero is
            // the honest value for "this has no schedule".
            track.events.add_event(
                songbird::events::EventData::new(
                    songbird::Event::Track(songbird::TrackEvent::Error),
                    watch.clone(),
                ),
                Duration::ZERO,
            );
            track.events.add_event(
                songbird::events::EventData::new(
                    songbird::Event::Track(songbird::TrackEvent::End),
                    watch,
                ),
                Duration::ZERO,
            );
            // `play_only`, which is exactly what `play_only_input` does
            // with the `Track` this builds (`src/driver/mod.rs:172-204`):
            // it stops every other input on the call, so the bridge is
            // always the channel's sole track.
            drop(handler.play_only(track));
        }
        // The bot is in the channel at this point and stays there; only the
        // playback install failed. The caller reports the failure — and
        // because this end is classified as in-channel, the guild is
        // recorded, so the `Leave` that follows (the operator's, or the one
        // a dropped voice handle sends on its way out) really does call
        // `Songbird::remove` rather than returning early on an empty
        // `joined_guild`.
        Err(_elapsed) => {
            return JoinOutcome::failed(
                AttemptEnd::StreamInstallFailed,
                "joined the channel but timed out installing the audio stream".to_string(),
            );
        }
    }
    JoinOutcome::of(AttemptEnd::Playing, Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The audio path into Discord is songbird's raw container, which tags
    /// its one track `CODEC_TYPE_PCM_F32LE` and leaves decoding to whatever
    /// symphonia codecs were compiled in. This crate takes songbird with
    /// `default-features = false`, and for the whole life of the first
    /// bridge that meant NO PCM decoder existed: every track died ~40 ms
    /// after starting with `Parse(DecodeError("no compatible track found"))`
    /// while `/status` said `connected: true` and `frames_sent` climbed
    /// against a track that was already gone. Nothing noticed, because
    /// every test channel was fed an empty conference and silence is what
    /// silence sounds like.
    ///
    /// So this asks songbird's own registry — the one the mixer consults —
    /// for the decoder the streaming input needs. It fails if symphonia's
    /// `pcm` feature is ever dropped from this crate's dependencies again.
    #[test]
    fn songbird_can_decode_the_pcm_this_crate_streams_into_it() {
        use songbird::input::core::codecs::CODEC_TYPE_PCM_F32LE;
        assert!(
            songbird::input::codecs::get_codec_registry()
                .get_codec(CODEC_TYPE_PCM_F32LE)
                .is_some(),
            "no symphonia decoder for PCM_F32LE: the bridge would join, report \
             connected, and deliver no audio at all"
        );
    }

    /// A handle onto a channel nobody is reading yet, plus that receiver.
    /// No thread, no runtime, no network — the whole point of a handle is
    /// that it only enqueues.
    fn test_handle(channel: u64) -> (GatewayHandle, StdReceiver<GatewayCmd>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (GatewayHandle { tx, channel }, rx)
    }

    #[test]
    fn every_op_enqueues_and_returns_rather_than_calling_discord() {
        // The property this pins is a timing one: these four methods run
        // on the node's 50 ms pump thread, inside the Commander's rule
        // evaluation. Each must leave one command on the queue and come
        // straight back; a REST call here would stall the audio path.
        let (mut ops, rx) = test_handle(555);

        ops.reply(99, "no");
        ops.announce("<@1> has the mic");
        ops.set_mute(7, true);
        ops.panel(Some("kd0xyz"));
        ops.panel(None);

        let queued: Vec<GatewayCmd> = rx.try_iter().collect();
        assert_eq!(
            queued.len(),
            5,
            "one command per op, and none of them blocked"
        );

        match &queued[0] {
            GatewayCmd::Reply { interaction, text } => {
                assert_eq!(*interaction, 99);
                assert_eq!(text, "no");
            }
            _ => panic!("reply must enqueue a Reply"),
        }
        match &queued[1] {
            GatewayCmd::PostText { channel, text } => {
                assert_eq!(*channel, 555, "an announce goes to the handle's channel");
                assert_eq!(text, "<@1> has the mic");
            }
            _ => panic!("announce must enqueue a PostText"),
        }
        match &queued[2] {
            GatewayCmd::SetMute { user, mute } => {
                assert_eq!(*user, 7);
                assert!(*mute);
            }
            _ => panic!("set_mute must enqueue a SetMute"),
        }
        match &queued[3] {
            GatewayCmd::Panel { channel, talking } => {
                assert_eq!(*channel, 555);
                assert_eq!(talking.as_deref(), Some("kd0xyz"));
            }
            _ => panic!("panel must enqueue a Panel"),
        }
        match &queued[4] {
            GatewayCmd::Panel { talking, .. } => assert_eq!(*talking, None),
            _ => panic!("panel must enqueue a Panel"),
        }
    }

    #[test]
    fn an_op_on_a_dead_gateway_is_dropped_rather_than_panicking() {
        // The runtime thread has gone, so the receiver is gone. A mute in
        // a READY sweep must not take the node's pump thread down with it.
        let (mut ops, rx) = test_handle(555);
        drop(rx);

        ops.set_mute(7, true);
        ops.announce("still here");
    }

    #[test]
    fn connect_with_refuses_an_empty_token_exactly_as_connect_does() {
        assert!(matches!(
            Gateway::connect_with("", None).err(),
            Some(DiscordError::NotConfigured)
        ));
        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(matches!(
            Gateway::connect_with(
                "",
                Some(GatewayOptions {
                    guild: 1,
                    commands: true,
                    events: tx,
                })
            )
            .err(),
            Some(DiscordError::NotConfigured)
        ));
    }

    /// The registration payload, as serenity would send it.
    ///
    /// This is as close to the wire as a test can get without a network:
    /// `CreateCommand` serialises to exactly the JSON that is posted to
    /// so the option TYPE numbers, the choice lists and the required flags
    /// are all checked here rather than discovered as a 400 at start-up.
    fn registration_json() -> serde_json::Value {
        serde_json::to_value(command_builders()).expect("the command builders must serialise")
    }

    #[test]
    fn the_registration_payload_is_the_table_and_nothing_else() {
        let json = registration_json();
        let commands = json.as_array().expect("an array of commands");

        let names: Vec<&str> = commands
            .iter()
            .map(|c| c["name"].as_str().expect("every command has a name"))
            .collect();
        let expected: Vec<&str> = COMMANDS.iter().map(|d| d.name).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn no_command_is_hidden_behind_a_discord_permission() {
        // The node enforces the tiers and refuses ephemerally. Hiding a
        // command in Discord's UI would make the node's answer depend on a
        // server's permission state — and would leave someone demoted in
        // the node still seeing it.
        let json = registration_json();
        for cmd in json.as_array().expect("an array of commands") {
            assert!(
                cmd.get("default_member_permissions")
                    .is_none_or(serde_json::Value::is_null),
                "/{} must be visible to everyone: {cmd}",
                cmd["name"]
            );
        }
    }

    #[test]
    fn dial_is_registered_as_three_subcommands_with_the_options_the_parser_reads() {
        // CommandOptionType::SubCommand is 1 and ::String is 3 on the
        // wire; `definitions_and_parser_agree` covers the names, and this
        // covers the mapping onto serenity's builders.
        let json = registration_json();
        let dial = json
            .as_array()
            .expect("an array")
            .iter()
            .find(|c| c["name"] == "dial")
            .expect("/dial is registered")
            .clone();
        let subs = dial["options"].as_array().expect("subcommands");
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0]["name"], "allstar");
        assert_eq!(subs[0]["type"], 1);
        assert_eq!(
            subs[0]["required"], false,
            "a subcommand is never itself required"
        );
        let node = &subs[0]["options"].as_array().expect("an option")[0];
        assert_eq!(node["name"], "node");
        assert_eq!(node["type"], 3);
        assert_eq!(node["required"], true);
        assert_eq!(subs[1]["name"], "m17");
        assert_eq!(subs[2]["name"], "dstar");
    }

    #[test]
    fn mic_registers_an_optional_on_off_choice_and_bot_a_required_one() {
        let json = registration_json();
        let commands = json.as_array().expect("an array").clone();
        let find = |name: &str| {
            commands
                .iter()
                .find(|c| c["name"] == name)
                .unwrap_or_else(|| panic!("/{name} is registered"))
                .clone()
        };

        let mic_state = find("mic")["options"].as_array().expect("an option")[0].clone();
        assert_eq!(mic_state["name"], "state");
        assert_eq!(
            mic_state["required"], false,
            "bare /mic must stay legal: it is how the mic is taken"
        );
        let choices: Vec<&str> = mic_state["choices"]
            .as_array()
            .expect("choices")
            .iter()
            .map(|c| c["value"].as_str().expect("a string value"))
            .collect();
        assert_eq!(choices, vec!["on", "off"]);

        let bot_state = find("bot")["options"].as_array().expect("an option")[0].clone();
        assert_eq!(bot_state["required"], true);
    }

    #[test]
    fn a_user_option_is_registered_as_a_user_picker() {
        // Type 6 is USER: Discord resolves the snowflake for us, which is
        // why `parse_command` never has to parse a mention out of a
        // string.
        let json = registration_json();
        let approve = json
            .as_array()
            .expect("an array")
            .iter()
            .find(|c| c["name"] == "approve")
            .expect("/approve is registered")
            .clone();
        let user = &approve["options"].as_array().expect("an option")[0];
        assert_eq!(user["name"], "user");
        assert_eq!(user["type"], 6);
        assert_eq!(user["required"], true);
    }

    #[test]
    fn a_non_http_error_contributes_no_status() {
        // `status_suffix` must never interpolate the error itself: the
        // request whose failure it reports carries a live interaction
        // token in its URL.
        assert_eq!(status_suffix(&serenity::Error::Other("no")), "");
        assert_eq!(http_status(&serenity::Error::Other("no")), None);
    }

    #[test]
    fn a_missing_application_id_is_named_in_the_suffix() {
        // The one status-less failure this module has actually met in the
        // field (2026-09-26): every slash command answered "thinking…" and
        // nothing else, and the log said only "could not answer an
        // interaction". That line has to say which of serenity's
        // status-less errors it was, or the next such failure is opaque
        // again — without printing the error, which for the transport
        // variants carries the interaction token in a URL.
        let e = serenity::Error::Http(serenity::http::HttpError::ApplicationIdMissing);
        assert_eq!(status_suffix(&e), " (no application id)");
        assert_eq!(http_status(&e), None);
    }

    #[test]
    fn a_rest_client_built_from_the_token_alone_cannot_edit_an_interaction() {
        // The mechanism behind the field failure above. Acknowledging an
        // interaction is addressed by the interaction's own id, but editing
        // the acknowledgement is a webhook call addressed by the
        // APPLICATION id — which `Http::new` does not know, and which
        // serenity only ever sets on the `Client`'s own `Http`, from READY.
        // The refusal happens before any request is built, so this touches
        // no network; it pins the precondition `Handler::ready` supplies to
        // the REST client the command loop answers with.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        let http = serenity::http::Http::new("not-a-token");
        assert_eq!(http.application_id(), None);
        let outcome = rt.block_on(http.edit_original_interaction_response(
            "not-an-interaction-token",
            &serde_json::json!({ "content": "x" }),
            Vec::new(),
        ));
        assert!(
            matches!(
                outcome,
                Err(serenity::Error::Http(
                    serenity::http::HttpError::ApplicationIdMissing
                ))
            ),
            "expected ApplicationIdMissing, got {:?}",
            outcome.err().map(|e| status_suffix(&e))
        );
    }

    #[test]
    fn connect_refuses_an_empty_token() {
        // No thread is spawned and no network is touched: an unset token is a
        // configuration answer, not a connection attempt.
        assert!(matches!(
            Gateway::connect("").err(),
            Some(DiscordError::NotConfigured)
        ));
    }

    /// A runtime shaped like the real one, for the tests below that need to
    /// drive an async helper. Nothing here touches the network.
    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build the test runtime")
    }

    #[test]
    fn a_join_interrupted_mid_flight_is_assumed_to_have_reached_the_channel() {
        // The unsafe side of a genuine ambiguity. `Songbird::join`'s first
        // stage sends the voice-state update — the thing that actually puts
        // the bot in the channel — and returns in milliseconds; the second
        // stage is what spends the ten-second budget waiting for Discord to
        // confirm. So a budget that expires means, realistically, that the
        // request went out and the bot may be showing in the channel.
        //
        // Answering `false` here is the stranded-bot bug: nothing records
        // the session, `handle_leave` returns early, and the bot sits there
        // beside its own "Not connected" line. Answering `true` wrongly
        // costs one `Songbird::remove` that has nothing to remove.
        assert!(leaves_bot_in_channel(AttemptEnd::TimedOutMidFlight));
    }

    #[test]
    fn the_ends_that_never_reached_discord_record_nothing() {
        // These three are not "probably not in the channel" — they are
        // known: two of them never called `Songbird::join` at all, and the
        // third is songbird failing before it could send the voice-state
        // update.
        assert!(!leaves_bot_in_channel(AttemptEnd::NotReady));
        assert!(!leaves_bot_in_channel(AttemptEnd::OutOfTime));
        assert!(!leaves_bot_in_channel(AttemptEnd::RefusedBeforeSending));
    }

    #[test]
    fn a_refusal_raised_after_the_voice_state_update_leaves_the_bot_in_the_channel() {
        // The stranded-bot bug, in its commonest form. songbird's own
        // second stage times out (`JoinError::TimedOut`) AFTER
        // `Call::join_inner` has already sent the voice-state update that
        // puts the bot in the channel, and `join_inner` does not undo it.
        // Classifying that as "never reached the channel" — which is what
        // one undivided `Refused` did — records no guild, so `handle_leave`
        // returns early and nothing ever calls `Songbird::remove`: the bot
        // sits in the voice channel beside its own "Not connected — …"
        // line until the daemon restarts.
        assert!(leaves_bot_in_channel(AttemptEnd::RefusedAfterSending));
    }

    #[test]
    fn a_join_error_is_classified_by_which_stage_raised_it() {
        use songbird::error::JoinError;

        // Stage 1 — `Call::join_inner` up to and including `update()`, the
        // send that puts the bot in the channel. Nothing reached Discord.
        assert_eq!(
            refusal_end(&JoinError::NoSender),
            AttemptEnd::RefusedBeforeSending
        );
        assert_eq!(
            refusal_end(&JoinError::NoCall),
            AttemptEnd::RefusedBeforeSending
        );

        // Stage 2 — awaiting Discord's confirmation. Both of these are
        // raised with the voice-state update already sent, so the bot may
        // well be showing in the channel.
        assert_eq!(
            refusal_end(&JoinError::TimedOut),
            AttemptEnd::RefusedAfterSending
        );
        assert_eq!(
            refusal_end(&JoinError::Dropped),
            AttemptEnd::RefusedAfterSending
        );
        assert_eq!(
            refusal_end(&JoinError::Driver(
                songbird::error::ConnectionError::TimedOut
            )),
            AttemptEnd::RefusedAfterSending
        );
    }

    #[test]
    fn everything_after_the_join_answered_is_in_the_channel() {
        assert!(leaves_bot_in_channel(AttemptEnd::StreamInstallFailed));
        assert!(leaves_bot_in_channel(AttemptEnd::Playing));
    }

    #[test]
    fn the_classification_is_what_builds_the_outcome() {
        // Not a restatement of the three tests above: this is the join
        // classification expressed as an outcome, which is the value
        // `apply_join_outcome` acts on. A `JoinOutcome` that ignored
        // `AttemptEnd` would pass the three tests above and fail this.
        assert!(
            JoinOutcome::failed(AttemptEnd::TimedOutMidFlight, "join timed out".to_string())
                .in_channel
        );
        assert!(
            !JoinOutcome::failed(AttemptEnd::RefusedBeforeSending, "join failed".to_string())
                .in_channel
        );
        assert!(
            JoinOutcome::failed(AttemptEnd::RefusedAfterSending, "join failed".to_string())
                .in_channel
        );
        assert!(JoinOutcome::of(AttemptEnd::Playing, Ok(())).in_channel);
    }

    /// The bug this whole type exists for: `Songbird::join` succeeded, the
    /// stream install timed out, so the operator is told the join failed —
    /// while the bot is in the voice channel. If the guild is not recorded,
    /// `handle_leave` returns early and NOTHING ever calls
    /// `Songbird::remove`: the bot sits there beside its own "Not connected
    /// — ..." line. That used to be hidden by the gateway going down with
    /// the bridge; a gateway that outlives the session does not.
    #[test]
    fn a_join_that_failed_after_reaching_the_channel_still_records_the_session() {
        let guild = to_guild_id(12_345).expect("the fixture guild id is non-zero");
        let (tx, _rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut pcm_tx = None;
        let mut joined_guild = None;

        let outcome = apply_join_outcome(
            JoinOutcome::failed(
                AttemptEnd::StreamInstallFailed,
                "joined the channel but timed out installing the audio stream".to_string(),
            ),
            guild,
            tx,
            &mut pcm_tx,
            &mut joined_guild,
        );

        assert_eq!(
            outcome,
            Err(DiscordError::Voice(
                "joined the channel but timed out installing the audio stream".into()
            )),
            "the operator still hears that the join failed"
        );
        assert_eq!(
            joined_guild,
            Some(guild),
            "the session must be recorded so a Leave can actually remove the bot"
        );
        assert!(
            pcm_tx.is_none(),
            "no audio path on a failed join: frames are dropped at the command loop"
        );
    }

    #[test]
    fn a_join_that_never_reached_the_channel_records_nothing() {
        // The mirror image, and the half that must NOT change: a join that
        // was refused or timed out before `Songbird::join` answered leaves
        // the bot nowhere, so a later `Leave` must not go asking songbird to
        // remove a call that never existed.
        let guild = to_guild_id(12_345).expect("the fixture guild id is non-zero");
        let (tx, _rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut pcm_tx = None;
        let mut joined_guild = None;

        let outcome = apply_join_outcome(
            JoinOutcome::failed(
                AttemptEnd::NotReady,
                "the Discord gateway did not reach READY in time".to_string(),
            ),
            guild,
            tx,
            &mut pcm_tx,
            &mut joined_guild,
        );

        assert!(outcome.is_err(), "the join failed and says so");
        assert_eq!(joined_guild, None, "nothing was joined, so nothing is left");
        assert!(pcm_tx.is_none());
    }

    #[test]
    fn a_successful_join_records_the_session_and_opens_the_audio_path() {
        let guild = to_guild_id(12_345).expect("the fixture guild id is non-zero");
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut pcm_tx = None;
        let mut joined_guild = None;

        assert_eq!(
            apply_join_outcome(
                JoinOutcome::of(AttemptEnd::Playing, Ok(())),
                guild,
                tx,
                &mut pcm_tx,
                &mut joined_guild
            ),
            Ok(())
        );

        assert_eq!(joined_guild, Some(guild));
        // Not merely `is_some()`: the sender installed must be the one whose
        // receiver feeds the track that was just handed to the driver.
        pcm_tx
            .expect("a successful join opens the frame path")
            .send(vec![1, 2, 3])
            .expect("the installed sender feeds the live stream");
        assert_eq!(rx.recv().expect("a frame arrives"), vec![1, 2, 3]);
    }

    #[test]
    fn a_failed_join_does_not_forget_a_session_that_is_still_live() {
        // A second Join attempted while one is up, which fails early: the
        // first session is untouched, so its guild must survive. Clearing it
        // here would strand that session exactly as not recording it does.
        let first = to_guild_id(111).expect("non-zero");
        let second = to_guild_id(222).expect("non-zero");
        let (tx, _rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut pcm_tx = None;
        let mut joined_guild = Some(first);

        let _ = apply_join_outcome(
            JoinOutcome::failed(AttemptEnd::OutOfTime, "join timed out".to_string()),
            second,
            tx,
            &mut pcm_tx,
            &mut joined_guild,
        );

        assert_eq!(
            joined_guild,
            Some(first),
            "the live session's guild must not be dropped by a later failed join"
        );
    }

    #[test]
    fn a_leave_songbird_refuses_is_an_error_not_a_silent_ok() {
        // `Ok` from a leave is precisely what the channel is told
        // "Disconnected — ... is no longer bridged into this channel." on. An
        // enqueue-only leave returned `Ok` for a disconnect nothing had
        // performed; this is the verification that replaces it.
        //
        // The manager has never joined anything, so `Songbird::remove`
        // answers `JoinError::NoCall` from its own call table — no network is
        // touched, and no join is attempted.
        let rt = test_runtime();
        let manager = songbird::Songbird::serenity_from_config(voice_config());
        let mut joined = to_guild_id(12_345);
        assert!(joined.is_some(), "the fixture guild id must be non-zero");

        let outcome = handle_leave(&rt, &manager, &mut joined);

        assert_eq!(
            outcome,
            Err(DiscordError::Voice(
                "leave failed: failed to join voice channel: tried to leave a non-existent call"
                    .into()
            )),
            "a refused leave must reach the caller as songbird's own reason"
        );
        assert!(
            joined.is_none(),
            "the guild id is cleared even on failure: retrying `remove` at Shutdown would only \
             repeat the same wait while Drop blocks on it"
        );
    }

    #[test]
    fn a_leave_with_nothing_joined_is_a_truthful_ok() {
        // `VoiceLink::leave` is documented idempotent, and nothing joined
        // means nothing to disconnect — so `Ok` here is a true statement, not
        // the unverified one above. It also never reaches songbird.
        let rt = test_runtime();
        let manager = songbird::Songbird::serenity_from_config(voice_config());
        let mut joined: Option<songbird::id::GuildId> = None;

        assert_eq!(handle_leave(&rt, &manager, &mut joined), Ok(()));
    }

    #[test]
    fn the_known_join_failures_read_exactly_as_songbird_words_them() {
        use songbird::error::JoinError;

        // These strings go into a public Discord channel. Pinning them
        // literally does two jobs: it proves the known variants still pass
        // through `Display` rather than collapsing into the fallback, and it
        // turns a change in songbird's wording into a CI failure rather than
        // a surprise in the channel.
        assert_eq!(
            join_failure_reason(&JoinError::NoCall),
            "failed to join voice channel: tried to leave a non-existent call"
        );
        assert_eq!(
            join_failure_reason(&JoinError::TimedOut),
            "failed to join voice channel: gateway response from Discord timed out"
        );
        assert_eq!(
            join_failure_reason(&JoinError::Dropped),
            "failed to join voice channel: request was cancelled/dropped"
        );
        assert_eq!(
            join_failure_reason(&JoinError::NoSender),
            "failed to join voice channel: no gateway destination"
        );
        // `Driver(_)` is the fifth trusted variant, and the one that wraps a
        // foreign error (`songbird::error::ConnectionError`) rather than
        // being a unit variant itself. Its safety rests entirely on
        // songbird's `Display` for `JoinError::Driver` discarding that inner
        // error by matching `Driver(_)` rather than `Driver(e) => e.fmt(f)` —
        // if a future songbird minor changed that arm to format the inner
        // error, `ConnectionError::Ws` debug-prints a tungstenite error
        // straight into a public Discord channel. This pins today's fixed
        // wording so that change fails CI instead of surfacing there.
        // `ConnectionError::TimedOut` is a unit variant, so it is
        // constructible despite the enum being `#[non_exhaustive]`.
        assert_eq!(
            join_failure_reason(&JoinError::Driver(
                songbird::error::ConnectionError::TimedOut
            )),
            "failed to join voice channel: establishing connection failed"
        );
        // The fallback arm itself cannot be exercised from here: the variant
        // that would reach it does not exist yet, and today's `Serenity`
        // variant needs a live `TrySendError<ShardRunnerMessage>` to build.
        // What this test guards is the other half of that bound — the list of
        // variants whose text is trusted — so that anything outside it has
        // nowhere to go but the fixed sentence.
    }

    #[test]
    fn this_bots_own_voice_state_is_a_bot_even_with_no_member_attached() {
        // `is_bot` is the ONLY thing stopping the Commander server-muting
        // this bridge: `Commander::voice` returns early for a bot and for
        // nobody else. An unresolvable member used to read as a human, and
        // the node would then mute its own bridge with no way back — the
        // bridge is not approved, cannot be given the mic, and the mute is
        // recorded as the node's own.
        assert!(is_own_bot(42, 42));
    }

    #[test]
    fn somebody_elses_voice_state_is_not_this_bot() {
        // The mirror half, and the one that must NOT change: if this
        // matched broadly it would silently skip every human in the
        // channel, which is an unmuted stranger on an armed bridge.
        assert!(!is_own_bot(43, 42));
    }

    #[test]
    fn before_ready_the_bots_own_id_matches_nobody() {
        // Zero is the "not learned yet" value, and voice states can arrive
        // before READY on a reconnect. A zero that matched everybody would
        // make every member a bot and the whole mute sweep a no-op.
        assert!(!is_own_bot(42, 0));
        assert!(!is_own_bot(0, 0));
    }

    #[test]
    fn the_only_songbird_config_this_crate_builds_never_decrypts_or_decodes() {
        // `DecodeMode::Pass` is the whole rule under which `receive` is
        // enabled at all, and songbird's DEFAULT is `Decrypt` — so this is
        // a value that must be set, not a default that must be left alone.
        // A `voice_config` that forgot the builder call, or that was
        // "tidied" to `Config::default()`, would open every packet this bot
        // is sent; `should_decrypt`/`should_decode` are songbird's own
        // predicates, so they answer for the cipher and the decoder rather
        // than for the name of the variant.
        let config = voice_config();
        assert_eq!(config.decode_mode, songbird::driver::DecodeMode::Pass);
        assert!(
            !config.decode_mode.should_decrypt(),
            "no received packet is ever decrypted"
        );
        assert!(
            !config.decode_mode.should_decode(),
            "no received packet is ever decoded"
        );
    }

    /// Drive one tick into a session exactly as the `VoiceTick` handler
    /// does. songbird's `VoiceTick` is `#[non_exhaustive]` and cannot be
    /// constructed outside its own crate, and the SSRC key set is the only
    /// thing the handler reads out of one — so this is the same input
    /// through a different door, with no packet involved at either.
    fn feed_tick(session: &TalkSession, speaking: &[u32], silent: &[u32]) {
        let speaking: HashSet<u32> = speaking.iter().copied().collect();
        let silent: HashSet<u32> = silent.iter().copied().collect();
        let mut state = session.0.lock().expect("talk state mutex poisoned");
        let edges = state.talkers.tick(&speaking, &silent);
        state.emit(edges);
    }

    /// The trait that gives [`TalkSession`] its `act`; songbird calls it on
    /// its driver event task and these tests call it directly.
    use songbird::EventHandler as _;

    /// One speaking-state update as songbird would deliver it.
    fn speaking_update(ssrc: u32, user: Option<u64>) -> songbird::model::payload::Speaking {
        songbird::model::payload::Speaking {
            delay: Some(0),
            speaking: songbird::model::SpeakingState::MICROPHONE,
            ssrc,
            user_id: user.map(songbird::model::id::UserId),
        }
    }

    #[test]
    fn a_mapping_that_arrives_mid_spurt_reaches_the_node_as_a_talking_edge() {
        // Discord's speaking opcode can arrive AFTER the packets it
        // describes. If the handler only forwarded fresh edges, a holder
        // who was already mid-sentence when their SSRC was named would
        // never be reported talking at all — and the five-second silence
        // window would unkey them five seconds into their over.
        let rt = test_runtime();
        let (tx, rx) = std::sync::mpsc::channel();
        let session = TalkSession::new(tx);

        // Packets first, mapping second.
        feed_tick(&session, &[7], &[]);
        assert!(
            rx.try_recv().is_err(),
            "an SSRC nobody can name is not reported"
        );
        rt.block_on(session.act(&songbird::EventContext::SpeakingStateUpdate(
            speaking_update(7, Some(300)),
        )));

        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::Talking {
                user: 300,
                talking: true
            })
        );
        assert!(rx.try_recv().is_err(), "and exactly one edge, not a stream");
    }

    #[test]
    fn a_speaking_update_with_no_user_id_names_nobody_and_is_dropped() {
        // Discord sends these; they carry an SSRC and no user. Mapping one
        // to user 0 would attribute somebody else's audio timing to a
        // snowflake that means "nobody", and 0 is a value the Commander
        // would happily treat as a mic holder.
        let rt = test_runtime();
        let (tx, rx) = std::sync::mpsc::channel();
        let session = TalkSession::new(tx);

        feed_tick(&session, &[7], &[]);
        rt.block_on(session.act(&songbird::EventContext::SpeakingStateUpdate(
            speaking_update(7, None),
        )));

        assert!(rx.try_recv().is_err(), "nothing was named, so nothing said");
    }

    #[test]
    fn ending_a_session_tells_the_node_its_talkers_stopped_exactly_once() {
        // The normal path. The Commander's hard time-out timer would
        // eventually rescue a holder left marked talking, but that backstop
        // is for the path where receive has failed — a leave must not need
        // it. The second `end` is the other half: the `Leave` arm and the
        // post-loop drain both call this, and a second "stopped" for
        // somebody who has since taken the mic again would unkey them.
        let (tx, rx) = std::sync::mpsc::channel();
        let session = TalkSession::new(tx);

        {
            let mut state = session.0.lock().expect("talk state mutex poisoned");
            let edges = state.talkers.map_ssrc(7, 300);
            state.emit(edges);
        }
        feed_tick(&session, &[7], &[]);
        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::Talking {
                user: 300,
                talking: true
            })
        );

        session.end();
        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::Talking {
                user: 300,
                talking: false
            })
        );

        session.end();
        assert!(rx.try_recv().is_err(), "an ended session says nothing more");

        // And a tick arriving from a handler songbird still holds is
        // ignored rather than resurrecting the holder.
        feed_tick(&session, &[7], &[]);
        assert!(rx.try_recv().is_err(), "a stale tick reports nothing");
    }

    #[test]
    fn an_empty_history_is_not_read_as_a_buried_panel() {
        // Discord answers a bot without Read Message History with HTTP 200
        // and an EMPTY ARRAY, not a 403 — so the "no 403, therefore the scan
        // worked" branch is exactly where this goes wrong. Read as "buried",
        // it deletes and re-posts the panel on every single redraw.
        assert!(
            !panel_is_buried_in(&[], 900),
            "an empty list means the scan told us nothing; a channel with no \
             messages cannot be hiding a panel either"
        );
        assert!(
            !panel_is_buried_in(&[901, 900, 899], 900),
            "our panel is right there"
        );
        let others: Vec<u64> = (1..=20).collect();
        assert!(
            panel_is_buried_in(&others, 900),
            "twenty newer messages and none of them ours is the case this scan exists for"
        );
    }

    #[test]
    fn a_finished_mute_is_pruned_and_an_unfinished_one_is_kept() {
        // The bookkeeping half of the shutdown wait, with no Discord in it:
        // a node that has muted a busy channel for hours must not be holding
        // a thousand dead `JoinHandle`s by the time it shuts down, and it
        // must still be holding the ones that have not landed.
        let rt = test_runtime();
        let mut in_flight = InFlight::default();
        let done = rt.spawn(async {});
        // Give it a moment to finish; `is_finished` is what the prune reads.
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
        in_flight.push(done);
        assert_eq!(
            in_flight.len(),
            1,
            "a push never prunes what it is pushing, only what came before"
        );
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        in_flight.push(rt.spawn(async move {
            // Parked until the test says so — the stand-in for a REST call
            // still on the wire when the teardown arrives.
            let _ = tokio::task::spawn_blocking(move || rx.recv()).await;
        }));
        assert_eq!(
            in_flight.len(),
            1,
            "the finished handle is pruned by the next push, not kept forever"
        );
        let taken = in_flight.take_all();
        assert_eq!(taken.len(), 1, "take_all hands back the unfinished handle");
        assert_eq!(
            in_flight.len(),
            0,
            "and empties the record, so a second wait cannot await the same task twice"
        );
        let _ = tx.send(());
        for h in taken {
            rt.block_on(async { drop(h.await) });
        }
    }

    #[test]
    fn the_unmutes_are_waited_for_rather_than_abandoned() {
        // The same hazard as the status post below, and the one that
        // actually leaves people muted: `set_member_mute` spawns its PATCH,
        // and `rt.shutdown_timeout` does not let a spawned task parked on
        // socket I/O run to completion. Without this wait a shutdown enqueues
        // every unmute and then exits out from under them.
        let rt = test_runtime();
        let landed = Arc::new(AtomicU64::new(0));
        let mut in_flight = InFlight::default();
        for _ in 0..3 {
            let flag = Arc::clone(&landed);
            in_flight.push(rt.spawn(async move {
                tokio::time::sleep(Duration::from_millis(120)).await;
                flag.fetch_add(1, Ordering::Relaxed);
            }));
        }

        await_in_flight(&rt, &mut in_flight);

        assert_eq!(
            landed.load(Ordering::Relaxed),
            3,
            "every unmute in flight must have finished before the teardown goes on"
        );
        assert_eq!(
            in_flight.len(),
            0,
            "and the record is consumed, so the post-loop drain cannot wait on them again"
        );
    }

    #[test]
    fn the_last_status_post_is_waited_for_rather_than_abandoned() {
        // `rt.shutdown_timeout` does NOT wait for a spawned task parked on
        // socket I/O — tokio's own docs say such a task is dropped at its next
        // yield, not run to completion — so the "not connected" line on a
        // failed join, posted microseconds before the `Bridge` drops, used to
        // land only by luck. This is the wait that replaces the luck: a post
        // still in flight when the teardown arrives must have finished by the
        // time this returns.
        let rt = test_runtime();
        let landed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&landed);
        let mut handle = Some(rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            flag.store(true, Ordering::Relaxed);
        }));

        await_last_post(&rt, &mut handle);

        assert!(
            landed.load(Ordering::Relaxed),
            "the teardown must wait for the post to complete, not detach it"
        );
        assert!(
            handle.is_none(),
            "the handle is consumed, so the post-loop drain cannot wait on it a second time"
        );
    }

    #[test]
    fn an_interrupted_session_reports_its_talkers_and_stays_usable() {
        // songbird's driver dropped with no `Leave` — kicked from the
        // channel, or the network went. The ticks simply stop, so without
        // this the holder stays latched `talking = true` and the silence
        // unkey never fires: the mic is open until the 180 s hard timer.
        //
        // The songbird side cannot be driven from a test —
        // `DisconnectData` is `#[non_exhaustive]`, so no `EventContext::
        // DriverDisconnect` can be built outside songbird — so this drives
        // what that arm calls.
        let (tx, rx) = std::sync::mpsc::channel();
        let session = TalkSession::new(tx);
        {
            let mut state = session.0.lock().expect("talk state mutex poisoned");
            let edges = state.talkers.map_ssrc(7, 300);
            state.emit(edges);
        }
        feed_tick(&session, &[7], &[]);
        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::Talking {
                user: 300,
                talking: true
            })
        );

        assert_eq!(session.interrupt(), vec![300]);
        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::Talking {
                user: 300,
                talking: false
            })
        );
        assert!(rx.try_recv().is_err(), "one falling edge, not a stream");

        // Still usable: this is not an `end`, because songbird retries the
        // driver itself and the same handlers serve the reconnected call.
        {
            let mut state = session.0.lock().expect("talk state mutex poisoned");
            let edges = state.talkers.map_ssrc(11, 300);
            state.emit(edges);
        }
        feed_tick(&session, &[11], &[]);
        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::Talking {
                user: 300,
                talking: true
            }),
            "the reconnected session reports talkers again"
        );
    }

    // -- track supervision -------------------------------------------------

    #[test]
    fn an_event_for_the_current_track_is_a_failure_worth_reporting() {
        let tracks = TrackGeneration::default();
        let current = tracks.next();
        assert!(tracks.is_current(current));
        assert!(
            tracks.claim_failure(current),
            "the track being fed right now died: that is the failure"
        );
    }

    #[test]
    fn an_event_for_a_superseded_track_is_ignored() {
        // `play_only_input` STOPS the previous track, and a `Leave` ends
        // it: both fire `TrackEvent::End` for a track this node deliberately
        // replaced. Reporting that as an outage would mean every rejoin
        // announced a dead audio feed into the channel it had just
        // successfully joined.
        let tracks = TrackGeneration::default();
        let old = tracks.next();
        let new = tracks.next();
        assert!(!tracks.is_current(old));
        assert!(tracks.is_current(new));
        assert!(
            !tracks.claim_failure(old),
            "the old track's End is the deliberate stop, not a failure"
        );
        assert!(
            tracks.claim_failure(new),
            "and the current one is still able to fail"
        );
    }

    #[test]
    fn one_dead_track_is_claimed_exactly_once() {
        // songbird fires BOTH `Error` and `End` for a track that errored
        // (`songbird-0.6.0/src/tracks/mode.rs:56,66`), so one death arrives
        // as two events on the same generation. The second must find the
        // generation already retired.
        let tracks = TrackGeneration::default();
        let current = tracks.next();
        assert!(tracks.claim_failure(current));
        assert!(
            !tracks.claim_failure(current),
            "the End that follows an Error is the same death"
        );
        assert!(
            !tracks.is_current(current),
            "a failed track is not the current one any more"
        );
    }

    #[test]
    fn the_first_track_minted_is_the_current_one() {
        // Handlers are tagged with what `next` returned. Handing out the
        // value a fresh counter already holds would tag the FIRST track
        // with a generation that a `claim_failure` from anywhere else
        // could have consumed before it was ever registered.
        let tracks = TrackGeneration::default();
        let first = tracks.next();
        assert!(
            tracks.is_current(first),
            "the track just installed is the one being fed"
        );
        assert_ne!(
            first,
            u64::default(),
            "and it is not the value a counter nobody has played against holds"
        );
    }

    #[test]
    fn a_dead_track_is_reported_to_the_node_once_and_recorded_for_status() {
        // The whole of the track handler except the two lines that read
        // songbird's context. One death, two events (songbird fires
        // `Error` then `End`), one `AudioFailed` and one reason kept.
        let (tx, rx) = std::sync::mpsc::channel();
        let supervisor = TrackSupervisor::new(Some(tx));
        let watch = supervisor.watch_next();

        assert!(watch.report("no compatible track found".to_string()));
        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::AudioFailed {
                reason: "no compatible track found".to_string()
            })
        );
        assert_eq!(
            supervisor.health.reason(),
            Some("no compatible track found".to_string()),
            "and `play` can report it, which is what `/status` shows"
        );

        assert!(
            !watch.report("the audio track ended".to_string()),
            "the End that follows an Error is the same death"
        );
        assert!(
            rx.try_recv().is_err(),
            "so the node hears about it exactly once"
        );
    }

    #[test]
    fn the_track_a_rejoin_replaced_reports_nothing() {
        // `play_only_input` stops the previous track and a `Leave` ends it,
        // so a deliberate handover fires `TrackEvent::End` for a track this
        // node retired on purpose. Announcing that as an outage would mean
        // every rejoin told the channel its audio feed had failed.
        let (tx, rx) = std::sync::mpsc::channel();
        let supervisor = TrackSupervisor::new(Some(tx));
        let old = supervisor.watch_next();
        let new = supervisor.watch_next();

        assert!(!old.report("the audio track ended".to_string()));
        assert!(rx.try_recv().is_err(), "a handover is not a failure");
        assert_eq!(
            supervisor.health.reason(),
            None,
            "and `play` keeps working against the track that replaced it"
        );

        assert!(
            new.report("no compatible track found".to_string()),
            "the current track can still fail"
        );
        assert_eq!(
            rx.try_recv(),
            Ok(GatewayEvent::AudioFailed {
                reason: "no compatible track found".to_string()
            })
        );
    }

    #[test]
    fn an_audio_only_gateway_records_the_failure_with_nobody_to_tell() {
        // No `GatewayOptions` means no Commander and no events channel.
        // The failure still has to be recorded, because `SongbirdLink::play`
        // is what reports it and `/status` is what shows it.
        let supervisor = TrackSupervisor::new(None);
        let watch = supervisor.watch_next();
        assert!(watch.report("no compatible track found".to_string()));
        assert_eq!(
            supervisor.health.reason(),
            Some("no compatible track found".to_string())
        );
    }

    #[test]
    fn a_join_that_never_installs_a_track_leaves_the_previous_one_supervised() {
        // Minting the watch when the join was REQUESTED retired the watch
        // on the track that was still playing: an early failure — never
        // READY, out of time, a refusal — left the previous session's
        // track feeding the channel with nothing watching it, because
        // `apply_join_outcome` returns through `outcome.result?` without
        // clearing `pcm_tx`. The generation must move only where a track
        // really is replaced.
        //
        // This drives the real `join_and_play` and reaches no network at
        // all: `gateway_ready` is false and the deadline has already
        // passed, so it returns at the READY poll before `Songbird::join`
        // is ever called.
        let rt = test_runtime();
        let manager = songbird::Songbird::serenity_from_config(voice_config());
        let ready = AtomicBool::new(false);
        let supervisor = TrackSupervisor::new(None);
        // The session that is playing now.
        let playing = supervisor.watch_next();
        let (_tx, input) = new_streaming_input();

        let outcome = rt.block_on(join_and_play(
            &manager,
            &ready,
            to_guild_id(12_345).expect("non-zero guild id"),
            to_channel_id(67_890).expect("non-zero channel id"),
            input,
            SessionHandlers {
                talk: None,
                tracks: &supervisor,
            },
            Instant::now(),
        ));

        assert_eq!(
            outcome.result,
            Err(DiscordError::Voice(
                "the Discord gateway did not reach READY in time".to_string()
            )),
            "the attempt must fail before anything is asked of Discord"
        );
        assert!(
            !outcome.in_channel,
            "and it must not claim the bot reached the channel"
        );
        assert!(
            playing.report("no compatible track found".to_string()),
            "the track that is still playing must still be supervised"
        );
    }

    #[test]
    fn a_join_after_a_failure_is_a_recovery_and_says_so_once() {
        // The node has to be able to say the audio came BACK, or a
        // successful rejoin leaves `/status` reporting the old failure for
        // the life of the process and the channel never hears that it is
        // fixed.
        let supervisor = TrackSupervisor::new(None);
        supervisor
            .health
            .fail("no compatible track found".to_string());

        assert!(supervisor.recover(), "this join fixed a recorded failure");
        assert_eq!(
            supervisor.health.reason(),
            None,
            "and the failure is cleared, so `play` works again"
        );
        assert!(
            !supervisor.recover(),
            "a second join fixed nothing: there was nothing left to fix"
        );
    }

    #[test]
    fn an_ordinary_join_is_not_a_recovery() {
        // Every successful join clears the health flag; only the ones that
        // had something to clear may announce anything, or the channel
        // gets "the audio feed is back" the first time the bridge ever
        // joins.
        let supervisor = TrackSupervisor::new(None);
        assert!(!supervisor.recover());
        assert_eq!(supervisor.health.reason(), None);
    }

    #[test]
    fn a_fresh_audio_health_reports_nothing_wrong() {
        let health = AudioHealth::default();
        assert_eq!(health.reason(), None);
    }

    #[test]
    fn the_first_reason_is_the_one_kept_until_a_join_clears_it() {
        // The cause is what the first event carried; anything after it is
        // a consequence. `Errored(Parse(...))` then "the audio track
        // ended" must not leave the operator reading the second one.
        let health = AudioHealth::default();
        health.fail("no compatible track found".to_string());
        health.fail("the audio track ended".to_string());
        assert_eq!(
            health.reason(),
            Some("no compatible track found".to_string())
        );

        // A successful join installs a new track, so the old one's failure
        // stops being the truth about this bridge.
        health.clear();
        assert_eq!(health.reason(), None);
        health.fail("the audio track ended".to_string());
        assert_eq!(health.reason(), Some("the audio track ended".to_string()));
    }

    #[test]
    fn an_audio_health_is_shared_by_every_clone() {
        // The point of the type: the runtime thread's handler records the
        // failure and a `SongbirdLink` the node holds is what reports it.
        let health = AudioHealth::default();
        let handle = health.clone();
        health.fail("no compatible track found".to_string());
        assert_eq!(
            handle.reason(),
            Some("no compatible track found".to_string())
        );
        handle.clear();
        assert_eq!(health.reason(), None);
    }

    #[test]
    fn a_track_that_errored_reports_songbirds_reason() {
        use songbird::tracks::{PlayError, PlayMode, TrackState};
        let errored = TrackState {
            playing: PlayMode::Errored(PlayError::Create(Arc::new(
                songbird::input::AudioStreamError::Unsupported,
            ))),
            ..TrackState::default()
        };
        assert_eq!(
            track_failure_reason([&errored]),
            "runtime error while playing track: input creation [failed to create audio: \
             operation was not supported]",
            "the operator gets songbird's own words"
        );
    }

    #[test]
    fn a_track_that_merely_ended_reports_that_it_ended() {
        use songbird::tracks::{PlayMode, TrackState};
        let ended = TrackState {
            playing: PlayMode::End,
            ..TrackState::default()
        };
        assert_eq!(track_failure_reason([&ended]), "the audio track ended");
        // And the global-event shape, which carries no tracks at all.
        assert_eq!(track_failure_reason([]), "the audio track ended");
    }
}
