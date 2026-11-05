// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`super::super::float::tia_102_baba::synthesis`] -- top-level decoder-side
//! synthesis orchestration (TIA-102.BABA_2003.pdf sections 8-11 combined): wires
//! [`super::reconstruct::reconstruct_spectral_amplitudes`]'s own output through
//! [`super::enhancement`]'s three caller-applied steps (spectral amplitude enhancement, Eq. 113's
//! own V/UV forcing, and Eq. 116's own final amplitude smoothing scale) before handing the result to
//! [`super::super::general::unvoiced_synthesis`]/[`super::super::general::voiced_synthesis`] and
//! summing per Eq. 142. See the float sibling's own doc comment for why this composition needs its
//! own module rather than being left to whatever eventually calls the two synthesis halves directly.
//!
//! **The one genuinely new fixed-point decision this module makes, beyond porting each individual
//! equation**: the combined output type. `voiced_synthesis::VoicedState::synthesize`'s own doc
//! comment already establishes that a single harmonic's own `M_l` can realistically reach `~3600`
//! (`enhancement::adaptive_voicing_threshold_q16`'s own doc comment cites the same figure), and that
//! up to 56 such harmonics summing constructively at a pitch pulse peak can exceed `i32`'s Q16.16
//! range on their own -- which is why that module's own final PCM cast saturates. Both synthesis
//! halves already return independently-saturated `i32` values, but naively summing two saturated
//! `i32`s (`s_uv[i] + s_v[i]`, Eq. 142) in plain `i32` arithmetic risks exactly the same class of
//! silent-wraparound bug this crate just fixed once already, one level up. Since the float sibling's
//! own reference chip comparison is separately documented as running "several times louder than the
//! chip" (`examples/ambe_chip_pcm_vs_float_synthesis_ratet27.rs`'s own doc comment), real combined
//! energy plausibly needs more headroom than a single `i32` Q16.16 value can hold even after both
//! halves are already clipped individually. This module therefore keeps the sum in `i64` (ample
//! headroom over `2 * i32::MAX`) and returns `[i64; N]` rather than re-narrowing to `i32` -- exactly
//! matching the float sibling's own `[f64; N]`, which is likewise an internal representation, not
//! final 16-bit PCM. Any real final clipping to a wire format's own sample width belongs to whatever
//! not-yet-built stage actually serializes PCM for transmission, not to this orchestration layer.

use super::enhancement::{
    adaptive_voicing_threshold_q16, amplitude_smoothing_scale_q16, amplitude_sum_q16, energy_q16,
    enhance_spectral_amplitudes_q16, smooth_voicing_decision_q16, update_amplitude_threshold_q16,
    update_local_energy_q16,
};
use super::error_estimation::FrameErrorsQ16;
use crate::ambe::fixed::general::unvoiced_synthesis::{
    advance_noise, NoiseState, UnvoicedState, N,
};
use crate::ambe::fixed::general::voiced_synthesis::VoicedState;

/// `round(-5.0 * 65536)`.
const NEG_FIVE_Q16_16: i64 = -5i64 << 16;
/// `round(10.0 * 65536)`.
const TEN_Q16_16: i64 = 10i64 << 16;

/// Persistent decoder-side synthesis state -- the fixed-point equivalent of
/// [`super::super::float::tia_102_baba::synthesis::SynthesisState`]. `s_e`/`tau_m` are `i64` Q16.16,
/// matching [`update_local_energy_q16`]/[`update_amplitude_threshold_q16`]'s own domain (see those
/// functions' own doc comments for why `i32` isn't wide enough). `s_e = 75000.0`, `tau_m = 20480.0`
/// per Annex A / Eq. 115's own first-branch value, exactly as the float sibling's own doc comment
/// derives.
pub struct SynthesisState {
    noise: NoiseState,
    unvoiced: UnvoicedState,
    voiced: VoicedState,
    s_e: i64,
    tau_m: i64,
    first_frame: bool,
    last_final_amplitudes: Option<(i64, Vec<bool>, Vec<i32>)>,
    comfort_noise_seed: i64,
    /// Q16.16 multiplier on the unvoiced half; 65536 (exactly 1.0, applied as a no-op) for the standard.
    unvoiced_gain_q16: i64,
}

impl SynthesisState {
    pub fn new() -> Self {
        Self {
            noise: NoiseState::new(),
            unvoiced: UnvoicedState::new(),
            voiced: VoicedState::new(),
            s_e: 75000i64 << 16,
            tau_m: 20480i64 << 16,
            first_frame: true,
            last_final_amplitudes: None,
            comfort_noise_seed: 3147,
            unvoiced_gain_q16: 1 << 16,
        }
    }

    pub fn set_unvoiced_gain_q16(&mut self, gain_q16: i64) {
        self.unvoiced_gain_q16 = gain_q16;
    }

    /// Section 7.8 (Frame Muting): `s~(n)` set to random noise uniformly distributed over
    /// `[-5, 5]` -- see the float sibling's own doc comment for the full spec citation and the
    /// separate-seed rationale (`comfort_noise_seed`, not `noise`, so a muted frame's own 160 draws
    /// don't desynchronize the shared window real unvoiced/voiced synthesis depends on). Each raw
    /// `advance_noise` draw (uniform over `0..53125`) is rescaled to Q16.16 via one exact integer
    /// ratio (`-5 + 10*seed/53125`, rounded to nearest) rather than truncated. The result still lands
    /// in `[-5.0, 5.0)`, the same half-open interval the float sibling's own `[-5.0, 5.0)` uses: the
    /// lowest raw draw (`seed = 0`) rescales to exactly `-5.0`, and the highest (`seed = 53124`)
    /// rescales to just under `5.0`, never reaching it.
    pub fn synthesize_comfort_frame(&mut self) -> [i64; N] {
        std::array::from_fn(|_| {
            self.comfort_noise_seed = advance_noise(self.comfort_noise_seed);
            let numerator = TEN_Q16_16 * self.comfort_noise_seed;
            let half = 53125 / 2;
            (numerator + half) / 53125 + NEG_FIVE_Q16_16
        })
    }

    /// Section 8-9 (Eq. 105-116): the fixed-point equivalent of the float sibling's own
    /// `finalize_parameters`. Returns `None` on a length mismatch.
    fn finalize_parameters(
        &mut self,
        reconstructed_amplitudes_q16: &[i32],
        omega0_tilde_q32: i64,
        decoded_voiced: &[bool],
        errors: &FrameErrorsQ16,
    ) -> Option<(Vec<bool>, Vec<i32>)> {
        if reconstructed_amplitudes_q16.len() != decoded_voiced.len() {
            return None;
        }

        // Section 8: spectral amplitude enhancement (Eq. 105-110).
        let r_m0 = energy_q16(reconstructed_amplitudes_q16);
        let enhanced =
            enhance_spectral_amplitudes_q16(reconstructed_amplitudes_q16, omega0_tilde_q32);

        // Section 9: V/UV smoothing (Eq. 111-113) -- forcing uses the *enhanced*, not-yet-gamma_M-
        // scaled amplitude, per enhancement::smooth_voicing_decision_q16's own established contract.
        self.s_e = update_local_energy_q16(self.s_e, r_m0);
        let v_m_q16 = adaptive_voicing_threshold_q16(errors, self.s_e);
        let smoothed_voiced: Vec<bool> = enhanced
            .iter()
            .zip(decoded_voiced)
            .map(|(&m, &v)| smooth_voicing_decision_q16(m, v, v_m_q16))
            .collect();

        // Section 9: amplitude smoothing (Eq. 114-116) -- A_M is the sum of the enhanced amplitudes
        // *before* gamma_M is applied to them; gamma_M is then the last step before synthesis.
        let a_m = amplitude_sum_q16(&enhanced);
        self.tau_m = update_amplitude_threshold_q16(errors, self.tau_m);
        let gamma_m_q16 = amplitude_smoothing_scale_q16(self.tau_m, a_m);
        let final_amplitudes: Vec<i32> = enhanced
            .iter()
            .map(|&m| crate::ambe::fixed::general::fixed_ops::mul_q16(m, gamma_m_q16))
            .collect();

        self.last_final_amplitudes = Some((
            omega0_tilde_q32,
            smoothed_voiced.clone(),
            final_amplitudes.clone(),
        ));
        Some((smoothed_voiced, final_amplitudes))
    }

    /// Section 11 (Eq. 117-142): advances the shared noise generator exactly once per frame, then
    /// synthesizes and sums both halves -- see this module's own doc comment for why the sum is kept
    /// in `i64` rather than re-narrowed to `i32`.
    fn synthesize_core(
        &mut self,
        omega0_tilde_q32: i64,
        voiced: &[bool],
        final_amplitudes_q16: &[i32],
    ) -> Option<[i64; N]> {
        if !self.first_frame {
            self.noise.advance_frame();
        }
        self.first_frame = false;

        let s_uv = self.unvoiced.synthesize(
            &self.noise,
            omega0_tilde_q32,
            voiced,
            final_amplitudes_q16,
        )?;
        let s_v =
            self.voiced
                .synthesize(&self.noise, omega0_tilde_q32, voiced, final_amplitudes_q16)?;

        let mut s = [0i64; N];
        for i in 0..N {
            let uv = if self.unvoiced_gain_q16 == 1 << 16 {
                s_uv[i] as i64
            } else {
                (s_uv[i] as i64 * self.unvoiced_gain_q16 + (1 << 15)) >> 16
            };
            s[i] = uv + s_v[i] as i64; // Eq. 142.
        }
        Some(s)
    }

    /// Synthesizes one 20 ms PCM frame (Eq. 142) from this frame's own *unenhanced* reconstructed
    /// spectral amplitudes, fundamental frequency, decoded V/UV decisions, and FEC error statistics.
    /// Returns `None` on a length mismatch between `reconstructed_amplitudes_q16` and `decoded_voiced`.
    pub fn synthesize_frame(
        &mut self,
        reconstructed_amplitudes_q16: &[i32],
        omega0_tilde_q32: i64,
        decoded_voiced: &[bool],
        errors: &FrameErrorsQ16,
    ) -> Option<[i64; N]> {
        let (voiced, final_amplitudes) = self.finalize_parameters(
            reconstructed_amplitudes_q16,
            omega0_tilde_q32,
            decoded_voiced,
            errors,
        )?;
        self.synthesize_core(omega0_tilde_q32, &voiced, &final_amplitudes)
    }

    /// Synthesizes a *repeated* frame (section 7.7, Eq. 99-104) -- see the float sibling's own doc
    /// comment for the full rationale (`M_bar_l(0) = M_bar_l(-1)`, skipping enhancement and its own
    /// `S_E`/`tau_M` state updates entirely). Returns `None` if no real frame has run yet.
    pub fn synthesize_repeated_frame(&mut self) -> Option<[i64; N]> {
        let (omega0_tilde_q32, voiced, final_amplitudes) = self.last_final_amplitudes.clone()?;
        self.synthesize_core(omega0_tilde_q32, &voiced, &final_amplitudes)
    }

    /// A repeated frame with every amplitude multiplied by `scale_q16` (a fade); the stored last frame stays untouched.
    pub fn synthesize_repeated_frame_scaled(&mut self, scale_q16: i32) -> Option<[i64; N]> {
        let (omega0_tilde_q32, voiced, final_amplitudes) = self.last_final_amplitudes.clone()?;
        let scaled: Vec<i32> = final_amplitudes
            .iter()
            .map(|&a| ((a as i64 * scale_q16 as i64) >> 16) as i32)
            .collect();
        self.synthesize_core(omega0_tilde_q32, &voiced, &scaled)
    }
}

impl Default for SynthesisState {
    fn default() -> Self {
        Self::new()
    }
}
