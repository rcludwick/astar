// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Appending one line to an append-only log (`log.jsonl`, `pictures.jsonl`).
//!
//! A log can be left with half a line at its end when a write is cut short
//! (a crash, a full disk). That torn tail is trimmed before the next line
//! goes on, so the new line starts at the beginning of a line and parses.
//! A log that is not a regular file — a symlink, a folder, a FIFO — is
//! refused rather than followed, so a planted link can't redirect the write.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is not a regular file", path.display()),
    )
}

/// Cut the file back to just after its last newline, when it does not end
/// with one.
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

/// Append `line` and a newline to the log at `path`, creating it, and sync
/// it to disk. `line` must not contain a newline of its own.
///
/// The path is checked before opening and again after (on the open file
/// and on the path), so a log swapped for a link between the two is caught
/// too.
///
/// # Errors
/// I/O errors; `InvalidInput` when the log is not a regular file or `line`
/// holds a newline.
pub fn append_line(path: &Path, line: &[u8]) -> io::Result<()> {
    if line.contains(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a log line can't contain a newline",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_file() => return Err(not_regular(path)),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    if !f.metadata()?.file_type().is_file()
        || !std::fs::symlink_metadata(path)?.file_type().is_file()
    {
        return Err(not_regular(path));
    }
    drop_torn_tail(&mut f)?;
    f.seek(SeekFrom::End(0))?;
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line);
    bytes.push(b'\n');
    // One write, so a reader never sees half a line from a whole append.
    f.write_all(&bytes)?;
    f.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astar-record-jsonl-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn appends_lines_and_creates_the_log() {
        let dir = temp("append");
        let log = dir.join("a.jsonl");
        append_line(&log, b"{\"a\":1}").unwrap();
        append_line(&log, b"{\"a\":2}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_tail_is_trimmed_before_appending() {
        let dir = temp("torn");
        let log = dir.join("a.jsonl");
        std::fs::write(&log, "{\"a\":1}\n{\"a\":").unwrap();
        append_line(&log, b"{\"a\":2}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_with_a_newline_is_refused() {
        let dir = temp("newline");
        let log = dir.join("a.jsonl");
        let err = append_line(&log, b"one\ntwo").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!log.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_log_is_refused_and_its_target_untouched() {
        let dir = temp("symlink");
        let target = dir.join("elsewhere.txt");
        std::fs::write(&target, b"keep").unwrap();
        let log = dir.join("a.jsonl");
        std::os::unix::fs::symlink(&target, &log).unwrap();
        let err = append_line(&log, b"{}").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_in_the_logs_place_is_refused() {
        let dir = temp("folder");
        let log = dir.join("a.jsonl");
        std::fs::create_dir(&log).unwrap();
        assert!(append_line(&log, b"{}").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
