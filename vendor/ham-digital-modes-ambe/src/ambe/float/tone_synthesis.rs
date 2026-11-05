// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tone-frame synthesis shared by D-STAR and AMBE+2 half-rate: turns a decoded tone frame (a single
//! tone, a DTMF digit, or a call-progress tone) into 20 ms of PCM sinusoids, with phase carried
//! across frames so a held tone is continuous.
//!
//! **Levels, measured against the live chip's decoder** (`examples/ambe_tone_level_probe.rs`): D-STAR tone
//! frames carry an 8-bit `volume` that sets each tone's amplitude exponentially ([`dstar_tone_amplitude`]),
//! identically for a single tone and for each tone of a dual tone. AMBE+2 half-rate tone frames carry a level
//! field the chip ignores when decoding: with `DCMODE_IN`'s `TS_ENABLE` set (required for it to synthesize tone
//! frames at all) its output is always ~24000 rms in total ([`AMBE_PLUS_2_TONE_RMS`]), i.e. a single tone of peak
//! `24000*sqrt(2)` (clipped by the chip at full scale) or two tones of peak 24000 each.

use super::tia_102_baba::unvoiced_synthesis::N;
use std::f64::consts::PI;

/// Peak amplitude (16-bit PCM units) used when a tone frame carries no usable level.
pub const DEFAULT_TONE_PEAK: f64 = 4000.0;

/// D-STAR: each tone's amplitude (16-bit PCM units) for a tone frame's 8-bit `volume`, from the chip's decoder:
/// exponential, `3268 * exp(0.04084 * (volume - 180))` (a factor of 1.8435 per 15 steps, measured at volumes 105-210
/// with the same per-tone value for single tones and each tone of a DTMF pair; the chip saturates near 238).
pub fn dstar_tone_amplitude(volume: u32) -> f64 {
    (3268.0 * (0.04084 * (volume as f64 - 180.0)).exp()).min(FULL_SCALE_PEAK)
}

/// Largest 16-bit PCM peak. Tone levels are clamped to it (the chip saturates near volume 238); speech synthesis output is
/// not clamped, so consumers must convert to 16-bit with saturation ([`saturate_to_i16`]).
pub const FULL_SCALE_PEAK: f64 = 32767.0;

/// Converts one float PCM sample to 16 bits with rounding and saturation.
pub fn saturate_to_i16(sample: f64) -> i16 {
    sample.round().clamp(-32768.0, 32767.0) as i16
}

/// The inverse of [`dstar_tone_amplitude`]: the `volume` field for a desired per-tone amplitude.
pub fn dstar_tone_volume_for_amplitude(amplitude: f64) -> u32 {
    (180.0 + (amplitude.max(1.0) / 3268.0).ln() / 0.04084)
        .round()
        .clamp(0.0, 255.0) as u32
}

/// AMBE+2 half-rate: the chip's total output level for any tone frame (single, DTMF, call progress), rms in PCM units.
pub const AMBE_PLUS_2_TONE_RMS: f64 = 24000.0;

const SAMPLE_RATE_HZ: f64 = 8000.0;
/// DTMF row frequencies (Hz), row 0-3.
pub const DTMF_ROW_HZ: [f64; 4] = [697.0, 770.0, 852.0, 941.0];
/// DTMF column frequencies (Hz), column 0-3.
pub const DTMF_COL_HZ: [f64; 4] = [1209.0, 1336.0, 1477.0, 1633.0];
/// North American call-progress tone pairs (Hz): dial tone, ringback, busy.
pub const CALL_DIAL_HZ: [f64; 2] = [350.0, 440.0];
pub const CALL_RING_HZ: [f64; 2] = [440.0, 480.0];
pub const CALL_BUSY_HZ: [f64; 2] = [480.0, 620.0];

/// Two phase accumulators (radians); a tone uses one or both.
pub struct ToneSynthesizer {
    phase: [f64; 2],
}

impl ToneSynthesizer {
    pub fn new() -> Self {
        Self { phase: [0.0; 2] }
    }

    /// Synthesizes one frame of the given 1 or 2 frequencies, each at `peak` amplitude.
    pub fn synthesize(&mut self, freqs_hz: &[f64], peak: f64) -> [f64; N] {
        let mut out = [0.0; N];
        for (slot, &hz) in freqs_hz.iter().take(2).enumerate() {
            let step = 2.0 * PI * hz / SAMPLE_RATE_HZ;
            let mut phase = self.phase[slot];
            for sample in out.iter_mut() {
                *sample += peak * phase.sin();
                phase += step;
            }
            self.phase[slot] = phase.rem_euclid(2.0 * PI);
        }
        out
    }

    /// Resets phase (call when a non-tone frame intervenes so the next tone starts cleanly).
    pub fn reset(&mut self) {
        self.phase = [0.0; 2];
    }

    pub fn dtmf(&mut self, row: u8, col: u8, peak: f64) -> [f64; N] {
        self.synthesize(
            &[
                DTMF_ROW_HZ[(row & 3) as usize],
                DTMF_COL_HZ[(col & 3) as usize],
            ],
            peak,
        )
    }
}

impl Default for ToneSynthesizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dstar_tone_level_saturates_at_full_scale() {
        assert!(dstar_tone_amplitude(230) < FULL_SCALE_PEAK);
        assert_eq!(dstar_tone_amplitude(255), FULL_SCALE_PEAK);
    }

    #[test]
    fn saturate_to_i16_rounds_and_clamps() {
        assert_eq!(saturate_to_i16(1.4), 1);
        assert_eq!(saturate_to_i16(-1.6), -2);
        assert_eq!(saturate_to_i16(240_000.0), i16::MAX);
        assert_eq!(saturate_to_i16(-240_000.0), i16::MIN);
        assert_eq!(saturate_to_i16(f64::NAN), 0);
    }

    fn dominant_hz(frame: &[f64; N]) -> f64 {
        // Goertzel-free brute force over 50..3500 Hz in 5 Hz steps.
        let mut best = (0.0, 0.0);
        let mut hz = 50.0;
        while hz < 3500.0 {
            let (mut re, mut im) = (0.0, 0.0);
            for (n, &s) in frame.iter().enumerate() {
                let a = 2.0 * PI * hz * n as f64 / SAMPLE_RATE_HZ;
                re += s * a.cos();
                im += s * a.sin();
            }
            let p = re * re + im * im;
            if p > best.1 {
                best = (hz, p);
            }
            hz += 5.0;
        }
        best.0
    }

    #[test]
    fn single_tone_lands_on_its_frequency_with_requested_peak() {
        let mut t = ToneSynthesizer::new();
        let frame = t.synthesize(&[1000.0], 1000.0);
        assert!((dominant_hz(&frame) - 1000.0).abs() <= 10.0);
        let peak = frame.iter().fold(0.0_f64, |m, &s| m.max(s.abs()));
        assert!((peak - 1000.0).abs() < 60.0, "peak {peak}");
    }

    #[test]
    fn phase_is_continuous_across_frames() {
        let mut t = ToneSynthesizer::new();
        let a = t.synthesize(&[500.0], 1000.0);
        let b = t.synthesize(&[500.0], 1000.0);
        let mut whole = ToneSynthesizer::new();
        let step = 2.0 * PI * 500.0 / SAMPLE_RATE_HZ;
        let expected_first_of_b = 1000.0 * (step * N as f64).sin();
        assert!((b[0] - expected_first_of_b).abs() < 1e-6);
        let _ = (a, &mut whole);
    }

    #[test]
    fn dtmf_digit_5_contains_770_and_1336() {
        let mut t = ToneSynthesizer::new();
        let frame = t.dtmf(1, 1, 1000.0);
        let power_at = |hz: f64| {
            let (mut re, mut im) = (0.0, 0.0);
            for (n, &s) in frame.iter().enumerate() {
                let a = 2.0 * PI * hz * n as f64 / SAMPLE_RATE_HZ;
                re += s * a.cos();
                im += s * a.sin();
            }
            re * re + im * im
        };
        assert!(power_at(770.0) > 20.0 * power_at(1000.0));
        assert!(power_at(1336.0) > 20.0 * power_at(1000.0));
    }

    #[test]
    fn dstar_tone_level_curve_matches_the_chip_measurements_and_inverts() {
        // Chip peaks measured at volumes 120/150/180/210: 283, 961, 3268, 11105.
        for (v, peak) in [
            (120u32, 283.0f64),
            (150, 961.0),
            (180, 3268.0),
            (210, 11105.0),
        ] {
            let a = dstar_tone_amplitude(v);
            assert!(
                (a / peak - 1.0).abs() < 0.04,
                "volume {v}: model {a} vs chip {peak}"
            );
        }
        for v in 60u32..=230 {
            assert_eq!(dstar_tone_volume_for_amplitude(dstar_tone_amplitude(v)), v);
        }
    }
}
