// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio-level metering as an `AudioBackend` decorator (iax-dd42).
//!
//! Wraps any backend, taps the per-callback peak amplitude the audio layer
//! already computes (`InputSink::write(.., meter)` for TX, `OutputSource::meter`
//! for RX) into lock-free [`Level`] cells, and forwards to the inner sink/source
//! unchanged. The core client is not modified.
//!
//! Volume gain (au-3365): [`Gain`] cells allow live TX/RX volume adjustment
//! without touching the core client. Scaling is applied in the decorator; the
//! fast path (gain == 1.0) is a true no-op.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub use astar_audio::peak_to_dbfs;
use astar_audio::{
    AudioBackend, AudioError, Compressor, DeviceInfo, InputSink, MicProfile, NoiseReducer,
    OutputSource, StreamConfig, StreamHandle, peak,
};

/// Capture-path DSP toggles (iax-d50d), shared with the harness checkboxes.
/// Applied in the metering decorator so the network call gets the same
/// noise-reduction + compression as the local parrot.
#[derive(Clone)]
pub struct MicDspFlags {
    pub denoise: Arc<AtomicBool>,
    pub compress: Arc<AtomicBool>,
    /// The calibrated per-mic profile (iax-fb8d), if measured. When present, the
    /// noise reducer is built from it (per-mic notches) instead of the generic
    /// 60 Hz hum filter — so a real call gets the same whine removal as the
    /// parrot. Shared with `ParrotShared.calibrated`.
    pub profile: Arc<Mutex<Option<MicProfile>>>,
}

/// A lock-free f32 level cell shared between an audio callback (writer) and a
/// front-end poll (reader). Stores the f32 bit pattern in an `AtomicU32` — the
/// same approach the call runtime uses for smoothed RTT.
#[derive(Clone, Default)]
pub struct Level(Arc<AtomicU32>);

impl Level {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicU32::new(0)))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    #[must_use]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// A lock-free f32 gain cell shared between a control path (writer) and an
/// audio callback (reader). Stores the f32 bit pattern in an `AtomicU32` —
/// identical to [`Level`] but defaults to **1.0** (unity gain, not silence).
/// Clamping is the caller's responsibility; `Gain` just stores/loads.
#[derive(Clone)]
pub struct Gain(Arc<AtomicU32>);

impl Gain {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicU32::new(1.0_f32.to_bits())))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    #[must_use]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

impl Default for Gain {
    fn default() -> Self {
        Self::new()
    }
}

/// `AudioBackend` decorator that taps TX (mic input) and RX (decoded output)
/// peak levels into shared [`Level`] cells and optionally applies a live
/// volume [`Gain`] to the audio buffers. Wrap a real backend, hand it to the
/// `Client`, and read the levels via [`MeteringBackend::tx_level`] /
/// [`MeteringBackend::rx_level`] (clone the handles before the backend is
/// moved into the client).
pub struct MeteringBackend {
    inner: Box<dyn AudioBackend>,
    tx: Level,
    rx: Level,
    tx_gain: Gain,
    rx_gain: Gain,
    /// Capture-path DSP toggles. `None` = no DSP (the local parrot, which runs
    /// its own DSP downstream); `Some` enables it here (the network call).
    dsp: Option<MicDspFlags>,
}

impl MeteringBackend {
    /// Create with fresh unity gains. Identical to the previous constructor so
    /// existing call sites compile unchanged.
    #[must_use]
    pub fn new(inner: Box<dyn AudioBackend>) -> Self {
        Self::with_gain(inner, Gain::new(), Gain::new())
    }

    /// Create with caller-supplied gain cells (persisted across reconnects).
    /// No capture DSP — used by the local parrot, which applies its own.
    #[must_use]
    pub fn with_gain(inner: Box<dyn AudioBackend>, tx_gain: Gain, rx_gain: Gain) -> Self {
        Self {
            inner,
            tx: Level::new(),
            rx: Level::new(),
            tx_gain,
            rx_gain,
            dsp: None,
        }
    }

    /// Like [`Self::with_gain`] but also runs noise-reduction + compression on
    /// the capture path when the shared flags are set (the network call).
    #[must_use]
    pub fn with_gain_and_dsp(
        inner: Box<dyn AudioBackend>,
        tx_gain: Gain,
        rx_gain: Gain,
        dsp: MicDspFlags,
    ) -> Self {
        Self {
            inner,
            tx: Level::new(),
            rx: Level::new(),
            tx_gain,
            rx_gain,
            dsp: Some(dsp),
        }
    }

    #[must_use]
    pub fn tx_level(&self) -> Level {
        self.tx.clone()
    }
    #[must_use]
    pub fn rx_level(&self) -> Level {
        self.rx.clone()
    }
    #[must_use]
    pub fn tx_gain(&self) -> Gain {
        self.tx_gain.clone()
    }
    #[must_use]
    pub fn rx_gain(&self) -> Gain {
        self.rx_gain.clone()
    }
}

impl AudioBackend for MeteringBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        self.inner.devices()
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        self.inner.default_input()
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        self.inner.default_output()
    }
    fn open_input(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        sink: Box<dyn InputSink>,
        overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let dsp = self.dsp.as_ref().map(|flags| {
            // Build the noise reducer from the calibrated per-mic profile if one
            // was measured, else the generic 60 Hz hum + gate.
            let nr = match &*flags.profile.lock().unwrap() {
                Some(profile) => NoiseReducer::from_profile(config.sample_rate, profile),
                None => NoiseReducer::new(config.sample_rate),
            };
            MicDsp {
                flags: flags.clone(),
                nr,
                comp: Compressor::new(config.sample_rate),
            }
        });
        // Forward the overrun cell unchanged so the capture overrun count
        // reaches the real backend's capture thread (iax-9e55).
        self.inner.open_input(
            device,
            config,
            Box::new(MeteringSink {
                inner: sink,
                level: self.tx.clone(),
                gain: self.tx_gain.clone(),
                buf: Vec::new(),
                dsp,
            }),
            overruns,
        )
    }
    fn open_output(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.inner.open_output(
            device,
            config,
            Box::new(MeteringSource {
                inner: source,
                level: self.rx.clone(),
                gain: self.rx_gain.clone(),
            }),
        )
    }
}

/// Per-stream capture DSP state owned by a [`MeteringSink`].
struct MicDsp {
    flags: MicDspFlags,
    nr: NoiseReducer,
    comp: Compressor,
}

struct MeteringSink {
    inner: Box<dyn InputSink>,
    level: Level,
    gain: Gain,
    /// Reusable scratch buffer for the gain != 1.0 / DSP path (avoids
    /// per-callback allocation on the hot audio thread).
    buf: Vec<f32>,
    /// Capture DSP (network call only); `None` = pass-through.
    dsp: Option<MicDsp>,
}
impl InputSink for MeteringSink {
    #[allow(clippy::float_cmp)] // exact unity-gain sentinel — epsilon would change the fast path
    fn write(&mut self, samples: &[f32], meter: f32) {
        let g = self.gain.get();
        let dsp_on = self.dsp.as_ref().is_some_and(|d| {
            d.flags.denoise.load(Ordering::Relaxed) || d.flags.compress.load(Ordering::Relaxed)
        });
        if g == 1.0 && !dsp_on {
            // Fast path: no scaling or DSP — forward as-is.
            self.level.set(meter);
            self.inner.write(samples, meter);
            return;
        }
        // Copy to scratch, apply gain, then noise-reduction → compression.
        self.buf.clear();
        if g == 1.0 {
            self.buf.extend_from_slice(samples);
        } else {
            self.buf
                .extend(samples.iter().map(|&s| (s * g).clamp(-1.0, 1.0)));
        }
        if let Some(d) = &mut self.dsp {
            if d.flags.denoise.load(Ordering::Relaxed) {
                d.nr.process(&mut self.buf);
            }
            if d.flags.compress.load(Ordering::Relaxed) {
                d.comp.process(&mut self.buf);
            }
        }
        let new_meter = peak(&self.buf);
        self.level.set(new_meter);
        self.inner.write(&self.buf, new_meter);
    }
}

struct MeteringSource {
    inner: Box<dyn OutputSource>,
    level: Level,
    gain: Gain,
}
impl OutputSource for MeteringSource {
    #[allow(clippy::float_cmp)] // exact unity-gain sentinel — epsilon would change the fast path
    fn read(&mut self, out: &mut [f32]) -> usize {
        let n = self.inner.read(out);
        let g = self.gain.get();
        if g != 1.0 {
            // Scale and clamp in-place. The audio layer computes peak(out)
            // AFTER read() returns, so the meter reflects the post-gain buffer
            // automatically — no meter recomputation needed here.
            for s in &mut out[..n] {
                *s = (*s * g).clamp(-1.0, 1.0);
            }
        }
        n
    }
    fn meter(&mut self, level: f32) {
        self.level.set(level);
        self.inner.meter(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn peak_to_dbfs_known_points() {
        assert!((peak_to_dbfs(1.0) - 0.0).abs() < 1e-3, "full scale ~ 0 dB");
        assert!((peak_to_dbfs(0.5) + 6.0206).abs() < 1e-2, "half ~ -6 dB");
        assert!(
            (peak_to_dbfs(0.0) + 60.0).abs() < 1e-6,
            "silence floors at -60"
        );
        assert!(
            (peak_to_dbfs(1e-9) + 60.0).abs() < 1e-6,
            "below floor clamps to -60"
        );
    }

    #[test]
    fn peak_to_dbfs_nan_floors() {
        assert!(
            (peak_to_dbfs(f32::NAN) + 60.0).abs() < 1e-6,
            "NaN floors at -60"
        );
    }

    #[test]
    fn level_round_trips_f32() {
        let l = Level::new();
        assert!((l.get() - 0.0).abs() < 1e-9);
        l.set(0.42);
        assert!((l.get() - 0.42).abs() < 1e-9);
    }

    #[test]
    fn gain_defaults_to_unity() {
        let g = Gain::new();
        assert!((g.get() - 1.0).abs() < 1e-9, "default must be 1.0");
    }

    #[test]
    fn gain_round_trips_f32() {
        let g = Gain::new();
        g.set(0.5);
        assert!((g.get() - 0.5).abs() < 1e-9);
        g.set(2.0);
        assert!((g.get() - 2.0).abs() < 1e-9);
    }

    /// Recording sink to prove the metering wrapper forwards samples + meter.
    struct RecSink {
        samples: Arc<Mutex<Vec<f32>>>,
        meters: Arc<Mutex<Vec<f32>>>,
    }
    impl InputSink for RecSink {
        fn write(&mut self, samples: &[f32], meter: f32) {
            self.samples.lock().unwrap().extend_from_slice(samples);
            self.meters.lock().unwrap().push(meter);
        }
    }

    #[test]
    fn metering_sink_taps_meter_and_forwards() {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let meters = Arc::new(Mutex::new(Vec::new()));
        let level = Level::new();
        let mut sink = MeteringSink {
            inner: Box::new(RecSink {
                samples: Arc::clone(&samples),
                meters: Arc::clone(&meters),
            }),
            level: level.clone(),
            gain: Gain::new(), // unity — fast path
            buf: Vec::new(),
            dsp: None,
        };
        sink.write(&[0.1, 0.2], 0.5);
        assert!((level.get() - 0.5).abs() < 1e-6, "tapped the meter");
        assert_eq!(*meters.lock().unwrap(), vec![0.5], "forwarded the meter");
        assert_eq!(
            *samples.lock().unwrap(),
            vec![0.1, 0.2],
            "forwarded samples"
        );
    }

    #[test]
    fn metering_sink_gain_unity_is_fast_path() {
        // Gain of 1.0 must pass samples and meter through unchanged.
        let samples_out = Arc::new(Mutex::new(Vec::new()));
        let meters_out = Arc::new(Mutex::new(Vec::new()));
        let level = Level::new();
        let gain = Gain::new(); // 1.0
        let mut sink = MeteringSink {
            inner: Box::new(RecSink {
                samples: Arc::clone(&samples_out),
                meters: Arc::clone(&meters_out),
            }),
            level: level.clone(),
            gain,
            buf: Vec::new(),
            dsp: None,
        };
        sink.write(&[0.3, -0.4], 0.4);
        assert_eq!(
            *samples_out.lock().unwrap(),
            vec![0.3, -0.4],
            "unity gain must not alter samples"
        );
        assert!((level.get() - 0.4).abs() < 1e-6, "level == supplied meter");
    }

    #[test]
    fn metering_sink_gain_scales_and_clamps() {
        // Gain of 2.0 should double and clamp at ±1.0; meter is recomputed.
        let samples_out = Arc::new(Mutex::new(Vec::new()));
        let meters_out = Arc::new(Mutex::new(Vec::new()));
        let level = Level::new();
        let gain = Gain::new();
        gain.set(2.0);
        let mut sink = MeteringSink {
            inner: Box::new(RecSink {
                samples: Arc::clone(&samples_out),
                meters: Arc::clone(&meters_out),
            }),
            level: level.clone(),
            gain,
            buf: Vec::new(),
            dsp: None,
        };
        sink.write(&[0.3, -0.6, 0.8], 0.8);
        let s = samples_out.lock().unwrap().clone();
        // 0.3*2=0.6 (not clamped), -0.6*2=-1.2→-1.0, 0.8*2=1.6→1.0
        assert!((s[0] - 0.6).abs() < 1e-6, "0.3 * 2 = 0.6");
        assert!((s[1] - (-1.0)).abs() < 1e-6, "-0.6 * 2 clamped to -1.0");
        assert!((s[2] - 1.0).abs() < 1e-6, "0.8 * 2 clamped to 1.0");
        // Meter forwarded is recomputed peak of [0.6, -1.0, 1.0] = 1.0
        let m = meters_out.lock().unwrap().clone();
        assert!((m[0] - 1.0).abs() < 1e-6, "meter recomputed to 1.0");
        assert!((level.get() - 1.0).abs() < 1e-6, "level reflects new peak");
    }

    #[test]
    fn metering_sink_applies_capture_compression_when_flagged() {
        let samples_out = Arc::new(Mutex::new(Vec::new()));
        let meters_out = Arc::new(Mutex::new(Vec::new()));
        let mut sink = MeteringSink {
            inner: Box::new(RecSink {
                samples: Arc::clone(&samples_out),
                meters: Arc::clone(&meters_out),
            }),
            level: Level::new(),
            gain: Gain::new(),
            buf: Vec::new(),
            dsp: Some(MicDsp {
                flags: MicDspFlags {
                    denoise: Arc::new(AtomicBool::new(false)),
                    compress: Arc::new(AtomicBool::new(true)),
                    profile: Arc::new(Mutex::new(None)),
                },
                nr: NoiseReducer::new(StreamConfig::default().sample_rate),
                comp: Compressor::new(StreamConfig::default().sample_rate),
            }),
        };
        // Sustained loud capture → the compressor settles → forwarded tail is
        // attenuated below the raw input level (net gain after makeup < 1).
        sink.write(&vec![0.5_f32; 6000], 0.5);
        let s = samples_out.lock().unwrap();
        assert!(
            s.last().unwrap().abs() < 0.5,
            "compression attenuates the loud tail: {}",
            s.last().unwrap()
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss)] // small sample indices → exact in f32
    fn metering_sink_applies_the_calibrated_notch() {
        use astar_audio::{MicProfile, NotchSpec};
        let samples_out = Arc::new(Mutex::new(Vec::new()));
        let meters_out = Arc::new(Mutex::new(Vec::new()));
        let profile = MicProfile {
            highpass_hz: 90.0,
            notches: vec![NotchSpec {
                freq_hz: 450.0,
                q: 20.0,
            }],
            noise_floor_dbfs: -50.0,
            gate_threshold_db: -44.0,
        };
        let mut sink = MeteringSink {
            inner: Box::new(RecSink {
                samples: Arc::clone(&samples_out),
                meters: Arc::clone(&meters_out),
            }),
            level: Level::new(),
            gain: Gain::new(),
            buf: Vec::new(),
            dsp: Some(MicDsp {
                flags: MicDspFlags {
                    denoise: Arc::new(AtomicBool::new(true)),
                    compress: Arc::new(AtomicBool::new(false)),
                    profile: Arc::new(Mutex::new(Some(profile.clone()))),
                },
                nr: NoiseReducer::from_profile(StreamConfig::default().sample_rate, &profile),
                comp: Compressor::new(StreamConfig::default().sample_rate),
            }),
        };
        // A 450 Hz tone (the mic's "whine") → the calibrated notch removes it.
        let sr = StreamConfig::default().sample_rate as f32;
        let tone: Vec<f32> = (0..4000)
            .map(|i| (std::f32::consts::TAU * 450.0 * i as f32 / sr).sin())
            .collect();
        sink.write(&tone, 0.5);
        let s = samples_out.lock().unwrap();
        let tail = s[s.len() / 2..]
            .iter()
            .fold(0.0_f32, |p, &x| p.max(x.abs()));
        assert!(
            tail < 0.3,
            "calibrated 450 Hz notch should remove the tone: {tail}"
        );
    }

    /// Recording source to prove the output wrapper taps `meter()`.
    struct RecSource {
        levels: Arc<Mutex<Vec<f32>>>,
    }
    impl OutputSource for RecSource {
        fn read(&mut self, out: &mut [f32]) -> usize {
            out.fill(0.0);
            out.len()
        }
        fn meter(&mut self, level: f32) {
            self.levels.lock().unwrap().push(level);
        }
    }

    #[test]
    fn metering_source_taps_meter_and_forwards() {
        let inner_levels = Arc::new(Mutex::new(Vec::new()));
        let level = Level::new();
        let mut src = MeteringSource {
            inner: Box::new(RecSource {
                levels: Arc::clone(&inner_levels),
            }),
            level: level.clone(),
            gain: Gain::new(), // unity
        };
        let mut buf = [1.0_f32; 4];
        assert_eq!(src.read(&mut buf), 4);
        src.meter(0.75);
        assert!((level.get() - 0.75).abs() < 1e-6, "tapped the output meter");
        assert_eq!(
            *inner_levels.lock().unwrap(),
            vec![0.75],
            "forwarded to inner"
        );
    }

    #[test]
    fn metering_source_gain_unity_is_noop() {
        // Unity gain: read() must not alter the samples.
        let level = Level::new();
        let gain = Gain::new(); // 1.0
        let mut src = MeteringSource {
            inner: Box::new(RecSource {
                levels: Arc::new(Mutex::new(Vec::new())),
            }),
            level,
            gain,
        };
        let mut buf = [0.0_f32; 4]; // RecSource writes 0.0 into buf
        src.read(&mut buf);
        // RecSource fills with 0.0 and unity gain keeps them at 0.0 — no-op.
        assert!(buf.iter().all(|&s| s == 0.0), "unity gain: no change");
    }

    #[test]
    fn metering_source_gain_scales_and_clamps() {
        // Source with a fixed buffer; apply gain, verify clamping.
        struct FixedSource(Vec<f32>);
        impl OutputSource for FixedSource {
            fn read(&mut self, out: &mut [f32]) -> usize {
                let n = out.len().min(self.0.len());
                out[..n].copy_from_slice(&self.0[..n]);
                n
            }
            fn meter(&mut self, _: f32) {}
        }
        let level = Level::new();
        let gain = Gain::new();
        gain.set(3.0);
        let mut src = MeteringSource {
            inner: Box::new(FixedSource(vec![0.2, -0.5, 0.4])),
            level,
            gain,
        };
        let mut buf = [0.0_f32; 3];
        assert_eq!(src.read(&mut buf), 3);
        // 0.2*3=0.6, -0.5*3=-1.5→-1.0, 0.4*3=1.2→1.0
        assert!((buf[0] - 0.6).abs() < 1e-6, "0.2 * 3 = 0.6");
        assert!((buf[1] - (-1.0)).abs() < 1e-6, "-0.5 * 3 clamped to -1.0");
        assert!((buf[2] - 1.0).abs() < 1e-6, "0.4 * 3 clamped to 1.0");
    }
}
