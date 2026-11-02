// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The recorder thread: one 20 ms clock that drains the taps, aligns the
//! two directions, writes them through the layout writer and hands codec
//! frames to the frame writer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::RecordError;
use crate::align::Aligner;
use crate::disk::{check_free_with, free_bytes};
use crate::frame_writer::FrameWriter;
use crate::frames::FrameEvent;
use crate::layout::{Layout, LayoutWriter, is_already_exists};
use crate::naming::{app_base_name, unique_base};
use crate::tap::PcmChunk;

/// How often free space is checked while recording.
pub const DISK_CHECK_EVERY: Duration = Duration::from_secs(10);
/// One tick of the recorder clock.
const TICK: Duration = Duration::from_millis(20);
const TICK_MS: u64 = 20;
const TICKS_PER_SEC: u32 = 50;
/// How often the thread checks that its files are still on disk. Writing to
/// a deleted file succeeds on Unix, so a removed folder is only noticed by
/// looking.
const PRESENCE_CHECK_EVERY: Duration = Duration::from_millis(200);

/// What to record and where. Public fields only, so a caller builds it as a
/// struct literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderConfig {
    /// Created if missing.
    pub dir: PathBuf,
    pub layout: Layout,
    /// Also write unamblify codec-frame captures under `dir/captured/`.
    pub frames: bool,
    /// The `<network> <node or reflector>` part of the file name.
    pub label: String,
    pub utc_offset_secs: i32,
    /// The rate of the heard and sent taps; a multiple of 50.
    pub sample_rate: u32,
    /// astar's version, for capture rows.
    pub version: String,
}

/// The receiving ends of the taps.
///
/// Every receiver is drained with `try_recv` on the recorder's own clock
/// and never parked in a blocking receive (see [`crate::tap::tap_channel`]).
pub struct RecorderInputs {
    pub heard: Receiver<PcmChunk>,
    pub sent: Receiver<PcmChunk>,
    pub frames: Option<Receiver<FrameEvent>>,
    /// Counters of items lost before they reached the recorder, summed into
    /// `dropped_frames`: each tap's `Tap::dropped_counter` and each slot's
    /// `TapSlot::contended_counter`. Only growth after `start` counts, so
    /// a slot's lifetime counter can be passed as it is.
    pub drop_counters: Vec<Arc<AtomicU64>>,
}

/// What the recorder is doing, as the app and the server show it. Crosses
/// the C ABI as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordState {
    pub recording: bool,
    pub elapsed_ms: u64,
    pub files: Vec<PathBuf>,
    /// Audio chunks, frames or events lost anywhere between the taps and
    /// the file: tap drops and contention, chunks at the wrong rate, and
    /// frames the aligner threw away.
    pub dropped_frames: u64,
    /// Why recording stopped by itself, if it did.
    pub error: Option<String>,
    pub layout: Layout,
}

impl RecordState {
    /// Nothing recorded yet.
    #[must_use]
    pub fn idle() -> RecordState {
        RecordState {
            recording: false,
            elapsed_ms: 0,
            files: Vec::new(),
            dropped_frames: 0,
            error: None,
            layout: Layout::default(),
        }
    }
}

/// Test seams, kept off [`RecorderConfig`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seams {
    pub free_space: fn(&Path) -> Option<u64>,
    pub disk_check_every: Duration,
    pub first_tick_after: Duration,
}

impl Default for Seams {
    fn default() -> Self {
        Seams {
            free_space: free_bytes,
            disk_check_every: DISK_CHECK_EVERY,
            first_tick_after: TICK,
        }
    }
}

/// A running recording. Dropping it stops it, as [`Recorder::stop`] does.
pub struct Recorder {
    shared: Arc<Mutex<RecordState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Opens the layout's files at the first free `base`, `base 2`, …. The
/// files are opened with `create_new`, so a name another writer took
/// between [`unique_base`] looking and the open is an `AlreadyExists`; the
/// next free base is tried then, up to 100 times.
fn create_unique(
    dir: &Path,
    base: &str,
    layout: Layout,
    rate: u32,
) -> Result<LayoutWriter, RecordError> {
    let mut last = None;
    for _ in 0..100 {
        let path = unique_base(dir, base, layout);
        match LayoutWriter::create(&path, layout, rate) {
            Err(e) if is_already_exists(&e) => last = Some(e),
            other => return other,
        }
    }
    Err(last.expect("the loop ran"))
}

fn lock(m: &Mutex<RecordState>) -> std::sync::MutexGuard<'_, RecordState> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Recorder {
    /// Creates the folder, checks free space, opens the file(s) and starts
    /// the thread.
    pub fn start(cfg: RecorderConfig, inputs: RecorderInputs) -> Result<Recorder, RecordError> {
        Self::start_with(cfg, inputs, Seams::default())
    }

    pub(crate) fn start_with(
        cfg: RecorderConfig,
        inputs: RecorderInputs,
        seams: Seams,
    ) -> Result<Recorder, RecordError> {
        if cfg.sample_rate == 0 || !cfg.sample_rate.is_multiple_of(TICKS_PER_SEC) {
            return Err(RecordError::Encode(format!(
                "unsupported sample rate {}",
                cfg.sample_rate
            )));
        }
        std::fs::create_dir_all(&cfg.dir)
            .map_err(|e| RecordError::Folder(format!("{}: {e}", cfg.dir.display())))?;
        check_free_with(seams.free_space, &cfg.dir)?;
        let base = app_base_name(SystemTime::now(), cfg.utc_offset_secs, &cfg.label);
        let writer = create_unique(&cfg.dir, &base, cfg.layout, cfg.sample_rate)?;
        let files = writer.paths();
        let frame = (cfg.sample_rate / TICKS_PER_SEC) as usize;
        let shared = Arc::new(Mutex::new(RecordState {
            recording: true,
            files: files.clone(),
            layout: cfg.layout,
            ..RecordState::idle()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            baselines: inputs
                .drop_counters
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect(),
            frame_writer: cfg.frames.then(|| FrameWriter::new(&cfg.dir, &cfg.version)),
            sample_rate: cfg.sample_rate,
            dir: cfg.dir,
            inputs,
            aligner: Aligner::new(frame),
            writer: Some(writer),
            files: files.clone(),
            ticks: 0,
            off_rate: 0,
            capture_dropped: 0,
            capture_failed: false,
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
            seams,
        };
        let on_panic = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("astar-recorder".to_owned())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()));
                if r.is_err() {
                    let mut s = lock(&on_panic);
                    s.recording = false;
                    s.error
                        .get_or_insert_with(|| "the recorder stopped unexpectedly".to_owned());
                }
            });
        match spawned {
            Ok(t) => Ok(Recorder {
                shared,
                stop,
                thread: Some(t),
            }),
            Err(e) => {
                for p in &files {
                    let _ = std::fs::remove_file(p);
                }
                Err(e.into())
            }
        }
    }

    /// The live state; after the recorder stopped itself, its final state
    /// with the reason.
    #[must_use]
    pub fn state(&self) -> RecordState {
        lock(&self.shared).clone()
    }

    /// Stops, finishes the file(s) and returns the final state. A recording
    /// stopped before its first tick leaves no file.
    #[must_use]
    pub fn stop(mut self) -> RecordState {
        self.shutdown();
        self.state()
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Moves a tap's chunks into one aligner lane; chunks at another rate are
/// counted and thrown away. `try_recv` only, never a blocking receive.
fn drain_pcm(rx: &Receiver<PcmChunk>, rate: u32, mut push: impl FnMut(&[i16])) -> u64 {
    let mut off_rate = 0;
    while let Ok(c) = rx.try_recv() {
        if c.rate == rate {
            push(&c.samples);
        } else {
            off_rate += 1;
        }
    }
    off_rate
}

struct Worker {
    dir: PathBuf,
    sample_rate: u32,
    inputs: RecorderInputs,
    baselines: Vec<u64>,
    aligner: Aligner,
    writer: Option<LayoutWriter>,
    frame_writer: Option<FrameWriter>,
    files: Vec<PathBuf>,
    ticks: u64,
    off_rate: u64,
    /// Frame events thrown away after the frame writer failed.
    capture_dropped: u64,
    /// The frame writer failed; later events are counted, not written.
    capture_failed: bool,
    shared: Arc<Mutex<RecordState>>,
    stop: Arc<AtomicBool>,
    seams: Seams,
}

impl Worker {
    /// The thread loop.
    ///
    /// Invariant: the tap receivers are only ever drained with `try_recv`.
    /// The loop never parks in `recv`, `recv_timeout` or a receiver's
    /// blocking iterator, because a parked receiver makes the audio thread's
    /// `try_send` take the channel's lock and futex-wake it. Between ticks
    /// the thread sleeps in `park_timeout` until the next deadline, which no
    /// channel touches; `Recorder::stop` unparks it. Each deadline is the
    /// previous one plus 20 ms, never "now + 20 ms", so the clock does not
    /// drift. `Instant` does not advance while the machine sleeps, so the
    /// only catch-up is after a stall in this process (a `SIGSTOP`, a slow
    /// `fsync`): the missed ticks are then written back to back, so the file
    /// keeps wall-clock length. (`the_recorder_never_parks_in_a_blocking_receive` checks the
    /// source for blocking receives.)
    fn run(mut self) {
        let mut deadline = Instant::now() + self.seams.first_tick_after;
        let mut next_disk = deadline + self.seams.disk_check_every;
        let mut next_presence = deadline + PRESENCE_CHECK_EVERY;
        let error = loop {
            if !self.sleep_until(deadline) {
                break None;
            }
            let now = Instant::now();
            if let Err(e) = self.tick(now) {
                break Some(e);
            }
            deadline += TICK;
            if now >= next_presence {
                next_presence = now + PRESENCE_CHECK_EVERY;
                if let Some(gone) = self.files.iter().find(|p| !p.exists()) {
                    break Some(RecordError::Folder(format!("{} is gone", gone.display())));
                }
            }
            if now >= next_disk {
                next_disk = now + self.seams.disk_check_every;
                if let Err(e) = check_free_with(self.seams.free_space, &self.dir) {
                    break Some(e);
                }
            }
        };
        self.finish(error);
    }

    /// Sleeps until `deadline`; false if a stop was asked for first.
    fn sleep_until(&self, deadline: Instant) -> bool {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            std::thread::park_timeout(deadline - now);
        }
    }

    fn tick(&mut self, now: Instant) -> Result<(), RecordError> {
        let rate = self.sample_rate;
        self.off_rate += drain_pcm(&self.inputs.heard, rate, |s| self.aligner.push_rx(s));
        self.off_rate += drain_pcm(&self.inputs.sent, rate, |s| self.aligner.push_tx(s));
        let (rx, tx) = self.aligner.tick();
        if let Some(w) = self.writer.as_mut() {
            w.write_tick(&rx, &tx)?;
        }
        self.ticks += 1;
        let mut failed = None;
        if let Some(frames) = &self.inputs.frames {
            while let Ok(ev) = frames.try_recv() {
                if failed.is_some() || self.capture_failed {
                    self.capture_dropped += 1;
                } else if let Some(fw) = self.frame_writer.as_mut()
                    && let Err(e) = fw.handle(ev, now)
                {
                    self.capture_dropped += 1;
                    failed = Some(e);
                }
            }
        }
        if let Some(e) = failed {
            self.drop_capture(&e);
        }
        if let Some(fw) = self.frame_writer.as_mut()
            && let Err(e) = fw.poll(now)
        {
            self.drop_capture(&e);
        }
        let (elapsed, dropped) = (self.elapsed_ms(), self.dropped());
        let mut s = lock(&self.shared);
        s.elapsed_ms = elapsed;
        s.dropped_frames = dropped;
        Ok(())
    }

    /// A codec-frame capture failure ends the capture, never the audio
    /// recording: the writer is sealed as far as it can be and dropped, and
    /// later frame events are counted into `dropped_frames`. Logged once.
    fn drop_capture(&mut self, e: &RecordError) {
        tracing::warn!(
            folder = %self.dir.join("captured").display(),
            error = %e,
            "codec-frame capture failed; audio recording continues without it"
        );
        if let Some(fw) = self.frame_writer.take() {
            let _ = fw.finish();
        }
        self.capture_failed = true;
    }

    fn elapsed_ms(&self) -> u64 {
        self.ticks * TICK_MS
    }

    fn dropped(&self) -> u64 {
        let taps: u64 = self
            .inputs
            .drop_counters
            .iter()
            .zip(&self.baselines)
            .map(|(c, &base)| c.load(Ordering::Relaxed).saturating_sub(base))
            .sum();
        taps + self.off_rate + self.capture_dropped + self.aligner.dropped()
    }

    /// Finishes the files (removing them if no tick was written) and
    /// publishes the final state.
    fn finish(mut self, error: Option<RecordError>) {
        let mut error = error.map(|e| e.to_string());
        if let Some(fw) = self.frame_writer.take()
            && let Err(e) = fw.finish()
        {
            tracing::warn!(
                folder = %self.dir.join("captured").display(),
                error = %e,
                "codec-frame capture could not be finished"
            );
        }
        let finished = self.writer.take().map(LayoutWriter::finish);
        let files = if self.ticks == 0 {
            for p in &self.files {
                let _ = std::fs::remove_file(p);
            }
            Vec::new()
        } else {
            if let Some(Err(e)) = finished {
                error.get_or_insert(e.to_string());
            }
            // A file whose folder vanished is not listed.
            self.files.iter().filter(|p| p.exists()).cloned().collect()
        };
        let (elapsed, dropped) = (self.elapsed_ms(), self.dropped());
        let mut s = lock(&self.shared);
        s.recording = false;
        s.elapsed_ms = elapsed;
        s.dropped_frames = dropped;
        s.files = files;
        s.error = error;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::read_rows;
    use crate::frames::{CaptureMode, Dir};
    use crate::tap::{FRAME_TAP_CAPACITY, HEARD_TAP_CAPACITY, SENT_TAP_CAPACITY, Tap, tap_channel};
    use std::sync::atomic::AtomicU32;

    const RATE: u32 = 16_000;
    const FRAME: usize = 320;

    fn tdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-rec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn cfg(dir: &Path) -> RecorderConfig {
        RecorderConfig {
            dir: dir.to_path_buf(),
            layout: Layout::Stereo,
            frames: false,
            label: "AllStar 2000".to_owned(),
            utc_offset_secs: 0,
            sample_rate: RATE,
            version: "0.1.16-beta".to_owned(),
        }
    }

    struct Feed {
        heard: Tap<PcmChunk>,
        sent: Tap<PcmChunk>,
        frames: Tap<FrameEvent>,
    }

    fn inputs() -> (Feed, RecorderInputs) {
        let (heard, hr) = tap_channel(HEARD_TAP_CAPACITY);
        let (sent, sr) = tap_channel(SENT_TAP_CAPACITY);
        let (frames, fr) = tap_channel(FRAME_TAP_CAPACITY);
        let counters = vec![heard.dropped_counter(), sent.dropped_counter()];
        (
            Feed {
                heard,
                sent,
                frames,
            },
            RecorderInputs {
                heard: hr,
                sent: sr,
                frames: Some(fr),
                drop_counters: counters,
            },
        )
    }

    #[test]
    fn create_unique_takes_the_next_free_base_and_overwrites_nothing() {
        let d = tdir("unique");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("call.flac"), b"theirs").unwrap();
        let w = create_unique(&d, "call", Layout::Stereo, RATE).unwrap();
        assert_eq!(w.paths(), vec![d.join("call 2.flac")]);
        w.finish().unwrap();
        assert_eq!(std::fs::read(d.join("call.flac")).unwrap(), b"theirs");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[allow(clippy::unnecessary_wraps)] // the seam's fn-pointer type
    fn plenty(_: &Path) -> Option<u64> {
        Some(10_000_000_000)
    }

    fn seams() -> Seams {
        Seams {
            free_space: plenty,
            disk_check_every: DISK_CHECK_EVERY,
            first_tick_after: TICK,
        }
    }

    fn flacs(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        rd.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "flac"))
            .collect()
    }

    #[allow(clippy::cast_possible_truncation)]
    fn decode(p: &Path) -> (u32, u32, Option<u64>, Vec<i16>) {
        let mut r = claxon::FlacReader::open(p).unwrap();
        let si = r.streaminfo();
        let (ch, rate, total) = (si.channels, si.sample_rate, si.samples);
        (
            ch,
            rate,
            total,
            r.samples().map(|s| s.unwrap() as i16).collect(),
        )
    }

    fn wait_stopped(r: &Recorder) -> RecordState {
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            let s = r.state();
            if !s.recording || Instant::now() > until {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn start_creates_the_folder_and_one_stereo_file() {
        let d = tdir("stereo");
        assert!(!d.exists());
        let (feed, inp) = inputs();
        let r = Recorder::start_with(cfg(&d), inp, seams()).unwrap();
        assert!(d.is_dir());
        for i in 0..50i16 {
            feed.heard.offer(PcmChunk {
                rate: RATE,
                samples: vec![i + 1; FRAME],
            });
            feed.sent.offer(PcmChunk {
                rate: RATE,
                samples: vec![-(i + 1); FRAME],
            });
            std::thread::sleep(TICK);
        }
        let live = r.state();
        assert!(live.recording);
        assert!(live.elapsed_ms > 0);
        let s = r.stop();
        assert!(!s.recording);
        assert_eq!(s.error, None);
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.layout, Layout::Stereo);
        let name = s.files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.ends_with(" AllStar 2000.flac"), "{name}");
        let (ch, rate, _, pcm) = decode(&s.files[0]);
        assert_eq!((ch, rate), (2, RATE));
        assert!(
            pcm.chunks(2).any(|p| p[0] > 0 && p[1] < 0),
            "receive left, transmit right"
        );
        assert_eq!(flacs(&d).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stop_with_no_audio_leaves_no_file() {
        let d = tdir("zero");
        let (_feed, inp) = inputs();
        let r = Recorder::start_with(
            cfg(&d),
            inp,
            Seams {
                first_tick_after: Duration::from_secs(3600),
                ..seams()
            },
        )
        .unwrap();
        let started = Instant::now();
        let s = r.stop();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop must not wait for the first tick"
        );
        assert!(!s.recording);
        assert_eq!(s.elapsed_ms, 0);
        assert_eq!(s.files, Vec::<PathBuf>::new());
        assert_eq!(flacs(&d), [] as [std::path::PathBuf; 0]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_tick_with_no_audio_keeps_a_silent_file() {
        let d = tdir("silent");
        let (_feed, inp) = inputs();
        let r = Recorder::start_with(cfg(&d), inp, seams()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let s = r.stop();
        let ticks = s.elapsed_ms / 20;
        assert!(ticks >= 5, "{ticks} ticks");
        assert_eq!(s.files.len(), 1);
        let (ch, _, total, pcm) = decode(&s.files[0]);
        assert_eq!(ch, 2);
        assert_eq!(total, Some(ticks * FRAME as u64));
        assert_eq!(pcm.len() as u64, ticks * FRAME as u64 * 2);
        assert!(pcm.iter().all(|&v| v == 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    static FLIP_CALLS: AtomicU32 = AtomicU32::new(0);
    #[allow(clippy::unnecessary_wraps)] // the seam's fn-pointer type
    fn plenty_then_full(_: &Path) -> Option<u64> {
        if FLIP_CALLS.fetch_add(1, Ordering::Relaxed) == 0 {
            Some(10_000_000_000)
        } else {
            Some(1)
        }
    }

    #[test]
    fn state_after_an_internal_stop_still_answers() {
        let d = tdir("internal");
        let (_feed, inp) = inputs();
        let r = Recorder::start_with(
            cfg(&d),
            inp,
            Seams {
                free_space: plenty_then_full,
                disk_check_every: Duration::from_millis(40),
                first_tick_after: TICK,
            },
        )
        .unwrap();
        let s = wait_stopped(&r);
        assert!(!s.recording);
        assert!(s.error.is_some());
        // Asking again, and stopping after the fact, still answer the same.
        assert_eq!(r.state(), s);
        let last = r.stop();
        assert!(!last.recording);
        assert_eq!(last.error, s.error);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rate_mismatched_chunks_are_dropped_and_counted() {
        let d = tdir("rate");
        let (feed, inp) = inputs();
        let r = Recorder::start_with(cfg(&d), inp, seams()).unwrap();
        for _ in 0..5 {
            feed.heard.offer(PcmChunk {
                rate: 8000,
                samples: vec![1000; 160],
            });
            feed.sent.offer(PcmChunk {
                rate: 8000,
                samples: vec![1000; 160],
            });
        }
        std::thread::sleep(Duration::from_millis(200));
        let s = r.stop();
        assert!(s.dropped_frames >= 10, "{}", s.dropped_frames);
        let (_, _, _, pcm) = decode(&s.files[0]);
        assert!(pcm.iter().all(|&v| v == 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dropped_frames_include_tap_drops() {
        let d = tdir("drops");
        let (_feed, mut inp) = inputs();
        let contended = Arc::new(AtomicU64::new(0));
        inp.drop_counters.insert(0, Arc::clone(&contended));
        let r = Recorder::start_with(cfg(&d), inp, seams()).unwrap();
        contended.store(7, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        assert!(r.state().dropped_frames >= 7);
        let s = r.stop();
        assert!(s.dropped_frames >= 7);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn drops_from_before_the_start_are_not_counted() {
        let d = tdir("baseline");
        let (_feed, mut inp) = inputs();
        inp.drop_counters.push(Arc::new(AtomicU64::new(40)));
        let r = Recorder::start_with(cfg(&d), inp, seams()).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(r.stop().dropped_frames, 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn frames_go_under_captured_when_enabled() {
        let d = tdir("frames");
        let (feed, inp) = inputs();
        let r = Recorder::start_with(
            RecorderConfig {
                frames: true,
                ..cfg(&d)
            },
            inp,
            seams(),
        )
        .unwrap();
        feed.frames.offer(FrameEvent::Start {
            dir: Dir::Rx,
            network: "dstar",
            mode: CaptureMode::Dstar,
            talker: Some("W1ABC".to_owned()),
            at: SystemTime::now(),
            vocoder: "hardware",
        });
        for i in 0..10u8 {
            feed.frames.offer(FrameEvent::Frame {
                dir: Dir::Rx,
                bytes: vec![i; 9],
                seq: None,
            });
            feed.frames.offer(FrameEvent::Pcm {
                dir: Dir::Rx,
                pcm: [i16::from(i); 160],
            });
        }
        feed.frames.offer(FrameEvent::End { dir: Dir::Rx });
        std::thread::sleep(Duration::from_millis(150));
        let s = r.stop();
        assert_eq!(s.error, None);
        let rows = read_rows(&d.join("captured/dstar/manifest.jsonl")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].frames, 10);
        assert_eq!(rows[0].version, "0.1.16-beta");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn frames_stay_off_disk_when_disabled() {
        let d = tdir("noframes");
        let (feed, inp) = inputs();
        let r = Recorder::start_with(cfg(&d), inp, seams()).unwrap();
        feed.frames.offer(FrameEvent::Start {
            dir: Dir::Rx,
            network: "dstar",
            mode: CaptureMode::Dstar,
            talker: None,
            at: SystemTime::now(),
            vocoder: "hardware",
        });
        feed.frames.offer(FrameEvent::Frame {
            dir: Dir::Rx,
            bytes: vec![0; 9],
            seq: None,
        });
        feed.frames.offer(FrameEvent::Pcm {
            dir: Dir::Rx,
            pcm: [0; 160],
        });
        std::thread::sleep(Duration::from_millis(100));
        let _ = r.stop();
        assert!(!d.join("captured").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_capture_keeps_the_audio_recording() {
        let d = tdir("capfail");
        std::fs::create_dir_all(&d).unwrap();
        // `captured` is a file, so the frame writer cannot make its folder.
        std::fs::write(d.join("captured"), b"not a folder").unwrap();
        let (feed, inp) = inputs();
        let r = Recorder::start_with(
            RecorderConfig {
                frames: true,
                ..cfg(&d)
            },
            inp,
            seams(),
        )
        .unwrap();
        let start = || FrameEvent::Start {
            dir: Dir::Rx,
            network: "dstar",
            mode: CaptureMode::Dstar,
            talker: None,
            at: SystemTime::now(),
            vocoder: "hardware",
        };
        feed.frames.offer(start());
        std::thread::sleep(Duration::from_millis(100));
        let first = r.state();
        assert!(first.recording, "{:?}", first.error);
        assert_eq!(first.error, None);
        assert!(first.dropped_frames >= 1);
        for i in 0..5u8 {
            feed.frames.offer(FrameEvent::Frame {
                dir: Dir::Rx,
                bytes: vec![i; 9],
                seq: None,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
        let later = r.state();
        assert!(later.recording);
        assert!(later.elapsed_ms > first.elapsed_ms, "audio ticks continue");
        assert!(later.dropped_frames >= first.dropped_frames + 5);
        let s = r.stop();
        assert_eq!(s.error, None);
        assert_eq!(s.files.len(), 1);
        assert!(decode(&s.files[0]).2.is_some_and(|t| t > 0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_vanished_folder_stops_with_an_error() {
        let d = tdir("vanish");
        let (_feed, inp) = inputs();
        let r = Recorder::start_with(cfg(&d), inp, seams()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(&d).unwrap();
        let s = wait_stopped(&r);
        assert!(
            !s.recording,
            "still recording a second after the folder went"
        );
        assert!(s.error.as_deref().is_some_and(|e| !e.is_empty()));
        assert!(
            s.files.is_empty(),
            "files that no longer exist are not listed"
        );
        let _ = r.stop();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn low_disk_refuses_to_start() {
        let d = tdir("lowstart");
        let (_feed, inp) = inputs();
        let e = Recorder::start_with(
            cfg(&d),
            inp,
            Seams {
                free_space: |_| Some(1),
                ..seams()
            },
        )
        .err()
        .expect("must refuse");
        assert!(matches!(e, RecordError::DiskLow { free_bytes: 1 }), "{e:?}");
        assert_eq!(flacs(&d), [] as [std::path::PathBuf; 0]);
        let _ = std::fs::remove_dir_all(&d);
    }

    static FREE: AtomicU64 = AtomicU64::new(10_000_000_000);
    #[allow(clippy::unnecessary_wraps)] // the seam's fn-pointer type
    fn free_from_static(_: &Path) -> Option<u64> {
        Some(FREE.load(Ordering::Relaxed))
    }

    #[test]
    fn low_disk_mid_recording_stops_cleanly() {
        let d = tdir("lowmid");
        let (_feed, inp) = inputs();
        let r = Recorder::start_with(
            cfg(&d),
            inp,
            Seams {
                free_space: free_from_static,
                disk_check_every: Duration::from_millis(50),
                first_tick_after: TICK,
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(r.state().recording);
        FREE.store(1, Ordering::Relaxed);
        let s = wait_stopped(&r);
        assert!(!s.recording);
        assert_eq!(
            s.error.as_deref(),
            Some("disk has only 0 MB free; recording needs 500 MB")
        );
        // Finished by the recorder itself, before anyone calls stop.
        assert_eq!(s.files.len(), 1);
        let (_, _, total, pcm) = decode(&s.files[0]);
        assert!(total.is_some_and(|t| t > 0));
        assert_eq!(pcm.len() as u64, total.unwrap() * 2);
        let _ = r.stop();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_mono_lists_both_files() {
        let d = tdir("twomono");
        let (_feed, inp) = inputs();
        let r = Recorder::start_with(
            RecorderConfig {
                layout: Layout::TwoMono,
                ..cfg(&d)
            },
            inp,
            seams(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(80));
        let s = r.stop();
        assert_eq!(s.layout, Layout::TwoMono);
        assert_eq!(s.files.len(), 2);
        assert!(s.files.iter().all(|p| decode(p).0 == 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn record_state_json_has_the_exact_keys() {
        let s = RecordState {
            recording: true,
            elapsed_ms: 42000,
            files: vec![PathBuf::from("/r/2026-10-26 19-00-04 AllStar 2000.flac")],
            dropped_frames: 0,
            error: None,
            layout: Layout::Stereo,
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"recording":true,"elapsed_ms":42000,"files":["/r/2026-10-26 19-00-04 AllStar 2000.flac"],"dropped_frames":0,"error":null,"layout":"stereo"}"#
        );
        let idle = RecordState::idle();
        assert!(!idle.recording && idle.files.is_empty() && idle.error.is_none());
        assert_eq!(idle.layout, Layout::Stereo);
    }

    /// The thread loop's invariant, checked against this file's own source:
    /// no tap receiver is ever parked in a blocking receive.
    #[test]
    fn the_recorder_never_parks_in_a_blocking_receive() {
        let src = include_str!("recorder.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for needle in [
            [".re", "cv("].concat(),
            [".recv", "_timeout("].concat(),
            [".recv", "_deadline("].concat(),
        ] {
            assert!(!code.contains(&needle), "recorder.rs uses `{needle}`");
        }
        assert!(code.contains(".try_recv()"));
    }
}
