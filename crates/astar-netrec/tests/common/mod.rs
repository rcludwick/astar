// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What the end-to-end tests share: a real session, in real time, with the
//! test's far talkers running beside it, and readers for what it wrote.

// Each test binary compiles its own copy and uses only part of it.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use astar_netarchive::{LogKind, LogRow, SessionMeta, read_rows};
use astar_netrec::config::{NetCfg, Target, Visibility};
use astar_netrec::runner::{NetAdapter, RealClock, StationAdapter, run_session};
use astar_netrec::schedule::Window;
use astar_netrec::session::SessionRecorder;
use astar_station::DstarVocoder;

/// Quieter than this, a file holds no voice. The test tone decodes to about
/// 900 RMS through Codec 2, which is its own level with no conference in the
/// way, and about 5800 through the D-Star vocoder.
pub const SILENCE_RMS: f64 = 300.0;

/// A folder for one test's archive, removed when the test ends.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "netrec-e2e-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("make scratch folder");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A net on `target`, as the scheduler would hand it over.
pub fn net(target: Target) -> NetCfg {
    NetCfg {
        name: "End to end net".to_string(),
        target,
        when: "Tue 19:00".to_string(),
        duration: Duration::from_secs(60),
        tz: jiff::tz::TimeZone::UTC,
        visibility: Visibility::Private,
        my_side: false,
        video: false,
        youtube_live: false,
        youtube_upload: false,
    }
}

/// The running session, as the far talkers see it.
pub struct Control<'a> {
    started: Instant,
    stop: &'a AtomicBool,
}

impl Control<'_> {
    /// Sleep until `secs` into the session.
    pub fn at(&self, secs: f64) {
        let due = self.started + Duration::from_secs_f64(secs);
        thread::sleep(due.saturating_duration_since(Instant::now()));
    }

    /// End the session now, as a shutdown would.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Stops the session if the script panics, so a failure shows at once rather
/// than after the whole window.
struct StopOnPanic<'a>(&'a AtomicBool);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.store(true, Ordering::Release);
        }
    }
}

/// What a finished session left behind.
pub struct Recorded {
    pub dir: PathBuf,
    pub meta: SessionMeta,
    pub rows: Vec<LogRow>,
    pub rate: u32,
}

/// Record `net` for `secs` seconds with no pre-roll, in real time, while
/// `script` plays the far end.
pub fn record(
    root: &Path,
    net: &NetCfg,
    secs: i64,
    script: impl FnOnce(&Control<'_>) + Send,
) -> Recorded {
    let mut adapter = StationAdapter::new(net, "N0CALL", 'R', DstarVocoder::Software, &[])
        .expect("build the listener");
    let rate = adapter.rate();
    let start = jiff::Timestamp::now();
    let end = start
        .checked_add(jiff::SignedDuration::from_secs(secs))
        .expect("a test-sized window");
    let window = Window {
        net: 0,
        start,
        end,
        join_at: start,
        leave_at: end,
    };
    let rec = SessionRecorder::open(root, net, &window, rate, start.to_zoned(net.tz.clone()))
        .expect("open the session");
    let dir = rec.dir().to_path_buf();
    let stop = AtomicBool::new(false);
    let control = Control {
        started: Instant::now(),
        stop: &stop,
    };
    let meta = thread::scope(|scope| {
        let session = scope.spawn(|| run_session(&mut adapter, rec, end, &RealClock, &stop));
        scope.spawn(|| {
            let _guard = StopOnPanic(&stop);
            script(&control);
        });
        session.join().expect("the session thread")
    })
    .expect("the session closes");
    let rows = read_rows(&dir).expect("read the session log");
    Recorded {
        dir,
        meta,
        rows,
        rate,
    }
}

/// One `Over` row of the log.
#[derive(Debug, Clone)]
pub struct Over {
    pub talker: Option<String>,
    pub file: String,
    pub offset_ms: u64,
    pub duration_ms: u64,
}

impl Recorded {
    pub fn overs(&self) -> Vec<Over> {
        self.rows
            .iter()
            .filter_map(|row| match &row.kind {
                LogKind::Over {
                    talker,
                    file,
                    offset_ms,
                    duration_ms,
                    ..
                } => Some(Over {
                    talker: talker.clone(),
                    file: file.clone(),
                    offset_ms: *offset_ms,
                    duration_ms: *duration_ms,
                }),
                _ => None,
            })
            .collect()
    }

    pub fn talkers(&self) -> Vec<String> {
        self.overs()
            .iter()
            .map(|o| o.talker.clone().unwrap_or_default())
            .collect()
    }

    pub fn errors(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| match &row.kind {
                LogKind::Error { message } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// How many rows `pick` accepts.
    pub fn count(&self, pick: impl Fn(&LogKind) -> bool) -> usize {
        self.rows.iter().filter(|row| pick(&row.kind)).count()
    }

    pub fn checkins(&self) -> String {
        fs::read_to_string(self.dir.join("checkins.txt")).expect("read checkins.txt")
    }

    /// The samples of an over's file, after checking it holds this session's
    /// format.
    pub fn over_samples(&self, over: &Over) -> Vec<i16> {
        let (rate, samples) = decode(&self.dir.join("overs").join(&over.file));
        assert_eq!(rate, self.rate, "{} is at the session rate", over.file);
        samples
    }

    /// The samples of `session.flac`.
    pub fn session_samples(&self) -> Vec<i16> {
        let (rate, samples) = decode(&self.dir.join("session.flac"));
        assert_eq!(rate, self.rate, "session.flac is at the session rate");
        samples
    }

    /// The part of `samples` from `from_ms` to `to_ms` on the timeline.
    pub fn slice<'s>(&self, samples: &'s [i16], from_ms: u64, to_ms: u64) -> &'s [i16] {
        let index = |ms: u64| {
            let at = ms * u64::from(self.rate) / 1000;
            usize::try_from(at)
                .expect("a test-sized offset")
                .min(samples.len())
        };
        &samples[index(from_ms)..index(to_ms)]
    }
}

/// Decode a FLAC file to its rate and samples.
pub fn decode(path: &Path) -> (u32, Vec<i16>) {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| panic!("{} does not open: {e}", path.display()));
    let rate = reader.streaminfo().sample_rate;
    let samples = reader
        .samples()
        .map(|s| i16::try_from(s.expect("a whole FLAC stream")).expect("16-bit samples"))
        .collect();
    (rate, samples)
}

pub fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s).powi(2)).sum();
    (sum / f64::from(u32::try_from(samples.len()).expect("a test-sized file"))).sqrt()
}

/// One 20 ms frame of a 1 kHz tone at 8 kHz. `n` carries the phase.
#[allow(clippy::cast_possible_truncation)] // amplitude 8000 < i16::MAX
pub fn tone_frame(n: &mut u32) -> [i16; 160] {
    let mut pcm = [0i16; 160];
    for sample in &mut pcm {
        let t = f64::from(*n) / 8_000.0;
        *sample = (8_000.0 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()) as i16;
        *n += 1;
    }
    pcm
}

/// Wait for `pred`, up to `timeout`.
pub fn wait_for(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    pred()
}

/// The checks every two-talker session must pass, on either network. The
/// script starts `first_talker` 2 s in for 3 s, and `second_talker` for 2 s
/// after a 2 s gap, in a 20 s window.
pub fn assert_two_talkers(run: &Recorded, first_talker: &str, second_talker: &str) {
    assert_eq!(run.errors(), Vec::<String>::new(), "no error rows");
    assert_eq!(
        run.talkers(),
        [first_talker, second_talker],
        "rows: {:?}",
        run.rows
    );
    assert_eq!(run.meta.overs, 2, "session.json agrees with the log");
    assert_eq!(run.meta.rejoins, 0);
    assert_eq!(
        run.meta.dropped_frames, 0,
        "nothing was lost to a slow consumer"
    );
    assert!(
        matches!(run.meta.ended, Some(astar_netarchive::Ended::Window)),
        "ended {:?}",
        run.meta.ended
    );
    assert_eq!(run.checkins(), format!("{first_talker}\n{second_talker}\n"));

    let on_disk = fs::read_dir(run.dir.join("overs"))
        .expect("read overs/")
        .count();
    assert_eq!(on_disk, 2, "two over files");
    let csv = fs::read_to_string(run.dir.join("log.csv")).expect("read log.csv");
    assert_eq!(csv.lines().count(), 3, "a header and two overs: {csv}");

    let overs = run.overs();
    for ((over, voice_ms), began_ms) in overs.iter().zip([3_000u64, 2_000]).zip([2_000u64, 7_000]) {
        assert!(
            over.file.contains(over.talker.as_deref().unwrap_or("?")),
            "{over:?} names its talker"
        );
        assert!(
            (began_ms..=began_ms + 500).contains(&over.offset_ms),
            "{over:?} should start {began_ms} ms into the session"
        );
        // The voice, plus up to the splitter's 1 s tail.
        assert!(
            (voice_ms - 300..=voice_ms + 1_400).contains(&over.duration_ms),
            "{over:?} should hold {voice_ms} ms of voice and its tail"
        );
        let samples = run.over_samples(over);
        let want = over.duration_ms * u64::from(run.rate) / 1000;
        assert_eq!(
            samples.len() as u64,
            want,
            "{over:?} is as long as its row says"
        );
        let level = rms(&samples);
        assert!(level > SILENCE_RMS, "{over:?} is silent: rms {level}");
    }

    // The whole-session file: a minute of the file is a minute of the net.
    let whole = run.session_samples();
    let want = 20 * run.rate as usize;
    let slack = run.rate as usize / 10;
    assert!(
        whole.len().abs_diff(want) <= slack,
        "session.flac is {} samples, wanted {want}",
        whole.len()
    );
    // Each talker is on the timeline where the log says, and the gap between
    // them is silence.
    for (over, voice_ms) in overs.iter().zip([3_000u64, 2_000]) {
        let voice = run.slice(&whole, over.offset_ms, over.offset_ms + voice_ms - 500);
        let level = rms(voice);
        assert!(
            level > SILENCE_RMS,
            "session.flac is silent under {over:?}: rms {level}"
        );
    }
    let gap = run.slice(&whole, overs[1].offset_ms - 1_000, overs[1].offset_ms - 200);
    assert!(rms(gap) < 1.0, "the gap holds sound: rms {}", rms(gap));
}
