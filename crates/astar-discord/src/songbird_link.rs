// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One voice session on a [`Gateway`](crate::Gateway).
//!
//! This is a handle and nothing more: a clone of the gateway's command
//! sender, a clone of its failure flag, and the channel the last join was
//! aimed at. All of the machinery — the tokio runtime, the serenity client,
//! the READY gate, the reconnect supervisor, and the reasons every timeout
//! here is the length it is — lives in [`crate::gateway`], which owns the
//! thread these commands are sent to. Read that module's docs first; this
//! file only waits for its answers.
//!
//! Handles are cheap and disposable by design. A `Gateway` may hand out one
//! per voice session, and dropping one ends a session, never the connection.
//!
//! **What a session carries, and in which direction.** Outbound: conference
//! audio, as [`GatewayCmd::Frame`]. Inbound: nothing but timing. songbird's
//! `receive` feature is enabled so that the gateway can see WHICH SSRCs
//! produced a packet in each 20 ms tick — the only signal Discord offers
//! that a speaker has stopped, and what unkeys a silent mic — under the
//! rule stated on `gateway::voice_config`: `DecodeMode::Pass`, so nothing
//! is decrypted and nothing is decoded. No Discord audio enters this
//! process, [`crate::NetLink::send`] still has no production caller, and
//! the conference member stays non-contributing. If that timing signal ever
//! stops arriving — it is the reverse-engineered half of the protocol — the
//! Commander's hard time-out timer still releases the mic.

use crate::gateway::{AudioHealth, GatewayCmd};
use crate::{DiscordError, VoiceLink};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, RecvTimeoutError, Sender as StdSender};
use std::time::Duration;

/// How long [`SongbirdLink::join`] waits for that outcome.
///
/// Deliberately LONGER than the gateway's own
/// [`JOIN_DEADLINE`](crate::gateway::JOIN_DEADLINE): the runtime thread polls
/// its command channel with a 200 ms timeout, so a Join can sit unread for that
/// long before its budget even starts, and `rt.block_on` adds scheduling
/// slack on top. If this fired first the caller would report its own opaque
/// "timed out" instead of songbird's real reason — which is exactly the
/// blindness this reply channel exists to remove.
const JOIN_REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long [`SongbirdLink::leave`] waits for ITS outcome.
///
/// Same shape as [`JOIN_REPLY_TIMEOUT`] and chosen the same way: the leave
/// itself is bounded by [`OP_TIMEOUT`](crate::gateway::OP_TIMEOUT), the
/// command loop can take 200 ms to notice the command at all, and
/// `rt.block_on` adds scheduling slack. Longer than the work it waits on,
/// so the reason the caller reports — and posts into a public channel — is
/// the runtime thread's, not a local guess.
const LEAVE_REPLY_TIMEOUT: Duration = Duration::from_secs(8);

/// A [`VoiceLink`] backed by songbird, over a gateway someone else owns.
///
/// Taken from [`Gateway::voice`](crate::Gateway::voice). Holding one keeps
/// nothing alive: every field is a clone, and the runtime thread these
/// commands cross into belongs to the `Gateway`.
pub struct SongbirdLink {
    tx: StdSender<GatewayCmd>,
    /// Set by the runtime thread when it dies, so `play` can report a failed
    /// gateway instead of silently dropping frames forever. Shared with the
    /// `Gateway` and with every other handle it has produced.
    failed: Arc<AtomicBool>,
    /// Why the audio track into Discord stopped, if it has. Shared with the
    /// `Gateway` and with the runtime thread's track handler, which is what
    /// sets it.
    ///
    /// **A different fault from `failed`, deliberately.** `failed` means the
    /// runtime thread is gone and the controller replaces the whole
    /// gateway; this means the thread is alive, the bot is still in the
    /// channel, and the one track carrying conference audio into it has
    /// died — which a rejoin fixes and a new gateway does not. Conflating
    /// them would take the bot offline over a dead track.
    audio: AudioHealth,
    /// The channel the last `join` was ASKED for, recorded whether or not
    /// that join succeeded. Status lines go to the voice channel's own text
    /// chat, so this is the same id — there is no second config key — and a
    /// failed or timed-out join must still be able to say so in the channel
    /// the operator is looking at.
    channel: Option<u64>,
}

impl SongbirdLink {
    /// Build a handle onto an existing gateway's command channel.
    ///
    /// Crate-private on purpose: a handle is only meaningful alongside the
    /// runtime thread that reads `tx` and sets `failed`, so
    /// [`Gateway::voice`](crate::Gateway::voice) is the only way to get one.
    pub(crate) fn new(
        tx: StdSender<GatewayCmd>,
        failed: Arc<AtomicBool>,
        audio: AudioHealth,
    ) -> Self {
        Self {
            tx,
            failed,
            audio,
            channel: None,
        }
    }

    fn send(&self, cmd: GatewayCmd) -> Result<(), DiscordError> {
        if self.failed.load(Ordering::Relaxed) {
            return Err(DiscordError::Voice("voice thread has stopped".into()));
        }
        self.tx
            .send(cmd)
            .map_err(|_| DiscordError::Voice("voice thread has stopped".into()))
    }
}

impl Drop for SongbirdLink {
    /// Ask, best effort, to leave whatever channel this handle joined — and
    /// do nothing else.
    ///
    /// **This is not the gateway's teardown.** Dropping a handle must never
    /// take the bot offline, so there is no `Shutdown` here and no thread to
    /// join; that belongs to [`Gateway::drop`](crate::Gateway) alone. What is
    /// left is the one thing a handle still owes: the session it opened.
    ///
    /// It owes it because a `Bridge` is NOT always dropped after a successful
    /// `leave()`. The controller's `Leave` does call `leave()` first, but a
    /// FAILED join drops the whole `Bridge` the instant `Bridge::join`
    /// returns `Err` — and a join can fail with the bot already sitting in
    /// the channel: "joined the channel but timed out installing the audio
    /// stream" says exactly that, and a reply that times out locally while
    /// the runtime thread's join later succeeds has the same shape. While the
    /// runtime lived inside the voice link, that stranded session was cleaned
    /// up by accident — dropping the link tore down the gateway and the
    /// session went with it. A persistent gateway removes that accident, so
    /// the ask is made explicitly here.
    ///
    /// **This only works because the gateway records the session at the
    /// moment it is entered**, not when the join finally succeeds — see
    /// `JoinOutcome` in [`crate::gateway`]. Without that, the `Leave` below
    /// would reach `handle_leave` with no guild recorded and return early,
    /// and this `Drop` would be theatre.
    ///
    /// Fire and forget, and it must stay that way: no reply is awaited and no
    /// thread is joined, because a `Drop` that blocked would reintroduce
    /// under the controller's mutexes exactly the stall the split removed.
    /// The receiver below is dropped immediately; the command loop answers
    /// with `try_send` and never blocks on a caller that has gone away. A
    /// leave with nothing joined is a no-op that never reaches the network,
    /// which is what the common case — a handle dropped after its own
    /// `leave()` — always is.
    ///
    /// **Against a persistent gateway the wire order after a failed join is
    /// `PostText` → this `Leave` → nothing**, and the "Not connected — ..."
    /// line still lands: it was spawned as its own task and the runtime keeps
    /// running, so nothing is racing it to a teardown. The `Leave` that
    /// follows is a real `Songbird::remove`, bounded by `OP_TIMEOUT`, and it
    /// takes the bot out of the channel the line says it is not bridged
    /// into. (On the non-persistent path the controller drops the `Gateway`
    /// right behind this, and `Shutdown`'s wait for the last post is what
    /// covers the same line.)
    fn drop(&mut self) {
        let (reply, _discarded) = std::sync::mpsc::sync_channel(1);
        let _ = self.send(GatewayCmd::Leave { reply });
    }
}

impl VoiceLink for SongbirdLink {
    /// Join, and report what actually happened.
    ///
    /// Enqueuing the command is not the outcome: the connection is made on the
    /// runtime thread, and a `Join` issued moments after the gateway was
    /// started races a multi-second handshake it will usually lose. Without
    /// the reply channel below that loss was invisible — the caller got
    /// `Ok`, `/status` said `connected: true`, `frames_sent` climbed against
    /// a dropped channel, and the operator heard silence.
    fn join(&mut self, guild: u64, channel: u64) -> Result<(), DiscordError> {
        // Recorded BEFORE the outcome is known: the caller posts a status
        // line on failure too, and that line belongs in the channel this
        // join was aimed at.
        self.channel = Some(channel);
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.send(GatewayCmd::Join {
            guild,
            channel,
            reply,
        })?;
        await_reply(&rx, JOIN_REPLY_TIMEOUT, "join")
    }

    /// Leave, and report what actually happened.
    ///
    /// Enqueuing is no more the outcome here than it is for [`Self::join`]:
    /// `Songbird::remove` can answer with a `JoinError` or not answer inside
    /// [`OP_TIMEOUT`](crate::gateway::OP_TIMEOUT), and the caller posts
    /// "Disconnected — ... is no longer bridged into this channel." into a
    /// public channel on `Ok`. Without
    /// this reply channel that line asserted a disconnection nothing had
    /// checked — the join-side bug, mirrored onto the leave side.
    fn leave(&mut self) -> Result<(), DiscordError> {
        // `channel` deliberately survives a leave: the "disconnected" line is
        // posted after it, into the channel just left.
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.send(GatewayCmd::Leave { reply })?;
        await_reply(&rx, LEAVE_REPLY_TIMEOUT, "leave")
    }

    /// Hand one frame to the voice session — unless there is nothing alive
    /// to hand it to.
    ///
    /// **The check is the point.** The audio path is one long-lived
    /// songbird track, and when it dies the channel this end pushes frames
    /// into stays open: every send still succeeds, `frames_sent` still
    /// climbs, `/status` still says `connected: true`, and the voice
    /// channel hears silence for as long as the bridge is up. That is not
    /// hypothetical — it is what the first bridge did for its entire life.
    /// So a dead track is reported here, through the path
    /// [`crate::Bridge::pump`] already has for a failed play: it records
    /// the reason in `last_error` and stops feeding for the rest of that
    /// pump, without posting anything per frame.
    ///
    /// # Errors
    /// [`DiscordError::Voice`] naming songbird's reason once the audio
    /// track has stopped, or saying the voice thread has stopped when the
    /// gateway itself is gone.
    fn play(&mut self, frame: &[i16]) -> Result<(), DiscordError> {
        if let Some(reason) = self.audio.reason() {
            return Err(DiscordError::Voice(format!(
                "the audio track into Discord stopped: {reason}"
            )));
        }
        self.send(GatewayCmd::Frame(frame.to_vec()))
    }

    fn post_text(&mut self, text: &str) -> Result<(), DiscordError> {
        let Some(channel) = self.channel else {
            return Err(DiscordError::Voice(
                "no channel has been joined, so there is nowhere to post".into(),
            ));
        };
        self.send(GatewayCmd::PostText {
            channel,
            text: text.to_string(),
        })
    }
}

/// Block for the runtime thread's verdict on one command — a `Join` or a
/// `Leave`. `op` names it, and appears in the two locally-generated reasons.
///
/// Three outcomes, all of them errors the caller can act on:
/// * the thread replied — pass its `Result` straight through;
/// * the sender was dropped without replying (the command loop exited, or the
///   runtime thread panicked and `catch_unwind` ended it) — the link is dead;
/// * nothing arrived inside `timeout` — report that, rather than returning
///   `Ok` for an operation that may never have happened.
fn await_reply(
    rx: &StdReceiver<Result<(), DiscordError>>,
    timeout: Duration,
    op: &str,
) -> Result<(), DiscordError> {
    match rx.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Disconnected) => Err(DiscordError::Voice(format!(
            "voice thread stopped before the {op} completed"
        ))),
        Err(RecvTimeoutError::Timeout) => Err(DiscordError::Voice(format!(
            "{op} did not complete within {}s",
            timeout.as_secs()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_reply_of_ok_is_the_join_succeeding() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.try_send(Ok(())).unwrap();
        assert_eq!(await_reply(&rx, Duration::from_secs(1), "join"), Ok(()));
    }

    #[test]
    fn a_reply_of_err_reaches_the_caller_verbatim() {
        // The whole point of the reply channel: songbird's reason, not a
        // locally invented one, is what the operator sees in `/status`.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.try_send(Err(DiscordError::Voice(
            "join failed: no gateway destination".into(),
        )))
        .unwrap();
        assert_eq!(
            await_reply(&rx, Duration::from_secs(1), "join"),
            Err(DiscordError::Voice(
                "join failed: no gateway destination".into()
            ))
        );
    }

    #[test]
    fn a_dropped_sender_is_a_failed_join_not_a_silent_success() {
        // The runtime thread exiting (gateway task dead, or a panic caught by
        // `connect`'s `catch_unwind`) drops the reply sender without ever
        // answering. Reporting `Ok` here is exactly the old bug.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), DiscordError>>(1);
        drop(tx);
        let outcome = await_reply(&rx, Duration::from_secs(1), "join");
        assert!(
            matches!(outcome, Err(DiscordError::Voice(ref m)) if m.contains("stopped")),
            "expected a stopped-thread error, got {outcome:?}"
        );
    }

    #[test]
    fn no_reply_inside_the_timeout_is_a_failed_join() {
        // The sender is held open and never used: nothing answers, so the
        // wait must end in an error rather than block forever or claim
        // success.
        let (_tx, rx) = std::sync::mpsc::sync_channel::<Result<(), DiscordError>>(1);
        let started = Instant::now();
        let outcome = await_reply(&rx, Duration::from_millis(50), "join");
        assert!(
            matches!(outcome, Err(DiscordError::Voice(ref m)) if m.contains("did not complete")),
            "expected a timeout error, got {outcome:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the wait must actually be bounded by the timeout, not return early"
        );
    }

    #[test]
    fn dropping_a_voice_handle_asks_to_leave_and_leaves_the_gateway_running() {
        // The whole point of the split, at the only level a test can reach
        // it without a network: the command channel belongs to the
        // `Gateway`, so a handle going away must post its `Leave` and then
        // leave the channel — and the gateway behind it — intact.
        //
        // `Gateway::voice` is not called here because building a real
        // `Gateway` needs a non-empty token, and `Gateway::connect` with one
        // spawns a thread that talks to Discord. This stands in for it with
        // the same two clones `voice()` hands over.
        let (tx, rx) = std::sync::mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let link = SongbirdLink::new(tx.clone(), Arc::clone(&failed), AudioHealth::default());

        drop(link);

        assert!(
            matches!(rx.try_recv(), Ok(GatewayCmd::Leave { .. })),
            "a dropped handle must ask to leave the session it opened"
        );
        // A `Shutdown` here would be the bug: it is what used to take the
        // bot offline the moment a bridge went away.
        assert!(
            rx.try_recv().is_err(),
            "a dropped handle must send nothing else — least of all Shutdown"
        );
        // The sender the `Gateway` holds is untouched, so the runtime thread
        // would still be reading commands.
        assert!(
            tx.send(GatewayCmd::Shutdown).is_ok(),
            "the gateway's own channel must survive a handle being dropped"
        );
    }

    #[test]
    fn play_refuses_once_the_audio_track_has_died() {
        // The bug this exists for: the frame channel survives the track, so
        // every send kept succeeding against a track that had errored forty
        // milliseconds into the session. `frames_sent` climbed, `/status`
        // said `connected: true`, and the channel heard nothing.
        let (tx, rx) = std::sync::mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let audio = AudioHealth::default();
        let mut link = SongbirdLink::new(tx, Arc::clone(&failed), audio.clone());

        assert_eq!(link.play(&[0i16; 4]), Ok(()), "a healthy track is fed");
        assert!(
            matches!(rx.try_recv(), Ok(GatewayCmd::Frame(_))),
            "and the frame really went"
        );

        audio.fail("no compatible track found".to_string());

        assert_eq!(
            link.play(&[0i16; 4]),
            Err(DiscordError::Voice(
                "the audio track into Discord stopped: no compatible track found".to_string()
            )),
            "the reason reaches the node verbatim, for `/status`"
        );
        assert!(
            rx.try_recv().is_err(),
            "and nothing is pushed into a dead track"
        );
        assert!(
            !failed.load(Ordering::Relaxed),
            "a dead track is not a dead gateway: the controller must not replace the bot"
        );

        // A successful join clears it, and the same handle works again —
        // the handle is not latched off, the track is.
        audio.clear();
        assert_eq!(link.play(&[0i16; 4]), Ok(()));
        assert!(matches!(rx.try_recv(), Ok(GatewayCmd::Frame(_))));
    }

    #[test]
    fn a_dropped_handle_on_a_failed_gateway_does_not_panic_or_block() {
        // `send` refuses once the runtime thread has set `failed`, and the
        // receiver may already be gone. Neither may turn a `Drop` — which
        // runs while the controller is unwinding a failed join — into a
        // panic or a wait.
        let (tx, rx) = std::sync::mpsc::channel();
        let failed = Arc::new(AtomicBool::new(true));
        let link = SongbirdLink::new(tx, Arc::clone(&failed), AudioHealth::default());
        drop(rx);

        drop(link);

        assert!(
            failed.load(Ordering::Relaxed),
            "the flag is shared, not consumed: the gateway's view is unchanged"
        );
    }
}
