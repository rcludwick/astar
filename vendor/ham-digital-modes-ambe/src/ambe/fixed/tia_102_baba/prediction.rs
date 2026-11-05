// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::tia_102_baba::prediction`: the decoder-side `reconstruct_log2_amplitude` and the
//! encoder-side `prediction_residual` (both built on the shared predicted/bias-correction terms).
//! `prediction_coefficient` is an exact table lookup (`L` only has 48 possible values, `9..=56`) rather than the
//! float sibling's own piecewise formula, avoiding hand-transcribing Q16.16 literals for a formula that's cheap to
//! precompute exactly instead.

use super::reconstruct_tables::PREDICTION_COEFFICIENT_Q16_16;
use crate::ambe::fixed::general::explog::{log2_q16, log2_q16_i64};
use crate::ambe::fixed::general::fixed_ops::{div_q16, mul_q16};

/// Same value as the float sibling's own `INITIAL_L_HAT_PREV` -- the spec's own stated
/// initialization ("upon initialization ... `L_hat(-1) = 30`"), pure integer, no port needed.
pub const INITIAL_L_HAT_PREV: u32 = 30;

/// `rho` (Eq. 55) in Q16.16, via [`PREDICTION_COEFFICIENT_Q16_16`]'s exact table lookup. Clamps to
/// the nearest valid `L` rather than panicking on an out-of-spec value.
pub fn prediction_coefficient_q16(l_hat_curr: u32) -> i32 {
    let index =
        (l_hat_curr.saturating_sub(9) as usize).min(PREDICTION_COEFFICIENT_Q16_16.len() - 1);
    PREDICTION_COEFFICIENT_Q16_16[index]
}

/// `k_hat_l` (Eq. 52) in Q16.16: `(l_hat_prev / l_hat_curr) * l`, computed as one exact rational
/// (`(l_hat_prev * l) / l_hat_curr`, scaled into Q16.16 directly) rather than dividing first, the
/// same reasoning `general::mbe_speech`'s own resampling computation uses. Not routed through
/// `div_q16` since that function's own inputs are Q16.16 *values*, not plain integer counts like
/// `l_hat_prev`/`l`/`l_hat_curr` here.
fn harmonic_index_ratio_q16(l: u32, l_hat_prev: u32, l_hat_curr: u32) -> i32 {
    ((((l_hat_prev as i64) * (l as i64)) << 16) / (l_hat_curr as i64)) as i32
}

/// `log2(M_tilde_j(-1))` in Q16.16, for any `j >= 0` -- the fixed-point equivalent of
/// `previous_log2_amplitude`, applying the same two spec boundary assumptions (Eq. 56/57).
/// `previous_m_q16` holds `M_tilde_j(-1)` in Q16.16 for `j = 1..=l_hat_prev`, one-indexed.
fn previous_log2_amplitude_q16(previous_m_q16: &[i32], l_hat_prev: u32, j: u32) -> i32 {
    if j == 0 {
        0
    } else {
        let clamped_j = j.min(l_hat_prev);
        log2_q16(previous_m_q16[(clamped_j - 1) as usize])
    }
}

/// The fixed-point equivalent of `predicted_and_bias_correction` -- shared by (a future)
/// fixed-point `prediction_residual` and [`reconstruct_log2_amplitude_q16`] below, same reasoning as
/// the float sibling's own doc comment on why the two must share one implementation.
fn predicted_and_bias_correction_q16(
    l: u32,
    l_hat_curr: u32,
    l_hat_prev: u32,
    previous_m_q16: &[i32],
) -> (i32, i32) {
    let rho_q16 = prediction_coefficient_q16(l_hat_curr);

    let k_hat_l_q16 = harmonic_index_ratio_q16(l, l_hat_prev, l_hat_curr);
    let floor_k_l = (k_hat_l_q16 >> 16).max(0) as u32;
    let delta_l_q16 = k_hat_l_q16 - ((floor_k_l as i32) << 16);

    let predicted_q16 = mul_q16(
        rho_q16,
        mul_q16(
            65536 - delta_l_q16,
            previous_log2_amplitude_q16(previous_m_q16, l_hat_prev, floor_k_l),
        ) + mul_q16(
            delta_l_q16,
            previous_log2_amplitude_q16(previous_m_q16, l_hat_prev, floor_k_l + 1),
        ),
    );

    let mut bias_sum_q16: i64 = 0;
    for lambda in 1..=l_hat_curr {
        let k_hat_lambda_q16 = harmonic_index_ratio_q16(lambda, l_hat_prev, l_hat_curr);
        let floor_k_lambda = (k_hat_lambda_q16 >> 16).max(0) as u32;
        let delta_lambda_q16 = k_hat_lambda_q16 - ((floor_k_lambda as i32) << 16);
        let term = mul_q16(
            65536 - delta_lambda_q16,
            previous_log2_amplitude_q16(previous_m_q16, l_hat_prev, floor_k_lambda),
        ) + mul_q16(
            delta_lambda_q16,
            previous_log2_amplitude_q16(previous_m_q16, l_hat_prev, floor_k_lambda + 1),
        );
        bias_sum_q16 += term as i64;
    }
    let bias_sum_q16 = bias_sum_q16 as i32;
    let bias_correction_q16 = div_q16(mul_q16(rho_q16, bias_sum_q16), (l_hat_curr as i32) << 16);

    (predicted_q16, bias_correction_q16)
}

/// The fixed-point equivalent of `reconstruct_log2_amplitude` (Eq. 75-77): `log2(M) = t_hat_l +
/// predicted - bias_correction`.
pub fn reconstruct_log2_amplitude_q16(
    l: u32,
    t_hat_l_q16: i32,
    l_hat_curr: u32,
    l_hat_prev: u32,
    previous_m_q16: &[i32],
) -> i32 {
    let (predicted_q16, bias_correction_q16) =
        predicted_and_bias_correction_q16(l, l_hat_curr, l_hat_prev, previous_m_q16);
    t_hat_l_q16 + predicted_q16 - bias_correction_q16
}

/// The fixed-point equivalent of `prediction_residual` (Eq. 54): `T_hat_l = log2(M_hat_l) - predicted +
/// bias_correction`, from the frame's own unquantized amplitude `unquantized_m_l_q16` (Q16.16, floored at one LSB
/// so a silent harmonic gives a very negative but finite residual instead of `i32::MIN`).
pub fn prediction_residual_q16(
    l: u32,
    unquantized_m_l_q16: i64,
    l_hat_curr: u32,
    l_hat_prev: u32,
    previous_m_q16: &[i32],
) -> i32 {
    let (predicted_q16, bias_correction_q16) =
        predicted_and_bias_correction_q16(l, l_hat_curr, l_hat_prev, previous_m_q16);
    log2_q16_i64(unquantized_m_l_q16.max(1)) - predicted_q16 + bias_correction_q16
}
