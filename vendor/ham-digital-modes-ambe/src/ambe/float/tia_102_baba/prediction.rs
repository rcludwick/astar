//! Spectral amplitude prediction residual (TIA-102.BABA_2003.pdf section 6.2, Eq. 52-57) -- the
//! differential coding step that transmits how much the current frame's spectral envelope changed
//! from the previous frame, in the log2 domain, rather than transmitting the envelope itself.
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 42, following the same
//! discipline as every other body-text equation in this spec (Type3 digit font defeats `pdftotext`).
//!
//! # `previous_m` must be real reconstructed history, not an unquantized estimate
//!
//! Eq. 54's own bias-corrected prediction needs `M_tilde_j(-1)`: the previous frame's *quantized and
//! reconstructed* spectral amplitudes, not an encoder's own unquantized estimate of them (that's the
//! whole point of a closed-loop predictive coder -- the encoder must predict from what the decoder
//! will actually have, which means simulating the decoder's own reconstruction). Neither
//! [`prediction_residual`] nor [`reconstruct_log2_amplitude`] below computes that reconstruction
//! itself -- both take `previous_m` as a plain input parameter, supplied by [`super::reconstruct`]
//! (dequantization, inverse DCT, and Eq. 75-79's own log2 reassembly) via `mod.rs`'s own
//! `encode_frame`, which is where the real reconstruction loop lives. Callers must supply real
//! reconstructed history (or the spec's own literal initialization values, [`INITIAL_L_HAT_PREV`] and
//! an all-`1.0` amplitude array, for the very first frame of a stream) rather than this module
//! quietly assuming zero history.

/// The number of harmonics assumed for the previous frame before any real previous frame exists
/// (the spec's own stated initialization value, not zero): "upon initialization ... `L_hat(-1) = 30`".
pub const INITIAL_L_HAT_PREV: u32 = 30;

/// The prediction coefficient `rho` (Eq. 55): how strongly the current frame's spectral amplitudes
/// are predicted from the previous frame's, as a function of how many harmonics the current frame
/// has. A sparser harmonic set (fewer, lower-frequency harmonics, small `l_hat_curr`) gets weaker
/// prediction; a fuller one gets strong prediction, on the presumption that harmonic-rich frames
/// change more smoothly frame to frame.
// [@ANCHOR: prediction_coefficient]
pub fn prediction_coefficient(l_hat_curr: u32) -> f64 {
    if l_hat_curr <= 15 {
        0.4
    } else if l_hat_curr <= 24 {
        0.03 * l_hat_curr as f64 - 0.05
    } else {
        0.7
    }
}

/// `k_hat_l` (Eq. 52): harmonic `l`'s own fractional position in the *previous* frame's harmonic
/// index space, found by scaling `l` by the ratio of the two frames' own harmonic counts (since a
/// pitch change between frames means harmonic `l` in this frame doesn't line up with harmonic `l` in
/// the previous one).
fn harmonic_index_ratio(l: u32, l_hat_prev: u32, l_hat_curr: u32) -> f64 {
    (l_hat_prev as f64 / l_hat_curr as f64) * l as f64
}

/// `delta_hat_l` (Eq. 53): the fractional part of [`harmonic_index_ratio`], used to linearly
/// interpolate the prediction between the previous frame's two nearest harmonics.
fn fractional_part(k_hat_l: f64) -> f64 {
    k_hat_l - k_hat_l.floor()
}

/// `log2(M_tilde_j(-1))` for any `j >= 0`, applying the spec's own two boundary assumptions so
/// callers don't need to special-case them: Eq. 56 (`M_tilde_0(-1) = 1.0`, so `log2` is exactly
/// `0.0`) and Eq. 57 (`M_tilde_j(-1) = M_tilde_{L_hat(-1)}(-1)` for `j > L_hat(-1)`, i.e. indices
/// past the previous frame's own last real harmonic hold at that last value rather than reading past
/// the end of `previous_m`).
///
/// `previous_m` holds `M_tilde_j(-1)` for `j = 1..=l_hat_prev`, one-indexed (`previous_m[0]` is
/// `M_tilde_1(-1)`).
// [@ANCHOR: previous_log2_amplitude]
fn previous_log2_amplitude(previous_m: &[f64], l_hat_prev: u32, j: u32) -> f64 {
    if j == 0 {
        0.0
    } else {
        let clamped_j = j.min(l_hat_prev);
        previous_m[(clamped_j - 1) as usize].log2()
    }
}

/// The pitch-interpolated prediction term and the frame-wide bias correction that both
/// [`prediction_residual`] (Eq. 54, encoder side) and [`reconstruct_log2_amplitude`] (Eq. 77,
/// decoder side) need identically -- the two equations are the same terms with opposite sign
/// (encoder subtracts the prediction and adds the bias back; decoder does the reverse to invert
/// it), so this is computed once and shared rather than kept as two separately-maintained copies
/// that could silently drift apart.
// [@ANCHOR: predicted_and_bias_correction]
fn predicted_and_bias_correction(
    l: u32,
    l_hat_curr: u32,
    l_hat_prev: u32,
    previous_m: &[f64],
) -> (f64, f64) {
    let rho = prediction_coefficient(l_hat_curr);

    let k_hat_l = harmonic_index_ratio(l, l_hat_prev, l_hat_curr);
    let delta_l = fractional_part(k_hat_l);
    let floor_k_l = k_hat_l.floor() as u32;
    let predicted =
        rho * (1.0 - delta_l) * previous_log2_amplitude(previous_m, l_hat_prev, floor_k_l)
            + rho * delta_l * previous_log2_amplitude(previous_m, l_hat_prev, floor_k_l + 1);

    let bias: f64 = (1..=l_hat_curr)
        .map(|lambda| {
            let k_hat_lambda = harmonic_index_ratio(lambda, l_hat_prev, l_hat_curr);
            let delta_lambda = fractional_part(k_hat_lambda);
            let floor_k_lambda = k_hat_lambda.floor() as u32;
            (1.0 - delta_lambda) * previous_log2_amplitude(previous_m, l_hat_prev, floor_k_lambda)
                + delta_lambda * previous_log2_amplitude(previous_m, l_hat_prev, floor_k_lambda + 1)
        })
        .sum();
    let bias_correction = (rho / l_hat_curr as f64) * bias;

    (predicted, bias_correction)
}

/// The prediction residual `T_hat_l` (Eq. 54) for harmonic `l`: this frame's own unquantized
/// spectral amplitude estimate `M_hat_l(0)` (from [`super::spectral_amplitude`]), minus a
/// pitch-interpolated prediction from the previous frame's reconstructed amplitudes, plus a
/// frame-wide bias correction (the sum over every current-frame harmonic's own prediction) that
/// removes the previous frame's overall level so only the *shape change* is transmitted -- see this
/// module's own doc comment on why `previous_m` must be real reconstructed history, not this
/// encoder's own unquantized estimate of it.
pub fn prediction_residual(
    l: u32,
    unquantized_m_l: f64,
    l_hat_curr: u32,
    l_hat_prev: u32,
    previous_m: &[f64],
) -> f64 {
    let (predicted, bias_correction) =
        predicted_and_bias_correction(l, l_hat_curr, l_hat_prev, previous_m);
    unquantized_m_l.log2() - predicted + bias_correction
}

/// The decoder-side inverse of [`prediction_residual`] (Eq. 75-77): reconstructs
/// `log2(M_tilde_l(0))`, harmonic `l`'s own log2 spectral amplitude for the *current* frame, from
/// its transmitted residual `t_hat_l` (the dequantized, inverse-DCT'd prediction residual, i.e.
/// `T_tilde_l` per this module's own doc comment) and the same previous-frame history
/// [`prediction_residual`] used to produce that residual in the first place. Exactly reverses
/// Eq. 54's own rearrangement: `log2(M) = T_hat_l + predicted - bias_correction`, using the
/// identical `predicted`/`bias_correction` terms (see [`predicted_and_bias_correction`]'s own doc
/// comment on why the two equations must share one implementation rather than risk drifting apart).
pub fn reconstruct_log2_amplitude(
    l: u32,
    t_hat_l: f64,
    l_hat_curr: u32,
    l_hat_prev: u32,
    previous_m: &[f64],
) -> f64 {
    let (predicted, bias_correction) =
        predicted_and_bias_correction(l, l_hat_curr, l_hat_prev, previous_m);
    t_hat_l + predicted - bias_correction
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Tests [@ANCHOR: prediction_coefficient]
    fn prediction_coefficient_matches_eq55_in_all_three_branches() {
        assert!((prediction_coefficient(15) - 0.4).abs() < 1e-12);
        assert!((prediction_coefficient(16) - (0.03 * 16.0 - 0.05)).abs() < 1e-12);
        assert!((prediction_coefficient(24) - (0.03 * 24.0 - 0.05)).abs() < 1e-12);
        assert!((prediction_coefficient(25) - 0.7).abs() < 1e-12);
    }

    #[test]
    // Tests [@ANCHOR: previous_log2_amplitude]
    fn previous_log2_amplitude_applies_both_spec_boundary_assumptions() {
        let previous_m = [2.0, 4.0, 8.0]; // M_tilde_1..3(-1), l_hat_prev = 3
                                          // Eq. 56: index 0 always reads as log2(1.0) = 0.0, regardless of previous_m's own contents.
        assert_eq!(previous_log2_amplitude(&previous_m, 3, 0), 0.0);
        // A real, in-range index reads straight through.
        assert_eq!(previous_log2_amplitude(&previous_m, 3, 2), 4.0f64.log2());
        // Eq. 57: an index past l_hat_prev holds at the last real value instead of panicking.
        assert_eq!(previous_log2_amplitude(&previous_m, 3, 5), 8.0f64.log2());
    }

    /// The real, checkable point of Eq. 54's own bias-correction term: when the previous frame's
    /// log2 amplitudes are all equal to the same constant (so every harmonic predicts the same
    /// value, regardless of pitch-driven index interpolation), that constant should cancel out of
    /// the residual entirely -- `T_hat_l` should reduce to plain `log2(M_hat_l(0))`, matching the
    /// spec's own stated intent that only the *shape change* (not the previous frame's overall
    /// level) gets transmitted.
    #[test]
    // Tests [@ANCHOR: predicted_and_bias_correction]
    fn a_constant_previous_frame_level_cancels_out_of_the_residual() {
        let l_hat_curr = 20;
        let l_hat_prev = 20;
        let constant_level = 4.0; // M_tilde_j(-1) = 4.0 for every j
        let previous_m = vec![constant_level; l_hat_prev as usize];

        for l in 1..=l_hat_curr {
            let unquantized_m_l = 3.0 + l as f64 * 0.1; // any real, varying per-harmonic estimate
            let residual =
                prediction_residual(l, unquantized_m_l, l_hat_curr, l_hat_prev, &previous_m);
            let expected = unquantized_m_l.log2();
            assert!(
                (residual - expected).abs() < 1e-9,
                "harmonic {l}: expected the constant previous level to cancel out, leaving {expected}, got {residual}"
            );
        }
    }

    /// A real, different-pitch case (l_hat_prev != l_hat_curr, so harmonic_index_ratio is not the
    /// identity map): the same "constant previous level cancels" property must still hold, since
    /// Eq. 54's bias correction sums over exactly the same interpolation the main prediction term
    /// uses -- proving the cancellation isn't an artifact of the l_hat_prev == l_hat_curr case above.
    #[test]
    fn a_constant_previous_frame_level_cancels_out_even_with_a_pitch_change() {
        let l_hat_curr = 15;
        let l_hat_prev = 22;
        let constant_level = 1.5;
        let previous_m = vec![constant_level; l_hat_prev as usize];

        for l in 1..=l_hat_curr {
            let unquantized_m_l = 2.0 + l as f64 * 0.05;
            let residual =
                prediction_residual(l, unquantized_m_l, l_hat_curr, l_hat_prev, &previous_m);
            let expected = unquantized_m_l.log2();
            assert!(
                (residual - expected).abs() < 1e-9,
                "harmonic {l}: expected cancellation under a pitch change too, expected {expected}, got {residual}"
            );
        }
    }

    /// `reconstruct_log2_amplitude` must be the exact inverse of `prediction_residual` -- the real
    /// property closing the decoder-side loop depends on: given the residual `prediction_residual`
    /// produced for a real, varying (not constant, unlike the tests above) unquantized amplitude,
    /// feeding it back through `reconstruct_log2_amplitude` with the same history must reproduce
    /// `log2(unquantized_m_l)` exactly (up to floating-point roundoff) -- not merely a plausible
    /// value.
    #[test]
    fn reconstruct_log2_amplitude_is_the_exact_inverse_of_prediction_residual() {
        let l_hat_curr = 18;
        let l_hat_prev = 22;
        let previous_m: Vec<f64> = (1..=l_hat_prev).map(|j| 1.0 + j as f64 * 0.3).collect();

        for l in 1..=l_hat_curr {
            let unquantized_m_l = 0.5 + l as f64 * 0.2;
            let residual =
                prediction_residual(l, unquantized_m_l, l_hat_curr, l_hat_prev, &previous_m);
            let reconstructed =
                reconstruct_log2_amplitude(l, residual, l_hat_curr, l_hat_prev, &previous_m);
            let expected = unquantized_m_l.log2();
            assert!(
                (reconstructed - expected).abs() < 1e-9,
                "harmonic {l}: expected the round trip to reproduce {expected}, got {reconstructed}"
            );
        }
    }
}
