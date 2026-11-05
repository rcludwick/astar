// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point spectral amplitude estimation (TIA-102.BABA_2003.pdf section 5.3, Eq. 43-44):
//! fixed-point sibling of [`crate::ambe::float::tia_102_baba::spectral_amplitude`].
//!
//! Amplitudes are returned as `i64` in Q16 (real amplitude times 65536): a full-scale 16-bit sinusoid
//! is about `3.3e4` times `2^16`, beyond a plain `i32` Q16.16, hence the wide type. Energies come from
//! [`super::pitch_refinement::RefinementFrame`] (`|S_w|^2` in Q20 as `i128`) and the roots use
//! [`crate::ambe::fixed::general::isqrt::isqrt_u128`], so no precision is lost narrowing before the
//! square root.
//!
//! * Voiced (Eq. 43): `sqrt(E_sig / E_win)` with `E_win = sum W_R^2` (Q44): `amp_q16 =
//!   isqrt(E_sig_q20 * 2^56 / E_win_q44)`.
//! * Unvoiced (Eq. 44): `sqrt(E_sig / width) / sum(w_R)`, where `sum(w_R)` equals `W_R(0)` (the
//!   window's own DC gain, Q22): `amp_q16 = isqrt(E_sig_q20 * 2^12 / width) * 2^22 / W_R(0)_q22`.

use super::pitch_refinement::{window_dft_16384_q22, Pitch, RefinementFrame};
use crate::ambe::fixed::general::isqrt::isqrt_u128;

/// `A_voiced(l)` (Eq. 43), Q16.
pub fn voiced_amplitude_q16(frame: &RefinementFrame, l: u32, pitch: &Pitch) -> i64 {
    let (lo, hi) = (pitch.band_start(l as i32), pitch.band_start(l as i32 + 1));
    let mut signal_energy = 0i128; // Q20
    let mut window_energy = 0i128; // Q44
    for m in lo..hi {
        signal_energy += frame.sw_at(m).norm_sqr();
        let wr = window_dft_16384_q22(pitch.window_index(m, l as i32)) as i128;
        window_energy += wr * wr;
    }
    if window_energy == 0 {
        return 0;
    }
    isqrt_u128(((signal_energy << 56) / window_energy) as u128) as i64
}

/// `A_unvoiced(l)` (Eq. 44), Q16.
pub fn unvoiced_amplitude_q16(frame: &RefinementFrame, l: u32, pitch: &Pitch) -> i64 {
    let (lo, hi) = (pitch.band_start(l as i32), pitch.band_start(l as i32 + 1));
    let width = (hi - lo) as i128;
    if width <= 0 {
        return 0;
    }
    let mut signal_energy = 0i128; // Q20
    for m in lo..hi {
        signal_energy += frame.sw_at(m).norm_sqr();
    }
    let root = isqrt_u128(((signal_energy << 12) / width) as u128) as i128;
    ((root << 22) / window_dft_16384_q22(0) as i128) as i64
}

/// `M_hat_l` for `l = 1..=l_hat` (Q16): the voiced or unvoiced estimator per the band's decision.
pub fn estimate_spectral_amplitudes_q16(
    frame: &RefinementFrame,
    l_hat: u32,
    k_hat: u32,
    pitch: &Pitch,
    voiced: &[bool],
) -> Vec<i64> {
    (1..=l_hat)
        .map(|l| {
            let k = l.div_ceil(3).min(k_hat).max(1);
            let is_voiced = voiced.get((k - 1) as usize).copied().unwrap_or(false);
            if is_voiced {
                voiced_amplitude_q16(frame, l, pitch)
            } else {
                unvoiced_amplitude_q16(frame, l, pitch)
            }
        })
        .collect()
}
