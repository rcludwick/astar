// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The live stream: a running session, drawn as frame A, encoded by
//! `ffmpeg` and pushed to `YouTube` and Twitch while the net is on the air.
//!
//! [`LiveStream::start`] takes the session's [`TapReceiver`] and runs one
//! engine thread. The engine reads the tap on its own clock, feeds a
//! [`SceneBuilder`], and for every frame period of *media* time (the tap's
//! `offset_ms`, so sound and picture stay on the session's own timeline)
//! hands that span's PCM to an audio FIFO and the frame's RGBA to `ffmpeg`'s
//! stdin. Each of those two pipes has a writer thread of its own, so neither
//! can hold up the other. `ffmpeg` writes FLV on its stdout; a reader thread
//! turns that into [`FlvTag`]s, keeps the newest sequence headers, and offers
//! every tag to each destination's [`Publisher`]. No stream key ever reaches
//! `ffmpeg`: it only writes FLV to a pipe.
//!
//! **Falling behind.** When the tap's newest audio is more than two seconds
//! ahead of the frame being written, the engine jumps to half a second
//! behind it. Sound and picture skip the same span, so the encoder never
//! sees the gap. A third jump within a minute restarts the encoder one rung
//! down [`LADDER`] (1080p30, 1080p15, 720p15); it never steps back up within
//! a session.
//!
//! **An encoder that exits** is started again after 1 s, doubling to 30 s,
//! as a destination reconnects. Its timestamps carry on from where the last
//! one stopped, so the publishers stay connected and see one unbroken
//! stream.
//!
//! [`LiveStream::stop`] closes the tap, kills `ffmpeg`, joins every thread
//! and removes the FIFO's folder.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ffmpeg::{Child, Ffmpeg};
use crate::flv::{FlvReader, FlvTag};
use crate::render::Renderer;
use crate::rtmp::{DestStatus, Destination, Publisher, SeqHeaders, Tuning};
use crate::scene::{FrameState, Mode, SceneBuilder, SessionInfo};
use crate::tap::{TapMsg, TapReceiver};
use crate::video::{LADDER, Profile, file_url, read_tail};

/// What the state file and `status` show of a live stream. It holds no
/// counters of its own, so it changes only when something worth showing
/// does.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct LiveSummary {
    /// The rung of the ladder in use, such as `1080p30`.
    pub profile: String,
    /// Each destination's status, in the order they were given.
    pub destinations: Vec<DestStatus>,
}

/// Why a live stream did not start.
#[derive(Debug)]
pub enum LiveError {
    /// No destination was given.
    NoDestinations,
    /// The FIFO's folder or the FIFO could not be made.
    Io(String),
    /// `ffmpeg` could not be started.
    Ffmpeg(String),
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDestinations => f.write_str("the live stream has nowhere to go"),
            Self::Io(why) => write!(f, "the live stream could not be set up: {why}"),
            Self::Ffmpeg(why) => write!(f, "the live encoder could not be started: {why}"),
        }
    }
}

impl std::error::Error for LiveError {}

/// How the engine paces itself. [`Default`] is what ships; tests shorten it.
#[derive(Clone, Debug)]
pub(crate) struct LiveTuning {
    /// Further behind the tap than this and the engine jumps.
    pub behind: Duration,
    /// Where a jump lands: this far behind the tap's newest audio.
    pub rejoin: Duration,
    /// This many jumps within `jump_window` steps down the ladder.
    pub jumps_to_step: usize,
    pub jump_window: Duration,
    /// The encoder's restart backoff: first wait, cap, and how long it must
    /// have run for the next wait to start from the first again.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub reset_after: Duration,
    /// An encoder that takes no frame for this long is killed and restarted.
    pub wedged: Duration,
    /// Tests: sleep this long before writing each frame, as a machine too
    /// slow for the rung would.
    pub slow_frame: Option<Duration>,
    /// The publishers' timeouts and backoff.
    pub publisher: Tuning,
}

impl Default for LiveTuning {
    fn default() -> Self {
        Self {
            behind: Duration::from_secs(2),
            rejoin: Duration::from_millis(500),
            jumps_to_step: 3,
            jump_window: Duration::from_secs(60),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            reset_after: Duration::from_secs(60),
            wedged: Duration::from_secs(10),
            slow_frame: None,
            publisher: Tuning::default(),
        }
    }
}

/// How often the engine refreshes the shared summary.
const SUMMARY_EVERY: Duration = Duration::from_millis(250);
/// Frames queued for the video writer: a little slack, no more, so a slow
/// encoder shows up as the engine falling behind.
const FRAME_QUEUE: usize = 2;
/// Frames' worth of audio queued for the FIFO writer. Generous, because
/// `ffmpeg` opens the FIFO only after it has opened and probed the video
/// on stdin, and audio queues meanwhile; the writer waits up to
/// [`FIFO_OPEN_WAIT`] for that, and 512 covers 15 s at 30 fps. Bounded, so an `ffmpeg` that stops reading audio costs a
/// restart rather than unbounded memory.
const AUDIO_QUEUE: usize = 512;
/// A gap in the tap longer than this restarts the audio buffer rather than
/// filling it with silence.
const MAX_GAP_S: u64 = 5;
/// How long the audio writer waits for `ffmpeg` to open the FIFO.
const FIFO_OPEN_WAIT: Duration = Duration::from_secs(15);
/// The gap left between one encoder's last timestamp and the next one's
/// first, so the timeline only ever moves forward.
const RESTART_GAP_MS: u32 = 100;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A live stream from one running session.
pub struct LiveStream {
    stop: Arc<AtomicBool>,
    engine: Option<JoinHandle<()>>,
    publishers: Option<Arc<Vec<Publisher>>>,
    rung: Arc<AtomicUsize>,
    slot: Arc<Mutex<LiveSummary>>,
    fifo_dir: PathBuf,
    pid: Arc<AtomicU32>,
}

impl LiveStream {
    /// Start streaming the session `info` describes, whose tap is `rx`, to
    /// every one of `dests`, encoding with `ff`. Returns once `ffmpeg` is
    /// running; the destinations connect on their own threads.
    ///
    /// # Errors
    /// [`LiveError`]: no destinations, no FIFO, or `ffmpeg` would not start.
    /// Nothing is left running or on disk.
    pub fn start(
        info: SessionInfo,
        ff: Ffmpeg,
        dests: Vec<Destination>,
        rx: TapReceiver,
    ) -> Result<Self, LiveError> {
        Self::start_with(info, ff, dests, rx, LiveTuning::default())
    }

    pub(crate) fn start_with(
        info: SessionInfo,
        ff: Ffmpeg,
        dests: Vec<Destination>,
        rx: TapReceiver,
        tuning: LiveTuning,
    ) -> Result<Self, LiveError> {
        if dests.is_empty() {
            return Err(LiveError::NoDestinations);
        }
        let fifo_dir = make_private_dir().map_err(|e| LiveError::Io(e.to_string()))?;
        let rate = info.sample_rate.max(1);
        let headers = Arc::new(Mutex::new(SeqHeaders::default()));
        let ts_end = Arc::new(AtomicU32::new(0));
        let profile = LADDER[0];
        let publishers: Arc<Vec<Publisher>> = Arc::new(
            dests
                .into_iter()
                .map(|d| Publisher::spawn_with(d, Arc::clone(&headers), tuning.publisher.clone()))
                .collect(),
        );
        let pipes = Pipes {
            ff,
            rate,
            headers: Arc::clone(&headers),
            publishers: Arc::clone(&publishers),
            ts_end: Arc::clone(&ts_end),
            slow_frame: tuning.slow_frame,
        };
        let first = match Encoder::spawn(&pipes, profile, &fifo_dir.join("audio-1.pcm"), 0) {
            Ok(enc) => enc,
            Err(e) => {
                drop(stop_publishers(publishers));
                let _ = std::fs::remove_dir_all(&fifo_dir);
                return Err(LiveError::Ffmpeg(e.to_string()));
            }
        };
        let pid = Arc::new(AtomicU32::new(first.pid));
        let stop = Arc::new(AtomicBool::new(false));
        let rung = Arc::new(AtomicUsize::new(0));
        let slot = Arc::new(Mutex::new(summary_of(0, &publishers)));
        let renderer = Renderer::new(profile.size);
        let mut scene = SceneBuilder::new(info, Mode::Live);
        scene.set_capacity(renderer.column_capacity());
        let engine = Engine {
            rate: u64::from(rate),
            rx,
            scene,
            renderer,
            rung: 0,
            shared_rung: Arc::clone(&rung),
            tuning,
            stop: Arc::clone(&stop),
            pipes,
            slot: Arc::clone(&slot),
            fifo_dir: fifo_dir.clone(),
            pid: Arc::clone(&pid),
            pending: VecDeque::new(),
            last_time: 0,
            newest: None,
            audio: VecDeque::new(),
            audio_start: 0,
            cursor: None,
            enc: Some(first),
            generation: 1,
            jumps: VecDeque::new(),
            backoff: Duration::ZERO,
            retry_at: None,
            last_frame: None,
            last_summary: Instant::now(),
        };
        let mut engine = engine;
        engine.backoff = engine.tuning.backoff_min;
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let handle = thread::Builder::new()
            .name("live-engine".into())
            .spawn(move || tracing::dispatcher::with_default(&dispatch, || engine.run()));
        let engine = match handle {
            Ok(h) => h,
            Err(e) => {
                // The engine (and with it the encoder) was dropped unstarted.
                drop(stop_publishers(publishers));
                let _ = std::fs::remove_dir_all(&fifo_dir);
                return Err(LiveError::Io(format!("could not start its thread: {e}")));
            }
        };
        Ok(Self {
            stop,
            engine: Some(engine),
            publishers: Some(publishers),
            rung,
            slot,
            fifo_dir,
            pid,
        })
    }

    /// Where the stream stands now.
    #[must_use]
    pub fn summary(&self) -> LiveSummary {
        match &self.publishers {
            Some(p) => summary_of(self.rung.load(Ordering::Acquire), p),
            None => lock(&self.slot).clone(),
        }
    }

    /// A summary the engine keeps current (a few times a second), for a
    /// reader that must never wait on the publishers.
    #[must_use]
    pub fn summary_slot(&self) -> Arc<Mutex<LiveSummary>> {
        Arc::clone(&self.slot)
    }

    /// Stop: close the tap, kill `ffmpeg`, close every destination, join
    /// every thread and remove the FIFO's folder. Returns where the stream
    /// stood just before.
    #[allow(clippy::must_use_candidate)] // stopping is the point; the summary is a bonus
    pub fn stop(mut self) -> LiveSummary {
        self.halt()
    }

    fn halt(&mut self) -> LiveSummary {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.engine.take() {
            let _ = h.join();
        }
        let summary = self.summary();
        if let Some(p) = self.publishers.take() {
            drop(stop_publishers(p));
        }
        let _ = std::fs::remove_dir_all(&self.fifo_dir);
        self.pid.store(0, Ordering::Release);
        *lock(&self.slot) = summary.clone();
        summary
    }

    /// The running encoder's process id, 0 between encoders.
    #[cfg(test)]
    pub(crate) fn encoder_pid(&self) -> u32 {
        self.pid.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn fifo_dir(&self) -> &Path {
        &self.fifo_dir
    }
}

impl Drop for LiveStream {
    fn drop(&mut self) {
        if self.engine.is_some() || self.publishers.is_some() {
            self.halt();
        }
    }
}

/// Signal every publisher, then wait for them: the stop takes as long as
/// the slowest, not the sum. Returns what is left to drop (nothing, unless
/// a thread still held a reference).
fn stop_publishers(p: Arc<Vec<Publisher>>) -> Option<Arc<Vec<Publisher>>> {
    for one in p.iter() {
        one.begin_stop();
    }
    match Arc::try_unwrap(p) {
        Ok(all) => {
            drop(all);
            None
        }
        Err(still) => Some(still),
    }
}

fn summary_of(rung: usize, publishers: &[Publisher]) -> LiveSummary {
    LiveSummary {
        profile: LADDER[rung.min(LADDER.len() - 1)].name(),
        destinations: publishers.iter().map(Publisher::status).collect(),
    }
}

// ---- the encoder ----

/// What every encoder of one stream shares.
struct Pipes {
    ff: Ffmpeg,
    rate: u32,
    headers: Arc<Mutex<SeqHeaders>>,
    publishers: Arc<Vec<Publisher>>,
    /// The latest timestamp handed to the publishers.
    ts_end: Arc<AtomicU32>,
    slow_frame: Option<Duration>,
}

/// One `ffmpeg` and its four threads: video writer, audio writer, FLV
/// reader and stderr tail.
struct Encoder {
    child: Arc<Mutex<Child>>,
    pid: u32,
    profile: Profile,
    video: Option<SyncSender<Arc<[u8]>>>,
    audio: Option<SyncSender<Vec<u8>>>,
    /// Tells the audio writer to stop waiting for the FIFO to open.
    done: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    tail: Option<JoinHandle<Vec<String>>>,
    fifo: PathBuf,
    started: Instant,
    /// Frames written since this encoder started.
    frames: u64,
}

impl Encoder {
    fn spawn(p: &Pipes, profile: Profile, fifo: &Path, base_ms: u32) -> io::Result<Self> {
        make_fifo(fifo)?;
        let mut child = match Child::spawn(&mut encoder_command(&p.ff, profile, p.rate, fifo)) {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(fifo);
                return Err(e);
            }
        };
        let pid = child.id();
        let stdin = child.take_stdin();
        let stdout = child.take_stdout();
        let tail = read_tail(child.take_stderr());
        let done = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::with_capacity(3);

        let (vtx, vrx) = mpsc::sync_channel::<Arc<[u8]>>(FRAME_QUEUE);
        let slow = p.slow_frame;
        threads.push(thread::spawn(move || {
            let Some(mut stdin) = stdin else { return };
            for frame in vrx {
                if let Some(d) = slow {
                    thread::sleep(d);
                }
                if stdin.write_all(&frame).is_err() {
                    return;
                }
            }
        }));

        let (atx, arx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE);
        let (path, gone) = (fifo.to_path_buf(), Arc::clone(&done));
        threads.push(thread::spawn(move || {
            let Some(mut f) = open_fifo_writer(&path, &gone) else {
                return;
            };
            for chunk in arx {
                if f.write_all(&chunk).is_err() {
                    return;
                }
            }
        }));

        let (headers, publishers, ts_end) = (
            Arc::clone(&p.headers),
            Arc::clone(&p.publishers),
            Arc::clone(&p.ts_end),
        );
        threads.push(thread::spawn(move || {
            let Some(stdout) = stdout else { return };
            let mut r = FlvReader::new(io::BufReader::new(stdout));
            while let Ok(Some(tag)) = r.next_tag() {
                let ts = base_ms.saturating_add(tag.ts_ms);
                ts_end.fetch_max(ts, Ordering::AcqRel);
                let tag = Arc::new(FlvTag { ts_ms: ts, ..tag });
                lock(&headers).observe(&tag);
                for one in publishers.iter() {
                    one.offer(Arc::clone(&tag));
                }
            }
        }));

        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            pid,
            profile,
            video: Some(vtx),
            audio: Some(atx),
            done,
            threads,
            tail: Some(tail),
            fifo: fifo.to_path_buf(),
            started: Instant::now(),
            frames: 0,
        })
    }

    /// Whether `ffmpeg` has exited (it is reaped if so).
    fn exited(&self) -> bool {
        !matches!(lock(&self.child).try_wait(), Ok(None))
    }

    fn kill(&self) {
        lock(&self.child).kill();
    }

    /// Kill `ffmpeg`, join its threads, remove the FIFO and return the last
    /// lines it wrote to stderr.
    fn shutdown(&mut self) -> Vec<String> {
        self.done.store(true, Ordering::Release);
        self.kill();
        self.video = None;
        self.audio = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.fifo);
        self.tail
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Queue one frame's audio for the FIFO writer without waiting. `false`
/// when the queue is full, which means `ffmpeg` has stopped reading it; a
/// writer that has gone (its encoder is ending) counts as taken.
fn queue_audio(tx: &SyncSender<Vec<u8>>, pcm: Vec<u8>) -> bool {
    !matches!(tx.try_send(pcm), Err(TrySendError::Full(_)))
}

/// `ffmpeg` reading RGBA frames on stdin and mono PCM from `fifo`, writing
/// FLV on stdout with the live settings.
fn encoder_command(ff: &Ffmpeg, profile: Profile, rate: u32, fifo: &Path) -> Command {
    let size = format!("{}x{}", profile.size.width, profile.size.height);
    let fps = profile.fps.to_string();
    let gop = (profile.fps * 2).to_string();
    let rate_k = format!("{}k", profile.video_kbps.min(6_000));
    let mut cmd = Command::new(&ff.path);
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "warning"])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-s", &size])
        .args(["-r", &fps, "-i", "pipe:0"])
        .args(["-f", "s16le", "-ar", &rate.to_string(), "-ac", "1", "-i"])
        .arg(file_url(fifo))
        .args(["-map", "0:v", "-map", "1:a"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
        ])
        .args(["-pix_fmt", "yuv420p"])
        .args(["-b:v", &rate_k, "-minrate", &rate_k, "-maxrate", &rate_k])
        .args(["-bufsize", &rate_k, "-x264-params", "nal-hrd=cbr"])
        .args(["-g", &gop, "-keyint_min", &gop, "-sc_threshold", "0"])
        .args(["-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2"])
        .args(["-f", "flv", "-flvflags", "no_duration_filesize", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

// ---- the FIFO ----

/// A fresh folder only this user can enter, under the system's temp dir.
#[cfg(unix)]
fn make_private_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::temp_dir();
    for _ in 0..100 {
        let dir = base.join(format!(
            "astar-live-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free folder name under the temp dir",
    ))
}

#[cfg(unix)]
fn make_fifo(path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the FIFO path"))?;
    // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Open the FIFO for writing once `ffmpeg` has it open for reading. A
/// blocking open would hang forever on an `ffmpeg` that died first, so it
/// is opened non-blocking (which fails until a reader is there) and tried
/// again until `done` or [`FIFO_OPEN_WAIT`]; then made blocking, so a write
/// waits for `ffmpeg` to read rather than failing.
#[cfg(unix)]
fn open_fifo_writer(path: &Path, done: &AtomicBool) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let give_up = Instant::now() + FIFO_OPEN_WAIT;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(f) => {
                let fd = f.as_raw_fd();
                // SAFETY: `fd` is open for as long as `f` lives.
                let ok = unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) == 0
                };
                return ok.then_some(f);
            }
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                if done.load(Ordering::Acquire) || Instant::now() >= give_up {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(not(unix))]
fn make_private_dir() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "live streaming needs a Unix FIFO",
    ))
}

#[cfg(not(unix))]
fn make_fifo(_: &Path) -> io::Result<()> {
    make_private_dir().map(|_| ())
}

#[cfg(not(unix))]
fn open_fifo_writer(_: &Path, _: &AtomicBool) -> Option<std::fs::File> {
    None
}

// ---- the engine ----

struct Engine {
    rate: u64,
    rx: TapReceiver,
    scene: SceneBuilder,
    renderer: Renderer,
    rung: usize,
    shared_rung: Arc<AtomicUsize>,
    tuning: LiveTuning,
    stop: Arc<AtomicBool>,
    pipes: Pipes,
    slot: Arc<Mutex<LiveSummary>>,
    fifo_dir: PathBuf,
    pid: Arc<AtomicU32>,
    /// Tap messages not yet applied, each with its session sample.
    pending: VecDeque<(u64, TapMsg)>,
    /// The session sample of the last message queued, which a message
    /// without a time of its own inherits.
    last_time: u64,
    /// The end of the newest audio the tap has given, in session samples.
    newest: Option<u64>,
    /// Audio for the FIFO, from session sample `audio_start` on.
    audio: VecDeque<i16>,
    audio_start: u64,
    /// The session sample the next frame starts at.
    cursor: Option<u64>,
    enc: Option<Encoder>,
    generation: u32,
    /// When the engine jumped, within `jump_window`.
    jumps: VecDeque<Instant>,
    backoff: Duration,
    /// When to start an encoder again, while there is none.
    retry_at: Option<Instant>,
    /// The last frame drawn and the state it was drawn from: an unchanged
    /// state sends the same bytes again without copying them.
    last_frame: Option<(FrameState, Arc<[u8]>)>,
    last_summary: Instant,
}

impl Engine {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn samples(&self, d: Duration) -> u64 {
        u64::try_from(d.as_millis()).unwrap_or(u64::MAX) * self.rate / 1_000
    }

    fn run(mut self) {
        while !self.stopped() {
            self.drain();
            self.refresh_summary(false);
            if self.enc.is_none() {
                self.follow();
                if self.retry_at.is_some_and(|t| Instant::now() >= t) {
                    self.start_encoder();
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
                continue;
            }
            if self.enc.as_ref().is_some_and(Encoder::exited) {
                self.encoder_died();
                continue;
            }
            if !self.step() {
                thread::sleep(Duration::from_millis(5));
            }
        }
        if let Some(mut enc) = self.enc.take() {
            enc.shutdown();
        }
        self.pid.store(0, Ordering::Release);
    }

    /// Move everything waiting in the tap into `pending`.
    fn drain(&mut self) {
        // `Disconnected` means the session has closed; what is queued still
        // plays out until `stop`.
        while let Ok(m) = self.rx.try_recv() {
            self.queue(m);
        }
    }

    fn queue(&mut self, m: TapMsg) {
        let at = |ms: u64| ms.saturating_mul(self.rate) / 1_000;
        let time = match &m {
            TapMsg::Audio { offset_ms, samples } => {
                let s = at(*offset_ms);
                let end = s + samples.len() as u64;
                self.newest = Some(self.newest.map_or(end, |n| n.max(end)));
                if self.cursor.is_none() {
                    self.cursor = Some(s);
                }
                s
            }
            TapMsg::OverStart { offset_ms, .. } => at(*offset_ms),
            TapMsg::OverTalker { .. } | TapMsg::OverEnd { .. } => self.last_time,
        };
        self.last_time = time;
        self.pending.push_back((time, m));
    }

    /// Apply queued messages that start before session sample `limit` to
    /// the scene, and their audio to the FIFO buffer when `to_fifo`.
    fn apply_until(&mut self, limit: u64, to_fifo: bool) {
        while self.pending.front().is_some_and(|(t, _)| *t < limit) {
            let Some((t, m)) = self.pending.pop_front() else {
                break;
            };
            match m {
                TapMsg::Audio { offset_ms, samples } => {
                    self.scene.audio(offset_ms, &samples);
                    if to_fifo {
                        self.buffer(t, &samples);
                    }
                }
                TapMsg::OverStart {
                    seq,
                    talker,
                    offset_ms,
                } => self.scene.over_start(seq, talker.as_deref(), offset_ms),
                TapMsg::OverTalker { seq, talker } => self.scene.over_talker(seq, &talker),
                TapMsg::OverEnd { seq, duration_ms } => self.scene.over_end(seq, duration_ms),
            }
        }
    }

    /// Keep `samples`, which start at session sample `t`, for the FIFO.
    fn buffer(&mut self, t: u64, samples: &[i16]) {
        let end = self.audio_start + self.audio.len() as u64;
        if self.audio.is_empty() || t > end + MAX_GAP_S * self.rate {
            self.audio.clear();
            self.audio_start = t;
        } else if t > end {
            // Ticks the tap dropped: silence in their place.
            self.audio.extend(std::iter::repeat_n(
                0,
                usize::try_from(t - end).unwrap_or(0),
            ));
        }
        let end = self.audio_start + self.audio.len() as u64;
        let skip = usize::try_from(end.saturating_sub(t)).unwrap_or(usize::MAX);
        self.audio.extend(samples.iter().skip(skip));
    }

    /// The `n` samples from session sample `from` as little-endian bytes,
    /// silence where none arrived; everything before `from + n` is dropped.
    fn take_audio(&mut self, from: u64, n: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(usize::try_from(n * 2).unwrap_or(0));
        for s in from..from + n {
            let v = s
                .checked_sub(self.audio_start)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| self.audio.get(i).copied())
                .unwrap_or(0);
            out.extend_from_slice(&v.to_le_bytes());
        }
        self.drop_audio_before(from + n);
        out
    }

    fn drop_audio_before(&mut self, s: u64) {
        let k = usize::try_from(s.saturating_sub(self.audio_start))
            .unwrap_or(usize::MAX)
            .min(self.audio.len());
        self.audio.drain(..k);
        self.audio_start = self.audio_start.max(s);
    }

    /// With no encoder, keep the scene current and the cursor at the tap's
    /// newest audio, so a new encoder starts from now.
    fn follow(&mut self) {
        self.apply_until(u64::MAX, false);
        self.audio.clear();
        if let Some(n) = self.newest {
            self.cursor = Some(n);
            self.audio_start = n;
        }
    }

    /// Write one frame and its audio if the tap has given enough. Returns
    /// whether anything happened.
    fn step(&mut self) -> bool {
        let (Some(newest), Some(cursor)) = (self.newest, self.cursor) else {
            return false;
        };
        if newest > cursor + self.samples(self.tuning.behind) {
            self.jump(newest);
            return true;
        }
        let Some(enc) = self.enc.as_ref() else {
            return false;
        };
        let fps = u64::from(enc.profile.fps.max(1));
        let k = enc.frames;
        let n = (k + 1) * self.rate / fps - k * self.rate / fps;
        if newest < cursor + n {
            return false;
        }
        self.apply_until(cursor + n, true);
        let pcm = self.take_audio(cursor, n);
        let state = self.scene.frame_at(cursor * 1_000 / self.rate);
        let frame = match &self.last_frame {
            Some((s, f)) if *s == state => Arc::clone(f),
            _ => {
                let f: Arc<[u8]> = Arc::from(self.renderer.render(&state));
                self.last_frame = Some((state, Arc::clone(&f)));
                f
            }
        };
        self.cursor = Some(cursor + n);
        if let Some(enc) = self.enc.as_mut() {
            enc.frames += 1;
        }
        self.send(pcm, frame);
        true
    }

    /// Hand a frame's audio and picture to the writers. Audio never waits.
    /// The picture waits while the encoder is busy (that is falling behind,
    /// which the next step notices), reading the tap meanwhile; an encoder
    /// that takes nothing for `wedged` is killed.
    fn send(&mut self, pcm: Vec<u8>, frame: Arc<[u8]>) {
        let Some(enc) = self.enc.as_ref() else { return };
        if let Some(a) = &enc.audio
            && !queue_audio(a, pcm)
        {
            // Dropping audio would put the sound behind the picture for
            // the rest of this encoder's run; a fresh one starts in step.
            tracing::warn!("the live encoder stopped reading audio; starting it again");
            enc.kill();
            return;
        }
        let Some(v) = enc.video.clone() else { return };
        let since = Instant::now();
        let mut frame = frame;
        loop {
            match v.try_send(frame) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => return,
                Err(TrySendError::Full(f)) => frame = f,
            }
            if self.stopped() {
                return;
            }
            if since.elapsed() >= self.tuning.wedged {
                tracing::warn!(
                    "the live encoder took no frame for {} s; starting it again",
                    self.tuning.wedged.as_secs()
                );
                if let Some(enc) = self.enc.as_ref() {
                    enc.kill();
                }
                return;
            }
            self.drain();
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Too far behind: skip to `rejoin` behind the newest audio, sound and
    /// picture alike, and step down the ladder after too many jumps.
    fn jump(&mut self, newest: u64) {
        let target = newest.saturating_sub(self.samples(self.tuning.rejoin));
        self.apply_until(target, false);
        self.drop_audio_before(target);
        self.cursor = Some(target);
        let now = Instant::now();
        while self
            .jumps
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.tuning.jump_window)
        {
            self.jumps.pop_front();
        }
        self.jumps.push_back(now);
        tracing::debug!("the live stream fell behind and skipped ahead");
        if self.jumps.len() >= self.tuning.jumps_to_step && self.rung + 1 < LADDER.len() {
            self.jumps.clear();
            self.rung += 1;
            self.shared_rung.store(self.rung, Ordering::Release);
            let profile = LADDER[self.rung];
            tracing::warn!(
                "the live stream cannot keep up; stepping down to {}",
                profile.name()
            );
            self.renderer = Renderer::new(profile.size);
            self.scene.set_capacity(self.renderer.column_capacity());
            self.last_frame = None;
            if let Some(mut enc) = self.enc.take() {
                enc.shutdown();
            }
            self.start_encoder();
            self.refresh_summary(true);
        }
    }

    fn start_encoder(&mut self) {
        self.generation += 1;
        let fifo = self.fifo_dir.join(format!("audio-{}.pcm", self.generation));
        let base = self
            .pipes
            .ts_end
            .load(Ordering::Acquire)
            .saturating_add(RESTART_GAP_MS);
        match Encoder::spawn(&self.pipes, LADDER[self.rung], &fifo, base) {
            Ok(enc) => {
                self.pid.store(enc.pid, Ordering::Release);
                self.enc = Some(enc);
                self.retry_at = None;
            }
            Err(e) => {
                tracing::error!("the live encoder could not be started: {e}");
                self.schedule_retry();
            }
        }
    }

    fn encoder_died(&mut self) {
        let Some(mut enc) = self.enc.take() else {
            return;
        };
        self.pid.store(0, Ordering::Release);
        let ran = enc.started.elapsed();
        let tail = enc.shutdown();
        let last = tail.last().map_or(String::new(), |l| format!(": {l}"));
        tracing::warn!(
            "the live encoder exited after {} s{last}; starting it again",
            ran.as_secs()
        );
        if ran >= self.tuning.reset_after {
            self.backoff = self.tuning.backoff_min;
        }
        self.schedule_retry();
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(self.tuning.backoff_max);
    }

    fn refresh_summary(&mut self, now: bool) {
        if !now && self.last_summary.elapsed() < SUMMARY_EVERY {
            return;
        }
        self.last_summary = Instant::now();
        let s = summary_of(self.rung, &self.pipes.publishers);
        let mut slot = lock(&self.slot);
        if *slot != s {
            *slot = s;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::demo;
    use crate::ffmpeg::ffmpeg_for_tests;
    use crate::ffmpeg::tests::Scratch;
    use crate::rtmp::tests::{Fake, Got, Mode as FakeMode, dest, quick, ts_of};
    use crate::rtmp::{DestState, IngestUrl};
    use crate::tap::{LiveTap, live_tap};
    use crate::video::tests::describe;

    fn info() -> SessionInfo {
        SessionInfo::from_meta(&demo::meta("Synthetic net", "M17-XXX C"), &[]).unwrap()
    }

    fn quick_live() -> LiveTuning {
        LiveTuning {
            backoff_min: Duration::from_millis(200),
            backoff_max: Duration::from_secs(2),
            publisher: quick(Duration::from_secs(2)),
            ..LiveTuning::default()
        }
    }

    /// Send `secs` of a session into `tap` at real time: 20 ms ticks at
    /// 8 kHz, with these overs (`start_ms`, `duration_ms`); silence between.
    fn feed(tap: LiveTap, secs: u64, overs: &'static [(u64, u64)]) -> JoinHandle<()> {
        thread::spawn(move || {
            let start = Instant::now();
            for n in 0..secs * 50 {
                let t = n * 20;
                for (i, &(from, len)) in overs.iter().enumerate() {
                    let seq = u32::try_from(i + 1).unwrap();
                    if t == from {
                        let talker = ["N0CALL", "KX0AAA"][i % 2].to_owned();
                        tap.send(TapMsg::OverStart {
                            seq,
                            talker: Some(talker),
                            offset_ms: t,
                        });
                    }
                    if t == from + len {
                        tap.send(TapMsg::OverEnd {
                            seq,
                            duration_ms: len,
                        });
                    }
                }
                let over = overs
                    .iter()
                    .position(|&(from, len)| (from..from + len).contains(&t));
                let samples: Box<[i16]> = (0..160u64)
                    .map(|k| over.map_or(0, |i| demo::voice(u32::try_from(i).unwrap(), t * 8 + k)))
                    .collect();
                tap.send(TapMsg::Audio {
                    offset_ms: t,
                    samples,
                });
                let next = start + Duration::from_millis(t + 20);
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        })
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only asks whether the process exists.
        unsafe { libc::kill(i32::try_from(pid).unwrap(), 0) == 0 }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn audio_queue_is_bounded_and_never_waits() {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE);
        let t = Instant::now();
        for _ in 0..AUDIO_QUEUE {
            assert!(queue_audio(&tx, vec![0; 320]));
        }
        // Full: refused at once, and the caller restarts the encoder.
        assert!(!queue_audio(&tx, vec![0; 320]));
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert_eq!(rx.try_iter().count(), AUDIO_QUEUE);
        // A writer that is gone is not a reason to restart.
        drop(rx);
        assert!(queue_audio(&tx, vec![0; 320]));
    }

    #[test]
    fn live_streams_into_a_local_ffmpeg() {
        let Some(ff) = ffmpeg_for_tests() else { return };
        let s = Scratch::new("live-ff");
        let port = free_port();
        let out = s.0.join("out.flv");
        let url = format!("rtmp://127.0.0.1:{port}/live2");
        // The "destination": ffmpeg listening on loopback, copying what it
        // gets into a file. Its stderr would name the stream key; dropped.
        let mut listener = Child::spawn(
            Command::new(&ff.path)
                .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
                .args(["-listen", "1", "-i", &url, "-c", "copy", "-f", "flv"])
                .arg(&out)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(500));
        let (tap, rx) = live_tap(crate::tap::LIVE_TAP_CAPACITY);
        let live = LiveStream::start_with(
            info(),
            ff.clone(),
            vec![dest(IngestUrl::parse(&url).unwrap())],
            rx,
            quick_live(),
        )
        .unwrap();
        feed(tap, 10, &[(1_000, 3_000), (5_000, 3_000)])
            .join()
            .unwrap();
        thread::sleep(Duration::from_millis(500));
        let summary = live.stop();
        assert_eq!(summary.profile, "1080p30");
        assert_eq!(
            summary.destinations[0].state,
            DestState::Live,
            "{summary:?}"
        );
        let until = Instant::now() + Duration::from_secs(10);
        while listener.try_wait().unwrap().is_none() && Instant::now() < until {
            thread::sleep(Duration::from_millis(50));
        }
        drop(listener);
        let (secs, video, audio) = describe(&ff, &out);
        assert!(secs >= 8.0, "{secs} s");
        assert!(
            video.contains("h264") && video.contains("1920x1080"),
            "{video}"
        );
        assert!(audio.contains("aac"), "{audio}");
    }

    #[test]
    fn falling_behind_steps_down_the_ladder() {
        let Some(ff) = ffmpeg_for_tests() else { return };
        let fake = Fake::start(FakeMode::Accept);
        let (tap, rx) = live_tap(crate::tap::LIVE_TAP_CAPACITY);
        let tuning = LiveTuning {
            behind: Duration::from_millis(600),
            rejoin: Duration::from_millis(150),
            // Ten frames a second at most: too slow for 30 or 15.
            slow_frame: Some(Duration::from_millis(100)),
            ..quick_live()
        };
        let live = LiveStream::start_with(info(), ff, vec![dest(fake.url())], rx, tuning).unwrap();
        let feeder = feed(tap, 16, &[(500, 6_000), (8_000, 6_000)]);
        let mut seen: Vec<String> = Vec::new();
        while !feeder.is_finished() {
            let p = live.summary().profile;
            if seen.last() != Some(&p) {
                seen.push(p);
            }
            thread::sleep(Duration::from_millis(20));
        }
        feeder.join().unwrap();
        let summary = live.stop();
        assert_eq!(seen, ["1080p30", "1080p15", "720p15"], "never back up");
        assert_eq!(summary.profile, "720p15");
        let conns = fake.conns();
        assert_eq!(conns.len(), 1, "one session across both restarts");
        let last = |video: bool| {
            conns[0]
                .got
                .iter()
                .filter(|g| match g {
                    Got::Video { seq, .. } => video && !seq,
                    Got::Audio { seq, .. } => !video && !seq,
                })
                .map(|g| ts_of(*g))
                .max()
                .unwrap()
        };
        let (v, a) = (last(true), last(false));
        assert!(v.abs_diff(a) < 200, "video ends at {v} ms, audio at {a} ms");
    }

    #[test]
    fn stop_leaves_no_ffmpeg_running() {
        let Some(ff) = ffmpeg_for_tests() else { return };
        let fake = Fake::start(FakeMode::Accept);
        let (tap, rx) = live_tap(crate::tap::LIVE_TAP_CAPACITY);
        let live =
            LiveStream::start_with(info(), ff, vec![dest(fake.url())], rx, quick_live()).unwrap();
        let feeder = feed(tap, 2, &[(200, 1_000)]);
        thread::sleep(Duration::from_millis(1_000));
        let pid = live.encoder_pid();
        let dir = live.fifo_dir().to_path_buf();
        assert_ne!(pid, 0);
        assert!(alive(pid));
        assert!(dir.join("audio-1.pcm").exists(), "the FIFO is there");
        // Stopped mid-feed, as SIGTERM would.
        live.stop();
        assert!(!alive(pid), "ffmpeg {pid} is still running");
        assert!(!dir.exists(), "{} is left", dir.display());
        feeder.join().unwrap();
    }

    #[test]
    fn encoder_restart_keeps_publishers_connected() {
        let Some(ff) = ffmpeg_for_tests() else { return };
        let fake = Fake::start(FakeMode::Accept);
        let (tap, rx) = live_tap(crate::tap::LIVE_TAP_CAPACITY);
        let live =
            LiveStream::start_with(info(), ff, vec![dest(fake.url())], rx, quick_live()).unwrap();
        let feeder = feed(tap, 9, &[(500, 3_000), (5_000, 3_000)]);
        let wait_for = |what: &str, done: &dyn Fn() -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while !done() {
                assert!(Instant::now() < until, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(20));
            }
        };
        let keys = || {
            fake.conns().first().map_or(0, |c| {
                c.got
                    .iter()
                    .filter(|g| matches!(g, Got::Video { key: true, .. }))
                    .count()
            })
        };
        wait_for("two key frames", &|| keys() >= 2);
        let first = live.encoder_pid();
        // SAFETY: `first` is the encoder this test started.
        unsafe { libc::kill(i32::try_from(first).unwrap(), libc::SIGKILL) };
        wait_for("a new encoder", &|| {
            let p = live.encoder_pid();
            p != 0 && p != first
        });
        let before = keys();
        wait_for("key frames from the new encoder", &|| keys() >= before + 2);
        feeder.join().unwrap();
        live.stop();
        let conns = fake.conns();
        assert_eq!(conns.len(), 1, "the publisher never reconnected");
        let ts: Vec<u32> = conns[0].got.iter().map(|g| ts_of(*g)).collect();
        assert!(
            ts.windows(2).all(|w| w[0] <= w[1]),
            "timestamps went backwards: {ts:?}"
        );
        let seqs = conns[0]
            .got
            .iter()
            .filter(|g| matches!(g, Got::Video { seq: true, .. }))
            .count();
        assert!(seqs >= 2, "the new encoder's sequence header went out");
    }
}
