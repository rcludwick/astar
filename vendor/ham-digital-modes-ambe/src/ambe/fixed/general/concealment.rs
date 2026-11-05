// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`crate::ambe::float::concealment`] (`ErrorPolicy::Concealing`): graded, continuity-aware repair of the
//! unprotected bits and a fading repeat instead of a hard mute. See the float module for the design and the measurements behind it.
//!
//! Numeric conventions: costs are in nats as Q16.16 (`i64`), levels and amplitudes in dB as Q16.16, the bit error rate estimate is
//! Q32 (a rate of 0.003 is only 197 in Q16, too coarse for the running average), and fade factors are Q16.16.

use super::explog::{exp2_q16, log2_q16};
use super::isqrt::isqrt_u128;

const ONE: i64 = 1 << 16;
/// `20 * log10(2)` in Q16.16: dB per doubling of amplitude.
const DB_PER_OCTAVE_Q16: i64 = 394_564;
/// `ln(2)` in Q16.16.
const LN2_Q16: i64 = 45_426;
/// The smallest amplitude considered (`1e-3`), as Q16.16.
const AMPLITUDE_FLOOR_Q16: i32 = 66;

fn div_q16(a_q16: i64, b_q16: i64) -> i64 {
    ((a_q16 as i128) << 16)
        .checked_div(b_q16 as i128)
        .unwrap_or(i64::MAX as i128)
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64
}
fn mul_q16(a: i64, b: i64) -> i64 {
    ((a as i128 * b as i128) >> 16) as i64
}
/// Amplitude (Q16.16 real value) to dB, Q16.16.
fn amplitude_db_q16(amplitude_q16: i64) -> i64 {
    let a = amplitude_q16.clamp(AMPLITUDE_FLOOR_Q16 as i64, i32::MAX as i64) as i32;
    (log2_q16(a) as i64 * DB_PER_OCTAVE_Q16) >> 16
}

/// What the continuity prior looks at, from one decoded frame (fixed-point sibling of `Descriptor`).
#[derive(Clone, Debug)]
pub struct Descriptor {
    pub w0_q16: i32,
    pub level_db_q16: i64,
    pub spectrum_db_q16: [i64; 16],
    pub voicing: [bool; 8],
}

impl Descriptor {
    /// `voiced` and `ml_q16` are 1-indexed by harmonic (index 0 unused).
    pub fn new(w0_q16: i32, voiced: &[bool], ml_q16: &[i32]) -> Self {
        let l = ml_q16.len() - 1;
        let mean_square_q32: u128 = ml_q16[1..]
            .iter()
            .map(|&m| (m as i128 * m as i128) as u128)
            .sum::<u128>()
            / l as u128;
        let rms_q16 = isqrt_u128(mean_square_q32) as i64;
        Self {
            w0_q16,
            level_db_q16: amplitude_db_q16(rms_q16),
            spectrum_db_q16: std::array::from_fn(|k| {
                amplitude_db_q16(ml_q16[1 + (k * l) / 16] as i64)
            }),
            voicing: std::array::from_fn(|b| voiced[1 + (b * l) / 8]),
        }
    }

    fn any_voiced(&self) -> bool {
        self.voicing.iter().any(|&v| v)
    }
}

/// Tunable constants in fixed point; `Default` equals the float `ConcealParams::default()` (checked by a test).
#[derive(Clone, Copy, Debug)]
pub struct ConcealParams {
    pub level_scale_db_q16: i64,
    pub shape_scale_db_q16: i64,
    pub voicing_cost_q16: i64,
    pub pitch_scale_q16: i64,
    pub garbage_cost_q16: i64,
    pub error_suspicion_q16: i64,
    pub ber_alpha_q16: i64,
    /// Bit error rate floor, Q32.
    pub ber_floor_q32: i64,
    pub flip_gate_q16: i64,
    pub fade_per_frame_q16: i64,
    pub reset_after: u32,
    pub widen_per_repeat_q16: i64,
}

impl Default for ConcealParams {
    /// Equals the float `ConcealParams::default()` in fixed point (checked by `tests/ambe_error_concealment.rs`).
    fn default() -> Self {
        Self {
            level_scale_db_q16: 229_376,
            shape_scale_db_q16: 393_216,
            voicing_cost_q16: 19_661,
            pitch_scale_q16: 9_830,
            garbage_cost_q16: 1_310_720,
            error_suspicion_q16: 131_072,
            ber_alpha_q16: 6_554,
            ber_floor_q32: 42_949_673,
            flip_gate_q16: 131_072,
            fade_per_frame_q16: 32_768,
            reset_after: 30,
            widen_per_repeat_q16: 16_384,
        }
    }
}

/// Decision for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept(usize),
    /// Repeat the previous frame scaled by this Q16.16 factor (0 = silence); `reset` says to reset the decoder state first.
    Repeat {
        scale_q16: i32,
        reset: bool,
    },
}

pub struct Concealer {
    pub params: ConcealParams,
    previous: Option<Descriptor>,
    ber_q32: i64,
    repeats: u32,
}

impl Concealer {
    pub fn new(params: ConcealParams) -> Self {
        Self {
            params,
            previous: None,
            ber_q32: params.ber_floor_q32,
            repeats: 0,
        }
    }

    fn continuity_cost_q16(&self, c: &Descriptor) -> i64 {
        let Some(p) = &self.previous else { return 0 };
        let q = &self.params;
        let widen_q16 = ONE + mul_q16(q.widen_per_repeat_q16, (self.repeats as i64) << 16);
        let mut cost = div_q16(
            (c.level_db_q16 - p.level_db_q16).abs(),
            mul_q16(q.level_scale_db_q16, widen_q16),
        );
        let sum_sq: u128 = c
            .spectrum_db_q16
            .iter()
            .zip(&p.spectrum_db_q16)
            .map(|(a, b)| ((a - b) as i128 * (a - b) as i128) as u128)
            .sum();
        let shape_q16 = isqrt_u128(sum_sq / 16) as i64;
        cost += div_q16(shape_q16, mul_q16(q.shape_scale_db_q16, widen_q16));
        cost += q.voicing_cost_q16
            * c.voicing
                .iter()
                .zip(&p.voicing)
                .filter(|(a, b)| a != b)
                .count() as i64;
        if c.any_voiced() && p.any_voiced() {
            let ln_ratio =
                ((log2_q16(c.w0_q16) as i64 - log2_q16(p.w0_q16) as i64) * LN2_Q16) >> 16;
            cost += div_q16(ln_ratio.abs(), mul_q16(q.pitch_scale_q16, widen_q16));
        }
        cost
    }

    /// Fixed-point sibling of `Concealer::decide`; `epsilon` is the number of errors the channel code corrected in this frame.
    pub fn decide(&mut self, candidates: &[(u32, Option<Descriptor>)], epsilon: u32) -> Decision {
        let q = self.params;
        let observed_q32 = ((epsilon as i128) << 32) / 47;
        self.ber_q32 = (self.ber_q32 as i128
            + (((observed_q32 - self.ber_q32 as i128) * q.ber_alpha_q16 as i128) >> 16))
            .clamp(q.ber_floor_q32 as i128, (2 * (1i128 << 32)) / 5) as i64;
        let gate_q32 = ((q.ber_floor_q32 as i128 * q.flip_gate_q16 as i128) >> 16) as i64;
        if epsilon == 0 && self.ber_q32 <= gate_q32 {
            if let Some((_, Some(d))) = candidates.first() {
                self.previous = Some(d.clone());
                self.repeats = 0;
                return Decision::Accept(0);
            }
        }
        // ln((1 - p) / p) with p = ber.
        let p_q16 = (self.ber_q32 >> 16).max(1);
        let ratio_q16 = (((1i64 << 16) - p_q16) << 16) / p_q16;
        let flip_cost_q16 =
            (log2_q16(ratio_q16.clamp(1, i32::MAX as i64) as i32) as i64 * LN2_Q16) >> 16;
        let mut best: Option<(usize, i64)> = None;
        for (i, (flips, desc)) in candidates.iter().enumerate() {
            let Some(d) = desc else { continue };
            if *flips > 0 && self.ber_q32 <= gate_q32 {
                continue;
            }
            let cost = flip_cost_q16 * *flips as i64
                + self.continuity_cost_q16(d)
                + q.error_suspicion_q16 * epsilon as i64;
            if best.is_none_or(|(_, b)| cost < b) {
                best = Some((i, cost));
            }
        }
        // Probability that at least one of the 25 raw bits is wrong: 1 - (1 - p)^25.
        let one_minus_p_q16 = ((1i64 << 16) - p_q16).max(1) as i32;
        let survive_q16 =
            exp2_q16((25 * log2_q16(one_minus_p_q16) as i64).clamp(i32::MIN as i64, 0) as i32)
                as i64;
        let p_raw_error_q16 = (ONE - survive_q16).max(1);
        let ln_p_raw_q16 = (log2_q16(p_raw_error_q16 as i32) as i64 * LN2_Q16) >> 16;
        let repeat_cost_q16 = q.garbage_cost_q16 - ln_p_raw_q16;
        match best {
            Some((i, cost)) if cost <= repeat_cost_q16 => {
                self.previous = candidates[i].1.clone();
                self.repeats = 0;
                Decision::Accept(i)
            }
            _ => {
                self.repeats += 1;
                let mut scale = ONE;
                for _ in 0..self.repeats.min(64) {
                    scale = mul_q16(scale, q.fade_per_frame_q16);
                }
                let scale_q16 = if scale < 1966 { 0 } else { scale as i32 };
                Decision::Repeat {
                    scale_q16,
                    reset: self.repeats >= q.reset_after,
                }
            }
        }
    }
}
