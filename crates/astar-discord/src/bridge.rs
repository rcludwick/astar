// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The run loop: node frames in, Discord frames out.

use crate::commander::{LinkRef, link_labels};
use crate::pipeline::FramePipeline;
use crate::{DiscordError, NetLink, PostFilter, VoiceLink};

/// What the bridge believes about itself, as posted into the channel's own
/// text chat.
///
/// Three states and no more: the channel is told when the bridge arrives,
/// when it leaves, and when it could not do either. The voice notice
/// (`announce_on_join`) is spoken once and gone; these lines persist for
/// anyone scrolling back, which is what makes the relay disclosure a
/// standing statement rather than a moment.
///
/// They are also the cross-check for the one failure this bridge cannot fix
/// from inside: a join that times out can leave the bot sitting in the voice
/// channel while astar believes it never joined. astar says what it thinks,
/// and a human who can see the bot's avatar in the member list spots the
/// contradiction immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeStatus {
    /// The join succeeded and node audio is flowing into the channel.
    Connected,
    /// The bridge left the channel cleanly.
    Disconnected,
    /// The join or the leave failed. The string is the reason, taken from a
    /// [`DiscordError`] and therefore free of credentials.
    Failed(String),
}

impl BridgeStatus {
    /// Build this state's reason string from a [`DiscordError`].
    ///
    /// [`DiscordError::Voice`]'s payload is unwrapped rather than rendered
    /// through `Display`, which would prefix every posted line with
    /// "discord: voice link failed:". The payload is documented as
    /// human-readable and credential-free.
    #[must_use]
    pub fn failed(e: &DiscordError) -> Self {
        match e {
            DiscordError::Voice(reason) => Self::Failed(reason.clone()),
            other => Self::Failed(other.to_string()),
        }
    }

    /// The exact line posted into the channel for this state.
    ///
    /// `node_label` is a noun phrase naming this node — "node 12345", or a
    /// generic stand-in when no node id is configured — so it reads the same
    /// either way.
    #[must_use]
    pub fn message(&self, node_label: &str) -> String {
        match self {
            Self::Connected => format!(
                "Bridging {node_label} into this channel. Audio here is received from \
                 amateur radio; this bridge does not transmit."
            ),
            Self::Disconnected => {
                format!("Disconnected — {node_label} is no longer bridged into this channel.")
            }
            Self::Failed(reason) => format!(
                "Not connected — {node_label} is not bridged into this channel: {reason}. \
                 No radio audio is being relayed here."
            ),
        }
    }
}

/// The hourly "still linked" line, for the same `node_label`, naming every
/// link in `linked` by network ([`link_labels`]).
///
/// Separate from [`BridgeStatus`] because it is not a state: it is the
/// standing state reasserted, and it is therefore the one line that is
/// posted repeatedly on purpose. It names what the node is linked to
/// because that is the only thing worth an hourly line — see
/// [`Bridge::post_heartbeat`] for why it is never posted without one.
#[must_use]
fn heartbeat_message(node_label: &str, linked: &[LinkRef]) -> String {
    format!(
        "Still linked — {node_label} is bridging {} into this channel. Audio here is received \
         from amateur radio; this bridge does not transmit.",
        link_labels(linked)
    )
}

/// Joins a [`NetLink`], a [`PostFilter`] and a [`VoiceLink`].
///
/// One direction only. [`NetLink::send`] is never called from here, and that
/// is asserted by `the_bridge_never_sends_to_the_net`.
pub struct Bridge {
    net: Box<dyn NetLink>,
    voice: Box<dyn VoiceLink>,
    filter: Box<dyn PostFilter>,
    pipeline: FramePipeline,
    frames_sent: u64,
    last_error: Option<DiscordError>,
    /// Noun phrase naming this node in every posted line.
    node_label: String,
    /// The last state this bridge told the channel about, so a state is
    /// never posted twice running.
    ///
    /// **It lives here, on the `Bridge`, because the `Bridge` owns the
    /// `VoiceLink`** — it is the only object through which a post can
    /// happen, and it is created by one Join and dropped by the matching
    /// Leave (or by a failed Join). A fresh `Bridge` therefore starts at
    /// `None`, which is what makes leave-then-rejoin post again without any
    /// code saying so. `announce_status` is the only writer and it posts and
    /// records in the same `&mut self` call, so the two cannot diverge.
    last_status: Option<BridgeStatus>,
}

impl Bridge {
    #[must_use]
    pub fn new(
        net: Box<dyn NetLink>,
        voice: Box<dyn VoiceLink>,
        filter: Box<dyn PostFilter>,
        node_label: String,
    ) -> Self {
        let pipeline = FramePipeline::new(filter.output_rate());
        Self {
            net,
            voice,
            filter,
            pipeline,
            frames_sent: 0,
            last_error: None,
            node_label,
            last_status: None,
        }
    }

    /// Post `status` into the channel unless that is what the channel was
    /// last told.
    ///
    /// The de-duplication is what keeps a gateway flapping through reconnect
    /// backoff from filling the channel: repeated Joins that keep failing
    /// the same way say it once.
    ///
    /// The state is recorded whether or not the post actually went out. A
    /// status line is best effort by design — if the gateway is down the
    /// `Failed` line will not reach Discord either, and retrying it on the
    /// next transition is precisely the spam this exists to prevent.
    fn announce_status(&mut self, status: &BridgeStatus) {
        if self.last_status.as_ref() == Some(status) {
            return;
        }
        let text = status.message(&self.node_label);
        self.last_status = Some(status.clone());
        if let Err(e) = self.voice.post_text(&text) {
            eprintln!("astar-discord: could not post the status line: {e}");
        }
    }

    /// Post the hourly "still connected" line — but only while the channel
    /// has actually been told the bridge is connected.
    ///
    /// The guard is not belt and braces: it is what makes "while connected"
    /// true by construction rather than by the caller remembering to check.
    /// Unlike [`Self::announce_status`] this posts every time it is called,
    /// because saying the same thing on a schedule is the entire point.
    ///
    /// **A heartbeat that cannot be posted retires the claim it was going to
    /// repeat.** `Connected` is only ever written by a join and cleared by a
    /// leave, so a link that dies mid-session used to leave "Bridging ... into
    /// this channel" standing as the channel's last word — for up to an hour
    /// at a time, and indefinitely if nobody ever leaves — while every
    /// heartbeat failed quietly into a log nobody in that channel can read.
    /// Recording the failure through [`Self::announce_status`] both attempts
    /// the correction and stops this bridge reasserting a connection it has
    /// just been told it does not have.
    ///
    /// **Silent while `linked` is empty.** Every line into the channel is a
    /// notification for everyone in it, and "still connected" with nothing
    /// on the far side tells them nothing they can act on — so a joined
    /// bridge with an empty link roster posts no heartbeat at all. The
    /// controller does not arm the timer in that state; this guard is the
    /// same rule stated where the words are made, so a caller that fires
    /// anyway still says nothing false.
    pub fn post_heartbeat(&mut self, linked: &[LinkRef]) {
        if self.last_status != Some(BridgeStatus::Connected) || linked.is_empty() {
            return;
        }
        let text = heartbeat_message(&self.node_label, linked);
        if let Err(e) = self.voice.post_text(&text) {
            eprintln!("astar-discord: could not post the heartbeat line: {e}");
            self.announce_status(&BridgeStatus::failed(&e));
        }
    }

    /// Drain every frame the net has queued and play each into the voice
    /// channel. Returns how many frames were played.
    ///
    /// Called from the server's 50 ms pump tick; the conference produces a
    /// frame every 20 ms, so a pump normally moves two or three.
    ///
    /// **The drain always completes, even after a playback failure.** The
    /// conference feeds this member through an unbounded `mpsc`, at ~2.5
    /// frames per 50 ms tick. Returning early on the first `play` error —
    /// which is permanent once the voice thread is gone — consumed one frame
    /// per tick while ~2.5 arrived, so the queue grew at roughly 10–15 KB/s:
    /// about a gigabyte a day on a node nobody is watching. So on an error
    /// the frame is recorded once and the rest of this pump keeps draining
    /// and discarding, which bounds the queue by the tick rate whatever the
    /// voice link is doing.
    pub fn pump(&mut self) -> usize {
        let mut n = 0;
        // Set the moment a `play` fails. It survives only for the rest of
        // this pump: the next tick tries again, so a link that recovers is
        // not latched off.
        let mut playing = true;
        while let Some(mono) = self.net.recv() {
            self.filter.push_frame(&mono);
            while let Some(filtered) = self.filter.pull_frame() {
                let frame = self.pipeline.process(&filtered);
                if !playing {
                    // Discarded on purpose — the drain is what keeps the
                    // conference's queue from growing without bound.
                    continue;
                }
                match self.voice.play(&frame) {
                    Ok(()) => {
                        self.last_error = None;
                        self.frames_sent += 1;
                        n += 1;
                    }
                    Err(e) => {
                        self.last_error = Some(e);
                        playing = false;
                    }
                }
            }
        }
        n
    }

    /// Frames played into the channel since construction.
    #[must_use]
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// The last playback failure, if any. Carries no credentials.
    #[must_use]
    pub fn last_error(&self) -> Option<&DiscordError> {
        self.last_error.as_ref()
    }

    /// Join the configured channel, then discard whatever the net queued
    /// while the join was in flight.
    ///
    /// **The discard is not tidiness; it is the latency budget.** The
    /// conference enrolls this bridge's member *before* the join is
    /// attempted (`handle_discord` needs `handle.sample_rate` to build the
    /// pipeline), and from that instant it feeds the member's UNBOUNDED
    /// `mpsc` one frame every 20 ms whether or not anything is listening. A
    /// gateway handshake takes seconds — up to the join deadline on a cold
    /// start — so by the time this returns, hundreds of frames are waiting.
    /// The first [`Bridge::pump`] would hand every one of them to the voice
    /// link in a single tick, and songbird's mixer plays its input at real
    /// time: the channel would run a join-duration behind the net, forever,
    /// with nothing in the design that could ever catch it up.
    ///
    /// So the queue is emptied once, here, between the join succeeding and
    /// the first pump. The stream then starts at "now" rather than at "when
    /// the member was enrolled". Nothing audible is lost — those frames are
    /// net audio from before the channel was connected, which no one in it
    /// could have heard in any case.
    ///
    /// Only on success: a failed join leaves the queue alone, because the
    /// caller withdraws the member and drops the whole bridge.
    ///
    /// Whichever way the join goes, the channel is told: [`BridgeStatus`]
    /// on success, [`BridgeStatus::Failed`] otherwise with songbird's own
    /// reason for the five variants trusted to say it plainly, and one fixed
    /// sentence for anything else. The failure line matters more than the
    /// success one — a join that times out can leave the bot sitting in the
    /// voice channel, and this is astar stating, where anyone can read it,
    /// that it believes otherwise.
    ///
    /// # Errors
    /// Propagates [`DiscordError`] from the voice link.
    pub fn join(&mut self, guild: u64, channel: u64) -> Result<(), DiscordError> {
        if let Err(e) = self.voice.join(guild, channel) {
            self.announce_status(&BridgeStatus::failed(&e));
            return Err(e);
        }
        // Straight off the net, NOT through `filter`/`pipeline`: these frames
        // are never played, so there is no resampler continuity to preserve
        // and no reason to spend the CPU.
        while self.net.recv().is_some() {}
        self.announce_status(&BridgeStatus::Connected);
        Ok(())
    }

    /// Leave the channel.
    ///
    /// The status line goes out **after** the leave, and reports what the
    /// leave actually did: claiming "disconnected" before asking to
    /// disconnect would be the same unverified assertion these lines exist
    /// to expose. Posting still works from outside the voice channel —
    /// message creation is a REST call, not part of the voice session.
    ///
    /// "What it did" is only as true as [`VoiceLink::leave`]'s `Ok`, and that
    /// is why `SongbirdLink::leave` carries a reply channel rather than
    /// returning the moment the command is enqueued: an `Ok` here means the
    /// runtime thread saw `Songbird::remove` succeed. Without it this line
    /// would post "Disconnected" for a disconnect that had merely been asked
    /// for — the join-side bug, mirrored.
    ///
    /// # Errors
    /// Propagates [`DiscordError`] from the voice link.
    pub fn leave(&mut self) -> Result<(), DiscordError> {
        let outcome = self.voice.leave();
        match &outcome {
            Ok(()) => self.announce_status(&BridgeStatus::Disconnected),
            Err(e) => self.announce_status(&BridgeStatus::failed(e)),
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::DISCORD_FRAME_PAIRS;
    use crate::{Bypass, RecordingLink, TestLink};

    fn bridge_with(rate: u32) -> (Bridge, std::sync::Arc<RecordingLink>) {
        let net = TestLink::new(rate);
        // Queue three frames of node audio before the bridge takes the link.
        for _ in 0..3 {
            net.push(vec![1000i16; (rate as usize) / 50]);
        }
        let voice = std::sync::Arc::new(RecordingLink::new());
        let bridge = Bridge::new(
            Box::new(net),
            Box::new(std::sync::Arc::clone(&voice)),
            Box::new(Bypass::new(rate)),
            "node 12345".to_string(),
        );
        (bridge, voice)
    }

    #[test]
    fn pump_turns_node_frames_into_discord_frames() {
        let (mut bridge, voice) = bridge_with(8000);
        let n = bridge.pump();
        assert_eq!(n, 3, "three queued frames drained in one pump");
        let frames = voice.frames();
        assert_eq!(frames.len(), 3);
        for f in &frames {
            assert_eq!(f.len(), DISCORD_FRAME_PAIRS * 2);
        }
        assert_eq!(bridge.frames_sent(), 3);
    }

    #[test]
    fn pump_is_a_no_op_when_the_node_is_silent() {
        let voice = std::sync::Arc::new(RecordingLink::new());
        let mut bridge = Bridge::new(
            Box::new(TestLink::new(8000)),
            Box::new(std::sync::Arc::clone(&voice)),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );
        assert_eq!(bridge.pump(), 0);
        assert_eq!(voice.frames(), [] as [std::vec::Vec<i16>; 0]);
    }

    #[test]
    fn a_successful_join_discards_what_piled_up_during_the_handshake() {
        // The regression this pins: the conference enrolls the member before
        // the join is attempted and feeds it a frame every 20 ms regardless,
        // so a multi-second gateway handshake leaves hundreds of frames in
        // an unbounded queue. Without the discard in `join`, the first pump
        // hands all 100 of these to the voice link at once — and songbird
        // plays its input at real time, so the channel stays two seconds
        // behind the net for the rest of the session.
        let net = TestLink::new(8000);
        for _ in 0..100 {
            net.push(vec![1000i16; 160]);
        }
        let feed = net.sender();
        let voice = std::sync::Arc::new(RecordingLink::new());
        let mut bridge = Bridge::new(
            Box::new(net),
            Box::new(std::sync::Arc::clone(&voice)),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );

        bridge.join(11, 22).expect("RecordingLink always joins");
        assert_eq!(
            voice.joined(),
            Some((11, 22)),
            "the join itself must still happen"
        );

        assert_eq!(
            bridge.pump(),
            0,
            "the first pump after a join must play none of the backlog"
        );
        assert!(
            voice.frames().is_empty(),
            "the backlog must never reach the voice link"
        );
        assert_eq!(bridge.frames_sent(), 0);

        // And the frames are GONE, not merely skipped: a fresh frame — net
        // audio from after the join — is the only thing the next pump plays.
        // Asserting only on the pump above would also pass if the backlog
        // were still sitting in the queue waiting for tick two.
        feed.send(vec![2000i16; 160])
            .expect("bridge holds the link");
        assert_eq!(bridge.pump(), 1, "post-join audio still flows");
        assert_eq!(
            voice.frames().len(),
            1,
            "exactly the one post-join frame, with no backlog behind it"
        );
    }

    /// A [`VoiceLink`] double that fails its first `play` and succeeds on
    /// every call after. `RecordingLink` always succeeds, so it cannot drive
    /// the recovery path this test needs.
    struct FlakyOnceLink {
        failed_once: bool,
    }

    impl FlakyOnceLink {
        fn new() -> Self {
            Self { failed_once: false }
        }
    }

    impl VoiceLink for FlakyOnceLink {
        fn post_text(&mut self, _text: &str) -> Result<(), DiscordError> {
            Ok(())
        }

        fn join(&mut self, _guild: u64, _channel: u64) -> Result<(), DiscordError> {
            Ok(())
        }

        fn leave(&mut self) -> Result<(), DiscordError> {
            Ok(())
        }

        fn play(&mut self, _frame: &[i16]) -> Result<(), DiscordError> {
            if self.failed_once {
                Ok(())
            } else {
                self.failed_once = true;
                Err(DiscordError::Voice("simulated failure".into()))
            }
        }
    }

    #[test]
    fn last_error_clears_after_a_successful_play_following_a_failure() {
        // One frame per pump. A failed pump now DRAINS the rest of the queue
        // rather than leaving it for the next tick (see `Bridge::pump`), so
        // the recovery frame has to be pushed after the failure — queueing
        // both up front would see the second discarded, not replayed.
        let net = TestLink::new(8000);
        net.push(vec![1000i16; 160]);
        let probe = net.clone_probe();
        let feed = net.sender();
        let mut bridge = Bridge::new(
            Box::new(net),
            Box::new(FlakyOnceLink::new()),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );

        bridge.pump();
        assert!(
            bridge.last_error().is_some(),
            "the first play failed, so last_error must be set"
        );
        assert_eq!(bridge.frames_sent(), 0, "the failed play sent nothing");

        // A fresh frame against the now-recovered link.
        feed.send(vec![1000i16; 160]).unwrap();
        assert_eq!(bridge.pump(), 1, "the recovered link plays the next frame");
        assert!(
            bridge.last_error().is_none(),
            "a later successful play must clear last_error, not leave it latched"
        );
        assert!(
            probe.sent().is_empty(),
            "recovery must not push anything back at the net"
        );
    }

    /// A [`VoiceLink`] double whose `play` fails forever, the way a dead
    /// songbird thread does.
    struct DeadLink;

    impl VoiceLink for DeadLink {
        fn post_text(&mut self, _text: &str) -> Result<(), DiscordError> {
            Err(DiscordError::Voice("voice thread has stopped".into()))
        }

        fn join(&mut self, _guild: u64, _channel: u64) -> Result<(), DiscordError> {
            Ok(())
        }

        fn leave(&mut self) -> Result<(), DiscordError> {
            Ok(())
        }

        fn play(&mut self, _frame: &[i16]) -> Result<(), DiscordError> {
            Err(DiscordError::Voice("voice thread has stopped".into()))
        }
    }

    /// A [`NetLink`] double that reports how many frames it has actually
    /// handed out. `TestLink` cannot answer that once it has been moved into
    /// a `Bridge`, and "how many frames left the queue" is precisely what
    /// this test is about — asserting on `pump()`'s return instead would be
    /// satisfied by the very bug it is meant to catch, since a dead link
    /// plays nothing either way.
    struct CountingLink {
        queued: std::collections::VecDeque<Vec<i16>>,
        handed_out: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl NetLink for CountingLink {
        fn recv(&mut self) -> Option<Vec<i16>> {
            let frame = self.queued.pop_front()?;
            self.handed_out
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(frame)
        }

        fn send(&mut self, _pcm: &[i16]) {
            unreachable!("a listen-only bridge never sends");
        }

        fn sample_rate(&self) -> u32 {
            8000
        }
    }

    #[test]
    fn a_failing_link_still_drains_the_whole_queue() {
        // The conference queues ~2.5 frames per 50 ms tick into an unbounded
        // channel. Returning early on the first `play` error took ONE frame
        // per tick out of it, so a dead voice thread meant the queue grew
        // forever — ~10-15 KB/s, about a gigabyte a day.
        let handed_out = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let net = CountingLink {
            queued: (0..10).map(|_| vec![1000i16; 160]).collect(),
            handed_out: std::sync::Arc::clone(&handed_out),
        };
        let mut bridge = Bridge::new(
            Box::new(net),
            Box::new(DeadLink),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );

        assert_eq!(bridge.pump(), 0, "a dead link plays nothing");
        assert_eq!(
            handed_out.load(std::sync::atomic::Ordering::Relaxed),
            10,
            "one pump must take EVERY queued frame off the net, failure or not"
        );
        assert!(
            bridge.last_error().is_some(),
            "the failure must still be reported"
        );
        assert_eq!(
            bridge.frames_sent(),
            0,
            "discarded frames must not be counted as sent"
        );
    }

    /// A [`VoiceLink`] whose `play` always fails the way a dead audio track
    /// does, counting every line it is asked to post.
    struct DeadTrackLink {
        posts: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl VoiceLink for DeadTrackLink {
        fn post_text(&mut self, text: &str) -> Result<(), DiscordError> {
            self.posts
                .lock()
                .expect("posts mutex poisoned")
                .push(text.to_string());
            Ok(())
        }

        fn join(&mut self, _guild: u64, _channel: u64) -> Result<(), DiscordError> {
            Ok(())
        }

        fn leave(&mut self) -> Result<(), DiscordError> {
            Ok(())
        }

        fn play(&mut self, _frame: &[i16]) -> Result<(), DiscordError> {
            Err(DiscordError::Voice(
                "the audio track into Discord stopped: no compatible track found".into(),
            ))
        }
    }

    #[test]
    fn a_dead_audio_track_is_recorded_for_status_and_never_posted_per_frame() {
        // What `Bridge` does with the error a dead track now produces: the
        // reason lands in `last_error`, which is what `/status` shows, and
        // NOTHING is posted. The conference feeds this ~50 frames a second
        // for as long as the bridge is up, so a post per failed frame would
        // be a channel full of the same sentence. The one line the channel
        // gets comes from the Commander's `AudioFailed` rule, once per
        // process.
        let posts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let net = TestLink::new(8000);
        for _ in 0..25 {
            net.push(vec![1000i16; 160]);
        }
        let mut bridge = Bridge::new(
            Box::new(net),
            Box::new(DeadTrackLink {
                posts: std::sync::Arc::clone(&posts),
            }),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );

        for _ in 0..5 {
            assert_eq!(bridge.pump(), 0, "a dead track plays nothing");
        }

        assert_eq!(
            bridge.last_error().map(ToString::to_string),
            Some(
                "discord: voice link failed: the audio track into Discord stopped: no \
                 compatible track found"
                    .to_string()
            )
        );
        assert_eq!(
            bridge.frames_sent(),
            0,
            "and nothing is counted as delivered"
        );
        assert_eq!(
            posts.lock().expect("posts mutex poisoned").as_slice(),
            Vec::<String>::new().as_slice(),
            "a failed play must not post into the channel at all"
        );
    }

    /// A [`VoiceLink`] whose `join` always fails the way a timed-out join
    /// does, while its `post_text` keeps working — the real shape of that
    /// failure, where the gateway is up and only the voice connection was
    /// lost. `RecordingLink` always joins, so it cannot drive this path. The
    /// posted lines go into shared state so the test can read them after the
    /// `Bridge` has taken the link.
    struct FailingJoinLink {
        posts: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl VoiceLink for FailingJoinLink {
        fn join(&mut self, _guild: u64, _channel: u64) -> Result<(), DiscordError> {
            Err(DiscordError::Voice(
                "join did not complete within 15s".into(),
            ))
        }

        fn leave(&mut self) -> Result<(), DiscordError> {
            Ok(())
        }

        fn play(&mut self, _frame: &[i16]) -> Result<(), DiscordError> {
            Ok(())
        }

        fn post_text(&mut self, text: &str) -> Result<(), DiscordError> {
            self.posts
                .lock()
                .expect("FailingJoinLink posts mutex poisoned")
                .push(text.to_string());
            Ok(())
        }
    }

    const CONNECTED_LINE: &str = "Bridging node 12345 into this channel. Audio here is received \
         from amateur radio; this bridge does not transmit.";
    const DISCONNECTED_LINE: &str =
        "Disconnected — node 12345 is no longer bridged into this channel.";
    const HEARTBEAT_LINE: &str = "Still linked — node 12345 is bridging AllStar 543260 into this \
         channel. Audio here is received from amateur radio; this bridge does not transmit.";
    /// What the roster holds when `HEARTBEAT_LINE` is the right thing to say.
    fn linked() -> Vec<LinkRef> {
        vec![LinkRef::allstar("543260")]
    }
    const DEAD_LINK_LINE: &str = "Not connected — node 12345 is not bridged into this channel: \
         voice thread has stopped. No radio audio is being relayed here.";
    const FAILED_LEAVE_LINE: &str = "Not connected — node 12345 is not bridged into this channel: \
         leaving the voice channel did not complete within 5s. No radio audio is being relayed \
         here.";

    /// A [`VoiceLink`] that joins, plays and leaves happily but can never
    /// post: the shape of a link whose runtime thread has died under a
    /// bridge that still believes it is connected. It records every line it
    /// was ASKED to post, which is what the tests below assert on — a
    /// failed post is still a statement this bridge tried to make.
    struct PostsAlwaysFailLink {
        attempts: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl VoiceLink for PostsAlwaysFailLink {
        fn join(&mut self, _guild: u64, _channel: u64) -> Result<(), DiscordError> {
            Ok(())
        }

        fn leave(&mut self) -> Result<(), DiscordError> {
            Ok(())
        }

        fn play(&mut self, _frame: &[i16]) -> Result<(), DiscordError> {
            Ok(())
        }

        fn post_text(&mut self, text: &str) -> Result<(), DiscordError> {
            self.attempts
                .lock()
                .expect("attempts mutex poisoned")
                .push(text.to_string());
            Err(DiscordError::Voice("voice thread has stopped".into()))
        }
    }

    /// A [`VoiceLink`] whose `leave` fails the way a `Songbird::remove` that
    /// never answered does, while `post_text` keeps working. `RecordingLink`
    /// always leaves successfully, so it cannot drive this path.
    struct FailingLeaveLink {
        posts: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl VoiceLink for FailingLeaveLink {
        fn join(&mut self, _guild: u64, _channel: u64) -> Result<(), DiscordError> {
            Ok(())
        }

        fn leave(&mut self) -> Result<(), DiscordError> {
            Err(DiscordError::Voice(
                "leaving the voice channel did not complete within 5s".into(),
            ))
        }

        fn play(&mut self, _frame: &[i16]) -> Result<(), DiscordError> {
            Ok(())
        }

        fn post_text(&mut self, text: &str) -> Result<(), DiscordError> {
            self.posts
                .lock()
                .expect("posts mutex poisoned")
                .push(text.to_string());
            Ok(())
        }
    }

    #[test]
    fn a_failed_leave_says_why_instead_of_claiming_disconnected() {
        // The leave side of the join-side bug. `SongbirdLink::leave` now
        // reports what `Songbird::remove` did rather than that a command was
        // enqueued, so an `Err` here is a leave that really did not happen —
        // and the bot may well still be sitting in the voice channel. Posting
        // "Disconnected — ... is no longer bridged into this channel." on top
        // of that would be exactly the unverified assertion these lines exist
        // to expose.
        let posts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut bridge = Bridge::new(
            Box::new(TestLink::new(8000)),
            Box::new(FailingLeaveLink {
                posts: std::sync::Arc::clone(&posts),
            }),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );

        bridge.join(11, 22).expect("this link joins");
        let outcome = bridge.leave();
        assert!(outcome.is_err(), "the link fails this leave");

        assert_eq!(
            *posts.lock().expect("posts mutex poisoned"),
            vec![CONNECTED_LINE.to_string(), FAILED_LEAVE_LINE.to_string()],
            "a failed leave must post the reason, never the disconnected line"
        );
    }

    #[test]
    fn a_heartbeat_that_cannot_be_posted_retires_the_connected_claim() {
        // A link that dies AFTER a successful join never used to revise its
        // claim: `last_status` moved only on a join or a leave, so the channel
        // kept showing "Bridging node 12345 into this channel." — for up to an
        // hour between heartbeats, and indefinitely if nobody ever left —
        // while every heartbeat failed quietly into a log nobody in that
        // channel can read. The point of these lines is that the channel
        // reflects reality.
        let attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut bridge = Bridge::new(
            Box::new(TestLink::new(8000)),
            Box::new(PostsAlwaysFailLink {
                attempts: std::sync::Arc::clone(&attempts),
            }),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );

        bridge
            .join(11, 22)
            .expect("this link joins; only posting fails");
        bridge.post_heartbeat(&linked());
        bridge.post_heartbeat(&linked());

        assert_eq!(
            *attempts.lock().expect("attempts mutex poisoned"),
            vec![
                CONNECTED_LINE.to_string(),
                HEARTBEAT_LINE.to_string(),
                DEAD_LINK_LINE.to_string(),
            ],
            "a heartbeat that fails must correct the claim, and the next one must not repeat it"
        );
    }

    #[test]
    fn a_join_posts_the_connected_line_and_a_second_join_does_not_repeat_it() {
        // Two halves of one rule. The first join must say so — with the
        // disclosure in it, which is why the text is asserted verbatim rather
        // than by `contains`. The second must not: a gateway flapping through
        // reconnect backoff re-announcing "connected" every cycle is exactly
        // what fills a channel with noise.
        let (mut bridge, voice) = bridge_with(8000);

        bridge.join(11, 22).expect("RecordingLink always joins");
        assert_eq!(
            voice.posts(),
            vec![CONNECTED_LINE.to_string()],
            "a join must post the connected line, once, with the disclosure in it"
        );

        bridge.join(11, 22).expect("RecordingLink always joins");
        assert_eq!(
            voice.posts().len(),
            1,
            "the same state twice must post once — a flapping gateway must not spam the channel"
        );
    }

    #[test]
    fn leave_then_rejoin_posts_connected_again() {
        // The de-duplication is against the LAST thing said, not against
        // everything ever said. Implemented as a "have I said this before?"
        // set instead of a "what did I last say?" slot, the third post below
        // would go missing and a rejoined bridge would sit in the channel
        // having never announced itself.
        let (mut bridge, voice) = bridge_with(8000);

        bridge.join(11, 22).expect("RecordingLink always joins");
        bridge.leave().expect("RecordingLink always leaves");
        bridge.join(11, 22).expect("RecordingLink always joins");

        assert_eq!(
            voice.posts(),
            vec![
                CONNECTED_LINE.to_string(),
                DISCONNECTED_LINE.to_string(),
                CONNECTED_LINE.to_string(),
            ],
            "leave-then-rejoin must post all three lines in order"
        );
    }

    #[test]
    fn a_failed_join_says_so_in_the_channel_with_the_reason() {
        // The residual this whole feature exists for: a join that times out
        // can leave the bot sitting in the voice channel while astar believes
        // it never joined. The bot's own words in the channel are what let a
        // human see the contradiction, so the failure MUST be posted and it
        // must carry the reason songbird gave rather than a generic line.
        let posts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut bridge = Bridge::new(
            Box::new(TestLink::new(8000)),
            Box::new(FailingJoinLink {
                posts: std::sync::Arc::clone(&posts),
            }),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );

        let outcome = bridge.join(11, 22);
        assert!(outcome.is_err(), "the link fails this join");

        assert_eq!(
            *posts.lock().expect("posts mutex poisoned"),
            vec![
                "Not connected — node 12345 is not bridged into this channel: join did not \
                 complete within 15s. No radio audio is being relayed here."
                    .to_string()
            ],
            "a failed join must post the reason songbird gave, not a generic line"
        );
    }

    #[test]
    fn the_heartbeat_is_silent_until_connected_and_then_repeats() {
        // Two properties of the hourly line, neither of which the transition
        // rule covers. Before a join there is nothing to be still connected
        // to, so it must say nothing. After one it must post EVERY time the
        // timer calls it — de-duplicating the heartbeat the way transitions
        // are de-duplicated would mean the channel hears it once and never
        // again, which is the opposite of what a heartbeat is.
        let (mut bridge, voice) = bridge_with(8000);

        bridge.post_heartbeat(&linked());
        assert!(
            voice.posts().is_empty(),
            "a bridge that has not joined must not claim to be still connected"
        );

        bridge.join(11, 22).expect("RecordingLink always joins");
        bridge.post_heartbeat(&linked());
        bridge.post_heartbeat(&linked());
        assert_eq!(
            voice.posts(),
            vec![
                CONNECTED_LINE.to_string(),
                HEARTBEAT_LINE.to_string(),
                HEARTBEAT_LINE.to_string(),
            ],
            "the heartbeat must post on every tick it is given, naming the link each time"
        );
    }

    #[test]
    fn the_heartbeat_says_nothing_while_nothing_is_linked() {
        // A joined bridge with an empty roster is the idle state of every
        // node most of the day. An hourly line into that channel is a
        // notification with nothing behind it, so the rule is silence —
        // and the heartbeat comes back, naming the link, once there is
        // one, without any state having to be reset in between.
        let (mut bridge, voice) = bridge_with(8000);
        bridge.join(11, 22).expect("RecordingLink always joins");

        bridge.post_heartbeat(&[]);
        bridge.post_heartbeat(&[]);
        assert_eq!(
            voice.posts(),
            vec![CONNECTED_LINE.to_string()],
            "an idle bridge posts no heartbeat"
        );

        bridge.post_heartbeat(&[LinkRef::allstar("543260"), LinkRef::allstar("2000")]);
        assert_eq!(
            voice.posts().last().map(String::as_str),
            Some(
                "Still linked — node 12345 is bridging AllStar 543260, 2000 into this channel. \
                 Audio here is received from amateur radio; this bridge does not transmit."
            ),
            "with links the line names every one of them, and keeps the disclosure"
        );
    }

    #[test]
    fn the_heartbeat_names_every_network_it_is_linked_on() {
        let (mut bridge, voice) = bridge_with(8000);
        bridge.join(11, 22).expect("RecordingLink always joins");
        bridge.post_heartbeat(&[LinkRef::allstar("543260"), LinkRef::m17("M17-USA", 'C')]);
        assert_eq!(
            voice.posts().last().map(String::as_str),
            Some(
                "Still linked — node 12345 is bridging AllStar 543260, M17 M17-USA C into this \
                 channel. Audio here is received from amateur radio; this bridge does not \
                 transmit."
            ),
            "each network names itself, and the receive-only disclosure is unchanged"
        );
    }

    #[test]
    fn the_bridge_never_sends_to_the_net() {
        // The safety property, asserted directly: drive audio through the
        // bridge and prove nothing was handed back to NetLink::send.
        let net = TestLink::new(8000);
        for _ in 0..10 {
            net.push(vec![2000i16; 160]);
        }
        let probe = net.clone_probe();
        let mut bridge = Bridge::new(
            Box::new(net),
            Box::new(RecordingLink::new()),
            Box::new(Bypass::new(8000)),
            "node 12345".to_string(),
        );
        bridge.pump();
        assert!(
            probe.sent().is_empty(),
            "a listen-only bridge must never contribute to the net"
        );
    }
}
