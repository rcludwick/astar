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

use std::cell::Cell;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use astar_netarchive::{Ended, RetainPolicy, SessionMeta, recover_unfinished, retain};
use jiff::civil::DateTime;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ConfigError, NetCfg, validate};
use crate::feed::AudioFeed;
use crate::listen::LinkStatus;
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

/// An adapter that keeps the state file's `linked` true to the link. The
/// session polls `status` every tick, so this sees every change, and writes
/// only on a change. It owns what it needs because a join moves the adapter
/// to its own thread.
struct Watched {
    inner: Box<dyn NetAdapter>,
    root: PathBuf,
    state: DaemonState,
    linked: Cell<bool>,
}

impl NetAdapter for Watched {
    fn join(&mut self) -> Result<Box<dyn AudioFeed>, String> {
        self.inner.join()
    }

    fn status(&self) -> LinkStatus {
        let status = self.inner.status();
        let linked = matches!(status, LinkStatus::Linked);
        if self.linked.replace(linked) != linked {
            let state = DaemonState {
                linked: Some(linked),
                ..self.state.clone()
            };
            if let Err(e) = write_state(&self.root, &state) {
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

/// Record an opened session to `leave_at`, showing it in the state file.
fn run_open_session(
    hold: &mut Hold,
    net: &NetCfg,
    (adapter, rec): (Box<dyn NetAdapter>, SessionRecorder),
    leave_at: Timestamp,
    clock: &dyn Clock,
    stop: &AtomicBool,
) -> Result<SessionMeta, SessionError> {
    let state = DaemonState::recording(net, clock.now());
    hold.show(state.clone());
    let mut watched = Watched {
        inner: adapter,
        root: hold.root.clone(),
        state,
        linked: Cell::new(false),
    };
    let result = run_session(&mut watched, rec, leave_at, clock, stop);
    // `Watched` wrote past `hold`, so what it remembers may be out of date.
    hold.shown = None;
    result
}

/// The daemon's working state.
struct Daemon<'a> {
    cfg: &'a Config,
    clock: &'a dyn Clock,
    stop: &'a AtomicBool,
    make: &'a dyn Fn(&NetCfg) -> Made,
    free: FreeFn,
    hold: Hold,
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
    /// this same thread, so `busy` is empty. A failure is only logged: it
    /// must not cost the next net.
    fn retain(&mut self) {
        self.last_retain = self.clock.instant();
        let r = &self.cfg.recorder;
        let policy = RetainPolicy {
            max_bytes: r.max_bytes,
            max_age: Duration::from_secs(u64::from(r.max_age_days) * 86_400),
        };
        match retain(&r.root, &policy, self.clock.now(), &[]) {
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
        let result = run_open_session(
            &mut self.hold,
            net,
            opened,
            window.leave_at,
            self.clock,
            self.stop,
        );
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
    clock: &dyn Clock,
    stop: &AtomicBool,
    make: &dyn Fn(&NetCfg) -> Made,
) -> Result<(), DaemonError> {
    run_with(cfg, clock, stop, make, astar_netarchive::free_bytes)
}

/// [`run`] with the free-space reading supplied.
pub(crate) fn run_with(
    cfg: &Config,
    clock: &dyn Clock,
    stop: &AtomicBool,
    make: &dyn Fn(&NetCfg) -> Made,
    free: FreeFn,
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
    let mut daemon = Daemon {
        cfg,
        clock,
        stop,
        make,
        free,
        hold,
        last_retain: clock.instant(),
        skipped: 0,
    };
    daemon.retain();
    daemon.run();
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
    let meta = run_open_session(&mut hold, net, opened, leave_at, clock, stop)?;
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
            format!("recording {net:?} since {since}, {link}")
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

/// What `check` prints: `validate`'s warnings, then the next windows, one line
/// each, in each net's zone.
///
/// # Errors
/// Whatever [`validate`] refuses.
pub fn check_report(
    cfg: &Config,
    now: Timestamp,
) -> Result<(Vec<String>, Vec<String>), ConfigError> {
    let warnings = validate(cfg, now)?;
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

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    use astar_netarchive::{Ended, read_meta, write_meta};

    use super::*;
    use crate::config::from_toml_str;
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
            run_with(&cfg, &clock, &stop, &linking(&fake, &seen), full).unwrap();
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
        run_with(&cfg, &clock, &stop, &linking(&fake, &seen), clears).unwrap();

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
        let err =
            record_now(&cfg, "Third", mins(2), &fake, &stop, &linking(&fake, &seen)).unwrap_err();
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
        let err = record_now(&cfg, "Noon net", mins(1), &fake, &stop, &make).unwrap_err();
        assert!(err.to_string().contains("no such callsign"), "{err}");
        assert!(!root.join(STATE_FILE).exists());
    }

    #[test]
    fn check_report_lists_windows_in_each_nets_zone() {
        let root = temp("report");
        let mut cfg = config(&root, "5m", "10m", &[("Noon net", "daily 12:00", "1h")]);
        cfg.nets[0].tz = jiff::tz::TimeZone::get("America/Denver").unwrap();
        cfg.recorder.callsign = "N0CALL".to_string();
        let (warnings, lines) = check_report(&cfg, at("00:00:00")).unwrap();
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
        let (_, lines) = check_report(&cfg, at("00:00:00")).unwrap();
        assert_eq!(
            lines,
            vec!["nothing scheduled in the next year".to_string()]
        );
    }
}
