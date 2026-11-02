// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Writes unamblify capture files from codec-frame events: a `.ambe` of the
//! raw frames, a decoded mono 8 kHz `.flac`, and one manifest row per stream.
//!
//! Known limit: `FrameEvent` carries no index, so a `Frame` and its `Pcm`
//! are paired by arrival order. If the bounded event queue drops an item in
//! the middle of a stream, later pairs can shift by one with nothing to
//! detect it. Drops are rare because the frame queue is generous, and the
//! visible signals are `astar.dropped_pcm` and the lost runs in the row.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::capture::{AstarExtra, CaptureRow, LostRun, append_row};
use crate::clock::rfc3339_utc;
use crate::frames::{CaptureMode, Dir, FrameEvent, Seq};
use crate::naming::capture_key;
use crate::{FlacWriter, RecordError};

pub use crate::frames::FRAME_IDLE_CLOSE;

/// Counts frames that never arrived, by comparing sequence numbers.
#[derive(Debug)]
pub struct LossCounter {
    frames_per_seq: u32,
    last: Option<u16>,
}

impl LossCounter {
    #[must_use]
    pub fn new(frames_per_seq: u32) -> Self {
        Self {
            frames_per_seq,
            last: None,
        }
    }

    /// Frames lost before this one. An equal value is the second half of the
    /// same packet, so it is no gap.
    pub fn observe(&mut self, seq: Seq) -> u32 {
        let lost = match self.last {
            Some(last) if last != seq.value && seq.modulus > 0 => {
                let m = i64::from(seq.modulus);
                let steps = (i64::from(seq.value) - i64::from(last) - 1).rem_euclid(m);
                u32::try_from(steps)
                    .unwrap_or(u32::MAX)
                    .saturating_mul(self.frames_per_seq)
            }
            _ => 0,
        };
        self.last = Some(seq.value);
        lost
    }
}

struct Stream {
    dir: Dir,
    mode: CaptureMode,
    network: &'static str,
    talker: Option<String>,
    at: SystemTime,
    vocoder: &'static str,
    key: String,
    ambe_path: PathBuf,
    flac_path: PathBuf,
    manifest: PathBuf,
    ambe: File,
    flac: FlacWriter,
    frames: u32,
    decoded: u32,
    dropped_pcm: u32,
    lost: Vec<LostRun>,
    counter: LossCounter,
    last_event: Instant,
}

/// Turns `FrameEvent`s into unamblify files under `<root>/captured/`.
pub struct FrameWriter {
    root: PathBuf,
    version: String,
    rx: Option<Stream>,
    tx: Option<Stream>,
    rows: Vec<CaptureRow>,
}

/// `Err(Folder)` when any of `dirs` exists but is not a real folder (a
/// symlink to one counts as not real). Missing folders pass.
fn refuse_links(dirs: &[&Path]) -> Result<(), RecordError> {
    for d in dirs {
        match std::fs::symlink_metadata(d) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(RecordError::Folder(format!(
                    "{} is not a plain folder",
                    d.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// `remove_file`, with a failure logged rather than dropped. A file that is
/// already gone is not a failure.
fn remove_logged(p: &Path) {
    if let Err(e) = std::fs::remove_file(p)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("capture: cannot delete {}: {e}", p.display());
    }
}

fn hex_digest(d: &[u8]) -> String {
    use std::fmt::Write as _;
    d.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn sha256_file(p: &Path) -> Result<String, RecordError> {
    let mut h = Sha256::new();
    std::io::copy(&mut File::open(p)?, &mut h)?;
    Ok(hex_digest(&h.finalize()))
}

impl FrameWriter {
    #[must_use]
    pub fn new(root: &Path, version: &str) -> FrameWriter {
        FrameWriter {
            root: root.to_path_buf(),
            version: version.to_owned(),
            rx: None,
            tx: None,
            rows: Vec::new(),
        }
    }

    #[must_use]
    pub fn rows(&self) -> &[CaptureRow] {
        &self.rows
    }

    fn slot(&mut self, dir: Dir) -> &mut Option<Stream> {
        match dir {
            Dir::Rx => &mut self.rx,
            Dir::Tx => &mut self.tx,
        }
    }

    pub fn handle(&mut self, ev: FrameEvent, now: Instant) -> Result<(), RecordError> {
        match ev {
            FrameEvent::Start {
                dir,
                network,
                mode,
                talker,
                at,
                vocoder,
            } => {
                self.close(dir)?;
                let s = self.open(dir, network, mode, talker, at, vocoder, now)?;
                *self.slot(dir) = Some(s);
            }
            FrameEvent::Frame { dir, bytes, seq } => {
                let Some(s) = self.slot(dir).as_mut() else {
                    return Ok(());
                };
                s.last_event = now;
                if bytes.len() != s.mode.frame_bytes() {
                    return Ok(());
                }
                if let Some(seq) = seq {
                    let frames = s.counter.observe(seq);
                    if frames > 0 {
                        s.lost.push(LostRun {
                            at_frame: s.frames,
                            frames,
                        });
                    }
                }
                if let Err(e) = s.ambe.write_all(&bytes) {
                    self.abandon(dir);
                    return Err(e.into());
                }
                s.frames += 1;
            }
            FrameEvent::Pcm { dir, pcm } => {
                let Some(s) = self.slot(dir).as_mut() else {
                    return Ok(());
                };
                s.last_event = now;
                if s.decoded >= s.frames {
                    s.dropped_pcm += 1;
                    return Ok(());
                }
                if let Err(e) = s.flac.push_interleaved(&pcm) {
                    self.abandon(dir);
                    return Err(e);
                }
                s.decoded += 1;
            }
            FrameEvent::End { dir } => self.close(dir)?,
        }
        Ok(())
    }

    /// Drops a stream after an I/O error and removes its partial files.
    fn abandon(&mut self, dir: Dir) {
        if let Some(s) = self.slot(dir).take() {
            let (a, f) = (s.ambe_path.clone(), s.flac_path.clone());
            drop(s);
            remove_logged(&a);
            remove_logged(&f);
        }
    }

    /// Closes any stream idle for `FRAME_IDLE_CLOSE`.
    pub fn poll(&mut self, now: Instant) -> Result<(), RecordError> {
        for dir in [Dir::Rx, Dir::Tx] {
            let idle = self
                .slot(dir)
                .as_ref()
                .is_some_and(|s| now.saturating_duration_since(s.last_event) >= FRAME_IDLE_CLOSE);
            if idle {
                self.close(dir)?;
            }
        }
        Ok(())
    }

    /// Closes every open stream and returns all rows written.
    pub fn finish(mut self) -> Result<Vec<CaptureRow>, RecordError> {
        let a = self.close(Dir::Rx);
        let b = self.close(Dir::Tx);
        a?;
        b?;
        Ok(self.rows)
    }

    #[allow(clippy::too_many_arguments)]
    fn open(
        &self,
        dir: Dir,
        network: &'static str,
        mode: CaptureMode,
        talker: Option<String>,
        at: SystemTime,
        vocoder: &'static str,
        now: Instant,
    ) -> Result<Stream, RecordError> {
        let captured = self.root.join("captured");
        let mode_dir = captured.join(mode.dir_name());
        let air = mode_dir.join("astar-air");
        // A capture folder that is a symlink (or a file) is refused, before
        // and after creating them, so nothing is written through a link.
        refuse_links(&[&captured, &mode_dir, &air])?;
        std::fs::create_dir_all(&air)?;
        refuse_links(&[&captured, &mode_dir, &air])?;
        let base_key = capture_key(at, network, talker.as_deref(), dir);
        let mut n = 1u32;
        loop {
            let key = if n == 1 {
                base_key.clone()
            } else {
                format!("{base_key}-{n}")
            };
            let stem = key.strip_prefix("astar-air/").unwrap_or(&key);
            let ambe_path = air.join(format!("{stem}.ambe"));
            let flac_path = air.join(format!("{stem}.flac"));
            match OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&ambe_path)
            {
                Ok(ambe) => {
                    // `create_new`, as for the `.ambe`: a file or a symlink
                    // already at the `.flac` name is never followed or
                    // overwritten. The pair moves on to the next name.
                    let flac = match FlacWriter::create_new(&flac_path, 8000, 1) {
                        Ok(f) => f,
                        Err(e) => {
                            drop(ambe);
                            remove_logged(&ambe_path);
                            if matches!(&e, RecordError::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists)
                            {
                                n += 1;
                                continue;
                            }
                            // Not ours to keep: a half-made `.flac` goes too.
                            remove_logged(&flac_path);
                            return Err(e);
                        }
                    };
                    return Ok(Stream {
                        dir,
                        mode,
                        network,
                        talker,
                        at,
                        vocoder,
                        key,
                        ambe_path,
                        flac_path,
                        manifest: mode_dir.join("manifest.jsonl"),
                        ambe,
                        flac,
                        frames: 0,
                        decoded: 0,
                        dropped_pcm: 0,
                        lost: Vec::new(),
                        counter: LossCounter::new(mode.frames_per_seq()),
                        last_event: now,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn close(&mut self, dir: Dir) -> Result<(), RecordError> {
        let Some(s) = self.slot(dir).take() else {
            return Ok(());
        };
        let row = Self::seal(s, &self.version)?;
        if let Some(row) = row {
            self.rows.push(row);
        }
        Ok(())
    }

    /// Trims the undecoded tail, syncs both files and appends the row. A
    /// stream with nothing decoded leaves no files and no row.
    fn seal(s: Stream, version: &str) -> Result<Option<CaptureRow>, RecordError> {
        // Decoding is first in, first out, so the frames with no decode are
        // the last ones.
        let kept = s.frames.min(s.decoded);
        let trimmed = s.frames - kept;
        s.ambe
            .set_len(u64::from(kept) * s.mode.frame_bytes() as u64)?;
        s.ambe.sync_all()?;
        s.flac.finish()?;
        if kept == 0 {
            drop(s.ambe);
            remove_logged(&s.ambe_path);
            remove_logged(&s.flac_path);
            return Ok(None);
        }
        let row = CaptureRow {
            key: s.key,
            mode: s.mode.dir_name().to_owned(),
            frames: kept,
            port: format!("astar:{}", s.network),
            prodid: s.mode.prodid().to_owned(),
            version: version.to_owned(),
            encode_ms: 0,
            decode_ms: 0,
            sha256_ambe: sha256_file(&s.ambe_path)?,
            sha256_wav: sha256_file(&s.flac_path)?,
            captured_at: rfc3339_utc(SystemTime::now()),
            attempts: 1,
            astar: AstarExtra {
                talker: s.talker,
                network: s.network.to_owned(),
                direction: s.dir.as_str().to_owned(),
                received_at: rfc3339_utc(s.at),
                lost: s.lost,
                vocoder: s.vocoder.to_owned(),
                trimmed_tail: trimmed,
                pcm: match s.dir {
                    Dir::Rx => "decoded",
                    Dir::Tx => "encoder-input",
                }
                .to_owned(),
                dropped_pcm: s.dropped_pcm,
            },
        };
        append_row(&s.manifest, &row)?;
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-fw-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn at() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_041_204)
    }

    fn start(dir: Dir, mode: CaptureMode, network: &'static str) -> FrameEvent {
        FrameEvent::Start {
            dir,
            network,
            mode,
            talker: Some("W1ABC".to_owned()),
            at: at(),
            vocoder: "hardware",
        }
    }

    fn frame(dir: Dir, i: u8, len: usize, seq: Option<Seq>) -> FrameEvent {
        FrameEvent::Frame {
            dir,
            bytes: vec![i; len],
            seq,
        }
    }

    fn pcm(dir: Dir, i: i16) -> FrameEvent {
        FrameEvent::Pcm {
            dir,
            pcm: std::array::from_fn(|k| i * 100 + i16::try_from(k).unwrap()),
        }
    }

    fn hex(b: &[u8]) -> String {
        hex_digest(&Sha256::digest(b))
    }

    fn mode_dir(root: &Path, m: &str) -> PathBuf {
        root.join("captured").join(m).join("astar-air")
    }

    fn seq(value: u16, modulus: u16) -> Seq {
        Seq { value, modulus }
    }

    #[test]
    fn one_dstar_over_writes_matching_ambe_flac_and_row() {
        let r = root("one");
        let mut w = FrameWriter::new(&r, "0.1.16-beta");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        let mut want = Vec::new();
        let mut samples = Vec::new();
        for i in 0..50u8 {
            w.handle(frame(Dir::Rx, i, 9, None), t).unwrap();
            let p = pcm(Dir::Rx, i16::from(i));
            if let FrameEvent::Pcm { pcm, .. } = &p {
                samples.extend_from_slice(pcm);
            }
            w.handle(p, t).unwrap();
            want.extend(vec![i; 9]);
        }
        w.handle(FrameEvent::End { dir: Dir::Rx }, t).unwrap();
        let rows = w.finish().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        let key = capture_key(at(), "dstar", Some("W1ABC"), Dir::Rx);
        assert_eq!(row.key, key);
        let stem = key.strip_prefix("astar-air/").unwrap();
        let air = mode_dir(&r, "dstar");
        let ambe = std::fs::read(air.join(format!("{stem}.ambe"))).unwrap();
        assert_eq!(ambe.len(), 450);
        assert_eq!(ambe, want);
        let flac_path = air.join(format!("{stem}.flac"));
        let mut rd = claxon::FlacReader::open(&flac_path).unwrap();
        assert_eq!(rd.streaminfo().sample_rate, 8000);
        assert_eq!(rd.streaminfo().channels, 1);
        let got: Vec<i16> = rd
            .samples()
            .map(|s| i16::try_from(s.unwrap()).unwrap())
            .collect();
        assert_eq!(got.len(), 50 * 160);
        assert_eq!(got, samples);
        assert_eq!(row.frames, 50);
        assert_eq!(row.mode, "dstar");
        assert_eq!(row.port, "astar:dstar");
        assert_eq!(row.prodid, "AMBE3000");
        assert_eq!(row.version, "0.1.16-beta");
        assert_eq!(row.attempts, 1);
        assert_eq!(row.encode_ms, 0);
        assert_eq!(row.decode_ms, 0);
        assert_eq!(row.sha256_ambe, hex(&ambe));
        assert_eq!(row.sha256_wav, hex(&std::fs::read(&flac_path).unwrap()));
        assert_eq!(row.astar.direction, "rx");
        assert_eq!(row.astar.pcm, "decoded");
        assert_eq!(row.astar.trimmed_tail, 0);
        let on_disk = crate::capture::read_rows(&r.join("captured/dstar/manifest.jsonl")).unwrap();
        assert_eq!(on_disk, rows);
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn a_stream_with_undecoded_tail_is_trimmed() {
        let r = root("tail");
        let mut w = FrameWriter::new(&r, "v");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        for i in 0..50u8 {
            w.handle(frame(Dir::Rx, i, 9, None), t).unwrap();
        }
        for i in 0..47 {
            w.handle(pcm(Dir::Rx, i), t).unwrap();
        }
        w.handle(FrameEvent::End { dir: Dir::Rx }, t).unwrap();
        let rows = w.finish().unwrap();
        assert_eq!(rows[0].frames, 47);
        assert_eq!(rows[0].astar.trimmed_tail, 3);
        let stem = rows[0].key.strip_prefix("astar-air/").unwrap();
        let ambe = std::fs::read(mode_dir(&r, "dstar").join(format!("{stem}.ambe"))).unwrap();
        assert_eq!(ambe.len(), 47 * 9);
        assert_eq!(rows[0].sha256_ambe, hex(&ambe));
        std::fs::remove_dir_all(&r).unwrap();
    }

    fn run_seqs(mode: CaptureMode, name: &str, seqs: &[u16], modulus: u16) -> Vec<LostRun> {
        let r = root(name);
        let mut w = FrameWriter::new(&r, "v");
        let t = Instant::now();
        w.handle(start(Dir::Rx, mode, "x"), t).unwrap();
        for &s in seqs {
            w.handle(
                frame(Dir::Rx, 1, mode.frame_bytes(), Some(seq(s, modulus))),
                t,
            )
            .unwrap();
            w.handle(pcm(Dir::Rx, 1), t).unwrap();
        }
        w.handle(FrameEvent::End { dir: Dir::Rx }, t).unwrap();
        let rows = w.finish().unwrap();
        std::fs::remove_dir_all(&r).unwrap();
        rows[0].astar.lost.clone()
    }

    #[test]
    fn dstar_sequence_gaps_are_counted() {
        assert_eq!(
            run_seqs(CaptureMode::Dstar, "gap", &[0, 1, 2, 5], 21),
            vec![LostRun {
                at_frame: 3,
                frames: 2
            }]
        );
        assert_eq!(
            run_seqs(CaptureMode::Dstar, "wrap", &[19, 20, 0], 21),
            [] as [crate::capture::LostRun; 0]
        );
    }

    #[test]
    fn m17_frame_numbers_count_two_frames_per_packet() {
        assert_eq!(
            run_seqs(
                CaptureMode::Codec2_3200,
                "m17",
                &[7, 7, 8, 8, 10, 10],
                0x8000
            ),
            vec![LostRun {
                at_frame: 4,
                frames: 2
            }]
        );
    }

    #[test]
    fn an_idle_stream_closes_after_one_second() {
        let r = root("idle");
        let mut w = FrameWriter::new(&r, "v");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        w.handle(frame(Dir::Rx, 1, 9, None), t).unwrap();
        w.handle(pcm(Dir::Rx, 1), t).unwrap();
        w.poll(t + Duration::from_millis(999)).unwrap();
        assert_eq!(w.rows(), []);
        w.poll(t + FRAME_IDLE_CLOSE).unwrap();
        assert_eq!(w.rows().len(), 1);
        assert_eq!(w.finish().unwrap().len(), 1);
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn a_wrong_width_frame_is_refused_not_written() {
        let r = root("wide");
        let mut w = FrameWriter::new(&r, "v");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        w.handle(frame(Dir::Rx, 7, 8, None), t).unwrap();
        w.handle(frame(Dir::Rx, 2, 9, None), t).unwrap();
        w.handle(pcm(Dir::Rx, 1), t).unwrap();
        let rows = w.finish().unwrap();
        assert_eq!(rows[0].frames, 1);
        let stem = rows[0].key.strip_prefix("astar-air/").unwrap();
        let ambe = std::fs::read(mode_dir(&r, "dstar").join(format!("{stem}.ambe"))).unwrap();
        assert_eq!(ambe, vec![2u8; 9]);
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn rx_and_tx_streams_are_independent() {
        let r = root("both");
        let mut w = FrameWriter::new(&r, "v");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        w.handle(start(Dir::Tx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        for i in 0..3u8 {
            w.handle(frame(Dir::Rx, i, 9, None), t).unwrap();
            w.handle(frame(Dir::Tx, 100 + i, 9, None), t).unwrap();
            w.handle(pcm(Dir::Rx, 1), t).unwrap();
            w.handle(pcm(Dir::Tx, 2), t).unwrap();
        }
        w.handle(FrameEvent::End { dir: Dir::Rx }, t).unwrap();
        w.handle(FrameEvent::End { dir: Dir::Tx }, t).unwrap();
        let rows = w.finish().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].key.ends_with("-rx"));
        assert!(rows[1].key.ends_with("-tx"));
        assert_eq!(rows[1].astar.pcm, "encoder-input");
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn pcm_without_a_frame_is_dropped_and_counted() {
        let r = root("extra");
        let mut w = FrameWriter::new(&r, "v");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        for i in 0..5u8 {
            w.handle(frame(Dir::Rx, i, 9, None), t).unwrap();
        }
        for i in 0..7 {
            w.handle(pcm(Dir::Rx, i), t).unwrap();
        }
        let rows = w.finish().unwrap();
        assert_eq!(rows[0].frames, 5);
        assert_eq!(rows[0].astar.dropped_pcm, 2);
        let stem = rows[0].key.strip_prefix("astar-air/").unwrap();
        let rd =
            claxon::FlacReader::open(mode_dir(&r, "dstar").join(format!("{stem}.flac"))).unwrap();
        assert_eq!(rd.streaminfo().samples, Some(5 * 160));
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn a_second_start_seals_the_first_stream() {
        let r = root("restart");
        let mut w = FrameWriter::new(&r, "v");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        w.handle(frame(Dir::Rx, 1, 9, None), t).unwrap();
        w.handle(pcm(Dir::Rx, 1), t).unwrap();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        assert_eq!(w.rows().len(), 1);
        w.handle(frame(Dir::Rx, 2, 9, None), t).unwrap();
        w.handle(pcm(Dir::Rx, 2), t).unwrap();
        let rows = w.finish().unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].key, rows[1].key);
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_capture_folder_is_refused() {
        for link in ["captured", "captured/dstar", "captured/dstar/astar-air"] {
            let r = root(&format!("link-{}", link.replace('/', "-")));
            let outside = root(&format!("link-out-{}", link.replace('/', "-")));
            let l = r.join(link);
            std::fs::create_dir_all(l.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &l).unwrap();
            let mut w = FrameWriter::new(&r, "0.1.16-beta");
            let e = w
                .handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), Instant::now())
                .unwrap_err();
            assert!(matches!(e, RecordError::Folder(_)), "{link}: {e}");
            assert_eq!(
                std::fs::read_dir(&outside).unwrap().count(),
                0,
                "{link}: nothing written through the link"
            );
            std::fs::remove_dir_all(&r).unwrap();
            std::fs::remove_dir_all(&outside).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_flac_is_never_followed_or_overwritten() {
        let r = root("planted-flac");
        let outside = root("planted-flac-out");
        std::fs::write(outside.join("victim"), b"keep me").unwrap();
        let key = capture_key(at(), "dstar", Some("W1ABC"), Dir::Rx);
        let stem = key.strip_prefix("astar-air/").unwrap().to_owned();
        let air = mode_dir(&r, "dstar");
        std::fs::create_dir_all(&air).unwrap();
        // A link where the first capture's .flac would go, and no .ambe.
        std::os::unix::fs::symlink(outside.join("victim"), air.join(format!("{stem}.flac")))
            .unwrap();
        let mut w = FrameWriter::new(&r, "0.1.16-beta");
        let t = Instant::now();
        w.handle(start(Dir::Rx, CaptureMode::Dstar, "dstar"), t)
            .unwrap();
        w.handle(frame(Dir::Rx, 1, 9, None), t).unwrap();
        w.handle(pcm(Dir::Rx, 1), t).unwrap();
        w.handle(FrameEvent::End { dir: Dir::Rx }, t).unwrap();
        let rows = w.finish().unwrap();
        assert_eq!(std::fs::read(outside.join("victim")).unwrap(), b"keep me");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].key,
            format!("{key}-2"),
            "the planted name is skipped"
        );
        assert!(
            !air.join(format!("{stem}.ambe")).exists(),
            "the .ambe made for the refused name is removed"
        );
        std::fs::remove_dir_all(&r).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
