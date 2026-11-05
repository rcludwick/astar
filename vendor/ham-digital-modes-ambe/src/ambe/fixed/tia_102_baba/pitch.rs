// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point TIA-102.BABA pitch estimation (TIA-102.BABA_2003.pdf section 5.1): the per-frame error
//! function `E(P)`, look-back and look-ahead tracking, and the initial-estimate decision. Fixed-point
//! sibling of [`crate::ambe::float::tia_102_baba::pitch`]; see [`super::super`]'s doc comment for the
//! numeric convention.
//!
//! # Representation
//!
//! * **Pitch** is a *candidate index* `0..=202` (`P = 21 + 0.5 * index`, so `2P = 42 + index`).
//!   Every candidate pitch is a multiple of half a sample, so the float code's `0.8 * P` / `1.2 * P`
//!   search bounds become the exact integer tests `4 * P2 <= 5 * c2 <= 6 * P2`, and its `n * P`
//!   half-sample lags become exact integer lag arithmetic.
//! * **Input samples** are plain integers (16-bit PCM scale, `i32` container).
//! * **`E(P)`** is Q16.16 in an `i32`.
//! * **Thresholds** (0.48, 0.85, 1.7, 0.4, 3.5, 0.05) are Q16.16 built by [`q16_ratio`].
//!
//! # Numerics of `E(P)`
//!
//! `E(P) = (S - P * Rsum) / (S * (1 - P * W4))` (float sibling's `error_function`) is invariant to
//! input scale, so the lowpass-filtered frame is block-normalised: its peak is shifted to
//! `[2^21, 2^22)`. With that, `v = s*w` (peak below `2^19`) gives `S = sum v^2 < 2^46`, and
//! `a = s*w^2` in Q10 (peak below `2^26`) gives the integer-lag autocorrelation table
//! `R[t] = sum a(j) a(j+t)` below `2^59` (fits `i64`). `r(t)` at a half-sample lag is the mean of the
//! two neighbouring integer lags, exactly the float sibling's linear interpolation. `Rsum` is
//! accumulated in `i128` (a sum of up to 15 terms each bounded by `R[0]`, which would come within a
//! factor of 2 of `i64::MAX`), then `x = Rsum / S` is formed in Q30 (`i128` numerator) and the final
//! quotient in Q16.16. The autocorrelation table is built once per frame (about 34 thousand multiply
//! -accumulates) rather than re-summed per candidate as the float sibling does, so the whole
//! 203-entry error table costs a small constant on top.
//!
//! # Verification (see `tests/ambe_fixed_tia_102_baba_pitch.rs`)
//!
//! Measured against the float sibling on real speech (all four `tests/fixtures/osr_speech` files,
//! 7748 frames at a 160-sample stride): `E(P)` over all 203 candidates has maximum absolute error
//! 0.00002 (maximum relative error 0.00011 where `|E| > 0.1`); the initial pitch estimate (candidate
//! index) agrees on 99.87% of frames both end to end (each side with its own look-back history) and
//! frame by frame given the float side's history (look-back alone 99.83%, look-ahead alone 99.87%).
//! The disagreements are half-sample ties between near-equal errors.

use super::pitch_tables::{
    INITIAL_WINDOW_FOURTH_SUM_Q40, INITIAL_WINDOW_Q31, INITIAL_WINDOW_SQ_Q40, LOWPASS_Q30,
};

/// Number of candidate pitches (`21.0, 21.5, ..., 122.0`).
pub const CANDIDATES: usize = 203;
/// `2 * 21`: `2P` of candidate index 0.
const P2_BASE: i64 = 42;
/// The spec's default previous pitch (100.0) as a candidate index.
pub const DEFAULT_PITCH_INDEX: usize = 158;

const ONE_Q16: i64 = 1 << 16;

/// `round(num / den * 65536)` for positive `num`, `den`.
pub const fn q16_ratio(num: i64, den: i64) -> i32 {
    ((num * ONE_Q16 * 2 + den) / (den * 2)) as i32
}

const T_0_48: i64 = q16_ratio(48, 100) as i64;
const T_0_85: i64 = q16_ratio(85, 100) as i64;
const T_1_7: i64 = q16_ratio(17, 10) as i64;
const T_0_4: i64 = q16_ratio(4, 10) as i64;
const T_3_5: i64 = q16_ratio(35, 10) as i64;
const T_0_05: i64 = q16_ratio(5, 100) as i64;

/// Candidate index to `2P` (the pitch in half samples).
pub const fn p2_of_index(idx: usize) -> i64 {
    P2_BASE + idx as i64
}

fn lowpass_at(n: i32) -> i64 {
    LOWPASS_Q30[n.unsigned_abs() as usize] as i64
}

/// One frame's pitch-analysis state: the normalised lowpass-filtered samples, `S`, and the integer-lag
/// autocorrelation of `s * w^2`.
pub struct PitchAnalysisFrame {
    /// `sum (s*w)^2` over the 301-tap window (block-normalised units).
    s_energy: i64,
    /// `R[t]`, `t = 0..=150`.
    autocorr: [i64; 151],
    silent: bool,
}

impl PitchAnalysisFrame {
    /// `raw` is PCM; the window is centred on `raw[center]` and reads `center-160..=center+160`.
    pub fn new(raw: &[i32], center: usize) -> Self {
        // Eq. 9 lowpass, Q30 accumulator (|acc| < 2^46).
        let mut acc = [0i64; 301];
        let mut peak = 0u64;
        for (i, slot) in acc.iter_mut().enumerate() {
            let n = i as i32 - 150;
            let mut sum = 0i64;
            for j in -10i32..=10 {
                let idx = (center as i32 + n - j) as usize;
                sum += raw[idx] as i64 * lowpass_at(j);
            }
            *slot = sum;
            peak = peak.max(sum.unsigned_abs());
        }
        if peak == 0 {
            return Self {
                s_energy: 0,
                autocorr: [0; 151],
                silent: true,
            };
        }
        // Block-normalise: peak into [2^21, 2^22).
        let bits = 64 - peak.leading_zeros() as i32;
        let shift = bits - 22;
        let mut s = [0i64; 301];
        for (dst, &src) in s.iter_mut().zip(acc.iter()) {
            *dst = if shift >= 0 {
                (src + ((1i64 << shift) >> 1)) >> shift
            } else {
                src << (-shift)
            };
        }
        let mut s_energy = 0i64;
        let mut a = [0i64; 301];
        for i in 0..301 {
            let mag = (i as i32 - 150).unsigned_abs() as usize;
            let v = (s[i] * INITIAL_WINDOW_Q31[mag] as i64 + (1 << 30)) >> 31;
            s_energy += v * v;
            a[i] = (s[i] * INITIAL_WINDOW_SQ_Q40[mag] + (1 << 29)) >> 30;
        }
        let mut autocorr = [0i64; 151];
        for (t, slot) in autocorr.iter_mut().enumerate() {
            let mut sum = 0i64;
            for j in 0..301 - t {
                sum += a[j] * a[j + t];
            }
            *slot = sum;
        }
        Self {
            s_energy,
            autocorr,
            silent: false,
        }
    }

    /// `E(P)` for candidate index `idx` (Q16.16). All-zero input scores the worst error, 1.0, as the
    /// float sibling does.
    pub fn error_function(&self, idx: usize) -> i32 {
        if self.silent || self.s_energy == 0 {
            return ONE_Q16 as i32;
        }
        let p2 = p2_of_index(idx);
        let n_max = 300 / p2;
        let mut rsum: i128 = self.autocorr[0] as i128;
        for n in 1..=n_max {
            let lag2 = n * p2;
            let lo = (lag2 / 2) as usize;
            let two_r = if lag2 % 2 == 0 {
                2 * self.autocorr[lo]
            } else {
                self.autocorr[lo] + self.autocorr[lo + 1]
            };
            rsum += two_r as i128;
        }
        // x = Rsum / S in Q30; R is in (Q10)^2 = 2^-20 units relative to S.
        let x_q30 = (rsum << 10) / self.s_energy as i128;
        let p_x = (p2 as i128 * x_q30) >> 1;
        let num = (1i128 << 30) - p_x;
        let den = (1i128 << 30) - ((p2 as i128 * INITIAL_WINDOW_FOURTH_SUM_Q40 as i128) >> 11);
        if den == 0 {
            return ONE_Q16 as i32;
        }
        let scaled = num << 16;
        let half = den.abs() >> 1;
        let q = if (scaled >= 0) == (den > 0) {
            (scaled.abs() + half) / den.abs()
        } else {
            -((scaled.abs() + half) / den.abs())
        };
        // Floored at zero like the float sibling (see its `error_function`).
        q.clamp(0, i32::MAX as i128) as i32
    }

    /// `E(P)` for every candidate.
    pub fn error_table(&self) -> ErrorTable {
        let mut t = [0i32; CANDIDATES];
        for (i, slot) in t.iter_mut().enumerate() {
            *slot = self.error_function(i);
        }
        ErrorTable(t)
    }
}

/// `E(P)` (Q16.16) for the 203 candidate pitches of one frame.
#[derive(Clone)]
pub struct ErrorTable(pub [i32; CANDIDATES]);

impl ErrorTable {
    pub fn at(&self, idx: usize) -> i32 {
        self.0[idx.min(CANDIDATES - 1)]
    }
}

/// Candidate indices `c` with `0.8 * P_ref <= P_c <= 1.2 * P_ref`, evaluated exactly in half samples.
fn tracking_range(ref_idx: usize) -> std::ops::RangeInclusive<usize> {
    let r2 = p2_of_index(ref_idx);
    let mut lo = CANDIDATES;
    let mut hi = 0usize;
    for c in 0..CANDIDATES {
        let c2 = p2_of_index(c);
        if 5 * c2 >= 4 * r2 && 5 * c2 <= 6 * r2 {
            lo = lo.min(c);
            hi = hi.max(c);
        }
    }
    lo..=hi
}

/// Look-back tracking (5.1.2): returns `(index of P_hat_B, CE_B)`. `prev1`/`prev2` are the previous
/// two frames' final `(pitch index, E)`; the defaults are `(DEFAULT_PITCH_INDEX, 0)`.
pub fn look_back_pitch_tracking(
    table: &ErrorTable,
    prev1: (usize, i32),
    prev2: (usize, i32),
) -> (usize, i32) {
    let (prev_idx, prev_err) = prev1;
    let (_, prev_err_older) = prev2;
    let mut best_idx = 0usize;
    let mut best_err = i32::MAX;
    for c in tracking_range(prev_idx) {
        let e = table.at(c);
        if e < best_err {
            best_idx = c;
            best_err = e;
        }
    }
    let ce = (best_err as i64 + prev_err as i64 + prev_err_older as i64)
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    (best_idx, ce)
}

/// Look-ahead tracking (5.1.3, including the smallest-sub-multiple-first order and the multiplied-out
/// ratio tests of the float sibling): returns `(index of P_hat_F, CE_F)`.
pub fn look_ahead_pitch_tracking(
    t0: &ErrorTable,
    t1: &ErrorTable,
    t2: &ErrorTable,
) -> (usize, i32) {
    // best_e2[p1] = min E2(p2) over p2 in range(p1); best_e12[p0] = min over p1 in range(p0) of
    // E1(p1) + best_e2[p1]. Equivalent to the float sibling's nested minimisation.
    let mut best_e2 = [0i64; CANDIDATES];
    for (p1, slot) in best_e2.iter_mut().enumerate() {
        *slot = tracking_range(p1)
            .map(|c| t2.at(c) as i64)
            .min()
            .unwrap_or(i64::MAX / 4);
    }
    let mut best_e12 = [0i64; CANDIDATES];
    for (p0, slot) in best_e12.iter_mut().enumerate() {
        *slot = tracking_range(p0)
            .map(|c| t1.at(c) as i64 + best_e2[c])
            .min()
            .unwrap_or(i64::MAX / 4);
    }
    let ce_f_at = |p0: usize| -> i64 { t0.at(p0) as i64 + best_e12[p0] };

    let mut p_hat_0 = 0usize;
    let mut best = i64::MAX;
    for p0 in 0..CANDIDATES {
        let ce = ce_f_at(p0);
        if ce < best {
            best = ce;
            p_hat_0 = p0;
        }
    }
    let ce_f_p_hat_0 = ce_f_at(p_hat_0);

    // Sub-multiples P_hat_0 / n (n = 2, 3, ...) while >= 21, snapped to the half-sample grid; tested
    // smallest pitch (largest n) first.
    let p2_0 = p2_of_index(p_hat_0);
    let mut n_top = 1i64;
    while p2_0 >= P2_BASE * (n_top + 1) {
        n_top += 1;
    }
    for n in (2..=n_top).rev() {
        let snapped2 = (2 * p2_0 + n) / (2 * n);
        let cand = (snapped2 - P2_BASE).clamp(0, CANDIDATES as i64 - 1) as usize;
        let ce = ce_f_at(cand);
        let ratio_ok = |limit_q16: i64| ce * ONE_Q16 <= limit_q16 * ce_f_p_hat_0;
        let satisfies_18 = ce <= T_0_85 && ratio_ok(T_1_7);
        let satisfies_19 = ce <= T_0_4 && ratio_ok(T_3_5);
        let satisfies_20 = ce <= T_0_05;
        if satisfies_18 || satisfies_19 || satisfies_20 {
            return (cand, ce.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
        }
    }
    (
        p_hat_0,
        ce_f_p_hat_0.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    )
}

/// 5.1.4: choose between the backward and forward estimates; returns the pitch index.
pub fn choose_initial_pitch_estimate(idx_b: usize, ce_b: i32, idx_f: usize, ce_f: i32) -> usize {
    if ce_b as i64 <= T_0_48 || ce_b <= ce_f {
        idx_b
    } else {
        idx_f
    }
}
