// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`super::super::float::tia_102_baba::decode`] -- the top-level bitstream decoder,
//! turning a received 144-bit frame into a synthesized 20 ms PCM frame. See the float sibling's own
//! doc comment for the full decode order and the reasoning behind each step (demodulation of
//! `c[1..6]` per Eq. 84-94, the mute-before-repeat check order, etc.) -- unchanged here, since none of that
//! reasoning is about numeric precision. The only things genuinely re-derived for fixed point are the
//! pieces [`super::super::mod`]'s own doc comment already calls out: parameter dequantization and
//! synthesis math. Everything else (Golay/Hamming FEC, bit prioritization/deprioritization, the
//! Annex F/G tables, per-band-to-per-harmonic voicing expansion, `K~`'s own formula) is pure
//! integer/bitwise arithmetic in the float sibling already and is reused directly via the imports
//! below, not duplicated.

use super::error_estimation::{
    estimate_errors_q16, should_mute_frame_q16, should_repeat_frame_q16, FrameErrorsQ16,
};
use super::parameter_encoding::{
    decode_voicing_decisions_per_harmonic, dequantize_fundamental_frequency_q16,
    dequantize_fundamental_frequency_q32, frequency_bands_count, harmonics_count_from_b0,
};
use super::prediction::INITIAL_L_HAT_PREV;
use super::reconstruct::reconstruct_spectral_amplitudes_q16;
use super::synthesis::SynthesisState;
use crate::ambe::fixed::general::unvoiced_synthesis::N;
use crate::ambe::float::tia_102_baba::bit_prioritization::{
    deprioritize_bits, extract_fundamental_frequency_quantizer, DeprioritizedBits,
};
use crate::ambe::float::tia_102_baba::tables::{gain_bit_allocation, higher_order_bit_allocation};
use crate::ambe::float::tia_102_baba::{decode_code_vectors, decode_code_vectors_chip};

/// `round(1.0 * 65536)` -- Annex A's own `M~_l(-1) = 1` (unity, not silent) initial history value, in
/// Q16.16.
const ONE_Q16_16: i32 = 65536;

/// The fixed-point equivalent of `DecodedParameters`.
pub struct DecodedParameters {
    pub omega0_tilde_q16: i32,
    /// `omega0_tilde` at Q32 radians/sample -- the precision synthesis uses.
    pub omega0_tilde_q32: i64,
    pub l_hat: u32,
    pub k_hat: u32,
    pub bits: DeprioritizedBits,
    pub voiced: Vec<bool>,
    pub reconstructed_amplitudes_q16: Vec<i32>,
    pub errors: FrameErrorsQ16,
}

/// The fixed-point equivalent of `FrameOutcome`.
pub enum FrameOutcome {
    Repeat,
    Mute,
    Decoded(DecodedParameters),
}

/// `b_hat_0` values `208..=255` are reserved/unused -- see the float sibling's own doc comment.
const MAX_VALID_B0: u32 = 207;

/// The fixed-point equivalent of `DecoderState`. `error_rate_prev` is `i32` Q16.16, matching
/// `estimate_errors_q16`'s own domain; `spectral_amplitudes_prev` is `Vec<i32>` Q16.16, seeded to
/// [`ONE_Q16_16`] per Annex A, matching the float sibling's own `1.0`.
pub struct DecoderState {
    synthesis: SynthesisState,
    l_hat_prev: u32,
    spectral_amplitudes_prev: Vec<i32>,
    error_rate_prev_q16: i32,
    /// Decode the DVSI chip's framing instead of the standard's (see the float sibling's `new_chip_wire`).
    chip_wire: bool,
}

impl DecoderState {
    /// A decoder for the standard's wire layer (demodulation plus textbook FEC, Eq. 81-94).
    pub fn new() -> Self {
        let l_hat_prev = INITIAL_L_HAT_PREV;
        Self {
            synthesis: SynthesisState::new(),
            l_hat_prev,
            spectral_amplitudes_prev: vec![ONE_Q16_16; l_hat_prev as usize],
            error_rate_prev_q16: 0,
            chip_wire: false,
        }
    }

    /// The DVSI chip's framing (plain FEC codewords, chip Hamming labelling, no modulation) for the
    /// chip-comparison tools; the same choice as the float sibling's `DecoderState::new_chip_wire`.
    pub fn new_chip_wire() -> Self {
        Self {
            chip_wire: true,
            ..Self::new()
        }
    }

    /// The fixed-point equivalent of `DecoderState::advance_history`.
    pub fn advance_history(&mut self, params: &DecodedParameters) {
        self.l_hat_prev = params.l_hat;
        self.spectral_amplitudes_prev = params.reconstructed_amplitudes_q16.clone();
    }

    /// The fixed-point equivalent of `DecoderState::decode_parameters` -- see the float sibling's own
    /// doc comment for the full step-by-step derivation this mirrors exactly.
    pub fn decode_parameters(&mut self, c: [u32; 8]) -> Option<FrameOutcome> {
        // Same wire layer choice as the float sibling: the standard's demodulate-then-FEC-decode, or
        // (chip-comparison tools only) the DVSI chip's framing. Pure integer code either way.
        let (u_vectors, corrected_error_counts) = if self.chip_wire {
            decode_code_vectors_chip(c)
        } else {
            decode_code_vectors(c)
        };
        let errors = estimate_errors_q16(&corrected_error_counts, self.error_rate_prev_q16);
        self.error_rate_prev_q16 = errors.rate_q16;

        let b0 = extract_fundamental_frequency_quantizer(&u_vectors);

        if should_mute_frame_q16(&errors) {
            return Some(FrameOutcome::Mute);
        }

        if b0 > MAX_VALID_B0 || should_repeat_frame_q16(&errors) {
            return Some(FrameOutcome::Repeat);
        }

        let omega0_tilde_q16 = dequantize_fundamental_frequency_q16(b0);
        let omega0_tilde_q32 = dequantize_fundamental_frequency_q32(b0);
        let l_hat = harmonics_count_from_b0(b0);
        let k_hat = frequency_bands_count(l_hat);

        let gain_widths: [u8; 5] =
            std::array::from_fn(|i| gain_bit_allocation(l_hat, i as u32 + 2).unwrap().0);
        let higher_widths: Vec<u8> = higher_order_bit_allocation(l_hat)?
            .iter()
            .copied()
            .filter(|&w| w > 0)
            .collect();

        let bits = deprioritize_bits(u_vectors, k_hat, gain_widths, &higher_widths)?;

        let voiced = decode_voicing_decisions_per_harmonic(bits.b1, k_hat, l_hat);

        let gain_values: [u32; 5] = std::array::from_fn(|i| bits.gain_vector[i].0);
        let higher_order_values: Vec<u32> = bits.higher_order.iter().map(|&(v, _)| v).collect();
        let reconstructed_amplitudes_q16 = reconstruct_spectral_amplitudes_q16(
            bits.b2 as u8,
            gain_values,
            &higher_order_values,
            l_hat,
            self.l_hat_prev,
            &self.spectral_amplitudes_prev,
        )?;

        Some(FrameOutcome::Decoded(DecodedParameters {
            omega0_tilde_q16,
            omega0_tilde_q32,
            l_hat,
            k_hat,
            bits,
            voiced,
            reconstructed_amplitudes_q16,
            errors,
        }))
    }

    /// The fixed-point equivalent of `DecoderState::decode_frame`.
    pub fn decode_frame(&mut self, c: [u32; 8]) -> Option<[i64; N]> {
        match self.decode_parameters(c)? {
            FrameOutcome::Repeat => self.synthesis.synthesize_repeated_frame(),
            FrameOutcome::Mute => Some(self.synthesis.synthesize_comfort_frame()),
            FrameOutcome::Decoded(params) => {
                let pcm = self.synthesis.synthesize_frame(
                    &params.reconstructed_amplitudes_q16,
                    params.omega0_tilde_q32,
                    &params.voiced,
                    &params.errors,
                )?;

                self.l_hat_prev = params.l_hat;
                self.spectral_amplitudes_prev = params.reconstructed_amplitudes_q16;

                Some(pcm)
            }
        }
    }
}

impl Default for DecoderState {
    fn default() -> Self {
        Self::new()
    }
}
