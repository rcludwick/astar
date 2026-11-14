// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One recorded session: the whole-window FLAC, one FLAC per over, and the logs.
//!
//! The recorder is driven one 20 ms tick at a time. Every tick adds exactly
//! one frame of samples to `session.flac` (silence when nothing arrived), so
//! the file's length is the session's length. Frames that did arrive also go
//! through [`OverSplitter`], which cuts them into overs, and each over is
//! written to its own file under `overs/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astar_netarchive::{
    Ended, LogKind, LogRow, META_FORMAT, MIN_FREE_BYTES, OverRec, SessionMeta, append_row,
    compute_peaks, net_dir_name, over_file_name, session_dir_name, write_checkins, write_log_csv,
    write_meta, write_peaks,
};
use astar_record::{FlacWriter, OverEvent, OverSplitter, SplitterConfig};

use crate::config::{NetCfg, Target, Visibility};
use crate::feed::Frame;
use crate::schedule::Window;

/// The free-space check runs every 10 s of ticks.
const DISK_CHECK_TICKS: u64 = 500;

/// Free bytes under a folder; swapped out in tests.
pub(crate) type FreeFn = fn(&Path) -> std::io::Result<u64>;

/// A talker named this soon after an over began, for an over that began
/// without one, is the over's talker (the file is renamed to say so).
const LATE_TALKER: Duration = Duration::from_secs(2);

/// What can go wrong while writing a session.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A file could not be created, written, renamed or finished.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An over's file or row failed. The whole-session recording is unharmed
    /// and the session can carry on.
    #[error("over: {0}")]
    Over(std::io::Error),
    /// The disk holding the archive has less than
    /// [`astar_netarchive::MIN_FREE_BYTES`] free.
    #[error("disk nearly full")]
    LowDisk,
}

fn io_err(e: impl std::fmt::Display) -> SessionError {
    SessionError::Io(std::io::Error::other(e.to_string()))
}

/// An over whose file is being written.
struct OpenOver {
    seq: u32,
    writer: FlacWriter,
    path: PathBuf,
    name: String,
    start: jiff::Zoned,
    offset_ms: u64,
    talker: Option<String>,
    started_at: Instant,
    /// Samples sent to the writer, counted here so a failed finish still
    /// leaves a duration.
    samples: u64,
}

/// The files of one session.
pub struct SessionRecorder {
    dir: PathBuf,
    meta: SessionMeta,
    rate: u32,
    frame_samples: usize,
    whole: Option<FlacWriter>,
    ticks: u64,
    splitter: OverSplitter,
    open: Option<OpenOver>,
    next_seq: u32,
    overs: Vec<OverRec>,
    tz: jiff::tz::TimeZone,
    /// Set once an over failure has its `Error` row, so a full disk logs one
    /// row and not fifty a second.
    over_error_logged: bool,
    root: PathBuf,
    free: FreeFn,
    /// Set once an unreadable free-space check has been logged.
    free_warned: bool,
    /// Makes the next whole-session write fail.
    #[cfg(test)]
    pub(crate) fail_whole: bool,
}

impl SessionRecorder {
    /// Create the session folder, `overs/`, `session.flac` and the first
    /// `session.json` (with `ended: None`).
    ///
    /// # Errors
    /// [`SessionError::Io`] if any of them cannot be made.
    #[allow(clippy::needless_pass_by_value)] // the agreed signature takes the clock by value
    pub fn open(
        root: &Path,
        net: &NetCfg,
        window: &Window,
        rate: u32,
        now: jiff::Zoned,
    ) -> Result<Self, SessionError> {
        Self::open_with(root, net, window, rate, now, astar_netarchive::free_bytes)
    }

    /// [`open`](Self::open) with the free-space reading supplied.
    ///
    /// # Errors
    /// [`SessionError::LowDisk`] below the free-space threshold, else as
    /// [`open`](Self::open).
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn open_with(
        root: &Path,
        net: &NetCfg,
        window: &Window,
        rate: u32,
        now: jiff::Zoned,
        free: FreeFn,
    ) -> Result<Self, SessionError> {
        let net_dir = root.join(net_dir_name(&net.name));
        fs::create_dir_all(&net_dir)?;
        // Measured once the folder exists, so a first run on a missing root
        // reads the real volume. A failed reading never blocks a recording,
        // but it is logged.
        match free(root) {
            Ok(b) if b < MIN_FREE_BYTES => return Err(SessionError::LowDisk),
            Ok(_) => {}
            Err(e) => tracing::warn!("cannot read free disk space: {e}"),
        }
        let base = session_dir_name(&window.join_at.to_zoned(net.tz.clone()));
        let mut n = 1u32;
        let dir = loop {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base} ({n})")
            };
            let dir = net_dir.join(name);
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e.into()),
            }
        };
        fs::create_dir(dir.join("overs"))?;
        let whole = FlacWriter::create_new(&dir.join("session.flac"), rate, 1).map_err(io_err)?;
        let (network, target) = match &net.target {
            Target::M17 { host, port, module } => ("m17", format!("{host}:{port} {module}")),
            Target::Dstar {
                host,
                port,
                module,
                reflector,
            } => (
                "dstar",
                format!(
                    "{}{host}:{port} {module}",
                    reflector
                        .as_ref()
                        .map_or(String::new(), |r| format!("{r} "))
                ),
            ),
        };
        let zoned = |t: jiff::Timestamp| {
            t.to_zoned(net.tz.clone())
                .strftime("%Y-%m-%dT%H:%M:%S%:z")
                .to_string()
        };
        let meta = SessionMeta {
            format: META_FORMAT,
            net: net.name.clone(),
            network: network.to_owned(),
            target,
            window_start: zoned(window.start),
            window_end: zoned(window.end),
            visibility: match net.visibility {
                Visibility::Private => "private",
                Visibility::Public => "public",
            }
            .to_owned(),
            my_side: net.my_side,
            sample_rate: rate,
            started_at: now.timestamp().to_string(),
            ended_at: None,
            ended: None,
            rejoins: 0,
            dropped_frames: 0,
            overs: 0,
            astar_version: env!("CARGO_PKG_VERSION").to_owned(),
            my_callsigns: net.my_callsigns.clone(),
        };
        write_meta(&dir, &meta)?;
        let frame_samples = (rate / 50) as usize;
        Ok(Self {
            dir,
            meta,
            rate,
            frame_samples,
            whole: Some(whole),
            ticks: 0,
            splitter: OverSplitter::new(SplitterConfig::default(), frame_samples),
            open: None,
            next_seq: 1,
            overs: Vec::new(),
            tz: net.tz.clone(),
            over_error_logged: false,
            root: root.to_owned(),
            free,
            free_warned: false,
            #[cfg(test)]
            fail_whole: false,
        })
    }

    /// Add one 20 ms tick. `frame` is the audio that arrived (if any) and
    /// `talker` who the network says is talking now.
    ///
    /// # Errors
    /// [`SessionError::Over`] if only an over failed, which leaves the session
    /// able to carry on, or [`SessionError::Io`] if the whole-session file
    /// failed. The first over failure gets an `Error` row, and so does every
    /// other failure.
    #[allow(clippy::needless_pass_by_value)] // the agreed signature takes the clock by value
    pub fn tick(
        &mut self,
        frame: Option<&Frame>,
        talker: Option<&str>,
        at: Instant,
        now: jiff::Zoned,
    ) -> Result<(), SessionError> {
        let mut result = self.tick_inner(frame, talker, at, &now);
        if self.ticks.is_multiple_of(DISK_CHECK_TICKS)
            && !matches!(result, Err(SessionError::Io(_)))
        {
            match (self.free)(&self.root) {
                Ok(b) if b < MIN_FREE_BYTES => result = Err(SessionError::LowDisk),
                Ok(_) => {}
                Err(e) => {
                    if !self.free_warned {
                        self.free_warned = true;
                        tracing::warn!("cannot read free disk space: {e}");
                    }
                }
            }
        }
        if matches!(result, Err(SessionError::Over(_))) {
            if self.over_error_logged {
                return result;
            }
            self.over_error_logged = true;
        }
        self.logged(result, &now)
    }

    fn tick_inner(
        &mut self,
        frame: Option<&Frame>,
        talker: Option<&str>,
        at: Instant,
        now: &jiff::Zoned,
    ) -> Result<(), SessionError> {
        debug_assert!(frame.is_none_or(|f| f.rate == self.rate));
        let mut fixed = vec![0i16; self.frame_samples];
        if let Some(f) = frame {
            let n = f.samples.len().min(self.frame_samples);
            fixed[..n].copy_from_slice(&f.samples[..n]);
        }
        // The over files can fail (a full disk, say) without the session
        // timeline noticing: the whole-session samples go in and `ticks`
        // advances on every tick, and the first over error is returned after.
        let overs = self.step_overs(frame.is_some().then_some(&fixed[..]), talker, at, now);
        let whole = match self.whole.as_mut() {
            Some(w) => w.push_interleaved(&fixed).map_err(io_err),
            None => Ok(()),
        };
        #[cfg(test)]
        let whole = if std::mem::take(&mut self.fail_whole) {
            Err(io_err("injected failure"))
        } else {
            whole
        };
        self.ticks += 1;
        whole.and(overs.map_err(|e| SessionError::Over(std::io::Error::other(e.to_string()))))
    }

    /// The over half of a tick. Runs every step even after a failure, and
    /// returns the first error.
    fn step_overs(
        &mut self,
        frame: Option<&[i16]>,
        talker: Option<&str>,
        at: Instant,
        now: &jiff::Zoned,
    ) -> Result<(), SessionError> {
        let mut first: Result<(), SessionError> = Ok(());
        let mut keep = |r: Result<(), SessionError>| {
            if first.is_ok() {
                first = r;
            }
        };
        // A talker change splits only on a tick that has a frame: with none,
        // the splitter is in its merge or tail window and the old talker's
        // tail must stay with the old over.
        let mut split = false;
        if frame.is_some()
            && let (Some(t), Some(o)) = (talker, self.open.as_mut())
        {
            match o.talker.as_deref() {
                None if at.saturating_duration_since(o.started_at) <= LATE_TALKER => {
                    o.talker = Some(t.to_owned());
                }
                Some(cur) if cur != t => split = true,
                _ => {}
            }
        }
        if split {
            keep(self.end_over());
            keep(self.start_over(talker, at, now));
        }
        let events = match frame {
            Some(f) => self.splitter.push(f, at),
            None => self.splitter.poll(at),
        };
        for ev in events {
            match ev {
                OverEvent::Start => keep(self.start_over(talker, at, now)),
                OverEvent::Audio(samples) => {
                    if let Some(o) = self.open.as_mut() {
                        o.samples += samples.len() as u64;
                        keep(o.writer.push_interleaved(&samples).map_err(io_err));
                    }
                }
                OverEvent::End => keep(self.end_over()),
            }
        }
        first
    }

    fn start_over(
        &mut self,
        talker: Option<&str>,
        at: Instant,
        now: &jiff::Zoned,
    ) -> Result<(), SessionError> {
        // Close a stray open over first (a Start while one is open).
        self.end_over()?;
        let seq = self.next_seq;
        self.next_seq += 1;
        let name = over_file_name(seq, now, talker);
        let path = self.dir.join("overs").join(&name);
        let writer = FlacWriter::create_new(&path, self.rate, 1).map_err(io_err)?;
        self.open = Some(OpenOver {
            seq,
            writer,
            path,
            name,
            start: now.clone(),
            offset_ms: self.ticks * self.frame_samples as u64 * 1000 / u64::from(self.rate),
            talker: talker.map(str::to_owned),
            started_at: at,
            samples: 0,
        });
        Ok(())
    }

    fn end_over(&mut self) -> Result<(), SessionError> {
        let Some(mut o) = self.open.take() else {
            return Ok(());
        };
        // A failed finish does not lose the over: it is still logged below.
        let finished = o.writer.finish().map_err(io_err).map(|_| ());
        // A talker that turned up after the file was named: rename it, never
        // over another file. If the rename fails the over keeps its old name
        // and the row says what is actually on disk.
        let wanted = over_file_name(o.seq, &o.start, o.talker.as_deref());
        if wanted != o.name {
            let to = o.path.with_file_name(&wanted);
            if rename_no_replace(&o.path, &to) {
                o.name = wanted;
            }
        }
        let duration_ms = o.samples * 1000 / u64::from(self.rate);
        // end = start + duration, so the three fields always agree.
        let start = o.start.timestamp();
        let end = start
            .checked_add(jiff::SignedDuration::from_millis(
                i64::try_from(duration_ms).unwrap_or(i64::MAX),
            ))
            .unwrap_or(start);
        let rec = OverRec {
            start: start.to_string(),
            end: end.to_string(),
            duration_ms,
            talker: o.talker,
            file: o.name,
        };
        let row = append_row(
            &self.dir,
            &LogRow {
                t: rec.end.clone(),
                kind: LogKind::Over {
                    start: rec.start.clone(),
                    end: rec.end.clone(),
                    duration_ms,
                    offset_ms: o.offset_ms,
                    talker: rec.talker.clone(),
                    file: rec.file.clone(),
                },
            },
        );
        self.overs.push(rec);
        finished?;
        row?;
        Ok(())
    }

    /// Log a joined, dropped, rejoined, left or error event.
    ///
    /// # Errors
    /// [`SessionError::Io`] if the row cannot be appended.
    pub fn event(&mut self, kind: LogKind, now: jiff::Timestamp) -> Result<(), SessionError> {
        append_row(
            &self.dir,
            &LogRow {
                t: now.to_string(),
                kind,
            },
        )?;
        Ok(())
    }

    /// Finish the open over and `session.flac`, write `log.csv`,
    /// `checkins.txt` and the final `session.json`.
    ///
    /// A failure on the open over is logged and does not stop the rest.
    ///
    /// # Errors
    /// [`SessionError::Io`] on any other write error; an `Error` row is
    /// logged too.
    #[allow(clippy::needless_pass_by_value)] // the agreed signature takes the clock by value
    pub fn close(
        mut self,
        ended: Ended,
        rejoins: u32,
        dropped_frames: u64,
        now: jiff::Zoned,
    ) -> Result<SessionMeta, SessionError> {
        let result = self.close_inner(ended, rejoins, dropped_frames, &now);
        self.logged(result, &now)
    }

    fn close_inner(
        &mut self,
        ended: Ended,
        rejoins: u32,
        dropped_frames: u64,
        now: &jiff::Zoned,
    ) -> Result<SessionMeta, SessionError> {
        // Every step runs even after one fails, so `session.json` gets its
        // `ended` whenever the disk lets it. The first error comes back.
        let mut first: Option<SessionError> = None;
        let mut keep = |r: Result<(), SessionError>| {
            if let Err(e) = r
                && first.is_none()
            {
                first = Some(e);
            }
        };
        let over = self.end_over();
        if let Err(e) = self.logged(over, now) {
            tracing::warn!("the last over of {} did not finish: {e}", self.meta.net);
        }
        if let Some(whole) = self.whole.take() {
            keep(whole.finish().map(|_| ()).map_err(io_err));
        }

        keep(write_log_csv(&self.dir, &self.overs).map_err(Into::into));
        keep(write_checkins(&self.dir, &self.overs, &self.meta.network).map_err(Into::into));

        keep(write_peaks(&self.dir, &compute_peaks(&self.dir, &self.overs)).map_err(Into::into));

        let mut meta = self.meta.clone();
        meta.ended_at = Some(now.timestamp().to_string());
        meta.ended = Some(ended);
        meta.rejoins = rejoins;
        meta.dropped_frames = dropped_frames;
        meta.overs = u32::try_from(self.overs.len()).unwrap_or(u32::MAX);
        // If this fails too (a full disk) the session stays `ended: None`
        // and the next start marks it crashed.
        keep(write_meta(&self.dir, &meta).map_err(Into::into));
        match first {
            Some(e) => Err(e),
            None => Ok(meta),
        }
    }

    /// Turn an `Err` into an `Error` row too, if the log still takes one.
    fn logged<T>(&self, r: Result<T, SessionError>, now: &jiff::Zoned) -> Result<T, SessionError> {
        if let Err(e) = &r {
            let _ = append_row(
                &self.dir,
                &LogRow {
                    t: now.timestamp().to_string(),
                    kind: LogKind::Error {
                        message: e.to_string(),
                    },
                },
            );
        }
        r
    }

    /// Swap the free-space reading (tests).
    #[cfg(test)]
    pub(crate) fn set_free_fn(&mut self, free: FreeFn) {
        self.free = free;
    }

    /// The session folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `t` in the net's own time zone.
    #[must_use]
    pub fn zoned(&self, t: jiff::Timestamp) -> jiff::Zoned {
        t.to_zoned(self.tz.clone())
    }
}

/// Rename `from` to `to` without replacing `to`: a hard link fails when `to`
/// exists, then the old name goes. `false`, with `from` untouched, on failure.
fn rename_no_replace(from: &Path, to: &Path) -> bool {
    if fs::hard_link(from, to).is_err() {
        return false;
    }
    if fs::remove_file(from).is_err() {
        let _ = fs::remove_file(to);
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    use super::*;
    use crate::config::{NetCfg, Target, Visibility};
    use jiff::SignedDuration;
    use std::sync::atomic::{AtomicU32, Ordering};

    const RATE: u32 = 8000;
    const FRAME: usize = 160;

    fn temp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "netrec-session-{tag}-{}-{}",
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
            duration: std::time::Duration::from_secs(3600),
            tz: jiff::tz::TimeZone::get("America/Denver").unwrap(),
            visibility: Visibility::Private,
            my_side: false,
            my_callsigns: Vec::new(),
            video: false,
            youtube_live: false,
            youtube_upload: false,
        }
    }

    fn t0() -> jiff::Zoned {
        "2026-11-03T18:55:00-07:00[America/Denver]".parse().unwrap()
    }

    fn window() -> Window {
        let s = t0().timestamp();
        Window {
            net: 0,
            start: s,
            end: s,
            join_at: s,
            leave_at: s,
        }
    }

    struct Rig {
        rec: Option<SessionRecorder>,
        base: Instant,
        n: u64,
    }

    impl Rig {
        fn new(root: &Path) -> Self {
            Rig {
                rec: Some(SessionRecorder::open(root, &net(), &window(), RATE, t0()).unwrap()),
                base: Instant::now(),
                n: 0,
            }
        }
        fn zoned(&self) -> jiff::Zoned {
            t0().checked_add(SignedDuration::from_millis(self.n as i64 * 20))
                .unwrap()
        }
        /// `ticks` ticks, with audio when `voice`, carrying `talker`.
        fn run(&mut self, ticks: u32, voice: bool, talker: Option<&str>) {
            for _ in 0..ticks {
                let frame = voice.then(|| Frame {
                    rate: RATE,
                    samples: (0..FRAME)
                        .map(|i| ((i * 97) % 2000) as i16 - 1000)
                        .collect(),
                });
                let at = self.base + std::time::Duration::from_millis(self.n * 20);
                let now = self.zoned();
                self.rec
                    .as_mut()
                    .unwrap()
                    .tick(frame.as_ref(), talker, at, now)
                    .unwrap();
                self.n += 1;
            }
        }
        fn dir(&self) -> PathBuf {
            self.rec.as_ref().unwrap().dir().to_owned()
        }
        fn close(&mut self) -> SessionMeta {
            let now = self.zoned();
            self.rec
                .take()
                .unwrap()
                .close(Ended::Window, 0, 0, now)
                .unwrap()
        }
    }

    fn decode(path: &Path) -> (u32, usize) {
        let mut r = claxon::FlacReader::open(path).unwrap();
        let rate = r.streaminfo().sample_rate;
        (rate, r.samples().inspect(|s| assert!(s.is_ok())).count())
    }

    fn over_files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir.join("overs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn over_rows(dir: &Path) -> Vec<(u64, Option<String>, String)> {
        astar_netarchive::read_rows(dir)
            .unwrap()
            .into_iter()
            .filter_map(|r| match r.kind {
                LogKind::Over {
                    offset_ms,
                    talker,
                    file,
                    ..
                } => Some((offset_ms, talker, file)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn whole_session_length_matches_ticks() {
        let root = temp("whole");
        let mut rig = Rig::new(&root);
        rig.run(100, true, Some("NC0NTL"));
        rig.run(400, false, None);
        let dir = rig.dir();
        rig.close();
        assert_eq!(decode(&dir.join("session.flac")), (RATE, 500 * FRAME));
    }

    #[test]
    fn folder_name_gains_a_suffix_when_taken() {
        let root = temp("dup");
        let a = SessionRecorder::open(&root, &net(), &window(), RATE, t0()).unwrap();
        let b = SessionRecorder::open(&root, &net(), &window(), RATE, t0()).unwrap();
        assert!(a.dir().ends_with("tuesday-m17-net/2026-11-03 1855"));
        assert!(b.dir().ends_with("tuesday-m17-net/2026-11-03 1855 (2)"));
        assert!(a.dir().join("overs").is_dir());
        let m = astar_netarchive::read_meta(a.dir()).unwrap();
        assert_eq!(m.ended, None);
    }

    #[test]
    fn two_talkers_make_two_overs() {
        let root = temp("two");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        rig.run(100, false, None);
        rig.run(50, true, Some("KX0AAA"));
        let dir = rig.dir();
        rig.close();
        assert_eq!(over_files(&dir).len(), 2);
        let rows = over_rows(&dir);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1.as_deref(), Some("NC0NTL"));
        assert_eq!(rows[1].1.as_deref(), Some("KX0AAA"));
        assert_eq!(
            fs::read_to_string(dir.join("checkins.txt")).unwrap(),
            "NC0NTL\nKX0AAA\n"
        );
    }

    #[test]
    fn short_gap_merges() {
        let root = temp("merge");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        rig.run(15, false, None);
        rig.run(50, true, Some("NC0NTL"));
        let dir = rig.dir();
        rig.close();
        assert_eq!(over_files(&dir).len(), 1);
    }

    #[test]
    fn talker_change_mid_over_splits() {
        let root = temp("split");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        rig.run(50, true, Some("KX0AAA"));
        let dir = rig.dir();
        rig.close();
        let rows = over_rows(&dir);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1.as_deref(), Some("NC0NTL"));
        assert_eq!(rows[1].1.as_deref(), Some("KX0AAA"));
        assert_eq!(rows[1].0, 1000);
    }

    #[test]
    fn late_talker_renames_file() {
        let root = temp("late");
        let mut rig = Rig::new(&root);
        rig.run(50, true, None);
        rig.run(50, true, Some("NC0NTL"));
        let dir = rig.dir();
        rig.close();
        let files = over_files(&dir);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with(" NC0NTL.flac"), "{files:?}");
        let rows = over_rows(&dir);
        assert_eq!(rows[0].2, files[0]);
        assert_eq!(rows[0].1.as_deref(), Some("NC0NTL"));
    }

    #[test]
    fn talker_after_two_seconds_is_not_a_late_talker() {
        let root = temp("toolate");
        let mut rig = Rig::new(&root);
        rig.run(150, true, None);
        rig.run(10, true, Some("NC0NTL"));
        let dir = rig.dir();
        rig.close();
        let files = over_files(&dir);
        assert!(files[0].ends_with(" unknown.flac"), "{files:?}");
    }

    #[test]
    fn hostile_callsign_is_sanitised_in_filename_only() {
        let root = temp("hostile");
        let mut rig = Rig::new(&root);
        let raw = "../../x/\u{ff}";
        rig.run(50, true, Some(raw));
        let dir = rig.dir();
        rig.close();
        let files = over_files(&dir);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], over_file_name(1, &t0(), Some(raw)));
        assert!(!files[0].contains('/'));
        let rows = over_rows(&dir);
        assert_eq!(rows[0].1.as_deref(), Some(raw));
        assert!(dir.join("overs").join(&files[0]).is_file());
    }

    #[test]
    fn raw_talker_stays_in_the_log_while_the_file_names_the_base_call() {
        let root = temp("basecall");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("KX0AAA  D"));
        let dir = rig.dir();
        rig.close();
        let files = over_files(&dir);
        assert!(files[0].ends_with(" KX0AAA.flac"), "{files:?}");
        assert_eq!(over_rows(&dir)[0].1.as_deref(), Some("KX0AAA  D"));
    }

    #[test]
    fn open_writes_my_callsigns() {
        let root = temp("mycalls");
        let mut cfg = net();
        cfg.my_callsigns = vec!["N0CALL".into()];
        let rec = SessionRecorder::open(&root, &cfg, &window(), RATE, t0()).unwrap();
        let dir = rec.dir().to_owned();
        let meta = astar_netarchive::read_meta(&dir).unwrap();
        assert_eq!(meta.my_callsigns, vec!["N0CALL".to_owned()]);
    }

    #[test]
    fn offset_ms_points_into_session() {
        let root = temp("offset");
        let mut rig = Rig::new(&root);
        rig.run(75, false, None);
        rig.run(50, true, Some("NC0NTL"));
        let dir = rig.dir();
        rig.close();
        assert_eq!(over_rows(&dir)[0].0, 1500);
    }

    #[test]
    fn csv_and_meta_written_on_close() {
        let root = temp("csv");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        rig.run(100, false, None);
        rig.run(50, true, Some("=KX0AAA"));
        let dir = rig.dir();
        let meta = rig.close();
        assert_eq!(meta.overs, 2);
        assert_eq!(meta.ended, Some(Ended::Window));
        assert_eq!(astar_netarchive::read_meta(&dir).unwrap(), meta);
        let csv = fs::read_to_string(dir.join("log.csv")).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "start,end,duration_s,talker,file");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains(",NC0NTL,"), "{csv}");
        assert!(lines[2].contains(",'=KX0AAA,"), "{csv}");
    }

    /// Crash recovery rebuilds the same summary a clean close wrote.
    #[test]
    fn a_recovered_summary_matches_a_closed_one() {
        let root = temp("recovered");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        rig.run(100, false, None);
        rig.run(50, true, Some("=KX0AAA"));
        let dir = rig.dir();
        let mut meta = rig.close();
        let csv = fs::read_to_string(dir.join("log.csv")).unwrap();
        let checkins = fs::read_to_string(dir.join("checkins.txt")).unwrap();

        fs::remove_file(dir.join("log.csv")).unwrap();
        fs::remove_file(dir.join("checkins.txt")).unwrap();
        meta.ended = None;
        meta.overs = 0;
        astar_netarchive::write_meta(&dir, &meta).unwrap();
        astar_netarchive::recover_unfinished(&root, t0().timestamp()).unwrap();

        assert_eq!(fs::read_to_string(dir.join("log.csv")).unwrap(), csv);
        assert_eq!(
            fs::read_to_string(dir.join("checkins.txt")).unwrap(),
            checkins
        );
        assert_eq!(astar_netarchive::read_meta(&dir).unwrap().overs, 2);
    }

    #[test]
    fn every_over_flac_decodes() {
        let root = temp("decode");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        rig.run(100, false, None);
        rig.run(50, true, None);
        let dir = rig.dir();
        rig.close();
        for f in over_files(&dir) {
            let (rate, n) = decode(&dir.join("overs").join(f));
            assert_eq!(rate, RATE);
            assert!(n >= 50 * FRAME);
        }
    }

    #[test]
    fn event_appends_a_row() {
        let root = temp("event");
        let mut rig = Rig::new(&root);
        rig.rec
            .as_mut()
            .unwrap()
            .event(LogKind::Joined, t0().timestamp())
            .unwrap();
        let dir = rig.dir();
        rig.close();
        let rows = astar_netarchive::read_rows(&dir).unwrap();
        assert_eq!(rows[0].kind, LogKind::Joined);
    }

    fn voice_frame() -> Frame {
        Frame {
            rate: RATE,
            samples: (0..FRAME)
                .map(|i| ((i * 97) % 2000) as i16 - 1000)
                .collect(),
        }
    }

    #[test]
    fn over_write_failure_keeps_session_length() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("overfail");
        let mut rig = Rig::new(&root);
        let dir = rig.dir();
        fs::set_permissions(dir.join("overs"), fs::Permissions::from_mode(0o555)).unwrap();
        let mut errs = 0;
        for _ in 0..10 {
            let at = rig.base + std::time::Duration::from_millis(rig.n * 20);
            let now = rig.zoned();
            let r = rig
                .rec
                .as_mut()
                .unwrap()
                .tick(Some(&voice_frame()), Some("A"), at, now);
            errs += usize::from(r.is_err());
            rig.n += 1;
        }
        fs::set_permissions(dir.join("overs"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(errs > 0);
        rig.run(10, false, None);
        rig.close();
        assert_eq!(decode(&dir.join("session.flac")).1, 20 * FRAME);
    }

    #[test]
    fn failed_row_append_still_counts_the_over() {
        let root = temp("rowfail");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        let log = rig.dir().join(astar_netarchive::LOG_FILE);
        let _ = fs::remove_file(&log);
        fs::create_dir(&log).unwrap();
        let mut errs = 0;
        for _ in 0..100 {
            let at = rig.base + std::time::Duration::from_millis(rig.n * 20);
            let now = rig.zoned();
            errs += usize::from(rig.rec.as_mut().unwrap().tick(None, None, at, now).is_err());
            rig.n += 1;
        }
        assert!(errs > 0);
        let rec = rig.rec.as_ref().unwrap();
        assert_eq!(rec.overs.len(), 1);
        assert_eq!(rec.overs[0].talker.as_deref(), Some("NC0NTL"));
    }

    #[test]
    fn end_equals_start_plus_duration() {
        let root = temp("endcalc");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("NC0NTL"));
        rig.run(150, false, None);
        let dir = rig.dir();
        rig.close();
        let rows = astar_netarchive::read_rows(&dir).unwrap();
        let LogKind::Over {
            start,
            end,
            duration_ms,
            ..
        } = &rows[0].kind
        else {
            panic!("no over row");
        };
        let s: jiff::Timestamp = start.parse().unwrap();
        let e: jiff::Timestamp = end.parse().unwrap();
        assert_eq!(e.duration_since(s).as_millis(), i128::from(*duration_ms));
        assert!(*duration_ms >= 1000);
    }

    #[test]
    fn talker_change_in_the_tail_makes_no_phantom_over() {
        let root = temp("phantom");
        let mut rig = Rig::new(&root);
        rig.run(50, true, Some("A"));
        rig.run(100, false, Some("B"));
        let dir = rig.dir();
        rig.close();
        let rows = over_rows(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.as_deref(), Some("A"));
    }

    #[test]
    fn close_finishes_an_open_over() {
        let root = temp("openclose");
        let mut rig = Rig::new(&root);
        rig.run(25, true, Some("NC0NTL"));
        let dir = rig.dir();
        let meta = rig.close();
        assert_eq!(meta.overs, 1);
        let f = over_files(&dir).remove(0);
        assert!(decode(&dir.join("overs").join(f)).1 >= 25 * FRAME);
    }

    #[test]
    fn close_writes_peaks_for_every_over() {
        let root = temp("peaks");
        let mut rig = Rig::new(&root);
        rig.run(25, true, Some("NC0NTL"));
        let dir = rig.dir();
        rig.close();
        let p = astar_netarchive::read_peaks(&dir).unwrap().unwrap();
        let files = over_files(&dir);
        assert_eq!(p.overs.keys().cloned().collect::<Vec<_>>(), files);
        assert!(p.overs.values().all(|v| v.len() == 48));
    }

    /// `n` voiced ticks and then enough silence to end the over, returning
    /// the tick results.
    fn over_then_gap(rig: &mut Rig, n: u32) -> Vec<Result<(), SessionError>> {
        let mut out = Vec::new();
        for i in 0..n + 150 {
            let frame = (i < n).then(voice_frame);
            let at = rig.base + std::time::Duration::from_millis(rig.n * 20);
            let now = rig.zoned();
            out.push(
                rig.rec
                    .as_mut()
                    .unwrap()
                    .tick(frame.as_ref(), Some("A"), at, now),
            );
            rig.n += 1;
        }
        out
    }

    #[test]
    fn over_failures_are_their_own_kind_and_log_one_row() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("overkind");
        let mut rig = Rig::new(&root);
        let dir = rig.dir();
        fs::set_permissions(dir.join("overs"), fs::Permissions::from_mode(0o555)).unwrap();
        let mut results = over_then_gap(&mut rig, 50);
        results.extend(over_then_gap(&mut rig, 50));
        fs::set_permissions(dir.join("overs"), fs::Permissions::from_mode(0o755)).unwrap();
        let errs: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        assert!(errs.len() >= 2, "{errs:?}");
        assert!(
            errs.iter().all(|e| matches!(e, SessionError::Over(_))),
            "{errs:?}"
        );
        rig.close();
        let rows = astar_netarchive::read_rows(&dir).unwrap();
        let errors = rows
            .iter()
            .filter(|r| matches!(r.kind, LogKind::Error { .. }))
            .count();
        assert_eq!(errors, 1, "{rows:?}");
    }

    #[test]
    fn whole_session_failure_outranks_an_over_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("wholekind");
        let mut rig = Rig::new(&root);
        let dir = rig.dir();
        fs::set_permissions(dir.join("overs"), fs::Permissions::from_mode(0o555)).unwrap();
        rig.rec.as_mut().unwrap().fail_whole = true;
        let at = rig.base;
        let now = rig.zoned();
        let r = rig
            .rec
            .as_mut()
            .unwrap()
            .tick(Some(&voice_frame()), Some("A"), at, now);
        fs::set_permissions(dir.join("overs"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(r, Err(SessionError::Io(_))), "{r:?}");
    }

    #[test]
    fn a_failing_over_cannot_stop_the_session_closing() {
        let root = temp("closeover");
        let mut rig = Rig::new(&root);
        rig.run(25, true, Some("NC0NTL"));
        let dir = rig.dir();
        // The open over's row cannot be written, so finishing it fails.
        fs::create_dir(dir.join(astar_netarchive::LOG_FILE)).unwrap();
        let meta = rig.close();
        assert_eq!(meta.ended, Some(Ended::Window));
        assert_eq!(decode(&dir.join("session.flac")).1, 25 * FRAME);
        assert_eq!(astar_netarchive::read_meta(&dir).unwrap(), meta);
    }

    #[test]
    fn odd_frame_length_keeps_the_timeline() {
        let root = temp("oddlen");
        let mut rig = Rig::new(&root);
        for i in 0..10u64 {
            let n = if i % 2 == 0 { 100 } else { 400 };
            let f = Frame {
                rate: RATE,
                samples: vec![500; n],
            };
            let at = rig.base + std::time::Duration::from_millis(i * 20);
            let now = rig.zoned();
            rig.rec
                .as_mut()
                .unwrap()
                .tick(Some(&f), None, at, now)
                .unwrap();
            rig.n += 1;
        }
        let dir = rig.dir();
        rig.close();
        assert_eq!(decode(&dir.join("session.flac")).1, 10 * FRAME);
    }

    #[allow(clippy::unnecessary_wraps)]
    fn low(_: &Path) -> std::io::Result<u64> {
        Ok(1024)
    }
    #[allow(clippy::unnecessary_wraps)]
    fn plenty(_: &Path) -> std::io::Result<u64> {
        Ok(u64::MAX)
    }
    fn unreadable(_: &Path) -> std::io::Result<u64> {
        Err(std::io::Error::other("no statvfs"))
    }

    #[test]
    fn low_disk_stops_cleanly_and_keeps_current() {
        let root = temp("lowdisk");
        let mut rig = Rig::new(&root);
        rig.rec.as_mut().unwrap().set_free_fn(plenty);
        rig.run(100, true, Some("NC0NTL"));
        rig.rec.as_mut().unwrap().set_free_fn(low);
        // The check runs every 500 ticks; ticks 100..500 pass.
        rig.run(399, true, Some("NC0NTL"));
        let at = rig.base + std::time::Duration::from_millis(rig.n * 20);
        let now = rig.zoned();
        let r = rig
            .rec
            .as_mut()
            .unwrap()
            .tick(Some(&voice_frame()), Some("NC0NTL"), at, now);
        assert!(matches!(r, Err(SessionError::LowDisk)), "{r:?}");
        rig.n += 1;
        let dir = rig.dir();
        let now = rig.zoned();
        let meta = rig
            .rec
            .take()
            .unwrap()
            .close(Ended::Error, 0, 0, now)
            .unwrap();
        assert_eq!(meta.ended, Some(Ended::Error));
        let rows = astar_netarchive::read_rows(&dir).unwrap();
        assert!(
            rows.iter().any(|r| r.kind
                == LogKind::Error {
                    message: "disk nearly full".to_owned()
                }),
            "{rows:?}"
        );
        assert_eq!(decode(&dir.join("session.flac")).1, 500 * FRAME);
        for f in over_files(&dir) {
            assert!(decode(&dir.join("overs").join(f)).1 > 0);
        }
    }

    #[test]
    fn open_refuses_below_the_threshold() {
        let root = temp("lowopen");
        let r = SessionRecorder::open_with(&root, &net(), &window(), RATE, t0(), low);
        assert!(matches!(r, Err(SessionError::LowDisk)));
    }

    #[test]
    fn an_unreadable_free_space_never_stops_recording() {
        let root = temp("freefail");
        let mut rig = Rig::new(&root);
        rig.rec.as_mut().unwrap().set_free_fn(unreadable);
        rig.run(1200, false, None);
        let dir = rig.dir();
        rig.close();
        assert_eq!(decode(&dir.join("session.flac")).1, 1200 * FRAME);
    }

    #[test]
    fn failed_csv_still_writes_meta() {
        let root = temp("csvfail");
        let mut rig = Rig::new(&root);
        rig.run(25, true, Some("NC0NTL"));
        let dir = rig.dir();
        // A folder where log.csv goes makes writing it fail.
        fs::create_dir(dir.join("log.csv")).unwrap();
        let now = rig.zoned();
        let r = rig.rec.take().unwrap().close(Ended::Window, 0, 0, now);
        assert!(r.is_err());
        let meta = astar_netarchive::read_meta(&dir).unwrap();
        assert_eq!(meta.ended, Some(Ended::Window));
        assert!(dir.join("checkins.txt").exists());
        assert_eq!(decode(&dir.join("session.flac")).1, 25 * FRAME);
    }
}
