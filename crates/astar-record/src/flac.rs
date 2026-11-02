// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Streaming FLAC writer: a valid 42-byte header first, frames appended as
//! audio arrives, the real totals patched in at `finish`.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flacenc::component::{BitRepr, StreamInfo};
use flacenc::config::Encoder;
use flacenc::error::Verify;
use flacenc::source::{Fill, FrameBuf};

use crate::RecordError;

/// Samples per channel in every full FLAC frame.
pub const FLAC_BLOCK: usize = 4096;
/// `fLaC` + metadata block header + STREAMINFO.
pub const FLAC_HEADER_LEN: u64 = 42;

/// Result of a finished file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlacSummary {
    pub path: PathBuf,
    pub samples_per_channel: u64,
    pub bytes: u64,
}

pub struct FlacWriter {
    file: File,
    path: PathBuf,
    config: flacenc::error::Verified<Encoder>,
    info: StreamInfo,
    channels: usize,
    /// Interleaved samples awaiting a full block.
    pending: Vec<i32>,
    frames: usize,
    total: u64,
    bytes: u64,
    min_frame: usize,
    max_frame: usize,
}

fn enc<E: std::fmt::Debug>(what: &str, e: E) -> RecordError {
    RecordError::Encode(format!("{what}: {e:?}"))
}

fn info_bytes(info: &StreamInfo) -> Result<Vec<u8>, RecordError> {
    let mut sink = flacenc::bitsink::ByteSink::new();
    info.write(&mut sink).map_err(|e| enc("streaminfo", e))?;
    Ok(sink.as_slice().to_vec())
}

impl FlacWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u8) -> Result<Self, RecordError> {
        Self::with_file(File::create(path)?, path, sample_rate, channels)
    }

    /// Like [`FlacWriter::create`], but refuses a path that already exists
    /// (`create_new`: an existing file, or a symlink planted there, is an
    /// `AlreadyExists` I/O error and is never followed or overwritten).
    pub fn create_new(path: &Path, sample_rate: u32, channels: u8) -> Result<Self, RecordError> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Self::with_file(file, path, sample_rate, channels)
    }

    fn with_file(
        mut file: File,
        path: &Path,
        sample_rate: u32,
        channels: u8,
    ) -> Result<Self, RecordError> {
        let config = Encoder::default()
            .into_verified()
            .map_err(|(_, e)| enc("config", e))?;
        let mut info = StreamInfo::new(sample_rate as usize, channels as usize, 16)
            .map_err(|e| enc("streaminfo", e))?;
        info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(|e| enc("block sizes", e))?;
        info.set_frame_sizes(0, 0)
            .map_err(|e| enc("frame sizes", e))?;
        let body = info_bytes(&info)?;
        debug_assert_eq!(body.len(), 34);
        let mut header = Vec::with_capacity(42);
        header.extend_from_slice(b"fLaC");
        header.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        header.extend_from_slice(&body);
        file.write_all(&header)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            config,
            info,
            channels: channels as usize,
            pending: Vec::with_capacity(FLAC_BLOCK * channels as usize),
            frames: 0,
            total: 0,
            bytes: FLAC_HEADER_LEN,
            min_frame: usize::MAX,
            max_frame: 0,
        })
    }

    pub fn push_interleaved(&mut self, samples: &[i16]) -> Result<(), RecordError> {
        let block = FLAC_BLOCK * self.channels;
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (block - self.pending.len()).min(rest.len());
            self.pending
                .extend(rest[..take].iter().map(|&s| i32::from(s)));
            rest = &rest[take..];
            if self.pending.len() == block {
                self.encode_pending()?;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn samples_per_channel(&self) -> u64 {
        self.total + (self.pending.len() / self.channels) as u64
    }

    /// Encode and write the queued block. The block is dropped whether or not
    /// it succeeds, so a failed block never wedges the writer; the counters
    /// move only after the bytes are on disk.
    fn encode_pending(&mut self) -> Result<(), RecordError> {
        let r = self.encode_block();
        self.pending.clear();
        r
    }

    fn encode_block(&mut self) -> Result<(), RecordError> {
        let n = self.pending.len() / self.channels;
        if n == 0 {
            return Ok(());
        }
        let mut buf =
            FrameBuf::with_size(self.channels, n.max(32)).map_err(|e| enc("framebuf", e))?;
        buf.fill_interleaved(&self.pending)
            .map_err(|e| enc("fill", e))?;
        let frame = flacenc::encode_fixed_size_frame(&self.config, &buf, self.frames, &self.info)
            .map_err(|e| enc("frame", e))?;
        let mut sink = flacenc::bitsink::ByteSink::new();
        frame.write(&mut sink).map_err(|e| enc("frame write", e))?;
        let bytes = sink.as_slice();
        self.file.write_all(bytes)?;
        self.min_frame = self.min_frame.min(bytes.len());
        self.max_frame = self.max_frame.max(bytes.len());
        self.bytes += bytes.len() as u64;
        self.total += n as u64;
        self.frames += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<FlacSummary, RecordError> {
        self.encode_pending()?;
        self.info
            .set_total_samples(usize::try_from(self.total).unwrap_or(usize::MAX));
        self.info
            .set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(|e| enc("block sizes", e))?;
        if self.frames > 0 {
            self.info
                .set_frame_sizes(self.min_frame, self.max_frame)
                .map_err(|e| enc("frame sizes", e))?;
        }
        let body = info_bytes(&self.info)?;
        self.file.seek(SeekFrom::Start(8))?;
        self.file.write_all(&body)?;
        self.file.sync_all()?;
        Ok(FlacSummary {
            path: self.path,
            samples_per_channel: self.total,
            bytes: self.bytes,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-record-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn decode(path: &Path) -> (u32, u32, Vec<i16>) {
        let mut r = claxon::FlacReader::open(path).unwrap();
        let si = r.streaminfo();
        let out: Vec<i16> = r.samples().map(|s| s.unwrap() as i16).collect();
        (si.sample_rate, si.channels, out)
    }

    fn ramp_with_noise(n: usize) -> Vec<i16> {
        let mut x: u32 = 12345;
        (0..n)
            .map(|i| match i {
                0 => i16::MIN,
                1 => i16::MAX,
                _ => {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let ramp = ((i * 37) % 65536) as i32 - 32768;
                    (ramp + ((x >> 24) as i32 - 128)).clamp(-32768, 32767) as i16
                }
            })
            .collect()
    }

    #[test]
    fn flac_round_trips_bit_exact() {
        let d = dir("rt");
        for &(rate, channels) in &[(8_000u32, 1u8), (16_000, 1), (16_000, 2)] {
            for &len in &[1usize, 4095, 4096, 4097, 160 * 50, 3 * 4096 + 7] {
                let p = d.join(format!("{rate}-{channels}-{len}.flac"));
                let pcm = ramp_with_noise(len * channels as usize);
                let mut w = FlacWriter::create(&p, rate, channels).unwrap();
                for chunk in pcm.chunks(320 * channels as usize) {
                    w.push_interleaved(chunk).unwrap();
                }
                let s = w.finish().unwrap();
                assert_eq!(s.samples_per_channel, len as u64);
                assert_eq!(s.bytes, std::fs::metadata(&p).unwrap().len());
                assert_eq!(
                    decode(&p),
                    (rate, u32::from(channels), pcm),
                    "{rate} {channels} {len}"
                );
            }
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn finished_header_carries_the_real_length_and_equal_block_sizes() {
        let d = dir("hdr");
        let p = d.join("a.flac");
        let len = 2 * FLAC_BLOCK + 11;
        let mut w = FlacWriter::create(&p, 16_000, 1).unwrap();
        w.push_interleaved(&ramp_with_noise(len)).unwrap();
        w.finish().unwrap();
        let r = claxon::FlacReader::open(&p).unwrap();
        let si = r.streaminfo();
        assert_eq!(si.samples, Some(len as u64));
        assert_eq!(si.min_block_size, FLAC_BLOCK as u16);
        assert_eq!(si.max_block_size, FLAC_BLOCK as u16);
        drop(r);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_writer_dropped_without_finish_leaves_a_playable_file() {
        let d = dir("crash");
        let p = d.join("a.flac");
        let pcm = ramp_with_noise(3 * FLAC_BLOCK + 100);
        let mut w = FlacWriter::create(&p, 16_000, 1).unwrap();
        w.push_interleaved(&pcm).unwrap();
        drop(w);
        let r = claxon::FlacReader::open(&p).unwrap();
        let si = r.streaminfo();
        assert_eq!(si.min_block_size, FLAC_BLOCK as u16);
        assert_eq!(si.max_block_size, FLAC_BLOCK as u16);
        assert_eq!(si.samples, None);
        drop(r);
        let (_, _, got) = decode(&p);
        assert_eq!(got, pcm[..3 * FLAC_BLOCK]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn create_new_refuses_an_existing_path_or_a_symlink() {
        let d = dir("new");
        let p = d.join("x.flac");
        let mut w = FlacWriter::create_new(&p, 8000, 1).unwrap();
        w.push_interleaved(&[1, 2, 3]).unwrap();
        w.finish().unwrap();
        let e = FlacWriter::create_new(&p, 8000, 1).err().unwrap();
        assert!(
            matches!(e, RecordError::Io(ref io) if io.kind() == std::io::ErrorKind::AlreadyExists)
        );
        #[cfg(unix)]
        {
            let target = d.join("target");
            std::os::unix::fs::symlink(&target, d.join("link.flac")).unwrap();
            assert!(FlacWriter::create_new(&d.join("link.flac"), 8000, 1).is_err());
            assert!(!target.exists(), "the link was not followed");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }
}
