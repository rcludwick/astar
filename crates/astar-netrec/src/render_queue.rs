// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Rendering `video.mp4` after each net, one session at a time.
//!
//! The daemon records on its own thread and must never wait for a render, so
//! a finished session's folder is handed to a [`RenderQueue`], whose one
//! worker thread renders the folders in the order they came. A render can
//! take minutes, and the next net may start meanwhile, so `ffmpeg` gets
//! [`RENDER_THREADS`] threads and no more: the next net's live encoder must
//! not be starved.
//!
//! A video is the first thing to go when the disk runs low, ahead of the
//! audio: a render is skipped, with one `info` line, unless
//! [`MIN_FREE_BYTES`] plus [`VIDEO_HEADROOM`] are free when it would start.
//! `astar-netrec render` can make that session's video later.
//!
//! Retention must not delete a folder the queue still holds, so the daemon
//! passes [`RenderQueue::busy`] to it.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use astar_netarchive::MIN_FREE_BYTES;
use astar_netvideo::{Ffmpeg, RenderOptions, VideoError, render_session};

use crate::session::FreeFn;

/// What a render needs free beyond what recording keeps back: a long net's
/// video is a gigabyte or so, and the next net still has to fit.
pub const VIDEO_HEADROOM: u64 = 2_000_000_000;

/// How many threads `ffmpeg` may encode a queued render with.
pub const RENDER_THREADS: u32 = 2;

/// What renders one session folder: `Ok` with a line for the log, or
/// `Err(None)` when the stop flag ended it, or `Err(Some(why))`.
pub type RenderFn = Arc<Render>;

/// The function behind a [`RenderFn`].
pub type Render = dyn Fn(&Path, &AtomicBool) -> Result<String, Option<String>> + Send + Sync;

#[derive(Default)]
struct State {
    waiting: VecDeque<PathBuf>,
    rendering: Option<PathBuf>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stop: AtomicBool,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        // The lock is never held across a render, and a render's panic is
        // caught, so a poisoned lock only means a panic elsewhere while it
        // was held; the state is still whole, so carry on with it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Session folders waiting for their video, and the thread that makes them.
pub struct RenderQueue {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl RenderQueue {
    /// A queue that renders with `ff`, counting `my_callsigns` as "my side"
    /// where a session names none. With no `ff` it renders nothing, and
    /// [`push`](Self::push) does nothing. `root` is the archive, whose free
    /// space decides whether a render may start.
    #[must_use]
    pub fn new(ff: Option<Ffmpeg>, root: PathBuf, my_callsigns: Vec<String>) -> Self {
        let render = ff.map(|ff| -> RenderFn {
            let opts = RenderOptions {
                my_callsigns,
                threads: Some(RENDER_THREADS),
            };
            Arc::new(
                move |dir, stop| match render_session(dir, &ff, &opts, stop) {
                    Ok(r) => Ok(format!(
                        "{} frames, {} reused, {:.1} s",
                        r.frames, r.reused, r.seconds
                    )),
                    Err(VideoError::Stopped) => Err(None),
                    Err(e) => Err(Some(e.to_string())),
                },
            )
        });
        Self::with(render, root, astar_netarchive::free_bytes)
    }

    /// [`new`](Self::new) with the render and the free-space reading
    /// supplied.
    pub(crate) fn with(render: Option<RenderFn>, root: PathBuf, free: FreeFn) -> Self {
        Self::with_spawn(render, root, free, |job| {
            thread::Builder::new()
                .name("netrec-render".to_owned())
                .spawn(job)
        })
    }

    /// [`with`](Self::with) with the thread start supplied. When the thread
    /// cannot start, the error is logged and the queue renders nothing.
    fn with_spawn(
        render: Option<RenderFn>,
        root: PathBuf,
        free: FreeFn,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<JoinHandle<()>>,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let worker = render.and_then(|render| {
            let job_shared = Arc::clone(&shared);
            match spawn(Box::new(move || work(&job_shared, &*render, &root, free))) {
                Ok(worker) => Some(worker),
                Err(e) => {
                    // Recording matters more than its video: carry on, as a
                    // queue without ffmpeg would.
                    tracing::error!(
                        "could not start the video render thread ({e}); no videos will be made"
                    );
                    None
                }
            }
        });
        Self { shared, worker }
    }

    /// Whether this queue renders anything at all.
    #[must_use]
    pub const fn renders(&self) -> bool {
        self.worker.is_some()
    }

    /// Whether the worker is still there to take folders: false on a queue
    /// that renders nothing, and false if the worker thread has ended.
    fn alive(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| !w.is_finished())
    }

    /// Queue `dir` for its video. Does nothing on a queue that renders
    /// nothing, one that is stopping, or one whose worker has gone.
    pub fn push(&self, dir: PathBuf) {
        if !self.alive() || self.shared.stop.load(Ordering::Acquire) {
            return;
        }
        self.shared.state().waiting.push_back(dir);
        self.shared.wake.notify_one();
    }

    /// Every folder queued or being rendered now. Empty once the worker has
    /// gone: nothing will render those folders, so retention may have them.
    #[must_use]
    pub fn busy(&self) -> Vec<PathBuf> {
        if !self.alive() {
            return Vec::new();
        }
        let state = self.shared.state();
        state
            .rendering
            .iter()
            .chain(state.waiting.iter())
            .cloned()
            .collect()
    }

    /// Stop: the render under way is killed (leaving no part file), the
    /// rest are dropped, and the worker is joined.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        {
            // Under the lock, so a worker about to wait cannot miss it.
            let _state = self.shared.state();
            self.shared.wake.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for RenderQueue {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The worker: render each folder in turn until stopped.
fn work(shared: &Shared, render: &Render, root: &Path, free: FreeFn) {
    loop {
        let dir = {
            let mut state = shared.state();
            loop {
                if shared.stop.load(Ordering::Acquire) {
                    state.waiting.clear();
                    return;
                }
                if let Some(dir) = state.waiting.pop_front() {
                    state.rendering = Some(dir.clone());
                    break dir;
                }
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // A render that panics must not take the worker with it: the folder
        // would stay busy forever, keeping every later one from retention.
        if panic::catch_unwind(AssertUnwindSafe(|| {
            render_one(shared, render, root, free, &dir);
        }))
        .is_err()
        {
            tracing::error!(
                "the video of {} failed: the render panicked; `astar-netrec render` can try again",
                dir.display()
            );
        }
        shared.state().rendering = None;
    }
}

fn render_one(shared: &Shared, render: &Render, root: &Path, free: FreeFn, dir: &Path) {
    let need = MIN_FREE_BYTES.saturating_add(VIDEO_HEADROOM);
    if let Ok(have) = free(root)
        && have < need
    {
        tracing::info!(
            "skipped the video of {}: {} MB free, and a video needs {} MB; `astar-netrec render` can make it later",
            dir.display(),
            have / 1_000_000,
            need / 1_000_000
        );
        return;
    }
    tracing::info!("rendering the video of {}", dir.display());
    match render(dir, &shared.stop) {
        Ok(what) => tracing::info!("wrote the video of {} ({what})", dir.display()),
        Err(None) => tracing::info!(
            "stopped the video of {}; `astar-netrec render` can make it later",
            dir.display()
        ),
        Err(Some(why)) => tracing::error!("the video of {} failed: {why}", dir.display()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};

    use astar_netvideo::{VIDEO_FILE, VIDEO_PART};

    use super::*;

    fn temp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "netrec-render-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Waits up to 10 s for `done`.
    fn wait_for(done: impl Fn() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        done()
    }

    /// A render that records the folders it is given.
    fn recording(seen: &Arc<Mutex<Vec<PathBuf>>>) -> RenderFn {
        let seen = Arc::clone(seen);
        Arc::new(move |dir, _| {
            seen.lock().unwrap().push(dir.to_path_buf());
            Ok(String::new())
        })
    }

    #[allow(clippy::unnecessary_wraps)]
    fn plenty(_: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn short(_: &Path) -> io::Result<u64> {
        Ok(MIN_FREE_BYTES + VIDEO_HEADROOM - 1)
    }

    #[test]
    fn renders_in_order_and_empties() {
        let root = temp("order");
        let seen = Arc::default();
        let q = RenderQueue::with(Some(recording(&seen)), root.clone(), plenty);
        q.push(root.join("a"));
        q.push(root.join("b"));
        assert!(wait_for(|| q.busy().is_empty()));
        q.stop();
        assert_eq!(*seen.lock().unwrap(), vec![root.join("a"), root.join("b")]);
    }

    #[test]
    fn a_queue_without_ffmpeg_takes_nothing() {
        let root = temp("off");
        let q = RenderQueue::new(None, root.clone(), Vec::new());
        assert!(!q.renders());
        q.push(root.join("a"));
        assert_eq!(q.busy(), Vec::<PathBuf>::new());
        q.stop();
    }

    #[test]
    fn a_queue_whose_thread_cannot_start_renders_nothing() {
        let root = temp("nothread");
        let seen = Arc::default();
        let q = RenderQueue::with_spawn(Some(recording(&seen)), root.clone(), plenty, |_| {
            Err(io::Error::other("no threads left"))
        });
        assert!(!q.renders());
        q.push(root.join("a"));
        assert_eq!(q.busy(), Vec::<PathBuf>::new());
        q.stop();
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn low_disk_skips_the_render() {
        let root = temp("lowdisk");
        let seen = Arc::default();
        let q = RenderQueue::with(Some(recording(&seen)), root.clone(), short);
        q.push(root.join("a"));
        assert!(wait_for(|| q.busy().is_empty()));
        q.stop();
        assert!(seen.lock().unwrap().is_empty(), "rendered on a low disk");
    }

    #[test]
    fn a_render_under_way_is_busy() {
        let root = temp("busy");
        let release = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&release);
        let render: RenderFn = Arc::new(move |_, stop| {
            while !r.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(String::new())
        });
        let q = RenderQueue::with(Some(render), root.clone(), plenty);
        q.push(root.join("a"));
        q.push(root.join("b"));
        assert!(wait_for(|| q.busy().len() == 2));
        assert_eq!(q.busy(), vec![root.join("a"), root.join("b")]);
        release.store(true, Ordering::Release);
        assert!(wait_for(|| q.busy().is_empty()));
        q.stop();
    }

    #[test]
    fn a_panicking_render_does_not_stop_the_queue() {
        let root = temp("panic");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        let bad = root.join("a");
        let render: RenderFn = Arc::new(move |dir, _| {
            assert!(dir != bad, "a render that panics");
            s.lock().unwrap().push(dir.to_path_buf());
            Ok(String::new())
        });
        let q = RenderQueue::with(Some(render), root.clone(), plenty);
        q.push(root.join("a"));
        q.push(root.join("b"));
        assert!(wait_for(|| q.busy().is_empty()), "busy: {:?}", q.busy());
        q.push(root.join("c"));
        assert!(wait_for(|| q.busy().is_empty()), "busy: {:?}", q.busy());
        q.stop();
        assert_eq!(*seen.lock().unwrap(), vec![root.join("b"), root.join("c")]);
    }

    /// A finished session from the archive fixture, under `root`.
    fn fixture_session(root: &Path) -> PathBuf {
        let _ = astar_netarchive::fixture::write_fixture(root);
        let net = root.join(astar_netarchive::net_dir_name("Tuesday M17 net"));
        std::fs::read_dir(&net)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.join("session.flac").is_file())
            .unwrap()
    }

    #[test]
    fn a_stopped_render_leaves_no_part_file() {
        let root = temp("stopped");
        let dir = fixture_session(&root);
        // Writes the part file, then never reads a frame, as a wedged
        // ffmpeg would.
        let script = root.join("ffmpeg");
        std::fs::write(
            &script,
            "#!/bin/sh\nfor a; do last=$a; done\necho junk > \"${last#file:}\"\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ff = Ffmpeg {
            path: script,
            version: "fake".to_owned(),
        };
        let q = RenderQueue::new(Some(ff), root.clone(), Vec::new());
        q.push(dir.clone());
        assert!(wait_for(|| dir.join(VIDEO_PART).exists()), "never started");
        let t = Instant::now();
        q.stop();
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert!(!dir.join(VIDEO_PART).exists(), "part file left");
        assert!(!dir.join(VIDEO_FILE).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
