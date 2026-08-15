// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Local mic→speaker "parrot" loopback for the harness (iax-3a3d): record
//! while the operator keys, play it back on release. Pure, no network. The
//! record/playback *decisions* live in two small audio callbacks; `LocalParrot`
//! (Task 2) is the I/O shell that opens and holds the streams.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, Compressor, DeviceInfo, Direction, InputSink, MicProfile, NoiseReducer,
    OutputSource, StreamConfig, StreamHandle, characterize,
};

use crate::metering::{Gain, Level, MeteringBackend};
use crate::session::ConsoleError;

/// What the parrot is doing right now, surfaced to the UI. Stored in an
/// `AtomicU8` so the SSE loop can read it lock-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParrotPhase {
    /// Not running (no streams open).
    Stopped,
    /// Running, idle — waiting for a key.
    Idle,
    /// Keyed: capturing mic audio.
    Recording,
    /// Released: playing the captured clip back.
    Playing,
}

impl ParrotPhase {
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    #[must_use]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Idle,
            2 => Self::Recording,
            3 => Self::Playing,
            _ => Self::Stopped,
        }
    }
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Idle => "idle",
            Self::Recording => "recording",
            Self::Playing => "playing",
        }
    }
}

/// Lock-free signals shared between the control side (HTTP/serial), the audio
/// callbacks, and the SSE reader. Cheap to clone (all `Arc`-backed).
#[derive(Clone)]
pub struct ParrotShared {
    /// Operator keying. Set by the serial bridge (CTS) and `/ptt`; read by the
    /// mic sink to gate recording.
    pub key: Arc<AtomicBool>,
    /// Current [`ParrotPhase`] as `u8`.
    pub phase: Arc<AtomicU8>,
    /// Record level (peak, post-gain) for the TX meter.
    pub tx: Level,
    /// Voice-compression toggle (iax-32cf). Set by `/parrot/compress`; read by
    /// the mic sink to decide whether to run the [`Compressor`] over capture.
    pub compress: Arc<AtomicBool>,
    /// Noise-reduction toggle (iax-a9d7). Set by `/parrot/denoise`; read by the
    /// mic sink to run the [`NoiseReducer`] (hum filter + gate) over capture.
    pub denoise: Arc<AtomicBool>,
    /// Calibrated per-mic noise profile (iax-fb8d), if measured. When present,
    /// the parrot builds its [`NoiseReducer`] from it on the next start.
    pub calibrated: Arc<Mutex<Option<MicProfile>>>,
}

impl ParrotShared {
    #[must_use]
    pub fn new() -> Self {
        Self {
            key: Arc::new(AtomicBool::new(false)),
            phase: Arc::new(AtomicU8::new(ParrotPhase::Stopped.as_u8())),
            tx: Level::new(),
            compress: Arc::new(AtomicBool::new(false)),
            denoise: Arc::new(AtomicBool::new(false)),
            calibrated: Arc::new(Mutex::new(None)),
        }
    }
    #[must_use]
    pub fn phase(&self) -> ParrotPhase {
        ParrotPhase::from_u8(self.phase.load(Ordering::Relaxed))
    }
    pub(crate) fn set_phase(&self, p: ParrotPhase) {
        self.phase.store(p.as_u8(), Ordering::Relaxed);
    }
}

impl Default for ParrotShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Input callback: append mic samples while keyed; on the key *release* edge,
/// ship the recorded clip to the speaker side and clear the buffer. Publishes
/// the record level (the post-gain `meter` from `MeteringSink`) while keyed.
pub(crate) struct ParrotMicSink {
    pub(crate) shared: ParrotShared,
    pub(crate) buf: Vec<f32>,
    pub(crate) was_keyed: bool,
    pub(crate) out: Sender<Vec<f32>>,
    /// Noise reducer (hum filter + gate) applied first when `shared.denoise`.
    pub(crate) nr: NoiseReducer,
    /// Voice compressor applied to capture when `shared.compress` is set.
    pub(crate) comp: Compressor,
    /// Scratch buffer for the processed copy (capture slice is read-only).
    pub(crate) scratch: Vec<f32>,
}

impl InputSink for ParrotMicSink {
    fn write(&mut self, samples: &[f32], meter: f32) {
        let keyed = self.shared.key.load(Ordering::Relaxed);
        self.shared.tx.set(if keyed { meter } else { 0.0 });
        if keyed {
            if !self.was_keyed {
                // Rising edge: each transmission starts the processors fresh.
                self.nr.reset();
                self.comp.reset();
            }
            let denoise = self.shared.denoise.load(Ordering::Relaxed);
            let compress = self.shared.compress.load(Ordering::Relaxed);
            if denoise || compress {
                // Chain: noise reduction (clean) before compression (lift).
                self.scratch.clear();
                self.scratch.extend_from_slice(samples);
                if denoise {
                    self.nr.process(&mut self.scratch);
                }
                if compress {
                    self.comp.process(&mut self.scratch);
                }
                self.buf.extend_from_slice(&self.scratch);
            } else {
                self.buf.extend_from_slice(samples);
            }
            self.shared.set_phase(ParrotPhase::Recording);
        } else if self.was_keyed {
            // release edge
            if self.buf.is_empty() {
                self.shared.set_phase(ParrotPhase::Idle);
            } else {
                let clip = std::mem::take(&mut self.buf);
                if self.out.send(clip).is_ok() {
                    self.shared.set_phase(ParrotPhase::Playing);
                } else {
                    self.shared.set_phase(ParrotPhase::Idle);
                }
            }
        }
        self.was_keyed = keyed;
    }
}

/// Output callback: drain queued clips to the speaker; silence when empty.
/// Returns the phase to `Idle` once the last queued clip finishes.
pub(crate) struct ParrotSpeakerSource {
    pub(crate) inbound: Receiver<Vec<f32>>,
    pub(crate) residual: VecDeque<f32>,
    pub(crate) shared: ParrotShared,
}

impl OutputSource for ParrotSpeakerSource {
    fn read(&mut self, out: &mut [f32]) -> usize {
        while let Ok(clip) = self.inbound.try_recv() {
            self.residual.extend(clip);
        }
        let n = out.len().min(self.residual.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.residual.pop_front().unwrap_or(0.0);
        }
        // Drained while playing → back to Idle. Never override Stopped (drop) or
        // a fresh Recording (re-key): only the Playing→Idle transition is ours.
        if self.residual.is_empty() && self.shared.phase() == ParrotPhase::Playing {
            self.shared.set_phase(ParrotPhase::Idle);
        }
        n
    }
}

/// Resolve a device by case-insensitive substring (must be unique), else fall
/// back to `default`. Mirrors the network path's `find_device` rule.
pub(crate) fn resolve_device(
    devices: &[DeviceInfo],
    query: Option<&str>,
    dir: Direction,
    default: Option<DeviceInfo>,
) -> Result<DeviceInfo, ConsoleError> {
    let usable = |d: &&DeviceInfo| d.direction == dir || d.direction == Direction::Duplex;
    match query {
        Some(q) if !q.is_empty() => {
            let needle = q.to_lowercase();
            let matches: Vec<&DeviceInfo> = devices
                .iter()
                .filter(usable)
                .filter(|d| d.name.to_lowercase().contains(&needle))
                .collect();
            match matches.as_slice() {
                [one] => Ok((*one).clone()),
                [] => Err(ConsoleError::Device(format!(
                    "no {dir:?} device matching {q:?}"
                ))),
                _ => Err(ConsoleError::Device(format!(
                    "{q:?} matches several {dir:?} devices"
                ))),
            }
        }
        _ => default.ok_or_else(|| ConsoleError::Device(format!("no default {dir:?} device"))),
    }
}

/// A running local parrot: owns the live input+output streams (kept alive for
/// its lifetime) and the metering backend behind them. Dropping it stops both
/// streams and marks the phase `Stopped`.
pub struct LocalParrot {
    _backend: MeteringBackend,
    _in: Box<dyn StreamHandle>,
    _out: Box<dyn StreamHandle>,
    shared: ParrotShared,
}

impl LocalParrot {
    /// Open input+output on the resolved devices and start the loopback.
    /// `shared` is the harness-wide key/phase/tx signal; `tx_gain`/`rx_gain` are
    /// the session's gain cells so the existing sliders apply here too.
    ///
    /// # Errors
    /// [`ConsoleError::Device`] if a device name does not uniquely resolve;
    /// [`ConsoleError::Audio`] if a stream fails to open.
    pub fn start(
        backend: Box<dyn AudioBackend>,
        input: Option<&str>,
        output: Option<&str>,
        shared: ParrotShared,
        tx_gain: Gain,
        rx_gain: Gain,
    ) -> Result<Self, ConsoleError> {
        let metering = MeteringBackend::with_gain(backend, tx_gain, rx_gain);
        let devices = metering.devices().map_err(ConsoleError::Audio)?;
        let in_dev = resolve_device(&devices, input, Direction::Input, metering.default_input())?;
        let out_dev = resolve_device(
            &devices,
            output,
            Direction::Output,
            metering.default_output(),
        )?;

        let (tx, rx) = channel::<Vec<f32>>();
        shared.set_phase(ParrotPhase::Idle);
        let sr = StreamConfig::default().sample_rate;
        // Use the calibrated per-mic filter if one was measured, else the
        // generic 60 Hz hum + gate.
        let nr = match &*shared.calibrated.lock().unwrap() {
            Some(profile) => NoiseReducer::from_profile(sr, profile),
            None => NoiseReducer::new(sr),
        };
        let mic = ParrotMicSink {
            shared: shared.clone(),
            buf: Vec::new(),
            was_keyed: false,
            out: tx,
            nr,
            comp: Compressor::new(sr),
            scratch: Vec::new(),
        };
        let spk = ParrotSpeakerSource {
            inbound: rx,
            residual: VecDeque::new(),
            shared: shared.clone(),
        };
        // Local parrot opens the mic directly (no router/snapshot); discard the
        // capture-overrun counter (iax-9e55).
        let in_h = metering
            .open_input(
                &in_dev,
                StreamConfig::default(),
                Box::new(mic),
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            )
            .map_err(ConsoleError::Audio)?;
        let out_h = metering
            .open_output(&out_dev, StreamConfig::default(), Box::new(spk))
            .map_err(ConsoleError::Audio)?;
        Ok(Self {
            _backend: metering,
            _in: in_h,
            _out: out_h,
            shared,
        })
    }

    /// Current phase (running parrots are never `Stopped`).
    #[must_use]
    pub fn phase(&self) -> ParrotPhase {
        self.shared.phase()
    }
}

impl Drop for LocalParrot {
    fn drop(&mut self) {
        // Streams stop when their handles drop (after this). Clear the key and
        // mark Stopped so the UI reflects "not running" immediately.
        self.shared.key.store(false, Ordering::Relaxed);
        self.shared.set_phase(ParrotPhase::Stopped);
    }
}

/// Input sink that buffers raw mic samples for calibration, up to `target`.
struct CalibrationSink {
    buf: Arc<Mutex<Vec<f32>>>,
    target: usize,
}

impl InputSink for CalibrationSink {
    fn write(&mut self, samples: &[f32], _meter: f32) {
        let mut b = self.buf.lock().unwrap();
        if b.len() < self.target {
            b.extend_from_slice(samples);
        }
    }
}

/// Record roughly `seconds` of mic input (no speech) on `input` and
/// characterize its noise into a [`MicProfile`]. Blocks while capturing, with a
/// 1 s grace timeout so a silent/stub device can't hang the caller.
///
/// # Errors
/// [`ConsoleError::Device`] if the device name doesn't uniquely resolve;
/// [`ConsoleError::Audio`] if the input stream fails to open.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn calibrate_mic(
    backend: &dyn AudioBackend,
    input: Option<&str>,
    seconds: f32,
) -> Result<MicProfile, ConsoleError> {
    let devices = backend.devices().map_err(ConsoleError::Audio)?;
    let in_dev = resolve_device(&devices, input, Direction::Input, backend.default_input())?;
    let sr = StreamConfig::default().sample_rate;
    let target = (seconds * sr as f32) as usize;
    let buf = Arc::new(Mutex::new(Vec::with_capacity(target)));
    // Mic calibration opens the mic directly (no router/snapshot); discard the
    // capture-overrun counter (iax-9e55).
    let handle = backend
        .open_input(
            &in_dev,
            StreamConfig::default(),
            Box::new(CalibrationSink {
                buf: Arc::clone(&buf),
                target,
            }),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        )
        .map_err(ConsoleError::Audio)?;

    let deadline = Instant::now() + Duration::from_secs_f32(seconds + 1.0);
    while buf.lock().unwrap().len() < target && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    handle.stop();
    let samples = std::mem::take(&mut *buf.lock().unwrap());
    Ok(characterize(&samples, sr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn mic(shared: &ParrotShared) -> (ParrotMicSink, Receiver<Vec<f32>>) {
        let (tx, rx) = channel();
        (
            ParrotMicSink {
                shared: shared.clone(),
                buf: Vec::new(),
                was_keyed: false,
                out: tx,
                nr: NoiseReducer::new(StreamConfig::default().sample_rate),
                comp: Compressor::new(StreamConfig::default().sample_rate),
                scratch: Vec::new(),
            },
            rx,
        )
    }

    #[test]
    #[allow(clippy::float_cmp)] // samples pass through unscaled — exact by construction
    fn records_while_keyed_then_plays_back_on_release() {
        let shared = ParrotShared::new();
        let (mut sink, rx) = mic(&shared);
        shared.key.store(true, Ordering::Relaxed);
        sink.write(&[0.5; 160], 0.5);
        sink.write(&[0.25; 160], 0.25);
        assert_eq!(shared.phase(), ParrotPhase::Recording);
        assert!(rx.try_recv().is_err(), "nothing queued until release");

        // release: the clip is queued for the speaker source (the sole channel
        // consumer — don't drain `rx` here or the source would see nothing).
        shared.key.store(false, Ordering::Relaxed);
        sink.write(&[0.0; 160], 0.0);
        assert_eq!(shared.phase(), ParrotPhase::Playing);

        // speaker drains the queued clip, then returns to Idle
        let mut src = ParrotSpeakerSource {
            inbound: rx,
            residual: VecDeque::new(),
            shared: shared.clone(),
        };
        let mut out = [9.0_f32; 320];
        let n = src.read(&mut out);
        assert_eq!(n, 320, "both keyed buffers concatenated and drained");
        assert_eq!(out[0], 0.5);
        assert_eq!(out[160], 0.25);
        let mut tail = [9.0_f32; 16];
        let n2 = src.read(&mut tail);
        assert_eq!(n2, 0, "queue drained");
        assert_eq!(
            shared.phase(),
            ParrotPhase::Idle,
            "Playing→Idle when drained"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)] // tx meter is hard-floored to exactly 0.0 — exactness is the point
    fn nothing_records_while_unkeyed() {
        let shared = ParrotShared::new();
        let (mut sink, rx) = mic(&shared);
        sink.write(&[0.5; 160], 0.5); // key never set
        assert!(rx.try_recv().is_err());
        assert_eq!(
            shared.phase(),
            ParrotPhase::Stopped,
            "unchanged while idle/unkeyed"
        );
        assert_eq!(shared.tx.get(), 0.0, "tx meter floored while unkeyed");
    }

    #[test]
    #[allow(clippy::cast_precision_loss)] // small sample indices → exact in f32
    fn noise_reduction_filters_capture_when_enabled() {
        let shared = ParrotShared::new();
        shared.denoise.store(true, Ordering::Relaxed);
        shared.key.store(true, Ordering::Relaxed);
        let (mut sink, _rx) = mic(&shared);
        // 60 Hz hum into capture → the hum filter attenuates it.
        let sr = StreamConfig::default().sample_rate as f32;
        let hum: Vec<f32> = (0..1600)
            .map(|i| (std::f32::consts::TAU * 60.0 * i as f32 / sr).sin())
            .collect();
        sink.write(&hum, 0.5);
        let tail = sink.buf[sink.buf.len() / 2..]
            .iter()
            .fold(0.0_f32, |p, &s| p.max(s.abs()));
        assert!(
            tail < 0.3,
            "60 Hz hum should be filtered from capture: {tail}"
        );
    }

    #[test]
    fn compression_processes_captured_audio_when_enabled() {
        let shared = ParrotShared::new();
        shared.compress.store(true, Ordering::Relaxed);
        shared.key.store(true, Ordering::Relaxed);
        let (mut sink, _rx) = mic(&shared);
        sink.write(&[0.5_f32; 800], 0.5); // 100 ms of a loud tone
        // Once the envelope settles, the loud tail is compressed below the raw
        // input level (−24 dB threshold, 4:1, +6 dB makeup → net attenuation).
        let last = *sink.buf.last().unwrap();
        assert!(
            last < 0.5,
            "compressed tail {last} attenuated below raw 0.5"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)] // capture must be byte-for-byte untouched
    fn capture_is_untouched_when_compression_disabled() {
        let shared = ParrotShared::new(); // compress defaults off
        shared.key.store(true, Ordering::Relaxed);
        let (mut sink, _rx) = mic(&shared);
        sink.write(&[0.5_f32; 160], 0.5);
        assert!(sink.buf.iter().all(|&s| s == 0.5), "raw capture unchanged");
    }

    #[test]
    fn empty_key_tap_records_no_clip_and_returns_idle() {
        let shared = ParrotShared::new();
        let (mut sink, rx) = mic(&shared);
        shared.key.store(true, Ordering::Relaxed);
        // no audio callback arrives while keyed (instant tap), then release:
        shared.key.store(false, Ordering::Relaxed);
        sink.was_keyed = true; // simulate "was keyed last callback"
        sink.write(&[0.0; 160], 0.0);
        assert!(rx.try_recv().is_err(), "empty tap queues nothing");
        assert_eq!(shared.phase(), ParrotPhase::Idle);
    }

    use astar_audio::{AudioError, DeviceId};

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

    struct LoopBackend;
    impl LoopBackend {
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
    impl AudioBackend for LoopBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                Self::dev("KT USB Audio", Direction::Input),
                Self::dev("Mac mini Speakers", Direction::Output),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(Self::dev("KT USB Audio", Direction::Input))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(Self::dev("Mac mini Speakers", Direction::Output))
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
    fn start_opens_streams_and_drop_marks_stopped() {
        let shared = ParrotShared::new();
        let parrot = LocalParrot::start(
            Box::new(LoopBackend),
            None,
            None,
            shared.clone(),
            Gain::new(),
            Gain::new(),
        )
        .expect("parrot starts on default devices");
        assert_eq!(parrot.phase(), ParrotPhase::Idle, "running parrot is Idle");
        drop(parrot);
        assert_eq!(
            shared.phase(),
            ParrotPhase::Stopped,
            "drop stops the parrot"
        );
    }

    #[test]
    fn start_rejects_unknown_device() {
        let shared = ParrotShared::new();
        let result = LocalParrot::start(
            Box::new(LoopBackend),
            Some("no-such-mic"),
            None,
            shared,
            Gain::new(),
            Gain::new(),
        );
        assert!(
            matches!(result, Err(ConsoleError::Device(_))),
            "unknown device name fails with Device error"
        );
    }
}
