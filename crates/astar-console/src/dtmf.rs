// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! In-band DTMF test harness support (iax-381b): a tone dialpad with local
//! sidetone playback, plus live decoding of two sources — the generated tones
//! themselves (local loopback) and the live mic input. Front-end-agnostic;
//! drives the [`astar_codec::dtmf`] primitive and exposes a pollable log of
//! detected digits.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use astar_audio::{AudioBackend, Direction, InputSink, OutputSource, StreamConfig, StreamHandle};
use astar_codec::dtmf::{DEFAULT_TONE_DBFS, DTMF_SAMPLE_RATE, DtmfDecoder, synthesize};

use crate::parrot::resolve_device;
use crate::session::ConsoleError;

/// Where a detected digit came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum DtmfSource {
    /// Decoded off the live capture (mic) stream.
    Mic,
    /// Decoded from a tone the dialpad just generated (round-trip self-test).
    Loopback,
}

/// One detected DTMF digit, with a monotonic sequence number for polling.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DetectedDigit {
    pub seq: u64,
    pub digit: char,
    pub source: DtmfSource,
}

/// How many recent digits the log retains.
const LOG_CAP: usize = 256;
/// Tone duration (ms) for a dialpad press.
const TONE_MS: u32 = 120;

/// Shared, cheaply-cloned log of detected digits. Shared between the mic
/// decode sink (audio thread), the loopback path (HTTP handler), and the
/// poller (SSE/HTTP).
#[derive(Clone)]
pub struct DtmfShared {
    log: Arc<Mutex<VecDeque<DetectedDigit>>>,
    seq: Arc<AtomicU64>,
}

impl DtmfShared {
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(VecDeque::new())),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Append a detected digit, capping the log at [`LOG_CAP`].
    fn record(&self, digit: char, source: DtmfSource) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut log = self.log.lock().unwrap();
        log.push_back(DetectedDigit { seq, digit, source });
        while log.len() > LOG_CAP {
            log.pop_front();
        }
    }

    /// Detected digits with `seq` greater than `since`, oldest first.
    #[must_use]
    pub fn since(&self, since: u64) -> Vec<DetectedDigit> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.seq > since)
            .cloned()
            .collect()
    }

    /// Synthesize `digit`, decode it back (round-trip self-test), record any
    /// detected digit as [`DtmfSource::Loopback`], and return the tone so the
    /// caller can also play it. `None` if `digit` is not a DTMF key.
    // Not `#[must_use]`: callers may want only the loopback record (no playback).
    #[allow(clippy::must_use_candidate)]
    pub fn record_loopback(&self, digit: char) -> Option<Vec<i16>> {
        let tone = synthesize(digit, DTMF_SAMPLE_RATE, TONE_MS, DEFAULT_TONE_DBFS)?;
        let mut decoder = DtmfDecoder::new(DTMF_SAMPLE_RATE);
        for d in decoder.process(&tone) {
            self.record(d, DtmfSource::Loopback);
        }
        Some(tone)
    }
}

impl Default for DtmfShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Input sink that runs a [`DtmfDecoder`] over the mic stream and records
/// detected digits as [`DtmfSource::Mic`].
pub(crate) struct DtmfDecodeSink {
    decoder: DtmfDecoder,
    shared: DtmfShared,
    scratch: Vec<i16>,
}

impl InputSink for DtmfDecodeSink {
    fn write(&mut self, samples: &[f32], _meter: f32) {
        self.scratch.clear();
        self.scratch.extend(
            samples
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16),
        );
        for d in self.decoder.process(&self.scratch) {
            self.shared.record(d, DtmfSource::Mic);
        }
    }
}

/// Output source that plays queued one-shot tone buffers, silence when idle.
pub(crate) struct DtmfToneSource {
    inbound: Receiver<Vec<f32>>,
    residual: VecDeque<f32>,
}

impl OutputSource for DtmfToneSource {
    fn read(&mut self, out: &mut [f32]) -> usize {
        while let Ok(tone) = self.inbound.try_recv() {
            self.residual.extend(tone);
        }
        let n = out.len().min(self.residual.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.residual.pop_front().unwrap_or(0.0);
        }
        n
    }
}

/// A running DTMF tester: holds the live mic-decode and tone-playback streams.
/// Dropping it stops both.
pub struct DtmfTester {
    _backend: Box<dyn AudioBackend>,
    _in: Box<dyn StreamHandle>,
    _out: Box<dyn StreamHandle>,
    tone_tx: Sender<Vec<f32>>,
}

impl DtmfTester {
    /// Open mic-decode + tone-playback streams on the resolved devices,
    /// feeding detected mic digits into `shared`.
    ///
    /// # Errors
    /// [`ConsoleError::Device`] if a device name does not uniquely resolve;
    /// [`ConsoleError::Audio`] if a stream fails to open.
    pub fn start(
        backend: Box<dyn AudioBackend>,
        input: Option<&str>,
        output: Option<&str>,
        shared: DtmfShared,
    ) -> Result<Self, ConsoleError> {
        let devices = backend.devices().map_err(ConsoleError::Audio)?;
        let in_dev = resolve_device(&devices, input, Direction::Input, backend.default_input())?;
        let out_dev = resolve_device(
            &devices,
            output,
            Direction::Output,
            backend.default_output(),
        )?;

        let (tone_tx, tone_rx) = channel::<Vec<f32>>();
        let sink = DtmfDecodeSink {
            decoder: DtmfDecoder::new(DTMF_SAMPLE_RATE),
            shared,
            scratch: Vec::new(),
        };
        let source = DtmfToneSource {
            inbound: tone_rx,
            residual: VecDeque::new(),
        };
        // DTMF tester opens the mic directly (no router/snapshot); discard the
        // capture-overrun counter (iax-9e55).
        let in_h = backend
            .open_input(
                &in_dev,
                StreamConfig::default(),
                Box::new(sink),
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            )
            .map_err(ConsoleError::Audio)?;
        let out_h = backend
            .open_output(&out_dev, StreamConfig::default(), Box::new(source))
            .map_err(ConsoleError::Audio)?;
        Ok(Self {
            _backend: backend,
            _in: in_h,
            _out: out_h,
            tone_tx,
        })
    }

    /// Queue a one-shot tone for local sidetone playback.
    pub fn play_tone(&self, tone: &[i16]) {
        let pcm: Vec<f32> = tone.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let _ = self.tone_tx.send(pcm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_records_the_generated_digit_and_returns_a_tone() {
        let shared = DtmfShared::new();
        let tone = shared.record_loopback('5').expect("valid digit");
        assert!(!tone.is_empty(), "a tone buffer is returned for playback");
        let log = shared.since(0);
        assert_eq!(log.len(), 1, "exactly one digit recorded");
        assert_eq!(log[0].digit, '5');
        assert_eq!(log[0].source, DtmfSource::Loopback);
    }

    #[test]
    fn loopback_rejects_a_non_dtmf_digit() {
        let shared = DtmfShared::new();
        assert!(shared.record_loopback('E').is_none());
        assert!(shared.since(0).is_empty(), "nothing recorded for a bad key");
    }

    #[test]
    fn since_filters_by_sequence_number() {
        let shared = DtmfShared::new();
        shared.record_loopback('1');
        let after_first = shared.since(0)[0].seq;
        shared.record_loopback('2');
        let fresh = shared.since(after_first);
        assert_eq!(fresh.len(), 1, "only the digit newer than `since`");
        assert_eq!(fresh[0].digit, '2');
    }

    #[test]
    fn mic_sink_decodes_a_tone_off_the_capture_stream() {
        let shared = DtmfShared::new();
        let mut sink = DtmfDecodeSink {
            decoder: DtmfDecoder::new(DTMF_SAMPLE_RATE),
            shared: shared.clone(),
            scratch: Vec::new(),
        };
        // A synthesized '7' presented as f32 capture samples.
        let tone = synthesize('7', DTMF_SAMPLE_RATE, 120, DEFAULT_TONE_DBFS).unwrap();
        let as_f32: Vec<f32> = tone.iter().map(|&s| f32::from(s) / 32768.0).collect();
        sink.write(&as_f32, 0.5);
        let log = shared.since(0);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].digit, '7');
        assert_eq!(log[0].source, DtmfSource::Mic);
    }

    #[test]
    #[allow(clippy::float_cmp)] // tone samples pass through unscaled — exact by construction
    fn tone_source_plays_queued_tone_then_falls_silent() {
        let (tx, rx) = channel::<Vec<f32>>();
        let mut src = DtmfToneSource {
            inbound: rx,
            residual: VecDeque::new(),
        };
        tx.send(vec![0.5; 160]).unwrap();
        let mut out = [9.0_f32; 200];
        let n = src.read(&mut out);
        assert_eq!(n, 160, "the queued tone is drained");
        assert_eq!(out[0], 0.5);
        assert_eq!(src.read(&mut out), 0, "silent once drained");
    }

    // --- stub backend for the stream-lifecycle test ---

    use astar_audio::{AudioError, DeviceId, DeviceInfo};

    struct NullHandle;
    impl StreamHandle for NullHandle {
        fn stop(self: Box<Self>) {}
        fn pause(&self) -> Result<(), AudioError> {
            Ok(())
        }
        fn resume(&self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    struct StubBackend;
    impl StubBackend {
        fn dev(name: &str, dir: Direction) -> DeviceInfo {
            DeviceInfo {
                id: DeviceId::new(name.to_string()),
                name: name.to_string(),
                direction: dir,
                channels: 1,
                native_sample_rates: vec![8_000],
            }
        }
    }
    impl AudioBackend for StubBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                Self::dev("Mic", Direction::Input),
                Self::dev("Speakers", Direction::Output),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(Self::dev("Mic", Direction::Input))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(Self::dev("Speakers", Direction::Output))
        }
        fn open_input(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            _s: Box<dyn InputSink>,
            _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            Ok(Box::new(NullHandle))
        }
        fn open_output(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            _s: Box<dyn OutputSource>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            Ok(Box::new(NullHandle))
        }
    }

    #[test]
    fn tester_starts_on_default_devices_and_plays_without_panic() {
        let shared = DtmfShared::new();
        let tester = DtmfTester::start(Box::new(StubBackend), None, None, shared)
            .expect("tester starts on default devices");
        tester.play_tone(&[0_i16; 160]); // no consumer on the stub; must not panic
    }
}
