// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `peaks.json`: how loud each stretch of each over is, for the page's
//! waveforms.
//!
//! Every over is cut into [`PEAK_BINS`] equal spans. A span's value is its
//! loudest sample as a whole percent of full scale (0 to 100), so the page can
//! draw a waveform without downloading and decoding any audio.

use std::collections::BTreeMap;
use std::io::{self, BufReader};
use std::path::Path;

use astar_record::tarball::open_regular;
use serde::{Deserialize, Serialize};

use crate::log::replace_file;
use crate::summary::OverRec;

pub const PEAKS_FILE: &str = "peaks.json";
/// Bars per over.
pub const PEAK_BINS: usize = 48;

/// The peaks of one session.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Peaks {
    /// Always 1.
    pub format: u32,
    pub bins: u32,
    /// Keyed by the over's `file` from `log.jsonl`.
    pub overs: BTreeMap<String, Vec<u8>>,
}

/// Peaks for every over whose file can be opened. A missing or linked file has
/// no entry; a file that will not decode has zeros, and one that fails part
/// way keeps what decoded before the failure.
#[must_use]
pub fn compute_peaks(session_dir: &Path, overs: &[OverRec]) -> Peaks {
    let mut out = BTreeMap::new();
    let dir = session_dir.join("overs");
    let real_dir = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir());
    if real_dir {
        for over in overs {
            if let Ok(Some((file, _))) = open_regular(&dir.join(&over.file)) {
                out.insert(over.file.clone(), bins_of(&decode_peaks(file)));
            }
        }
    }
    Peaks {
        format: 1,
        bins: u32::try_from(PEAK_BINS).unwrap_or(u32::MAX),
        overs: out,
    }
}

/// The absolute peak of each frame, whatever decoded.
fn decode_peaks(file: std::fs::File) -> Vec<u16> {
    let mut frames = Vec::new();
    let Ok(mut reader) = claxon::FlacReader::new(BufReader::new(file)) else {
        return frames;
    };
    let info = reader.streaminfo();
    let channels = info.channels;
    let shift = info.bits_per_sample.saturating_sub(16);
    let mut blocks = reader.blocks();
    let mut buf = Vec::new();
    while let Ok(Some(block)) = blocks.read_next_or_eof(buf) {
        for i in 0..block.duration() {
            let m = (0..channels)
                .map(|c| block.sample(c, i).unsigned_abs() >> shift)
                .max()
                .unwrap_or(0);
            frames.push(u16::try_from(m.min(32768)).unwrap_or(u16::MAX));
        }
        buf = block.into_buffer();
    }
    frames
}

fn bins_of(frames: &[u16]) -> Vec<u8> {
    let mut max = vec![0u32; PEAK_BINS];
    let n = frames.len().max(1);
    for (i, &f) in frames.iter().enumerate() {
        // i < n, so the bin is below PEAK_BINS.
        let bin = (i * PEAK_BINS / n).min(PEAK_BINS - 1);
        max[bin] = max[bin].max(u32::from(f));
    }
    max.into_iter()
        .map(|m| u8::try_from((m * 100).div_ceil(32768).min(100)).unwrap_or(100))
        .collect()
}

/// Writes `peaks.json` through a temporary file and a rename.
///
/// # Errors
/// I/O errors writing or renaming.
pub fn write_peaks(session_dir: &Path, p: &Peaks) -> io::Result<()> {
    let bytes = serde_json::to_vec(p).map_err(io::Error::other)?;
    replace_file(session_dir, PEAKS_FILE, &bytes)
}

/// Reads `peaks.json`. `None` when it is missing, a link, or does not parse
/// (the last two log a warning).
///
/// # Errors
/// I/O errors other than the file being absent or refused.
pub fn read_peaks(session_dir: &Path) -> io::Result<Option<Peaks>> {
    let path = session_dir.join(PEAKS_FILE);
    let Some((mut file, _)) = open_regular(&path)? else {
        if std::fs::symlink_metadata(&path).is_ok() {
            tracing::warn!("{} is not a plain file; ignored", path.display());
        }
        return Ok(None);
    };
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut file, &mut bytes)?;
    match serde_json::from_slice(&bytes) {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            tracing::warn!("{} does not parse: {e}", path.display());
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_record::FlacWriter;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-peaks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("overs")).unwrap();
        d
    }

    fn rec(file: &str) -> OverRec {
        OverRec {
            start: "2026-11-03T02:00:00Z".to_owned(),
            end: "2026-11-03T02:00:02Z".to_owned(),
            duration_ms: 1920,
            talker: None,
            file: file.to_owned(),
        }
    }

    fn flac(path: &Path, samples: &[i16]) {
        let mut w = FlacWriter::create(path, 8000, 1).unwrap();
        w.push_interleaved(samples).unwrap();
        w.finish().unwrap();
    }

    fn half_and_half() -> Vec<i16> {
        let mut s = vec![0i16; 7680];
        s.extend((0..7680).map(|i| if i % 2 == 0 { 32767 } else { -32767 }));
        s
    }

    #[test]
    fn peaks_of_half_silence_half_full_scale() {
        let d = temp("half");
        flac(&d.join("overs/a.flac"), &half_and_half());
        let p = compute_peaks(&d, &[rec("a.flac")]);
        let bins = &p.overs["a.flac"];
        assert_eq!(bins.len(), 48);
        assert!(bins[..24].iter().all(|&b| b == 0), "{bins:?}");
        assert!(bins[24..].iter().all(|&b| b == 100), "{bins:?}");
    }

    #[test]
    fn peaks_skip_missing_and_symlinked_over_files() {
        let d = temp("skip");
        flac(&d.join("real.flac"), &half_and_half());
        std::os::unix::fs::symlink(d.join("real.flac"), d.join("overs/link.flac")).unwrap();
        let p = compute_peaks(&d, &[rec("link.flac"), rec("missing.flac")]);
        assert!(p.overs.is_empty());
    }

    #[test]
    fn peaks_of_a_torn_flac_keep_what_decodes() {
        let d = temp("torn");
        let path = d.join("overs/t.flac");
        flac(&path, &half_and_half());
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() * 60 / 100]).unwrap();
        let p = compute_peaks(&d, &[rec("t.flac")]);
        assert_eq!(p.overs["t.flac"].len(), 48);
    }

    #[test]
    fn peaks_of_garbage_are_zeros() {
        let d = temp("junk");
        std::fs::write(d.join("overs/j.flac"), b"not flac").unwrap();
        let p = compute_peaks(&d, &[rec("j.flac")]);
        assert_eq!(p.overs["j.flac"], vec![0u8; 48]);
    }

    #[test]
    fn peaks_json_roundtrips_and_absent_reads_none() {
        let d = temp("rt");
        assert_eq!(read_peaks(&d).unwrap(), None);
        flac(&d.join("overs/a.flac"), &half_and_half());
        let p = compute_peaks(&d, &[rec("a.flac")]);
        write_peaks(&d, &p).unwrap();
        assert_eq!(read_peaks(&d).unwrap(), Some(p));
        std::fs::write(d.join(PEAKS_FILE), b"{nope").unwrap();
        assert_eq!(read_peaks(&d).unwrap(), None);
    }
}
