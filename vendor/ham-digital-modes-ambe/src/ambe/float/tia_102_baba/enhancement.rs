//! Spectral amplitude enhancement and adaptive smoothing (TIA-102.BABA_2003.pdf sections 8-9,
//! Eq. 105-116) -- decoder-side quality improvements applied to reconstruction's own output
//! ([`super::reconstruct::reconstruct_spectral_amplitudes`]) before speech synthesis. These enhanced
//! amplitudes are deliberately a dead end for this codec's own state, not fed back into anything:
//! section 8's own text states plainly "the unenhanced spectral amplitudes are required by future
//! frames in the computation of Equation (77) [-- `prediction::reconstruct_log2_amplitude`],
//! however, the enhanced spectral amplitudes are used in speech synthesis" -- i.e. `FrameState`'s own
//! `spectral_amplitudes` field (feeding the next frame's prediction) must stay unenhanced, and this
//! module's own output is a separate, one-way branch toward section 11's eventual synthesis work.
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 63-66 (self-numbered pages
//! 47-50), following the same discipline as every other equation in this spec. Every numeric
//! constant below was independently cross-checked against `kchmck/imbe.rs`'s own MIT-licensed
//! `enhance.rs` (an unrelated, working, third-party IMBE decoder, vendored at
//! `hams_com/reference/ambe/imbe.rs/` for future reference) and matches exactly -- including the two
//! easily-conflated, similar-looking thresholds Eq. 112 (`V_M`'s own `epsilon_T <= 4` branch) and
//! Eq. 115 (`tau_M`'s own `epsilon_T <= 6` branch), which really are two different numbers for two
//! different variables, not a transcription slip -- each confirmed independently at 600 DPI before
//! either was trusted.

use std::f64::consts::PI;

use super::error_estimation::FrameErrors;

/// `R_M0` (Eq. 105): the current frame's own spectral-amplitude energy.
pub fn energy(spectral_amplitudes: &[f64]) -> f64 {
    spectral_amplitudes.iter().map(|m| m * m).sum()
}

/// `R_M1` (Eq. 106): the same energy, weighted by each harmonic's own phase term -- a measure of how
/// concentrated the spectral envelope's own energy is at low frequency.
pub fn scaled_energy(spectral_amplitudes: &[f64], omega0_hat: f64) -> f64 {
    spectral_amplitudes
        .iter()
        .enumerate()
        .map(|(idx, &m)| {
            let l = (idx + 1) as f64;
            m * m * (omega0_hat * l).cos()
        })
        .sum()
}

/// `W_l` (Eq. 107): the raw enhancement weight for harmonic `l` (1-indexed), before the Eq. 108
/// clamp/cutoff is applied. Callers must ensure `r_m0 != 0.0` (this module's own
/// [`enhance_spectral_amplitudes`] guarantees that by returning early on a silent frame before ever
/// calling this function). The denominator's own `r_m0^2 - r_m1^2` factor is otherwise never zero in
/// practice even though `|r_m1| <= r_m0` always holds (Cauchy-Schwarz, since `r_m1` is `r_m0`'s own
/// sum re-weighted by `cos(omega0*l) in [-1, 1]`): hitting zero would require `|r_m1|` to equal `r_m0`
/// at exact float precision, which needs every harmonic's own phase term to align perfectly -- not
/// reachable by any real input this codec produces.
// [@ANCHOR: weight]
fn weight(m_l: f64, l: f64, omega0_hat: f64, r_m0: f64, r_m1: f64) -> f64 {
    let numerator =
        0.96 * PI * (r_m0 * r_m0 + r_m1 * r_m1 - 2.0 * r_m0 * r_m1 * (omega0_hat * l).cos());
    let denominator = omega0_hat * r_m0 * (r_m0 * r_m0 - r_m1 * r_m1);
    m_l.sqrt() * (numerator / denominator).powf(0.25)
}

/// The full spectral amplitude enhancement pipeline (Eq. 105-110): computes `R_M0`/`R_M1`, applies
/// Eq. 108's per-harmonic weighting (harmonics with `8*l <= L_hat` pass through unweighted; the rest
/// are scaled by [`weight`], clamped to `[0.5, 1.2]`), then rescales the whole result (Eq. 109-110)
/// so its own total energy exactly matches the unenhanced input's `R_M0` -- a real, checkable
/// invariant (see the test below), not just a plausible-sounding normalization.
// [@ANCHOR: enhance_spectral_amplitudes]
pub fn enhance_spectral_amplitudes(spectral_amplitudes: &[f64], omega0_hat: f64) -> Vec<f64> {
    let l_hat = spectral_amplitudes.len() as u32;
    let r_m0 = energy(spectral_amplitudes);

    // A silent frame (all spectral amplitudes zero, so R_M0 == 0) has nothing to enhance: Eq. 107's
    // own weight() divides by R_M0 in its denominator, and Eq. 109's own rescale divides by the
    // enhanced energy, so continuing on into either would produce a 0.0/0.0 == NaN, not merely a
    // *later* NaN at the final rescale -- confirmed by tracing weight()'s own arithmetic when
    // r_m0/r_m1 are both zero, not assumed. imbe.rs's own `EnhancedSpectrals::new` has no such guard
    // either; it instead relies on its caller (`decode.rs`'s `Bootstrap::Silence` branch) to never
    // invoke enhancement on a silent frame at all -- this codec doesn't yet have that caller-side
    // check wired up, so the guard belongs here instead, matching the spec's own physical intent that
    // a silent frame's enhanced amplitudes are just as silent.
    if r_m0 == 0.0 {
        return spectral_amplitudes.to_vec();
    }

    let r_m1 = scaled_energy(spectral_amplitudes, omega0_hat);

    let mut enhanced: Vec<f64> = spectral_amplitudes
        .iter()
        .enumerate()
        .map(|(idx, &m_l)| {
            let l = idx as u32 + 1;
            if 8 * l <= l_hat {
                m_l
            } else {
                let w_l = weight(m_l, l as f64, omega0_hat, r_m0, r_m1);
                w_l.clamp(0.5, 1.2) * m_l
            }
        })
        .collect();

    let enhanced_energy: f64 = enhanced.iter().map(|m| m * m).sum();
    let gamma = (r_m0 / enhanced_energy).sqrt();
    for m in enhanced.iter_mut() {
        *m *= gamma;
    }
    enhanced
}

/// `S_E(0)` (Eq. 111): the local energy parameter, an EWMA of `R_M0` floored at `10000.0`.
pub fn update_local_energy(previous_s_e: f64, r_m0: f64) -> f64 {
    (0.95 * previous_s_e + 0.05 * r_m0).max(10000.0)
}

/// `V_M` (Eq. 112): the adaptive V/UV-forcing threshold used by [`smooth_voicing_decision`]. Uses
/// `f64::INFINITY` for the first branch (the spec's own literal infinity symbol), so no amplitude
/// can ever exceed it in that regime.
// [@ANCHOR: adaptive_voicing_threshold]
pub fn adaptive_voicing_threshold(errors: &FrameErrors, s_e: f64) -> f64 {
    if errors.rate <= 0.005 && errors.total <= 4 {
        f64::INFINITY
    } else if errors.rate <= 0.0125 && errors.hamming_init == 0 {
        45.255 * s_e.powf(0.375) / (277.26 * errors.rate).exp()
    } else {
        1.414 * s_e.powf(0.375)
    }
}

/// `v_bar_l` (Eq. 113): forces harmonic `l` voiced if its own enhanced amplitude exceeds `v_m`;
/// otherwise leaves the decoded V/UV decision unchanged. Note this can only ever turn an unvoiced
/// decision *into* voiced, never the reverse -- the spec's own "otherwise" branch is "leave alone,"
/// not "declare unvoiced."
pub fn smooth_voicing_decision(enhanced_m_l: f64, decoded_voiced: bool, v_m: f64) -> bool {
    enhanced_m_l > v_m || decoded_voiced
}

/// `A_M` (Eq. 114): the sum of the enhanced spectral amplitudes, feeding [`amplitude_smoothing_scale`].
pub fn amplitude_sum(enhanced: &[f64]) -> f64 {
    enhanced.iter().sum()
}

/// `tau_M(0)` (Eq. 115): the amplitude-smoothing threshold, carried forward frame to frame.
// [@ANCHOR: update_amplitude_threshold]
pub fn update_amplitude_threshold(errors: &FrameErrors, previous_tau_m: f64) -> f64 {
    if errors.rate <= 0.005 && errors.total <= 6 {
        20480.0
    } else {
        6000.0 - 300.0 * errors.total as f64 + previous_tau_m
    }
}

/// `gamma_M` (Eq. 116): the final smoothing scale factor, applied by the caller to each enhanced
/// spectral amplitude (Fig. 25's own "Spectral Amplitude Smoothing" stage, after enhancement and
/// V/UV smoothing have both already run).
// [@ANCHOR: amplitude_smoothing_scale]
pub fn amplitude_smoothing_scale(tau_m: f64, a_m: f64) -> f64 {
    if tau_m > a_m {
        1.0
    } else {
        tau_m / a_m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_and_scaled_energy_match_eq105_106_at_a_hand_computed_value() {
        let amplitudes = [2.0, 3.0, 4.0];
        let omega0_hat = 0.1;
        assert!((energy(&amplitudes) - 29.0).abs() < 1e-9); // 4+9+16

        let expected_scaled: f64 =
            4.0 * (0.1f64 * 1.0).cos() + 9.0 * (0.1f64 * 2.0).cos() + 16.0 * (0.1f64 * 3.0).cos();
        assert!((scaled_energy(&amplitudes, omega0_hat) - expected_scaled).abs() < 1e-9);
    }

    /// The real, checkable point of Eq. 109-110's own rescaling step: the enhanced output's total
    /// energy must exactly equal the unenhanced input's `R_M0`, regardless of how much Eq. 108's own
    /// per-harmonic weighting changed the shape -- the spec's own stated purpose ("remove any energy
    /// difference between the enhanced and unenhanced amplitudes").
    #[test]
    // Tests [@ANCHOR: enhance_spectral_amplitudes]
    fn enhance_spectral_amplitudes_preserves_total_energy() {
        let amplitudes: Vec<f64> = (1..=20)
            .map(|l| 1.0 + (l as f64 * 0.37).sin().abs())
            .collect();
        let omega0_hat = 2.0 * PI / 80.0;
        let original_energy = energy(&amplitudes);

        let enhanced = enhance_spectral_amplitudes(&amplitudes, omega0_hat);
        assert_eq!(enhanced.len(), amplitudes.len());
        let enhanced_energy = energy(&enhanced);
        assert!(
            (enhanced_energy - original_energy).abs() < 1e-6,
            "expected enhancement to preserve total energy exactly: {original_energy} vs {enhanced_energy}"
        );
    }

    #[test]
    // Tests [@ANCHOR: weight]
    fn enhance_spectral_amplitudes_leaves_low_harmonics_unweighted_before_rescaling() {
        // For L_hat=8, only l=1 satisfies 8*l <= 8 -- every harmonic 2..=8 gets weighted. Checked
        // indirectly: a uniform input (all amplitudes equal) makes R_M1 == R_M0 * cos(omega0*l)
        // averaged oddly, so instead just confirm the harmonic-1 special case structurally by
        // comparing against a hand-unrolled computation of the same formula.
        let amplitudes = vec![2.0; 8];
        let omega0_hat = 2.0 * PI / 100.0;
        let r_m0 = energy(&amplitudes);
        let r_m1 = scaled_energy(&amplitudes, omega0_hat);

        // Harmonic 1: 8*1 <= 8, so it passes through unweighted (before the final rescale).
        // Harmonic 2: 8*2 > 8, so it's weighted by Eq. 107, clamped to [0.5, 1.2].
        let w2 = weight(amplitudes[1], 2.0, omega0_hat, r_m0, r_m1).clamp(0.5, 1.2);
        let pre_rescale_h1 = amplitudes[0];
        let pre_rescale_h2 = w2 * amplitudes[1];

        // Reconstruct what the final (rescaled) values should be using the same gamma the function
        // itself would compute, by replicating the full pre-rescale vector.
        let pre_rescale: Vec<f64> = (1..=8u32)
            .map(|l| {
                let m_l = amplitudes[(l - 1) as usize];
                if 8 * l <= 8 {
                    m_l
                } else {
                    weight(m_l, l as f64, omega0_hat, r_m0, r_m1).clamp(0.5, 1.2) * m_l
                }
            })
            .collect();
        let pre_rescale_energy: f64 = pre_rescale.iter().map(|m| m * m).sum();
        let gamma = (r_m0 / pre_rescale_energy).sqrt();

        let enhanced = enhance_spectral_amplitudes(&amplitudes, omega0_hat);
        assert!((enhanced[0] - pre_rescale_h1 * gamma).abs() < 1e-9);
        assert!((enhanced[1] - pre_rescale_h2 * gamma).abs() < 1e-9);
    }

    /// A real, reachable boundary case (a genuinely silent frame), not a hypothetical: without the
    /// `r_m0 == 0.0` guard, Eq. 107's own weight() computes `0.0 / 0.0` internally for every harmonic
    /// past the unweighted cutoff, producing NaN well before Eq. 109's own rescale is ever reached --
    /// so this test exercises an L_hat large enough (20) that some harmonics do fall past the
    /// `8*l <= L_hat` cutoff and would have hit that branch.
    #[test]
    fn enhance_spectral_amplitudes_handles_an_all_zero_frame_without_producing_nan() {
        let amplitudes = vec![0.0; 20];
        let omega0_hat = 2.0 * PI / 100.0;
        let enhanced = enhance_spectral_amplitudes(&amplitudes, omega0_hat);
        assert_eq!(enhanced.len(), 20);
        for (idx, &m) in enhanced.iter().enumerate() {
            assert!(m.is_finite(), "harmonic {} was non-finite: {}", idx + 1, m);
            assert_eq!(m, 0.0, "harmonic {} expected 0.0, got {}", idx + 1, m);
        }
    }

    #[test]
    fn update_local_energy_matches_eq111_in_both_branches() {
        // Above the floor: the EWMA value wins.
        let e = update_local_energy(20000.0, 20000.0);
        assert!((e - (0.95 * 20000.0 + 0.05 * 20000.0)).abs() < 1e-9);
        // Below the floor: clamps to 10000.0.
        let e = update_local_energy(0.0, 0.0);
        assert_eq!(e, 10000.0);
    }

    fn errors_with(total: u32, rate: f64, hamming_init: u32) -> FrameErrors {
        FrameErrors {
            total,
            rate,
            golay_init: 0,
            hamming_init,
        }
    }

    #[test]
    // Tests [@ANCHOR: adaptive_voicing_threshold]
    fn adaptive_voicing_threshold_matches_eq112_in_all_three_branches() {
        assert_eq!(
            adaptive_voicing_threshold(&errors_with(4, 0.005, 0), 12345.0),
            f64::INFINITY
        );
        let s_e = 20000.0f64;
        let expected_branch2 = 45.255 * s_e.powf(0.375) / (277.26 * 0.01f64).exp();
        assert!(
            (adaptive_voicing_threshold(&errors_with(5, 0.01, 0), s_e) - expected_branch2).abs()
                < 1e-6
        );
        // hamming_init != 0 forces the "otherwise" branch even though the rate is small.
        let expected_branch3 = 1.414 * s_e.powf(0.375);
        assert!(
            (adaptive_voicing_threshold(&errors_with(5, 0.01, 1), s_e) - expected_branch3).abs()
                < 1e-6
        );
        // A high rate also forces the "otherwise" branch.
        assert!(
            (adaptive_voicing_threshold(&errors_with(5, 0.02, 0), s_e) - expected_branch3).abs()
                < 1e-6
        );
    }

    #[test]
    fn smooth_voicing_decision_only_ever_forces_voiced_never_unvoiced() {
        assert!(smooth_voicing_decision(10.0, false, 5.0)); // forced voiced
        assert!(smooth_voicing_decision(10.0, true, 5.0)); // already voiced, stays voiced
        assert!(!smooth_voicing_decision(1.0, false, 5.0)); // below threshold, stays unvoiced
        assert!(smooth_voicing_decision(1.0, true, 5.0)); // below threshold, but decoded voiced stays voiced
    }

    #[test]
    // Tests [@ANCHOR: update_amplitude_threshold]
    fn update_amplitude_threshold_matches_eq115_in_both_branches() {
        assert_eq!(
            update_amplitude_threshold(&errors_with(6, 0.005, 0), 999.0),
            20480.0
        );
        let e = errors_with(10, 0.006, 0);
        assert!(
            (update_amplitude_threshold(&e, 500.0) - (6000.0 - 300.0 * 10.0 + 500.0)).abs() < 1e-9
        );
    }

    #[test]
    // Tests [@ANCHOR: amplitude_smoothing_scale]
    fn amplitude_smoothing_scale_matches_eq116_in_both_branches() {
        assert_eq!(amplitude_smoothing_scale(100.0, 50.0), 1.0);
        assert!((amplitude_smoothing_scale(50.0, 100.0) - 0.5).abs() < 1e-9);
        // Exact boundary: tau_m == a_m takes the "otherwise" branch, giving 1.0 either way.
        assert_eq!(amplitude_smoothing_scale(50.0, 50.0), 1.0);
    }
}
