// SPDX-License-Identifier: LGPL-3.0-or-later
//! Streaming PCM-to-frame encoder for D-STAR's AMBE: shared pitch analysis
//! ([`crate::ambe::float::tia_102_baba::encoder::FrameAnalyzer`]), voicing/amplitude analysis at the quantized pitch
//! ([`crate::ambe::float::mbe_encode::analyze_at_pitch`]), then the inverse of this mode's dequantization chain
//! ([`crate::ambe::float::mbe_encode::quantize_speech`]). A mirror of the decoder's own state is advanced by
//! actually dequantizing each emitted frame, so the encoder's predictions always match what a decoder will hold.
//! Frames whose 20 ms slot is a DTMF digit or a sustained single tone are emitted as tone frames (see
//! [`crate::ambe::float::tone_detect`]), as the chip's own detector does. Emits logical 72-bit frames (see [`super::interleave::frame_to_wire_bytes`] for the chip's wire layout).

use super::decode::{dequantize, DStarDecoderState, RawParameters};
use super::encode::{build_frame, build_tone_frame, pack_raw_parameters};
use super::quantize::quantize_pitch;
use super::tables;
use crate::ambe::float::mbe_encode::{
    analyze_at_pitch, quantize_speech, AnalysisState, ModeTables, PrevState, SpeechTarget,
};
use crate::ambe::float::tia_102_baba::encoder::{FrameAnalyzer, HighPassFilter};
use crate::ambe::float::tone_detect::{detect_tone, volume_for_amplitude, DetectedTone};

pub struct Encoder {
    analyzer: FrameAnalyzer,
    high_pass: HighPassFilter,
    mirror: DStarDecoderState,
    analysis: AnalysisState,
}

impl Encoder {
    /// Analysis-centre offset (samples) at which this encoder's pitch track lines up with the chip encoder's: voiced
    /// frames agree within 2 pitch steps 66% of the time at -80 (`examples/dstar_chip_encode_compare.rs`, `DUMP_B0`), and
    /// the median difference is 0; at the default 0 the best alignment needs a one-frame shift.
    pub const CHIP_ALIGNED_CENTER_OFFSET: i32 = -80;

    // The chip encoder parks every all-unvoiced frame on b0 = 34, but copying that convention lowers the envelope
    // correlation of our stream through the chip's decoder (0.870 -> 0.865 at the best alignment, 0.819 -> 0.707 at
    // -80) even though it raises pitch-index agreement (40% -> 60% within 2 steps), so it is deliberately not copied.

    pub fn new() -> Self {
        let mut analyzer = FrameAnalyzer::new();
        analyzer.set_center_offset(Self::CHIP_ALIGNED_CENTER_OFFSET);
        Self {
            analyzer,
            high_pass: HighPassFilter::default(),
            mirror: DStarDecoderState::initial(),
            analysis: AnalysisState::new(),
        }
    }

    pub fn set_center_offset(&mut self, samples: i32) {
        self.analyzer.set_center_offset(samples);
    }

    /// # Panics
    /// If any sample is NaN or infinite: garbage input must fail loudly, not become a confident-looking frame.
    pub fn push_samples(&mut self, samples: &[f64]) {
        assert!(
            samples.iter().all(|s| s.is_finite()),
            "encoder input contains a non-finite sample"
        );
        // The standard's input high-pass filter (Eq. 3) removes DC offset, which otherwise corrupts the pitch estimate.
        let filtered: Vec<f64> = samples
            .iter()
            .map(|&x| (self.high_pass.step(x) + 0.5).floor())
            .collect();
        self.analyzer.push_samples(&filtered);
    }

    /// The next 72-bit logical frame if enough lookahead has been pushed.
    pub fn next_frame(&mut self) -> Option<u128> {
        let a = self.analyzer.next_analysis()?;
        if let Some(det) = detect_tone(&a.slot_samples) {
            let index = match det.tone {
                DetectedTone::Dtmf { row, col } => 128 + row as u32 + 4 * col as u32,
                DetectedTone::Single { index, .. } => index,
            };
            return Some(build_tone_frame(index, volume_for_amplitude(det.amplitude)));
        }
        let b0 = quantize_pitch(a.omega0_hat);
        let l = tables::L_TABLE[b0 as usize];
        let f0 = super::decode::f0_from_b0(b0);
        let w0 = f0 * 2.0 * std::f64::consts::PI;
        let (voiced, ml) = analyze_at_pitch(&a, w0, l, &mut self.analysis);

        let mode = ModeTables {
            vuv: &tables::VUV,
            dg: &tables::DG,
            prba24: &tables::PRBA24,
            prba58: &tables::PRBA58,
            lmprbl: &tables::LMPRBL,
            hoc: [
                &tables::HOC_B5,
                &tables::HOC_B6,
                &tables::HOC_B7,
                &tables::HOC_B8,
            ],
            hoc_b8_even_only: true,
            rho: crate::ambe::float::dstar::decode::PREDICTOR_RHO,
            gamma_scale: crate::ambe::float::dstar::decode::GAMMA_SCALE,
            gamma_memory: crate::ambe::float::dstar::decode::GAMMA_MEMORY,
        };
        let q = quantize_speech(
            &SpeechTarget {
                l,
                w0,
                vuv_f0: f0,
                voiced: &voiced,
                ml: &ml,
            },
            &PrevState {
                l: self.mirror.l,
                log2_ml: &self.mirror.log2_ml,
                gamma: self.mirror.gamma,
            },
            &mode,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambe::float::dstar::decode::{parse_frame, DequantizedFrame};

    /// Encoding a harmonic signal, then dequantizing every emitted frame with the real decoder, reproduces the
    /// signal's pitch and a spectral envelope whose level tracks the signal's (VQ quantization limits the shape).
    #[test]
    fn encoder_round_trip_recovers_pitch_and_level() {
        let period = 50.0;
        let signal: Vec<f64> = (0..160 * 30)
            .map(|i| {
                (1..=8)
                    .map(|h| {
                        1500.0 / h as f64
                            * (2.0 * std::f64::consts::PI * h as f64 * i as f64 / period).sin()
                    })
                    .sum()
            })
            .collect();
        let mut enc = Encoder::new();
        enc.push_samples(&signal);
        let mut frames = Vec::new();
        while let Some(f) = enc.next_frame() {
            frames.push(f);
        }
        frames.extend(enc.finish());
        assert!(frames.len() >= 25);

        let mut state = DStarDecoderState::initial();
        let mut checked = 0;
        for (i, &frame) in frames.iter().enumerate() {
            let parsed = parse_frame(frame);
            assert_eq!(
                parsed.epsilon_c0 + parsed.epsilon_c1,
                0,
                "a freshly built frame must be error free"
            );
            if let DequantizedFrame::Speech(p) = dequantize(parsed.d, &mut state) {
                if (6..24).contains(&i) {
                    let p_est = 2.0 * std::f64::consts::PI / p.w0;
                    assert!(
                        (p_est / period - 1.0).abs() < 0.05,
                        "frame {i}: decoded period {p_est}"
                    );
                    // The strongest harmonics (1-8) should carry real energy.
                    let peak = p.ml[1..=8.min(p.l as usize)]
                        .iter()
                        .cloned()
                        .fold(0.0, f64::max);
                    assert!(
                        peak > 100.0,
                        "frame {i}: peak harmonic amplitude {peak} implausibly small"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked >= 15);
    }

    #[test]
    fn dtmf_and_single_tones_are_emitted_as_tone_frames_with_the_chips_index_and_level() {
        use crate::ambe::float::dstar::decode::{
            classify_b0, decode_tone, extract_raw_parameters, FrameKind,
        };
        let sine = |freqs: &[f64], amp: f64| -> Vec<f64> {
            (0..160 * 12)
                .map(|i| {
                    freqs
                        .iter()
                        .map(|&hz| {
                            amp * (2.0 * std::f64::consts::PI * hz * i as f64 / 8000.0).sin()
                        })
                        .sum()
                })
                .collect()
        };
        // Measured on the chip: DTMF '5' (770 + 1336 Hz) at amplitude 4000 -> index 133, volume 186;
        // 1 kHz at amplitude 12000 -> index 32, volume 213.
        for (signal, index, volume) in [
            (sine(&[770.0, 1336.0], 4000.0), 133u32, 186u32),
            (sine(&[1000.0], 12000.0), 32, 213),
        ] {
            let mut enc = Encoder::new();
            enc.push_samples(&signal);
            let mut frames = Vec::new();
            while let Some(f) = enc.next_frame() {
                frames.push(f);
            }
            assert!(frames.len() >= 8);
            for &f in &frames {
                let d = parse_frame(f).d;
                assert_eq!(classify_b0(extract_raw_parameters(d).b0), FrameKind::Tone);
                let t = decode_tone(d);
                assert_eq!(t.index, index);
                assert!(
                    (t.volume as i32 - volume as i32).abs() <= 1,
                    "volume {} vs {}",
                    t.volume,
                    volume
                );
            }
        }
    }
}
