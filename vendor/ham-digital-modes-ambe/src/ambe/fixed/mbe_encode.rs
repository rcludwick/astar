// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point voicing and amplitude analysis at a *given* (already quantized) pitch, for the D-STAR and
//! AMBE+2 encoders: fixed-point sibling of `ambe::float::mbe_encode::analyze_at_pitch`. The quantization half of that
//! float module is `general::mbe_encode::quantize_speech`.

use crate::ambe::fixed::general::fixed_ops::{div_q16, mul_q16, TWO_PI_Q16_16};
use crate::ambe::fixed::general::mbe_encode::{
    quantize_speech, target_log2_ml_q16, ModeTables, PrevState, QuantizedSpeech, SpeechTarget,
};
use crate::ambe::fixed::general::mbe_speech::MbeDecoderState;
use crate::ambe::fixed::tia_102_baba::encoder::FrameAnalysis;
use crate::ambe::fixed::tia_102_baba::pitch_refinement::{Pitch, RefinementFrame};
use crate::ambe::fixed::tia_102_baba::spectral_amplitude::estimate_spectral_amplitudes_q16;
use crate::ambe::fixed::tia_102_baba::vuv::{
    determine_voicing, frequency_bands_count, XI_MAX_INITIAL_Q16,
};

/// Frame-to-frame state of the voicing/amplitude analysis (energy tracker, `Q16`, and the previous
/// frame's band decisions).
pub struct AnalysisState {
    xi_max_q16: i128,
    prev_bands: Vec<bool>,
}

impl AnalysisState {
    pub fn new() -> Self {
        Self {
            xi_max_q16: XI_MAX_INITIAL_Q16,
            prev_bands: Vec::new(),
        }
    }
}

impl Default for AnalysisState {
    fn default() -> Self {
        Self::new()
    }
}

/// Voicing per harmonic (`voiced[h]`, index 0 unused) and amplitudes `ml[h]` (Q16, index 0 unused)
/// for harmonics `1..=l` at `pitch`, mirroring the float `analyze_at_pitch` exactly: bands from
/// [`determine_voicing`] are padded to `K_hat(l)` with the last decision, then each harmonic takes
/// its band's flag.
pub fn analyze_at_pitch(
    refinement: &RefinementFrame,
    initial_pitch_error_q16: i32,
    pitch: &Pitch,
    l: u32,
    state: &mut AnalysisState,
) -> (Vec<bool>, Vec<i64>) {
    let (bands, xi_max) = determine_voicing(
        refinement,
        pitch,
        initial_pitch_error_q16,
        state.xi_max_q16,
        &state.prev_bands,
    );
    state.xi_max_q16 = xi_max;
    state.prev_bands = bands.clone();
    let k_hat = frequency_bands_count(l) as usize;
    let mut padded = bands;
    let fill = padded.last().copied().unwrap_or(false);
    while padded.len() < k_hat {
        padded.push(fill);
    }
    let amplitudes = estimate_spectral_amplitudes_q16(refinement, l, k_hat as u32, pitch, &padded);
    let mut voiced = vec![false; l as usize + 1];
    let mut ml = vec![0i64; l as usize + 1];
    for h in 1..=l as usize {
        let band = h.div_ceil(3).clamp(1, k_hat);
        voiced[h] = padded[band - 1];
        ml[h] = amplitudes[h - 1];
    }
    (voiced, ml)
}

/// [`analyze_at_pitch`] for a [`FrameAnalysis`] from the streaming analyzer, at `pitch` (the frame's
/// own refined pitch, or a decoder-quantized one) and harmonic count `l`.
pub fn analyze_frame_at_pitch(
    frame: &FrameAnalysis,
    pitch: &Pitch,
    l: u32,
    state: &mut AnalysisState,
) -> (Vec<bool>, Vec<i64>) {
    analyze_at_pitch(
        &frame.refinement,
        frame.initial_pitch_error_q16,
        pitch,
        l,
        state,
    )
}

/// Analysis and quantization of one frame at an already-chosen pitch, shared by the D-STAR and AMBE+2 streaming
/// encoders: analyses voicing/amplitudes at the decoder's own `w0` (`w0_q32`, radians/sample Q32, the table entry for
/// the chosen `b0`), converts them to the decoder's log2 domain and quantizes them against `prev`, the mirror of the
/// decoder state. `f0_q16` is the table fundamental in cycles/sample (Q16.16). `w0` and the V/UV pitch are derived
/// exactly as the fixed decoder derives them (`mul_q16(f0, 2*pi)` and `div_q16(w0, 2*pi)`), so the search inverts the
/// fixed decoder.
pub fn analyze_and_quantize(
    frame: &FrameAnalysis,
    l: u32,
    f0_q16: i32,
    w0_q32: i64,
    tables: &ModeTables,
    prev: &MbeDecoderState,
    state: &mut AnalysisState,
) -> QuantizedSpeech {
    let w0_q16 = mul_q16(f0_q16, TWO_PI_Q16_16);
    let vuv_f0_q16 = div_q16(w0_q16, TWO_PI_Q16_16);
    let pitch = Pitch::from_omega0_q30(w0_q32 >> 2);
    let (voiced, ml) = analyze_frame_at_pitch(frame, &pitch, l, state);
    let ml_q16: Vec<i32> = ml
        .iter()
        .map(|&m| m.clamp(0, i32::MAX as i64) as i32)
        .collect();
    let log2_ml_q16 = target_log2_ml_q16(w0_q16, &voiced, &ml_q16);
    quantize_speech(
        &SpeechTarget {
            l,
            vuv_f0_q16,
            voiced: &voiced,
            ml_q16: &ml_q16,
            log2_ml_q16: &log2_ml_q16,
        },
        &PrevState::from_decoder_state(prev),
        tables,
    )
}
