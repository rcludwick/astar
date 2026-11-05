// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`crate::ambe::float::tone_synthesis`]: turns a decoded D-STAR / AMBE+2
//! half-rate tone frame (single tone, DTMF digit, call-progress tone) into 20 ms of PCM
//! sinusoids, with the phase carried across frames so a held tone is continuous.
//!
//! **Conventions**: a tone's phase accumulator is a wrapping `u32` in turns (`1u32 << 32` is a full
//! cycle, see [`super::trig`]), so the modulo-2*pi reduction the float sibling does with
//! `rem_euclid` is the free wraparound. Frequencies are Q16.16 hertz (`i32`); the DTMF and
//! call-progress tables are plain integer hertz. Peak levels are `i64` Q16.16 in 16-bit PCM units
//! (a full D-STAR volume of 255 gives a peak near 70000, and an AMBE+2 single tone peaks at
//! `24000*sqrt(2)`, neither of which fits an `i32` Q16.16), and the output is `[i64; N]` Q16.16 like
//! [`crate::ambe::fixed::tia_102_baba::synthesis::SynthesisState`].
//!
//! **Accuracy against the float sibling** (`tests/ambe_fixed_dstar_synthesis.rs`,
//! `tests/ambe_fixed_ambe_plus_2_synthesis.rs`): the sine comes from the 256-entry interpolated
//! quarter-wave table (error about 2e-5 of the peak) and the per-sample phase step is rounded to one
//! part in `2^32` of a turn, so a tone frame agrees with the float output at better than 80 dB SNR
//! and stays that close over long runs (the phase error grows at most a few `1e-6` of a turn per
//! second).
//!
//! **Levels** (measured against the live chip, see the float sibling): D-STAR volume sets each
//! tone's amplitude exponentially ([`dstar_tone_amplitude_q16`]); AMBE+2 tone frames always come out
//! at 24000 rms in total ([`AMBE_PLUS_2_TONE_RMS`]).

use super::explog::{exp2_q16, log2_q16_i64};
use super::isqrt::isqrt_u64;
use super::trig::sin_q16;
use super::unvoiced_synthesis::N;

/// Peak amplitude (16-bit PCM units) used when a tone frame carries no usable level.
pub const DEFAULT_TONE_PEAK: i64 = 4000;

/// `round(0.04084 * log2(e) * 2^32)`: the D-STAR tone level slope (base-e exponent 0.04084 per
/// volume step) converted to a base-2 exponent, in Q32 so a 75-step span keeps its precision.
const DSTAR_LOG2_SLOPE_Q32: i64 = 253_058_036;

/// D-STAR: each tone's peak amplitude in Q16.16 16-bit PCM units for a tone frame's 8-bit
/// `volume`: `3268 * exp(0.04084 * (volume - 180))`, the chip's measured curve (see the float
/// sibling's `dstar_tone_amplitude`), evaluated as `3268 * 2^y` with the integer part of `y`
/// applied as a shift so the very large and very small ends do not saturate `exp2_q16`.
pub fn dstar_tone_amplitude_q16(volume: u32) -> i64 {
    let y_q32 = (volume as i64 - 180) * DSTAR_LOG2_SLOPE_Q32;
    let y_q16 = (y_q32 + (1 << 15)) >> 16;
    let n = y_q16 >> 16; // floor
    let frac = (y_q16 & 0xFFFF) as i32;
    let base = 3268i64 * exp2_q16(frac) as i64; // Q16.16, in [3268, 6536)
    let amplitude = if n >= 0 {
        base << n
    } else {
        (base + (1i64 << (-n - 1))) >> -n
    };
    // Clamped to the 16-bit full-scale peak, like the float sibling's `FULL_SCALE_PEAK` (the chip saturates near volume 238).
    amplitude.min(32767 << 16)
}

/// The inverse of [`dstar_tone_amplitude_q16`]: the `volume` field for a desired per-tone peak amplitude (Q16.16 PCM
/// units), `round(180 + log2(A / 3268) / DSTAR_LOG2_SLOPE)` clamped to `0..=255`. Port of the float
/// `dstar_tone_volume_for_amplitude` (the amplitude is floored at 1, like the float).
pub fn dstar_tone_volume_for_amplitude_q16(amplitude_q16: i64) -> u32 {
    let lg = log2_q16_i64(amplitude_q16.max(1 << 16)) as i64 - log2_q16_i64(3268i64 << 16) as i64;
    let steps_q16 = (lg << 32) / DSTAR_LOG2_SLOPE_Q32; // log2 ratio (Q16) / slope (Q32) -> volume steps, Q16
    ((180 * 65536 + steps_q16 + 32768) >> 16).clamp(0, 255) as u32
}

/// AMBE+2 half-rate: the chip's total output level for any tone frame, rms in PCM units.
pub const AMBE_PLUS_2_TONE_RMS: i64 = 24000;

/// Each tone's peak (Q16.16) for an AMBE+2 dual tone: `24000` per tone.
pub fn ambe_plus_2_dual_tone_peak_q16() -> i64 {
    AMBE_PLUS_2_TONE_RMS << 16
}

/// The peak (Q16.16) for an AMBE+2 single tone: `24000 * sqrt(2)`.
pub fn ambe_plus_2_single_tone_peak_q16() -> i64 {
    let rms_q16 = (AMBE_PLUS_2_TONE_RMS << 16) as u64;
    isqrt_u64(2 * rms_q16 * rms_q16) as i64
}

const SAMPLE_RATE_HZ: u64 = 8000;
/// DTMF row frequencies (Hz), row 0-3.
pub const DTMF_ROW_HZ: [u32; 4] = [697, 770, 852, 941];
/// DTMF column frequencies (Hz), column 0-3.
pub const DTMF_COL_HZ: [u32; 4] = [1209, 1336, 1477, 1633];
/// North American call-progress tone pairs (Hz): dial tone, ringback, busy.
pub const CALL_DIAL_HZ: [u32; 2] = [350, 440];
pub const CALL_RING_HZ: [u32; 2] = [440, 480];
pub const CALL_BUSY_HZ: [u32; 2] = [480, 620];

/// The per-sample phase step, in turns of `1u32 << 32`, for a Q16.16 frequency in hertz:
/// `hz / 8000 * 2^32`, rounded to nearest.
fn phase_step(hz_q16: i32) -> u32 {
    let numerator = (hz_q16.max(0) as u64) << 16;
    ((numerator + SAMPLE_RATE_HZ / 2) / SAMPLE_RATE_HZ) as u32
}

/// Two phase accumulators (turns); a tone uses one or both.
pub struct ToneSynthesizer {
    phase: [u32; 2],
}

impl ToneSynthesizer {
    pub fn new() -> Self {
        Self { phase: [0; 2] }
    }

    /// Synthesizes one frame of the given 1 or 2 frequencies (Q16.16 Hz), each at `peak_q16`
    /// amplitude (Q16.16 PCM units). Output samples are Q16.16.
    pub fn synthesize(&mut self, freqs_hz_q16: &[i32], peak_q16: i64) -> [i64; N] {
        let mut out = [0i64; N];
        for (slot, &hz_q16) in freqs_hz_q16.iter().take(2).enumerate() {
            let step = phase_step(hz_q16);
            let mut phase = self.phase[slot];
            for sample in out.iter_mut() {
                let product = peak_q16 as i128 * sin_q16(phase) as i128;
                *sample += ((product + (1i128 << 15)) >> 16) as i64;
                phase = phase.wrapping_add(step);
            }
            self.phase[slot] = phase;
        }
        out
    }

    /// [`Self::synthesize`] for whole-hertz frequencies (the DTMF and call-progress tables).
    pub fn synthesize_hz(&mut self, freqs_hz: &[u32], peak_q16: i64) -> [i64; N] {
        let q16: Vec<i32> = freqs_hz
            .iter()
            .take(2)
            .map(|&hz| (hz as i32) << 16)
            .collect();
        self.synthesize(&q16, peak_q16)
    }

    /// Resets phase (call when a non-tone frame intervenes so the next tone starts cleanly).
    pub fn reset(&mut self) {
        self.phase = [0; 2];
    }

    pub fn dtmf(&mut self, row: u8, col: u8, peak_q16: i64) -> [i64; N] {
        self.synthesize_hz(
            &[
                DTMF_ROW_HZ[(row & 3) as usize],
                DTMF_COL_HZ[(col & 3) as usize],
            ],
            peak_q16,
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
    fn dstar_levels_match_the_chip_measurements() {
        // Chip peaks measured at volumes 120/150/180/210: 283, 961, 3268, 11105.
        for (v, peak) in [(120u32, 283i64), (150, 961), (180, 3268), (210, 11105)] {
            let a = dstar_tone_amplitude_q16(v) >> 16;
            assert!(
                (a - peak).abs() * 25 < peak,
                "volume {v}: model {a} vs chip {peak}"
            );
        }
        assert_eq!(dstar_tone_amplitude_q16(180) >> 16, 3268);
    }

    #[test]
    fn dstar_tone_volume_inverts_the_amplitude_curve() {
        for v in 60u32..=230 {
            assert_eq!(
                dstar_tone_volume_for_amplitude_q16(dstar_tone_amplitude_q16(v)),
                v,
                "volume {v}"
            );
        }
        assert_eq!(
            dstar_tone_volume_for_amplitude_q16(0),
            dstar_tone_volume_for_amplitude_q16(1 << 16)
        );
        assert_eq!(dstar_tone_volume_for_amplitude_q16(i64::MAX >> 8), 255);
    }

    #[test]
    fn ambe_plus_2_levels() {
        assert_eq!(ambe_plus_2_dual_tone_peak_q16(), 24000 << 16);
        assert_eq!(ambe_plus_2_single_tone_peak_q16() >> 16, 33941);
    }

    #[test]
    fn single_tone_has_requested_peak_and_continuous_phase() {
        let mut t = ToneSynthesizer::new();
        let a = t.synthesize(&[500 << 16], 1000 << 16);
        let b = t.synthesize(&[500 << 16], 1000 << 16);
        let peak = a.iter().map(|s| s.abs()).max().unwrap() >> 16;
        assert!((peak - 1000).abs() < 60, "peak {peak}");
        // 500 Hz at 8 kHz repeats every 16 samples, so 160 samples later the frame repeats exactly.
        for i in 0..N {
            assert!((a[i] - b[i]).abs() < 200, "sample {i}");
        }
    }
}
