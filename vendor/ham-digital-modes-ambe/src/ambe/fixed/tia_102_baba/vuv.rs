// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point voiced/unvoiced determination (TIA-102.BABA_2003.pdf section 5.2, Eq. 31-42):
//! fixed-point sibling of [`crate::ambe::float::tia_102_baba::vuv`], built on
//! [`super::pitch_refinement`]'s exact-fraction band arithmetic ([`Pitch`]) and spectrum error sums.
//!
//! # Numeric formats
//!
//! * `xi_LF`, `xi_HF`, `xi_0`, `xi_max`: `i128` in Q16 (`sum |S_w|^2 / W_R(0)^2`, in the input's own
//!   squared PCM units, so the spec's absolute `20000` floor applies unchanged). A full-scale input
//!   stays below `2^54` in this format.
//! * The energy-dependent function `M(xi)` and the thresholds `Theta`: Q30 (`i64`).
//! * The band voicing measure `D_k`: Q30, computed as `error / real` from two Q60 `i128` energies
//!   (both shifted down together first if the denominator would leave headroom short).
//! * `omega0`: Q30 inside [`Pitch`]; the thresholds' `0.3096 (k - 1) omega0` term is an integer
//!   product against the constant 3096/10000, and the spec's 0.5625 and 0.45 factors are the exact
//!   fractions 9/16 and 9/20.
//!
//! The comparison `D_k < Theta_k` is made on the Q30 values directly, so it differs from the float
//! sibling only where the two are within about `2^-30` relative of each other, or through the input
//! quantisation of `S_w` (Q10) and the harmonic amplitudes (Q16); measured agreement on real speech
//! is stated in `tests/ambe_fixed_tia_102_baba_vuv_amplitude.rs`.

use super::pitch_refinement::{
    spectrum_error_and_energy, window_dft_16384_q22, Pitch, RefinementFrame,
};
use crate::ambe::fixed::general::isqrt::isqrt_u64;

/// `20000` in Q16: the spec's floor on `xi_max` (Eq. 41).
pub const XI_MAX_FLOOR_Q16: i128 = 20000 << 16;
/// The initial `xi_max` of a stream (the floor).
pub const XI_MAX_INITIAL_Q16: i128 = XI_MAX_FLOOR_Q16;

const ONE_Q30: i64 = 1 << 30;
/// `0.5` in Q16, the initial-pitch-error limit above which bands 2.. are forced unvoiced.
const HALF_Q16: i32 = 1 << 15;

/// `L_hat` from Eq. 31.
pub fn harmonics_count(pitch: &Pitch) -> u32 {
    pitch.harmonics_count()
}

/// `K_hat`, the number of frequency bands (Eq. 34).
pub fn frequency_bands_count(l_hat: u32) -> u32 {
    if l_hat <= 36 {
        l_hat.div_ceil(3)
    } else {
        12
    }
}

/// `sum |S_w(m)|^2 / W_R(0)^2` over bins `lo..=hi`, Q16.
fn xi_over(frame: &RefinementFrame, lo: i32, hi: i32) -> i128 {
    let wr0 = window_dft_16384_q22(0) as i128;
    let mut sum = 0i128; // Q20
    for m in lo..=hi {
        sum += frame.sw_at(m).norm_sqr();
    }
    (sum << 40) / (wr0 * wr0)
}

/// `xi_LF` (Eq. 38), Q16.
pub fn xi_lf(frame: &RefinementFrame) -> i128 {
    xi_over(frame, 0, 63)
}

/// `xi_HF` (Eq. 39), Q16.
pub fn xi_hf(frame: &RefinementFrame) -> i128 {
    xi_over(frame, 64, 128)
}

/// `xi_max` update (Eq. 41), Q16.
pub fn update_xi_max(xi_max_prev: i128, xi_0: i128) -> i128 {
    if xi_0 > xi_max_prev {
        (xi_max_prev + xi_0) / 2
    } else {
        let decayed = (99 * xi_max_prev + xi_0) / 100;
        decayed.max(XI_MAX_FLOOR_Q16)
    }
}

/// `M(xi)` (Eq. 42) in Q30.
pub fn energy_dependent_function_q30(xi_max: i128, xi_0: i128, xi_lf: i128, xi_hf: i128) -> i64 {
    // (0.0025 xi_max + xi_0) / (0.01 xi_max + xi_0), scaled by 400 top and bottom.
    let num = xi_max + 400 * xi_0;
    let den = 4 * xi_max + 400 * xi_0;
    let base = ((num << 30) / den) as i64;
    if xi_lf >= 5 * xi_hf {
        base
    } else {
        // sqrt(xi_lf / (5 xi_hf)) < 1, as a Q30 value: sqrt(ratio_q60) = sqrt(ratio) * 2^30.
        let ratio_q60 = ((xi_lf << 60) / (5 * xi_hf)) as u64;
        let root_q30 = isqrt_u64(ratio_q60) as i128;
        ((base as i128 * root_q30) >> 30) as i64
    }
}

/// `Theta_xi(k)` (Eq. 37) in Q30. `initial_pitch_error_q16` is `E(P_hat_I)`.
pub fn voicing_threshold_q30(
    k: u32,
    omega0_q30: i64,
    initial_pitch_error_q16: i32,
    previous_band_voiced: bool,
    m_xi_q30: i64,
) -> i64 {
    if initial_pitch_error_q16 > HALF_Q16 && k >= 2 {
        return 0;
    }
    let drop = ((k as i128 - 1) * omega0_q30 as i128 * 3096) / 10000;
    let factor = ONE_Q30 as i128 - drop;
    let scaled = (m_xi_q30 as i128 * factor) >> 30;
    if previous_band_voiced {
        ((scaled * 9) >> 4) as i64
    } else {
        ((scaled * 9) / 20) as i64
    }
}

/// `error / real` as Q30, for non-negative Q60 `i128` energies with `real > 0`; shifts both down
/// together so the scaled numerator cannot overflow.
fn ratio_q30(err: i128, real: i128) -> i64 {
    let bits = 128 - real.leading_zeros() as i32;
    let shift = (bits - 90).max(0);
    let (e, r) = (err >> shift, real >> shift);
    if r == 0 {
        return ONE_Q30;
    }
    ((e << 30) / r).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// The band voicing measure `D_k` (Eq. 35/36) in Q30. A band with no spectral energy counts as
/// maximally unvoiced (1.0), as in the float sibling.
pub fn voicing_measure_q30(
    frame: &RefinementFrame,
    k: u32,
    l_hat: u32,
    pitch: &Pitch,
    is_highest_band: bool,
) -> i64 {
    let m_lo = pitch.band_start(3 * k as i32 - 2);
    let upper_l = if is_highest_band { l_hat } else { 3 * k };
    let m_hi = pitch.band_start(upper_l as i32 + 1);
    let (err, real) = spectrum_error_and_energy(frame, pitch, l_hat, m_lo, m_hi);
    if real <= 0 {
        return ONE_Q30;
    }
    ratio_q30(err, real)
}

/// Per-band voicing decisions and the updated `xi_max` (Q16). `previous_v` are the previous frame's
/// band decisions (empty for the first frame).
pub fn determine_voicing(
    frame: &RefinementFrame,
    pitch: &Pitch,
    initial_pitch_error_q16: i32,
    xi_max_prev_q16: i128,
    previous_v: &[bool],
) -> (Vec<bool>, i128) {
    let l_hat = harmonics_count(pitch);
    let k_hat = frequency_bands_count(l_hat);

    let lf = xi_lf(frame);
    let hf = xi_hf(frame);
    let xi_0 = lf + hf;
    let xi_max = update_xi_max(xi_max_prev_q16, xi_0);
    let m_xi = energy_dependent_function_q30(xi_max, xi_0, lf, hf);

    let voiced = (1..=k_hat)
        .map(|k| {
            let d_k = voicing_measure_q30(frame, k, l_hat, pitch, k == k_hat);
            let previous = previous_v.get((k - 1) as usize).copied().unwrap_or(false);
            let theta = voicing_threshold_q30(
                k,
                pitch.omega0_q30(),
                initial_pitch_error_q16,
                previous,
                m_xi,
            );
            d_k < theta
        })
        .collect();
    (voiced, xi_max)
}
