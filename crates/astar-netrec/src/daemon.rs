// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The daemon loop, one-off recordings, and what `check` and `status` print.
//!
//! The loop is a sequence of passes. Each pass asks the schedule which window
//! comes next, waits for it, records it, and then asks again starting from
//! that window's `leave_at`, so a window never comes up twice.
//!
//! A window that cannot be recorded (a full disk, a network that will not
//! build, a session that fails) is tried again a minute later for as long as
//! it stays open. Nothing here spins, and nothing short of a stop ends the
//! loop: a recorder that gives up misses every net after the bad one.
//!
//! Only one recorder may run against an archive, because two would log in
//! with one identity and the second's crash recovery would mark the first's
//! open session as crashed. So a recorder holds an operating-system lock on
//! [`LOCK_FILE`] for as long as it lives. The kernel frees that lock however
//! the process ends, even in a power cut, so nothing stale can ever block a
//! start. [`STATE_FILE`] only tells `status` what the holder is doing.
//!
//! A public net with `youtube_live` or `twitch_live` on also streams live
//! while it records: the session gets a [`astar_netvideo::LiveTap`], and a
//! live stream reads it on threads of its own. Nothing about the stream can
//! hold up or fail the recording; a stream that will not start is logged
//! and the net is recorded all the same.

use std::cell::RefCell;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError as MutexTryLockError};
use std::thread;
use std::time::{Duration, Instant};

use astar_netarchive::{Ended, RetainPolicy, SessionMeta, recover_unfinished, retain};
use astar_netvideo::{
    DestName, DestState, Destination, Ffmpeg, FfmpegError, IngestUrl, LIVE_TAP_CAPACITY,
    LiveStream, LiveSummary, SessionInfo, TapReceiver, live_tap,
};
use jiff::civil::DateTime;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::config::{
    CREDENTIALS_FILE, Config, ConfigError, NetCfg, StreamKeys, TWITCH_KEY_ENV, TWITCH_KEY_NAME,
    Visibility, YOUTUBE_KEY_ENV, YOUTUBE_KEY_NAME, validate_with_keys,
};
use crate::feed::AudioFeed;
use crate::listen::LinkStatus;
use crate::render_queue::RenderQueue;
use crate::runner::{Clock, NetAdapter, StationAdapter, run_session};
use crate::schedule::{Window, current_or_next, windows_between};
use crate::session::{FreeFn, SessionError, SessionRecorder};

/// The state file's name, in `recorder.root`.
pub const STATE_FILE: &str = ".netrec-state.json";
/// The lock file's name, in `recorder.root`. It is empty and is never removed:
/// the lock on it is what counts, and removing the file while another process
/// was opening it would let two recorders lock two different files.
pub const LOCK_FILE: &str = ".netrec.lock";

/// The longest the daemon sleeps before looking at `stop` and the wall clock
/// again. Short, so a SIGTERM is answered at once and a Mac that wakes from
/// sleep joins a net on time.
const POLL: Duration = Duration::from_millis(500);
/// How long to wait before trying a window again, and between looks at an
/// empty schedule.
const RETRY_WAIT: Duration = Duration::from_secs(60);
/// How often retention runs while nothing is recording.
const RETAIN_EVERY: Duration = Duration::from_secs(3600);
/// How many times a start tries for the lock, and how long it waits between
/// tries. `status` holds the lock for an instant to see whether it is free,
/// and that must not read as a second recorder.
const LOCK_TRIES: u32 = 5;
const LOCK_RETRY_WAIT: Duration = Duration::from_millis(50);
/// How many windows `check` lists.
const CHECK_WINDOWS: usize = 10;
/// How far ahead `check` looks.
const CHECK_HOURS: i64 = 24 * 366;

/// What a factory returns: an adapter, or a sentence saying why not.
pub type Made = Result<Box<dyn NetAdapter>, String>;

/// Why the recorder would not start, or a one-off recording failed.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The archive folder or the lock file could not be used.
    #[error("{path}: {source}")]
    Io {
        /// The file or folder concerned.
        path: PathBuf,
        /// The operating-system error.
        source: io::Error,
    },
    /// Another recorder holds the lock.
    #[error(
        "another recorder is running{}, and two recorders would log in with one identity; stop it first",
        as_pid(*.pid)
    )]
    AlreadyRunning {
        /// The other recorder's process id, if its state file gives one.
        pid: Option<u32>,
    },
    /// `now` named a net the config does not have.
    #[error("no net is named {name:?}; the nets are: {known}")]
    UnknownNet {
        /// The name asked for.
        name: String,
        /// The names on offer, ready to print.
        known: String,
    },
    /// The adapter for a net could not be built.
    #[error("{0}")]
    Adapter(String),
    /// A session could not be opened or closed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// A session ran and ended with [`Ended::Error`].
    #[error("the recording of {0:?} failed; see its session log")]
    SessionFailed(String),
}

fn as_pid(pid: Option<u32>) -> String {
    pid.map(|pid| format!(" as pid {pid}")).unwrap_or_default()
}

/// What the state file says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    /// `"idle"`, `"waiting"` or `"recording"`.
    pub state: String,
    /// The net being recorded or waited for.
    pub net: Option<String>,
    /// When the recording began, RFC 3339 in the net's zone.
    pub since: Option<String>,
    /// When the next join is due, RFC 3339 in the net's zone.
    pub next: Option<String>,
    /// While recording, whether the link is up.
    pub linked: Option<bool>,
    /// The recorder's process id.
    pub pid: u32,
    /// While streaming live, where the stream stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveSummary>,
}

impl DaemonState {
    fn idle() -> Self {
        Self {
            state: "idle".to_string(),
            net: None,
            since: None,
            next: None,
            linked: None,
            pid: std::process::id(),
            live: None,
        }
    }

    fn waiting(net: &NetCfg, joins: Timestamp) -> Self {
        Self {
            state: "waiting".to_string(),
            net: Some(net.name.clone()),
            next: Some(zoned_text(net, joins)),
            ..Self::idle()
        }
    }

    fn recording(net: &NetCfg, since: Timestamp) -> Self {
        Self {
            state: "recording".to_string(),
            net: Some(net.name.clone()),
            since: Some(zoned_text(net, since)),
            linked: Some(false),
            ..Self::idle()
        }
    }
}

fn zoned_text(net: &NetCfg, t: Timestamp) -> String {
    t.to_zoned(net.tz.clone())
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

fn read_state(root: &Path) -> Option<DaemonState> {
    serde_json::from_slice(&fs::read(root.join(STATE_FILE)).ok()?).ok()
}

/// Replace the state file in one step, so a reader sees the old state or the
/// new one and never half of either.
fn write_state(root: &Path, state: &DaemonState) -> io::Result<()> {
    let tmp = root.join(format!("{STATE_FILE}.{}.tmp", state.pid));
    fs::write(&tmp, serde_json::to_vec_pretty(state)?)
        .and_then(|()| fs::rename(&tmp, root.join(STATE_FILE)))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

/// Whether a recorder holds the lock on the archive at `root`. Only a shared
/// lock is tried, and the file is opened read-only and never created, so
/// `status` needs no write access. An archive that has no lock file has never
/// had a recorder.
fn recorder_running(root: &Path) -> bool {
    File::open(root.join(LOCK_FILE))
        .is_ok_and(|file| matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock)))
}

/// This process's claim on the archive. Dropping it removes the state file
/// and then frees the lock.
struct Hold {
    root: PathBuf,
    /// What the file says now, so an unchanged state is not rewritten.
    shown: Option<DaemonState>,
    /// Held for its lock. Closing it frees the lock, and so does the end of
    /// this process, however that comes.
    _lock: File,
}

impl Hold {
    /// Take the lock, or refuse if another recorder has it.
    ///
    /// A state file that will not write is only a warning: on a full disk
    /// this is what stands between start-up and the retention that frees it.
    fn claim(root: &Path) -> Result<Self, DaemonError> {
        fs::create_dir_all(root).map_err(|source| DaemonError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let lock = lock_archive(root)?;
        let state = DaemonState::idle();
        let shown = match write_state(root, &state) {
            Ok(()) => Some(state),
            Err(e) => {
                tracing::warn!("the state file could not be written: {e}");
                // What is there is a dead recorder's. Better no news than that.
                let _ = fs::remove_file(root.join(STATE_FILE));
                None
            }
        };
        Ok(Self {
            root: root.to_path_buf(),
            shown,
            _lock: lock,
        })
    }

    /// Show `state`. A state file that will not write costs `status` its
    /// news, which is no reason to stop recording.
    fn show(&mut self, state: DaemonState) {
        if self.shown.as_ref() == Some(&state) {
            return;
        }
        if let Err(e) = write_state(&self.root, &state) {
            tracing::warn!("the state file could not be written: {e}");
        }
        self.shown = Some(state);
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.root.join(STATE_FILE));
    }
}

/// Open the lock file and take the lock on it.
fn lock_archive(root: &Path) -> Result<File, DaemonError> {
    let path = root.join(LOCK_FILE);
    let io_err = |source| DaemonError::Io {
        path: path.clone(),
        source,
    };
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io_err)?;
    for _ in 0..LOCK_TRIES {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => thread::sleep(LOCK_RETRY_WAIT),
            Err(TryLockError::Error(e)) => return Err(io_err(e)),
        }
    }
    Err(DaemonError::AlreadyRunning {
        pid: read_state(root).map(|held| held.pid),
    })
}

/// An adapter that keeps the state file's `linked` true to the link, and
/// its `live` true to the live stream. The session polls `status` every
/// tick, so this sees every change, and writes only on a change. It owns
/// what it needs because a join moves the adapter to its own thread.
struct Watched {
    inner: Box<dyn NetAdapter>,
    root: PathBuf,
    /// What the state file says now.
    shown: RefCell<DaemonState>,
    /// The live stream's summary, which the stream keeps current.
    live: Option<Arc<Mutex<LiveSummary>>>,
}

impl Watched {
    /// The live summary, unless the stream holds it this instant: a tick
    /// never waits for it, and the next tick looks again.
    fn live_now(&self, previous: Option<&LiveSummary>) -> Option<LiveSummary> {
        let slot = self.live.as_ref()?;
        match slot.try_lock() {
            Ok(s) => Some(s.clone()),
            Err(MutexTryLockError::Poisoned(p)) => Some(p.into_inner().clone()),
            Err(MutexTryLockError::WouldBlock) => previous.cloned(),
        }
    }
}

/// Whether two live summaries show the same thing: the rung, and each
/// destination's name and state. A retry count or an error's wording
/// changes on every failed connection, and rewriting the state file for
/// each would be churn, so those ride along with the next real change.
fn same_live(a: Option<&LiveSummary>, b: Option<&LiveSummary>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.profile == b.profile
                && a.destinations.len() == b.destinations.len()
                && a.destinations
                    .iter()
                    .zip(&b.destinations)
                    .all(|(x, y)| x.name == y.name && x.state == y.state)
        }
        _ => false,
    }
}

impl NetAdapter for Watched {
    fn join(&mut self) -> Result<Box<dyn AudioFeed>, String> {
        self.inner.join()
    }

    fn status(&self) -> LinkStatus {
        let status = self.inner.status();
        let linked = matches!(status, LinkStatus::Linked);
        let mut shown = self.shown.borrow_mut();
        let live = self.live_now(shown.live.as_ref());
        if shown.linked != Some(linked) || !same_live(shown.live.as_ref(), live.as_ref()) {
            shown.linked = Some(linked);
            shown.live = live;
            if let Err(e) = write_state(&self.root, &shown) {
                tracing::warn!("the state file could not be written: {e}");
            }
        }
        status
    }

    fn talker(&self) -> Option<String> {
        self.inner.talker()
    }

    fn leave(&mut self) {
        self.inner.leave();
    }

    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn dropped_frames(&self) -> u64 {
        self.inner.dropped_frames()
    }
}

/// Build what a session needs: the adapter, then its files. Nothing is
/// joined yet.
fn open_session(
    root: &Path,
    net: &NetCfg,
    window: &Window,
    clock: &dyn Clock,
    make: &dyn Fn(&NetCfg) -> Made,
    free: FreeFn,
) -> Result<(Box<dyn NetAdapter>, SessionRecorder), DaemonError> {
    let adapter = make(net).map_err(DaemonError::Adapter)?;
    let now = clock.now().to_zoned(net.tz.clone());
    let rec = SessionRecorder::open_with(root, net, window, adapter.rate(), now, free)?;
    Ok((adapter, rec))
}

/// Record an opened session to `leave_at`, showing it in the state file,
/// and stream it live if the net says so. Returns the session's folder with
/// how it ended.
#[allow(clippy::too_many_arguments)] // one session's whole context
fn run_open_session(
    hold: &mut Hold,
    cfg: &Config,
    live: &Live,
    net: &NetCfg,
    (adapter, mut rec): (Box<dyn NetAdapter>, SessionRecorder),
    leave_at: Timestamp,
    clock: &dyn Clock,
    stop: &AtomicBool,
) -> (PathBuf, Result<SessionMeta, SessionError>) {
    let dir = rec.dir().to_path_buf();
    let state = DaemonState::recording(net, clock.now());
    hold.show(state.clone());
    let stream = start_live(cfg, live, net, &mut rec);
    // `shown` is what is on disk, which has no `live` yet: the first tick
    // writes it.
    let mut watched = Watched {
        inner: adapter,
        root: hold.root.clone(),
        shown: RefCell::new(state),
        live: stream.as_ref().map(|h| Arc::clone(&h.summary)),
    };
    let result = run_session(&mut watched, rec, leave_at, clock, stop);
    if let Some(handle) = stream {
        let last = (handle.stop)();
        tracing::info!(
            "the live stream of {:?} ended: {}",
            net.name,
            describe_live(&last)
        );
    }
    // `Watched` wrote past `hold`, so what it remembers may be out of date.
    hold.shown = None;
    (dir, result)
}

/// Give `rec` a live tap and start its stream, when `net` streams live
/// anywhere. `None` (and the recording carries on) when it does not, or
/// when the stream would not start.
fn start_live(
    cfg: &Config,
    live: &Live,
    net: &NetCfg,
    rec: &mut SessionRecorder,
) -> Option<LiveHandle> {
    let dests = live_destinations(cfg, net, &live.keys);
    if dests.is_empty() {
        return None;
    }
    let info = match SessionInfo::from_meta(rec.meta(), &cfg.recorder.my_callsigns) {
        Ok(info) => info,
        Err(e) => {
            tracing::error!("{:?} will not stream live: {e}", net.name);
            return None;
        }
    };
    let (tap, rx) = live_tap(LIVE_TAP_CAPACITY);
    let handle = (live.start)(info, dests, rx)?;
    rec.set_tap(tap);
    Some(handle)
}

/// Where `net` streams live: each destination switched on that has a key
/// and a usable ingest URL. Empty for a private net, whatever its switches
/// say (`check` refuses that config; this refuses it again). A destination
/// switched on with no key, or a bad URL, is skipped with one error line.
#[must_use]
pub fn live_destinations(cfg: &Config, net: &NetCfg, keys: &StreamKeys) -> Vec<Destination> {
    if !(net.youtube_live || net.twitch_live) {
        return Vec::new();
    }
    if net.visibility != Visibility::Public {
        tracing::error!(
            "{:?} is private, so it is never streamed live; set visibility = \"public\" to stream it",
            net.name
        );
        return Vec::new();
    }
    let wanted = [
        (
            net.youtube_live,
            DestName::YouTube,
            &cfg.live.youtube_ingest,
            &keys.youtube,
            YOUTUBE_KEY_NAME,
            YOUTUBE_KEY_ENV,
        ),
        (
            net.twitch_live,
            DestName::Twitch,
            &cfg.live.twitch_ingest,
            &keys.twitch,
            TWITCH_KEY_NAME,
            TWITCH_KEY_ENV,
        ),
    ];
    let mut out = Vec::new();
    for (on, name, ingest, key, key_name, env) in wanted {
        if !on {
            continue;
        }
        let Some(key) = key else {
            tracing::error!(
                "{:?} is not streamed to {name}: no {name} stream key is set; put {key_name} in {CREDENTIALS_FILE} beside the config, or set {env}",
                net.name
            );
            continue;
        };
        match IngestUrl::parse(ingest) {
            Ok(ingest) => out.push(Destination {
                name,
                ingest,
                key: key.clone(),
            }),
            Err(e) => tracing::error!("{:?} is not streamed to {name}: {e}", net.name),
        }
    }
    out
}

/// What starts a live stream for a session: its frame info, where to send
/// it, and its tap. `None` when no stream started; the recording goes on.
pub type LiveStarter = dyn Fn(SessionInfo, Vec<Destination>, TapReceiver) -> Option<LiveHandle>;

/// A running live stream, as the daemon sees it.
pub struct LiveHandle {
    /// Where the stream stands, kept current by the stream.
    pub summary: Arc<Mutex<LiveSummary>>,
    /// Stops the stream and says where it stood.
    pub stop: Box<dyn FnOnce() -> LiveSummary>,
}

impl LiveHandle {
    /// The handle on a running [`LiveStream`].
    #[must_use]
    pub fn of(stream: LiveStream) -> Self {
        Self {
            summary: stream.summary_slot(),
            stop: Box::new(move || stream.stop()),
        }
    }
}

/// What the daemon streams live with: the stream keys and a starter.
pub struct Live {
    /// The keys, each present or not.
    pub keys: StreamKeys,
    /// Starts a session's stream.
    pub start: Box<LiveStarter>,
}

impl Live {
    /// Nothing streams.
    #[must_use]
    pub fn off() -> Self {
        Self {
            keys: StreamKeys::default(),
            start: Box::new(|_, _, _| None),
        }
    }

    /// Stream with `ff`. Without one, a net that wants to stream is
    /// recorded only, with an error line saying why.
    #[must_use]
    pub fn with_ffmpeg(keys: StreamKeys, ff: Option<Ffmpeg>) -> Self {
        Self {
            keys,
            start: Box::new(move |info, dests, rx| {
                let Some(ff) = ff.clone() else {
                    tracing::error!(
                        "{:?} is not streamed live: no usable ffmpeg was found (see `astar-netrec check`)",
                        info.net
                    );
                    return None;
                };
                let net = info.net.clone();
                match LiveStream::start(info, ff, dests, rx) {
                    Ok(stream) => {
                        tracing::info!("streaming {net:?} live");
                        Some(LiveHandle::of(stream))
                    }
                    Err(e) => {
                        tracing::error!("{net:?} is not streamed live: {e}; recording goes on");
                        None
                    }
                }
            }),
        }
    }
}

/// `YouTube live, Twitch retrying`.
fn describe_live(s: &LiveSummary) -> String {
    let dests: Vec<String> = s
        .destinations
        .iter()
        .map(|d| {
            let name = match d.name.as_str() {
                "youtube" => "YouTube",
                "twitch" => "Twitch",
                other => other,
            };
            let state = match d.state {
                DestState::Connecting => "connecting",
                DestState::Live => "live",
                DestState::Retrying => "retrying",
                DestState::Off => "off",
            };
            format!("{name} {state}")
        })
        .collect();
    format!("{}: {}", s.profile, dests.join(", "))
}

/// The daemon's working state.
struct Daemon<'a> {
    cfg: &'a Config,
    live: &'a Live,
    clock: &'a dyn Clock,
    stop: &'a AtomicBool,
    make: &'a dyn Fn(&NetCfg) -> Made,
    free: FreeFn,
    hold: Hold,
    /// Finished sessions waiting for their video.
    queue: RenderQueue,
    last_retain: Instant,
    skipped: u64,
}

impl Daemon<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn after(&self, wait: Duration) -> Timestamp {
        let now = self.clock.now();
        now.checked_add(SignedDuration::try_from(wait).unwrap_or(SignedDuration::ZERO))
            .unwrap_or(now)
    }

    /// Run retention. No session is ever open here, because sessions run on
    /// this same thread, so `busy` is the folders queued for or under a
    /// video render. A failure is only logged: it must not cost the next
    /// net.
    fn retain(&mut self) {
        self.last_retain = self.clock.instant();
        let r = &self.cfg.recorder;
        let policy = RetainPolicy {
            max_bytes: r.max_bytes,
            max_age: Duration::from_secs(u64::from(r.max_age_days) * 86_400),
        };
        match retain(&r.root, &policy, self.clock.now(), &self.queue.busy()) {
            Ok(report) if !report.deleted.is_empty() => {
                tracing::info!(
                    "retention deleted {} session(s), {} bytes kept",
                    report.deleted.len(),
                    report.kept_bytes
                );
            }
            Ok(_) => {}
            Err(e) => tracing::error!("retention failed: {e}"),
        }
    }

    /// Wait until the wall clock reaches `wake`, keeping up hourly retention.
    /// Returns `false` if `stop` was set.
    fn rest(&mut self, wake: Timestamp) -> bool {
        loop {
            if self.stopped() {
                return false;
            }
            if self
                .clock
                .instant()
                .saturating_duration_since(self.last_retain)
                >= RETAIN_EVERY
            {
                self.retain();
            }
            let now = self.clock.now();
            if now >= wake {
                return true;
            }
            let left = wake.duration_since(now).unsigned_abs().min(POLL);
            self.clock.sleep_until(self.clock.instant() + left);
        }
    }

    /// Passes until `stop`. `after` is the `leave_at` of the window just
    /// handled, which is where the next lookup starts. A session ends on its
    /// last tick, up to 20 ms before `leave_at`, so looking up from `now`
    /// would find the same window again.
    fn run(&mut self) {
        let cfg = self.cfg;
        let mut after: Option<Timestamp> = None;
        while !self.stopped() {
            let now = self.clock.now();
            let from = after.map_or(now, |t| t.max(now));
            let Some(window) = current_or_next(cfg, from) else {
                self.hold.show(DaemonState::idle());
                self.rest(self.after(RETRY_WAIT));
                continue;
            };
            let net = &cfg.nets[window.net];
            if window.join_at > now {
                self.hold.show(DaemonState::waiting(net, window.join_at));
                tracing::info!("next: {:?} at {}", net.name, window.join_at);
            }
            if !self.rest(window.join_at) {
                break;
            }
            if self.clock.now() >= window.leave_at {
                tracing::warn!(
                    "missed {:?}: the clock passed the end of its window at {} before it could join",
                    net.name,
                    window.leave_at
                );
            } else {
                self.record_window(net, &window);
            }
            after = Some(window.leave_at);
        }
    }

    /// Record `window`, trying again a minute after each failure for as long
    /// as the window stays open.
    fn record_window(&mut self, net: &NetCfg, window: &Window) {
        let mut recorded = false;
        loop {
            if self.attempt(net, window, &mut recorded) {
                return;
            }
            let wake = self.after(RETRY_WAIT).min(window.leave_at);
            if self.clock.now() < window.leave_at {
                self.hold.show(DaemonState::waiting(net, wake));
            }
            if !self.rest(wake) {
                return;
            }
            if self.clock.now() >= window.leave_at {
                break;
            }
        }
        if !recorded {
            self.skipped += 1;
            tracing::error!(
                "skipped {:?}: nothing could be recorded in its window ({} skipped since start)",
                net.name,
                self.skipped
            );
        }
    }

    /// One try at a session. Returns `true` if the window is over with: it
    /// ran to the end, or a stop ended it.
    fn attempt(&mut self, net: &NetCfg, window: &Window, recorded: &mut bool) -> bool {
        let root = &self.cfg.recorder.root;
        let opened = match open_session(root, net, window, self.clock, self.make, self.free) {
            Ok(opened) => opened,
            Err(e) => {
                tracing::error!("cannot record {:?}: {e}", net.name);
                return false;
            }
        };
        *recorded = true;
        tracing::info!("recording {:?} until {}", net.name, window.leave_at);
        let (dir, result) = run_open_session(
            &mut self.hold,
            self.cfg,
            self.live,
            net,
            opened,
            window.leave_at,
            self.clock,
            self.stop,
        );
        // Queued before retention runs, so retention leaves it alone. A
        // stopped daemon renders nothing: the queue stops with it.
        if net.video
            && !self.stopped()
            && matches!(&result, Ok(meta) if meta.ended != Some(Ended::Error))
        {
            self.queue.push(dir);
        }
        self.retain();
        match result {
            Ok(meta) if meta.ended == Some(Ended::Error) => {
                tracing::error!("the session of {:?} failed", net.name);
                false
            }
            Ok(meta) => {
                tracing::info!("finished {:?}: {} over(s)", net.name, meta.overs);
                true
            }
            Err(e) => {
                tracing::error!("the session of {:?} did not close: {e}", net.name);
                false
            }
        }
    }
}

/// Run the daemon until `stop` is set. `make` builds a fresh adapter for each
/// session.
///
/// # Errors
/// [`DaemonError::AlreadyRunning`] if another recorder holds the lock, or
/// [`DaemonError::Io`] if the lock file cannot be opened or locked. Once it
/// is running, failures are logged and the daemon carries on.
pub fn run(
    cfg: &Config,
    ff: Option<Ffmpeg>,
    live: &Live,
    clock: &dyn Clock,
    stop: &AtomicBool,
    make: &dyn Fn(&NetCfg) -> Made,
) -> Result<(), DaemonError> {
    let queue = RenderQueue::new(
        ff,
        cfg.recorder.root.clone(),
        cfg.recorder.my_callsigns.clone(),
    );
    run_live(
        cfg,
        live,
        clock,
        stop,
        make,
        astar_netarchive::free_bytes,
        queue,
    )
}

/// The `ffmpeg` the videos and live streams use, when any net wants
/// either. Without one the nets are still recorded, and the log says once
/// why there are no videos or streams.
#[must_use]
pub fn find_ffmpeg(cfg: &Config) -> Option<Ffmpeg> {
    if !cfg
        .nets
        .iter()
        .any(|n| n.video || n.youtube_live || n.twitch_live)
    {
        return None;
    }
    match astar_netvideo::find(&cfg.video.ffmpeg) {
        Ok(ff) => {
            tracing::info!(
                "videos and live streams use ffmpeg {} at {}",
                ff.version,
                ff.path.display()
            );
            Some(ff)
        }
        Err(e) => {
            tracing::warn!("no videos or live streams will be made: {e}");
            None
        }
    }
}

/// Remove the `video.mp4.part` a render that is gone left in any session
/// folder. Its session is not queued again: `astar-netrec render` makes it.
fn remove_leftover_parts(root: &Path) {
    let Ok(nets) = fs::read_dir(root) else { return };
    for net in nets.flatten() {
        if !net.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(sessions) = fs::read_dir(net.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            let dir = session.path();
            if !session.file_type().is_ok_and(|t| t.is_dir())
                || fs::symlink_metadata(dir.join(astar_netvideo::VIDEO_PART)).is_err()
            {
                continue;
            }
            match astar_netvideo::remove_leftover_part(&dir) {
                Ok(true) => tracing::info!(
                    "removed an unfinished video from {}; `astar-netrec render` can make it",
                    dir.display()
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    "cannot remove the unfinished video in {}: {e}",
                    dir.display()
                ),
            }
        }
    }
}

/// [`run`] with the free-space reading supplied and nothing streamed live.
#[cfg(test)]
pub(crate) fn run_with(
    cfg: &Config,
    clock: &dyn Clock,
    stop: &AtomicBool,
    make: &dyn Fn(&NetCfg) -> Made,
    free: FreeFn,
    queue: RenderQueue,
) -> Result<(), DaemonError> {
    run_live(cfg, &Live::off(), clock, stop, make, free, queue)
}

/// [`run`] with the free-space reading supplied.
pub(crate) fn run_live(
    cfg: &Config,
    live: &Live,
    clock: &dyn Clock,
    stop: &AtomicBool,
    make: &dyn Fn(&NetCfg) -> Made,
    free: FreeFn,
    queue: RenderQueue,
) -> Result<(), DaemonError> {
    // The claim comes first, and holds until this returns: recovery marks every open session as crashed,
    // and a live recorder's session is open.
    let hold = Hold::claim(&cfg.recorder.root)?;
    match recover_unfinished(&cfg.recorder.root, clock.now()) {
        Ok(fixed) => {
            for dir in fixed {
                tracing::warn!("marked {} as crashed", dir.display());
            }
        }
        Err(e) => tracing::error!("crash recovery failed: {e}"),
    }
    remove_leftover_parts(&cfg.recorder.root);
    let mut daemon = Daemon {
        cfg,
        live,
        clock,
        stop,
        make,
        free,
        hold,
        queue,
        last_retain: clock.instant(),
        skipped: 0,
    };
    daemon.retain();
    daemon.run();
    // A render under way is killed and leaves no part file.
    daemon.queue.stop();
    Ok(())
}

fn find_net<'a>(cfg: &'a Config, name: &str) -> Result<(usize, &'a NetCfg), DaemonError> {
    let wanted = name.to_lowercase();
    cfg.nets
        .iter()
        .enumerate()
        .find(|(_, n)| n.name.to_lowercase() == wanted)
        .ok_or_else(|| DaemonError::UnknownNet {
            name: name.to_string(),
            known: if cfg.nets.is_empty() {
                "(none in this config)".to_string()
            } else {
                cfg.nets
                    .iter()
                    .map(|n| format!("{:?}", n.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        })
}

/// Record the net called `name` (in any case) right now, for `length`. A stop
/// ends it early and that still counts as success.
///
/// It holds the lock like the daemon does. A daemon started meanwhile
/// would otherwise log in beside it, and mark its open session as crashed.
///
/// # Errors
/// [`DaemonError::UnknownNet`], [`DaemonError::AlreadyRunning`], or whatever
/// stopped the recording.
pub fn record_now(
    cfg: &Config,
    live: &Live,
    name: &str,
    length: Duration,
    clock: &dyn Clock,
    stop: &AtomicBool,
    make: &dyn Fn(&NetCfg) -> Made,
) -> Result<SessionMeta, DaemonError> {
    let (index, net) = find_net(cfg, name)?;
    let mut hold = Hold::claim(&cfg.recorder.root)?;
    let now = clock.now();
    let leave_at = now
        .checked_add(SignedDuration::try_from(length).unwrap_or(SignedDuration::ZERO))
        .unwrap_or(now);
    let window = Window {
        net: index,
        start: now,
        end: leave_at,
        join_at: now,
        leave_at,
    };
    let free = astar_netarchive::free_bytes;
    let opened = open_session(&cfg.recorder.root, net, &window, clock, make, free)?;
    let meta = run_open_session(&mut hold, cfg, live, net, opened, leave_at, clock, stop).1?;
    if meta.ended == Some(Ended::Error) {
        return Err(DaemonError::SessionFailed(net.name.clone()));
    }
    Ok(meta)
}

/// Build the real adapter for `net`.
///
/// # Errors
/// A sentence saying why the listener cannot be built.
pub fn station_adapter(cfg: &Config, net: &NetCfg) -> Made {
    let r = &cfg.recorder;
    let adapter = StationAdapter::new(
        net,
        &r.callsign,
        r.callsign_suffix,
        cfg.vocoder.dstar,
        &cfg.vocoder.trusted_serials,
    )
    .map_err(|e| e.to_string())?;
    Ok(Box::new(adapter))
}

/// One readable line about the recorder using the archive at `root`.
#[must_use]
pub fn status_line(root: &Path) -> String {
    if !recorder_running(root) {
        return match last_seen(&root.join(STATE_FILE)) {
            Some(seen) => format!("not running (last seen {seen})"),
            None => "not running".to_string(),
        };
    }
    let Some(state) = read_state(root) else {
        return "running, but its state file is missing".to_string();
    };
    let net = state.net.as_deref().unwrap_or("?");
    match state.state.as_str() {
        "recording" => {
            let since = wall_clock(state.since.as_deref(), "%H:%M");
            let link = if state.linked == Some(true) {
                "linked"
            } else {
                "not linked"
            };
            let live = state
                .live
                .as_ref()
                .map_or(String::new(), |l| format!(" · live {}", describe_live(l)));
            format!("recording {net:?} since {since}, {link}{live}")
        }
        "waiting" => {
            let joins = wall_clock(state.next.as_deref(), "%a %Y-%m-%d %H:%M");
            format!("waiting for {net:?}, joining {joins}")
        }
        _ => "idle, nothing scheduled".to_string(),
    }
}

/// When the state file was last written, in the system's zone.
fn last_seen(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let t = Timestamp::try_from(modified).ok()?;
    Some(
        t.to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
    )
}

/// Format the state file's RFC 3339 text at the wall-clock time it was
/// written with, which is the net's own zone.
fn wall_clock(text: Option<&str>, format: &str) -> String {
    let Some(text) = text else {
        return "?".to_string();
    };
    text.parse::<DateTime>()
        .map_or_else(|_| text.to_string(), |t| t.strftime(format).to_string())
}

/// What `check` prints: the warnings (a live switch with no key among
/// them), then the next windows, one line each, in each net's zone.
///
/// # Errors
/// Whatever [`validate_with_keys`] refuses.
pub fn check_report(
    cfg: &Config,
    now: Timestamp,
    keys: &StreamKeys,
) -> Result<(Vec<String>, Vec<String>), ConfigError> {
    let warnings = validate_with_keys(cfg, now, keys)?;
    let horizon = now
        .checked_add(SignedDuration::from_hours(CHECK_HOURS))
        .unwrap_or(now);
    let mut lines: Vec<String> = windows_between(cfg, now, horizon)
        .iter()
        .take(CHECK_WINDOWS)
        .map(|w| {
            let net = &cfg.nets[w.net];
            let local = |t: Timestamp, format: &str| {
                t.to_zoned(net.tz.clone()).strftime(format).to_string()
            };
            format!(
                "{}  {:?}  joins {}, leaves {}",
                local(w.start, "%a %Y-%m-%d %H:%M %Z"),
                net.name,
                local(w.join_at, "%H:%M"),
                local(w.leave_at, "%H:%M"),
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("nothing scheduled in the next year".to_string());
    }
    Ok((warnings, lines))
}

/// What `check` says about video and live: the `ffmpeg` found (or why
/// none), and which stream keys are set. Only whether a key is there, never
/// any of it.
#[must_use]
pub fn check_media(ff: &Result<Ffmpeg, FfmpegError>, keys: &StreamKeys) -> Vec<String> {
    let ffmpeg = match ff {
        Ok(ff) => format!("ffmpeg: {} {}", ff.path.display(), ff.version),
        Err(e) => format!("ffmpeg: {e}"),
    };
    let set = |k: &Option<_>| if k.is_some() { "set" } else { "missing" };
    vec![
        ffmpeg,
        format!(
            "live: YouTube key {}, Twitch key {}",
            set(&keys.youtube),
            set(&keys.twitch)
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    use astar_netarchive::{Ended, read_meta, write_meta};

    use super::*;
    use crate::config::from_toml_str;
    use crate::render_queue::RenderFn;
    use crate::runner::testing::{FakeAdapter, FakeClock, Plan, Seen};

    const RATE: u32 = 8000;
    /// More sessions than any test here records. A daemon that finds the same
    /// window again makes no fake time pass, so without this limit the bug
    /// would hang the tests instead of failing them.
    const MAX_JOINS: usize = 20;

    const fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    const fn mins(m: u64) -> Duration {
        Duration::from_secs(m * 60)
    }

    fn temp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "netrec-daemon-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A config whose nets are `(name, when, duration)`, in UTC.
    fn config(root: &Path, pre: &str, post: &str, nets: &[(&str, &str, &str)]) -> Config {
        let mut text = format!(
            "[recorder]\nroot = \"{}\"\ncallsign = \"W1AW\"\npre_roll = \"{pre}\"\npost_roll = \"{post}\"\nmax_age_days = 30\n",
            root.display()
        );
        for (name, when, duration) in nets {
            write!(
                text,
                "[[net]]\nname = \"{name}\"\nnetwork = \"m17\"\nhost = \"127.0.0.1\"\nport = 17000\nmodule = \"C\"\nwhen = \"{when}\"\nduration = \"{duration}\"\ntz = \"UTC\"\n"
            )
            .unwrap();
        }
        from_toml_str(&text).unwrap()
    }

    /// The daemon as `run` starts it, but with no videos: the tests here
    /// must not render with whatever `ffmpeg` this machine has.
    fn run(
        cfg: &Config,
        clock: &dyn Clock,
        stop: &AtomicBool,
        make: &dyn Fn(&NetCfg) -> Made,
    ) -> Result<(), DaemonError> {
        let queue = no_videos(&cfg.recorder.root);
        run_with(cfg, clock, stop, make, astar_netarchive::free_bytes, queue)
    }

    fn no_videos(root: &Path) -> RenderQueue {
        RenderQueue::new(None, root.to_path_buf(), Vec::new())
    }

    /// A queue whose renders record the folder they were given, after
    /// waiting for `release` (or a stop).
    fn stub_queue(
        root: &Path,
        seen: &Arc<Mutex<Vec<PathBuf>>>,
        release: &Arc<AtomicBool>,
    ) -> RenderQueue {
        let seen = Arc::clone(seen);
        let release = Arc::clone(release);
        let render: RenderFn = Arc::new(move |dir, stop| {
            while !release.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            seen.lock().unwrap().push(dir.to_path_buf());
            Ok(String::new())
        });
        RenderQueue::with(
            Some(render),
            root.to_path_buf(),
            astar_netarchive::free_bytes,
        )
    }

    /// Waits up to 10 s of real time for `done`.
    fn real_wait(done: impl Fn() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until && !done() {
            thread::sleep(Duration::from_millis(10));
        }
        done()
    }

    fn at(hms: &str) -> jiff::Timestamp {
        format!("2026-11-03T{hms}Z").parse().unwrap()
    }

    /// A fake clock that runs a hook after every sleep.
    struct Script<'a> {
        clock: FakeClock,
        hook: Box<dyn Fn(&FakeClock) + 'a>,
    }

    impl<'a> Script<'a> {
        fn new(clock: &FakeClock, hook: impl Fn(&FakeClock) + 'a) -> Self {
            Self {
                clock: clock.clone(),
                hook: Box::new(hook),
            }
        }

        /// Sets `stop` once `after` of fake time has passed.
        fn stopping(clock: &FakeClock, after: Duration, stop: &'a AtomicBool) -> Self {
            Self::new(clock, move |c| {
                if c.elapsed() >= after {
                    stop.store(true, Ordering::SeqCst);
                }
            })
        }
    }

    impl Clock for Script<'_> {
        fn now(&self) -> jiff::Timestamp {
            self.clock.now()
        }

        fn instant(&self) -> std::time::Instant {
            self.clock.instant()
        }

        fn sleep_until(&self, i: std::time::Instant) {
            self.clock.sleep_until(i);
            (self.hook)(&self.clock);
        }

        fn settle(&self, done: &dyn Fn() -> bool) {
            self.clock.settle(done);
        }
    }

    /// A factory of adapters that link at once. Every adapter shares `seen`.
    fn linking<'a>(
        clock: &'a FakeClock,
        seen: &'a Arc<Mutex<Seen>>,
    ) -> impl Fn(&NetCfg) -> Made + 'a {
        move |_| {
            let joins = seen.lock().unwrap().joins.len();
            assert!(joins < MAX_JOINS, "re-recording a window: {joins} joins");
            let mut a = FakeAdapter::new(clock, RATE, vec![Plan::up(0)]);
            a.seen = Arc::clone(seen);
            Ok(Box::new(a))
        }
    }

    /// Every session folder under `root`, sorted.
    fn sessions(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for net in fs::read_dir(root).unwrap() {
            let net = net.unwrap().path();
            if !net.is_dir() {
                continue;
            }
            for s in fs::read_dir(&net).unwrap() {
                out.push(s.unwrap().path());
            }
        }
        out.sort();
        out
    }

    /// Seconds of audio in a session's `session.flac`.
    fn seconds(dir: &Path) -> u64 {
        let mut r = claxon::FlacReader::open(dir.join("session.flac")).unwrap();
        r.samples().count() as u64 / u64::from(RATE)
    }

    /// Plant a session in the archive: finished long ago, or not finished.
    fn plant(root: &Path, name: &str, ended: Option<&str>) -> PathBuf {
        let dir = root.join("Old net").join(name);
        fs::create_dir_all(&dir).unwrap();
        let meta = SessionMeta {
            format: astar_netarchive::META_FORMAT,
            net: "Old net".into(),
            network: "m17".into(),
            target: "h:1 C".into(),
            window_start: "2020-01-01T00:00:00Z".into(),
            window_end: "2020-01-01T01:00:00Z".into(),
            visibility: "private".into(),
            my_side: false,
            sample_rate: RATE,
            started_at: "2020-01-01T00:00:00Z".into(),
            ended_at: ended.map(str::to_owned),
            ended: ended.map(|_| Ended::Window),
            rejoins: 0,
            dropped_frames: 0,
            overs: 0,
            astar_version: "0".into(),
            my_callsigns: Vec::new(),
        };
        write_meta(&dir, &meta).unwrap();
        dir
    }

    fn write_state(root: &Path, state: &str, pid: u32) {
        let s = DaemonState {
            state: state.into(),
            net: Some("Tuesday M17 net".into()),
            since: Some("2026-11-03T19:55:00-07:00".into()),
            next: None,
            linked: Some(true),
            pid,
            live: None,
        };
        fs::write(root.join(STATE_FILE), serde_json::to_vec(&s).unwrap()).unwrap();
    }

    /// Take the archive's lock, as a running recorder does.
    fn take_lock(root: &Path) -> fs::File {
        let file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(LOCK_FILE))
            .unwrap();
        file.try_lock().unwrap();
        file
    }

    /// The pid of a process that has exited.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// A process other than this one that is certainly alive: whoever ran it.
    fn live_pid() -> u32 {
        std::os::unix::process::parent_id()
    }

    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Self;

        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    /// Run `f` and return what it logged at warn level and above.
    fn logged(f: impl FnOnce()) -> String {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn waits_then_records_next_window() {
        let root = temp("next");
        let cfg = config(&root, "30s", "30s", &[("Noon net", "daily 12:00", "2m")]);
        let fake = FakeClock::new(at("11:50:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let clock = Script::stopping(&fake, mins(20), &stop);
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();

        let found = sessions(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(seconds(&found[0]), 180);
        assert_eq!(read_meta(&found[0]).unwrap().ended, Some(Ended::Window));
        assert_eq!(seen.lock().unwrap().joins, vec![secs(9 * 60 + 30)]);
        assert!(!root.join(STATE_FILE).exists(), "removed on a clean exit");
    }

    #[test]
    fn starting_mid_window_records_the_remainder() {
        let root = temp("mid");
        let cfg = config(&root, "1m", "1m", &[("Noon net", "daily 12:00", "60m")]);
        let fake = FakeClock::new(at("12:30:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let clock = Script::stopping(&fake, mins(40), &stop);
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();

        let found = sessions(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(seconds(&found[0]), 31 * 60, "leave_at - now");
    }

    #[test]
    fn two_windows_in_a_row() {
        let root = temp("two");
        let nets = [
            ("First", "daily 12:00", "2m"),
            ("Second", "daily 12:02", "2m"),
        ];
        let cfg = config(&root, "0s", "0s", &nets);
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let clock = Script::stopping(&fake, mins(30), &stop);
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();

        let found = sessions(&root);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(read_meta(&found[0]).unwrap().net, "First");
        assert_eq!(read_meta(&found[1]).unwrap().net, "Second");
        assert_eq!(seconds(&found[0]), 120);
        assert_eq!(seconds(&found[1]), 120);
    }

    #[test]
    fn a_finished_window_is_not_recorded_twice() {
        let root = temp("twice");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let clock = Script::stopping(&fake, mins(10), &stop);
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();

        assert_eq!(sessions(&root).len(), 1);
        assert_eq!(seen.lock().unwrap().joins.len(), 1);
    }

    #[test]
    fn stop_while_waiting_exits_quickly() {
        let root = temp("stop");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let fake = FakeClock::new(at("06:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let clock = Script::stopping(&fake, mins(1), &stop);
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();

        assert!(fake.elapsed() <= mins(1) + secs(1), "{:?}", fake.elapsed());
        assert_eq!(sessions(&root).len(), 0);
        assert!(!root.join(STATE_FILE).exists());
    }

    #[test]
    fn stop_during_a_session_closes_it_as_stopped() {
        let root = temp("stopped");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "10m")]);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let clock = Script::stopping(&fake, mins(1), &stop);
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();

        let found = sessions(&root);
        assert_eq!(found.len(), 1, "no second attempt after a stop");
        assert_eq!(read_meta(&found[0]).unwrap().ended, Some(Ended::Stopped));
        assert_eq!(seconds(&found[0]), 60);
        assert!(!root.join(STATE_FILE).exists());
    }

    #[test]
    fn the_state_file_follows_the_daemon() {
        let root = temp("state");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "2m")]);
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let waiting: Mutex<Option<String>> = Mutex::default();
        let recording: Mutex<Option<String>> = Mutex::default();
        let clock = Script::new(&fake, |c| {
            if c.elapsed() >= secs(30) && waiting.lock().unwrap().is_none() {
                *waiting.lock().unwrap() = Some(status_line(&root));
            }
            if c.elapsed() >= secs(90) && recording.lock().unwrap().is_none() {
                *recording.lock().unwrap() = Some(status_line(&root));
            }
            if c.elapsed() >= mins(5) {
                stop.store(true, Ordering::SeqCst);
            }
        });
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();

        let waiting = waiting.lock().unwrap().clone().unwrap();
        assert_eq!(
            waiting,
            "waiting for \"Noon net\", joining Tue 2026-11-03 12:00"
        );
        let recording = recording.lock().unwrap().clone().unwrap();
        assert_eq!(recording, "recording \"Noon net\" since 12:00, linked");
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().starts_with(STATE_FILE))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    // ---- live ----

    const KEY: &str = "N0CALL-TEST-STREAM-KEY-9X";

    fn keys(youtube: bool, twitch: bool) -> StreamKeys {
        StreamKeys {
            youtube: youtube.then(|| astar_secrets::Secret::new(KEY.to_owned())),
            twitch: twitch.then(|| astar_secrets::Secret::new(format!("{KEY}-TW"))),
        }
    }

    /// A public noon net streaming to `YouTube` (and Twitch when `twitch`).
    fn live_config(root: &Path, twitch: bool) -> Config {
        let mut cfg = config(root, "0s", "0s", &[("Noon net", "daily 12:00", "2m")]);
        cfg.nets[0].visibility = Visibility::Public;
        cfg.nets[0].youtube_live = true;
        cfg.nets[0].twitch_live = twitch;
        cfg.nets[0].video = false;
        cfg
    }

    fn summary(state: DestState) -> LiveSummary {
        LiveSummary {
            profile: "1080p30".into(),
            destinations: vec![astar_netvideo::DestStatus {
                name: "youtube".into(),
                state,
                reconnects: 0,
                last_error: None,
            }],
        }
    }

    /// What a stub stream saw.
    #[derive(Default)]
    struct Stub {
        starts: AtomicU32,
        dests: Mutex<Vec<String>>,
        /// Tap messages the session dropped, read when the stream stopped.
        dropped: Mutex<Option<u64>>,
        stops: AtomicU32,
    }

    /// A live starter that never reads its tap and reports `state`.
    fn stub_live(keys: StreamKeys, stub: &Arc<Stub>, state: DestState) -> Live {
        let stub = Arc::clone(stub);
        Live {
            keys,
            start: Box::new(move |_info, dests, rx| {
                stub.starts.fetch_add(1, Ordering::SeqCst);
                *stub.dests.lock().unwrap() = dests.iter().map(|d| d.name.to_string()).collect();
                let slot = Arc::new(Mutex::new(summary(state)));
                let stub = Arc::clone(&stub);
                let last = summary(state);
                Some(LiveHandle {
                    summary: slot,
                    stop: Box::new(move || {
                        *stub.dropped.lock().unwrap() = Some(rx.dropped());
                        stub.stops.fetch_add(1, Ordering::SeqCst);
                        last
                    }),
                })
            }),
        }
    }

    fn run_noon(cfg: &Config, live: &Live, hook: impl Fn(&FakeClock)) {
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let clock = Script::new(&fake, |c| {
            hook(c);
            if c.elapsed() >= mins(4) {
                stop.store(true, Ordering::SeqCst);
            }
        });
        let root = cfg.recorder.root.clone();
        run_live(
            cfg,
            live,
            &clock,
            &stop,
            &linking(&fake, &seen),
            astar_netarchive::free_bytes,
            no_videos(&root),
        )
        .unwrap();
    }

    #[test]
    fn private_net_never_goes_live() {
        let root = temp("private-live");
        let mut cfg = live_config(&root, true);
        // Switched on, but private: `validate` refuses this, and it is
        // bypassed here on purpose.
        cfg.nets[0].visibility = Visibility::Private;
        let log = logged(|| {
            assert!(live_destinations(&cfg, &cfg.nets[0], &keys(true, true)).is_empty());
        });
        assert!(log.contains("private"), "{log}");
        let stub = Arc::new(Stub::default());
        run_noon(
            &cfg,
            &stub_live(keys(true, true), &stub, DestState::Live),
            |_| {},
        );
        assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
        assert_eq!(sessions(&root).len(), 1, "recorded all the same");
    }

    #[test]
    fn live_off_starts_nothing() {
        let root = temp("live-off");
        let mut cfg = live_config(&root, false);
        cfg.nets[0].youtube_live = false;
        assert!(live_destinations(&cfg, &cfg.nets[0], &keys(true, true)).is_empty());
        let stub = Arc::new(Stub::default());
        run_noon(
            &cfg,
            &stub_live(keys(true, true), &stub, DestState::Live),
            |_| {},
        );
        assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
        assert_eq!(sessions(&root).len(), 1);
    }

    #[test]
    fn switched_on_without_a_key_skips_that_destination() {
        let root = temp("live-nokey");
        let cfg = live_config(&root, true);
        let mut dests = Vec::new();
        let log = logged(|| dests = live_destinations(&cfg, &cfg.nets[0], &keys(true, false)));
        assert_eq!(dests.len(), 1);
        assert_eq!(dests[0].name, DestName::YouTube);
        assert_eq!(dests[0].ingest.host, "a.rtmps.youtube.com");
        let errors: Vec<&str> = log.lines().filter(|l| l.contains("ERROR")).collect();
        assert_eq!(errors.len(), 1, "{log}");
        assert!(errors[0].contains("Twitch"), "{log}");
        assert!(errors[0].contains("twitch_stream_key"), "{log}");
        assert!(errors[0].contains(CREDENTIALS_FILE), "{log}");
        assert!(errors[0].contains(TWITCH_KEY_ENV), "{log}");
        assert!(!log.contains(KEY), "{log}");
        // No key at all: nothing to stream, and the daemon starts nothing.
        let stub = Arc::new(Stub::default());
        run_noon(
            &cfg,
            &stub_live(keys(false, false), &stub, DestState::Live),
            |_| {},
        );
        assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    }

    /// Runs `f` on a thread of its own and fails if it is not done within
    /// `limit`, so a regression that blocks fails the test rather than
    /// hanging the run.
    fn within(limit: Duration, f: impl FnOnce() + Send + 'static) {
        let (done, finished) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            f();
            let _ = done.send(());
        });
        match finished.recv_timeout(limit) {
            Ok(()) => worker.join().expect("worker"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("not done within {limit:?}: something blocked")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::panic::resume_unwind(worker.join().expect_err("it panicked"))
            }
        }
    }

    #[test]
    fn stalled_live_never_delays_a_tick() {
        // The session runs on a fake clock, so it takes a second or two;
        // a tick that blocked on the stalled stream would never finish.
        within(Duration::from_secs(60), || {
            let root = temp("live-stall");
            let cfg = live_config(&root, false);
            let stub = Arc::new(Stub::default());
            run_noon(
                &cfg,
                &stub_live(keys(true, false), &stub, DestState::Live),
                |_| {},
            );
            assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
            assert_eq!(
                stub.stops.load(Ordering::SeqCst),
                1,
                "stopped after the session"
            );
            assert_eq!(*stub.dests.lock().unwrap(), ["YouTube"]);
            let found = sessions(&root);
            assert_eq!(found.len(), 1);
            // Every tick of the two-minute window is in the recording, though
            // nothing ever read the tap after its first thousand messages.
            assert_eq!(seconds(&found[0]), 120);
            let meta = read_meta(&found[0]).unwrap();
            assert_eq!(meta.ended, Some(Ended::Window));
            let dropped = stub
                .dropped
                .lock()
                .unwrap()
                .expect("the stream was stopped");
            assert!(dropped >= 5_000, "{dropped} dropped");
        });
    }

    /// An adapter that is always linked and never joins.
    struct AlwaysLinked;

    impl NetAdapter for AlwaysLinked {
        fn join(&mut self) -> Result<Box<dyn AudioFeed>, String> {
            Err("not in this test".into())
        }
        fn status(&self) -> LinkStatus {
            LinkStatus::Linked
        }
        fn talker(&self) -> Option<String> {
            None
        }
        fn leave(&mut self) {}
        fn rate(&self) -> u32 {
            8_000
        }
        fn dropped_frames(&self) -> u64 {
            0
        }
    }

    #[test]
    fn the_state_file_is_written_on_a_state_change_not_on_every_retry() {
        let root = temp("live-churn");
        let cfg = live_config(&root, false);
        let slot = Arc::new(Mutex::new(summary(DestState::Retrying)));
        let w = Watched {
            inner: Box::new(AlwaysLinked),
            root: root.clone(),
            shown: RefCell::new(DaemonState::recording(&cfg.nets[0], at("12:00:00"))),
            live: Some(Arc::clone(&slot)),
        };
        let state = root.join(STATE_FILE);
        w.status();
        assert!(state.exists(), "the first tick writes the state file");
        fs::remove_file(&state).unwrap();
        // Retrying again, with a new count and a new error: no write.
        for n in 1..=5 {
            let mut s = slot.lock().unwrap();
            s.destinations[0].reconnects = n;
            s.destinations[0].last_error = Some(format!("refused, try {n}"));
            drop(s);
            w.status();
        }
        assert!(!state.exists(), "rewritten for a retry count");
        // Live: a state change, so it is written, with the latest counts.
        slot.lock().unwrap().destinations[0].state = DestState::Live;
        w.status();
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(json["live"]["destinations"][0]["state"], "live");
        assert_eq!(json["live"]["destinations"][0]["reconnects"], 5);
    }

    #[test]
    fn state_file_shows_live() {
        let root = temp("live-state");
        let cfg = live_config(&root, false);
        let stub = Arc::new(Stub::default());
        let text: Mutex<Option<String>> = Mutex::default();
        let line: Mutex<Option<String>> = Mutex::default();
        run_noon(
            &cfg,
            &stub_live(keys(true, false), &stub, DestState::Live),
            |c| {
                if c.elapsed() >= secs(90) && text.lock().unwrap().is_none() {
                    *text.lock().unwrap() = fs::read_to_string(root.join(STATE_FILE)).ok();
                    *line.lock().unwrap() = Some(status_line(&root));
                }
            },
        );
        let text = text
            .lock()
            .unwrap()
            .clone()
            .expect("a state file mid-session");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["state"], "recording");
        assert_eq!(json["live"]["profile"], "1080p30");
        assert_eq!(json["live"]["destinations"][0]["state"], "live");
        assert_eq!(json["live"]["destinations"][0]["name"], "youtube");
        assert!(!text.contains(KEY), "{text}");
        let line = line.lock().unwrap().clone().unwrap();
        assert_eq!(
            line,
            "recording \"Noon net\" since 12:00, linked · live 1080p30: YouTube live"
        );
    }

    #[test]
    fn state_file_shows_live_while_the_link_is_still_down() {
        let root = temp("live-unlinked");
        let cfg = live_config(&root, false);
        let stub = Arc::new(Stub::default());
        let live = stub_live(keys(true, false), &stub, DestState::Connecting);
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let text: Mutex<Option<String>> = Mutex::default();
        let clock = Script::new(&fake, |c| {
            if c.elapsed() >= secs(65) && text.lock().unwrap().is_none() {
                *text.lock().unwrap() = fs::read_to_string(root.join(STATE_FILE)).ok();
                stop.store(true, Ordering::SeqCst);
            }
        });
        // A link that never comes up: `linked` never changes, so only the
        // live summary can make the first write.
        let make = |_: &NetCfg| -> Made {
            let mut a = FakeAdapter::new(
                &fake,
                RATE,
                vec![Plan::Link {
                    script: vec![(Duration::ZERO, LinkStatus::Linking)],
                    voice: 0,
                }],
            );
            a.seen = Arc::clone(&seen);
            Ok(Box::new(a))
        };
        let free = astar_netarchive::free_bytes;
        run_live(&cfg, &live, &clock, &stop, &make, free, no_videos(&root)).unwrap();
        let text = text
            .lock()
            .unwrap()
            .clone()
            .expect("a state file mid-session");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["linked"], false, "{text}");
        assert_eq!(
            json["live"]["destinations"][0]["state"], "connecting",
            "{text}"
        );
    }

    #[test]
    fn status_line_reports_live() {
        let root = temp("live-status");
        let _held = take_lock(&root);
        let mut live = summary(DestState::Live);
        live.destinations.push(astar_netvideo::DestStatus {
            name: "twitch".into(),
            state: DestState::Retrying,
            reconnects: 2,
            last_error: Some("timed out waiting for the server (127.0.0.1:1935)".into()),
        });
        let s = DaemonState {
            live: Some(live),
            ..DaemonState::recording(
                &config(&root, "0s", "0s", &[("Tuesday M17 net", "Tue 19:00", "1h")]).nets[0],
                at("19:55:00"),
            )
        };
        fs::write(root.join(STATE_FILE), serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(
            status_line(&root),
            "recording \"Tuesday M17 net\" since 19:55, not linked · live 1080p30: YouTube live, Twitch retrying"
        );
        // A state file from before live streams existed still reads.
        write_state(&root, "recording", std::process::id());
        assert_eq!(
            status_line(&root),
            "recording \"Tuesday M17 net\" since 19:55, linked"
        );
    }

    #[test]
    fn check_reports_key_presence_only() {
        let root = temp("live-check");
        let cfg = live_config(&root, true);
        let k = keys(true, false);
        let missing = Err(FfmpegError::Missing(PathBuf::from("ffmpeg")));
        let lines = check_media(&missing, &k);
        assert_eq!(lines[1], "live: YouTube key set, Twitch key missing");
        assert!(
            lines[0].starts_with("ffmpeg: ffmpeg was not found"),
            "{lines:?}"
        );
        let found = Ok(Ffmpeg {
            path: PathBuf::from("/opt/homebrew/bin/ffmpeg"),
            version: "7.1.1".into(),
        });
        assert_eq!(
            check_media(&found, &keys(false, true))[..],
            [
                "ffmpeg: /opt/homebrew/bin/ffmpeg 7.1.1",
                "live: YouTube key missing, Twitch key set"
            ]
        );
        let (warnings, _) = check_report(&cfg, at("00:00:00"), &k).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("twitch_live is on but no Twitch stream key")),
            "{warnings:?}"
        );
        for text in lines.iter().chain(&warnings) {
            assert!(!text.contains(KEY), "{text}");
        }
    }

    #[test]
    fn status_reads_state_file() {
        let root = temp("status");
        let _held = take_lock(&root);
        write_state(&root, "recording", std::process::id());
        assert_eq!(
            status_line(&root),
            "recording \"Tuesday M17 net\" since 19:55, linked"
        );
    }

    #[test]
    fn status_without_a_state_file_is_not_running() {
        assert_eq!(status_line(&temp("nofile")), "not running");
    }

    #[test]
    fn status_with_a_dead_pid_says_when_it_was_last_seen() {
        let root = temp("dead");
        write_state(&root, "recording", dead_pid());
        let line = status_line(&root);
        assert!(line.starts_with("not running (last seen 20"), "{line}");
        assert!(line.ends_with(')'), "{line}");
    }

    /// A power cut leaves the state file behind, and after the reboot its pid
    /// may belong to some other service. Only the lock says who is running.
    #[test]
    fn status_with_stale_state_says_not_running() {
        let root = temp("stalestatus");
        write_state(&root, "recording", live_pid());
        let line = status_line(&root);
        assert!(line.starts_with("not running (last seen 20"), "{line}");
    }

    #[test]
    fn status_with_a_held_lock_and_no_state_file_says_running() {
        let root = temp("nostate");
        let _held = take_lock(&root);
        assert_eq!(status_line(&root), "running, but its state file is missing");
    }

    #[test]
    fn status_does_not_create_the_lock_file() {
        let root = temp("nolock");
        let _ = status_line(&root);
        assert!(!root.join(LOCK_FILE).exists());
    }

    #[test]
    fn a_held_lock_refuses_a_start_even_with_no_state_file() {
        let root = temp("lockonly");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let open = plant(&root, "open", None);
        let _held = take_lock(&root);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(true);
        let seen = Arc::default();

        let err = run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap_err();

        assert!(matches!(err, DaemonError::AlreadyRunning { .. }), "{err}");
        assert!(err.to_string().contains("another recorder"), "{err}");
        assert_eq!(read_meta(&open).unwrap().ended, None);
    }

    /// `status` takes the lock for a moment to find out whether anyone has
    /// it. That must not read as a second recorder to a daemon starting then.
    #[test]
    fn a_status_probe_does_not_refuse_a_start() {
        let root = temp("probe");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let probe = take_lock(&root);
        probe.unlock().unwrap();
        probe.try_lock_shared().unwrap();
        let fake = FakeClock::new(at("11:00:00"));
        let stop = AtomicBool::new(true);
        let seen = Arc::default();

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            drop(probe);
        });
        run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap();
        release.join().unwrap();
    }

    #[test]
    fn the_lock_file_outlives_the_daemon_and_retention() {
        let root = temp("keeplock");
        let mut cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        cfg.recorder.max_bytes = 0;
        let old = plant(&root, "old", Some("2020-01-01T01:00:00Z"));
        let fake = FakeClock::new(at("11:00:00"));
        let stop = AtomicBool::new(true);
        let seen = Arc::default();
        run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap();

        assert!(!old.exists(), "retention ran");
        assert!(root.join(LOCK_FILE).exists());
        drop(take_lock(&root));
    }

    /// A truly full disk cannot be arranged here, so a folder sits where the
    /// temporary state file goes, which fails the write the same way.
    #[test]
    fn a_state_file_that_will_not_write_does_not_stop_the_daemon() {
        let root = temp("fullstate");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let old = plant(&root, "old", Some("2020-01-01T01:00:00Z"));
        write_state(&root, "recording", live_pid());
        fs::create_dir(root.join(format!("{STATE_FILE}.{}.tmp", std::process::id()))).unwrap();
        let fake = FakeClock::new(at("11:00:00"));
        let stop = AtomicBool::new(true);
        let seen = Arc::default();

        let logs = logged(|| run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap());

        assert!(!old.exists(), "retention is what could free the disk");
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("state file"), "{logs}");
    }

    /// With no new state written, the dead recorder's must not stand in for it.
    #[test]
    fn a_state_file_that_will_not_write_does_not_leave_the_old_one_showing() {
        let root = temp("oldstate");
        write_state(&root, "recording", live_pid());
        fs::create_dir(root.join(format!("{STATE_FILE}.{}.tmp", std::process::id()))).unwrap();

        let _hold = Hold::claim(&root).unwrap();

        assert_eq!(status_line(&root), "running, but its state file is missing");
    }

    #[test]
    fn second_daemon_refuses_to_start() {
        let root = temp("second");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let open = plant(&root, "open", None);
        let _held = take_lock(&root);
        write_state(&root, "recording", live_pid());
        let before = fs::read(root.join(STATE_FILE)).unwrap();
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();

        let err = run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap_err();

        assert!(matches!(err, DaemonError::AlreadyRunning { .. }), "{err}");
        assert!(err.to_string().contains(&live_pid().to_string()), "{err}");
        assert_eq!(
            read_meta(&open).unwrap().ended,
            None,
            "the running daemon's open session is not marked crashed"
        );
        assert_eq!(fs::read(root.join(STATE_FILE)).unwrap(), before);
    }

    #[test]
    fn a_stale_state_file_does_not_stop_a_start() {
        let root = temp("stale");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        write_state(&root, "recording", dead_pid());
        let fake = FakeClock::new(at("11:00:00"));
        let stop = AtomicBool::new(true);
        let seen = Arc::default();
        run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap();
        assert!(!root.join(STATE_FILE).exists());
    }

    #[test]
    fn start_up_recovers_then_retains() {
        let root = temp("startup");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let old = plant(&root, "old", Some("2020-01-01T01:00:00Z"));
        let open = plant(&root, "open", None);
        let fake = FakeClock::new(at("11:00:00"));
        let stop = AtomicBool::new(true);
        let seen = Arc::default();
        run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap();

        assert!(!old.exists(), "past max_age_days");
        // Marked crashed at `now`, so it is new by age and stays.
        assert_eq!(read_meta(&open).unwrap().ended, Some(Ended::Crashed));
    }

    #[test]
    fn retention_runs_after_each_session() {
        let root = temp("after");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "2m")]);
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let old = root.join("Old net").join("old");
        let still_there = AtomicBool::new(false);
        let clock = Script::new(&fake, |c| {
            // During the session: planted after start-up, before the end.
            if c.elapsed() >= secs(90) && !still_there.swap(true, Ordering::SeqCst) {
                plant(&root, "old", Some("2020-01-01T01:00:00Z"));
            }
            if c.elapsed() >= mins(4) {
                stop.store(true, Ordering::SeqCst);
            }
        });
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();
        assert!(still_there.load(Ordering::SeqCst));
        assert!(!old.exists(), "retained when the session closed");
    }

    #[test]
    fn retention_runs_hourly_while_idle() {
        let root = temp("hourly");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let fake = FakeClock::new(at("01:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let old = root.join("Old net").join("old");
        let planted = AtomicBool::new(false);
        let at_30_min: Mutex<Option<bool>> = Mutex::default();
        let clock = Script::new(&fake, |c| {
            if c.elapsed() >= mins(10) && !planted.swap(true, Ordering::SeqCst) {
                plant(&root, "old", Some("2020-01-01T01:00:00Z"));
            }
            if c.elapsed() >= mins(30) && at_30_min.lock().unwrap().is_none() {
                *at_30_min.lock().unwrap() = Some(old.exists());
            }
            if c.elapsed() >= mins(61) {
                stop.store(true, Ordering::SeqCst);
            }
        });
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();
        assert_eq!(*at_30_min.lock().unwrap(), Some(true), "not yet an hour");
        assert!(!old.exists(), "gone by the hour");
    }

    #[test]
    fn a_finished_session_is_queued_when_video_is_on() {
        let root = temp("video");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "2m")]);
        assert!(cfg.nets[0].video, "on unless switched off");
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let rendered: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let release = Arc::new(AtomicBool::new(true));
        let queue = stub_queue(&root, &rendered, &release);
        let clock = Script::new(&fake, |c| {
            if c.elapsed() >= mins(4) {
                // Real time: the worker is a real thread.
                real_wait(|| !rendered.lock().unwrap().is_empty());
                stop.store(true, Ordering::SeqCst);
            }
        });
        let free = astar_netarchive::free_bytes;
        run_with(&cfg, &clock, &stop, &linking(&fake, &seen), free, queue).unwrap();
        let found = sessions(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(*rendered.lock().unwrap(), found);
    }

    #[test]
    fn video_off_queues_nothing() {
        let root = temp("novideo");
        let mut cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "2m")]);
        cfg.nets[0].video = false;
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let rendered: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let release = Arc::new(AtomicBool::new(true));
        let queue = stub_queue(&root, &rendered, &release);
        let clock = Script::new(&fake, |c| {
            if c.elapsed() >= mins(4) {
                // Long enough for a queued render to have run.
                thread::sleep(Duration::from_millis(300));
                stop.store(true, Ordering::SeqCst);
            }
        });
        let free = astar_netarchive::free_bytes;
        run_with(&cfg, &clock, &stop, &linking(&fake, &seen), free, queue).unwrap();
        assert_eq!(sessions(&root).len(), 1);
        assert!(rendered.lock().unwrap().is_empty());
    }

    #[test]
    fn retention_skips_a_session_being_rendered() {
        let root = temp("renderbusy");
        let mut cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "2m")]);
        cfg.recorder.max_bytes = 0;
        let fake = FakeClock::new(at("11:59:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let rendered: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        // Never released: the render is under way until the stop.
        let release = Arc::new(AtomicBool::new(false));
        let queue = stub_queue(&root, &rendered, &release);
        let clock = Script::stopping(&fake, mins(4), &stop);
        let free = astar_netarchive::free_bytes;
        run_with(&cfg, &clock, &stop, &linking(&fake, &seen), free, queue).unwrap();
        let found = sessions(&root);
        assert_eq!(found.len(), 1, "deleted while it was being rendered");
        assert_eq!(read_meta(&found[0]).unwrap().ended, Some(Ended::Window));
    }

    #[test]
    fn leftover_part_file_is_removed_at_start() {
        let root = temp("leftover");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let dir = plant(&root, "old", Some("2026-11-03T01:00:00Z"));
        fs::write(dir.join(astar_netvideo::VIDEO_PART), b"half a video").unwrap();
        fs::write(dir.join(astar_netvideo::VIDEO_FILE), b"an old video").unwrap();
        let fake = FakeClock::new(at("11:00:00"));
        let stop = AtomicBool::new(true);
        let seen = Arc::default();
        run(&cfg, &fake, &stop, &linking(&fake, &seen)).unwrap();
        assert!(!dir.join(astar_netvideo::VIDEO_PART).exists());
        // Not re-queued, and the finished video is left alone.
        assert_eq!(
            fs::read(dir.join(astar_netvideo::VIDEO_FILE)).unwrap(),
            b"an old video"
        );
    }

    #[test]
    fn low_disk_retries_each_minute_then_skips_the_window() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        #[allow(clippy::unnecessary_wraps)] // the shape of a free-space reading
        fn full(_: &Path) -> io::Result<u64> {
            let n = CALLS.fetch_add(1, Ordering::SeqCst);
            assert!(n < 50, "spinning on a full disk");
            Ok(0)
        }
        let root = temp("lowdisk");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "5m")]);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let clock = Script::stopping(&fake, mins(10), &stop);
        let logs = logged(|| {
            run_with(
                &cfg,
                &clock,
                &stop,
                &linking(&fake, &seen),
                full,
                no_videos(&root),
            )
            .unwrap();
        });

        assert_eq!(CALLS.load(Ordering::SeqCst), 5, "one try a minute");
        assert_eq!(sessions(&root).len(), 0);
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("disk nearly full"), "{logs}");
        assert_eq!(logs.matches("skipped \"Noon net\"").count(), 1, "{logs}");
    }

    #[test]
    fn space_freed_mid_window_records_the_rest() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        #[allow(clippy::unnecessary_wraps)] // the shape of a free-space reading
        fn clears(_: &Path) -> io::Result<u64> {
            let n = CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(if n < 2 { 0 } else { u64::MAX })
        }
        let root = temp("freed");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "5m")]);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let clock = Script::stopping(&fake, mins(10), &stop);
        run_with(
            &cfg,
            &clock,
            &stop,
            &linking(&fake, &seen),
            clears,
            no_videos(&root),
        )
        .unwrap();

        let found = sessions(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(seconds(&found[0]), 180);
    }

    #[test]
    fn adapter_failure_waits_a_minute_like_a_full_disk() {
        let root = temp("adapter");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "5m")]);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let tries: Mutex<Vec<Duration>> = Mutex::default();
        let make = |_: &NetCfg| -> Made {
            let mut tries = tries.lock().unwrap();
            assert!(tries.len() < 50, "spinning on a bad adapter");
            tries.push(fake.elapsed());
            Err("no such callsign".to_string())
        };
        let clock = Script::stopping(&fake, mins(10), &stop);
        let logs = logged(|| run(&cfg, &clock, &stop, &make).unwrap());

        let want: Vec<_> = (0..5).map(mins).collect();
        assert_eq!(*tries.lock().unwrap(), want);
        assert!(logs.contains("no such callsign"), "{logs}");
        assert_eq!(logs.matches("skipped \"Noon net\"").count(), 1, "{logs}");
    }

    #[test]
    fn a_session_that_errors_retries_in_a_new_folder() {
        let root = temp("error");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "3m")]);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let tries = AtomicU32::new(0);
        let make = |_: &NetCfg| -> Made {
            let n = tries.fetch_add(1, Ordering::SeqCst);
            assert!((n as usize) < MAX_JOINS, "re-recording a window");
            let plan = if n == 0 { Plan::Panic } else { Plan::up(0) };
            Ok(Box::new(FakeAdapter::new(&fake, RATE, vec![plan])))
        };
        let clock = Script::stopping(&fake, mins(10), &stop);
        run(&cfg, &clock, &stop, &make).unwrap();

        let found = sessions(&root);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(read_meta(&found[0]).unwrap().ended, Some(Ended::Error));
        assert_eq!(read_meta(&found[1]).unwrap().ended, Some(Ended::Window));
        assert_eq!(seconds(&found[1]), 120, "a minute after the failure");
    }

    #[test]
    fn sleeping_through_a_window_logs_it_missed() {
        let root = temp("missed");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let fake = FakeClock::new(at("11:00:00"));
        let stop = AtomicBool::new(false);
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let jumped = AtomicBool::new(false);
        let clock = Script::new(&fake, |c| {
            if !jumped.swap(true, Ordering::SeqCst) {
                c.advance(mins(5 * 60));
            }
            if c.elapsed() >= mins(6 * 60) {
                stop.store(true, Ordering::SeqCst);
            }
        });
        let logs = logged(|| run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap());

        assert_eq!(sessions(&root).len(), 0);
        assert_eq!(seen.lock().unwrap().joins.len(), 0);
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("missed"), "{logs}");
        assert!(logs.contains("Noon net"), "{logs}");
    }

    #[test]
    fn with_no_windows_the_daemon_idles_until_stopped() {
        let root = temp("idle");
        let cfg = config(&root, "0s", "0s", &[]);
        let fake = FakeClock::new(at("12:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let line: Mutex<String> = Mutex::default();
        let clock = Script::new(&fake, |c| {
            if c.elapsed() >= mins(5) {
                *line.lock().unwrap() = status_line(&root);
                stop.store(true, Ordering::SeqCst);
            }
        });
        run(&cfg, &clock, &stop, &linking(&fake, &seen)).unwrap();
        assert_eq!(*line.lock().unwrap(), "idle, nothing scheduled");
    }

    #[test]
    fn now_records_for_the_length_asked_and_ignores_case() {
        let root = temp("now");
        let cfg = config(&root, "5m", "5m", &[("Noon Net", "daily 12:00", "1m")]);
        let fake = FakeClock::new(at("03:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let meta = record_now(
            &cfg,
            &Live::off(),
            "noon NET",
            mins(2),
            &fake,
            &stop,
            &linking(&fake, &seen),
        )
        .unwrap();

        assert_eq!(meta.ended, Some(Ended::Window));
        let found = sessions(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(seconds(&found[0]), 120);
        assert!(!root.join(STATE_FILE).exists());
    }

    #[test]
    fn now_with_an_unknown_net_lists_the_names() {
        let root = temp("unknown");
        let nets = [
            ("First", "daily 12:00", "1m"),
            ("Second", "daily 14:00", "1m"),
        ];
        let cfg = config(&root, "0s", "0s", &nets);
        let fake = FakeClock::new(at("03:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let err = record_now(
            &cfg,
            &Live::off(),
            "Third",
            mins(2),
            &fake,
            &stop,
            &linking(&fake, &seen),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, DaemonError::UnknownNet { .. }), "{text}");
        assert!(text.contains("First") && text.contains("Second"), "{text}");
    }

    #[test]
    fn now_refuses_while_a_daemon_runs() {
        let root = temp("nowheld");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let _held = take_lock(&root);
        write_state(&root, "waiting", live_pid());
        let fake = FakeClock::new(at("03:00:00"));
        let stop = AtomicBool::new(false);
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let err = record_now(
            &cfg,
            &Live::off(),
            "Noon net",
            mins(2),
            &fake,
            &stop,
            &linking(&fake, &seen),
        )
        .unwrap_err();
        assert!(matches!(err, DaemonError::AlreadyRunning { .. }), "{err}");
        assert_eq!(seen.lock().unwrap().joins.len(), 0);
        assert!(root.join(STATE_FILE).exists(), "not ours to remove");
    }

    #[test]
    fn now_holds_the_state_file_while_it_records() {
        let root = temp("nowstate");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let fake = FakeClock::new(at("03:00:00"));
        let stop = AtomicBool::new(false);
        let seen = Arc::default();
        let line: Mutex<String> = Mutex::default();
        let clock = Script::new(&fake, |c| {
            if c.elapsed() == secs(30) {
                *line.lock().unwrap() = status_line(&root);
            }
        });
        record_now(
            &cfg,
            &Live::off(),
            "Noon net",
            mins(1),
            &clock,
            &stop,
            &linking(&fake, &seen),
        )
        .unwrap();
        assert_eq!(
            *line.lock().unwrap(),
            "recording \"Noon net\" since 03:00, linked"
        );
    }

    #[test]
    fn a_failed_now_is_an_error() {
        let root = temp("nowfail");
        let cfg = config(&root, "0s", "0s", &[("Noon net", "daily 12:00", "1m")]);
        let fake = FakeClock::new(at("03:00:00"));
        let stop = AtomicBool::new(false);
        let make = |_: &NetCfg| -> Made { Err("no such callsign".to_string()) };
        let err =
            record_now(&cfg, &Live::off(), "Noon net", mins(1), &fake, &stop, &make).unwrap_err();
        assert!(err.to_string().contains("no such callsign"), "{err}");
        assert!(!root.join(STATE_FILE).exists());
    }

    #[test]
    fn check_report_lists_windows_in_each_nets_zone() {
        let root = temp("report");
        let mut cfg = config(&root, "5m", "10m", &[("Noon net", "daily 12:00", "1h")]);
        cfg.nets[0].tz = jiff::tz::TimeZone::get("America/Denver").unwrap();
        cfg.recorder.callsign = "N0CALL".to_string();
        let (warnings, lines) = check_report(&cfg, at("00:00:00"), &StreamKeys::default()).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("N0CALL")),
            "{warnings:?}"
        );
        assert_eq!(lines.len(), 10);
        assert_eq!(
            lines[0],
            "Tue 2026-11-03 12:00 MST  \"Noon net\"  joins 11:55, leaves 13:10"
        );
    }

    #[test]
    fn check_report_says_when_nothing_is_scheduled() {
        let root = temp("empty");
        let cfg = config(&root, "5m", "10m", &[]);
        let (_, lines) = check_report(&cfg, at("00:00:00"), &StreamKeys::default()).unwrap();
        assert_eq!(
            lines,
            vec!["nothing scheduled in the next year".to_string()]
        );
    }
}
