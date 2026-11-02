// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The node logger: a thread that records every over the conference
//! carries, one FLAC per over, and keeps the folder under its caps.
//!
//! It enrols a NON-contributing conference member through
//! [`TapSource::enrol`] (which refuses parrot mode), so it hears the whole
//! relay mix — every link, the M17 monitor, a Discord bridge if one runs —
//! and can put nothing on the air. Unlike the stream tap it never fills
//! quiet with silence: the gaps are what the over splitter splits on.
//!
//! Retention deletes only what this logger wrote: files named by a row of
//! its own `index.jsonl` (at the `<date>/<name>` path it gives them), and
//! capture files named by `astar-air/` rows of `captured/*/manifest.jsonl`.
//! Nothing else in the folder is walked, counted or deleted.
//!
//! Downloads: [`NodeLogger::plan_download`] lists, under the folder lock,
//! the files a download will send (names only, nothing held open).
//! [`crate::recording::http`] streams that plan with `astar_record::tarball::write_tar_planned`, which
//! opens one file at a time just before writing it, skips a file pruned or
//! deleted since planning, and writes `index.jsonl` and the manifests last,
//! listing only what it sent. So pruning and [`NodeLogger::delete`] never
//! wait for a download, and a download of any size holds one file open.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use astar_record::capture::CaptureRow;
use astar_record::clock::{parse_rfc3339, rfc3339_utc};
use astar_record::disk::MIN_FREE_BYTES;
use astar_record::frame_writer::FrameWriter;
use astar_record::frames::FrameEvent;
use astar_record::naming::key_component;
use astar_record::tap::{FRAME_TAP_CAPACITY, Tap, tap_channel};
use astar_record::tarball::{DownloadPlan, PlannedEntry, PlannedLedger, PlannedLine};
use astar_record::{FlacWriter, OverEvent, OverSplitter, SplitterConfig};

use crate::command::NodeEvent;
use crate::conference_tap::TapSource;
use crate::config::RecordingCfg;
use crate::recording::index::{self, INDEX_FILE, IndexRow};

/// How often the logger retries enrolling while there is no conference.
pub const ENROL_RETRY: Duration = Duration::from_secs(2);
/// How often an enrolled logger checks its member still exists.
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// One conference tick.
const FRAME: Duration = Duration::from_millis(20);
/// The logger's own clock: how often it drains its channels.
const TICK: Duration = Duration::from_millis(10);
/// A talker keyed this recently, or an M17 station heard this recently,
/// counts for an over that starts now.
const TALKER_WINDOW: Duration = Duration::from_secs(2);
/// How often free space is measured again during a long over.
const FREE_CHECK_EVERY: Duration = Duration::from_secs(5);
/// How often the caps run on their own, so a quiet node still ages its
/// recordings out. A run that falls due during an over waits for its end.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// Where the logger gets its member, its talkers and its codec frames.
/// `NodeController` in the daemon; fakes in tests.
pub trait RecordSource: TapSource {
    /// The node's event bus (link key-ups name `AllStar` talkers).
    fn subscribe(&self) -> Receiver<NodeEvent>;
    /// The M17 station heard within `within`, if any.
    fn m17_recent_talker(&self, within: Duration) -> Option<String>;
    /// Install (or remove) the station's codec-frame tap.
    fn set_frame_tap(&self, tap: Option<Tap<FrameEvent>>);
}

impl RecordSource for crate::controller::NodeController {
    fn subscribe(&self) -> Receiver<NodeEvent> {
        crate::controller::NodeController::subscribe(self)
    }
    fn m17_recent_talker(&self, within: Duration) -> Option<String> {
        crate::controller::NodeController::m17_recent_talker(self, within)
    }
    fn set_frame_tap(&self, tap: Option<Tap<FrameEvent>>) {
        self.set_logger_frame_tap(tap);
    }
}

/// What the logger is doing, for the admin page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LoggerStatus {
    /// `"recording"`, `"idle"`, `"no_conference"` or `"disk_low"`.
    pub state: &'static str,
    /// Free bytes under the folder at the last measurement.
    pub free_bytes: Option<u64>,
    /// Bytes the logger's own files take (recordings, captures, ledgers).
    pub total_bytes: u64,
}

#[derive(Default)]
struct Shared {
    recording: bool,
    enrolled: bool,
    disk_low: bool,
    free_bytes: Option<u64>,
    total_bytes: u64,
}

/// The server's node logger. See the module docs.
pub struct NodeLogger {
    dir: PathBuf,
    max_bytes: u64,
    max_age: Duration,
    frames: bool,
    free_space: fn(&Path) -> Option<u64>,
    shared: Mutex<Shared>,
    /// Held by everything that changes the folder, and by
    /// [`NodeLogger::open_download`] while it opens files.
    files: Mutex<()>,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl NodeLogger {
    /// Start the logger thread against `source`.
    #[must_use]
    pub fn start(cfg: &RecordingCfg, source: Arc<dyn RecordSource>) -> Arc<Self> {
        Self::start_with(cfg, source, astar_record::disk::free_bytes)
    }

    /// [`NodeLogger::start`] with the free-space measurement swapped out.
    ///
    /// # Panics
    /// If the thread cannot be spawned.
    #[must_use]
    pub(crate) fn start_with(
        cfg: &RecordingCfg,
        source: Arc<dyn RecordSource>,
        free_space: fn(&Path) -> Option<u64>,
    ) -> Arc<Self> {
        let logger = Self::new_with(cfg, free_space);
        let worker = Arc::clone(&logger);
        let handle = std::thread::Builder::new()
            .name("node-logger".into())
            .spawn(move || Worker::new(&worker, source.as_ref()).run())
            .expect("spawn the node logger");
        *logger.thread.lock().expect("logger poisoned") = Some(handle);
        logger
    }

    /// The logger without its thread: the folder made, the caps run once
    /// and the totals measured. [`NodeLogger::start_with`] adds the thread;
    /// tests drive a [`Worker`] by hand.
    fn new_with(cfg: &RecordingCfg, free_space: fn(&Path) -> Option<u64>) -> Arc<Self> {
        let logger = Arc::new(Self {
            dir: cfg.dir.clone(),
            max_bytes: cfg.max_bytes.bytes,
            max_age: Duration::from_secs(u64::from(cfg.max_age_days) * 86_400),
            frames: cfg.frames,
            free_space,
            shared: Mutex::new(Shared::default()),
            files: Mutex::new(()),
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        if let Err(e) = std::fs::create_dir_all(&logger.dir) {
            tracing::warn!("recording: cannot create {}: {e}", logger.dir.display());
        }
        {
            let files = logger.files.lock().expect("logger poisoned");
            // The caps run at start, so a node that stays quiet still
            // drops what aged out while it was down.
            let total = logger.retain(true);
            logger.warn_unindexed();
            drop(files);
            let mut sh = logger.shared.lock().expect("logger poisoned");
            sh.total_bytes = total;
            sh.free_bytes = (logger.free_space)(&logger.dir);
        }
        logger
    }

    /// The recordings folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn status(&self) -> LoggerStatus {
        let sh = self.shared.lock().expect("logger poisoned");
        let state = if sh.recording {
            "recording"
        } else if sh.disk_low {
            "disk_low"
        } else if !sh.enrolled {
            "no_conference"
        } else {
            "idle"
        };
        LoggerStatus {
            state,
            free_bytes: sh.free_bytes,
            total_bytes: sh.total_bytes,
        }
    }

    /// Every recording, newest first.
    #[must_use]
    pub fn rows(&self) -> Vec<IndexRow> {
        let mut rows = index::read_all(&self.dir);
        rows.reverse();
        rows.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        rows
    }

    /// The file of the recording called `name` (a row's own `name`; the
    /// path is the row's, checked as [`NodeLogger::delete`] checks it, and
    /// nothing is built from `name`). `None` when no row has that name or
    /// its file is not a plain file in a plain day folder.
    #[must_use]
    pub fn file_for(&self, name: &str) -> Option<PathBuf> {
        let _files = self.files.lock().expect("logger poisoned");
        index::read_all(&self.dir)
            .iter()
            .find(|r| r.name == name)
            .and_then(|r| self.owned_recording(r))
    }

    /// Delete the recording called `name` (a row's own `name`; nothing is
    /// built from it). `false` when no row has that name.
    pub fn delete(&self, name: &str) -> bool {
        let files = self.files.lock().expect("logger poisoned");
        let mut rows = index::read_all(&self.dir);
        let Some(pos) = rows.iter().position(|r| r.name == name) else {
            return false;
        };
        let row = rows.remove(pos);
        if let Err(e) = index::rewrite(&self.dir, &rows) {
            tracing::warn!("recording: cannot rewrite the index: {e}");
            return false;
        }
        if let Some(p) = self.owned_recording(&row) {
            remove_logged(&p);
        }
        let total = self.retain(false);
        drop(files);
        self.shared.lock().expect("logger poisoned").total_bytes = total;
        true
    }

    /// The plan for a download of recordings started at or after `since`
    /// (all of them for `None`), made under the folder lock from the
    /// ledgers: every recording an index row names at a path this logger
    /// owns, and every capture row whose `.ambe` and `.flac` both exist,
    /// with their ledger lines. Nothing is opened. [`crate::recording::http`] streams it with
    /// `astar_record::tarball::write_tar_planned`; pruning and deletes carry
    /// on meanwhile and never wait for it.
    #[must_use]
    pub fn plan_download(&self, since: Option<SystemTime>) -> DownloadPlan {
        let _files = self.files.lock().expect("logger poisoned");
        let mut plan = DownloadPlan::default();
        let mut index_lines = Vec::new();
        for row in index::read_all(&self.dir) {
            if let Some(since) = since
                && parse_rfc3339(&row.started_at).is_none_or(|t| t < since)
            {
                continue;
            }
            let Some(path) = self.owned_recording(&row) else {
                continue;
            };
            let Ok(line) = serde_json::to_string(&row) else {
                continue;
            };
            let day = path.parent().map(Path::to_path_buf).unwrap_or_default();
            index_lines.push(PlannedLine {
                needs: vec![plan.entries.len()],
                bytes: line.into_bytes(),
            });
            plan.entries.push(PlannedEntry {
                path_in_tar: format!("recordings/{}", row.path),
                file: path,
                real_dirs: vec![day],
            });
        }
        plan.ledgers.push(PlannedLedger {
            path_in_tar: "recordings/index.jsonl".to_owned(),
            lines: index_lines,
            keep_if_empty: true,
        });
        for man in read_manifests(&self.dir) {
            let Some(mode) = man.mode_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let air = man.mode_dir.join("astar-air");
            let real_dirs = vec![self.dir.join("captured"), man.mode_dir.clone(), air];
            let mut lines = Vec::new();
            for line in &man.lines {
                let Some(row) = &line.row else { continue };
                if let Some(since) = since
                    && parse_rfc3339(&row.captured_at).is_none_or(|t| t < since)
                {
                    continue;
                }
                // unamblify needs both halves; a row missing one is left out.
                let files = capture_files(&man.mode_dir, row);
                if files.len() != 2 {
                    continue;
                }
                let mut needs = Vec::new();
                for p in files {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    needs.push(plan.entries.len());
                    plan.entries.push(PlannedEntry {
                        path_in_tar: format!("captured/{mode}/astar-air/{name}"),
                        file: p.clone(),
                        real_dirs: real_dirs.clone(),
                    });
                }
                lines.push(PlannedLine {
                    needs,
                    bytes: line.raw.clone().into_bytes(),
                });
            }
            if !lines.is_empty() {
                plan.ledgers.push(PlannedLedger {
                    path_in_tar: format!("captured/{mode}/manifest.jsonl"),
                    lines,
                    keep_if_empty: false,
                });
            }
        }
        plan
    }

    /// Warns (once, at start) about files that look like this logger's
    /// recordings but that no index row names. They are reported, never
    /// deleted or counted.
    fn warn_unindexed(&self) {
        let indexed: std::collections::HashSet<String> = index::read_all(&self.dir)
            .into_iter()
            .map(|r| r.path)
            .collect();
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut stray = Vec::new();
        for day in rd.flatten() {
            let Some(date) = day.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_date(&date) || !is_real_dir(&day.path()) {
                continue;
            }
            for f in std::fs::read_dir(day.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let Some(name) = f.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let rel = format!("{date}/{name}");
                if looks_like_ours(&name) && !indexed.contains(&rel) {
                    stray.push(rel);
                }
            }
        }
        if !stray.is_empty() {
            stray.sort();
            tracing::warn!(
                "recording: {} file(s) in {} look like recordings but are not in {INDEX_FILE}; \
                 they are left alone and never pruned (first: {})",
                stray.len(),
                self.dir.display(),
                stray[0]
            );
        }
    }

    /// Stop the thread: the open over is finished and indexed, the member
    /// withdrawn and the frame tap removed.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().expect("logger poisoned").take() {
            let _ = t.join();
        }
    }

    /// The file a row names, if it is one this logger would have written:
    /// `<YYYY-MM-DD>/<name>` with a plain `name`, a regular file (never a
    /// symlink) under the folder.
    fn owned_recording(&self, row: &IndexRow) -> Option<PathBuf> {
        let (date, name) = row.path.split_once('/')?;
        if !is_date(date)
            || !is_real_dir(&self.dir.join(date))
            || name != row.name
            || !plain_name(name)
            || Path::new(name).extension() != Some("flac".as_ref())
        {
            return None;
        }
        let p = self.dir.join(date).join(name);
        is_regular(&p).then_some(p)
    }

    /// Measures free space and records it; `true` when there is enough.
    /// Sets or clears `disk_low` either way.
    fn free_ok(&self) -> bool {
        let free = (self.free_space)(&self.dir);
        let ok = free.is_none_or(|f| f >= MIN_FREE_BYTES);
        let mut sh = self.shared.lock().expect("logger poisoned");
        sh.free_bytes = free;
        sh.disk_low = !ok;
        ok
    }

    /// Runs the caps now. Never waits for a download (see the module docs).
    pub(crate) fn prune(&self) {
        let files = self.files.lock().expect("logger poisoned");
        let total = self.retain(true);
        drop(files);
        self.shared.lock().expect("logger poisoned").total_bytes = total;
    }

    /// Measures the logger's own files and, when `apply`, deletes rows past
    /// `max_age`, then the oldest rows while over `max_bytes`. Returns the
    /// total left. The caller holds `self.files`, so nothing else changes
    /// the folder meanwhile.
    fn retain(&self, apply: bool) -> u64 {
        let mut rows = index::read_all(&self.dir);
        let mut manifests = read_manifests(&self.dir);
        let mut items: Vec<Item> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let files: Vec<PathBuf> = self.owned_recording(r).into_iter().collect();
            items.push(Item {
                at: parse_rfc3339(&r.started_at),
                bytes: files.iter().map(|p| file_len(p)).sum(),
                files,
                which: Which::Recording(i),
                gone: false,
            });
        }
        for (m, man) in manifests.iter().enumerate() {
            for (l, line) in man.lines.iter().enumerate() {
                let Some(row) = &line.row else { continue };
                let files = capture_files(&man.mode_dir, row);
                if files.is_empty() {
                    continue;
                }
                items.push(Item {
                    at: parse_rfc3339(&row.astar.received_at)
                        .or_else(|| parse_rfc3339(&row.captured_at)),
                    bytes: files.iter().map(|p| file_len(p)).sum(),
                    files,
                    which: Which::Capture(m, l),
                    gone: false,
                });
            }
        }
        let ledgers = file_len(&self.dir.join(INDEX_FILE))
            + manifests.iter().map(|m| file_len(&m.path)).sum::<u64>();
        let mut total = ledgers + items.iter().map(|i| i.bytes).sum::<u64>();
        if !apply {
            return total;
        }

        if let Some(cutoff) = SystemTime::now().checked_sub(self.max_age) {
            for it in &mut items {
                if it.at.is_some_and(|t| t < cutoff) {
                    it.gone = true;
                    total -= it.bytes;
                }
            }
        }
        // Oldest first; a row whose time does not parse counts as oldest.
        while total > self.max_bytes {
            let oldest = items
                .iter_mut()
                .filter(|i| !i.gone)
                .min_by_key(|i| i.at.unwrap_or(SystemTime::UNIX_EPOCH));
            let Some(it) = oldest else { break };
            it.gone = true;
            total -= it.bytes;
        }
        let gone: Vec<&Item> = items.iter().filter(|i| i.gone).collect();
        // Index rows whose path is not one of ours age out too; their
        // `files` list is empty, so nothing outside the ledger is touched.
        if gone.is_empty() {
            return total;
        }
        let mut drop_rows = vec![false; rows.len()];
        for it in &gone {
            match it.which {
                Which::Recording(i) => drop_rows[i] = true,
                Which::Capture(m, l) => manifests[m].lines[l].keep = false,
            }
        }
        // Ledgers first, files after: a crash between the two leaves a
        // stray file (harmless) rather than a row pointing at nothing.
        if drop_rows.iter().any(|&d| d) {
            let mut i = 0;
            rows.retain(|_| {
                let keep = !drop_rows[i];
                i += 1;
                keep
            });
            if let Err(e) = index::rewrite(&self.dir, &rows) {
                tracing::warn!("recording: cannot rewrite the index: {e}");
                return total;
            }
        }
        for man in &manifests {
            if man.lines.iter().all(|l| l.keep) {
                continue;
            }
            let mut text = String::new();
            for l in man.lines.iter().filter(|l| l.keep) {
                text.push_str(&l.raw);
                text.push('\n');
            }
            if let Err(e) = index::replace_file(&man.path, text.as_bytes()) {
                tracing::warn!("recording: cannot rewrite {}: {e}", man.path.display());
                return total;
            }
        }
        for it in gone {
            for p in &it.files {
                remove_logged(p);
            }
        }
        // The ledgers shrank too; measure again rather than estimate.
        self.retain(false)
    }
}

enum Which {
    Recording(usize),
    Capture(usize, usize),
}

struct Item {
    at: Option<SystemTime>,
    bytes: u64,
    files: Vec<PathBuf>,
    which: Which,
    gone: bool,
}

struct ManifestLine {
    raw: String,
    row: Option<CaptureRow>,
    keep: bool,
}

struct Manifest {
    path: PathBuf,
    mode_dir: PathBuf,
    lines: Vec<ManifestLine>,
}

/// `[A-Za-z0-9_.+-]`, not starting with `.`: a name this logger writes.
fn plain_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// `YYYY-MM-DD`, the shape of the logger's day folders.
fn is_date(s: &str) -> bool {
    s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// `20261027-010004-<net>-<talker>[-n].flac`: a name the logger writes.
fn looks_like_ours(name: &str) -> bool {
    let b = name.as_bytes();
    plain_name(name)
        && Path::new(name).extension() == Some("flac".as_ref())
        && b.len() > 16
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'-'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'-'
}

/// A directory itself, not a symlink to one.
fn is_real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
}

/// `remove_file`, with a failure logged rather than dropped. A file that is
/// already gone is not a failure.
fn remove_logged(p: &Path) {
    if let Err(e) = std::fs::remove_file(p)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("recording: cannot delete {}: {e}", p.display());
    }
}

/// A new recording at `path`, through `FlacWriter::create_new`. A file
/// made before the writer gave up (the header could not be encoded or
/// written) is removed; `AlreadyExists` means the name was never ours, and
/// whatever holds it is left alone.
fn create_recording(path: &Path, rate: u32) -> Result<FlacWriter, astar_record::RecordError> {
    FlacWriter::create_new(path, rate, 1).inspect_err(|e| {
        if !matches!(e, astar_record::RecordError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists)
        {
            remove_logged(path);
        }
    })
}

/// Renames `from` to `to` without ever replacing `to`: a hard link (which
/// fails with `AlreadyExists` when `to` is taken), then the old name is
/// removed. `false`, with `from` left as it was, when `to` exists or the
/// link cannot be made; the recording then keeps its first name.
fn rename_no_replace(from: &Path, to: &Path) -> bool {
    if std::fs::hard_link(from, to).is_err() {
        return false;
    }
    if let Err(e) = std::fs::remove_file(from) {
        // Both names point at the file: drop the new one, keep the first.
        tracing::warn!("recording: cannot remove {}: {e}", from.display());
        let _ = std::fs::remove_file(to);
        return false;
    }
    true
}

fn is_regular(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

fn file_len(p: &Path) -> u64 {
    std::fs::symlink_metadata(p)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map_or(0, |m| m.len())
}

/// Every `captured/<mode>/manifest.jsonl` under `dir`, as raw lines.
fn read_manifests(dir: &Path) -> Vec<Manifest> {
    let captured = dir.join("captured");
    let mut out = Vec::new();
    if !is_real_dir(&captured) {
        return out;
    }
    let Ok(rd) = std::fs::read_dir(&captured) else {
        return out;
    };
    let mut modes: Vec<String> = rd
        .flatten()
        .filter(|e| is_real_dir(&e.path()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| plain_name(n))
        .collect();
    modes.sort();
    for mode in modes {
        let mode_dir = captured.join(&mode);
        let path = mode_dir.join("manifest.jsonl");
        if !is_regular(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| ManifestLine {
                raw: l.to_owned(),
                row: serde_json::from_str(l).ok(),
                keep: true,
            })
            .collect();
        out.push(Manifest {
            path,
            mode_dir,
            lines,
        });
    }
    out
}

/// The `.ambe` and `.flac` an `astar-air/<stem>` row names, those that exist.
fn capture_files(mode_dir: &Path, row: &CaptureRow) -> Vec<PathBuf> {
    let Some(stem) = row.key.strip_prefix("astar-air/").filter(|s| plain_name(s)) else {
        return Vec::new();
    };
    if !is_real_dir(mode_dir) || !is_real_dir(&mode_dir.join("astar-air")) {
        return Vec::new();
    }
    ["ambe", "flac"]
        .iter()
        .map(|ext| mode_dir.join("astar-air").join(format!("{stem}.{ext}")))
        .filter(|p| is_regular(p))
        .collect()
}

/// Is `p` a plain relative path (no `..`, no root)?
fn is_plain_relative(p: &str) -> bool {
    Path::new(p)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
}

/// Who has been talking.
#[derive(Default)]
struct Talkers {
    /// `AllStar` nodes in the order they keyed; `None` while still keyed,
    /// `Some(t)` once they unkeyed at `t`.
    nodes: Vec<(String, Option<Instant>)>,
}

impl Talkers {
    /// Returns the node that just keyed, if the event is a key-up.
    fn observe(&mut self, ev: &NodeEvent, now: Instant) -> Option<String> {
        let NodeEvent::Link {
            kind, node, keyed, ..
        } = ev
        else {
            return None;
        };
        let up = kind == "keyed" && *keyed == Some(true);
        let down = (kind == "keyed" && *keyed == Some(false)) || kind == "disconnected";
        if up {
            self.nodes.retain(|(n, _)| n != node);
            self.nodes.push((node.clone(), None));
            return Some(node.clone());
        }
        if down && let Some(e) = self.nodes.iter_mut().find(|(n, _)| n == node) {
            e.1 = Some(now);
        }
        None
    }

    /// Nodes keyed now or within [`TALKER_WINDOW`], in keying order.
    fn current(&mut self, now: Instant) -> Vec<String> {
        self.nodes
            .retain(|(_, t)| t.is_none_or(|t| now.saturating_duration_since(t) < TALKER_WINDOW));
        self.nodes.iter().map(|(n, _)| n.clone()).collect()
    }
}

/// One over in progress.
struct Over {
    started_at: String,
    /// (talker, network) in the order heard.
    talkers: Vec<(String, &'static str)>,
    /// `None` when the over is being skipped (low disk, an open error).
    file: Option<OpenFile>,
    rate: u32,
}

struct OpenFile {
    writer: FlacWriter,
    path: PathBuf,
    name: String,
    /// The network the name was built with.
    named_for: &'static str,
    next_free_check: Instant,
}

impl Over {
    fn add(&mut self, talker: String, network: &'static str) {
        if !self.talkers.iter().any(|(t, _)| *t == talker) {
            self.talkers.push((talker, network));
        }
    }
}

struct Member {
    handle: astar_iax::BridgeMemberHandle,
    splitter: OverSplitter,
    checked: Instant,
}

struct Worker<'a> {
    logger: &'a NodeLogger,
    source: &'a dyn RecordSource,
    events: Receiver<NodeEvent>,
    talkers: Talkers,
    member: Option<Member>,
    next_enrol: Instant,
    over: Option<Over>,
    frames: Option<(FrameWriter, Receiver<FrameEvent>)>,
    /// The caps are owed a run; it happens once no over is open.
    prune_due: bool,
    /// When the caps next run on their own ([`PRUNE_EVERY`]).
    next_prune: Instant,
}

/// `20261027-010004`, from `2026-10-27T01:00:04Z`.
fn stamp(started_at: &str) -> String {
    let digits: String = started_at.chars().filter(char::is_ascii_digit).collect();
    format!("{}-{}", &digits[..8], &digits[8..14])
}

/// `20261027-010004-allstar-2001`: every piece through `key_component`, so
/// nothing heard off the air can reach the path.
fn base_name(started_at: &str, network: &str, talker: Option<&str>) -> String {
    format!(
        "{}-{}-{}",
        stamp(started_at),
        key_component(network),
        key_component(talker.unwrap_or(""))
    )
}

impl<'a> Worker<'a> {
    fn new(logger: &'a NodeLogger, source: &'a dyn RecordSource) -> Self {
        let events = source.subscribe();
        let frames = logger.frames.then(|| {
            let (tap, rx) = tap_channel(FRAME_TAP_CAPACITY);
            source.set_frame_tap(Some(tap));
            (FrameWriter::new(&logger.dir, env!("CARGO_PKG_VERSION")), rx)
        });
        let now = Instant::now();
        Self {
            logger,
            source,
            events,
            talkers: Talkers::default(),
            member: None,
            next_enrol: now,
            over: None,
            frames,
            prune_due: false,
            next_prune: now + PRUNE_EVERY,
        }
    }

    fn run(mut self) {
        while !self.logger.stop.load(Ordering::Relaxed) {
            self.step(Instant::now());
            std::thread::sleep(TICK);
        }
        self.shutdown();
    }

    /// One pass of the loop at `now`: talkers, the conference mix, codec
    /// frames, then the caps if they are owed and no over is open. Every
    /// time it uses is `now`, so tests drive it with explicit instants.
    fn step(&mut self, now: Instant) {
        self.drain_events(now);
        self.pump_conference(now);
        self.pump_frames(now);
        if now >= self.next_prune {
            self.next_prune = now + PRUNE_EVERY;
            self.prune_due = true;
        }
        if self.prune_due && self.over.is_none() {
            self.prune_due = false;
            self.logger.prune();
        }
        self.publish();
    }

    /// Finishes the open over, withdraws the member, removes the frame tap
    /// and runs any caps still owed.
    fn shutdown(mut self) {
        self.end_over();
        if let Some(m) = self.member.take() {
            self.source.withdraw(m.handle.id);
        }
        if let Some((fw, _rx)) = self.frames.take() {
            self.source.set_frame_tap(None);
            if let Err(e) = fw.finish() {
                tracing::warn!("recording: frame capture did not finish: {e}");
            }
            self.prune_due = true;
        }
        if self.prune_due {
            self.logger.prune();
        }
        let mut sh = self.logger.shared.lock().expect("logger poisoned");
        sh.recording = false;
        sh.enrolled = false;
    }

    fn publish(&self) {
        let mut sh = self.logger.shared.lock().expect("logger poisoned");
        sh.recording = self.over.as_ref().is_some_and(|o| o.file.is_some());
        sh.enrolled = self.member.is_some();
    }

    fn drain_events(&mut self, now: Instant) {
        loop {
            match self.events.try_recv() {
                Ok(ev) => {
                    if let Some(node) = self.talkers.observe(&ev, now)
                        && let Some(o) = self.over.as_mut()
                    {
                        o.add(node, "allstar");
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    // The bus went away; keep recording without talkers.
                    let (_tx, rx) = std::sync::mpsc::channel();
                    self.events = rx;
                    return;
                }
            }
        }
    }

    fn pump_conference(&mut self, now: Instant) {
        if self.member.is_none() && now >= self.next_enrol {
            match self.source.enrol() {
                Some(handle) => {
                    let frame = usize::try_from(handle.sample_rate / 50)
                        .unwrap_or(160)
                        .max(1);
                    let cfg = SplitterConfig {
                        frame: FRAME,
                        ..SplitterConfig::default()
                    };
                    tracing::info!("recording: enrolled at {} Hz", handle.sample_rate);
                    self.member = Some(Member {
                        splitter: OverSplitter::new(cfg, frame),
                        handle,
                        checked: now,
                    });
                }
                None => self.next_enrol = now + ENROL_RETRY,
            }
        }
        let Some(mut m) = self.member.take() else {
            return;
        };
        let mut lost = false;
        // Drain first, then push the whole batch: the frames are
        // back-dated one frame apart from `now`, so a late pass adds no
        // silence, and the end-of-over check runs only once nothing is
        // left queued.
        let mut batch = Vec::new();
        loop {
            match m.handle.from_conference.try_recv() {
                Ok(frame) => batch.push(frame),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    lost = true;
                    break;
                }
            }
        }
        let rate = m.handle.sample_rate;
        for ev in m.splitter.push_batch(&batch, now) {
            self.over_event(ev, rate, now);
        }
        for ev in m.splitter.poll(now) {
            self.over_event(ev, rate, now);
        }
        if !lost && now.saturating_duration_since(m.checked) >= CHECK_EVERY {
            m.checked = now;
            lost = !self.source.still_enrolled(m.handle.id);
        }
        if lost {
            self.end_over();
            self.source.withdraw(m.handle.id);
            self.next_enrol = now + ENROL_RETRY;
            tracing::info!("recording: member withdrawn; re-enrolling when a conference returns");
        } else {
            self.member = Some(m);
        }
    }

    fn over_event(&mut self, ev: OverEvent, rate: u32, now: Instant) {
        match ev {
            OverEvent::Start => self.start_over(rate, now),
            OverEvent::Audio(samples) => self.write(&samples, now),
            OverEvent::End => self.end_over(),
        }
    }

    fn start_over(&mut self, rate: u32, now: Instant) {
        self.end_over();
        let started_at = rfc3339_utc(SystemTime::now());
        let mut over = Over {
            started_at,
            talkers: Vec::new(),
            file: None,
            rate,
        };
        for node in self.talkers.current(now) {
            over.add(node, "allstar");
        }
        if let Some(call) = self.source.m17_recent_talker(TALKER_WINDOW) {
            over.add(call, "m17");
        }
        let _ = std::fs::create_dir_all(&self.logger.dir);
        if self.logger.free_ok() {
            over.file = self.open(&over, now);
        } else {
            tracing::warn!("recording: under 500 MB free; this over is not recorded");
        }
        self.over = Some(over);
    }

    fn open(&self, over: &Over, now: Instant) -> Option<OpenFile> {
        let (talker, network) = over
            .talkers
            .first()
            .map_or((None, "node"), |(t, n)| (Some(t.as_str()), *n));
        let date = &over.started_at[..10];
        let day = self.logger.dir.join(date);
        if let Err(e) = std::fs::create_dir_all(&day) {
            tracing::warn!("recording: cannot create {}: {e}", day.display());
            return None;
        }
        if !is_real_dir(&day) {
            tracing::warn!(
                "recording: {} is not a plain folder; over skipped",
                day.display()
            );
            return None;
        }
        let base = base_name(&over.started_at, network, talker);
        for n in 1u32..1000 {
            let name = if n == 1 {
                format!("{base}.flac")
            } else {
                format!("{base}-{n}.flac")
            };
            let path = day.join(&name);
            // `create_new`: an existing file or a planted symlink is
            // refused, never followed or overwritten.
            return match create_recording(&path, over.rate) {
                Ok(writer) => Some(OpenFile {
                    writer,
                    path,
                    name,
                    named_for: network,
                    next_free_check: now + FREE_CHECK_EVERY,
                }),
                Err(astar_record::RecordError::Io(e))
                    if e.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(e) => {
                    tracing::warn!("recording: cannot start {}: {e}", path.display());
                    None
                }
            };
        }
        None
    }

    fn write(&mut self, samples: &[i16], now: Instant) {
        let Some(over) = self.over.as_mut() else {
            return;
        };
        let Some(file) = over.file.as_mut() else {
            return;
        };
        if let Err(e) = file.writer.push_interleaved(samples) {
            tracing::warn!("recording: write failed, over dropped: {e}");
            if let Some(f) = over.file.take() {
                drop(f.writer);
                remove_logged(&f.path);
            }
            return;
        }
        if now >= file.next_free_check {
            file.next_free_check = now + FREE_CHECK_EVERY;
            if !self.logger.free_ok() {
                tracing::warn!("recording: under 500 MB free; the over is cut short");
                // Keep what was written; skip the rest of the over.
                let mut over = self.over.take().expect("checked above");
                self.finish(&mut over);
                over.file = None;
                self.over = Some(over);
            }
        }
    }

    fn end_over(&mut self) {
        if let Some(mut over) = self.over.take() {
            if over.talkers.is_empty()
                && let Some(call) = self.source.m17_recent_talker(TALKER_WINDOW)
            {
                over.add(call, "m17");
            }
            self.finish(&mut over);
        }
    }

    /// Closes the over's file, renames it if a talker turned up after it
    /// was opened, indexes it and marks the caps due (they run once no
    /// over is open).
    fn finish(&mut self, over: &mut Over) {
        let Some(mut f) = over.file.take() else {
            return;
        };
        let summary = match f.writer.finish() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("recording: cannot finish {}: {e}", f.path.display());
                remove_logged(&f.path);
                return;
            }
        };
        if f.named_for == "node"
            && let Some((talker, network)) = over.talkers.first()
        {
            let name = format!(
                "{}.flac",
                base_name(&over.started_at, network, Some(talker))
            );
            let to = f.path.with_file_name(&name);
            if rename_no_replace(&f.path, &to) {
                f.path = to;
                f.name = name;
            }
        }
        let network = over.talkers.first().map_or("node", |(_, n)| *n);
        let rate = u64::from(over.rate.max(1));
        let row = IndexRow {
            path: format!("{}/{}", &over.started_at[..10], f.name),
            name: f.name,
            started_at: over.started_at.clone(),
            duration_ms: summary.samples_per_channel * 1000 / rate,
            talkers: over.talkers.iter().map(|(t, _)| t.clone()).collect(),
            network: network.to_owned(),
            bytes: summary.bytes,
        };
        debug_assert!(is_plain_relative(&row.path));
        {
            let _files = self.logger.files.lock().expect("logger poisoned");
            if let Err(e) = index::append(&self.logger.dir, &row) {
                tracing::warn!("recording: cannot index {}: {e}", row.name);
            }
        }
        self.prune_due = true;
    }

    fn pump_frames(&mut self, now: Instant) {
        let Some((writer, rx)) = self.frames.as_mut() else {
            return;
        };
        let before = writer.rows().len();
        let mut failed = None;
        while let Ok(mut ev) = rx.try_recv() {
            // A stream that starts on a full disk is not opened: it
            // becomes an End (closing any earlier stream that way),
            // and its frames then find no stream and are dropped.
            if let FrameEvent::Start { dir, .. } = ev
                && !self.logger.free_ok()
            {
                ev = FrameEvent::End { dir };
            }
            if let Err(e) = writer.handle(ev, now) {
                failed = Some(e);
                break;
            }
        }
        if failed.is_none()
            && let Err(e) = writer.poll(now)
        {
            failed = Some(e);
        }
        let grew = writer.rows().len() > before;
        if let Some(e) = failed {
            // Frames are auxiliary: capture stops, audio recording goes on.
            tracing::warn!("recording: frame capture stopped: {e}");
            self.source.set_frame_tap(None);
            self.frames = None;
        }
        if grew {
            self.prune_due = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ByteSize;
    use astar_record::frames::{CaptureMode, Dir};
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;

    const RATE: u32 = 8000;
    const FRAME_SAMPLES: usize = 160;

    /// A fake node: a conference whose mix the test feeds, an event bus,
    /// an M17 last-heard and a frame-tap slot. No network anywhere.
    struct Fake {
        allow: AtomicBool,
        enrolments: AtomicU32,
        enrolled: AtomicBool,
        feed: Mutex<Option<mpsc::Sender<Vec<i16>>>>,
        events: Mutex<Vec<mpsc::Sender<NodeEvent>>>,
        m17: Mutex<Option<String>>,
        frame_tap: Mutex<Option<Tap<FrameEvent>>>,
    }

    impl Fake {
        fn new(allow: bool) -> Arc<Self> {
            Arc::new(Self {
                allow: AtomicBool::new(allow),
                enrolments: AtomicU32::new(0),
                enrolled: AtomicBool::new(false),
                feed: Mutex::new(None),
                events: Mutex::new(Vec::new()),
                m17: Mutex::new(None),
                frame_tap: Mutex::new(None),
            })
        }

        fn keyed(&self, node: &str) {
            for tx in self.events.lock().unwrap().iter() {
                let _ = tx.send(NodeEvent::Link {
                    kind: "keyed".into(),
                    node: node.into(),
                    call: 1,
                    reason: None,
                    keyed: Some(true),
                });
            }
        }

        /// Puts one frame in the member's queue, as the conference would.
        fn send(&self, frame: Vec<i16>) {
            if let Some(tx) = self.feed.lock().unwrap().as_ref() {
                let _ = tx.send(frame);
            }
        }
    }

    /// One frame of noise; the same `seed` start gives the same frames.
    fn noise(seed: &mut u32) -> Vec<i16> {
        (0..FRAME_SAMPLES)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (*seed >> 16) as i16 / 4
            })
            .collect()
    }

    /// A worker with no thread, stepped by hand on an explicit clock: the
    /// conference "produces" one frame every 20 ms of that clock and the
    /// worker drains whatever is queued at each step. No sleeps anywhere.
    struct Driver<'a> {
        w: Worker<'a>,
        fake: &'a Fake,
        t: Instant,
    }

    impl<'a> Driver<'a> {
        /// Enrols on the first step.
        fn new(log: &'a NodeLogger, fake: &'a Fake) -> Self {
            let mut w = Worker::new(log, fake);
            let t = Instant::now();
            w.step(t);
            assert!(fake.enrolled.load(Ordering::SeqCst), "enrolled");
            Self { w, fake, t }
        }

        fn step_by(&mut self, ms: u64) {
            self.t += Duration::from_millis(ms);
            self.w.step(self.t);
        }

        /// `frames` frames one per 20 ms, the worker stepping every 10 ms.
        fn burst(&mut self, frames: u32) {
            self.burst_stepping(frames, &[10]);
        }

        /// `frames` frames one per 20 ms, the worker stepping at the
        /// irregular intervals `gaps` (cycled): frames due by each step
        /// are queued just before it, so a long gap drains several.
        fn burst_stepping(&mut self, frames: u32, gaps: &[u64]) {
            let mut seed = 0x1234_5678u32;
            let t0 = self.t;
            let mut sent = 0u32;
            let mut g = gaps.iter().cycle();
            while sent < frames {
                self.t += Duration::from_millis(*g.next().unwrap());
                while sent < frames && t0 + FRAME * sent <= self.t {
                    self.fake.send(noise(&mut seed));
                    sent += 1;
                }
                self.w.step(self.t);
            }
        }

        /// Quiet for `ms`, stepping every 10 ms.
        fn idle(&mut self, ms: u64) {
            for _ in 0..ms / 10 {
                self.step_by(10);
            }
        }

        fn stop(self) {
            self.w.shutdown();
        }
    }

    impl TapSource for Fake {
        fn enrol(&self) -> Option<astar_iax::BridgeMemberHandle> {
            self.enrolments.fetch_add(1, Ordering::SeqCst);
            if !self.allow.load(Ordering::SeqCst) {
                return None;
            }
            let n = self.enrolments.load(Ordering::SeqCst);
            let (to_conference, _rx) = mpsc::channel();
            let (tx, from_conference) = mpsc::channel();
            *self.feed.lock().unwrap() = Some(tx);
            self.enrolled.store(true, Ordering::SeqCst);
            Some(astar_iax::BridgeMemberHandle {
                id: astar_audio::MemberId::from_raw(u64::from(n)),
                to_conference,
                from_conference,
                sample_rate: RATE,
            })
        }
        fn still_enrolled(&self, _id: astar_audio::MemberId) -> bool {
            self.enrolled.load(Ordering::SeqCst)
        }
        fn withdraw(&self, _id: astar_audio::MemberId) {
            self.enrolled.store(false, Ordering::SeqCst);
        }
    }

    impl RecordSource for Fake {
        fn subscribe(&self) -> Receiver<NodeEvent> {
            let (tx, rx) = mpsc::channel();
            self.events.lock().unwrap().push(tx);
            rx
        }
        fn m17_recent_talker(&self, _within: Duration) -> Option<String> {
            self.m17.lock().unwrap().clone()
        }
        fn set_frame_tap(&self, tap: Option<Tap<FrameEvent>>) {
            *self.frame_tap.lock().unwrap() = tap;
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-logger-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(dir: &Path) -> RecordingCfg {
        RecordingCfg {
            enabled: true,
            dir: dir.to_path_buf(),
            max_bytes: ByteSize {
                bytes: 2_000_000_000,
                text: "2 GB".into(),
            },
            max_age_days: 30,
            frames: false,
        }
    }

    #[allow(clippy::unnecessary_wraps)] // the free-space seam's signature
    fn plenty(_: &Path) -> Option<u64> {
        Some(10_000_000_000)
    }

    fn start(cfg: &RecordingCfg, fake: &Arc<Fake>) -> Arc<NodeLogger> {
        NodeLogger::start_with(cfg, Arc::clone(fake) as Arc<dyn RecordSource>, plenty)
    }

    /// A logger with no thread, for a [`Driver`] or for the folder alone.
    fn quiet(cfg: &RecordingCfg) -> Arc<NodeLogger> {
        NodeLogger::new_with(cfg, plenty)
    }

    fn wait(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Total samples from a FLAC's STREAMINFO.
    fn flac_samples(p: &Path) -> u64 {
        let b = std::fs::read(p).unwrap();
        assert_eq!(&b[..4], b"fLaC");
        let s = &b[8..];
        (u64::from(s[13] & 0x0F) << 32)
            | u64::from(u32::from_be_bytes([s[14], s[15], s[16], s[17]]))
    }

    fn flac_files(dir: &Path) -> Vec<PathBuf> {
        fn walk(d: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "flac") {
                    out.push(p);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out
    }

    /// A row as an earlier run would have left it, with its file.
    fn seed(dir: &Path, days_ago: u64, tag: &str) -> IndexRow {
        let at = SystemTime::now() - Duration::from_secs(days_ago * 86_400);
        let started_at = astar_record::clock::rfc3339_utc(at);
        let name = format!("seed-{tag}.flac");
        let path = format!("{}/{name}", &started_at[..10]);
        std::fs::create_dir_all(dir.join(&started_at[..10])).unwrap();
        std::fs::write(dir.join(&path), vec![1u8; 1000]).unwrap();
        let row = IndexRow {
            name,
            path,
            started_at,
            duration_ms: 1000,
            talkers: vec![],
            network: "node".into(),
            bytes: 1000,
        };
        index::append(dir, &row).unwrap();
        row
    }

    #[test]
    fn a_recording_that_cannot_start_leaves_no_file() {
        let dir = temp("nostart");
        let p = dir.join("x.flac");
        // The file is made, then the encoder refuses a rate FLAC cannot carry.
        assert!(create_recording(&p, u32::MAX).is_err());
        assert!(!p.exists(), "the empty file is removed");
        // A name already taken is AlreadyExists, and the file is not ours.
        std::fs::write(&p, b"theirs").unwrap();
        let e = create_recording(&p, RATE).err().unwrap();
        assert!(
            matches!(&e, astar_record::RecordError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists),
            "{e}"
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"theirs");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rename_never_replaces_an_existing_file() {
        let dir = temp("rename");
        let from = dir.join("a.flac");
        let to = dir.join("b.flac");
        std::fs::write(&from, b"ours").unwrap();
        std::fs::write(&to, b"theirs").unwrap();
        assert!(!rename_no_replace(&from, &to));
        assert_eq!(std::fs::read(&from).unwrap(), b"ours");
        assert_eq!(std::fs::read(&to).unwrap(), b"theirs");
        std::fs::remove_file(&to).unwrap();
        assert!(rename_no_replace(&from, &to));
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"ours");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_file_per_key_up() {
        let dir = temp("two");
        let fake = Fake::new(true);
        let log = quiet(&cfg(&dir));
        let mut d = Driver::new(&log, &fake);
        d.burst(50);
        d.idle(2000);
        // The second over is drained at irregular steps, up to three
        // frames at a time: jitter must splice no silence in.
        d.burst_stepping(50, &[10, 30, 5, 45, 10, 20, 35, 15, 60]);
        d.idle(2000);
        d.stop();
        let rows = index::read_all(&dir);
        assert_eq!(rows.len(), 2, "{rows:?}");
        for r in &rows {
            let samples = flac_samples(&dir.join(&r.path));
            // Exactly 50 voice frames and the 50-frame (1 s) tail.
            assert_eq!(samples, 100 * FRAME_SAMPLES as u64, "{r:?}");
            assert_eq!(r.duration_ms, 2000);
            assert_eq!(r.network, "node");
            assert_eq!(r.talkers, [] as [std::string::String; 0]);
            assert_eq!(r.bytes, std::fs::metadata(dir.join(&r.path)).unwrap().len());
            assert_eq!(r.path, format!("{}/{}", &r.started_at[..10], r.name));
        }
        assert_ne!(rows[0].name, rows[1].name);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stalled_worker_neither_pads_nor_splits_the_over() {
        let dir = temp("stall");
        let fake = Fake::new(true);
        let log = quiet(&cfg(&dir));
        let mut d = Driver::new(&log, &fake);
        d.burst(10);
        // The worker stalls 600 ms while the conference keeps sending:
        // thirty frames wait in the queue and are drained in one step.
        let mut seed = 1u32;
        for _ in 0..30 {
            fake.send(noise(&mut seed));
        }
        d.step_by(600);
        d.burst(10);
        d.idle(2000);
        d.stop();
        let rows = index::read_all(&dir);
        assert_eq!(rows.len(), 1, "one over, not split: {rows:?}");
        let samples = flac_samples(&dir.join(&rows[0].path));
        assert_eq!(samples, (50 + 50) * FRAME_SAMPLES as u64);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_first_talker_names_the_file_and_all_are_listed() {
        let dir = temp("talkers");
        let fake = Fake::new(true);
        let log = quiet(&cfg(&dir));
        let mut d = Driver::new(&log, &fake);
        fake.keyed("2001");
        d.burst(15);
        fake.keyed("2002");
        d.burst(15);
        d.idle(2000);
        d.stop();
        let rows = index::read_all(&dir);
        let r = &rows[0];
        assert!(r.name.contains("-allstar-2001"), "{r:?}");
        assert!(
            Path::new(&r.name).extension().is_some_and(|x| x == "flac"),
            "{r:?}"
        );
        assert_eq!(r.talkers, vec!["2001", "2002"]);
        assert_eq!(r.network, "allstar");
        assert!(dir.join(&r.path).is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_m17_talker_is_named() {
        let dir = temp("m17");
        let fake = Fake::new(true);
        *fake.m17.lock().unwrap() = Some("W1ABC".into());
        let log = quiet(&cfg(&dir));
        let mut d = Driver::new(&log, &fake);
        d.burst(15);
        d.idle(2000);
        d.stop();
        let rows = index::read_all(&dir);
        assert!(rows[0].name.contains("-m17-W1ABC"), "{:?}", rows[0]);
        assert_eq!(rows[0].network, "m17");
        assert_eq!(rows[0].talkers, vec!["W1ABC"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hostile_talker_cannot_escape_the_directory() {
        let dir = temp("hostile");
        let fake = Fake::new(true);
        let log = quiet(&cfg(&dir));
        let mut d = Driver::new(&log, &fake);
        fake.keyed("../../x");
        d.burst(15);
        d.idle(2000);
        d.stop();
        let rows = index::read_all(&dir);
        let r = &rows[0];
        assert!(r.name.contains("______x"), "{r:?}");
        assert!(!r.path.contains(".."), "{r:?}");
        let canon_dir = dir.canonicalize().unwrap();
        let file = dir.join(&r.path).canonicalize().unwrap();
        assert!(
            file.starts_with(&canon_dir),
            "{file:?} escaped {canon_dir:?}"
        );
        assert_eq!(flac_files(&dir), vec![dir.join(&r.path)]);
        // The talker list keeps what was heard, as text.
        assert_eq!(r.talkers, vec!["../../x"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_cap_deletes_oldest_first() {
        // Measure one over's file, then cap a fresh folder at 2.5 of them.
        let probe = temp("cap-probe");
        let fake = Fake::new(true);
        let log = quiet(&cfg(&probe));
        let mut d = Driver::new(&log, &fake);
        d.burst(25);
        d.idle(2000);
        d.stop();
        let size = index::read_all(&probe)[0].bytes;
        std::fs::remove_dir_all(&probe).unwrap();

        let dir = temp("cap");
        let mut c = cfg(&dir);
        c.max_bytes = ByteSize {
            bytes: size * 5 / 2,
            text: "test".into(),
        };
        let fake = Fake::new(true);
        let log = quiet(&c);
        let mut d = Driver::new(&log, &fake);
        let mut names = Vec::new();
        for _ in 1..=3 {
            d.burst(25);
            d.idle(1100);
            let newest = index::read_all(&dir).last().unwrap().clone();
            assert!(!names.contains(&newest.name), "the over closed");
            names.push(newest.name.clone());
        }
        d.stop();
        let rows = index::read_all(&dir);
        let kept: Vec<_> = rows.iter().map(|r| r.name.clone()).collect();
        assert_eq!(kept, names[1..].to_vec());
        assert_eq!(flac_files(&dir).len(), 2);
        assert!(log.status().total_bytes <= c.max_bytes.bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_recordings_age_out_at_start_on_a_quiet_node() {
        let dir = temp("age");
        let old = seed(&dir, 31, "old");
        let young = seed(&dir, 29, "young");
        let log = quiet(&cfg(&dir));
        assert!(!dir.join(&old.path).exists(), "the caps run at start");
        assert!(dir.join(&young.path).is_file());
        assert_eq!(index::read_all(&dir), vec![young]);
        assert_eq!(log.status().total_bytes, log.retain(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_caps_run_hourly_but_wait_for_an_open_over() {
        let dir = temp("hourly");
        let fake = Fake::new(true);
        let log = quiet(&cfg(&dir));
        let mut d = Driver::new(&log, &fake);
        // Aged out while the logger ran, with nothing closing since.
        let old = seed(&dir, 31, "old");
        d.idle(100);
        assert!(dir.join(&old.path).is_file(), "not due yet");
        // Due mid-over: deferred until the over closes.
        d.burst(10);
        d.w.next_prune = d.t;
        d.burst(10);
        assert!(d.w.over.is_some());
        assert!(dir.join(&old.path).is_file(), "never during an over");
        d.idle(2000);
        assert!(!dir.join(&old.path).exists(), "run once the over closed");
        // And on its own an hour later, with no over at all.
        let older = seed(&dir, 40, "older");
        d.step_by(PRUNE_EVERY.as_secs() * 1000);
        assert!(!dir.join(&older.path).exists(), "hourly on a quiet node");
        d.stop();
        assert_eq!(index::read_all(&dir).len(), 1, "only the new over");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pruning_never_touches_files_it_did_not_write() {
        let dir = temp("stray");
        let outside = temp("stray-outside");
        // A stray file at the top, one inside a date folder, and an old
        // index row whose path climbs out of the folder.
        std::fs::write(dir.join("notes.txt"), vec![0u8; 4096]).unwrap();
        let old = seed(&dir, 31, "old");
        let date_dir = dir.join(&old.started_at[..10]);
        std::fs::write(date_dir.join("stray.flac"), vec![0u8; 4096]).unwrap();
        let victim = outside.join("victim.flac");
        std::fs::write(&victim, b"keep me").unwrap();
        let hostile = IndexRow {
            name: "victim.flac".into(),
            path: format!(
                "../{}/victim.flac",
                outside.file_name().unwrap().to_str().unwrap()
            ),
            ..old.clone()
        };
        index::append(&dir, &hostile).unwrap();
        let mut c = cfg(&dir);
        c.max_bytes = ByteSize {
            bytes: 100,
            text: "100 B".into(),
        };
        let fake = Fake::new(true);
        let log = quiet(&c);
        let mut d = Driver::new(&log, &fake);
        d.burst(15);
        d.idle(2000);
        assert_eq!(log.status().state, "idle");
        d.stop();
        assert!(index::read_all(&dir).is_empty(), "pruned to nothing");
        assert!(!dir.join(&old.path).exists(), "its own old file goes");
        assert!(dir.join("notes.txt").is_file());
        assert!(date_dir.join("stray.flac").is_file());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    /// A seeded capture row and its two files.
    fn seed_capture(dir: &Path, days_ago: u64, stem: &str) {
        use astar_record::capture::{AstarExtra, CaptureRow, append_row};
        let at = astar_record::clock::rfc3339_utc(
            SystemTime::now() - Duration::from_secs(days_ago * 86_400),
        );
        let mode = dir.join("captured/codec2-3200");
        std::fs::create_dir_all(mode.join("astar-air")).unwrap();
        std::fs::write(
            mode.join(format!("astar-air/{stem}.ambe")),
            format!("ambe {stem}"),
        )
        .unwrap();
        std::fs::write(
            mode.join(format!("astar-air/{stem}.flac")),
            format!("flac {stem}"),
        )
        .unwrap();
        let row = CaptureRow {
            key: format!("astar-air/{stem}"),
            mode: "codec2-3200".into(),
            frames: 1,
            port: "astar:m17".into(),
            prodid: "codec2".into(),
            version: "0.1.16-beta".into(),
            encode_ms: 0,
            decode_ms: 0,
            sha256_ambe: String::new(),
            sha256_wav: String::new(),
            captured_at: at.clone(),
            attempts: 1,
            astar: AstarExtra {
                talker: Some("W1ABC".into()),
                network: "m17".into(),
                direction: "rx".into(),
                received_at: at,
                lost: vec![],
                vocoder: "codec2".into(),
                trimmed_tail: 0,
                pcm: "decoded".into(),
                dropped_pcm: 0,
            },
        };
        append_row(&mode.join("manifest.jsonl"), &row).unwrap();
    }

    fn untar(bytes: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
        use std::io::Read;
        let mut a = tar::Archive::new(bytes);
        let mut out = std::collections::BTreeMap::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            let p = e.path().unwrap().to_string_lossy().into_owned();
            let mut body = Vec::new();
            e.read_to_end(&mut body).unwrap();
            out.insert(p, body);
        }
        out
    }

    #[test]
    fn a_planned_download_sends_only_what_survives_pruning_and_deletes() {
        let dir = temp("download");
        // Seeded after the start-up caps, as if aged out while running.
        let log = quiet(&cfg(&dir));
        let old = seed(&dir, 31, "old");
        let gone = seed(&dir, 2, "gone");
        let kept = seed(&dir, 1, "kept");
        let old_stem = "20260901-000000-m17-W1ABC-rx";
        let new_stem = "20261027-000000-m17-W1ABC-rx";
        seed_capture(&dir, 31, old_stem);
        seed_capture(&dir, 1, new_stem);
        let kept_bytes = std::fs::read(dir.join(&kept.path)).unwrap();
        let plan = log.plan_download(None);
        assert_eq!(plan.entries.len(), 7, "{:?}", plan.entries);
        // After planning: the old recording and old capture age out, one
        // recording is deleted by hand. Neither waits for the download.
        log.prune();
        assert!(log.delete(&gone.name));
        assert!(!dir.join(&old.path).exists() && !dir.join(&gone.path).exists());
        assert_eq!(log.status().total_bytes, log.retain(false));
        let mut out = Vec::new();
        let summary = astar_record::tarball::write_tar_planned(&mut out, &plan).unwrap();
        assert_eq!((summary.sent, summary.skipped), (3, 4));
        let files = untar(&out);
        let air = "captured/codec2-3200/astar-air";
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec![
                format!("{air}/{new_stem}.ambe"),
                format!("{air}/{new_stem}.flac"),
                "captured/codec2-3200/manifest.jsonl".to_owned(),
                format!("recordings/{}", kept.path),
                "recordings/index.jsonl".to_owned(),
            ]
        );
        assert_eq!(files[&format!("recordings/{}", kept.path)], kept_bytes);
        assert_eq!(
            files[&format!("{air}/{new_stem}.ambe")],
            format!("ambe {new_stem}").as_bytes()
        );
        let index_rows: Vec<IndexRow> = String::from_utf8(files["recordings/index.jsonl"].clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(index_rows, vec![kept.clone()]);
        let manifest =
            String::from_utf8(files["captured/codec2-3200/manifest.jsonl"].clone()).unwrap();
        assert_eq!(manifest.lines().count(), 1);
        assert!(manifest.contains(new_stem) && !manifest.contains(old_stem));
        log.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_since_a_time_leaves_older_rows_out() {
        let dir = temp("download-since");
        let old = seed(&dir, 3, "old");
        let new = seed(&dir, 1, "new");
        let log = quiet(&cfg(&dir));
        let since = SystemTime::now() - Duration::from_secs(2 * 86_400);
        let plan = log.plan_download(Some(since));
        log.stop();
        let names: Vec<_> = plan.entries.iter().map(|e| e.path_in_tar.clone()).collect();
        assert_eq!(names, vec![format!("recordings/{}", new.path)]);
        let index: Vec<String> = plan.ledgers[0]
            .lines
            .iter()
            .map(|l| String::from_utf8(l.bytes.clone()).unwrap())
            .collect();
        assert_eq!(index.len(), 1);
        assert!(index[0].contains(&new.name) && !index[0].contains(&old.name));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_day_folder_is_never_pruned_or_served() {
        let dir = temp("daylink");
        let outside = temp("daylink-outside");
        let old = seed(&dir, 31, "old");
        // Swap the day folder for a symlink to a folder holding a file of
        // the same name.
        let day = dir.join(&old.started_at[..10]);
        std::fs::remove_dir_all(&day).unwrap();
        std::fs::write(outside.join(&old.name), b"keep me").unwrap();
        std::os::unix::fs::symlink(&outside, &day).unwrap();
        // The start-up caps already see the row; prune once more by hand.
        let log = quiet(&cfg(&dir));
        assert_eq!(
            log.plan_download(None).entries,
            [] as [astar_record::tarball::PlannedEntry; 0]
        );
        log.prune();
        assert!(
            index::read_all(&dir).is_empty(),
            "the old row still ages out"
        );
        assert_eq!(std::fs::read(outside.join(&old.name)).unwrap(), b"keep me");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn delete_and_rows_go_by_indexed_name_only() {
        let dir = temp("delete");
        let a = seed(&dir, 2, "a");
        let b = seed(&dir, 1, "b");
        let log = quiet(&cfg(&dir));
        assert_eq!(
            log.rows()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec![b.name.as_str(), a.name.as_str()],
            "newest first"
        );
        assert!(!log.delete("../index.jsonl"));
        assert!(!log.delete(""));
        assert!(log.delete(&a.name));
        assert!(!dir.join(&a.path).exists());
        assert!(!log.delete(&a.name));
        assert_eq!(log.rows(), vec![b]);
        assert_eq!(log.dir(), dir.as_path());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn low_disk_skips_the_over_and_says_so() {
        let dir = temp("lowdisk");
        let fake = Fake::new(true);
        let log = NodeLogger::new_with(&cfg(&dir), |_| Some(1));
        let mut d = Driver::new(&log, &fake);
        d.burst(15);
        assert_eq!(log.status().state, "disk_low");
        d.idle(800);
        let status = log.status();
        d.stop();
        assert_eq!(status.state, "disk_low");
        assert_eq!(status.free_bytes, Some(1));
        assert_eq!(flac_files(&dir), [] as [std::path::PathBuf; 0]);
        assert_eq!(
            index::read_all(&dir),
            [] as [crate::recording::index::IndexRow; 0]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parrot_mode_records_nothing() {
        let dir = temp("parrot");
        let fake = Fake::new(false);
        let log = start(&cfg(&dir), &fake);
        wait("an enrol attempt", || {
            fake.enrolments.load(Ordering::SeqCst) > 0
        });
        std::thread::sleep(Duration::from_millis(200));
        let status = log.status();
        log.stop();
        assert_eq!(status.state, "no_conference");
        assert_eq!(flac_files(&dir), [] as [std::path::PathBuf; 0]);
        assert_eq!(
            index::read_all(&dir),
            [] as [crate::recording::index::IndexRow; 0]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn frame_capture_clears_disk_low_once_space_is_back() {
        static FREE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let dir = temp("frames-disk");
        let mut c = cfg(&dir);
        c.frames = true;
        let fake = Fake::new(false);
        let log = NodeLogger::start_with(&c, Arc::clone(&fake) as Arc<dyn RecordSource>, |_| {
            Some(FREE.load(Ordering::SeqCst))
        });
        wait("the frame tap", || fake.frame_tap.lock().unwrap().is_some());
        let tap = fake.frame_tap.lock().unwrap().clone().unwrap();
        let start = || FrameEvent::Start {
            dir: Dir::Rx,
            network: "m17",
            mode: CaptureMode::Codec2_3200,
            talker: Some("W1ABC".into()),
            at: SystemTime::now(),
            vocoder: "codec2",
        };
        assert!(tap.offer(start()));
        wait("disk_low", || log.status().state == "disk_low");
        assert!(
            !dir.join("captured").exists(),
            "nothing opened on a low disk"
        );
        FREE.store(10_000_000_000, Ordering::SeqCst);
        assert!(tap.offer(start()));
        wait("disk_low cleared", || log.status().state == "no_conference");
        assert_eq!(log.status().free_bytes, Some(10_000_000_000));
        log.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn m17_frames_go_to_codec2_3200() {
        let dir = temp("frames");
        let mut c = cfg(&dir);
        c.frames = true;
        let fake = Fake::new(false);
        let log = start(&c, &fake);
        wait("the frame tap", || fake.frame_tap.lock().unwrap().is_some());
        let tap = fake.frame_tap.lock().unwrap().clone().unwrap();
        assert!(tap.offer(FrameEvent::Start {
            dir: Dir::Rx,
            network: "m17",
            mode: CaptureMode::Codec2_3200,
            talker: Some("W1ABC".into()),
            at: SystemTime::now(),
            vocoder: "codec2",
        }));
        for _ in 0..10 {
            assert!(tap.offer(FrameEvent::Frame {
                dir: Dir::Rx,
                bytes: vec![0x55; 8],
                seq: None,
            }));
            assert!(tap.offer(FrameEvent::Pcm {
                dir: Dir::Rx,
                pcm: [100; 160],
            }));
        }
        assert!(tap.offer(FrameEvent::End { dir: Dir::Rx }));
        let manifest = dir.join("captured/codec2-3200/manifest.jsonl");
        wait("a manifest row", || {
            astar_record::capture::read_rows(&manifest).is_ok_and(|r| r.len() == 1)
        });
        log.stop();
        let row = &astar_record::capture::read_rows(&manifest).unwrap()[0];
        assert_eq!(row.frames, 10);
        assert_eq!(row.port, "astar:m17");
        assert!(row.key.starts_with("astar-air/"), "{}", row.key);
        assert!(row.key.contains("-m17-W1ABC-rx"), "{}", row.key);
        assert!(
            fake.frame_tap.lock().unwrap().is_none(),
            "stop removes the tap"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
