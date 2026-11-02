// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The manifest row astar appends for each captured stream. It is
//! unamblify's `CaptureRow` plus one extra field, `astar`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A run of codec frames that never arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LostRun {
    pub at_frame: u32,
    pub frames: u32,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What astar knows that unamblify's row has no field for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AstarExtra {
    pub talker: Option<String>,
    pub network: String,
    pub direction: String,
    pub received_at: String,
    pub lost: Vec<LostRun>,
    pub vocoder: String,
    pub trimmed_tail: u32,
    pub pcm: String,
    /// Decoded blocks dropped because they had no raw frame to pair with.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped_pcm: u32,
}

/// One line of `captured/<mode>/manifest.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRow {
    pub key: String,
    pub mode: String,
    pub frames: u32,
    pub port: String,
    pub prodid: String,
    pub version: String,
    pub encode_ms: u64,
    pub decode_ms: u64,
    pub sha256_ambe: String,
    pub sha256_wav: String,
    pub captured_at: String,
    pub attempts: u32,
    pub astar: AstarExtra,
}

/// Cuts a torn last line (one with no newline, so never completed) off the
/// end of the file.
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

/// Appends one line and syncs it. A torn last line from a crash is cut
/// off first, so the new row starts on a line of its own.
pub fn append_row(manifest: &Path, row: &CaptureRow) -> io::Result<()> {
    let mut line = serde_json::to_string(row).map_err(io::Error::other)?;
    line.push('\n');
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(manifest)?;
    drop_torn_tail(&mut f)?;
    f.seek(SeekFrom::End(0))?;
    f.write_all(line.as_bytes())?;
    f.sync_data()
}

/// Every row in the manifest. A last line that does not parse (a torn
/// write) is skipped; a bad line anywhere else is an error.
pub fn read_rows(manifest: &Path) -> io::Result<Vec<CaptureRow>> {
    let text = std::fs::read_to_string(manifest)?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut rows = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        match serde_json::from_str(l) {
            Ok(r) => rows.push(r),
            Err(_) if i + 1 == lines.len() => {}
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }
    Ok(rows)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn row(key: &str) -> CaptureRow {
        CaptureRow {
            key: key.to_owned(),
            mode: "dstar".to_owned(),
            frames: 50,
            port: "astar:dstar".to_owned(),
            prodid: "AMBE3000".to_owned(),
            version: "0.1.16-beta".to_owned(),
            encode_ms: 0,
            decode_ms: 0,
            sha256_ambe: "a".repeat(64),
            sha256_wav: "b".repeat(64),
            captured_at: "2026-10-26T19:00:09Z".to_owned(),
            attempts: 1,
            astar: AstarExtra {
                talker: Some("W1ABC".to_owned()),
                network: "dstar".to_owned(),
                direction: "rx".to_owned(),
                received_at: "2026-10-26T19:00:04Z".to_owned(),
                lost: vec![LostRun {
                    at_frame: 10,
                    frames: 2,
                }],
                vocoder: "hardware".to_owned(),
                trimmed_tail: 0,
                dropped_pcm: 0,
                pcm: "decoded".to_owned(),
            },
        }
    }

    #[test]
    fn a_torn_last_line_is_skipped() {
        let d = std::env::temp_dir().join(format!("astar-capture-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let m = d.join("manifest.jsonl");
        append_row(&m, &row("astar-air/one")).unwrap();
        append_row(&m, &row("astar-air/two")).unwrap();
        let mut f = OpenOptions::new().append(true).open(&m).unwrap();
        f.write_all(b"{\"key\":\"astar-air/thr").unwrap();
        drop(f);
        let rows = read_rows(&m).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].key, "astar-air/two");
        // appending after the tear drops it and the new row reads back
        append_row(&m, &row("astar-air/four")).unwrap();
        let rows = read_rows(&m).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], row("astar-air/four"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_bad_line_in_the_middle_is_an_error() {
        let d = std::env::temp_dir().join(format!("astar-capture-mid-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let m = d.join("manifest.jsonl");
        std::fs::write(&m, "garbage\n").unwrap();
        append_row(&m, &row("astar-air/one")).unwrap();
        assert!(read_rows(&m).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
