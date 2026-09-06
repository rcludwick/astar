// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `--wav`: an [`AudioBackend`] that captures a decoded output bus into a
//! file instead of a real device.
//!
//! Written for `dstar-listen` (iax-a9d4 Task 7) and shared with `ysf-listen`
//! rather than forked. It is not D-Star-specific in anything but the label it
//! used to hard-code: what it captures is 8 kHz mono PCM off a station's
//! output bus, which is the same shape whatever vocoder produced it. The
//! label is now a constructor argument, so a WAV written by one command is
//! distinguishable from the other in a device list.
//!
//! Swapped in via `Station::with_backend_factory`, so the session's own
//! output-routing code never has to know the difference.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};

/// 44-byte canonical RIFF/WAVE header size (no extra chunks).
const WAV_HEADER_LEN: usize = 44;
/// How often the `--wav` writer thread re-patches the header in place with
/// the data length written so far — see the call site's comment for why
/// (bounds the worst case on an abrupt exit to "up to this much of the
/// declared length is stale", never "corrupt file").
const HEADER_PATCH_INTERVAL: Duration = Duration::from_secs(1);

/// The one "device" this backend offers, named after the command that
/// opened it so two commands' captures are told apart in a device list.
fn wav_device(label: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(&format!("{label}-wav")),
        name: format!("{label} --wav"),
        direction: Direction::Output,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

/// An [`AudioBackend`] with no real input/output devices: its one output
/// "device" pulls decoded audio on a background thread at the stream's own
/// cadence (matching how a real cpal callback would be driven) and appends
/// it, as 16-bit PCM, to a WAV file opened once for the whole session — every
/// transmission received while linked lands in the same file, one after
/// another.
pub struct WavBackend {
    path: PathBuf,
    label: &'static str,
}

impl WavBackend {
    /// `label` names the command doing the capturing (`"dstar-listen"`,
    /// `"ysf-listen"`); it only ever reaches the synthetic device's id and
    /// name.
    pub fn new(path: PathBuf, label: &'static str) -> Self {
        Self { path, label }
    }
}

impl AudioBackend for WavBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![wav_device(self.label)])
    }

    fn default_input(&self) -> Option<DeviceInfo> {
        None
    }

    fn default_output(&self) -> Option<DeviceInfo> {
        Some(wav_device(self.label))
    }

    fn open_input(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Err(AudioError::DeviceNotFound(
            "dstar-listen --wav has no input device (D-Star RX never opens one)".into(),
        ))
    }

    fn open_output(
        &self,
        _device: &DeviceInfo,
        config: StreamConfig,
        mut source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let file = File::create(&self.path)
            .map_err(|e| AudioError::BuildStream(format!("create {}: {e}", self.path.display())))?;
        let mut writer = BufWriter::new(file);
        let sample_rate = config.sample_rate;
        let channels = config.channels;
        // A real (zero-length-data) header, not an all-zero placeholder: if
        // the process is killed before the writer thread's first periodic
        // patch below ever runs (see that comment for why this can't just be
        // "patch once at clean shutdown"), the file on disk is still a
        // valid, readable (if silent) WAV rather than unparseable garbage.
        // The explicit `flush` is NOT redundant with `write_all`: `BufWriter`
        // only writes through to the underlying `File` (and hence to disk)
        // once its internal buffer fills, is explicitly flushed, or is
        // dropped — and an abrupt kill (SIGKILL, or the double-Ctrl-C
        // escalation below) skips `Drop` entirely, same as it skips every
        // other destructor. Without this, a kill in the first
        // `HEADER_PATCH_INTERVAL` leaves a literal EMPTY (0-byte) file, not
        // the intended silent-but-valid one — caught by this fix's own
        // smoke test, not a hypothetical.
        write_wav_header(&mut writer, sample_rate, channels, 0)
            .and_then(|()| writer.flush())
            .map_err(|e| AudioError::BuildStream(format!("write initial header: {e}")))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let pull_ms = config.buffer_ms.max(1);
        let samples_per_pull = ((u64::from(sample_rate) * u64::from(pull_ms)) / 1_000)
            .max(1)
            .saturating_mul(u64::from(channels.max(1))) as usize;
        // How many pulls between header re-patches — see the loop below.
        let patch_every_n_pulls = (HEADER_PATCH_INTERVAL.as_millis() / u128::from(pull_ms)).max(1);

        let join = thread::Builder::new()
            .name("dstar-wav-writer".to_string())
            .spawn(move || {
                let mut buf = vec![0.0_f32; samples_per_pull];
                let mut data_len: u64 = 0;
                let mut pcm = Vec::<u8>::with_capacity(buf.len() * 2);
                let mut pulls_since_patch: u128 = 0;
                while !thread_stop.load(Ordering::Relaxed) {
                    buf.fill(0.0);
                    let _ = source.read(&mut buf);
                    pcm.clear();
                    for &s in &buf {
                        // Same f32-to-i16 device-sample conversion
                        // `astar_audio::stream`'s real cpal output
                        // callback uses (see its module doc's `#![allow(...)]`
                        // for the same lint) — the `clamp` just above bounds
                        // the input to `[-1.0, 1.0]` first, so this never
                        // wraps, only (intentionally) truncates the fraction.
                        #[allow(clippy::cast_possible_truncation)]
                        let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                        pcm.extend_from_slice(&v.to_le_bytes());
                    }
                    if writer.write_all(&pcm).is_err() {
                        break;
                    }
                    data_len += pcm.len() as u64;
                    pulls_since_patch += 1;

                    // Periodically rewrite the header in place with the
                    // data length written SO FAR (iax-a9d4 Task 7 review
                    // fix): a double-Ctrl-C escalates to `process::exit`
                    // (see `listen`), which skips the clean-shutdown path
                    // below entirely — without this, that path is the WAV's
                    // only chance at a non-placeholder header, so a
                    // double-Ctrl-C (or a SIGKILL, or a crash) would leave
                    // an unreadable file. Patching here instead bounds the
                    // worst case to "missing up to ~1s of the declared
                    // `data` length" (the file is always independently
                    // readable), not "corrupt", regardless of how the
                    // process ends.
                    if pulls_since_patch >= patch_every_n_pulls {
                        pulls_since_patch = 0;
                        let capped = u32::try_from(data_len).unwrap_or(u32::MAX);
                        if patch_header_in_place(&mut writer, sample_rate, channels, capped)
                            .is_err()
                        {
                            break;
                        }
                    }

                    thread::sleep(Duration::from_millis(u64::from(pull_ms)));
                }
                let capped = u32::try_from(data_len).unwrap_or(u32::MAX);
                let _ = patch_header_in_place(&mut writer, sample_rate, channels, capped);
                let _ = writer.flush();
            })
            .map_err(|e| AudioError::BuildStream(format!("spawn wav writer thread: {e}")))?;

        Ok(Box::new(WavStreamHandle {
            stop,
            join: Some(join),
        }))
    }
}

/// Rewinds to the start of the file, overwrites the header with the current
/// `data_len`, then seeks back to the write position (`WAV_HEADER_LEN +
/// data_len`) so the caller's next `write_all` resumes appending PCM exactly
/// where it left off. `BufWriter::seek` flushes any buffered bytes before
/// moving the underlying file position, both times — so this never
/// reorders/loses PCM already handed to `writer.write_all`.
fn patch_header_in_place(
    writer: &mut BufWriter<File>,
    sample_rate: u32,
    channels: u16,
    data_len: u32,
) -> io::Result<()> {
    writer.seek(SeekFrom::Start(0))?;
    write_wav_header(writer, sample_rate, channels, data_len)?;
    let resume_at = u64::try_from(WAV_HEADER_LEN).unwrap_or(44) + u64::from(data_len);
    writer.seek(SeekFrom::Start(resume_at))?;
    Ok(())
}

/// Canonical 44-byte RIFF/WAVE header for 16-bit PCM.
fn write_wav_header(
    w: &mut impl Write,
    sample_rate: u32,
    channels: u16,
    data_len: u32,
) -> io::Result<()> {
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let block_align = channels * 2;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size (PCM)
    w.write_all(&1u16.to_le_bytes())?; // audio format: PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?; // bits per sample
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

struct WavStreamHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl WavStreamHandle {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl StreamHandle for WavStreamHandle {
    fn stop(mut self: Box<Self>) {
        self.shutdown();
    }

    fn pause(&self) -> Result<(), AudioError> {
        Ok(())
    }

    fn resume(&self) -> Result<(), AudioError> {
        Ok(())
    }
}

impl Drop for WavStreamHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_round_trips_riff_fields() {
        let mut buf = Vec::new();
        write_wav_header(&mut buf, 8_000, 1, 320).expect("write header");
        assert_eq!(buf.len(), WAV_HEADER_LEN);
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 36 + 320);
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(buf[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(buf[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(buf[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(buf[24..28].try_into().unwrap()), 8_000);
        assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), 16_000);
        assert_eq!(u16::from_le_bytes(buf[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(buf[34..36].try_into().unwrap()), 16);
        assert_eq!(&buf[36..40], b"data");
        assert_eq!(u32::from_le_bytes(buf[40..44].try_into().unwrap()), 320);
    }

    #[test]
    fn patch_header_in_place_preserves_already_written_pcm_and_resumes_appending() {
        let path = std::env::temp_dir().join(format!(
            "astar-cli-dstar-wav-patch-test-{}-{:?}.wav",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = File::create(&path).expect("create temp wav");
        let mut writer = BufWriter::new(file);
        write_wav_header(&mut writer, 8_000, 1, 0).expect("initial header");

        // Write one "pull"'s worth of PCM, patch the header mid-stream (as
        // the writer thread does periodically — see HEADER_PATCH_INTERVAL),
        // then write a second pull's worth and patch again.
        let first: [u8; 4] = [0x01, 0x02, 0x03, 0x04];
        writer.write_all(&first).expect("write first pull");
        let after_first = u32::try_from(first.len()).unwrap();
        patch_header_in_place(&mut writer, 8_000, 1, after_first).expect("patch mid-stream");

        let second: [u8; 4] = [0x05, 0x06, 0x07, 0x08];
        writer.write_all(&second).expect("write second pull");
        let total = u32::try_from(first.len() + second.len()).unwrap();
        patch_header_in_place(&mut writer, 8_000, 1, total).expect("final patch");
        writer.flush().expect("flush");
        drop(writer);

        let bytes = std::fs::read(&path).expect("read back");
        std::fs::remove_file(&path).ok();

        assert_eq!(bytes.len(), WAV_HEADER_LEN + first.len() + second.len());
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            total,
            "final data chunk size reflects everything written"
        );
        // The PCM itself must be intact, in order, and land right after the
        // header — proves the mid-stream seek-patch-seek-back never
        // reordered, duplicated, or clobbered already-written data.
        let mut expected_pcm = first.to_vec();
        expected_pcm.extend_from_slice(&second);
        assert_eq!(&bytes[WAV_HEADER_LEN..], expected_pcm.as_slice());
    }

    #[test]
    fn a_wav_file_is_immediately_valid_before_any_pcm_or_patch() {
        // The initial header written by `open_output` (data_len = 0), not
        // the old all-zero placeholder — a process killed before the first
        // periodic patch still leaves a parseable (if silent) file.
        let mut buf = Vec::new();
        write_wav_header(&mut buf, 8_000, 1, 0).expect("initial header");
        assert_eq!(buf.len(), WAV_HEADER_LEN);
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[36..40], b"data");
        assert_eq!(u32::from_le_bytes(buf[40..44].try_into().unwrap()), 0);
    }
}
