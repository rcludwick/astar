// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`crate::ambe::float::mbe_synthesis`]: the mode-independent MBE frame
//! synthesis D-STAR and AMBE+2 half-rate share, turning one frame's already-dequantized
//! `(w0, per-harmonic voicing, Ml)` into 20 ms of Q16.16 PCM by reusing
//! [`crate::ambe::fixed::tia_102_baba::synthesis::SynthesisState`] (enhancement, smoothing, voiced and
//! unvoiced synthesis). See the float sibling for the mbelib background and the two deliberate
//! differences from mbelib; FEC statistics feed the same smoothing thresholds with the two Golay
//! blocks these modes carry (`epsilon_c0`, `epsilon_c1`) and zero for the vectors they lack.

use super::unvoiced_synthesis::N;
use crate::ambe::fixed::tia_102_baba::error_estimation::estimate_errors_q16;
use crate::ambe::fixed::tia_102_baba::synthesis::SynthesisState;

/// `round(0.10 * 65536)`: the output's high-frequency lift weight, `float::mbe_synthesis::HIGH_LIFT_WEIGHT`.
const HIGH_LIFT_WEIGHT_Q16_16: i64 = 6554;

/// `round(0.87 * 65536)`: the unvoiced gain, `float::mbe_synthesis::UNVOICED_GAIN`.
const UNVOICED_GAIN_Q16_16: i64 = 57016;

pub struct MbeSynthesizer {
    synthesis: SynthesisState,
    error_rate_prev_q16: i32,
    lift_history: [i64; 2],
}

impl MbeSynthesizer {
    pub fn new() -> Self {
        let mut synthesis = SynthesisState::new();
        synthesis.set_unvoiced_gain_q16(UNVOICED_GAIN_Q16_16);
        Self {
            synthesis,
            error_rate_prev_q16: 0,
            lift_history: [0; 2],
        }
    }

    fn lift(&mut self, frame: [i64; N]) -> [i64; N] {
        let mut out = frame;
        for (o, &x) in out.iter_mut().zip(frame.iter()) {
            let second_difference = x - 2 * self.lift_history[0] + self.lift_history[1];
            *o =
                x.saturating_add((second_difference.saturating_mul(HIGH_LIFT_WEIGHT_Q16_16)) >> 16);
            self.lift_history = [x, self.lift_history[0]];
        }
        out
    }

    /// Synthesizes one speech frame. `voiced` and `ml_q16` are both 1-indexed by harmonic (index 0
    /// is unused padding), exactly as `SpeechParameters` carries them. Returns `None` on a length
    /// mismatch.
    pub fn synthesize_speech(
        &mut self,
        w0_q32: i64,
        voiced: &[bool],
        ml_q16: &[i32],
        epsilon_c0: u32,
        epsilon_c1: u32,
    ) -> Option<[i64; N]> {
        if voiced.len() != ml_q16.len() || voiced.len() < 2 {
            return None;
        }
        let errors = estimate_errors_q16(
            &[epsilon_c0, epsilon_c1, 0, 0, 0, 0, 0],
            self.error_rate_prev_q16,
        );
        self.error_rate_prev_q16 = errors.rate_q16;
        let frame = self
            .synthesis
            .synthesize_frame(&ml_q16[1..], w0_q32, &voiced[1..], &errors)?;
        Some(self.lift(frame))
    }

    /// Repeats the previous frame's parameters (an erasure); `None` before any real frame has run.
    pub fn synthesize_repeat(&mut self) -> Option<[i64; N]> {
        let frame = self.synthesis.synthesize_repeated_frame()?;
        Some(self.lift(frame))
    }

    /// Repeats the previous frame with its amplitudes scaled by `scale_q16` (fading a run of damaged frames).
    pub fn synthesize_repeat_scaled(&mut self, scale_q16: i32) -> Option<[i64; N]> {
        let frame = self.synthesis.synthesize_repeated_frame_scaled(scale_q16)?;
        Some(self.lift(frame))
    }

    /// A silence frame: all zeros.
    pub fn synthesize_silence(&self) -> [i64; N] {
        [0; N]
    }
}

impl Default for MbeSynthesizer {
    fn default() -> Self {
        Self::new()
    }
}
