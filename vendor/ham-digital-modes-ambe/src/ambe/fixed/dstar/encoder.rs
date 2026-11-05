// SPDX-License-Identifier: LGPL-3.0-or-later
//! Streaming fixed-point PCM-to-frame encoder for D-STAR's AMBE: fixed-point sibling of
//! [`crate::ambe::float::dstar::encoder::Encoder`], with the same public surface (`new`, `set_center_offset`,
//! `push_samples`, `next_frame`, `finish`) and the same u128 logical 72-bit frames. Pipeline: the fixed streaming
//! pitch analyzer ([`crate::ambe::fixed::tia_102_baba::encoder::FrameAnalyzer`]), tone detection
//! ([`crate::ambe::fixed::general::tone_detect`]), the chip pitch map (`f0 = 2^(-4.258618 - 0.021766 * b0)`, as
//! [`super::encode::quantize_pitch_p8`] and the `W0_TABLE_*` tables), voicing/amplitude analysis at the quantized pitch
//! ([`crate::ambe::fixed::mbe_encode`]) and the fixed quantizer ([`crate::ambe::fixed::general::mbe_encode`]). A
//! mirror of the fixed decoder state is advanced by dequantizing each emitted frame.
//!
//! Input is 16-bit PCM (`&[i16]`). Everything is integer arithmetic; see `tests/ambe_fixed_dstar_encoder.rs` for
//! the measured agreement with the float encoder on real speech.

use super::decode::dequantize;
use super::encode::{
    build_frame, build_tone_frame, mode_tables, pack_raw_parameters, quantize_pitch_p8,
    RawParameters,
};
use super::tables_q16::{W0_TABLE_Q16_16, W0_TABLE_Q32};
use crate::ambe::fixed::general::mbe_speech::MbeDecoderState;
use crate::ambe::fixed::general::tone_detect::{
    detect_tone, volume_for_amplitude_q16, DetectedTone,
};
use crate::ambe::fixed::mbe_encode::{analyze_and_quantize, AnalysisState};
use crate::ambe::fixed::tia_102_baba::encoder::{FrameAnalyzer, HighPassFilter};
use crate::ambe::float::dstar::tables::L_TABLE;

pub struct Encoder {
    analyzer: FrameAnalyzer,
    high_pass: HighPassFilter,
    mirror: MbeDecoderState,
    analysis: AnalysisState,
}

impl Encoder {
    /// Analysis-centre offset (samples) at which this encoder's pitch track lines up with the chip encoder's; see the
    /// float encoder's constant of the same name.
    pub const CHIP_ALIGNED_CENTER_OFFSET: i32 = -80;

    pub fn new() -> Self {
        let mut analyzer = FrameAnalyzer::new();
        analyzer.set_center_offset(Self::CHIP_ALIGNED_CENTER_OFFSET);
        Self {
            analyzer,
            high_pass: HighPassFilter::default(),
            mirror: MbeDecoderState::initial(),
            analysis: AnalysisState::new(),
        }
    }

    pub fn set_center_offset(&mut self, samples: i32) {
        self.analyzer.set_center_offset(samples);
    }

    pub fn push_samples(&mut self, samples: &[i16]) {
        // The standard's input high-pass filter (Eq. 3), as in the float encoder; the output is saturated back to 16 bits.
        let filtered: Vec<i16> = samples
            .iter()
            .map(|&s| {
                self.high_pass
                    .step(s as i32)
                    .clamp(i16::MIN as i32, i16::MAX as i32) as i16
            })
            .collect();
        self.analyzer.push_samples(&filtered);
    }

    /// The next 72-bit logical frame if enough lookahead has been pushed.
    pub fn next_frame(&mut self) -> Option<u128> {
        let a = self.analyzer.next_analysis()?;
        let slot: Vec<i16> = a.slot_samples.iter().map(|&s| s as i16).collect(); // lossless: pushed as i16
        if let Some(det) = detect_tone(&slot) {
            let index = match det.tone {
                DetectedTone::Dtmf { row, col } => 128 + row as u32 + 4 * col as u32,
                DetectedTone::Single { index, .. } => index,
            };
            return Some(build_tone_frame(
                index,
                volume_for_amplitude_q16(det.amplitude_q16),
            ));
        }
        let b0 = quantize_pitch_p8(a.p8);
        let l = L_TABLE[b0 as usize];
        let q = analyze_and_quantize(
            &a,
            l,
            W0_TABLE_Q16_16[b0 as usize],
            W0_TABLE_Q32[b0 as usize],
            &mode_tables(),
            &self.mirror,
            &mut self.analysis,
        );
        let raw = RawParameters {
            b0,
            b1: q.b1,
            b2: q.b2,
            b3: q.b3,
            b4: q.b4,
            b5: q.b5,
            b6: q.b6,
            b7: q.b7,
            b8: q.b8,
        };
        let d = pack_raw_parameters(&raw);
        dequantize(d, &mut self.mirror);
        Some(build_frame(d))
    }

    /// Pads with silence so every pushed sample's frame is emitted, returning the remaining frames.
    pub fn finish(&mut self) -> Vec<u128> {
        self.analyzer.finish_input();
        let mut out = Vec::new();
        while let Some(f) = self.next_frame() {
            out.push(f);
        }
        out
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}
