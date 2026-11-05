// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::tia_102_baba::reconstruct` -- spectral amplitude reconstruction
//! (TIA-102.BABA_2003.pdf section 6.4, Eq. 67-79). `block_lengths_for_l`,
//! `higher_order_bit_allocation`, and `higher_order_coefficient_positions` are already pure integer
//! in the float sibling and are reused directly; `gain_vector_bits` (added to the float sibling this
//! session) supplies the bit-width half of `gain_bit_allocation` without ever constructing the `f64`
//! step-size half (see that function's own doc comment for why that distinction matters here).

use super::prediction;
use super::reconstruct_tables::{
    GAIN_BIT_ALLOCATION_STEP_Q16_16, GAIN_QUANTIZER_LEVELS_Q16_16,
    HIGHER_ORDER_COEFFICIENT_SIGMA_Q16_16, HIGHER_ORDER_STEP_MULTIPLIER_Q16_16,
};
use crate::ambe::fixed::general::explog::exp2_q16;
use crate::ambe::fixed::general::fixed_ops::mul_q16;
use crate::ambe::fixed::general::trig::cos_pi_frac;
use crate::ambe::float::tia_102_baba::quantize::higher_order_coefficient_positions;
use crate::ambe::float::tia_102_baba::tables::{
    block_lengths_for_l, gain_vector_bits, higher_order_bit_allocation,
};

/// The fixed-point equivalent of `dequantize_uniform` (Eq. 68/71's shared bin-center formula):
/// `0` if `bits == 0`, otherwise `step_size * (quantizer_value - 2^(bits-1) + 0.5)`.
fn dequantize_uniform_q16(quantizer_value: u32, bits: u8, step_size_q16: i32) -> i32 {
    if bits == 0 {
        return 0;
    }
    let half_range = 1i64 << (bits - 1);
    let diff_plus_half_q16 = (((quantizer_value as i64 - half_range) << 16) + 32768) as i32;
    mul_q16(step_size_q16, diff_plus_half_q16)
}

/// The fixed-point equivalent of `reconstruct_gain_vector` (Eq. 68).
pub fn reconstruct_gain_vector_q16(b2: u8, gain_values: [u32; 5], l: u32) -> Option<[i32; 6]> {
    let g1_q16 = *GAIN_QUANTIZER_LEVELS_Q16_16.get(b2 as usize)?;
    let l_index = (l as usize).checked_sub(9).filter(|&i| i < 48)?;
    let mut g_hat = [0i32; 6];
    g_hat[0] = g1_q16;
    for (idx, element) in (2..=6u32).enumerate() {
        let bits = gain_vector_bits(l, element)?;
        let step_size_q16 = GAIN_BIT_ALLOCATION_STEP_Q16_16[l_index][(element - 2) as usize];
        g_hat[(element - 1) as usize] =
            dequantize_uniform_q16(gain_values[idx], bits, step_size_q16);
    }
    Some(g_hat)
}

/// The fixed-point equivalent of `inverse_gain_vector_dct` (Eq. 69-70): the fixed 6-point inverse
/// DCT reconstructing `R_hat_1..R_hat_6` from `G_hat_1..G_hat_6`.
pub fn inverse_gain_vector_dct_q16(g_hat: &[i32; 6]) -> [i32; 6] {
    let mut r_hat = [0i32; 6];
    for (i, slot) in r_hat.iter_mut().enumerate() {
        let b_times_2 = 2 * (i as i64) + 1; // 2*(i+1) - 1, i.e. 2*(i1 - 0.5) for i1 = i+1
        let mut sum: i64 = 0;
        for (m, &g) in g_hat.iter().enumerate() {
            let alpha: i64 = if m == 0 { 1 } else { 2 };
            let cos_val = cos_pi_frac(m as i64, b_times_2, 6);
            sum += alpha * (g as i64) * (cos_val as i64);
        }
        *slot = (sum >> 16) as i32;
    }
    r_hat
}

/// The fixed-point equivalent of `inverse_block_dct` (Eq. 73-74): the inverse DCT for an arbitrary
/// block length, reconstructing one block's own `c` values from its DCT coefficients.
pub fn inverse_block_dct_q16(dct_coeffs: &[i32]) -> Vec<i32> {
    let j = dct_coeffs.len();
    if j == 0 {
        return Vec::new();
    }
    (1..=j)
        .map(|j_idx| {
            let b_times_2 = 2 * (j_idx as i64) - 1; // j_idx is already 1-indexed here
            let mut sum: i64 = 0;
            for (k_idx, &c) in dct_coeffs.iter().enumerate() {
                let alpha: i64 = if k_idx == 0 { 1 } else { 2 };
                let cos_val = cos_pi_frac(k_idx as i64, b_times_2, j as i64);
                sum += alpha * (c as i64) * (cos_val as i64);
            }
            (sum >> 16) as i32
        })
        .collect()
}

/// The fixed-point equivalent of `reconstruct_higher_order_coefficients` (Eq. 71-72).
pub fn reconstruct_higher_order_coefficients_q16(
    quantized_values: &[u32],
    l: u32,
) -> Option<[Vec<i32>; 6]> {
    let bit_allocation = higher_order_bit_allocation(l)?;
    let positions = higher_order_coefficient_positions(l)?;
    if bit_allocation.len() != positions.len() {
        return None;
    }
    let lengths = block_lengths_for_l(l)?;
    let mut blocks: [Vec<i32>; 6] = std::array::from_fn(|i| vec![0i32; lengths[i] as usize]);

    let mut value_iter = quantized_values.iter();
    for (&(block_idx, k), &bits) in positions.iter().zip(bit_allocation.iter()) {
        let value = if bits == 0 {
            0
        } else {
            let sigma_q16 = *HIGHER_ORDER_COEFFICIENT_SIGMA_Q16_16.get(k)?;
            let multiplier_q16 = *HIGHER_ORDER_STEP_MULTIPLIER_Q16_16.get(bits as usize)?;
            let step_size_q16 = mul_q16(multiplier_q16, sigma_q16);
            let quantizer_value = *value_iter.next()?;
            dequantize_uniform_q16(quantizer_value, bits, step_size_q16)
        };
        blocks[block_idx][k - 1] = value;
    }
    if value_iter.next().is_some() {
        return None;
    }
    Some(blocks)
}

/// The fixed-point equivalent of `reconstruct_spectral_amplitudes` (Eq. 67-79): the full
/// reconstruction pipeline from this frame's own quantizer values to reconstructed linear spectral
/// amplitudes `M_tilde_l(0)` in Q16.16, for `1 <= l <= l_hat_curr`.
pub fn reconstruct_spectral_amplitudes_q16(
    b2: u8,
    gain_values: [u32; 5],
    higher_order_quantized_values: &[u32],
    l_hat_curr: u32,
    l_hat_prev: u32,
    previous_m_q16: &[i32],
) -> Option<Vec<i32>> {
    let g_hat = reconstruct_gain_vector_q16(b2, gain_values, l_hat_curr)?;
    let r_hat = inverse_gain_vector_dct_q16(&g_hat);

    let mut blocks =
        reconstruct_higher_order_coefficients_q16(higher_order_quantized_values, l_hat_curr)?;
    for (i, block) in blocks.iter_mut().enumerate() {
        block[0] = r_hat[i];
    }

    let t_hat: Vec<i32> = blocks
        .iter()
        .flat_map(|block| inverse_block_dct_q16(block))
        .collect();
    if t_hat.len() != l_hat_curr as usize {
        return None;
    }

    Some(
        (1..=l_hat_curr)
            .map(|l| {
                let log2_m_q16 = prediction::reconstruct_log2_amplitude_q16(
                    l,
                    t_hat[(l - 1) as usize],
                    l_hat_curr,
                    l_hat_prev,
                    previous_m_q16,
                );
                exp2_q16(log2_m_q16)
            })
            .collect(),
    )
}
