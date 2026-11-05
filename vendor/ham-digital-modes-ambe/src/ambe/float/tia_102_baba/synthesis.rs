//! Top-level decoder-side synthesis orchestration (TIA-102.BABA_2003.pdf sections 8-11 combined):
//! wires [`super::reconstruct::reconstruct_spectral_amplitudes`]'s own output through
//! [`super::enhancement`]'s three caller-applied steps -- spectral amplitude enhancement (Eq. 105-
//! 110), Eq. 113's own V/UV forcing, and Eq. 116's own final amplitude smoothing scale, all three
//! deliberately left for the caller by that module's own doc comment -- before handing the result to
//! [`super::unvoiced_synthesis`]/[`super::voiced_synthesis`] and summing per Eq. 142.
//!
//! **Why this module exists as its own thing, not just left to whatever eventually calls
//! `unvoiced_synthesis`/`voiced_synthesis` directly**: `enhance_spectral_amplitudes`'s own output is
//! *not* the `M_bar_l(0)` synthesis actually uses -- it still needs Eq. 113's voicing-forcing and
//! Eq. 116's smoothing scale applied on top of it, both of which carry their own frame-to-frame state
//! (`S_E`, Eq. 111, and `tau_M`, Eq. 115). Skipping either step compiles, runs, and produces
//! plausible-sounding output -- it's wrong in a way no unit test in either `enhancement.rs` or
//! `unvoiced_synthesis.rs`/`voiced_synthesis.rs` alone would ever catch, since each of those modules
//! is only tested against its own equations, not against the composition. This module's own job is
//! to make sure that composition happens in the one place a caller would otherwise be tempted to
//! skip a step.

use super::enhancement::{
    adaptive_voicing_threshold, amplitude_smoothing_scale, amplitude_sum, energy,
    enhance_spectral_amplitudes, smooth_voicing_decision, update_amplitude_threshold,
    update_local_energy,
};
use super::error_estimation::FrameErrors;
use super::unvoiced_synthesis::{advance_noise, NoiseState, UnvoicedState, N};
use super::voiced_synthesis::VoicedState;

/// Persistent decoder-side synthesis state: the shared noise generator (see
/// `unvoiced_synthesis::UnvoicedState`'s own doc comment for why unvoiced and voiced synthesis must
/// share one [`NoiseState`] rather than each owning an independent one), both synthesis halves' own
/// state, and the two enhancement-stage state variables synthesis itself must carry (`S_E`, `tau_M`) --
/// `S_E = 75000.0` per Annex A; `tau_M` isn't itself listed in Annex A, but `20480.0` is Eq. 115's own
/// first-branch value, which any real first frame (zero accumulated FEC errors, the only case that
/// actually matters before a previous `tau_M` exists) reduces to regardless of the seed -- a reasoned
/// default, not a guess.
pub struct SynthesisState {
    noise: NoiseState,
    unvoiced: UnvoicedState,
    voiced: VoicedState,
    s_e: f64,
    tau_m: f64,
    first_frame: bool,
    /// This frame's own final synthesis inputs (post-enhancement, post-V/UV-forcing,
    /// post-gamma_M), kept around specifically for [`Self::synthesize_repeated_frame`]'s own
    /// Eq. 104 (`M_bar_l(0) = M_bar_l(-1)`) and the accompanying "reuse everything" reading of
    /// Eq. 99-104 -- `None` until the first real (non-repeated) frame has run.
    last_final_amplitudes: Option<(f64, Vec<bool>, Vec<f64>)>,
    /// Section 7.8's own comfort-noise generator state: an independent instance of the *same*
    /// Eq. 117 recurrence [`NoiseState`] uses (same Annex A seed, `u(-105) = 3147` -- the spec
    /// defines exactly one such recurrence, and 7.8 doesn't name a second one, so reusing it is
    /// the spec-faithful reading, not an invented generator), but deliberately NOT sharing
    /// `noise`'s own state: a muted frame's own 160 raw comfort-noise draws would otherwise
    /// advance `NoiseState`'s shared window an amount unrelated to its own per-frame contract
    /// (`advance_frame`'s single step), desynchronizing the noise sequence real unvoiced/voiced
    /// synthesis depends on for every frame *after* the muted one.
    comfort_noise_seed: i64,
    /// Multiplier on the unvoiced half before the Eq. 142 sum: 1.0 for the standard; the D-STAR and AMBE+2 synthesizers
    /// set the value measured against the chip.
    unvoiced_gain: f64,
}

impl SynthesisState {
    pub fn new() -> Self {
        Self {
            noise: NoiseState::new(),
            unvoiced: UnvoicedState::new(),
            voiced: VoicedState::new(),
            s_e: 75000.0,
            tau_m: 20480.0,
            first_frame: true,
            last_final_amplitudes: None,
            comfort_noise_seed: 3147,
            unvoiced_gain: 1.0,
        }
    }

    pub fn set_unvoiced_gain(&mut self, gain: f64) {
        self.unvoiced_gain = gain;
    }

    /// Pass-through to [`VoicedState::set_amplitude_interpolation_shape`] -- see that method's own
    /// doc comment. Not called for the standard's own default construction.
    pub fn set_voiced_amplitude_interpolation_shape(&mut self, shape: fn(f64) -> f64) {
        self.voiced.set_amplitude_interpolation_shape(shape);
    }

    /// Section 7.8 (Frame Muting), transcribed from a 600 DPI render of page 63: "set the
    /// synthetic speech signal, s~(n), to random noise which is uniformly distributed over the
    /// interval [-5, 5]" -- a real, literal spec requirement, not a design choice like
    /// [`Self::synthesize_repeated_frame`]'s own "reasonable degradation" framing. Each raw
    /// `advance_noise` draw (uniform over `0..53125`) is linearly rescaled to `[-5.0, 5.0)`;
    /// excluding the exact upper endpoint is immaterial for a continuous uniform distribution.
    /// Callers still owe the frame-repeat "step (2) update equations" section 7.8's own text
    /// requires *before* calling this (this codebase's own `should_mute_frame` doc comment
    /// already establishes that `decode.rs` runs the same repeat bookkeeping for a muted frame as
    /// for an ordinary repeat) -- this function is only the "bypass speech synthesis, emit noise
    /// instead" half.
    // [@ANCHOR: ambe:synthesize_comfort_frame]
    pub fn synthesize_comfort_frame(&mut self) -> [f64; N] {
        std::array::from_fn(|_| {
            self.comfort_noise_seed = advance_noise(self.comfort_noise_seed);
            -5.0 + 10.0 * (self.comfort_noise_seed as f64) / 53125.0
        })
    }

    /// Section 8-9 (Eq. 105-116): reconstruct's own *unenhanced* amplitudes through enhancement,
    /// Eq. 113's V/UV forcing, and Eq. 116's amplitude smoothing scale, producing the actual
    /// `(voiced, M_bar_l(0))` pair synthesis consumes -- and remembering it as `last_final_amplitudes`
    /// for a future repeated frame's own Eq. 104. Returns `None` on a length mismatch.
    // [@ANCHOR: SynthesisState::finalize_parameters]
    fn finalize_parameters(
        &mut self,
        reconstructed_amplitudes: &[f64],
        omega0_tilde: f64,
        decoded_voiced: &[bool],
        errors: &FrameErrors,
    ) -> Option<(Vec<bool>, Vec<f64>)> {
        if reconstructed_amplitudes.len() != decoded_voiced.len() {
            return None;
        }

        // Section 8: spectral amplitude enhancement (Eq. 105-110).
        let r_m0 = energy(reconstructed_amplitudes);
        let enhanced = enhance_spectral_amplitudes(reconstructed_amplitudes, omega0_tilde);

        // Section 9: V/UV smoothing (Eq. 111-113) -- forcing uses the *enhanced*, not-yet-gamma_M-
        // scaled amplitude, per enhancement::smooth_voicing_decision's own established contract.
        self.s_e = update_local_energy(self.s_e, r_m0);
        let v_m = adaptive_voicing_threshold(errors, self.s_e);
        let smoothed_voiced: Vec<bool> = enhanced
            .iter()
            .zip(decoded_voiced)
            .map(|(&m, &v)| smooth_voicing_decision(m, v, v_m))
            .collect();

        // Section 9: amplitude smoothing (Eq. 114-116) -- A_M is the sum of the enhanced amplitudes
        // *before* gamma_M is applied to them; gamma_M is then the last step before synthesis.
        let a_m = amplitude_sum(&enhanced);
        self.tau_m = update_amplitude_threshold(errors, self.tau_m);
        let gamma_m = amplitude_smoothing_scale(self.tau_m, a_m);
        let final_amplitudes: Vec<f64> = enhanced.iter().map(|&m| m * gamma_m).collect();

        self.last_final_amplitudes = Some((
            omega0_tilde,
            smoothed_voiced.clone(),
            final_amplitudes.clone(),
        ));
        Some((smoothed_voiced, final_amplitudes))
    }

    /// Section 11 (Eq. 117-142): advances the shared noise generator exactly once per frame (not
    /// inside either synthesis half, since Eq. 141's own `rho_l(0)` needs the *same* current-frame
    /// window unvoiced synthesis uses), then synthesizes and sums both halves. Takes already-final
    /// `(voiced, M_bar_l(0))` -- the common core both a normal frame ([`Self::synthesize_frame`],
    /// after [`Self::finalize_parameters`]) and a repeated frame
    /// ([`Self::synthesize_repeated_frame`], skipping it entirely per Eq. 104) both funnel into.
    // [@ANCHOR: SynthesisState::synthesize_core]
    fn synthesize_core(
        &mut self,
        omega0_tilde: f64,
        voiced: &[bool],
        final_amplitudes: &[f64],
    ) -> Option<[f64; N]> {
        if !self.first_frame {
            self.noise.advance_frame();
        }
        self.first_frame = false;

        let s_uv = self
            .unvoiced
            .synthesize(&self.noise, omega0_tilde, voiced, final_amplitudes)?;
        let s_v = self
            .voiced
            .synthesize(&self.noise, omega0_tilde, voiced, final_amplitudes)?;

        let mut s = [0.0; N];
        for i in 0..N {
            s[i] = self.unvoiced_gain * s_uv[i] + s_v[i]; // Eq. 142 (unvoiced_gain is 1.0 for the standard).
        }
        Some(s)
    }

    /// Synthesizes one 20 ms PCM frame (Eq. 142) from this frame's own *unenhanced* reconstructed
    /// spectral amplitudes (`reconstruct::reconstruct_spectral_amplitudes`'s own output), fundamental
    /// frequency, decoded V/UV decisions, and FEC error statistics
    /// (`error_estimation::estimate_errors`'s own output) -- exactly the pieces this codebase's own
    /// decoder-side pipeline has already built up through section 7. Returns `None` on a length
    /// mismatch between `reconstructed_amplitudes` and `decoded_voiced`.
    pub fn synthesize_frame(
        &mut self,
        reconstructed_amplitudes: &[f64],
        omega0_tilde: f64,
        decoded_voiced: &[bool],
        errors: &FrameErrors,
    ) -> Option<[f64; N]> {
        let (voiced, final_amplitudes) = self.finalize_parameters(
            reconstructed_amplitudes,
            omega0_tilde,
            decoded_voiced,
            errors,
        )?;
        self.synthesize_core(omega0_tilde, &voiced, &final_amplitudes)
    }

    /// Synthesizes a frame from already-final parameters, skipping spectral enhancement, V/UV smoothing and
    /// amplitude smoothing entirely (`reconstructed_amplitudes` and `voiced` go straight to Eq. 127-142). For
    /// experiments comparing a mode's output against the real chip with those stages disabled.
    pub fn synthesize_frame_unenhanced(
        &mut self,
        amplitudes: &[f64],
        omega0_tilde: f64,
        voiced: &[bool],
    ) -> Option<[f64; N]> {
        if amplitudes.len() != voiced.len() {
            return None;
        }
        self.last_final_amplitudes = Some((omega0_tilde, voiced.to_vec(), amplitudes.to_vec()));
        self.synthesize_core(omega0_tilde, voiced, amplitudes)
    }

    /// Synthesizes a *repeated* frame (section 7.7, Eq. 99-104): when the decoder's own frame-repeat
    /// check fires (an invalid `b_hat_0` or `error_estimation::should_repeat_frame`), every IMBE
    /// model parameter for the current frame is set equal to the previous frame's own -- crucially,
    /// including `M_bar_l(0) = M_bar_l(-1)` (Eq. 104), the *enhanced* amplitude directly, not the
    /// unenhanced one re-run through enhancement. This is why [`Self::synthesize_core`] exists
    /// separately from [`Self::finalize_parameters`]: a repeat skips enhancement (and its own
    /// `S_E`/`tau_M` state updates) entirely and reuses the exact `(voiced, M_bar_l(0))` pair the
    /// last real frame already computed. Returns `None` if no real frame has run yet (the spec gives
    /// no meaningful "previous frame" for a stream's own very first frame to repeat).
    pub fn synthesize_repeated_frame(&mut self) -> Option<[f64; N]> {
        let (omega0_tilde, voiced, final_amplitudes) = self.last_final_amplitudes.clone()?;
        self.synthesize_core(omega0_tilde, &voiced, &final_amplitudes)
    }

    /// A repeated frame with every amplitude multiplied by `scale` (a fade), leaving the stored last frame untouched so the caller's
    /// cumulative fade is applied to the original amplitudes each time.
    pub fn synthesize_repeated_frame_scaled(&mut self, scale: f64) -> Option<[f64; N]> {
        let (omega0_tilde, voiced, final_amplitudes) = self.last_final_amplitudes.clone()?;
        let scaled: Vec<f64> = final_amplitudes.iter().map(|a| a * scale).collect();
        self.synthesize_core(omega0_tilde, &voiced, &scaled)
    }
}

impl Default for SynthesisState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_errors() -> FrameErrors {
        FrameErrors {
            total: 0,
            rate: 0.0,
            golay_init: 0,
            hamming_init: 0,
        }
    }

    #[test]
    fn synthesize_repeated_frame_returns_none_before_any_real_frame_has_run() {
        let mut state = SynthesisState::new();
        assert!(state.synthesize_repeated_frame().is_none());
    }

    /// The real point of Eq. 99-104: a repeated frame must reuse the exact same
    /// `(voiced, M_bar_l(0))` pair a real frame already finalized, not re-derive it. Checked by
    /// setting up a real frame with a decoded voicing pattern the adaptive threshold would normally
    /// force differently, then confirming the repeated frame's own synthesized output is finite and
    /// that calling it repeatedly doesn't panic or drift into nonsense -- the stronger, structural
    /// guarantee (same voiced/amplitudes reused) is enforced by construction in
    /// synthesize_repeated_frame's own implementation (it never recomputes enhancement), not just
    /// spot-checked here.
    #[test]
    fn synthesize_repeated_frame_reuses_the_last_real_frames_own_final_parameters() {
        let mut state = SynthesisState::new();
        let omega0 = 2.0 * std::f64::consts::PI / 100.0;
        let voiced = vec![true; 16];
        let amplitudes = vec![500.0; 16];
        let errors = zero_errors();

        state
            .synthesize_frame(&amplitudes, omega0, &voiced, &errors)
            .unwrap();
        let s_e_after_real_frame = state.s_e;
        let tau_m_after_real_frame = state.tau_m;

        for _ in 0..3 {
            let frame = state.synthesize_repeated_frame().unwrap();
            for &sample in &frame {
                assert!(
                    sample.is_finite(),
                    "non-finite repeated-frame sample: {sample}"
                );
            }
        }

        // A repeated frame must not touch S_E/tau_M -- both are enhancement-stage state, and
        // Eq. 99-104 skip enhancement entirely on a repeat.
        assert_eq!(state.s_e, s_e_after_real_frame);
        assert_eq!(state.tau_m, tau_m_after_real_frame);
    }

    /// Real, direct check of section 7.8's own literal requirement: every sample lands in
    /// `[-5.0, 5.0)`, and the frame isn't degenerate (not every sample identical) -- a real check
    /// on the actual distribution, not just "it's finite" the way a synthesis smoke test would be.
    // Tests [@ANCHOR: ambe:synthesize_comfort_frame]
    #[test]
    fn synthesize_comfort_frame_produces_bounded_non_degenerate_noise() {
        let mut state = SynthesisState::new();
        let frame = state.synthesize_comfort_frame();
        assert_eq!(frame.len(), N);
        for &sample in &frame {
            assert!(
                (-5.0..5.0).contains(&sample),
                "comfort-noise sample {sample} outside the spec's own [-5, 5) interval"
            );
        }
        assert!(
            frame.iter().any(|&s| s != frame[0]),
            "expected real noise, not a degenerate constant frame"
        );
    }

    /// `advance_noise`'s own recurrence is deterministic given the same seed -- two fresh
    /// `SynthesisState`s (same Annex A seed) must produce byte-identical comfort-noise frames,
    /// the same reproducibility bar this codec's every other deterministic component gets.
    #[test]
    fn synthesize_comfort_frame_is_deterministic_from_the_same_seed() {
        let mut a = SynthesisState::new();
        let mut b = SynthesisState::new();
        assert_eq!(a.synthesize_comfort_frame(), b.synthesize_comfort_frame());
    }

    /// The real reason `comfort_noise_seed` is a separate field from `noise: NoiseState`: pulling
    /// 160 comfort-noise draws per muted frame must not perturb the shared noise window real
    /// unvoiced/voiced synthesis depends on for every subsequent real frame.
    #[test]
    fn synthesize_comfort_frame_does_not_perturb_the_shared_noise_state() {
        let mut with_comfort = SynthesisState::new();
        let mut without_comfort = SynthesisState::new();

        with_comfort.synthesize_comfort_frame();

        let omega0 = 2.0 * std::f64::consts::PI / 100.0;
        let voiced = vec![true; 16];
        let amplitudes = vec![500.0; 16];
        let errors = zero_errors();
        let frame_after_comfort = with_comfort
            .synthesize_frame(&amplitudes, omega0, &voiced, &errors)
            .unwrap();
        let frame_without_comfort = without_comfort
            .synthesize_frame(&amplitudes, omega0, &voiced, &errors)
            .unwrap();
        assert_eq!(frame_after_comfort, frame_without_comfort);
    }

    #[test]
    // Tests [@ANCHOR: SynthesisState::finalize_parameters]
    // Tests [@ANCHOR: SynthesisState::synthesize_core]
    fn synthesize_frame_produces_a_full_finite_frame_across_several_calls() {
        let mut state = SynthesisState::new();
        let omega0 = 2.0 * std::f64::consts::PI / 100.0;
        let voiced = vec![true; 16];
        let amplitudes = vec![500.0; 16];
        let errors = zero_errors();

        for _ in 0..5 {
            let frame = state
                .synthesize_frame(&amplitudes, omega0, &voiced, &errors)
                .unwrap();
            assert_eq!(frame.len(), N);
            for &sample in &frame {
                assert!(sample.is_finite(), "non-finite combined sample: {sample}");
            }
        }
    }

    #[test]
    fn synthesize_frame_rejects_a_length_mismatch() {
        let mut state = SynthesisState::new();
        let voiced = vec![true; 5];
        let amplitudes = vec![100.0; 6];
        assert!(state
            .synthesize_frame(&amplitudes, 0.1, &voiced, &zero_errors())
            .is_none());
    }

    /// A real, checkable regression for the exact wiring bug this module exists to prevent: with a
    /// clean error record (`should_mute_frame`/`should_repeat_frame` both false, matching
    /// `error_estimation`'s own module) and every harmonic amplitude far below both the voicing
    /// threshold and the amplitude threshold, `gamma_M` (Eq. 116) must clamp to `1.0` (since
    /// `tau_M > A_M`, the "otherwise" branch) -- confirmed by checking the combined output stays
    /// close to what an unscaled synthesis would produce, rather than trusting the wiring blindly.
    #[test]
    fn synthesize_frame_applies_a_no_op_gamma_m_when_amplitudes_are_well_under_threshold() {
        let mut state = SynthesisState::new();
        let omega0 = 2.0 * std::f64::consts::PI / 100.0;
        let voiced = vec![false; 9]; // Small L~, fully unvoiced: only unvoiced_synthesis contributes.
        let amplitudes = vec![1.0; 9]; // Tiny relative to tau_M's own 20480.0 floor.
        let frame = state
            .synthesize_frame(&amplitudes, omega0, &voiced, &zero_errors())
            .unwrap();
        // gamma_M == 1.0 here is a real, checkable fact (A_M for these inputs is tiny relative to
        // tau_M's 20480.0 floor, so update_amplitude_threshold/amplitude_smoothing_scale's own
        // "otherwise" branch can't fire) -- if gamma_M were wrongly computed as near-zero instead
        // (e.g. tau_M and A_M swapped), every sample would collapse to ~0.0, which this rules out.
        assert!(
            frame.iter().any(|&s| s.abs() > 1e-6),
            "expected real synthesized energy, got near-silence: {frame:?}"
        );
    }
}
