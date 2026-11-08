// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! How a call is laid out on disk: one stereo file, two mono files, or one
//! mixed mono file.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::RecordError;
use crate::flac::{FlacSummary, FlacWriter};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    /// Receive left, transmit right, one file.
    #[default]
    Stereo,
    /// A receive file and a transmit file.
    TwoMono,
    /// Receive and transmit summed into one mono file.
    Mixed,
}

impl Layout {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Stereo => "stereo",
            Layout::TwoMono => "two-mono",
            Layout::Mixed => "mixed",
        }
    }

    /// 0 stereo, 1 two-mono, 2 mixed.
    #[must_use]
    pub fn from_ffi(v: i32) -> Option<Layout> {
        match v {
            0 => Some(Layout::Stereo),
            1 => Some(Layout::TwoMono),
            2 => Some(Layout::Mixed),
            _ => None,
        }
    }
}

impl FromStr for Layout {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "stereo" => Ok(Layout::Stereo),
            "two-mono" => Ok(Layout::TwoMono),
            "mixed" => Ok(Layout::Mixed),
            other => Err(format!("unknown layout `{other}`")),
        }
    }
}

enum Sink {
    Stereo(Box<FlacWriter>),
    TwoMono {
        rx: Box<FlacWriter>,
        tx: Box<FlacWriter>,
    },
    Mixed(Box<FlacWriter>),
}

/// Writes each aligned tick to the file(s) the layout calls for.
pub struct LayoutWriter {
    sink: Sink,
    paths: Vec<PathBuf>,
    scratch: Vec<i16>,
}

pub(crate) fn with_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = base.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// `FlacWriter::create_new`: an existing file (or a planted symlink) is an
/// `AlreadyExists` error and is never followed or overwritten. A file made
/// before the writer gave up is removed; one that already existed is not
/// ours and is left alone.
fn create_new_flac(path: &Path, rate: u32, channels: u8) -> Result<FlacWriter, RecordError> {
    FlacWriter::create_new(path, rate, channels).inspect_err(|e| {
        if !is_already_exists(e) {
            let _ = std::fs::remove_file(path);
        }
    })
}

/// Whether `e` is the `AlreadyExists` of a name someone else took.
#[must_use]
pub fn is_already_exists(e: &RecordError) -> bool {
    matches!(e, RecordError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists)
}

impl LayoutWriter {
    /// `base` has no extension; the layout appends its own. Every file is
    /// opened with `create_new`: a name that already exists fails with
    /// `AlreadyExists` (see [`is_already_exists`]) and nothing is
    /// overwritten, so the caller picks the next unique base and retries.
    pub fn create(
        base: &Path,
        layout: Layout,
        sample_rate: u32,
    ) -> Result<LayoutWriter, RecordError> {
        let (sink, paths) = match layout {
            Layout::Stereo => {
                let p = with_suffix(base, ".flac");
                (
                    Sink::Stereo(Box::new(create_new_flac(&p, sample_rate, 2)?)),
                    vec![p],
                )
            }
            Layout::TwoMono => {
                let (prx, ptx) = (with_suffix(base, " rx.flac"), with_suffix(base, " tx.flac"));
                let rx = create_new_flac(&prx, sample_rate, 1)?;
                let tx = match create_new_flac(&ptx, sample_rate, 1) {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = rx.finish();
                        let _ = std::fs::remove_file(&prx);
                        return Err(e);
                    }
                };
                (
                    Sink::TwoMono {
                        rx: Box::new(rx),
                        tx: Box::new(tx),
                    },
                    vec![prx, ptx],
                )
            }
            Layout::Mixed => {
                let p = with_suffix(base, ".flac");
                (
                    Sink::Mixed(Box::new(create_new_flac(&p, sample_rate, 1)?)),
                    vec![p],
                )
            }
        };
        Ok(LayoutWriter {
            sink,
            paths,
            scratch: Vec::new(),
        })
    }

    /// Writes one tick. `rx` and `tx` must be the same length.
    pub fn write_tick(&mut self, rx: &[i16], tx: &[i16]) -> Result<(), RecordError> {
        debug_assert_eq!(rx.len(), tx.len());
        match &mut self.sink {
            Sink::Stereo(w) => {
                self.scratch.clear();
                for (&r, &t) in rx.iter().zip(tx) {
                    self.scratch.push(r);
                    self.scratch.push(t);
                }
                w.push_interleaved(&self.scratch)
            }
            Sink::TwoMono { rx: wr, tx: wt } => {
                wr.push_interleaved(rx)?;
                wt.push_interleaved(tx)
            }
            Sink::Mixed(w) => {
                self.scratch.clear();
                self.scratch
                    .extend(rx.iter().zip(tx).map(|(&r, &t)| r.saturating_add(t)));
                w.push_interleaved(&self.scratch)
            }
        }
    }

    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.paths.clone()
    }

    /// Samples per channel written so far.
    #[must_use]
    pub fn samples(&self) -> u64 {
        match &self.sink {
            Sink::Stereo(w) | Sink::Mixed(w) => w.samples_per_channel(),
            Sink::TwoMono { rx, .. } => rx.samples_per_channel(),
        }
    }

    pub fn finish(self) -> Result<Vec<FlacSummary>, RecordError> {
        match self.sink {
            Sink::Stereo(w) | Sink::Mixed(w) => Ok(vec![(*w).finish()?]),
            Sink::TwoMono { rx, tx } => {
                let a = (*rx).finish();
                let b = (*tx).finish();
                Ok(vec![a?, b?])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("astar-record-layout-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[allow(clippy::cast_possible_truncation)]
    fn decode(path: &Path) -> (u32, Vec<i16>) {
        let mut r = claxon::FlacReader::open(path).unwrap();
        let ch = r.streaminfo().channels;
        (ch, r.samples().map(|s| s.unwrap() as i16).collect())
    }

    #[test]
    fn stereo_puts_receive_left_and_transmit_right() {
        let d = dir("stereo");
        let base = d.join("call");
        let mut w = LayoutWriter::create(&base, Layout::Stereo, 16000).unwrap();
        for _ in 0..4 {
            w.write_tick(&[1; 320], &[2; 320]).unwrap();
        }
        assert_eq!(w.paths(), vec![d.join("call.flac")]);
        assert_eq!(w.samples(), 1280);
        let s = w.finish().unwrap();
        assert_eq!(s.len(), 1);
        let (ch, pcm) = decode(&s[0].path);
        assert_eq!(ch, 2);
        assert_eq!(pcm.len(), 2560);
        assert!(pcm.chunks(2).all(|p| p == [1, 2]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_mono_writes_rx_and_tx_files_together() {
        let d = dir("two");
        let base = d.join("call");
        let mut w = LayoutWriter::create(&base, Layout::TwoMono, 16000).unwrap();
        for _ in 0..3 {
            w.write_tick(&[5; 320], &[-6; 320]).unwrap();
        }
        assert_eq!(
            w.paths(),
            vec![d.join("call rx.flac"), d.join("call tx.flac")]
        );
        let s = w.finish().unwrap();
        assert_eq!(s.len(), 2);
        let (c0, rx) = decode(&s[0].path);
        let (c1, tx) = decode(&s[1].path);
        assert_eq!((c0, c1), (1, 1));
        assert_eq!((rx.len(), tx.len()), (960, 960));
        assert!(rx.iter().all(|&v| v == 5) && tx.iter().all(|&v| v == -6));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mixed_sums_and_clips() {
        let d = dir("mixed");
        let base = d.join("call");
        let mut w = LayoutWriter::create(&base, Layout::Mixed, 16000).unwrap();
        let mut rx = vec![0i16; 320];
        let mut tx = vec![0i16; 320];
        (rx[0], tx[0]) = (30000, 10000);
        (rx[1], tx[1]) = (-30000, -10000);
        (rx[2], tx[2]) = (100, -40);
        w.write_tick(&rx, &tx).unwrap();
        let s = w.finish().unwrap();
        let (ch, pcm) = decode(&s[0].path);
        assert_eq!(ch, 1);
        assert_eq!(&pcm[..3], &[32767, -32768, 60]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let d = dir("exists");
        let base = d.join("call");
        std::fs::write(d.join("call.flac"), b"theirs").unwrap();
        let e = LayoutWriter::create(&base, Layout::Stereo, 16000)
            .err()
            .unwrap();
        assert!(is_already_exists(&e), "{e}");
        assert_eq!(std::fs::read(d.join("call.flac")).unwrap(), b"theirs");
        // Two-mono: tx taken, so the rx file made first is removed again.
        std::fs::write(d.join("call tx.flac"), b"theirs").unwrap();
        let e = LayoutWriter::create(&base, Layout::TwoMono, 16000)
            .err()
            .unwrap();
        assert!(is_already_exists(&e), "{e}");
        assert!(!d.join("call rx.flac").exists());
        assert_eq!(std::fs::read(d.join("call tx.flac")).unwrap(), b"theirs");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn layout_names_round_trip() {
        for l in [Layout::Stereo, Layout::TwoMono, Layout::Mixed] {
            assert_eq!(l.as_str().parse::<Layout>().unwrap(), l);
            assert_eq!(
                serde_json::to_string(&l).unwrap(),
                format!("\"{}\"", l.as_str())
            );
        }
        assert_eq!(Layout::from_ffi(1), Some(Layout::TwoMono));
        assert_eq!(Layout::from_ffi(3), None);
        assert!("nope".parse::<Layout>().is_err());
    }
}
