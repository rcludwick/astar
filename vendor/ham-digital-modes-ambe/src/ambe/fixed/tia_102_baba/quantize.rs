// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::tia_102_baba::quantize` and the quantizer half of the float `tia_102_baba` module:
//! block partitioning, the per-block and gain-vector DCTs (Eq. 60-61), and the saturating uniform quantizers for the
//! gain vector and the higher-order coefficients (Eq. 62-63), plus the nearest-level search for `b_hat_2`.
//!
//! Values are Q16.16 (`i32`); sums use `i64`. The uniform quantizers take the floor of `value / step` exactly, as a
//! floor division of the two Q16.16 integers (`i64::div_euclid`, the step being positive), so the only difference
//! from the float sibling is the Q16.16 rounding of the value and of the step (tables:
//! [`super::reconstruct_tables`], the same tables the fixed dequantizer uses, so this quantizer inverts the fixed
//! decoder). Block lengths, bit allocations and positions are integers, reused from the float tables.

use super::reconstruct_tables::{
    GAIN_BIT_ALLOCATION_STEP_Q16_16, GAIN_QUANTIZER_LEVELS_Q16_16,
    HIGHER_ORDER_COEFFICIENT_SIGMA_Q16_16, HIGHER_ORDER_STEP_MULTIPLIER_Q16_16,
};
use crate::ambe::fixed::general::fixed_ops::mul_q16;
use crate::ambe::fixed::general::trig::cos_pi_frac;
use crate::ambe::float::tia_102_baba::quantize::higher_order_coefficient_positions;
use crate::ambe::float::tia_102_baba::tables::{
    block_lengths_for_l, gain_vector_bits, higher_order_bit_allocation,
};

/// Splits `l` residuals into Annex J's six blocks; `None` for an `l` outside `9..=56` or a wrong residual count.
pub fn partition_into_blocks_q16(residuals_q16: &[i32], l: u32) -> Option<[Vec<i32>; 6]> {
    let lengths = block_lengths_for_l(l)?;
    if residuals_q16.len() != l as usize {
        return None;
    }
    let mut blocks: [Vec<i32>; 6] = Default::default();
    let mut offset = 0usize;
    for (block, &len) in blocks.iter_mut().zip(lengths.iter()) {
        *block = residuals_q16[offset..offset + len as usize].to_vec();
        offset += len as usize;
    }
    Some(blocks)
}

/// Eq. 60: `C_k = (1/J) sum_j c_j cos(pi (k-1)(j-1/2) / J)`, `k = 1..=J` (returned 0-indexed).
pub fn block_dct_q16(c_q16: &[i32]) -> Vec<i32> {
    let j = c_q16.len();
    if j == 0 {
        return Vec::new();
    }
    (1..=j)
        .map(|k| {
            let mut sum = 0i64; // Q32
            for (idx, &c) in c_q16.iter().enumerate() {
                sum +=
                    c as i64 * cos_pi_frac(k as i64 - 1, 2 * (idx as i64 + 1) - 1, j as i64) as i64;
            }
            ((sum / j as i64) >> 16) as i32
        })
        .collect()
}

/// Eq. 61: `G_m = (1/6) sum_i R_i cos(pi (m-1)(i-1/2) / 6)` for the six block DC terms.
pub fn gain_vector_dct_q16(r_hat_q16: &[i32; 6]) -> [i32; 6] {
    let mut g = [0i32; 6];
    for (m, slot) in g.iter_mut().enumerate() {
        let mut sum = 0i64;
        for (i, &r) in r_hat_q16.iter().enumerate() {
            sum += r as i64 * cos_pi_frac(m as i64, 2 * (i as i64 + 1) - 1, 6) as i64;
        }
        *slot = ((sum / 6) >> 16) as i32;
    }
    g
}

/// `b_hat_2`: the index of the gain quantizer level nearest `g_hat_1` (first minimum on ties, like the float).
pub fn quantize_gain_index_q16(g_hat_1_q16: i32) -> u8 {
    let mut best = (i64::MAX, 0usize);
    for (i, &level) in GAIN_QUANTIZER_LEVELS_Q16_16.iter().enumerate() {
        let d = (level as i64 - g_hat_1_q16 as i64).abs();
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1 as u8
}

/// The shared saturating uniform quantizer of Eq. 62/63: `0` below `-2^(bits-1)`, `2^bits - 1` at or above
/// `2^(bits-1)`, else the floored zero-offset index. `step_q16` must be positive.
fn saturating_uniform_quantize_q16(value_q16: i32, bits: u8, step_q16: i32) -> u32 {
    let half_range = 1i64 << (bits - 1);
    let idx = (value_q16 as i64).div_euclid(step_q16.max(1) as i64);
    if idx < -half_range {
        0
    } else if idx >= half_range {
        ((1i64 << bits) - 1) as u32
    } else {
        (idx + half_range) as u32
    }
}

/// Eq. 62: quantizes `G_hat_2..G_hat_6` into `(value, bits)` pairs.
pub fn quantize_gain_vector_q16(g_hat_q16: &[i32; 6], l: u32) -> Option<[(u32, u8); 5]> {
    let l_index = (l as usize).checked_sub(9).filter(|&i| i < 48)?;
    let mut out = [(0u32, 0u8); 5];
    for (idx, element) in (2..=6u32).enumerate() {
        let bits = gain_vector_bits(l, element)?;
        let step = GAIN_BIT_ALLOCATION_STEP_Q16_16[l_index][idx];
        out[idx] = (
            saturating_uniform_quantize_q16(g_hat_q16[(element - 1) as usize], bits, step),
            bits,
        );
    }
    Some(out)
}

/// Eq. 63: quantizes every transmitted higher-order coefficient (zero-bit allocations are skipped), in the flat
/// `[C_1,2, ..., C_6,J6]` order, each paired with its bit width.
pub fn quantize_higher_order_coefficients_q16(
    dct_blocks_q16: &[Vec<i32>; 6],
    l: u32,
) -> Option<Vec<(u32, u8)>> {
    let bit_allocation = higher_order_bit_allocation(l)?;
    let positions = higher_order_coefficient_positions(l)?;
    if bit_allocation.len() != positions.len() {
        return None;
    }
    let mut out = Vec::new();
    for (&(block_idx, k), &bits) in positions.iter().zip(bit_allocation.iter()) {
        if bits == 0 {
            continue;
        }
        let sigma = *HIGHER_ORDER_COEFFICIENT_SIGMA_Q16_16.get(k)?;
        let multiplier = *HIGHER_ORDER_STEP_MULTIPLIER_Q16_16.get(bits as usize)?;
        let step = mul_q16(multiplier, sigma);
        let c = *dct_blocks_q16[block_idx].get(k - 1)?;
        out.push((saturating_uniform_quantize_q16(c, bits, step), bits));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dct_of_a_constant_block_is_zero_except_the_first_coefficient() {
        let dct = block_dct_q16(&[2 << 16; 7]);
        assert!((dct[0] - (2 << 16)).abs() < 8);
        for &c in &dct[1..] {
            assert!(c.abs() < 8, "{c}");
        }
        let g = gain_vector_dct_q16(&[3 << 16; 6]);
        assert!((g[0] - (3 << 16)).abs() < 8);
        assert!(g[1..].iter().all(|&x| x.abs() < 8));
    }

    #[test]
    fn saturating_quantizer_follows_eq_62() {
        // 3 bits, step 1.0: indices -4..=3 map to 0..=7, saturating outside.
        let q = |v_q16: i32| saturating_uniform_quantize_q16(v_q16, 3, 65536);
        assert_eq!(
            (
                q(-10 << 16),
                q(-4 << 16),
                q(-(7 << 15)),
                q(0),
                q(64880),
                q(3 << 16),
                q(9 << 16)
            ),
            (0, 0, 0, 4, 4, 7, 7)
        );
        assert_eq!(q(-655), 3); // floor(-0.01) = -1
    }

    #[test]
    fn gain_index_is_the_nearest_level() {
        for (i, &level) in GAIN_QUANTIZER_LEVELS_Q16_16.iter().enumerate() {
            assert_eq!(quantize_gain_index_q16(level) as usize, i);
        }
    }
}
