// SPDX-License-Identifier: LGPL-3.0-or-later
//! Mode-independent quantization of one frame's MBE speech parameters for D-STAR and AMBE+2 half-rate: the
//! inverse of the shared dequantization chain (`dstar::decode::dequantize` / `ambe_plus_2::decode::dequantize`,
//! both transcribed from mbelib's `mbe_decodeAmbe2400Parms` / `mbe_decodeAmbe2450Parms`).
//!
//! Given a target pitch (already quantized, so `L` and `w0` are the decoder's), per-harmonic voicing and
//! per-harmonic amplitudes `Ml`, it chooses the indices `b1..b8`:
//! - `b1`: the V/UV pattern row that best agrees with the target voicing (amplitude weighted) over the decoder's
//!   `jl = floor(l*16*f0)` slots;
//! - `b2`: the gain-delta index nearest the value that makes the decoder's mean log amplitude match;
//! - `b3`/`b4`: nearest PRBA vectors for `Gm[2..4]` / `Gm[5..8]`, from the forward transforms of the four blocks'
//!   first two DCT coefficients;
//! - `b5..b8`: nearest higher-order-coefficient rows per block (only the coefficients the decoder reads).
//!
//! The decoder recursion is `log2Ml[l] = Tl[l] + pred[l] - Sum43 + Gamma`, `Gamma = gamma - 0.5*log2(L) -
//! mean(Tl)` (the mean of `Tl` cancels, so it is free), with `pred[l] = rho * interp(previous log2Ml)`.

use std::f64::consts::{PI, SQRT_2};

/// The per-mode tables the quantizer searches.
pub struct ModeTables<'a> {
    pub vuv: &'a [[bool; 8]],
    pub dg: &'a [f64],
    pub prba24: &'a [[f64; 3]],
    pub prba58: &'a [[f64; 4]],
    pub lmprbl: &'a [[u32; 4]],
    pub hoc: [&'a [[f64; 4]]; 4],
    /// D-STAR/AMBE+2 `b8` only ever carries even indices (its low bit is always 0).
    pub hoc_b8_even_only: bool,
    /// The mode's amplitude-predictor weight (`0.65` for AMBE+2, `dstar::decode::PREDICTOR_RHO` for D-STAR).
    pub rho: f64,
    /// The mode's gain recursion `gamma = gamma_scale*DG + gamma_memory*gamma_prev` (see `dstar::decode::GAMMA_SCALE`).
    pub gamma_scale: f64,
    pub gamma_memory: f64,
}

/// What the frame's analysis wants the decoder to reproduce. `voiced` and `ml` are 1-indexed by harmonic (index 0
/// unused), length `l + 1`.
pub struct SpeechTarget<'a> {
    pub l: u32,
    pub w0: f64,
    /// The fundamental used for the V/UV slot lookup `jl = floor(l * 16 * f0)`, in cycles/sample.
    pub vuv_f0: f64,
    pub voiced: &'a [bool],
    pub ml: &'a [f64],
}

/// The decoder-side state the recursion predicts from (`DStarDecoderState` / `DecoderState` fields).
pub struct PrevState<'a> {
    pub l: u32,
    pub log2_ml: &'a [f64],
    pub gamma: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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

/// `0.693`: the constant the decoder uses to turn `log2Ml` into a linear amplitude (`exp(0.693 * log2Ml)`).
const LN2_APPROX: f64 = 0.693;

fn prev_at(prev: &PrevState, idx: usize) -> f64 {
    // mbelib sets the previous frame's log2Ml[0] to log2Ml[1].
    let idx = if idx == 0 { 1 } else { idx };
    prev.log2_ml
        .get(idx)
        .copied()
        .unwrap_or_else(|| *prev.log2_ml.last().unwrap_or(&0.0))
}

fn nearest<const N: usize>(
    table: &[[f64; N]],
    target: &[f64; N],
    used: usize,
    only_even: bool,
) -> u32 {
    let mut best = (f64::INFINITY, 0usize);
    for (i, row) in table.iter().enumerate() {
        if only_even && i % 2 == 1 {
            continue;
        }
        let d: f64 = (0..used).map(|k| (row[k] - target[k]).powi(2)).sum();
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1 as u32
}

/// # Panics
/// If any of `target.ml[1..=l]` is non-finite: a NaN amplitude would otherwise be silently floored
/// by `f64::max(1e-6)` below (`f64::max` returns its other argument when one side is NaN) and quantized
/// into a confident-looking speech frame instead of failing loudly, matching
/// [`super::dstar::quantize::quantize_pitch`]/[`super::ambe_plus_2::quantize::quantize_pitch`]'s own
/// finiteness check on the pitch half of this same encode path (bug-hunt 2026-09-19, claims
/// `mbe_quantize_speech.md`/`float_quantize_pitch_and_tables.md`; the pitch half was fixed the same
/// day in `d1bc1ea4`, this amplitude half was not).
pub fn quantize_speech(
    target: &SpeechTarget,
    prev: &PrevState,
    tables: &ModeTables,
) -> QuantizedSpeech {
    let l = target.l as usize;
    assert!(
        target.ml[1..=l].iter().all(|m| m.is_finite()),
        "spectral amplitude must be finite, got {:?}",
        target.ml[1..=l].to_vec()
    );

    // b1: the V/UV pattern agreeing best with the target, weighted by amplitude.
    let jl_of = |harmonic: usize| ((harmonic as f64 * 16.0 * target.vuv_f0) as usize).min(7);
    let mut b1 = (f64::NEG_INFINITY, 0usize);
    for (row_idx, row) in tables.vuv.iter().enumerate() {
        let score: f64 = (1..=l)
            .map(|h| {
                let w = target.ml[h].max(1e-6);
                if row[jl_of(h)] == target.voiced[h] {
                    w
                } else {
                    -w
                }
            })
            .sum();
        if score > b1.0 {
            b1 = (score, row_idx);
        }
    }

    // The decoder's own prediction terms.
    let prev_l = prev.l.max(1) as f64;
    let mut pred = vec![0.0f64; l + 1];
    let mut sum43 = 0.0;
    #[allow(clippy::needless_range_loop)] // `h` is both the harmonic number and the `pred` index
    for h in 1..=l {
        let f = (prev_l / l as f64) * h as f64;
        let ik = f.floor() as usize;
        let delta = f - ik as f64;
        let interp = (1.0 - delta) * prev_at(prev, ik) + delta * prev_at(prev, ik + 1);
        sum43 += interp;
        pred[h] = tables.rho * interp;
    }
    sum43 *= tables.rho / l as f64;

    // Target log2Ml (undoing the unvoiced scaling the decoder applies), then x = log2Ml - pred.
    let unvc = 0.2046 / target.w0.sqrt();
    let x: Vec<f64> = (0..=l)
        .map(|h| {
            if h == 0 {
                return 0.0;
            }
            let m = target.ml[h].max(1e-30);
            let eff = if target.voiced[h] { m } else { m / unvc };
            eff.ln() / LN2_APPROX - pred[h]
        })
        .collect();
    let mean_x = x[1..=l].iter().sum::<f64>() / l as f64;
    let gamma_target = mean_x + sum43 + 0.5 * (l as f64).log2();
    let delta_target = (gamma_target - tables.gamma_memory * prev.gamma) / tables.gamma_scale;
    let b2 = tables
        .dg
        .iter()
        .enumerate()
        .min_by(|a, b| {
            (a.1 - delta_target)
                .abs()
                .total_cmp(&(b.1 - delta_target).abs())
        })
        .map(|(i, _)| i)
        .unwrap_or(0);

    // Zero-mean Tl, split into the four blocks and DCT'd.
    let tl: Vec<f64> = (0..=l)
        .map(|h| if h == 0 { 0.0 } else { x[h] - mean_x })
        .collect();
    let ji = tables.lmprbl[l];
    let mut cik = [[0.0f64; 18]; 5];
    let mut start = 1usize;
    for block in 0..4 {
        let j_len = ji[block] as usize;
        for (k, cik_k) in cik[block + 1]
            .iter_mut()
            .enumerate()
            .take(j_len.min(17) + 1)
            .skip(1)
        {
            let mut sum = 0.0;
            for j in 1..=j_len {
                if start + j - 1 <= l {
                    sum += tl[start + j - 1]
                        * (PI * (k as f64 - 1.0) * (j as f64 - 0.5) / j_len as f64).cos();
                }
            }
            *cik_k = sum / j_len as f64;
        }
        start += j_len;
    }

    // Ri from each block's first two coefficients, then Gm by the forward 8-point cosine transform.
    let mut ri = [0.0f64; 9];
    for block in 1..=4usize {
        let (c1, c2) = (cik[block][1], cik[block][2]);
        ri[2 * block - 1] = c1 + SQRT_2 * c2;
        ri[2 * block] = c1 - SQRT_2 * c2;
    }
    let mut gm = [0.0f64; 9];
    for (m, slot) in gm.iter_mut().enumerate().skip(1) {
        let sum: f64 = (1..=8)
            .map(|i| ri[i] * (PI * (m as f64 - 1.0) * (i as f64 - 0.5) / 8.0).cos())
            .sum();
        *slot = sum / 8.0;
    }
    let b3 = nearest(tables.prba24, &[gm[2], gm[3], gm[4]], 3, false);
    let b4 = nearest(tables.prba58, &[gm[5], gm[6], gm[7], gm[8]], 4, false);

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
                tables.hoc[block],
                &tgt,
                used,
                block == 3 && tables.hoc_b8_even_only,
            )
        };
    }

    QuantizedSpeech {
        b1: b1.1 as u32,
        b2: b2 as u32,
        b3,
        b4,
        b5: hoc_idx[0],
        b6: hoc_idx[1],
        b7: hoc_idx[2],
        b8: hoc_idx[3],
    }
}

/// Frame-to-frame state of the voicing/amplitude analysis (energy tracker and previous band decisions).
pub struct AnalysisState {
    xi_max: f64,
    prev_bands: Vec<bool>,
}

impl AnalysisState {
    pub fn new() -> Self {
        Self {
            xi_max: 20000.0,
            prev_bands: Vec::new(),
        }
    }
}

impl Default for AnalysisState {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-harmonic voicing and amplitudes for one frame at an already-quantized pitch `w0` (rad/sample) with `l`
/// harmonics, reusing TIA-102.BABA's analysis (`vuv::determine_voicing`, `spectral_amplitude`), whose amplitude scale is
/// the one this crate's shared synthesis expects. Both returned vectors are 1-indexed (index 0 unused), length
/// `l + 1`.
pub fn analyze_at_pitch(
    frame: &crate::ambe::float::tia_102_baba::encoder::FrameAnalysis,
    w0: f64,
    l: u32,
    state: &mut AnalysisState,
) -> (Vec<bool>, Vec<f64>) {
    use crate::ambe::float::tia_102_baba::spectral_amplitude::estimate_spectral_amplitudes;
    use crate::ambe::float::tia_102_baba::vuv::{determine_voicing, frequency_bands_count};

    let (bands, xi_max) = determine_voicing(
        &frame.refinement,
        w0,
        frame.initial_pitch_error,
        state.xi_max,
        &state.prev_bands,
    );
    state.xi_max = xi_max;
    state.prev_bands = bands.clone();
    let k_hat = frequency_bands_count(l) as usize;
    let mut padded = bands;
    let fill = *padded.last().unwrap_or(&false);
    while padded.len() < k_hat {
        padded.push(fill);
    }
    let amplitudes = estimate_spectral_amplitudes(&frame.refinement, l, k_hat as u32, w0, &padded);
    let mut voiced = vec![false; l as usize + 1];
    let mut ml = vec![0.0; l as usize + 1];
    for h in 1..=l as usize {
        let band = (h.div_ceil(3)).clamp(1, k_hat);
        voiced[h] = padded[band - 1];
        // A harmonic estimated as (nearly) silent is floored at -60 dB so its logarithm cannot swamp its block's coefficients.
        ml[h] = amplitudes[h - 1].max(1e-3);
    }
    (voiced, ml)
}
