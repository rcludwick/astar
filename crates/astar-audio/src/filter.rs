// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Biquad IIR filters (iax-a9d7).
//!
//! A single [`Biquad`] section (RBJ "Audio EQ Cookbook" coefficients) plus a
//! [`HumFilter`] that chains a high-pass with a comb of notches to strip mains
//! hum (50/60 Hz fundamental + harmonics) and low-frequency rumble from voice.
//! f32 mono at the stream rate (8 kHz for IAX2).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::f32::consts::TAU;

/// One biquad section in transposed direct-form II.
#[derive(Debug, Clone)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    s1: f32,
    s2: f32,
}

impl Biquad {
    /// A 2nd-order high-pass at `freq` Hz with quality `q`.
    #[must_use]
    pub fn highpass(sample_rate: u32, freq: f32, q: f32) -> Self {
        let w0 = TAU * freq / sample_rate as f32;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b0 = f32::midpoint(1.0, cos);
        let b1 = -(1.0 + cos);
        let b2 = f32::midpoint(1.0, cos);
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;
        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    /// A 2nd-order low-pass at `freq` Hz with quality `q`.
    #[must_use]
    pub fn lowpass(sample_rate: u32, freq: f32, q: f32) -> Self {
        let w0 = TAU * freq / sample_rate as f32;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b0 = (1.0 - cos) / 2.0;
        let b1 = 1.0 - cos;
        let b2 = (1.0 - cos) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;
        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    /// A band-reject (notch) at `freq` Hz with quality `q` (higher = narrower).
    #[must_use]
    pub fn notch(sample_rate: u32, freq: f32, q: f32) -> Self {
        let w0 = TAU * freq / sample_rate as f32;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let b0 = 1.0;
        let b1 = -2.0 * cos;
        let b2 = 1.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;
        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    fn normalized(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    /// Filter one sample (transposed direct-form II).
    pub fn process_sample(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Filter a buffer in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.process_sample(*s);
        }
    }

    /// Clear the filter state.
    pub fn reset(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
    }
}

/// Tuning for the [`HumFilter`].
#[derive(Debug, Clone, Copy)]
pub struct HumFilterParams {
    /// High-pass corner (Hz): removes DC, rumble, and the mains fundamental.
    pub highpass_hz: f32,
    /// Mains base frequency (Hz): 60 in the Americas, 50 elsewhere.
    pub base_hz: f32,
    /// Number of notches placed at `base_hz × 1..=harmonics`.
    pub harmonics: usize,
    /// Notch quality (higher = narrower, more voice preserved).
    pub notch_q: f32,
}

impl Default for HumFilterParams {
    fn default() -> Self {
        Self {
            highpass_hz: 90.0,
            base_hz: 60.0,
            harmonics: 3,
            notch_q: 12.0,
        }
    }
}

/// Strips mains hum from voice: a high-pass followed by a comb of notches at
/// the mains fundamental and its harmonics.
#[derive(Debug, Clone)]
pub struct HumFilter {
    highpass: Biquad,
    notches: Vec<Biquad>,
}

impl HumFilter {
    /// A hum filter at `sample_rate` with 60 Hz defaults.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self::with_params(sample_rate, HumFilterParams::default())
    }

    /// A hum filter from an explicit `(freq_hz, q)` notch list plus a high-pass
    /// — the form a measured [`crate::MicProfile`] uses.
    #[must_use]
    pub fn from_notches(sample_rate: u32, highpass_hz: f32, notches: &[(f32, f32)]) -> Self {
        Self {
            highpass: Biquad::highpass(sample_rate, highpass_hz, 0.707),
            notches: notches
                .iter()
                .map(|&(f, q)| Biquad::notch(sample_rate, f, q))
                .collect(),
        }
    }

    /// A hum filter at `sample_rate` with explicit parameters.
    #[must_use]
    pub fn with_params(sample_rate: u32, params: HumFilterParams) -> Self {
        let highpass = Biquad::highpass(sample_rate, params.highpass_hz, 0.707);
        let notches = (1..=params.harmonics)
            .map(|k| Biquad::notch(sample_rate, params.base_hz * k as f32, params.notch_q))
            .collect();
        Self { highpass, notches }
    }

    /// Filter one sample: high-pass, then each notch in series.
    pub fn process_sample(&mut self, x: f32) -> f32 {
        let mut y = self.highpass.process_sample(x);
        for n in &mut self.notches {
            y = n.process_sample(y);
        }
        y
    }

    /// Filter a buffer in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.process_sample(*s);
        }
    }

    /// Clear all section state.
    pub fn reset(&mut self) {
        self.highpass.reset();
        for n in &mut self.notches {
            n.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state peak of a `freq` sine after passing through `f`.
    fn steady_peak(f: &mut Biquad, sample_rate: u32, freq: f32, cycles: usize) -> f32 {
        let n = ((sample_rate as f32 / freq) * cycles as f32) as usize;
        let mut peak = 0.0_f32;
        for i in 0..n {
            let x = (TAU * freq * i as f32 / sample_rate as f32).sin();
            let y = f.process_sample(x);
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn highpass_removes_dc() {
        let mut hp = Biquad::highpass(8000, 90.0, 0.707);
        let mut last = 0.0;
        for _ in 0..4000 {
            last = hp.process_sample(1.0); // constant DC
        }
        assert!(last.abs() < 0.01, "DC not removed: {last}");
    }

    #[test]
    fn highpass_passes_a_midband_tone() {
        let mut hp = Biquad::highpass(8000, 90.0, 0.707);
        let peak = steady_peak(&mut hp, 8000, 1500.0, 40);
        assert!(peak > 0.9, "1.5 kHz should pass ~unattenuated: {peak}");
    }

    #[test]
    fn lowpass_passes_a_low_tone() {
        // 1 kHz sits well inside a 3.6 kHz low-pass — should pass ~unattenuated.
        let mut lp = Biquad::lowpass(16_000, 3_600.0, 0.707);
        let peak = steady_peak(&mut lp, 16_000, 1_000.0, 40);
        assert!(peak > 0.9, "1 kHz should pass the 3.6 kHz low-pass: {peak}");
    }

    #[test]
    fn lowpass_attenuates_a_high_tone() {
        // 6 kHz is well above a 3.6 kHz corner — a single section should already
        // drop it by more than 12 dB (peak < 0.25).
        let mut lp = Biquad::lowpass(16_000, 3_600.0, 0.707);
        let peak = steady_peak(&mut lp, 16_000, 6_000.0, 60);
        assert!(
            peak < 0.25,
            "6 kHz should be attenuated by the low-pass: {peak}"
        );
    }

    #[test]
    fn notch_kills_its_target_tone() {
        let mut n = Biquad::notch(8000, 60.0, 12.0);
        let peak = steady_peak(&mut n, 8000, 60.0, 120);
        assert!(peak < 0.1, "60 Hz notch should crush 60 Hz: {peak}");
    }

    #[test]
    fn notch_passes_an_off_target_tone() {
        let mut n = Biquad::notch(8000, 60.0, 12.0);
        let peak = steady_peak(&mut n, 8000, 1000.0, 40);
        assert!(peak > 0.9, "1 kHz should pass the 60 Hz notch: {peak}");
    }

    #[test]
    #[allow(clippy::float_cmp)] // identical state + input → exact
    fn process_matches_sample_loop() {
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.3).sin()).collect();
        let mut a = Biquad::highpass(8000, 120.0, 0.707);
        let mut buf = input.clone();
        a.process(&mut buf);
        let mut b = Biquad::highpass(8000, 120.0, 0.707);
        let looped: Vec<f32> = input.iter().map(|&x| b.process_sample(x)).collect();
        assert_eq!(buf, looped);
    }

    /// Steady-state peak of a `freq` sine after passing through a `HumFilter`.
    fn hum_peak(f: &mut HumFilter, sample_rate: u32, freq: f32, cycles: usize) -> f32 {
        let n = ((sample_rate as f32 / freq) * cycles as f32) as usize;
        let mut peak = 0.0_f32;
        for i in 0..n {
            let x = (TAU * freq * i as f32 / sample_rate as f32).sin();
            let y = f.process_sample(x);
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn hum_filter_crushes_the_mains_fundamental_and_harmonics() {
        for hz in [60.0, 120.0, 180.0] {
            let mut h = HumFilter::new(8000);
            let peak = hum_peak(&mut h, 8000, hz, 120);
            assert!(peak < 0.2, "{hz} Hz hum not removed: {peak}");
        }
    }

    #[test]
    fn hum_filter_passes_the_voice_band() {
        let mut h = HumFilter::new(8000);
        let peak = hum_peak(&mut h, 8000, 1000.0, 40);
        assert!(peak > 0.9, "1 kHz voice tone should pass: {peak}");
    }
}
