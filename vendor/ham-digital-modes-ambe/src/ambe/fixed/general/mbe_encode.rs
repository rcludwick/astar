// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::mbe_encode::quantize_speech`: the mode-independent quantization of one
//! frame's MBE speech parameters (D-STAR and AMBE+2 half-rate), the inverse of
//! [`super::mbe_speech::dequantize_speech`]. Same structure and API shape as the float sibling; everything is
//! Q16.16 (`i32`) with `i64` for sums and squared distances.
//!
//! Where the float encoder takes linear amplitudes and takes `ln(m)/0.693` inside, this port takes the target
//! log-amplitudes already in the decoder's log2 domain (Q16.16), computed by [`target_log2_ml_q16`] from a Q16.16
//! linear amplitude (including the decoder's `0.693` constant and the unvoiced scale `0.2046/sqrt(w0)`). The V/UV
//! choice still needs the linear amplitudes as weights, so [`SpeechTarget`] carries both.
//!
//! The prediction terms (`0.65 * interp`, `Sum43`, the `L` resampling) replicate
//! [`super::mbe_speech::dequantize_speech`]'s exact integer expressions, so the encoder inverts the fixed decoder
//! (not merely the float one). The tables are the existing Q16.16 tables (`fixed::dstar::tables_q16`,
//! `fixed::ambe_plus_2::tables_q16`); the voicing patterns and `LMPRBL` are shared integer/bool tables.
//!
//! **Measured against the float encoder**: see `tests/ambe_fixed_mbe_encode.rs` (its module doc records the
//! measured agreement numbers and the characterization of disagreements as near-ties). Sources of disagreement:
//! Q16.16 rounding of the table rows (half an LSB, `7.6e-6`), the table-interpolated `log2`, and `f64` vs. integer
//! floor at V/UV slot boundaries.

use super::explog::log2_q16;
use super::fixed_ops::{mul_q16, TWO_PI_Q16_16};
use super::mbe_speech::MbeDecoderState;
use super::trig::cos_pi_frac;

/// `round(sqrt(2) * 65536)`.
const SQRT2_Q16_16: i32 = 92682;
/// `round(0.2046 * 65536)`, the decoder's unvoiced scale.
const POINT_2046_Q16_16: i32 = 13409;
/// `round(ln(2) / 0.693 * 65536)`: the float encoder computes `ln(m) / 0.693`, i.e. `log2(m)` times this.
const LN2_OVER_POINT_693_Q16_16: i32 = 65550;
/// The float encoder's floor on the amplitude used for the log (`1e-3`), as Q16.16.
const MIN_LOG_AMPLITUDE_Q16_16: i32 = 66;

/// The per-mode tables the quantizer searches (Q16.16 counterparts of the float `ModeTables`).
pub struct ModeTables<'a> {
    pub vuv: &'a [[bool; 8]],
    pub dg_q16: &'a [i32],
    pub prba24_q16: &'a [[i32; 3]],
    pub prba58_q16: &'a [[i32; 4]],
    pub lmprbl: &'a [[u32; 4]],
    pub hoc_q16: [&'a [[i32; 4]]; 4],
    /// D-STAR `b8` only ever carries even indices (its low bit is always 0).
    pub hoc_b8_even_only: bool,
    /// The decoder's predictor weight in Q16.16 (see `mbe_speech::SpeechTables::rho_q16`).
    pub rho_q16: i64,
    /// The decoder's gain recursion in Q16.16 (see `mbe_speech::SpeechTables`).
    pub gamma_scale_q16: i64,
    pub gamma_memory_q16: i64,
}

/// What the frame's analysis wants the decoder to reproduce. `voiced`, `ml_q16` and `log2_ml_q16` are 1-indexed by
/// harmonic (index 0 unused), length `l + 1`.
pub struct SpeechTarget<'a> {
    pub l: u32,
    /// Fundamental for the V/UV slot lookup `jl = floor(h * 16 * f0)`, in cycles/sample, Q16.16.
    pub vuv_f0_q16: i32,
    pub voiced: &'a [bool],
    /// Linear amplitudes (Q16.16), used only as V/UV weights.
    pub ml_q16: &'a [i32],
    /// Target log2 amplitudes in the decoder's domain (Q16.16), from [`target_log2_ml_q16`].
    pub log2_ml_q16: &'a [i32],
}

/// The decoder-side state the recursion predicts from (mirrors [`MbeDecoderState`]).
pub struct PrevState<'a> {
    pub l: u32,
    pub log2_ml_q16: &'a [i32],
    pub gamma_q16: i32,
}

impl<'a> PrevState<'a> {
    pub fn from_decoder_state(state: &'a MbeDecoderState) -> Self {
        Self {
            l: state.l,
            log2_ml_q16: &state.log2_ml_q16,
            gamma_q16: state.gamma_q16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantizedSpeech {
    pub b1: u32,
    pub b2: u32,
    pub b3: u32,
    pub b4: u32,
    pub b5: u32,
    pub b6: u32,
    pub b7: u32,
    pub b8: u32,
}

/// Converts Q16.16 linear amplitudes (`ml_q16`, 1-indexed, index 0 unused) into the decoder's log2 domain:
/// `log2Ml = ln(eff) / 0.693` with `eff = m` for a voiced harmonic and `m / unvc` for an unvoiced one,
/// `unvc = 0.2046 / sqrt(w0)` (`w0_q16` in radians/sample, Q16.16). Amplitudes are floored at `1e-3` like the float.
pub fn target_log2_ml_q16(w0_q16: i32, voiced: &[bool], ml_q16: &[i32]) -> Vec<i32> {
    // log2(m / unvc) = log2(m) + 0.5*log2(w0) - log2(0.2046)
    let unvoiced_boost = (log2_q16(w0_q16.max(1)) >> 1) as i64 - log2_q16(POINT_2046_Q16_16) as i64;
    let mut out = vec![0i32; ml_q16.len()];
    for h in 1..ml_q16.len() {
        let m = ml_q16[h].max(MIN_LOG_AMPLITUDE_Q16_16);
        let mut lg = log2_q16(m) as i64;
        if !voiced[h] {
            lg += unvoiced_boost;
        }
        out[h] = ((lg * LN2_OVER_POINT_693_Q16_16 as i64) >> 16) as i32;
    }
    out
}

fn prev_at(log2_ml_q16: &[i32], idx: usize) -> i64 {
    // mbelib sets the previous frame's log2Ml[0] to log2Ml[1].
    let idx = if idx == 0 { 1 } else { idx };
    log2_ml_q16
        .get(idx)
        .copied()
        .unwrap_or_else(|| *log2_ml_q16.last().unwrap_or(&0)) as i64
}

fn nearest<const N: usize>(
    table: &[[i32; N]],
    target: &[i32; N],
    used: usize,
    only_even: bool,
) -> u32 {
    let mut best = (i64::MAX, 0usize);
    for (i, row) in table.iter().enumerate() {
        if only_even && i % 2 == 1 {
            continue;
        }
        let d: i64 = (0..used)
            .map(|k| {
                let e = row[k] as i64 - target[k] as i64;
                e * e
            })
            .sum();
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1 as u32
}

/// The decoder's V/UV slot for harmonic `h`: `floor(h * 16 * f0)` computed exactly as `dequantize_speech` does.
fn voicing_slot(h: usize, f0_q16: i32) -> usize {
    let jl_q16 = mul_q16((h as i32) << 16, f0_q16).saturating_mul(16);
    ((jl_q16 >> 16).max(0) as usize).min(7)
}

/// Fixed-point `quantize_speech`; see the module doc and the float sibling for the algorithm.
pub fn quantize_speech(
    target: &SpeechTarget,
    prev: &PrevState,
    tables: &ModeTables,
) -> QuantizedSpeech {
    let l = target.l as usize;
    let l64 = l as i64;

    // b1: the V/UV pattern agreeing best with the target, weighted by amplitude.
    let (mut b1_score, mut b1_row) = (i64::MIN, 0usize);
    for (row_idx, row) in tables.vuv.iter().enumerate() {
        let mut score = 0i64;
        for h in 1..=l {
            let w = (target.ml_q16[h] as i64).max(1);
            if row[voicing_slot(h, target.vuv_f0_q16)] == target.voiced[h] {
                score += w;
            } else {
                score -= w;
            }
        }
        if score > b1_score {
            (b1_score, b1_row) = (score, row_idx);
        }
    }

    // The decoder's own prediction terms, with its exact integer rounding.
    let prev_l = prev.l.max(1) as i64;
    let mut pred = vec![0i64; l + 1];
    let mut sum43 = 0i64;
    for (h, slot) in pred.iter_mut().enumerate().skip(1) {
        let f_q16 = ((prev_l * h as i64) << 16) / l64;
        let ik = (f_q16 >> 16).max(0) as usize;
        let delta = f_q16 - ((ik as i64) << 16);
        let one_minus_delta = 65536 - delta;
        let (p0, p1) = (
            prev_at(prev.log2_ml_q16, ik),
            prev_at(prev.log2_ml_q16, ik + 1),
        );
        sum43 += one_minus_delta * p0 + delta * p1;
        *slot =
            ((tables.rho_q16 * one_minus_delta * p0) >> 32) + ((tables.rho_q16 * delta * p1) >> 32);
    }
    let sum43_scaled = (((sum43 >> 16) * tables.rho_q16) >> 16) / l64;

    // x = target log2Ml - pred; its mean sets the gain, the zero-mean remainder is Tl.
    let x: Vec<i64> = (0..=l)
        .map(|h| {
            if h == 0 {
                0
            } else {
                target.log2_ml_q16[h] as i64 - pred[h]
            }
        })
        .collect();
    let mean_x = x[1..=l].iter().sum::<i64>() / l64;
    let gamma_target = mean_x + sum43_scaled + (log2_q16((l as i32) << 16) >> 1) as i64;
    let delta_target = (((gamma_target - ((prev.gamma_q16 as i64 * tables.gamma_memory_q16) >> 16))
        as i128)
        << 16)
        / tables.gamma_scale_q16 as i128;
    let delta_target = delta_target as i64;
    let (mut b2_dist, mut b2_idx) = (i64::MAX, 0usize);
    for (i, &dg) in tables.dg_q16.iter().enumerate() {
        let d = (dg as i64 - delta_target).abs();
        if d < b2_dist {
            (b2_dist, b2_idx) = (d, i);
        }
    }

    // Zero-mean Tl split into four blocks and DCT'd.
    let tl: Vec<i64> = (0..=l)
        .map(|h| if h == 0 { 0 } else { x[h] - mean_x })
        .collect();
    let ji = tables.lmprbl[l];
    let mut cik = [[0i32; 18]; 5];
    let mut start = 1usize;
    for block in 0..4 {
        let j_len = ji[block] as usize;
        for (k, cik_k) in cik[block + 1]
            .iter_mut()
            .enumerate()
            .take(j_len.min(17) + 1)
            .skip(1)
        {
            let mut sum = 0i64;
            for j in 1..=j_len {
                if start + j - 1 <= l {
                    let c = cos_pi_frac(k as i64 - 1, 2 * j as i64 - 1, j_len as i64);
                    sum += tl[start + j - 1] * c as i64;
                }
            }
            *cik_k = ((sum / j_len as i64) >> 16) as i32;
        }
        start += j_len;
    }

    // Ri from each block's first two coefficients, then Gm by the forward 8-point cosine transform.
    let mut ri = [0i32; 9];
    for block in 1..=4usize {
        let (c1, c2) = (cik[block][1], mul_q16(SQRT2_Q16_16, cik[block][2]));
        ri[2 * block - 1] = c1 + c2;
        ri[2 * block] = c1 - c2;
    }
    let mut gm = [0i32; 9];
    for (m, slot) in gm.iter_mut().enumerate().skip(2) {
        let sum: i64 = (1..=8usize)
            .map(|i| ri[i] as i64 * cos_pi_frac(m as i64 - 1, 2 * i as i64 - 1, 8) as i64)
            .sum();
        *slot = (sum >> 19) as i32; // >> 16 for the cosine's Q16.16, / 8 for the transform
    }
    let b3 = nearest(tables.prba24_q16, &[gm[2], gm[3], gm[4]], 3, false);
    let b4 = nearest(tables.prba58_q16, &[gm[5], gm[6], gm[7], gm[8]], 4, false);

    // Higher-order coefficients: the decoder reads C[block][3..=min(J, 6)].
    let mut hoc_idx = [0u32; 4];
    for block in 0..4 {
        let j_len = ji[block] as usize;
        let used = j_len.saturating_sub(2).min(4);
        let tgt = [
            cik[block + 1][3],
            cik[block + 1][4],
            cik[block + 1][5],
            cik[block + 1][6],
        ];
        hoc_idx[block] = if used == 0 {
            0
        } else {
            nearest(
                tables.hoc_q16[block],
                &tgt,
                used,
                block == 3 && tables.hoc_b8_even_only,
            )
        };
    }

    QuantizedSpeech {
        b1: b1_row as u32,
        b2: b2_idx as u32,
        b3,
        b4,
        b5: hoc_idx[0],
        b6: hoc_idx[1],
        b7: hoc_idx[2],
        b8: hoc_idx[3],
    }
}

/// `w0` (radians/sample, Q16.16) for a fundamental `f0` (cycles/sample, Q16.16), for callers building a target.
pub fn w0_from_f0_q16(f0_q16: i32) -> i32 {
    mul_q16(f0_q16, TWO_PI_Q16_16)
}
