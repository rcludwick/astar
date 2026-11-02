// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `index.jsonl`: one line per recording the node logger wrote.
//!
//! The index is the logger's ledger: retention deletes only files a row
//! here names, so a file the logger did not write is never touched.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The index's file name, directly under the recordings folder.
pub const INDEX_FILE: &str = "index.jsonl";

/// One recording: one over the conference carried.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexRow {
    /// The file name, e.g. `20261027-010004-allstar-2001.flac`.
    pub name: String,
    /// Relative to the recordings folder: `2026-10-27/<name>`.
    pub path: String,
    /// RFC 3339, UTC, whole seconds.
    pub started_at: String,
    pub duration_ms: u64,
    /// Everyone heard during the over, first talker first. Off the air:
    /// render as text only.
    pub talkers: Vec<String>,
    /// `"allstar"`, `"m17"` or `"node"` (nobody known).
    pub network: String,
    pub bytes: u64,
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join(INDEX_FILE)
}

/// Cuts a torn last line (no newline: a write that never finished).
fn drop_torn_tail(f: &mut File) -> io::Result<()> {
    let mut bytes = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.read_to_end(&mut bytes)?;
    if bytes.last().is_some_and(|&b| b != b'\n') {
        let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        f.set_len(keep as u64)?;
    }
    Ok(())
}

/// Appends one row and syncs it. An index that is not a regular file (a
/// symlink planted in its place, a folder, a FIFO) is refused, never
/// followed or written through.
///
/// # Errors
/// Any I/O error opening or writing the index; `InvalidInput` when the
/// index is not a regular file.
pub fn append(dir: &Path, row: &IndexRow) -> io::Result<()> {
    let mut line = serde_json::to_string(row).map_err(io::Error::other)?;
    line.push('\n');
    let path = index_path(dir);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if !m.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // The check above and the open are two steps; a symlink swapped in
    // between is caught here, before anything is written.
    if !f.metadata()?.file_type().is_file()
        || !std::fs::symlink_metadata(&path)?.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    drop_torn_tail(&mut f)?;
    f.seek(SeekFrom::End(0))?;
    f.write_all(line.as_bytes())?;
    f.sync_data()
}

/// Every row, oldest first. A missing index is empty; a line that does not
/// parse (a torn write, a hand edit) is skipped.
#[must_use]
pub fn read_all(dir: &Path) -> Vec<IndexRow> {
    let Ok(text) = std::fs::read_to_string(index_path(dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Writes `bytes` to `path` through a temporary file beside it and a
/// `rename`, so a reader sees the old file or the new one, never half.
pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // A leftover temporary file (or a symlink planted in its place) is
    // removed, never followed: `remove_file` unlinks a link itself, and
    // `create_new` refuses anything that reappears.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Replaces the whole index with `rows`.
///
/// # Errors
/// Any I/O error writing the temporary file or renaming it.
pub fn rewrite(dir: &Path, rows: &[IndexRow]) -> io::Result<()> {
    let mut text = String::new();
    for r in rows {
        text.push_str(&serde_json::to_string(r).map_err(io::Error::other)?);
        text.push('\n');
    }
    replace_file(&index_path(dir), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-index-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn row(name: &str) -> IndexRow {
        IndexRow {
            name: format!("{name}.flac"),
            path: format!("2026-10-27/{name}.flac"),
            started_at: "2026-10-27T01:00:04Z".to_owned(),
            duration_ms: 2000,
            talkers: vec!["2001".to_owned()],
            network: "allstar".to_owned(),
            bytes: 1234,
        }
    }

    #[test]
    fn rows_round_trip_and_a_missing_index_is_empty() {
        let d = temp("rt");
        assert_eq!(read_all(&d), [] as [crate::recording::index::IndexRow; 0]);
        append(&d, &row("a")).unwrap();
        append(&d, &row("b")).unwrap();
        assert_eq!(read_all(&d), vec![row("a"), row("b")]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_torn_line_is_skipped_and_cut_before_the_next_append() {
        let d = temp("torn");
        append(&d, &row("a")).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(d.join(INDEX_FILE))
            .unwrap();
        f.write_all(b"{\"name\":\"tor").unwrap();
        drop(f);
        assert_eq!(read_all(&d), vec![row("a")]);
        append(&d, &row("c")).unwrap();
        assert_eq!(read_all(&d), vec![row("a"), row("c")]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rewrite_replaces_the_index_through_a_rename() {
        let d = temp("rewrite");
        for n in ["a", "b", "c"] {
            append(&d, &row(n)).unwrap();
        }
        rewrite(&d, &[row("c")]).unwrap();
        assert_eq!(read_all(&d), vec![row("c")]);
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(INDEX_FILE)]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_as_the_temporary_file_is_not_followed() {
        let d = temp("tmplink");
        let target = d.join("target");
        std::fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, d.join(format!("{INDEX_FILE}.tmp"))).unwrap();
        rewrite(&d, &[row("a")]).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        assert_eq!(read_all(&d), vec![row("a")]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn append_refuses_a_symlinked_or_non_regular_index() {
        let d = temp("applink");
        let target = d.join("target");
        std::fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, d.join(INDEX_FILE)).unwrap();
        let e = append(&d, &row("a")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        std::fs::remove_file(d.join(INDEX_FILE)).unwrap();
        std::fs::create_dir(d.join(INDEX_FILE)).unwrap();
        let e = append(&d, &row("a")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
