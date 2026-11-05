// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::tia_102_baba::enhancement` (Eq. 105-116) -- spectral amplitude
//! enhancement and adaptive smoothing. **The float sibling's own doc comment specifically warns
//! that Eq. 112 (`V_M`'s `epsilon_T <= 4` branch) and Eq. 115 (`tau_M`'s `epsilon_T <= 6` branch)
//! are two different numbers for two different variables, easily conflated** -- ported here with
//! that same care, each threshold kept next to its own real equation number in a comment, not
//! merged or assumed identical.
//!
//! **Energy-domain quantities need a wider range than plain Q16.16, not a different Q-format.**
//! Live chip data (`examples/ambe_fixed_chip_validate_ratet27.rs`'s own `R_M0` range print) shows
//! real `R_M0` (Eq. 105, the sum of squared spectral amplitudes) spans roughly 9 to 4x10^8 across
//! real decoded speech -- 8 orders of magnitude, far beyond Q16.16's own ~32767 real-valued ceiling,
//! and far beyond what any single linear rescale (dividing by one fixed constant before storing in
//! an `i32`) could cover at both ends at once. `R_M0`, `R_M1` (Eq. 106), `S_E` (Eq. 111), `tau_M`
//! (Eq. 115), and `A_M` (Eq. 114) are therefore kept as `i64` with the *same* 16 fractional bits as
//! everywhere else in this crate (`real = raw / 65536`, see [`super::super::general::fixed_ops::
//! mul_q16_i64`]/[`super::super::general::explog::log2_q16_i64`]) -- not a second Q-format, just a
//! wider container for the same convention, since Q16.16's *fractional* precision (16 bits) was
//! never the problem, only its *integer* range (31 usable bits in an `i32`).
//!
//! [`weight_q16`] (Eq. 107) can't just widen its container, though: its own float formula computes
//! `r_m0^2`, and squaring a value that may already need 30+ bits would need a 128-bit product merely
//! to stay exact, before ever dividing back down by another one. Instead it's algebraically
//! rewritten in terms of `k = R_M1/R_M0` (a dimensionless ratio, bounded to `[-1,1]` by
//! Cauchy-Schwarz, since `|R_M1| <= R_M0`) -- substituting `R_M1 = k*R_M0` into the float formula's
//! own `(R_M0^2 + R_M1^2 - 2*R_M0*R_M1*cos) / (omega0*R_M0*(R_M0^2-R_M1^2))` cancels every `R_M0^2`
//! term down to a single, un-squared `R_M0` in the denominator -- and the whole ratio is then
//! evaluated in the *log* domain (`log2(weight) = 0.5*log2(M_l) + 0.25*log2(ratio)`, `exp2`'d back at
//! the end) so that single `R_M0` factor only ever needs [`super::super::general::explog::
//! log2_q16_i64`], never a full-width division or multiplication of the raw energy value itself.

use super::error_estimation::FrameErrorsQ16;
use crate::ambe::fixed::general::explog::{exp2_q16, log2_q16, log2_q16_i64, LOG2_E_Q16_16};
use crate::ambe::fixed::general::fixed_ops::{div_q16_i64, mul_q16, mul_q16_i64, sqrt_q16};
use crate::ambe::fixed::general::trig::{cos_q16, phase_from_radians_q32};

/// `round(0.005 * 65536)`.
const POINT_005_Q16_16: i32 = 328;
/// `round(0.0125 * 65536)`.
const POINT_0125_Q16_16: i32 = 819;
/// `0.5` in Q16.16 -- [`weight_q16`]'s own lower clamp (Eq. 108).
const HALF_Q16_16: i32 = 32768;
/// `round(1.2 * 65536)` -- [`weight_q16`]'s own upper clamp (Eq. 108).
const ONE_POINT_2_Q16_16: i32 = 78643;
/// `round(0.96 * pi * 65536)` -- Eq. 107's own leading constant.
const POINT_96_PI_Q16_16: i32 = 197652;
/// A sentinel standing in for the spec's own literal `f64::INFINITY` (Eq. 112's first branch) --
/// `smooth_voicing_decision_q16`'s own `enhanced_m_l > v_m` comparison can never be true against
/// `i32::MAX` since no real Q16.16 amplitude reaches it, matching the float sibling's own "never
/// force voiced in this regime" intent exactly, without needing a real infinity representation.
const INFINITY_SENTINEL_Q16_16: i32 = i32::MAX;

/// `R_M0` (Eq. 105), as an `i64` with 16 fractional bits (see this module's own doc comment).
///
/// Derivation of the final shift: each `m_q16` is `real_m * 2^16`, so `m_q16^2` is
/// `real_m^2 * 2^32`, and summing gives `R_M0_real * 2^32`. The desired output is `R_M0_real *
/// 2^16` (16 fractional bits), so the sum is shifted right by `32 - 16 = 16`, not divided all the
/// way down to a plain integer.
pub fn energy_q16(spectral_amplitudes_q16: &[i32]) -> i64 {
    let sum: i128 = spectral_amplitudes_q16
        .iter()
        .map(|&m| (m as i128) * (m as i128))
        .sum();
    (sum >> 16) as i64
}

/// `R_M1` (Eq. 106), as an `i64` with 16 fractional bits: the same energy, weighted by each
/// harmonic's own phase term. `omega0_q32` (Q32 radians/sample) is a genuine radian angle (not a multiple of `pi`), so
/// this uses [`phase_from_radians_q32`] rather than the DCT-style `cos_pi_frac`.
///
/// Derivation of the final shift: `m_q16^2 * cos_val_q16` is `real_m^2 * 2^32 * real_cos * 2^16 =
/// R_M1_term_real * 2^48`; summing gives `R_M1_real * 2^48`. The desired output is `R_M1_real *
/// 2^16`, so the sum is shifted right by `48 - 16 = 32`.
pub fn scaled_energy_q16(spectral_amplitudes_q16: &[i32], omega0_q32: i64) -> i64 {
    let sum: i128 = spectral_amplitudes_q16
        .iter()
        .enumerate()
        .map(|(idx, &m)| {
            let l = (idx as i32) + 1;
            let cos_val = cos_q16(phase_from_radians_q32(omega0_q32 * l as i64)) as i128;
            (m as i128) * (m as i128) * cos_val
        })
        .sum();
    (sum >> 32) as i64
}

/// The fixed-point equivalent of `weight` (Eq. 107), evaluated in the log domain -- see this
/// module's own doc comment for the algebraic rewrite that avoids ever computing `r_m0^2`. Returns
/// the raw (not yet clamped to `[0.5, 1.2]`) weight; the caller clamps, exactly as the float sibling
/// separates `weight` from `enhance_spectral_amplitudes`'s own clamp.
fn weight_q16(m_l_q16: i32, l: i32, omega0_q32: i64, r_m0: i64, r_m1: i64) -> i32 {
    if m_l_q16 <= 0 {
        return 0; // sqrt(0) = 0 -> weight = 0, matching the float formula's own `sqrt(m_l)` factor.
    }

    let cos_val_q16 = cos_q16(phase_from_radians_q32(omega0_q32 * l as i64));

    let k_q16 = div_q16_i64(r_m1, r_m0); // R_M1/R_M0, bounded to [-1,1] by Cauchy-Schwarz.
    let k_sq_q16 = mul_q16(k_q16, k_q16);
    let two_k_cos_q16 = 2 * mul_q16(k_q16, cos_val_q16);
    // 1 + k^2 - 2*k*cos(omega0*l) = (k - cos)^2 + sin^2 >= 0 always; degenerate (== 0) only at the
    // Cauchy-Schwarz boundary (k = +-1, a fully phase-coherent harmonic) -- a real encoder's own
    // predictive/quantized amplitudes essentially never reach this exactly, but a rounding-induced
    // near-zero is guarded below rather than risking a division by (near) zero.
    let inner_q16 = 65536 + k_sq_q16 - two_k_cos_q16;
    // 1 - k^2: also vanishes only at the same Cauchy-Schwarz boundary.
    let one_minus_k_sq_q16 = 65536 - k_sq_q16;

    if inner_q16 <= 0 || one_minus_k_sq_q16 <= 0 {
        // Both the float formula's own numerator and denominator vanish together here (or rounding
        // pushed one just past zero) -- there is no well-defined ratio to take a log of. Fall back
        // to the "no enhancement" clamp boundary rather than dividing by (near) zero; this crate's
        // own established convention (see `log2_q16`'s own sentinel) is to saturate predictably
        // rather than propagate a NaN/infinity fixed point has no representation for.
        return HALF_Q16_16;
    }

    let log2_ratio_q16 = log2_q16(POINT_96_PI_Q16_16) + log2_q16(inner_q16)
        - (log2_q16_i64(omega0_q32) - (16 << 16)) // `log2_q16_i64` reads a Q32 value as Q16.16: +16.
        - log2_q16_i64(r_m0)
        - log2_q16(one_minus_k_sq_q16);

    // log2(weight) = 0.5*log2(m_l) + 0.25*log2(ratio).
    let log2_w_q16 = mul_q16(log2_q16(m_l_q16), HALF_Q16_16) + mul_q16(log2_ratio_q16, 16384);
    exp2_q16(log2_w_q16)
}

/// The fixed-point equivalent of `enhance_spectral_amplitudes` (Eq. 105-110).
pub fn enhance_spectral_amplitudes_q16(
    spectral_amplitudes_q16: &[i32],
    omega0_q32: i64,
) -> Vec<i32> {
    let l_hat = spectral_amplitudes_q16.len() as u32;
    let r_m0 = energy_q16(spectral_amplitudes_q16);

    if r_m0 == 0 {
        return spectral_amplitudes_q16.to_vec();
    }

    let r_m1 = scaled_energy_q16(spectral_amplitudes_q16, omega0_q32);

    let mut enhanced: Vec<i32> = spectral_amplitudes_q16
        .iter()
        .enumerate()
        .map(|(idx, &m_l_q16)| {
            let l = idx as u32 + 1;
            if 8 * l <= l_hat {
                m_l_q16
            } else {
                let w_l_q16 = weight_q16(m_l_q16, l as i32, omega0_q32, r_m0, r_m1);
                let clamped = w_l_q16.clamp(HALF_Q16_16, ONE_POINT_2_Q16_16);
                mul_q16(clamped, m_l_q16)
            }
        })
        .collect();

    let enhanced_energy = energy_q16(&enhanced);
    let gamma_q16 = sqrt_q16(div_q16_i64(r_m0, enhanced_energy));
    for m in enhanced.iter_mut() {
        *m = mul_q16(*m, gamma_q16);
    }
    enhanced
}

/// `S_E(0)` (Eq. 111), as an `i64` with 16 fractional bits: an EWMA of `R_M0`, floored at `10000.0`
/// -- the same domain as [`energy_q16`]'s own `R_M0`, since Eq. 111 mixes the two directly.
pub fn update_local_energy_q16(previous_s_e: i64, r_m0: i64) -> i64 {
    const POINT_95_Q16_16: i32 = 62259;
    const POINT_05_Q16_16: i32 = 3277;
    const TEN_THOUSAND_Q16_16: i64 = 10000i64 << 16;
    let value = mul_q16_i64(previous_s_e, POINT_95_Q16_16) + mul_q16_i64(r_m0, POINT_05_Q16_16);
    value.max(TEN_THOUSAND_Q16_16)
}

/// `V_M` (Eq. 112) -- the adaptive V/UV-forcing threshold. **Eq. 112 specifically** (`epsilon_T <=
/// 4`, not Eq. 115's own `epsilon_T <= 6` -- see this module's own doc comment).
///
/// **Entirely log-domain, not just `s_e`'s own `log2`.** An earlier version computed
/// `45.255 * s_e.powf(0.375)` as an ordinary linear Q16.16 `i32` *before* dividing by
/// `exp(277.26*rate)` -- but that intermediate product can itself exceed `i32`'s own Q16.16 ceiling
/// even when the *final*, divided `V_M` does not (confirmed by a real test failure: `s_e = 4e8`
/// gives a final `V_M ~= 4757`, comfortably in range, but the undivided intermediate `~76187` is
/// still 2.3x too large for a plain Q16.16 `i32`, silently corrupting the result). Every term here
/// is instead combined as a `log2` sum first, with a single `exp2_q16` at the very end -- the same
/// structure [`weight_q16`] uses and for the same reason: log-domain sums never need to represent
/// the large linear intermediate at all. Returns an ordinary Q16.16 `i32`: real spectral amplitudes
/// `M_l` top out around `3600`, so once the final `exp2_q16` would saturate past `i32`'s own
/// ceiling, `V_M > M_l` is already unconditionally false regardless of `V_M`'s exact saturated
/// value -- that saturation is the correct answer, not a precision bug.
pub fn adaptive_voicing_threshold_q16(errors: &FrameErrorsQ16, s_e: i64) -> i32 {
    if errors.rate_q16 <= POINT_005_Q16_16 && errors.total <= 4 {
        INFINITY_SENTINEL_Q16_16
    } else if errors.rate_q16 <= POINT_0125_Q16_16 && errors.hamming_init == 0 {
        const FORTY_FIVE_POINT_255_Q16_16: i32 = 2965832; // round(45.255 * 65536)
        const TWO_SEVEN_SEVEN_POINT_26_Q16_16: i32 = 18170511; // round(277.26 * 65536)
                                                               // log2(V_M) = log2(45.255) + 0.375*log2(s_e) - 277.26*rate*log2(e)
                                                               //                                                ^^^^^^^^^^^^^^^^^ log2(exp(277.26*rate))
        let log2_v_m_q16 = log2_q16(FORTY_FIVE_POINT_255_Q16_16)
            + mul_q16(24576, log2_q16_i64(s_e)) // 24576 = 0.375
            - mul_q16(mul_q16(TWO_SEVEN_SEVEN_POINT_26_Q16_16, errors.rate_q16), LOG2_E_Q16_16);
        exp2_q16(log2_v_m_q16)
    } else {
        const ONE_POINT_414_Q16_16: i32 = 92668; // round(1.414 * 65536)
                                                 // log2(V_M) = log2(1.414) + 0.375*log2(s_e)
        let log2_v_m_q16 = log2_q16(ONE_POINT_414_Q16_16) + mul_q16(24576, log2_q16_i64(s_e));
        exp2_q16(log2_v_m_q16)
    }
}

/// `v_bar_l` (Eq. 113).
pub fn smooth_voicing_decision_q16(
    enhanced_m_l_q16: i32,
    decoded_voiced: bool,
    v_m_q16: i32,
) -> bool {
    enhanced_m_l_q16 > v_m_q16 || decoded_voiced
}

/// `A_M` (Eq. 114), as an `i64` with 16 fractional bits (up to 56 harmonics at ~3600 real amplitude
/// each can reach roughly 200000, beyond a plain Q16.16 `i32`'s own ~32767 real-valued ceiling).
pub fn amplitude_sum_q16(enhanced_q16: &[i32]) -> i64 {
    enhanced_q16.iter().map(|&m| m as i64).sum()
}

/// `tau_M(0)` (Eq. 115), as an `i64` with 16 fractional bits -- the same domain as [`amplitude_sum_q16`]'s
/// own `A_M`, since Eq. 116 compares the two directly. **Eq. 115 specifically** (`epsilon_T <= 6`,
/// not Eq. 112's own `epsilon_T <= 4` -- see this module's own doc comment).
pub fn update_amplitude_threshold_q16(errors: &FrameErrorsQ16, previous_tau_m: i64) -> i64 {
    if errors.rate_q16 <= POINT_005_Q16_16 && errors.total <= 6 {
        20480i64 << 16
    } else {
        ((6000i64 - 300 * (errors.total as i64)) << 16) + previous_tau_m
    }
}

/// `gamma_M` (Eq. 116), returning an ordinary Q16.16 `i32` -- the ratio itself is always in
/// `[0, 1]` by construction (the `tau_m > a_m` branch handles the only case it could exceed `1`).
pub fn amplitude_smoothing_scale_q16(tau_m: i64, a_m: i64) -> i32 {
    if tau_m > a_m {
        65536
    } else {
        div_q16_i64(tau_m, a_m)
    }
}
