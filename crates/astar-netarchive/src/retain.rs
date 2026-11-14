// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Retention and crash recovery for the archive.
//!
//! The archive is `root/<net>/<session>/`. A *session* is a real directory
//! (never a symlink) directly under a real `<net>` directory that holds a
//! regular `session.json`. [`retain`] is the only code that deletes
//! recordings, so it checks every level with `symlink_metadata` and leaves
//! alone anything it cannot positively identify as one of its own.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::log::{META_FILE, read_meta, read_rows, write_meta};
use crate::model::{Ended, LogKind, SessionMeta};
use crate::summary::{overs_from_rows, write_checkins, write_log_csv};

/// Below this many free bytes, recording refuses to start or stops. The same
/// 500 MB the node's recorder uses.
pub use astar_record::disk::MIN_FREE_BYTES;

/// How much of the archive to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainPolicy {
    pub max_bytes: u64,
    pub max_age: Duration,
}

/// What [`retain`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainReport {
    pub deleted: Vec<PathBuf>,
    /// The archive's size after the deletions.
    pub kept_bytes: u64,
}

/// Bytes free to an unprivileged writer on the volume holding `path`.
///
/// # Errors
/// An error when the volume cannot be measured.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    astar_record::disk::free_bytes(path)
        .ok_or_else(|| io::Error::other(format!("cannot read free space under {}", path.display())))
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// One session folder found under the archive.
pub(crate) struct Found {
    pub(crate) net_dir: PathBuf,
    pub(crate) dir: PathBuf,
    /// `None` when `session.json` could not be read or parsed.
    pub(crate) meta: Option<SessionMeta>,
}

fn sub_dirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if is_real_dir(&path) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

pub(crate) fn find_sessions(root: &Path) -> io::Result<Vec<Found>> {
    let nets = match sub_dirs(root) {
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for net_dir in nets {
        let sessions = match sub_dirs(&net_dir) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("cannot list {}: {e}", net_dir.display());
                continue;
            }
        };
        for dir in sessions {
            if !is_regular_file(&dir.join(META_FILE)) {
                continue;
            }
            let meta = match read_meta(&dir) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!(
                        "leaving {} alone: unreadable session.json: {e}",
                        dir.display()
                    );
                    None
                }
            };
            out.push(Found {
                net_dir: net_dir.clone(),
                dir,
                meta,
            });
        }
    }
    Ok(out)
}

/// Bytes under `dir`, never following a symlink.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match fs::symlink_metadata(e.path()) {
            Ok(m) if m.file_type().is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

struct Candidate {
    dir: PathBuf,
    net_dir: PathBuf,
    size: u64,
    /// `ended_at`, else `started_at`.
    age_time: Option<jiff::Timestamp>,
    /// `started_at`, else `ended_at`.
    order_time: Option<jiff::Timestamp>,
}

fn parse(s: Option<&String>) -> Option<jiff::Timestamp> {
    s.and_then(|s| s.parse().ok())
}

/// Whether `dir` is one of the `busy` folders. A folder that cannot be
/// resolved cannot be compared, so it counts as busy and is left alone.
fn is_busy(dir: &Path, busy: &[PathBuf]) -> bool {
    let Ok(canon) = fs::canonicalize(dir) else {
        return true;
    };
    busy.iter()
        .any(|b| b == dir || fs::canonicalize(b).is_ok_and(|bc| bc == canon))
}

/// Remove a session folder, `session.json` last. If anything fails part way,
/// the folder is still recognised as a session and a later pass finishes it.
fn remove_session(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.file_name().is_some_and(|n| n == META_FILE) {
            continue;
        }
        if is_real_dir(&path) {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    fs::remove_file(dir.join(META_FILE))?;
    fs::remove_dir(dir)
}

/// Delete `c` after checking, again, that it is still a real folder inside a
/// real net folder inside `root`.
fn delete(root: &Path, c: &Candidate, deleted: &mut Vec<PathBuf>) -> bool {
    let ok = is_real_dir(root)
        && is_real_dir(&c.net_dir)
        && c.net_dir.parent() == Some(root)
        && is_real_dir(&c.dir)
        && c.dir.parent() == Some(c.net_dir.as_path())
        && is_regular_file(&c.dir.join(META_FILE));
    if !ok {
        tracing::warn!(
            "not deleting {}: it no longer looks like a session",
            c.dir.display()
        );
        return false;
    }
    match remove_session(&c.dir) {
        Ok(()) => {
            deleted.push(c.dir.clone());
            true
        }
        Err(e) => {
            tracing::warn!("could not delete {}: {e}", c.dir.display());
            false
        }
    }
}

/// Delete finished sessions older than `policy.max_age`, then the oldest
/// finished ones while the archive is over `policy.max_bytes`.
///
/// Sessions in `busy`, sessions without `ended`, and sessions whose
/// `session.json` cannot be read are never deleted, though their size counts
/// towards the total.
///
/// `root` is resolved once on entry, so a symlinked `root` works; the folders
/// below it must still be real folders. The paths in the report are spelled
/// under the `root` that was passed in.
///
/// # Errors
/// I/O errors listing `root`.
pub fn retain(
    root: &Path,
    policy: &RetainPolicy,
    now: jiff::Timestamp,
    busy: &[PathBuf],
) -> io::Result<RetainReport> {
    let given = root;
    let root = match fs::canonicalize(root) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(RetainReport {
                deleted: Vec::new(),
                kept_bytes: 0,
            });
        }
        Err(e) => return Err(e),
    };
    let root = root.as_path();
    let mut total = 0u64;
    let mut candidates = Vec::new();
    for f in find_sessions(root)? {
        let size = dir_size(&f.dir);
        total += size;
        let Some(meta) = f.meta else { continue };
        if meta.ended.is_none() || is_busy(&f.dir, busy) {
            continue;
        }
        let started = parse(Some(&meta.started_at));
        let ended = parse(meta.ended_at.as_ref());
        if started.is_none() && ended.is_none() {
            continue;
        }
        candidates.push(Candidate {
            dir: f.dir,
            net_dir: f.net_dir,
            size,
            age_time: ended.or(started),
            order_time: started.or(ended),
        });
    }

    let mut deleted = Vec::new();
    let mut left = Vec::new();
    for c in candidates {
        let age = c.age_time.map(|t| now.duration_since(t));
        let old = age.is_some_and(|a| a.is_positive() && a.unsigned_abs() > policy.max_age);
        if old && delete(root, &c, &mut deleted) {
            total = total.saturating_sub(c.size);
        } else {
            left.push(c);
        }
    }
    left.sort_by_key(|c| c.order_time);
    for c in left {
        if total <= policy.max_bytes {
            break;
        }
        if delete(root, &c, &mut deleted) {
            total = total.saturating_sub(c.size);
        }
    }
    let deleted = deleted
        .into_iter()
        .map(|d| {
            d.strip_prefix(root)
                .map_or(d.clone(), |rel| given.join(rel))
        })
        .collect();
    Ok(RetainReport {
        deleted,
        kept_bytes: total,
    })
}

/// Why [`delete_session`] did not delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteRefusal {
    /// `ended` is not set: the session is still recording, or it crashed and
    /// recovery has not closed it yet.
    Open,
    /// `dir` is not a real session folder two levels under the archive root.
    NotASession,
    /// The checks passed and the removal failed part way.
    Failed,
}

/// Delete one finished session, on a user's request.
///
/// `dir` must be a real folder (not a symlink) inside a real net folder
/// directly under the resolved `root`, holding a regular `session.json` that
/// parses and has `ended` set. Removal goes through the same checks and the
/// same `session.json`-last order as [`retain`].
///
/// # Errors
/// A [`DeleteRefusal`] saying why nothing (or not everything) was deleted.
pub fn delete_session(root: &Path, dir: &Path) -> Result<(), DeleteRefusal> {
    use std::path::Component;
    let root = fs::canonicalize(root).map_err(|_| DeleteRefusal::NotASession)?;
    // No `..` or `.` anywhere: "two levels under root" must mean exactly
    // that, and the scan never spells a path with them.
    if dir
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(DeleteRefusal::NotASession);
    }
    let mut tail = dir.components().rev();
    let (Some(Component::Normal(session)), Some(Component::Normal(net))) =
        (tail.next(), tail.next())
    else {
        return Err(DeleteRefusal::NotASession);
    };
    let holder = dir
        .parent()
        .and_then(Path::parent)
        .ok_or(DeleteRefusal::NotASession)?;
    // The folder above the net may be spelled through a link (a symlinked
    // root); the net and session folders themselves may not be links.
    if fs::canonicalize(holder).ok().as_deref() != Some(root.as_path()) {
        return Err(DeleteRefusal::NotASession);
    }
    let net_dir = root.join(net);
    let dir = net_dir.join(session);
    if !is_real_dir(&net_dir) || !is_real_dir(&dir) || !is_regular_file(&dir.join(META_FILE)) {
        return Err(DeleteRefusal::NotASession);
    }
    let meta = read_meta(&dir).map_err(|_| DeleteRefusal::NotASession)?;
    if meta.ended.is_none() {
        return Err(DeleteRefusal::Open);
    }
    let candidate = Candidate {
        dir,
        net_dir,
        size: 0,
        age_time: None,
        order_time: None,
    };
    let mut deleted = Vec::new();
    if delete(&root, &candidate, &mut deleted) {
        Ok(())
    } else {
        Err(DeleteRefusal::Failed)
    }
}

/// Fill in what a crash kept a session from writing: the `overs` and `rejoins`
/// counts, `log.csv` and `checkins.txt`, all from whatever `log.jsonl` still
/// holds. `dropped_frames` lived only in memory, so it stays as it was. A
/// summary that cannot be written is logged, and the session is still closed.
fn rebuild_summary(dir: &Path, meta: &mut SessionMeta) {
    let rows = match read_rows(dir) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("cannot read the log of {}: {e}", dir.display());
            return;
        }
    };
    let overs = overs_from_rows(&rows);
    let rejoins = rows
        .iter()
        .filter(|r| matches!(r.kind, LogKind::Rejoined))
        .count();
    meta.overs = u32::try_from(overs.len()).unwrap_or(u32::MAX);
    meta.rejoins = u32::try_from(rejoins).unwrap_or(u32::MAX);
    for written in [
        write_log_csv(dir, &overs),
        write_checkins(dir, &overs, &meta.network),
    ] {
        if let Err(e) = written {
            tracing::warn!("cannot finish the summary of {}: {e}", dir.display());
        }
    }
}

/// Mark every session with `ended: None` as [`Ended::Crashed`], ended at
/// `now`, and rebuild its summary from its log. `session.json` is written
/// last, so a pass that fails part way is done again at the next start. The
/// FLACs need no repair: a length-unknown header is valid. Returns the folders
/// it changed.
///
/// # Errors
/// I/O errors listing `root`. A session that cannot be rewritten is logged
/// and skipped.
pub fn recover_unfinished(root: &Path, now: jiff::Timestamp) -> io::Result<Vec<PathBuf>> {
    let mut fixed = Vec::new();
    for f in find_sessions(root)? {
        let Some(mut meta) = f.meta else { continue };
        if meta.ended.is_some() {
            continue;
        }
        rebuild_summary(&f.dir, &mut meta);
        meta.ended = Some(Ended::Crashed);
        meta.ended_at = Some(now.to_string());
        match write_meta(&f.dir, &meta) {
            Ok(()) => fixed.push(f.dir),
            Err(e) => tracing::warn!("could not mark {} crashed: {e}", f.dir.display()),
        }
    }
    Ok(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{LOG_FILE, append_row};
    use crate::model::{LogKind, LogRow};
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "netarchive-retain-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn meta(started: &str, ended: Option<&str>) -> SessionMeta {
        SessionMeta {
            format: crate::model::META_FORMAT,
            net: "n".into(),
            network: "m17".into(),
            target: "h:1 C".into(),
            window_start: started.into(),
            window_end: started.into(),
            visibility: "private".into(),
            my_side: false,
            sample_rate: 8000,
            started_at: started.into(),
            ended_at: ended.map(str::to_owned),
            ended: ended.map(|_| Ended::Window),
            rejoins: 0,
            dropped_frames: 0,
            overs: 0,
            astar_version: "t".into(),
            my_callsigns: Vec::new(),
        }
    }

    /// A session folder with a 10 000-byte `session.flac`.
    fn session(root: &Path, net: &str, name: &str, m: &SessionMeta) -> PathBuf {
        let dir = root.join(net).join(name);
        fs::create_dir_all(&dir).unwrap();
        write_meta(&dir, m).unwrap();
        fs::write(dir.join("session.flac"), vec![0u8; 10_000]).unwrap();
        dir
    }

    fn now() -> jiff::Timestamp {
        "2026-11-05T00:00:00Z".parse().unwrap()
    }

    fn day(n: u32) -> String {
        format!("2026-10-{n:02}T02:00:00Z")
    }

    #[test]
    fn deletes_by_age_then_size() {
        let root = temp("agesize");
        let old = session(
            &root,
            "net",
            "old",
            &meta("2026-08-01T01:00:00Z", Some("2026-08-01T02:00:00Z")),
        );
        let a = session(&root, "net", "a", &meta(&day(10), Some(&day(10))));
        let b = session(&root, "net", "b", &meta(&day(20), Some(&day(20))));
        let c = session(&root, "net", "c", &meta(&day(30), Some(&day(30))));
        let policy = RetainPolicy {
            max_bytes: 25_000,
            max_age: Duration::from_secs(60 * 86_400),
        };
        let r = retain(&root, &policy, now(), &[]).unwrap();
        assert_eq!(r.deleted, vec![old.clone(), a.clone()]);
        assert!(!old.exists() && !a.exists());
        assert!(b.exists() && c.exists());
        assert!(
            r.kept_bytes <= 25_000 && r.kept_bytes >= 20_000,
            "{}",
            r.kept_bytes
        );
    }

    #[test]
    fn never_deletes_busy_or_unfinished() {
        let root = temp("busy");
        let busy = session(
            &root,
            "net",
            "busy",
            &meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z")),
        );
        let open = session(&root, "net", "open", &meta("2026-01-01T01:00:00Z", None));
        let done = session(
            &root,
            "net",
            "done",
            &meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z")),
        );
        fs::write(root.join(".netrec-state.json"), "{}").unwrap();
        fs::write(root.join(".netrec.lock"), "").unwrap();
        let policy = RetainPolicy {
            max_bytes: 0,
            max_age: Duration::from_secs(86_400),
        };
        let r = retain(&root, &policy, now(), std::slice::from_ref(&busy)).unwrap();
        assert_eq!(r.deleted, vec![done]);
        assert!(busy.exists() && open.exists());
        assert!(root.join(".netrec-state.json").exists());
        assert!(root.join(".netrec.lock").exists());
        // The total still counts the sessions it could not delete.
        assert!(r.kept_bytes >= 20_000);
    }

    #[test]
    fn leaves_an_unreadable_session_alone() {
        let root = temp("unreadable");
        let dir = root.join("net").join("bad");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(META_FILE), "not json").unwrap();
        let policy = RetainPolicy {
            max_bytes: 0,
            max_age: Duration::ZERO,
        };
        let r = retain(&root, &policy, now(), &[]).unwrap();
        assert_eq!(r.deleted, Vec::<PathBuf>::new());
        assert!(dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_session() {
        use std::os::unix::fs::symlink;
        let root = temp("symlink");
        let outside = temp("symlink-outside");
        let target = session(
            &outside,
            "x",
            "victim",
            &meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z")),
        );
        fs::create_dir_all(root.join("net")).unwrap();
        symlink(&target, root.join("net").join("linked")).unwrap();
        // A whole net folder that is a symlink is refused too.
        symlink(outside.join("x"), root.join("linknet")).unwrap();
        let policy = RetainPolicy {
            max_bytes: 0,
            max_age: Duration::ZERO,
        };
        let r = retain(&root, &policy, now(), &[]).unwrap();
        assert!(r.deleted.is_empty(), "{:?}", r.deleted);
        assert!(target.join("session.flac").exists());
        assert!(target.join(META_FILE).exists());
        // Recovery does not follow them either.
        assert_eq!(
            recover_unfinished(&root, now()).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn age_pass_deletes_with_a_generous_size_limit() {
        let root = temp("ageonly");
        let old = session(
            &root,
            "net",
            "old",
            &meta("2026-08-01T01:00:00Z", Some("2026-08-01T02:00:00Z")),
        );
        let fresh = session(&root, "net", "fresh", &meta(&day(30), Some(&day(30))));
        let policy = RetainPolicy {
            max_bytes: u64::MAX,
            max_age: Duration::from_secs(60 * 86_400),
        };
        let r = retain(&root, &policy, now(), &[]).unwrap();
        assert_eq!(r.deleted, vec![old.clone()]);
        assert!(!old.exists() && fresh.exists());
    }

    #[test]
    fn ended_at_decides_age_and_started_at_is_the_fallback() {
        let root = temp("agebasis");
        // Started long ago, ended recently: young, so kept.
        let long = session(
            &root,
            "net",
            "long",
            &meta("2026-08-01T01:00:00Z", Some(&day(30))),
        );
        // No usable ended_at: started_at decides, and it is old.
        let mut m = meta("2026-08-01T01:00:00Z", Some("garbage"));
        m.ended_at = Some("garbage".into());
        let fallback = session(&root, "net", "fallback", &m);
        let policy = RetainPolicy {
            max_bytes: u64::MAX,
            max_age: Duration::from_secs(60 * 86_400),
        };
        let r = retain(&root, &policy, now(), &[]).unwrap();
        assert_eq!(r.deleted, vec![fallback.clone()]);
        assert!(long.exists() && !fallback.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_works_but_a_symlinked_session_is_still_refused() {
        use std::os::unix::fs::symlink;
        let real = temp("rootreal");
        let holder = temp("rootlink");
        let link = holder.join("archive");
        symlink(&real, &link).unwrap();
        let old = session(
            &real,
            "net",
            "old",
            &meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z")),
        );
        let outside = temp("rootoutside");
        let victim = session(
            &outside,
            "x",
            "victim",
            &meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z")),
        );
        symlink(&victim, real.join("net").join("linked")).unwrap();
        let policy = RetainPolicy {
            max_bytes: 0,
            max_age: Duration::ZERO,
        };
        let r = retain(&link, &policy, now(), &[]).unwrap();
        assert!(!old.exists());
        assert_eq!(r.deleted, vec![link.join("net").join("old")]);
        assert!(victim.join(META_FILE).exists());
        assert!(victim.join("session.flac").exists());
    }

    #[test]
    fn session_json_goes_last() {
        let root = temp("jsonlast");
        let dir = session(
            &root,
            "net",
            "s",
            &meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z")),
        );
        // A file that cannot be removed as a file stops the pass before
        // session.json is touched: a non-empty folder named like a file is
        // removed fine, so use a read-only parent instead.
        let sub = dir.join("overs");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("1.flac"), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).unwrap();
            let policy = RetainPolicy {
                max_bytes: 0,
                max_age: Duration::ZERO,
            };
            let r = retain(&root, &policy, now(), &[]).unwrap();
            let root_user = fs::File::create(sub.join("probe")).is_ok();
            fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
            if !root_user {
                assert_eq!(r.deleted, Vec::<PathBuf>::new());
                assert!(dir.join(META_FILE).exists(), "still a recognised session");
            }
        }
    }

    #[test]
    fn recover_marks_crashed() {
        let root = temp("recover");
        let open = session(&root, "net", "open", &meta("2026-11-04T01:00:00Z", None));
        let done = session(
            &root,
            "net",
            "done",
            &meta("2026-11-04T01:00:00Z", Some("2026-11-04T02:00:00Z")),
        );
        let fixed = recover_unfinished(&root, now()).unwrap();
        assert_eq!(fixed, vec![open.clone()]);
        let m = read_meta(&open).unwrap();
        assert_eq!(m.ended, Some(Ended::Crashed));
        assert_eq!(m.ended_at.as_deref(), Some("2026-11-05T00:00:00Z"));
        assert_eq!(read_meta(&done).unwrap().ended, Some(Ended::Window));
    }

    /// The log of a session that died part way, as `append_row` left it.
    fn crashed_log(dir: &Path) {
        let over = |n: u32, talker: Option<&str>, file: &str| LogRow {
            t: format!("2026-11-04T01:0{n}:10Z"),
            kind: LogKind::Over {
                start: format!("2026-11-04T01:0{n}:00Z"),
                end: format!("2026-11-04T01:0{n}:10Z"),
                duration_ms: 10_400,
                offset_ms: 60_000 * u64::from(n),
                talker: talker.map(str::to_owned),
                file: file.to_owned(),
            },
        };
        let plain = |kind| LogRow {
            t: "2026-11-04T01:00:00Z".to_owned(),
            kind,
        };
        for row in [
            plain(LogKind::Joined),
            over(1, Some("NC0NTL"), "001 19-01-00 NC0NTL.flac"),
            plain(LogKind::Dropped {
                reason: "keepalive lost".to_owned(),
            }),
            plain(LogKind::Rejoined),
            over(2, Some("=KX\u{7}0AAA"), "002 19-02-00 KX_0AAA.flac"),
            plain(LogKind::Dropped {
                reason: "keepalive lost".to_owned(),
            }),
            plain(LogKind::Rejoined),
            over(3, Some("NC0NTL"), "003 19-03-00 NC0NTL.flac"),
        ] {
            append_row(dir, &row).unwrap();
        }
    }

    #[test]
    fn recovery_rebuilds_the_summary() {
        let root = temp("rebuild");
        let open = session(&root, "net", "open", &meta("2026-11-04T01:00:00Z", None));
        crashed_log(&open);
        recover_unfinished(&root, now()).unwrap();

        let m = read_meta(&open).unwrap();
        assert_eq!(m.ended, Some(Ended::Crashed));
        assert_eq!(m.overs, 3);
        assert_eq!(m.rejoins, 2);
        // Nothing on disk says how many frames were lost.
        assert_eq!(m.dropped_frames, 0);

        let csv = fs::read_to_string(open.join("log.csv")).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "start,end,duration_s,talker,file");
        assert_eq!(lines.len(), 4, "{csv}");
        assert_eq!(
            lines[1],
            "2026-11-04T01:01:00Z,2026-11-04T01:01:10Z,10.4,NC0NTL,001 19-01-00 NC0NTL.flac"
        );
        assert!(lines[2].contains(",'=KX\u{7}0AAA,"), "{csv}");
        assert_eq!(
            fs::read_to_string(open.join("checkins.txt")).unwrap(),
            "NC0NTL\n=KX0AAA\n"
        );
    }

    #[test]
    fn recovery_keeps_what_a_torn_log_still_holds() {
        let root = temp("tornlog");
        let open = session(&root, "net", "open", &meta("2026-11-04T01:00:00Z", None));
        crashed_log(&open);
        // The power went in the middle of a character of the last row.
        let mut log = OpenOptions::new()
            .append(true)
            .open(open.join(LOG_FILE))
            .unwrap();
        log.write_all(b"{\"t\":\"2026-11-04T01:04:10Z\",\"kind\":\"over\",\"talker\":\"\xC3")
            .unwrap();
        drop(log);

        assert_eq!(
            recover_unfinished(&root, now()).unwrap(),
            vec![open.clone()]
        );
        let m = read_meta(&open).unwrap();
        assert_eq!((m.overs, m.rejoins), (3, 2));
        assert_eq!(
            fs::read_to_string(open.join("log.csv"))
                .unwrap()
                .lines()
                .count(),
            4
        );
    }

    #[test]
    fn recovery_with_no_log_still_marks_and_summarises() {
        let root = temp("nolog");
        let open = session(&root, "net", "open", &meta("2026-11-04T01:00:00Z", None));
        recover_unfinished(&root, now()).unwrap();
        let m = read_meta(&open).unwrap();
        assert_eq!(m.ended, Some(Ended::Crashed));
        assert_eq!((m.overs, m.rejoins), (0, 0));
        assert_eq!(
            fs::read_to_string(open.join("log.csv")).unwrap(),
            "start,end,duration_s,talker,file\n"
        );
        assert_eq!(fs::read_to_string(open.join("checkins.txt")).unwrap(), "");
    }

    #[test]
    fn recovery_marks_a_session_whose_summary_cannot_be_written() {
        let root = temp("noscv");
        let open = session(&root, "net", "open", &meta("2026-11-04T01:00:00Z", None));
        crashed_log(&open);
        fs::create_dir(open.join("log.csv")).unwrap();
        // An unreadable log gives the session no rows, and it still closes.
        let dead = session(&root, "net", "dead", &meta("2026-11-04T01:00:00Z", None));
        fs::create_dir(dead.join(LOG_FILE)).unwrap();
        assert_eq!(recover_unfinished(&root, now()).unwrap().len(), 2);
        let m = read_meta(&open).unwrap();
        assert_eq!(m.ended, Some(Ended::Crashed));
        assert_eq!(m.overs, 3);
        assert!(open.join("checkins.txt").is_file());
        assert_eq!(read_meta(&dead).unwrap().ended, Some(Ended::Crashed));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_does_not_write_through_a_planted_link() {
        use std::os::unix::fs::symlink;
        let root = temp("csvlink");
        let outside = temp("csvlink-outside");
        let victim = outside.join("victim");
        fs::write(&victim, "keep").unwrap();
        let open = session(&root, "net", "open", &meta("2026-11-04T01:00:00Z", None));
        symlink(&victim, open.join("log.csv")).unwrap();
        recover_unfinished(&root, now()).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn delete_refuses_an_open_session() {
        let root = temp("delopen");
        let open = session(&root, "net", "open", &meta("2026-11-04T01:00:00Z", None));
        assert_eq!(delete_session(&root, &open), Err(DeleteRefusal::Open));
        assert!(open.join(META_FILE).exists());
    }

    #[test]
    fn delete_refuses_a_dir_outside_root() {
        let root = temp("delroot");
        let outside = temp("delroot-outside");
        let done = meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z"));
        let victim = session(&outside, "net", "victim", &done);
        let mine = session(&root, "net", "mine", &done);
        assert_eq!(
            delete_session(&root, &victim),
            Err(DeleteRefusal::NotASession)
        );
        // Too shallow, too deep, or spelled with `..`.
        assert_eq!(
            delete_session(&root, &root.join("net")),
            Err(DeleteRefusal::NotASession)
        );
        let deep = session(&root, "net/mine", "deeper", &done);
        assert_eq!(
            delete_session(&root, &deep),
            Err(DeleteRefusal::NotASession)
        );
        let dotted = root.join("net").join("..").join("net").join("mine");
        assert_eq!(
            delete_session(&root, &dotted),
            Err(DeleteRefusal::NotASession)
        );
        #[cfg(unix)]
        {
            let link = root.join("net").join("linked");
            std::os::unix::fs::symlink(&victim, &link).unwrap();
            assert_eq!(
                delete_session(&root, &link),
                Err(DeleteRefusal::NotASession)
            );
        }
        assert!(victim.join(META_FILE).exists());
        assert!(mine.join(META_FILE).exists());
        // A folder with no session.json is not a session either.
        let empty = root.join("net").join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            delete_session(&root, &empty),
            Err(DeleteRefusal::NotASession)
        );
        assert!(empty.exists());
    }

    #[test]
    fn delete_removes_a_finished_session() {
        let root = temp("deldone");
        let done = meta("2026-01-01T01:00:00Z", Some("2026-01-01T02:00:00Z"));
        let dir = session(&root, "net", "done", &done);
        fs::create_dir_all(dir.join("overs")).unwrap();
        fs::write(dir.join("overs").join("001.flac"), b"x").unwrap();
        let keep = session(&root, "net", "keep", &done);
        assert_eq!(delete_session(&root, &dir), Ok(()));
        assert!(!dir.exists());
        assert!(keep.join(META_FILE).exists());
        // The scan's spelling (under the resolved root) works too.
        let canon = fs::canonicalize(&keep).unwrap();
        assert_eq!(delete_session(&root, &canon), Ok(()));
        assert!(!keep.exists());
    }

    #[test]
    fn free_bytes_reads_a_real_folder() {
        assert!(free_bytes(&std::env::temp_dir()).unwrap() > 0);
        assert!(free_bytes(Path::new("/definitely/not/here")).is_err());
    }
}
