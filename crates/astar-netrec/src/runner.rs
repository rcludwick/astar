// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One session from join to leave: the 20 ms tick loop and the rejoin logic.
//!
//! Two things run at once here. The tick loop writes one frame of audio or
//! silence every 20 ms, so a minute of the file is always a minute of the
//! net. The link comes and goes beside it: a failed join backs off and tries
//! again, and a dropped link logs one `dropped` row and rejoins. Neither ever
//! holds up the other. A join can spend seconds on DNS or a dongle scan, so
//! it runs on its own thread while the ticks carry on.
//!
//! The ticks run on the monotonic clock, so the file stays even, but the
//! window ends on the wall clock. The monotonic clock stops while a machine
//! sleeps, so on waking it can believe half the window is still to come.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use astar_netarchive::{Ended, LogKind, SessionMeta};
use astar_station::DstarVocoder;

use crate::config::{NetCfg, Target};
use crate::feed::{AudioFeed, Frame};
use crate::listen::{LinkStatus, ListenError, ListenOnlyStation, login_callsign};
use crate::session::{SessionError, SessionRecorder};

/// The first wait after a failed join.
pub const BACKOFF_START: Duration = Duration::from_secs(1);
/// The longest wait between join attempts.
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// How long a link must stay up before the backoff starts again from
/// [`BACKOFF_START`]. A reflector that accepts a login and then drops it says
/// `Linked` every time, so `Linked` alone proves nothing.
pub const STABLE_LINK: Duration = Duration::from_secs(60);
/// How long a link may stay `Linking` before the attempt counts as failed.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Milliseconds in one tick.
const TICK_MS: u64 = 20;
/// Further behind than this, the loop stops running ticks one by one and
/// fills the gap.
const CATCH_UP_AFTER: Duration = Duration::from_secs(1);
/// Further behind than this, the gap also gets an `Error` row.
const RESYNC_AFTER: Duration = Duration::from_secs(5);

/// A network the recorder can join. It must be `Send` because a join runs on
/// its own thread.
pub trait NetAdapter: Send {
    /// Start listening. This may take seconds. Watch [`Self::status`] for
    /// the link.
    ///
    /// # Errors
    /// A sentence saying why, which goes in the session log.
    fn join(&mut self) -> Result<Box<dyn AudioFeed>, String>;
    /// How the link is doing.
    fn status(&self) -> LinkStatus;
    /// Who the network says is talking.
    fn talker(&self) -> Option<String>;
    /// Drop the link, if there is one.
    fn leave(&mut self);
    /// The sample rate of the feeds, in Hz.
    fn rate(&self) -> u32;
    /// Frames lost to a slow consumer since this adapter was built.
    fn dropped_frames(&self) -> u64;
}

/// Where the time comes from.
pub trait Clock {
    /// The wall clock.
    fn now(&self) -> jiff::Timestamp;
    /// The monotonic clock.
    fn instant(&self) -> Instant;
    /// Sleep until `i`. Returns at once if `i` has passed.
    fn sleep_until(&self, i: Instant);
    /// Called after a join is handed to its thread. The real clock moves on
    /// by itself and does nothing here. A fake clock waits for `done`, so
    /// that a join takes no fake time.
    fn settle(&self, _done: &dyn Fn() -> bool) {}
}

/// The system clocks.
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> jiff::Timestamp {
        jiff::Timestamp::now()
    }

    fn instant(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&self, i: Instant) {
        let wait = i.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            thread::sleep(wait);
        }
    }
}

/// A [`ListenOnlyStation`] joined to one net's target.
pub struct StationAdapter {
    listen: ListenOnlyStation,
    target: Target,
    login: String,
}

impl StationAdapter {
    /// Build the listener for `net`, logging in as `callsign` with `suffix`.
    /// Opens no socket and no device.
    ///
    /// # Errors
    /// [`ListenError::BadCallsign`] if the callsign cannot log in on this
    /// network, or whatever [`ListenOnlyStation::new`] refuses.
    pub fn new(
        net: &NetCfg,
        callsign: &str,
        suffix: char,
        vocoder: DstarVocoder,
        trusted_serials: &[String],
    ) -> Result<Self, ListenError> {
        Ok(Self {
            login: login_callsign(net.target.network(), callsign, suffix)?,
            listen: ListenOnlyStation::new(vocoder, trusted_serials)?,
            target: net.target.clone(),
        })
    }
}

impl NetAdapter for StationAdapter {
    fn join(&mut self) -> Result<Box<dyn AudioFeed>, String> {
        let joined = match &self.target {
            Target::M17 { host, port, module } => {
                self.listen.join_m17(host, *port, *module, &self.login)
            }
            Target::Dstar {
                host,
                port,
                module,
                reflector,
            } => self
                .listen
                .join_dstar(host, *port, *module, &self.login, reflector.as_deref()),
        };
        joined.map_err(|e| e.to_string())
    }

    fn status(&self) -> LinkStatus {
        self.listen.status()
    }

    fn talker(&self) -> Option<String> {
        self.listen.talker()
    }

    fn leave(&mut self) {
        self.listen.leave();
    }

    fn rate(&self) -> u32 {
        self.listen.rate()
    }

    fn dropped_frames(&self) -> u64 {
        self.listen.dropped_frames()
    }
}

/// Record one session, joining at the start and leaving at `leave_at`, or
/// when `stop` is set. The file ends up as long as the session ran, whatever
/// the link did. A machine that sleeps through part of the session leaves the
/// file short by that much: nothing can record while it sleeps.
///
/// An over that cannot be written is logged and the session carries on. If
/// `session.flac` itself cannot be written, the session ends with
/// [`Ended::Error`].
///
/// # Errors
/// [`SessionError`] only if the session's files cannot be closed.
pub fn run_session(
    adapter: &mut dyn NetAdapter,
    rec: SessionRecorder,
    leave_at: jiff::Timestamp,
    clock: &dyn Clock,
    stop: &AtomicBool,
) -> Result<SessionMeta, SessionError> {
    let mut timeline = Timeline::new(rec, leave_at, clock);
    thread::scope(|scope| {
        let mut link = Link::new(adapter, clock);
        let ended = loop {
            if timeline.done() {
                break Ended::Window;
            }
            clock.sleep_until(timeline.deadline());
            if stop.load(Ordering::Acquire) {
                break Ended::Stopped;
            }
            timeline.note_clocks_apart();
            // This comes before the link moves, so that a machine waking
            // after `leave_at` cannot start a join. The last tick still runs,
            // because a stall that moved both clocks has a gap to fill.
            let over = timeline.window_over();
            let stepped = if over {
                Ok(())
            } else {
                link.step(scope, &mut timeline)
            };
            if stepped
                .and_then(|()| timeline.advance(&mut link, over))
                .is_err()
            {
                break Ended::Error;
            }
            if over {
                break Ended::Window;
            }
        };
        link.finish(timeline, ended)
    })
}

/// The session's clock and its files. Tick `k` belongs at `start + k * 20 ms`.
struct Timeline<'c> {
    rec: SessionRecorder,
    clock: &'c dyn Clock,
    start: Instant,
    start_at: jiff::Timestamp,
    leave_at: jiff::Timestamp,
    ticks: u64,
    total: u64,
    errors: HashSet<String>,
    over_failed: bool,
}

impl<'c> Timeline<'c> {
    fn new(rec: SessionRecorder, leave_at: jiff::Timestamp, clock: &'c dyn Clock) -> Self {
        let start_at = clock.now();
        let left = leave_at.duration_since(start_at).as_millis().max(0);
        Self {
            rec,
            clock,
            start: clock.instant(),
            start_at,
            leave_at,
            ticks: 0,
            total: u64::try_from(left).unwrap_or(u64::MAX).div_ceil(TICK_MS),
            errors: HashSet::new(),
            over_failed: false,
        }
    }

    fn done(&self) -> bool {
        self.ticks >= self.total
    }

    /// Whether the wall clock has reached the end of the window.
    fn window_over(&self) -> bool {
        self.clock.now() >= self.leave_at
    }

    /// Log one `Error` row if the two clocks have parted, which means the
    /// machine slept or the wall clock was set. The file cannot be mended:
    /// from here on it runs behind the wall clock, and the session says so.
    fn note_clocks_apart(&mut self) {
        let wall = self.clock.now().duration_since(self.start_at).as_millis();
        let ticks = self
            .clock
            .instant()
            .saturating_duration_since(self.start)
            .as_millis();
        let apart = wall.abs_diff(i128::try_from(ticks).unwrap_or(i128::MAX));
        if apart > RESYNC_AFTER.as_millis() {
            self.error_once(
                "the wall clock and the recording's clock differ by more than 5 s; \
                 the machine may have slept, and the file no longer lines up with the wall clock"
                    .to_string(),
            );
        }
    }

    /// When the next tick is due.
    fn deadline(&self) -> Instant {
        self.start + Duration::from_millis(self.ticks * TICK_MS)
    }

    /// Write the tick that is due. First, if the loop has fallen too far
    /// behind, fill the gap. On the last pass, after the wall clock has
    /// reached the end of the window, any gap is filled, because there is no
    /// later tick to catch up on and the file would end short.
    fn advance(&mut self, link: &mut Link<'_, '_>, last: bool) -> Result<(), SessionError> {
        let behind = self
            .clock
            .instant()
            .saturating_duration_since(self.deadline());
        if behind > CATCH_UP_AFTER || (last && behind > Duration::ZERO) {
            self.catch_up(behind, link)?;
        }
        if self.done() {
            return Ok(());
        }
        let frame = link.take();
        let talker = link.talker();
        self.write(frame.as_ref(), talker.as_deref())
    }

    /// Fill a gap of `behind`, so the file keeps wall-clock time. Audio that
    /// queued up during the gap is the newest thing in it, so it goes at the
    /// end, and the rest of the gap is silence. The feed is read only until
    /// it runs dry.
    ///
    /// The talker is whoever the network names now, because frames carry no
    /// name of their own. If the talker changed during the gap, the queued
    /// audio is credited to the newer one.
    fn catch_up(&mut self, behind: Duration, link: &mut Link<'_, '_>) -> Result<(), SessionError> {
        if behind > RESYNC_AFTER {
            self.error_once("recording fell more than 5 s behind; the gap is silence".to_string());
        }
        let missed = u64::try_from(behind.as_millis() / u128::from(TICK_MS)).unwrap_or(u64::MAX);
        let missed = missed.min(self.total - self.ticks);
        let mut queued = VecDeque::new();
        while (queued.len() as u64) < missed {
            match link.take() {
                Some(frame) => queued.push_back(frame),
                None => break,
            }
        }
        for _ in queued.len() as u64..missed {
            self.write(None, None)?;
        }
        let talker = link.talker();
        for frame in &queued {
            self.write(Some(frame), talker.as_deref())?;
        }
        Ok(())
    }

    /// Write one tick at its own place on the timeline. Only a failure of
    /// `session.flac` comes back as an error.
    fn write(&mut self, frame: Option<&Frame>, talker: Option<&str>) -> Result<(), SessionError> {
        let at = self.deadline();
        let offset = i64::try_from(self.ticks * TICK_MS).unwrap_or(i64::MAX);
        let now = self
            .start_at
            .checked_add(jiff::SignedDuration::from_millis(offset))
            .unwrap_or(self.start_at);
        let now = self.rec.zoned(now);
        self.ticks += 1;
        match self.rec.tick(frame, talker, at, now) {
            Err(SessionError::Over(e)) => {
                if !self.over_failed {
                    self.over_failed = true;
                    tracing::warn!("an over could not be written: {e}");
                }
                Ok(())
            }
            other => other,
        }
    }

    /// Append a row. A log that will not take it must not stop the audio.
    fn row(&mut self, kind: LogKind) {
        if let Err(e) = self.rec.event(kind, self.clock.now()) {
            tracing::warn!("a session log row could not be written: {e}");
        }
    }

    /// Append an `Error` row, the first time this text comes up.
    fn error_once(&mut self, message: String) {
        if self.errors.insert(message.clone()) {
            self.row(LogKind::Error { message });
        }
    }
}

/// The result of a join, with the adapter it borrowed.
type Joined<'a> = (&'a mut dyn NetAdapter, Result<Box<dyn AudioFeed>, String>);

/// Where the link is.
enum Phase<'scope, 'a> {
    /// Not joined. The next attempt is due at `retry_at`.
    Waiting { retry_at: Instant },
    /// A join is running on its own thread, which has the adapter.
    Joining(ScopedJoinHandle<'scope, Joined<'a>>),
    /// Joined, and waiting for the reflector since `since`.
    Linking { since: Instant },
    /// The reflector accepted the link at `since`.
    Linked { since: Instant },
}

/// The link side of a session.
struct Link<'scope, 'a: 'scope> {
    /// `None` only while a join has it, or after a join panicked with it.
    adapter: Option<&'a mut dyn NetAdapter>,
    phase: Phase<'scope, 'a>,
    /// The newest join's feed. It outlives the link, so what was queued when
    /// the link dropped still reaches the file.
    feed: Option<Box<dyn AudioFeed>>,
    backoff: Duration,
    ever_linked: bool,
    rejoins: u32,
    dropped_at_start: u64,
    dropped: u64,
}

impl<'scope, 'a: 'scope> Link<'scope, 'a> {
    fn new(adapter: &'a mut dyn NetAdapter, clock: &dyn Clock) -> Self {
        let dropped_at_start = adapter.dropped_frames();
        Self {
            adapter: Some(adapter),
            phase: Phase::Waiting {
                retry_at: clock.instant(),
            },
            feed: None,
            backoff: BACKOFF_START,
            ever_linked: false,
            rejoins: 0,
            dropped_at_start,
            dropped: dropped_at_start,
        }
    }

    fn take(&mut self) -> Option<Frame> {
        self.feed.as_mut()?.take()
    }

    fn talker(&self) -> Option<String> {
        self.adapter.as_ref()?.talker()
    }

    /// Move the link on by one tick. This never waits for the network.
    ///
    /// # Errors
    /// The join thread panicked, so there is no adapter left to go on with.
    fn step(
        &mut self,
        scope: &'scope Scope<'scope, '_>,
        tl: &mut Timeline<'_>,
    ) -> Result<(), SessionError> {
        let now = tl.clock.instant();
        if matches!(self.phase, Phase::Waiting { retry_at } if now >= retry_at) {
            self.start_join(scope, tl.clock);
        }
        if matches!(&self.phase, Phase::Joining(handle) if handle.is_finished()) {
            self.finish_join(tl)?;
        }
        if let Some(adapter) = &self.adapter {
            self.dropped = adapter.dropped_frames();
        }
        self.watch(tl);
        Ok(())
    }

    fn start_join(&mut self, scope: &'scope Scope<'scope, '_>, clock: &dyn Clock) {
        let Some(adapter) = self.adapter.take() else {
            return;
        };
        let handle = scope.spawn(move || {
            let joined = adapter.join();
            (adapter, joined)
        });
        clock.settle(&|| handle.is_finished());
        self.phase = Phase::Joining(handle);
    }

    fn finish_join(&mut self, tl: &mut Timeline<'_>) -> Result<(), SessionError> {
        let now = tl.clock.instant();
        let phase = std::mem::replace(&mut self.phase, Phase::Waiting { retry_at: now });
        let Phase::Joining(handle) = phase else {
            return Ok(());
        };
        let Ok((adapter, joined)) = handle.join() else {
            let message = "the join thread panicked".to_string();
            tl.error_once(message.clone());
            return Err(SessionError::Io(std::io::Error::other(message)));
        };
        self.adapter = Some(adapter);
        match joined {
            Ok(feed) => {
                self.feed = Some(feed);
                self.phase = Phase::Linking { since: now };
            }
            Err(reason) => self.join_failed(reason, tl),
        }
        Ok(())
    }

    /// Act on what the adapter says about the link.
    fn watch(&mut self, tl: &mut Timeline<'_>) {
        let since = match self.phase {
            Phase::Linking { since } => Some(since),
            Phase::Linked { .. } => None,
            Phase::Waiting { .. } | Phase::Joining(_) => return,
        };
        let Some(adapter) = &self.adapter else {
            return;
        };
        match (adapter.status(), since) {
            (LinkStatus::Linked, Some(_)) => self.linked(tl),
            (LinkStatus::Failed(reason) | LinkStatus::Ended(reason), Some(_)) => {
                self.join_failed(reason, tl);
            }
            (LinkStatus::Failed(reason) | LinkStatus::Ended(reason), None) => {
                tl.row(LogKind::Dropped { reason });
                self.forgive_a_stable_link(tl);
                self.leave_and_wait(tl);
            }
            (LinkStatus::Linking, Some(since))
                if tl.clock.instant().saturating_duration_since(since) >= JOIN_TIMEOUT =>
            {
                let reason = format!(
                    "no link after {} s of waiting for the reflector",
                    JOIN_TIMEOUT.as_secs()
                );
                self.join_failed(reason, tl);
            }
            (LinkStatus::Linking, _) | (LinkStatus::Linked, None) => {}
        }
    }

    fn linked(&mut self, tl: &mut Timeline<'_>) {
        self.phase = Phase::Linked {
            since: tl.clock.instant(),
        };
        if self.ever_linked {
            self.rejoins += 1;
            tl.row(LogKind::Rejoined);
        } else {
            self.ever_linked = true;
            tl.row(LogKind::Joined);
        }
    }

    /// Start the backoff again if the link that just dropped held long enough
    /// to count as working.
    fn forgive_a_stable_link(&mut self, tl: &Timeline<'_>) {
        if let Phase::Linked { since } = self.phase
            && tl.clock.instant().saturating_duration_since(since) >= STABLE_LINK
        {
            self.backoff = BACKOFF_START;
        }
    }

    fn join_failed(&mut self, reason: String, tl: &mut Timeline<'_>) {
        tl.error_once(reason);
        self.leave_and_wait(tl);
    }

    /// Drop what is left of the link and set the next attempt.
    fn leave_and_wait(&mut self, tl: &Timeline<'_>) {
        if let Some(adapter) = self.adapter.as_mut() {
            adapter.leave();
        }
        let wait = self.backoff;
        self.backoff = (wait * 2).min(BACKOFF_CAP);
        self.phase = Phase::Waiting {
            retry_at: tl.clock.instant() + wait,
        };
    }

    /// End the session. A join still running keeps the adapter, and may keep
    /// it for a long time, so the files close first and it is left afterwards.
    fn finish(mut self, mut tl: Timeline<'_>, ended: Ended) -> Result<SessionMeta, SessionError> {
        if let Some(adapter) = self.adapter.as_mut() {
            adapter.leave();
        }
        tl.row(LogKind::Left);
        let now = tl.rec.zoned(tl.clock.now());
        let dropped = self.dropped.saturating_sub(self.dropped_at_start);
        let closed = tl.rec.close(ended, self.rejoins, dropped, now);
        if let Phase::Joining(handle) = self.phase
            && let Ok((adapter, _)) = handle.join()
        {
            adapter.leave();
        }
        closed
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A clock that only moves when told to, and a network that does what its
    //! script says.

    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

    use super::*;

    struct ClockInner {
        base: Instant,
        start: jiff::Timestamp,
        elapsed_ms: AtomicU64,
        slept_ms: AtomicU64,
        free_running: AtomicBool,
    }

    /// A clock moved only by `sleep_until` and `advance`, so a ten-minute
    /// window runs in milliseconds.
    #[derive(Clone)]
    pub(crate) struct FakeClock(Arc<ClockInner>);

    impl FakeClock {
        pub(crate) fn new(start: jiff::Timestamp) -> Self {
            Self(Arc::new(ClockInner {
                base: Instant::now(),
                start,
                elapsed_ms: AtomicU64::new(0),
                slept_ms: AtomicU64::new(0),
                free_running: AtomicBool::new(false),
            }))
        }

        /// Fake time since the clock was made.
        pub(crate) fn elapsed(&self) -> Duration {
            Duration::from_millis(self.0.elapsed_ms.load(Ordering::SeqCst))
        }

        /// Move the clock on, as a stall would.
        pub(crate) fn advance(&self, by: Duration) {
            let ms = u64::try_from(by.as_millis()).expect("a test-sized duration");
            self.0.elapsed_ms.fetch_add(ms, Ordering::SeqCst);
        }

        /// Sleep the machine: the wall clock moves on and the monotonic clock
        /// does not, because `Instant` stops while a machine sleeps.
        pub(crate) fn machine_sleep(&self, by: Duration) {
            let ms = u64::try_from(by.as_millis()).expect("a test-sized duration");
            self.0.slept_ms.fetch_add(ms, Ordering::SeqCst);
        }

        /// Stop waiting for joins, so fake time passes during one. Each sleep
        /// then takes a little real time too, for the join's thread to see it.
        pub(crate) fn free_running(&self) {
            self.0.free_running.store(true, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> jiff::Timestamp {
            let slept = Duration::from_millis(self.0.slept_ms.load(Ordering::SeqCst));
            let wall = self.elapsed() + slept;
            let ms = i64::try_from(wall.as_millis()).expect("a test-sized duration");
            self.0
                .start
                .checked_add(jiff::SignedDuration::from_millis(ms))
                .expect("a test-sized time")
        }

        fn instant(&self) -> Instant {
            self.0.base + self.elapsed()
        }

        fn sleep_until(&self, i: Instant) {
            let target = i.saturating_duration_since(self.0.base);
            let ms = u64::try_from(target.as_millis()).expect("a test-sized duration");
            self.0.elapsed_ms.fetch_max(ms, Ordering::SeqCst);
            if self.0.free_running.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_micros(200));
            }
        }

        fn settle(&self, done: &dyn Fn() -> bool) {
            while !self.0.free_running.load(Ordering::SeqCst) && !done() {
                thread::yield_now();
            }
        }
    }

    /// What one join does. The last plan repeats for ever.
    #[derive(Clone)]
    pub(crate) enum Plan {
        /// `join` returns this error.
        Refuse(String),
        /// `join` succeeds. The status follows `script`, whose offsets count
        /// from the join, and the feed gives `voice` frames and then nothing.
        Link {
            script: Vec<(Duration, LinkStatus)>,
            voice: u64,
        },
        /// `join` blocks until the test says so, then does the inner plan.
        Until(Arc<dyn Fn() -> bool + Send + Sync>, Box<Plan>),
        /// `join` panics.
        Panic,
    }

    impl Plan {
        /// Link at once and stay up, giving `voice` frames.
        pub(crate) fn up(voice: u64) -> Self {
            Self::Link {
                script: vec![(Duration::ZERO, LinkStatus::Linked)],
                voice,
            }
        }
    }

    /// What the fake saw, shared with the test.
    #[derive(Default)]
    pub(crate) struct Seen {
        /// The fake time of every join attempt.
        pub(crate) joins: Vec<Duration>,
        /// Calls to `leave`.
        pub(crate) leaves: u32,
        /// Calls to `take`, on any feed.
        pub(crate) takes: u64,
    }

    /// A scripted network.
    pub(crate) struct FakeAdapter {
        clock: FakeClock,
        plans: VecDeque<Plan>,
        script: Option<(Duration, Vec<(Duration, LinkStatus)>)>,
        rate: u32,
        /// What the fake saw.
        pub(crate) seen: Arc<Mutex<Seen>>,
        /// Reported as the talker.
        pub(crate) talker: Option<String>,
        /// Fake times at which `status` stalls, and for how long.
        pub(crate) stalls: Mutex<Vec<(Duration, Duration)>>,
        /// Set this flag once the clock reaches this fake time.
        pub(crate) stop_at: Option<(Duration, Arc<AtomicBool>)>,
        /// The dropped-frame count, and the fake time and value it changes to.
        pub(crate) dropped: (u64, Duration, u64),
    }

    impl FakeAdapter {
        pub(crate) fn new(clock: &FakeClock, rate: u32, plans: Vec<Plan>) -> Self {
            Self {
                clock: clock.clone(),
                plans: plans.into(),
                script: None,
                rate,
                seen: Arc::default(),
                talker: None,
                stalls: Mutex::default(),
                stop_at: None,
                dropped: (0, Duration::ZERO, 0),
            }
        }

        fn next_plan(&mut self) -> Plan {
            if self.plans.len() > 1 {
                self.plans.pop_front().expect("a plan")
            } else {
                self.plans.front().expect("a plan").clone()
            }
        }

        fn run_plan(&mut self, plan: Plan) -> Result<Box<dyn AudioFeed>, String> {
            match plan {
                Plan::Refuse(text) => Err(text),
                Plan::Panic => panic!("scripted join panic"),
                Plan::Until(ready, then) => {
                    while !ready() {
                        thread::yield_now();
                    }
                    self.run_plan(*then)
                }
                Plan::Link { script, voice } => {
                    self.script = Some((self.clock.elapsed(), script));
                    Ok(Box::new(FakeFeed {
                        left: voice,
                        rate: self.rate,
                        seen: Arc::clone(&self.seen),
                    }))
                }
            }
        }
    }

    impl NetAdapter for FakeAdapter {
        fn join(&mut self) -> Result<Box<dyn AudioFeed>, String> {
            let at = self.clock.elapsed();
            self.seen.lock().unwrap().joins.push(at);
            let plan = self.next_plan();
            self.run_plan(plan)
        }

        fn status(&self) -> LinkStatus {
            let now = self.clock.elapsed();
            if let Some((at, flag)) = &self.stop_at
                && now >= *at
            {
                flag.store(true, Ordering::SeqCst);
            }
            let mut stalls = self.stalls.lock().unwrap();
            if let Some(i) = stalls.iter().position(|(at, _)| now >= *at) {
                let (_, by) = stalls.remove(i);
                self.clock.advance(by);
            }
            let Some((joined, script)) = &self.script else {
                return LinkStatus::Ended("not joined".to_string());
            };
            script
                .iter()
                .rev()
                .find(|(offset, _)| now >= *joined + *offset)
                .map_or(LinkStatus::Linking, |(_, status)| status.clone())
        }

        fn talker(&self) -> Option<String> {
            self.talker.clone()
        }

        fn leave(&mut self) {
            self.script = None;
            self.seen.lock().unwrap().leaves += 1;
        }

        fn rate(&self) -> u32 {
            self.rate
        }

        fn dropped_frames(&self) -> u64 {
            let (before, at, after) = self.dropped;
            if self.clock.elapsed() >= at {
                after
            } else {
                before
            }
        }
    }

    struct FakeFeed {
        left: u64,
        rate: u32,
        seen: Arc<Mutex<Seen>>,
    }

    impl AudioFeed for FakeFeed {
        fn take(&mut self) -> Option<Frame> {
            self.seen.lock().unwrap().takes += 1;
            if self.left == 0 {
                return None;
            }
            self.left -= 1;
            Some(Frame {
                rate: self.rate,
                samples: vec![1_000; (self.rate / 50) as usize],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    use astar_netarchive::{LOG_FILE, read_meta, read_rows};

    use super::testing::{FakeAdapter, FakeClock, Plan, Seen};
    use super::*;
    use crate::config::Visibility;
    use crate::schedule::Window;

    const RATE: u32 = 8000;
    const FRAME: usize = 160;

    const fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn temp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "netrec-runner-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn net() -> NetCfg {
        NetCfg {
            name: "Tuesday M17 net".into(),
            target: Target::M17 {
                host: "example.org".into(),
                port: 17000,
                module: 'C',
            },
            when: "Tue 19:00".into(),
            duration: secs(3600),
            tz: jiff::tz::TimeZone::get("America/Denver").unwrap(),
            visibility: Visibility::Private,
            my_side: false,
            video: false,
            youtube_live: false,
            youtube_upload: false,
        }
    }

    fn t0() -> jiff::Timestamp {
        "2026-11-03T18:55:00-07:00".parse().unwrap()
    }

    /// A session of `seconds`, a clock at its start, and an open recorder.
    struct Rig {
        clock: FakeClock,
        rec: SessionRecorder,
        dir: PathBuf,
        leave_at: jiff::Timestamp,
        seconds: u64,
    }

    impl Rig {
        fn new(tag: &str, seconds: u64) -> Self {
            let root = temp(tag);
            let leave_at = t0()
                .checked_add(jiff::SignedDuration::from_secs(
                    i64::try_from(seconds).unwrap(),
                ))
                .unwrap();
            let window = Window {
                net: 0,
                start: t0(),
                end: leave_at,
                join_at: t0(),
                leave_at,
            };
            let now = t0().to_zoned(net().tz);
            let rec = SessionRecorder::open(&root, &net(), &window, RATE, now).unwrap();
            let dir = rec.dir().to_owned();
            Self {
                clock: FakeClock::new(t0()),
                rec,
                dir,
                leave_at,
                seconds,
            }
        }

        fn adapter(&self, plans: Vec<Plan>) -> FakeAdapter {
            FakeAdapter::new(&self.clock, RATE, plans)
        }

        fn run(self, adapter: &mut FakeAdapter) -> Done {
            self.run_with(adapter, &AtomicBool::new(false))
        }

        fn run_with(self, adapter: &mut FakeAdapter, stop: &AtomicBool) -> Done {
            let clock = self.clock.clone();
            self.run_on(adapter, &clock, stop)
        }

        fn run_on(self, adapter: &mut FakeAdapter, clock: &dyn Clock, stop: &AtomicBool) -> Done {
            let seconds = self.seconds;
            let meta = run_session(adapter, self.rec, self.leave_at, clock, stop).expect("closed");
            Done {
                meta,
                dir: self.dir,
                seconds,
                seen: Arc::clone(&adapter.seen),
            }
        }
    }

    /// A clock on a machine that sleeps once: after a sleep ending at `at` of
    /// tick time, the wall clock has gained `by`.
    struct SleepsOnce {
        clock: FakeClock,
        at: Duration,
        by: Duration,
        slept: AtomicBool,
    }

    impl SleepsOnce {
        fn new(clock: &FakeClock, at: Duration, by: Duration) -> Self {
            Self {
                clock: clock.clone(),
                at,
                by,
                slept: AtomicBool::new(false),
            }
        }
    }

    impl Clock for SleepsOnce {
        fn now(&self) -> jiff::Timestamp {
            self.clock.now()
        }

        fn instant(&self) -> Instant {
            self.clock.instant()
        }

        fn sleep_until(&self, i: Instant) {
            self.clock.sleep_until(i);
            if self.clock.elapsed() >= self.at && !self.slept.swap(true, Ordering::SeqCst) {
                self.clock.machine_sleep(self.by);
            }
        }

        fn settle(&self, done: &dyn Fn() -> bool) {
            self.clock.settle(done);
        }
    }

    /// A finished session.
    struct Done {
        meta: SessionMeta,
        dir: PathBuf,
        seconds: u64,
        seen: Arc<Mutex<Seen>>,
    }

    impl Done {
        /// Row kinds in order, as short names.
        fn rows(&self) -> Vec<&'static str> {
            read_rows(&self.dir)
                .unwrap()
                .iter()
                .map(|r| match r.kind {
                    LogKind::Joined => "joined",
                    LogKind::Over { .. } => "over",
                    LogKind::Dropped { .. } => "dropped",
                    LogKind::Rejoined => "rejoined",
                    LogKind::Left => "left",
                    LogKind::Error { .. } => "error",
                })
                .collect()
        }

        fn joins(&self) -> Vec<Duration> {
            self.seen.lock().unwrap().joins.clone()
        }

        fn assert_full_length(&self) {
            let want = usize::try_from(self.seconds).unwrap() * 50 * FRAME;
            assert_eq!(samples(&self.dir).len(), want);
        }
    }

    fn samples(dir: &Path) -> Vec<i32> {
        let mut r = claxon::FlacReader::open(dir.join("session.flac")).unwrap();
        r.samples().map(Result::unwrap).collect()
    }

    fn refused_link(reason: &str) -> Plan {
        Plan::Link {
            script: vec![(Duration::ZERO, LinkStatus::Ended(reason.to_string()))],
            voice: 0,
        }
    }

    #[test]
    fn joins_records_and_leaves() {
        let rig = Rig::new("joins", 10);
        let mut a = rig.adapter(vec![Plan::up(100)]);
        a.talker = Some("NC0NTL".to_string());
        let done = rig.run(&mut a);
        assert_eq!(done.rows(), ["joined", "over", "left"]);
        assert_eq!(done.meta.ended, Some(Ended::Window));
        assert_eq!(done.meta.rejoins, 0);
        assert_eq!(done.meta.overs, 1);
        assert_eq!(done.seen.lock().unwrap().leaves, 1);
        done.assert_full_length();
        assert_eq!(
            fs::read_to_string(done.dir.join("checkins.txt")).unwrap(),
            "NC0NTL\n"
        );
    }

    #[test]
    fn audio_lands_where_the_clock_says() {
        let rig = Rig::new("place", 4);
        let mut a = rig.adapter(vec![Plan::up(50)]);
        let done = rig.run(&mut a);
        let s = samples(&done.dir);
        assert!(s[..50 * FRAME].iter().all(|v| *v == 1_000));
        assert!(s[50 * FRAME..].iter().all(|v| *v == 0));
    }

    #[test]
    fn drop_then_rejoin_logs_once_each() {
        let rig = Rig::new("drop", 20);
        let drop_at_3 = Plan::Link {
            script: vec![
                (Duration::ZERO, LinkStatus::Linked),
                (secs(3), LinkStatus::Failed("keepalive lost".to_string())),
            ],
            voice: 0,
        };
        let down = Plan::Refuse("no route".to_string());
        let mut a = rig.adapter(vec![drop_at_3, down.clone(), down, Plan::up(0)]);
        let done = rig.run(&mut a);
        assert_eq!(
            done.rows(),
            ["joined", "dropped", "error", "rejoined", "left"]
        );
        let rows = read_rows(&done.dir).unwrap();
        assert_eq!(
            rows[1].kind,
            LogKind::Dropped {
                reason: "keepalive lost".to_string()
            }
        );
        assert_eq!(done.meta.rejoins, 1);
        // Dropped at 3; retries at 4, then 6, then 10.
        assert_eq!(done.joins(), [secs(0), secs(4), secs(6), secs(10)]);
        done.assert_full_length();
    }

    #[test]
    fn unreachable_for_whole_window_closes_cleanly() {
        let rig = Rig::new("unreach", 600);
        let mut a = rig.adapter(vec![Plan::Refuse("no route to host".to_string())]);
        let done = rig.run(&mut a);
        assert_eq!(done.rows(), ["error", "left"]);
        assert_eq!(done.meta.ended, Some(Ended::Window));
        let joins = done.joins();
        assert!(joins.len() <= 26, "{} attempts", joins.len());
        assert!(joins.windows(2).all(|w| w[1].abs_diff(w[0]) <= BACKOFF_CAP));
        assert_eq!(joins[..6], [0, 1, 3, 7, 15, 31].map(secs));
        assert_eq!(joins[6].abs_diff(joins[5]), secs(30));
        done.assert_full_length();
    }

    #[test]
    fn each_new_error_text_gets_its_own_row() {
        let rig = Rig::new("texts", 10);
        let a_err = Plan::Refuse("first".to_string());
        let b_err = Plan::Refuse("second".to_string());
        let mut a = rig.adapter(vec![a_err.clone(), a_err, b_err]);
        let done = rig.run(&mut a);
        assert_eq!(done.rows(), ["error", "error", "left"]);
    }

    #[test]
    fn stop_flag_ends_with_stopped() {
        let rig = Rig::new("stop", 60);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let stop = Arc::new(AtomicBool::new(false));
        a.stop_at = Some((secs(5), Arc::clone(&stop)));
        let done = rig.run_with(&mut a, &stop);
        assert_eq!(done.meta.ended, Some(Ended::Stopped));
        assert_eq!(done.rows(), ["joined", "left"]);
        assert_eq!(done.seen.lock().unwrap().leaves, 1);
        let n = samples(&done.dir).len();
        assert!(
            (5 * 50 * FRAME..=5 * 50 * FRAME + 2 * FRAME).contains(&n),
            "{n}"
        );
    }

    #[test]
    fn refused_while_linking_retries_with_backoff() {
        let rig = Rig::new("refused", 20);
        let refused = refused_link("reflector said no");
        let mut a = rig.adapter(vec![refused.clone(), refused.clone(), refused, Plan::up(0)]);
        let done = rig.run(&mut a);
        // A link that never came up is a failed join: no drop, and the first
        // link that holds is a join, not a rejoin.
        assert_eq!(done.rows(), ["error", "joined", "left"]);
        assert_eq!(done.meta.rejoins, 0);
        assert_eq!(done.joins(), [secs(0), secs(1), secs(3), secs(7)]);
        assert_eq!(done.seen.lock().unwrap().leaves, 4);
        done.assert_full_length();
    }

    #[test]
    fn stuck_linking_times_out_and_retries() {
        let rig = Rig::new("stuck", 70);
        let mut a = rig.adapter(vec![Plan::Link {
            script: vec![(Duration::ZERO, LinkStatus::Linking)],
            voice: 0,
        }]);
        let done = rig.run(&mut a);
        assert_eq!(done.rows(), ["error", "left"]);
        // Gives up at 30, waits 1; gives up at 61, waits 2.
        assert_eq!(done.joins(), [secs(0), secs(31), secs(63)]);
        assert_eq!(done.seen.lock().unwrap().leaves, 3);
        done.assert_full_length();
    }

    /// A link that comes up and goes down `up` later.
    fn link_for(up: Duration) -> Plan {
        Plan::Link {
            script: vec![
                (Duration::ZERO, LinkStatus::Linked),
                (up, LinkStatus::Ended("gone".to_string())),
            ],
            voice: 0,
        }
    }

    /// A reflector that accepts and then drops must not be logged in to once
    /// a second all night: `Linked` alone proves nothing.
    #[test]
    fn flapping_link_backs_off() {
        let rig = Rig::new("flap", 300);
        let mut a = rig.adapter(vec![link_for(secs(1))]);
        let done = rig.run(&mut a);
        // Up for 1 s each time, with waits of 1, 2, 4, 8, 16, then 30.
        let joins = done.joins();
        assert_eq!(joins[..7], [0, 2, 5, 10, 19, 36, 67].map(secs));
        assert!(joins.len() <= 14, "{} logins", joins.len());
        assert!(joins.windows(2).all(|w| w[1].abs_diff(w[0]) <= secs(31)));
        let rows = done.rows();
        let dropped = rows.iter().filter(|r| **r == "dropped").count();
        assert_eq!(dropped, joins.len());
        done.assert_full_length();
    }

    #[test]
    fn a_link_that_holds_for_a_minute_resets_the_backoff() {
        let rig = Rig::new("reset", 90);
        let down = Plan::Refuse("no route".to_string());
        let held = link_for(STABLE_LINK);
        let mut a = rig.adapter(vec![down.clone(), down.clone(), down, held, Plan::up(0)]);
        let done = rig.run(&mut a);
        // Linked at 7 and dropped at 67, so the wait is 1 s again and not 8.
        assert_eq!(done.joins(), [0, 1, 3, 7, 68].map(secs));
        assert_eq!(done.meta.rejoins, 1);
    }

    #[test]
    fn a_link_just_short_of_a_minute_does_not_reset_the_backoff() {
        let rig = Rig::new("noreset", 90);
        let down = Plan::Refuse("no route".to_string());
        let held = link_for(secs(59));
        let mut a = rig.adapter(vec![down.clone(), down.clone(), down, held, Plan::up(0)]);
        let done = rig.run(&mut a);
        // Linked at 7 and dropped at 66, and the 8 s wait stands.
        assert_eq!(done.joins(), [0, 1, 3, 7, 74].map(secs));
    }

    #[test]
    fn a_failed_rejoin_is_not_a_rejoin() {
        let rig = Rig::new("norejoin", 10);
        let brief = Plan::Link {
            script: vec![
                (Duration::ZERO, LinkStatus::Linked),
                (secs(1), LinkStatus::Ended("gone".to_string())),
            ],
            voice: 0,
        };
        let mut a = rig.adapter(vec![brief, refused_link("no")]);
        let done = rig.run(&mut a);
        assert_eq!(done.meta.rejoins, 0);
        assert_eq!(done.rows(), ["joined", "dropped", "error", "left"]);
    }

    #[test]
    fn dropped_frames_reach_session_json() {
        let rig = Rig::new("dropped", 4);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        // The count is since the adapter was built: this session lost 7.
        a.dropped = (2, secs(1), 9);
        let done = rig.run(&mut a);
        assert_eq!(done.meta.dropped_frames, 7);
        assert_eq!(read_meta(&done.dir).unwrap().dropped_frames, 7);
    }

    #[test]
    fn short_stall_catches_up_with_the_queued_audio() {
        let rig = Rig::new("short", 6);
        let mut a = rig.adapter(vec![Plan::up(60)]);
        a.stalls = Mutex::new(vec![(secs(1) / 2, secs(1) / 2)]);
        let done = rig.run(&mut a);
        done.assert_full_length();
        let voiced = samples(&done.dir).iter().filter(|v| **v != 0).count();
        assert_eq!(voiced, 60 * FRAME);
        assert!(!done.rows().contains(&"error"));
    }

    #[test]
    fn stall_past_a_second_keeps_wall_clock_and_every_frame() {
        let rig = Rig::new("stall", 10);
        let mut a = rig.adapter(vec![Plan::up(50)]);
        // 25 frames in; the other 25 are queued when the stall ends.
        a.stalls = Mutex::new(vec![(secs(1) / 2, secs(3))]);
        let done = rig.run(&mut a);
        done.assert_full_length();
        let s = samples(&done.dir);
        assert_eq!(s.iter().filter(|v| **v != 0).count(), 50 * FRAME);
        // The queued frames are the newest thing in the gap, so they end it.
        let stall_end = (25 + 150) * FRAME;
        assert_eq!(s[stall_end - 1], 1_000);
        assert_eq!(s[stall_end - 25 * FRAME - 1], 0);
        // 500 ticks, but only as many takes in the gap as found audio, plus
        // the one that found the feed dry.
        assert_eq!(done.seen.lock().unwrap().takes, 500 - 150 + 26);
        assert!(!done.rows().contains(&"error"));
    }

    #[test]
    fn stall_past_five_seconds_logs_one_row_and_resyncs() {
        let rig = Rig::new("resync", 30);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        a.stalls = Mutex::new(vec![(secs(2), secs(7)), (secs(15), secs(8))]);
        let done = rig.run(&mut a);
        done.assert_full_length();
        assert_eq!(done.rows(), ["joined", "error", "left"]);
        assert_eq!(done.meta.ended, Some(Ended::Window));
    }

    #[test]
    fn stall_past_the_end_stops_at_the_window() {
        let rig = Rig::new("pastend", 5);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        a.stalls = Mutex::new(vec![(secs(4), secs(3))]);
        let done = rig.run(&mut a);
        done.assert_full_length();
    }

    #[test]
    fn slow_join_never_holds_up_the_ticks() {
        let rig = Rig::new("slow", 10);
        rig.clock.free_running();
        let clock = rig.clock.clone();
        let ready = Arc::new(move || clock.elapsed() >= secs(5));
        let mut a = rig.adapter(vec![Plan::Until(ready, Box::new(Plan::up(0)))]);
        let done = rig.run(&mut a);
        done.assert_full_length();
        assert_eq!(done.rows(), ["joined", "left"]);
        let rows = read_rows(&done.dir).unwrap();
        let joined: jiff::Timestamp = rows[0].t.parse().unwrap();
        assert!(joined.duration_since(t0()).as_secs() >= 5);
    }

    #[test]
    fn join_still_running_at_the_end_cannot_hold_up_the_files() {
        let rig = Rig::new("hung", 3);
        rig.clock.free_running();
        let dir = rig.dir.clone();
        // This join returns only once session.json says the session ended.
        let closed = Arc::new(move || read_meta(&dir).is_ok_and(|m| m.ended.is_some()));
        let mut a = rig.adapter(vec![Plan::Until(closed, Box::new(Plan::up(0)))]);
        let done = rig.run(&mut a);
        done.assert_full_length();
        assert_eq!(done.rows(), ["left"]);
        // It is still left once it comes back.
        assert_eq!(done.seen.lock().unwrap().leaves, 1);
    }

    #[test]
    fn a_panicking_join_closes_the_session() {
        let rig = Rig::new("panic", 5);
        let mut a = rig.adapter(vec![Plan::Panic]);
        let done = rig.run(&mut a);
        assert_eq!(done.meta.ended, Some(Ended::Error));
        assert!(done.rows().contains(&"error"));
    }

    #[test]
    fn over_write_error_keeps_the_session_going() {
        use std::os::unix::fs::PermissionsExt;
        let rig = Rig::new("overerr", 6);
        let overs = rig.dir.join("overs");
        fs::set_permissions(&overs, fs::Permissions::from_mode(0o555)).unwrap();
        let mut a = rig.adapter(vec![Plan::up(100)]);
        let done = rig.run(&mut a);
        fs::set_permissions(&overs, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(done.meta.ended, Some(Ended::Window));
        done.assert_full_length();
    }

    #[test]
    fn whole_session_write_error_ends_the_session() {
        let mut rig = Rig::new("wholeerr", 6);
        rig.rec.fail_whole = true;
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let done = rig.run(&mut a);
        assert_eq!(done.meta.ended, Some(Ended::Error));
        assert_eq!(read_meta(&done.dir).unwrap().ended, Some(Ended::Error));
        assert!(done.rows().contains(&"error"));
        assert_eq!(done.seen.lock().unwrap().leaves, 1);
    }

    #[test]
    fn row_failure_does_not_stop_the_audio() {
        let rig = Rig::new("rowerr", 6);
        fs::create_dir(rig.dir.join(LOG_FILE)).unwrap();
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let seconds = rig.seconds;
        let (rec, leave_at, clock, dir) = (rig.rec, rig.leave_at, rig.clock, rig.dir);
        let meta = run_session(&mut a, rec, leave_at, &clock, &AtomicBool::new(false)).unwrap();
        assert_eq!(meta.ended, Some(Ended::Window));
        let want = usize::try_from(seconds).unwrap() * 50 * FRAME;
        assert_eq!(samples(&dir).len(), want);
    }

    #[test]
    fn low_disk_ends_the_session_with_an_error_and_leaves() {
        let mut rig = Rig::new("lowdisk", 30);
        rig.rec.set_free_fn(|_| Ok(1024));
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let done = rig.run(&mut a);
        assert_eq!(done.meta.ended, Some(Ended::Error));
        assert_eq!(done.seen.lock().unwrap().leaves, 1);
        assert!(read_rows(&done.dir).unwrap().iter().any(|r| r.kind
            == LogKind::Error {
                message: "disk nearly full".to_owned()
            }));
    }

    #[test]
    fn window_already_over_closes_at_once() {
        let mut rig = Rig::new("over", 5);
        rig.leave_at = t0();
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let done = rig.run(&mut a);
        assert_eq!(done.meta.ended, Some(Ended::Window));
        assert_eq!(done.joins(), [] as [Duration; 0]);
        assert_eq!(samples(&done.dir), [] as [i32; 0]);
    }

    /// A Mac sleeps at 10 s of a 60 s window and wakes two minutes later. The
    /// reflector dropped the link meanwhile, and the tick clock still thinks
    /// 50 s are left. The window is over, so nothing may log in again.
    #[test]
    fn wall_clock_past_leave_at_ends_session_with_no_further_join() {
        let rig = Rig::new("woke", 60);
        let dropped = Plan::Link {
            script: vec![
                (Duration::ZERO, LinkStatus::Linked),
                (secs(10), LinkStatus::Ended("timed out".to_string())),
            ],
            voice: 0,
        };
        let mut a = rig.adapter(vec![dropped, Plan::up(0)]);
        let fake = rig.clock.clone();
        let clock = SleepsOnce::new(&fake, secs(10), secs(120));
        let done = rig.run_on(&mut a, &clock, &AtomicBool::new(false));

        assert_eq!(done.joins(), [secs(0)]);
        assert_eq!(done.meta.ended, Some(Ended::Window));
        assert!(fake.elapsed() <= secs(10) + Duration::from_millis(40));
        assert_eq!(done.seen.lock().unwrap().leaves, 1);
    }

    /// A sleep that ends inside the window costs the file its alignment. The
    /// session still ends on the wall clock, and says what happened.
    #[test]
    fn a_sleep_inside_the_window_logs_one_row_and_ends_on_the_wall_clock() {
        let rig = Rig::new("nap", 60);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let fake = rig.clock.clone();
        let clock = SleepsOnce::new(&fake, secs(10), secs(30));
        let done = rig.run_on(&mut a, &clock, &AtomicBool::new(false));

        assert_eq!(done.rows(), ["joined", "error", "left"]);
        let rows = read_rows(&done.dir).unwrap();
        let LogKind::Error { message } = &rows[1].kind else {
            panic!("{:?}", rows[1].kind);
        };
        assert!(message.contains("wall clock"), "{message}");
        assert_eq!(done.meta.ended, Some(Ended::Window));
        assert_eq!(done.joins(), [secs(0)]);
        assert_eq!(fake.elapsed(), secs(30));
    }

    #[test]
    fn clocks_within_five_seconds_log_nothing() {
        let rig = Rig::new("skew", 20);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let fake = rig.clock.clone();
        let clock = SleepsOnce::new(&fake, secs(5), secs(5));
        let done = rig.run_on(&mut a, &clock, &AtomicBool::new(false));
        assert_eq!(done.rows(), ["joined", "left"]);
    }

    /// A stall moves both clocks, so the file can still be filled to the end
    /// of the window, even when the stall was in the sleep itself.
    #[test]
    fn a_late_wake_past_the_end_still_fills_the_file() {
        struct Stalls(FakeClock, AtomicBool);
        impl Clock for Stalls {
            fn now(&self) -> jiff::Timestamp {
                self.0.now()
            }
            fn instant(&self) -> Instant {
                self.0.instant()
            }
            fn sleep_until(&self, i: Instant) {
                self.0.sleep_until(i);
                if self.0.elapsed() >= secs(4) && !self.1.swap(true, Ordering::SeqCst) {
                    self.0.advance(secs(3));
                }
            }
            fn settle(&self, done: &dyn Fn() -> bool) {
                self.0.settle(done);
            }
        }
        let rig = Rig::new("latewake", 5);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let clock = Stalls(rig.clock.clone(), AtomicBool::new(false));
        let done = rig.run_on(&mut a, &clock, &AtomicBool::new(false));
        done.assert_full_length();
        assert_eq!(done.meta.ended, Some(Ended::Window));
    }

    /// A stall under a second that carries the wall clock across `leave_at`
    /// still leaves a file that runs to the end of the window.
    #[test]
    fn a_small_stall_across_the_end_still_fills_the_file() {
        struct Stalls(FakeClock, AtomicBool);
        impl Clock for Stalls {
            fn now(&self) -> jiff::Timestamp {
                self.0.now()
            }
            fn instant(&self) -> Instant {
                self.0.instant()
            }
            fn sleep_until(&self, i: Instant) {
                self.0.sleep_until(i);
                if self.0.elapsed() >= secs(4) + secs(4) / 5 && !self.1.swap(true, Ordering::SeqCst)
                {
                    self.0.advance(secs(1) / 2);
                }
            }
            fn settle(&self, done: &dyn Fn() -> bool) {
                self.0.settle(done);
            }
        }
        let rig = Rig::new("smallwake", 5);
        let mut a = rig.adapter(vec![Plan::up(0)]);
        let clock = Stalls(rig.clock.clone(), AtomicBool::new(false));
        let done = rig.run_on(&mut a, &clock, &AtomicBool::new(false));
        done.assert_full_length();
        assert_eq!(done.meta.ended, Some(Ended::Window));
    }

    #[test]
    fn station_adapter_builds_for_a_good_callsign_only() {
        let call = |c: &str| StationAdapter::new(&net(), c, 'R', DstarVocoder::Software, &[]);
        let good = call("N0CALL").map_err(|e| e.to_string()).unwrap();
        assert!(good.rate() > 0);
        assert_eq!(good.login, "N0CALL R");
        assert_eq!(good.dropped_frames(), 0);
        assert!(matches!(good.status(), LinkStatus::Ended(_)));
        assert!(matches!(call("N0CALLXX"), Err(ListenError::BadCallsign(_))));
    }
}
