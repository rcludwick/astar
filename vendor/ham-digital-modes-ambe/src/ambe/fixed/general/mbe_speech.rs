// SPDX-License-Identifier: LGPL-3.0-or-later
//! The shared MBE "speech frame" dequantization core -- fixed-point port of the mbelib-derived
//! `dequantize` body both `ambe::float::ambe_plus_2::decode` and `ambe::float::dstar::decode` carry
//! almost line-for-line identically (same PRBA -> Gm -> Ri 8-point cosine sum, Cik construction, HOC
//! lookup, per-block inverse DCT into `Tl`, and the `Sum42`/`Sum43`/`BigGamma` log-domain amplitude
//! recursion). Kept in one place here rather than duplicated per mode, the same reasoning
//! [`super::super::super::general`] applies to the FEC layer -- except this code genuinely needs a
//! fixed-point rewrite (the float sibling is real floating-point math), so it lives under `fixed`
//! rather than being reused unchanged.
//!
//! Each mode's own `b0`-keyed table lookups (`L_TABLE`/`W0_TABLE` or D-STAR's own formula) and
//! frame-kind dispatch (`Erasure`/`Silence`/`Tone`/`CallProgress` handling, which genuinely differs
//! per mode) stay in that mode's own `ambe::fixed::<mode>::decode` -- this module only covers the
//! `FrameKind::Speech` case, taking `l`/`w0` already resolved by the caller.

use super::explog::exp2_q16;
use super::fixed_ops::{div_q16, mul_q16, TWO_PI_Q16_16};
use super::trig::cos_pi_frac;

/// `round(0.65 * 65536)` -- the `Sum43`/`log2_Ml` recursion's own fixed blend-weight constant
/// (mbelib's real `.65` literal, `AMBE_CHIP_VALIDATION_FINDINGS.md`'s own transcription history).
pub const POINT_65_Q16_16: i64 = 42598;
/// Gain-recursion constants in Q16.16: scale 1 and 2, memory 0.5 and 0.
pub const GAMMA_SCALE_1_Q16_16: i64 = 65536;
pub const GAMMA_SCALE_2_Q16_16: i64 = 131072;
pub const GAMMA_MEMORY_HALF_Q16_16: i64 = 32768;
pub const GAMMA_MEMORY_ZERO_Q16_16: i64 = 0;
/// `round(0.8 * 65536)`, the D-STAR predictor weight (`float::dstar::decode::PREDICTOR_RHO`).
pub const POINT_80_Q16_16: i64 = 52429;
/// `round(0.2046 * 65536)` -- the unvoiced scaling constant (mbelib's real `unvc` formula).
const POINT_2046_Q16_16: i32 = 13409;

/// The nine raw parameter indices needed by [`dequantize_speech`] -- `b0` itself is not included
/// since the caller has already used it to resolve `l`/`w0` before calling this function.
pub struct RawSpeechParameters {
    pub b1: u32,
    pub b2: u32,
    pub b3: u32,
    pub b4: u32,
    pub b5: u32,
    pub b6: u32,
    pub b7: u32,
    pub b8: u32,
}

/// Every mode-specific table [`dequantize_speech`] needs, all already in this module's own Q16.16
/// convention (generated from the mode's real float tables, not re-derived formulas -- see each
/// mode's own `tables_q16.rs` generator for the exact provenance). `prba24`/`prba58` and the four
/// `hoc_b*` tables are kept as separate fields (not one array) since their own row widths differ (3,
/// 4, and 4 columns respectively).
pub struct SpeechTables<'a> {
    pub vuv: &'a [[bool; 8]],
    pub dg_q16: &'a [i32],
    pub prba24_q16: &'a [[i32; 3]],
    pub prba58_q16: &'a [[i32; 4]],
    /// `LMPRBL[l]`'s own four per-block DCT lengths -- pure integers in every mode, reused as-is.
    pub lmprbl: &'a [[u32; 4]],
    pub hoc_b5_q16: &'a [[i32; 4]],
    pub hoc_b6_q16: &'a [[i32; 4]],
    pub hoc_b7_q16: &'a [[i32; 4]],
    pub hoc_b8_q16: &'a [[i32; 4]],
    /// The amplitude predictor's weight in Q16.16: `POINT_65_Q16_16` for AMBE+2, `POINT_80_Q16_16` for D-STAR (the chip's value).
    pub rho_q16: i64,
    /// The gain recursion `gamma = scale*DG + memory*gamma_prev` in Q16.16 (`float::dstar::decode::GAMMA_SCALE`).
    pub gamma_scale_q16: i64,
    pub gamma_memory_q16: i64,
}

/// Persistent decoder state across frames, generic over every MBE-family mode -- the fixed-point
/// equivalent of `ambe_plus_2::decode::DecoderState`/`dstar::decode::DStarDecoderState` (which are
/// themselves identical in shape, just duplicated per mode in the float tree).
pub struct MbeDecoderState {
    pub l: u32,
    pub log2_ml_q16: Vec<i32>,
    pub gamma_q16: i32,
}

impl MbeDecoderState {
    /// A reasoned initial state for the very first frame -- see the float sibling's own
    /// `DecoderState::initial`/`DStarDecoderState::initial` doc comments for why a flat, constant
    /// starting point is low-stakes here (this recursion's own gain term is a frame-to-frame
    /// difference, not an absolute value).
    pub fn initial() -> Self {
        MbeDecoderState {
            l: 9,
            log2_ml_q16: vec![0; 10],
            gamma_q16: 0,
        }
    }
}

/// One frame's own real, dequantized semantic parameters -- the `FrameKind::Speech` case only (the
/// caller handles `Erasure`/`Silence`/`Tone`/mode-specific special frames itself).
pub struct SpeechParameters {
    pub l: u32,
    pub w0_q16: i32,
    /// `w0` at Q32 radians/sample (`2^32` per radian), the precision synthesis needs -- see
    /// `voiced_synthesis`. `w0_q16` stays for everything that only needs Q16.16 (amplitude scaling).
    pub w0_q32: i64,
    pub voiced: Vec<bool>,
    pub ml_q16: Vec<i32>,
}

/// Reads `log2_ml_q16` at `idx`, clamping to the last element for an out-of-range index -- matches
/// the float sibling's own `.get(idx).copied().unwrap_or(*last)` resampling behavior exactly (needed
/// because the previous frame's own `L` can differ from this frame's `L`).
fn prev_at(log2_ml_q16: &[i32], idx: usize) -> i32 {
    // mbelib sets the previous frame's log2Ml[0] to log2Ml[1] (read when L grows).
    let idx = if idx == 0 { 1 } else { idx };
    log2_ml_q16
        .get(idx)
        .copied()
        .unwrap_or_else(|| *log2_ml_q16.last().unwrap_or(&0))
}

/// The fixed-point equivalent of `ambe_plus_2::decode::dequantize`/`dstar::decode::dequantize`'s own
/// `FrameKind::Speech` branch. `l`/`w0_q16` are the caller's own already-resolved table lookups (or,
/// for D-STAR, formula results); `raw`/`tables` supply everything else. Advances `state` in place,
/// exactly mirroring the float sibling's own side effect.
pub fn dequantize_speech(
    l: u32,
    w0_q16: i32,
    w0_q32: i64,
    vuv_w0_q16: i32,
    raw: &RawSpeechParameters,
    tables: &SpeechTables,
    state: &mut MbeDecoderState,
) -> SpeechParameters {
    let l_usize = l as usize;

    // f0 = w0 / (2*pi) -- needed for the VUV per-harmonic lookup below.
    // `vuv_w0_q16` is the fundamental used for the V/UV slot lookup only: mbelib uses its own unscaled pitch there
    // (D-STAR passes the table pitch before the chip-fit scale; AMBE+2 passes `w0_q16` itself).
    let f0_q16 = div_q16(vuv_w0_q16, TWO_PI_Q16_16);

    // jl = floor(harmonic * 16 * f0) -- an exact Q16.16-to-integer floor via a plain right shift
    // (every operand here is non-negative). **A real, quantified, expected source of rare
    // disagreement with the float sibling, not a bug**: `f0_q16` already carries up to ~1.5e-5
    // absolute rounding error from being stored in Q16.16 in the first place (a single ULP). When
    // the true (infinite-precision) value of `harmonic*16*f0` falls within that margin of an
    // integer, `floor()` can legitimately land on a different side in fixed point than in `f64` --
    // confirmed directly (not just theorized): scanning every `(harmonic, b0)` pair this crate's
    // real tables can produce found 16 such boundary crossings out of 7056 combinations (~0.23%),
    // and live chip validation against 3320 real frames of real recorded speech found exactly one
    // frame with exactly one harmonic's voiced/unvoiced decision flipped this way (D-STAR,
    // `examples/ambe_fixed_chip_validate_dstar.rs`) -- 99.97% of real frames matched on every
    // harmonic. A wider fixed-point format for `f0` would shrink this rate, not eliminate it: any
    // finite-precision quantization has *some* nonzero probability of disagreeing with exact real
    // arithmetic at a floor boundary, so this is inherent to fixed-point synthesis, not a defect to
    // chase to zero.
    let mut voiced = vec![false; l_usize + 1];
    for (harmonic, slot) in voiced.iter_mut().enumerate().skip(1) {
        let jl_q16 = mul_q16((harmonic as i32) << 16, f0_q16).saturating_mul(16);
        let jl = (jl_q16 >> 16).max(0) as usize;
        *slot = tables.vuv[raw.b1 as usize][jl.min(7)];
    }

    let delta_gamma_q16 = tables.dg_q16[raw.b2 as usize];
    let gamma_q16 = (((delta_gamma_q16 as i64 * tables.gamma_scale_q16) >> 16)
        + ((state.gamma_q16 as i64 * tables.gamma_memory_q16) >> 16))
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;

    // PRBA -> Gm -> Ri: an 8-point cosine sum (mbelib's own real `Gm`/`Ri` construction). `gm[0]`
    // (array index 0, never accessed by the float sibling's own `enumerate().skip(1)`) and `gm[1]`
    // (`m=1`, `am=1.0`, but `gm[1]` is always `0.0`) both contribute exactly nothing -- so this loop
    // starts at array index 2 (`m=2`, matching the float sibling's own array-index-as-`m`
    // convention exactly) rather than carrying two always-0 terms, and every remaining term uses
    // `am=2` unconditionally (only `m=1`, already excluded, ever used `am=1`).
    let prba24 = tables.prba24_q16[raw.b3 as usize];
    let prba58 = tables.prba58_q16[raw.b4 as usize];
    let gm: [i32; 9] = [
        0, 0, prba24[0], prba24[1], prba24[2], prba58[0], prba58[1], prba58[2], prba58[3],
    ];

    let mut ri = [0i32; 9];
    for (i, slot) in ri.iter_mut().enumerate().skip(1) {
        let mut sum: i64 = 0;
        for (m, &gm_m) in gm.iter().enumerate().skip(2) {
            let cos_val = cos_pi_frac((m as i64) - 1, 2 * (i as i64) - 1, 8);
            sum += 2 * (gm_m as i64) * (cos_val as i64);
        }
        *slot = (sum >> 16) as i32; // one extra Q16.16 factor from `gm_m * cos_val`, shift back down
    }

    let sqrt2_q16 = super::isqrt::isqrt_u64(2u64 << 32) as i32; // sqrt(2) in Q16.16
    let rconst_q16 = sqrt2_q16 >> 2; // 1/(2*sqrt(2)) = sqrt(2)/4

    let mut cik = [[0i32; 18]; 5];
    cik[1][1] = (ri[1] + ri[2]) >> 1;
    cik[1][2] = mul_q16(rconst_q16, ri[1] - ri[2]);
    cik[2][1] = (ri[3] + ri[4]) >> 1;
    cik[2][2] = mul_q16(rconst_q16, ri[3] - ri[4]);
    cik[3][1] = (ri[5] + ri[6]) >> 1;
    cik[3][2] = mul_q16(rconst_q16, ri[5] - ri[6]);
    cik[4][1] = (ri[7] + ri[8]) >> 1;
    cik[4][2] = mul_q16(rconst_q16, ri[7] - ri[8]);

    let ji = tables.lmprbl[l_usize];
    let hoc_tables: [&[[i32; 4]]; 4] = [
        tables.hoc_b5_q16,
        tables.hoc_b6_q16,
        tables.hoc_b7_q16,
        tables.hoc_b8_q16,
    ];
    let hoc_indices = [raw.b5, raw.b6, raw.b7, raw.b8];
    for block in 0..4usize {
        for k in 3..=ji[block] {
            if k <= 6 {
                cik[block + 1][k as usize] =
                    hoc_tables[block][hoc_indices[block] as usize][(k - 3) as usize];
            }
        }
    }

    // Inverse DCT each block's own Cik into Tl (log-domain per-harmonic residual).
    let mut tl = vec![0i32; l_usize + 1];
    let mut harmonic = 1usize;
    for block in 0..4 {
        let block_len = ji[block] as usize;
        for j in 1..=block_len {
            let mut sum: i64 = 0;
            for (k, &cik_k) in cik[block + 1]
                .iter()
                .enumerate()
                .take(block_len + 1)
                .skip(1)
            {
                let cos_val = cos_pi_frac((k as i64) - 1, 2 * (j as i64) - 1, block_len as i64);
                let ak: i64 = if k == 1 { 1 } else { 2 };
                sum += ak * (cik_k as i64) * (cos_val as i64);
            }
            if harmonic <= l_usize {
                tl[harmonic] = (sum >> 16) as i32;
            }
            harmonic += 1;
        }
    }

    let prev_l = state.l.max(1) as i64;
    let mut int_kl = vec![0usize; l_usize + 1];
    let mut delta_l_q16 = vec![0i32; l_usize + 1];
    let mut sum43: i64 = 0;
    for h in 1..=l_usize {
        // f = (prev_l / l) * h, kept as one exact rational (prev_l*h)/l rather than computing the
        // ratio prev_l/l first, so this matches the float sibling's own rounding behavior as
        // closely as fixed point can (no intermediate division to accumulate extra error).
        let f_q16 = ((prev_l * (h as i64)) << 16) / (l as i64);
        let ik = (f_q16 >> 16).max(0) as usize;
        let delta = f_q16 - ((ik as i64) << 16);
        int_kl[h] = ik;
        delta_l_q16[h] = delta as i32;
        let one_minus_delta = 65536 - delta;
        sum43 += one_minus_delta * (prev_at(&state.log2_ml_q16, ik) as i64)
            + delta * (prev_at(&state.log2_ml_q16, ik + 1) as i64);
    }
    // sum43 currently carries two extra Q16.16 factors (delta_l and the log2_ml value); shift back
    // to one Q16.16 factor, then apply the *0.65/l scaling.
    let sum43_q16 = sum43 >> 16;
    let sum43_scaled_q16 = ((sum43_q16 * tables.rho_q16) >> 16) / (l as i64);

    let sum_tl: i64 = tl[1..=l_usize].iter().map(|&v| v as i64).sum();
    let sum42_q16 = (sum_tl / (l as i64)) as i32;
    let l_log2_q16 = super::explog::log2_q16((l as i32) << 16);
    let big_gamma_q16 = gamma_q16 - (l_log2_q16 >> 1) - sum42_q16;

    let mut log2_ml_q16 = vec![0i32; l_usize + 1];
    let mut ml_q16 = vec![0i32; l_usize + 1];
    let w0_sqrt_q16 = super::fixed_ops::sqrt_q16(w0_q16);
    let unvc_q16 = div_q16(POINT_2046_Q16_16, w0_sqrt_q16);
    for h in 1..=l_usize {
        let delta = delta_l_q16[h] as i64;
        let one_minus_delta = 65536 - delta;
        let ik = int_kl[h];
        let c1 =
            (tables.rho_q16 * one_minus_delta * (prev_at(&state.log2_ml_q16, ik) as i64)) >> 32;
        let c2 = (tables.rho_q16 * delta * (prev_at(&state.log2_ml_q16, ik + 1) as i64)) >> 32;
        log2_ml_q16[h] = (tl[h] as i64 + c1 + c2 - sum43_scaled_q16 + big_gamma_q16 as i64) as i32;
        let exp2_val = exp2_q16(log2_ml_q16[h]);
        ml_q16[h] = if voiced[h] {
            exp2_val
        } else {
            mul_q16(unvc_q16, exp2_val)
        };
    }

    state.l = l;
    state.log2_ml_q16 = log2_ml_q16.clone();
    state.gamma_q16 = gamma_q16;

    SpeechParameters {
        l,
        w0_q16,
        w0_q32,
        voiced,
        ml_q16,
    }
}
