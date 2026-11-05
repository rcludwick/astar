// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point TIA-102.BABA frame encoder: fixed-point sibling of `ambe::float::tia_102_baba::{encode_frame,
//! encode_prioritized_bits, FrameState}`. One 20 ms frame goes from the streaming analysis (a
//! [`RefinementFrame`], the refined [`Pitch`] and the initial-pitch error, see
//! [`super::encoder::FrameAnalysis`]) through voicing decision, spectral amplitude estimation, prediction, block
//! DCTs, gain-vector DCT, quantization of `b_hat_0..b_hat_{L+1}`, bit prioritization and FEC to the eight code
//! vectors `c_hat_0..c_hat_7`.
//!
//! Q16.16 (`i32`) throughout with `i64` sums, as in the fixed decoder ([`super::decode`]), whose reconstruction
//! ([`super::reconstruct::reconstruct_spectral_amplitudes_q16`]) supplies the closed-loop history, so the encoder
//! inverts the fixed decoder exactly. The pitch quantizer `b_hat_0` (Eq. 45) is exact integer arithmetic on the
//! [`Pitch`] fraction. The bit prioritization and FEC (`prioritize_bits`, `encode_code_vectors`) are pure integer code
//! shared with the float tree.

use super::pitch_refinement::{Pitch, RefinementFrame};
use super::prediction::{prediction_residual_q16, INITIAL_L_HAT_PREV};
use super::quantize::{
    block_dct_q16, gain_vector_dct_q16, partition_into_blocks_q16, quantize_gain_index_q16,
    quantize_gain_vector_q16, quantize_higher_order_coefficients_q16,
};
use super::reconstruct::reconstruct_spectral_amplitudes_q16;
use super::spectral_amplitude::estimate_spectral_amplitudes_q16;
use super::vuv::{determine_voicing, frequency_bands_count, harmonics_count, XI_MAX_INITIAL_Q16};
use crate::ambe::float::tia_102_baba::bit_prioritization::prioritize_bits;
use crate::ambe::float::tia_102_baba::encode_code_vectors;
use crate::ambe::float::tia_102_baba::parameter_encoding::encode_voicing_decisions;
use crate::ambe::float::tia_102_baba::tables::block_lengths_for_l;

/// The per-frame state carried into the next [`encode_frame`] call: the energy tracker, the previous frame's band
/// decisions and harmonic count, and its reconstructed amplitudes (Q16.16), i.e. what the decoder will hold.
pub struct FrameState {
    xi_max_q16: i128,
    l_hat: u32,
    voiced: Vec<bool>,
    spectral_amplitudes_q16: Vec<i32>,
}

impl FrameState {
    /// The initial state of a stream: no voicing history, `xi_max` at its floor, `L_hat(-1) = 30` and a flat
    /// unity-amplitude history (see the float `FrameState::initial` for why that constant is a safe choice).
    pub fn initial() -> Self {
        FrameState {
            xi_max_q16: XI_MAX_INITIAL_Q16,
            l_hat: INITIAL_L_HAT_PREV,
            voiced: Vec::new(),
            spectral_amplitudes_q16: vec![1 << 16; INITIAL_L_HAT_PREV as usize],
        }
    }
}

/// Encodes one frame to the eight code vectors; `None` if `L_hat` is outside `9..=56` or a table check fails.
pub fn encode_frame(
    frame: &RefinementFrame,
    pitch: &Pitch,
    initial_pitch_error_q16: i32,
    previous_state: &FrameState,
    sync_bit: bool,
) -> Option<([u32; 8], FrameState)> {
    let (u, state) = encode_prioritized_bits(
        frame,
        pitch,
        initial_pitch_error_q16,
        previous_state,
        sync_bit,
    )?;
    Some((encode_code_vectors(u), state))
}

/// [`encode_frame`] stopping before FEC: the prioritized bit vectors `u_hat_0..u_hat_7` and the next state.
pub fn encode_prioritized_bits(
    frame: &RefinementFrame,
    pitch: &Pitch,
    initial_pitch_error_q16: i32,
    previous_state: &FrameState,
    sync_bit: bool,
) -> Option<([u32; 8], FrameState)> {
    let l_hat = harmonics_count(pitch);
    block_lengths_for_l(l_hat)?; // reject an out-of-range L before any L-sized work
    let k_hat = frequency_bands_count(l_hat);

    let (voiced, xi_max_q16) = determine_voicing(
        frame,
        pitch,
        initial_pitch_error_q16,
        previous_state.xi_max_q16,
        &previous_state.voiced,
    );
    // Floor at one PCM step (log2 = 0), as the float sibling does: the logarithm of zero is undefined.
    let amplitudes: Vec<i64> =
        estimate_spectral_amplitudes_q16(frame, l_hat, k_hat, pitch, &voiced)
            .into_iter()
            .map(|m| m.max(1 << 16))
            .collect();

    let residuals: Vec<i32> = (1..=l_hat)
        .map(|l| {
            prediction_residual_q16(
                l,
                amplitudes[(l - 1) as usize],
                l_hat,
                previous_state.l_hat,
                &previous_state.spectral_amplitudes_q16,
            )
        })
        .collect();

    let blocks = partition_into_blocks_q16(&residuals, l_hat)?;
    let dct_blocks: [Vec<i32>; 6] = std::array::from_fn(|i| block_dct_q16(&blocks[i]));
    let r_hat: [i32; 6] = std::array::from_fn(|i| dct_blocks[i][0]);
    let g_hat = gain_vector_dct_q16(&r_hat);

    let b0 = pitch.quantizer_b0();
    let b1 = encode_voicing_decisions(&voiced);
    let b2 = quantize_gain_index_q16(g_hat[0]);
    let gain_vector = quantize_gain_vector_q16(&g_hat, l_hat)?;
    let higher_order = quantize_higher_order_coefficients_q16(&dct_blocks, l_hat)?;

    let u = prioritize_bits(
        b0,
        b1,
        k_hat,
        b2 as u32,
        gain_vector,
        &higher_order,
        sync_bit,
    )?;

    // The decoder's own history for the next frame's prediction: this frame's quantizer values run back through the
    // fixed dequantization.
    let gain_values: [u32; 5] = std::array::from_fn(|i| gain_vector[i].0);
    let higher_order_values: Vec<u32> = higher_order.iter().map(|&(v, _)| v).collect();
    let reconstructed = reconstruct_spectral_amplitudes_q16(
        b2,
        gain_values,
        &higher_order_values,
        l_hat,
        previous_state.l_hat,
        &previous_state.spectral_amplitudes_q16,
    )?;

    Some((
        u,
        FrameState {
            xi_max_q16,
            l_hat,
            voiced,
            spectral_amplitudes_q16: reconstructed,
        },
    ))
}
