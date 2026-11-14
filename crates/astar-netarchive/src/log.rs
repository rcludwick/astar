// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `log.jsonl` (append-only) and `session.json` (replaced atomically).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::model::{LogRow, SessionMeta};

pub const LOG_FILE: &str = "log.jsonl";
pub const META_FILE: &str = "session.json";

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is not a regular file", path.display()),
    )
}

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

/// Appends one row and syncs it. A log that is not a regular file (a
/// symlink, a folder, a FIFO) is refused, never followed.
///
/// # Errors
/// I/O errors; `InvalidInput` when the log is not a regular file.
pub fn append_row(session_dir: &Path, row: &LogRow) -> io::Result<()> {
    let mut line = serde_json::to_string(row).map_err(io::Error::other)?;
    line.push('\n');
    let path = session_dir.join(LOG_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if !m.file_type().is_file() => return Err(not_regular(&path)),
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
    if !f.metadata()?.file_type().is_file()
        || !std::fs::symlink_metadata(&path)?.file_type().is_file()
    {
        return Err(not_regular(&path));
    }
    drop_torn_tail(&mut f)?;
    f.seek(SeekFrom::End(0))?;
    f.write_all(line.as_bytes())?;
    f.sync_data()
}

/// Every row, oldest first. A missing log is empty; a line that does not
/// parse (a torn last write) is skipped.
///
/// # Errors
/// I/O errors other than the log not existing.
pub fn read_rows(session_dir: &Path) -> io::Result<Vec<LogRow>> {
    // Bytes, not text: a write torn inside a character is not valid UTF-8, and
    // that must cost one line rather than the whole log.
    let bytes = match std::fs::read(session_dir.join(LOG_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(bytes
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect())
}

/// Writes `session.json` through a temporary file and a rename.
///
/// # Errors
/// I/O errors writing or renaming.
pub fn write_meta(session_dir: &Path, meta: &SessionMeta) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
    replace_file(session_dir, META_FILE, &bytes)
}

/// Put `bytes` in `session_dir/name` through a temporary file and a rename. A
/// link at `name` is replaced, never followed.
pub(crate) fn replace_file(session_dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let path = session_dir.join(name);
    let tmp = session_dir.join(format!("{name}.tmp"));
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Reads `session.json`.
///
/// # Errors
/// I/O errors, or `InvalidData` when it does not parse.
pub fn read_meta(session_dir: &Path) -> io::Result<SessionMeta> {
    let bytes = std::fs::read(session_dir.join(META_FILE))?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Ended, LogKind};
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-netarchive-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn row(kind: LogKind) -> LogRow {
        LogRow {
            t: "2026-11-03T02:00:12Z".to_owned(),
            kind,
        }
    }

    fn meta(overs: u32, ended: Option<Ended>) -> SessionMeta {
        SessionMeta {
            format: crate::model::META_FORMAT,
            net: "Tuesday M17 net".to_owned(),
            network: "m17".to_owned(),
            target: "example.org:17000 C".to_owned(),
            window_start: "2026-11-03T18:55:00-07:00".to_owned(),
            window_end: "2026-11-03T20:00:00-07:00".to_owned(),
            visibility: "private".to_owned(),
            my_side: false,
            sample_rate: 8000,
            started_at: "2026-11-04T01:55:00Z".to_owned(),
            ended_at: None,
            ended,
            rejoins: 0,
            dropped_frames: 0,
            overs,
            astar_version: "0.1.19-beta".to_owned(),
            my_callsigns: Vec::new(),
        }
    }

    #[test]
    fn append_then_read_roundtrips_every_kind() {
        let d = temp("rt");
        let rows = vec![
            row(LogKind::Joined),
            row(LogKind::Over {
                start: "2026-11-03T02:00:12Z".to_owned(),
                end: "2026-11-03T02:00:20Z".to_owned(),
                duration_ms: 8000,
                offset_ms: 312_000,
                talker: Some("NC0NTL".to_owned()),
                file: "001 19-00-12 NC0NTL.flac".to_owned(),
            }),
            row(LogKind::Dropped {
                reason: "timeout".to_owned(),
            }),
            row(LogKind::Rejoined),
            row(LogKind::Left),
            row(LogKind::Error {
                message: "boom".to_owned(),
            }),
        ];
        for r in &rows {
            append_row(&d, r).unwrap();
        }
        assert_eq!(read_rows(&d).unwrap(), rows);
    }

    #[test]
    fn torn_tail_is_trimmed_before_append() {
        let d = temp("torn");
        std::fs::write(d.join(LOG_FILE), br#"{"t":"x","kind":"jo"#).unwrap();
        append_row(&d, &row(LogKind::Joined)).unwrap();
        assert_eq!(read_rows(&d).unwrap().len(), 1);
    }

    #[test]
    fn refuses_symlinked_log() {
        let d = temp("sym");
        let target = d.join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, d.join(LOG_FILE)).unwrap();
        assert!(append_row(&d, &row(LogKind::Joined)).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"");
    }

    #[test]
    fn read_rows_survives_a_row_torn_inside_a_character() {
        let d = temp("tornutf8");
        append_row(&d, &row(LogKind::Joined)).unwrap();
        let mut bytes = std::fs::read(d.join(LOG_FILE)).unwrap();
        bytes.extend_from_slice(b"{\"t\":\"x\",\"kind\":\"error\",\"message\":\"\xC3");
        std::fs::write(d.join(LOG_FILE), bytes).unwrap();
        assert_eq!(read_rows(&d).unwrap(), vec![row(LogKind::Joined)]);
    }

    #[test]
    fn a_session_json_that_does_not_parse_is_invalid_data() {
        let d = temp("badmeta");
        std::fs::write(d.join(META_FILE), "not json").unwrap();
        assert_eq!(
            read_meta(&d).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn session_json_says_its_format_and_an_old_one_reads_as_format_one() {
        let d = temp("format");
        write_meta(&d, &meta(1, None)).unwrap();
        let text = std::fs::read_to_string(d.join(META_FILE)).unwrap();
        assert!(text.contains("\"format\": 1"), "{text}");
        let old: Vec<&str> = text.lines().filter(|l| !l.contains("format")).collect();
        std::fs::write(d.join(META_FILE), old.join("\n")).unwrap();
        assert_eq!(read_meta(&d).unwrap().format, 1);
    }

    #[test]
    fn meta_write_is_atomic() {
        let d = temp("meta");
        write_meta(&d, &meta(1, None)).unwrap();
        let second = meta(2, Some(Ended::Window));
        write_meta(&d, &second).unwrap();
        assert!(!d.join("session.json.tmp").exists());
        assert_eq!(read_meta(&d).unwrap(), second);
    }
}
