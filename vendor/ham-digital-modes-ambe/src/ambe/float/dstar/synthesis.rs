// SPDX-License-Identifier: LGPL-3.0-or-later
//! D-STAR bits-to-PCM decoding: `parse_frame` (FEC + de-whitening) -> `dequantize` -> the shared
//! [`super::super::mbe_synthesis::MbeSynthesizer`]. Tone frames (DTMF / single tone) are recognized
//! by `dequantize` and synthesized as sinusoids via [`ToneSynthesizer`].

use super::decode::{
    classify_b0, classify_tone_index, dequantize, dtmf_digit_from_tone_index,
    extract_raw_parameters, parse_frame, DStarDecoderState, DequantizedFrame, FrameKind, ToneKind,
};
use crate::ambe::float::concealment::{ConcealParams, Concealer, Decision, Descriptor};
use crate::ambe::float::mbe_synthesis::MbeSynthesizer;
use crate::ambe::float::mbe_synthesis::{BadFrameAction, ErrorPolicy};
use crate::ambe::float::tia_102_baba::unvoiced_synthesis::N;
use crate::ambe::float::tone_synthesis::{dstar_tone_amplitude, ToneSynthesizer};

pub struct DStarSynthesisDecoder {
    dequant: DStarDecoderState,
    synth: MbeSynthesizer,
    tone: ToneSynthesizer,
    /// Consecutive frames repeated because of channel errors (mbelib's `repeat`).
    repeats: u32,
    error_policy: ErrorPolicy,
    concealer: Concealer,
}

impl DStarSynthesisDecoder {
    pub fn new() -> Self {
        Self {
            dequant: DStarDecoderState::initial(),
            synth: MbeSynthesizer::new(),
            tone: ToneSynthesizer::new(),
            repeats: 0,
            error_policy: ErrorPolicy::default(),
            concealer: Concealer::new(ConcealParams::default()),
        }
    }

    /// Selects how channel errors are handled (see [`ErrorPolicy`]); the default is [`ErrorPolicy::Concealing`].
    pub fn with_error_policy(mut self, policy: ErrorPolicy) -> Self {
        self.error_policy = policy;
        self
    }

    /// Replaces the concealment constants (used to tune them; the defaults are the tuned set).
    pub fn with_conceal_params(mut self, params: ConcealParams) -> Self {
        self.concealer = Concealer::new(params);
        self
    }

    /// The raw (unprotected) bits whose corruption costs the most audio and that are considered for single-bit correction:
    /// the voicing pattern `d[38..42)`, the low gain bits `d[42..44)`, the low bits of the two spectral vectors `d[44..48)` and the
    /// pitch's lowest bit `d[48]` (`examples/ambe_bit_sensitivity.rs`).
    const CORRECTABLE_BITS: [usize; 11] = [38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48];

    fn decode_concealing(&mut self, parsed: &super::decode::ParsedFrame) -> Option<[f64; N]> {
        let mut candidates: Vec<(u32, Option<Descriptor>)> = Vec::new();
        let mut readings = Vec::new();
        for bit in std::iter::once(None).chain(Self::CORRECTABLE_BITS.iter().map(|&b| Some(b))) {
            let d = bit.map_or(parsed.d, |b| parsed.d ^ (1u64 << (48 - b)));
            let mut state = DStarDecoderState {
                l: self.dequant.l,
                log2_ml: self.dequant.log2_ml.clone(),
                gamma: self.dequant.gamma,
            };
            match dequantize(d, &mut state) {
                DequantizedFrame::Speech(p) => {
                    candidates.push((
                        bit.is_some() as u32,
                        Some(Descriptor::new(p.w0, &p.voiced, &p.ml)),
                    ));
                    readings.push(Some((p, state)));
                }
                _ => {
                    candidates.push((bit.is_some() as u32, None));
                    readings.push(None);
                }
            }
        }
        match self
            .concealer
            .decide(&candidates, parsed.epsilon_c0 + parsed.epsilon_c1)
        {
            Decision::Accept(i) => {
                let (p, state) = readings.swap_remove(i)?;
                self.dequant = state;
                self.tone.reset();
                self.synth.synthesize_speech(
                    p.w0,
                    &p.voiced,
                    &p.ml,
                    parsed.epsilon_c0,
                    parsed.epsilon_c1,
                )
            }
            Decision::Repeat { scale, reset } => {
                if reset {
                    self.dequant = DStarDecoderState::initial();
                    self.synth = MbeSynthesizer::new();
                    return Some([0.0; N]);
                }
                if scale == 0.0 {
                    return Some([0.0; N]);
                }
                self.synth.synthesize_repeat_scaled(scale)
            }
        }
    }

    /// Decodes one logical 72-bit frame (see `interleave::wire_bytes_to_frame`) into 20 ms of PCM.
    pub fn decode_frame(&mut self, logical_frame: u128) -> Option<[f64; N]> {
        let parsed = parse_frame(logical_frame);
        // mbelib's bad-frame policy (`mbe_processAmbe2400Dataf`): a speech frame with more than 3 corrected errors
        // reuses the previous frame's parameters without touching the predictor state, and after 3 such repeats in a
        // row the decoder mutes (silence) and reinitializes.
        let b0 = extract_raw_parameters(parsed.d).b0;
        let is_speech = classify_b0(b0) == FrameKind::Speech;
        if self.error_policy == ErrorPolicy::Concealing && is_speech {
            return self.decode_concealing(&parsed);
        }
        // The chip treats the reserved pitch codes 125 and 127 as invalid frames (repeat three times, then near-silence). Normal
        // operation stays lenient: a tone frame whose uncoded pitch bit flipped is better decoded as the tone.
        let chip_invalid =
            self.error_policy == ErrorPolicy::ChipCompatible && matches!(b0, 125 | 127);
        if (is_speech
            && self
                .error_policy
                .is_bad(parsed.epsilon_c0, parsed.epsilon_c1))
            || chip_invalid
        {
            self.repeats += 1;
            let action = match self.error_policy.bad_frame_action(self.repeats) {
                BadFrameAction::Decode if chip_invalid => BadFrameAction::Mute,
                other => other,
            };
            match action {
                BadFrameAction::Repeat => return self.synth.synthesize_repeat(),
                BadFrameAction::Mute => {
                    self.dequant = DStarDecoderState::initial();
                    self.synth = MbeSynthesizer::new();
                    return Some(self.synth.synthesize_silence());
                }
                BadFrameAction::Decode => {}
            }
        } else {
            self.repeats = 0;
        }
        match dequantize(parsed.d, &mut self.dequant) {
            DequantizedFrame::Speech(p) => {
                self.tone.reset();
                self.synth.synthesize_speech(
                    p.w0,
                    &p.voiced,
                    &p.ml,
                    parsed.epsilon_c0,
                    parsed.epsilon_c1,
                )
            }
            DequantizedFrame::Tone(t) => match classify_tone_index(t.index) {
                ToneKind::Single { hz } => {
                    Some(self.tone.synthesize(&[hz], dstar_tone_amplitude(t.volume)))
                }
                ToneKind::Dual => match dtmf_digit_from_tone_index(t.index) {
                    Some((row, col)) => {
                        Some(self.tone.dtmf(row, col, dstar_tone_amplitude(t.volume)))
                    }
                    // Dual-tone codes 144..=163: meaning unidentified, so emit silence.
                    None => Some([0.0; N]),
                },
                ToneKind::Invalid => Some([0.0; N]),
            },
        }
    }
}

impl Default for DStarSynthesisDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambe::float::dstar::decode::RawParameters;
    use crate::ambe::float::dstar::encode::{build_frame, pack_raw_parameters};

    #[test]
    fn mbelib_bad_frame_policy_repeats_three_times_then_mutes() {
        let raw = RawParameters {
            b0: 40,
            b1: 15,
            b2: 12,
            b3: 100,
            b4: 50,
            b5: 3,
            b6: 4,
            b7: 5,
            b8: 2,
        };
        let clean = build_frame(pack_raw_parameters(&raw));
        // Two flipped bits in each of C0 (bits 71..49) and C1 (bits 48..26): four corrected errors in total.
        let bad = clean ^ (1u128 << 70) ^ (1u128 << 60) ^ (1u128 << 45) ^ (1u128 << 35);
        let parsed = parse_frame(bad);
        assert!(
            parsed.epsilon_c0 + parsed.epsilon_c1 > 3,
            "test frame must exceed the error threshold"
        );

        let mut dec = DStarSynthesisDecoder::new().with_error_policy(ErrorPolicy::Clean);
        let first = dec.decode_frame(clean).unwrap();
        assert!(first.iter().any(|&s| s != 0.0));
        for i in 0..3 {
            let repeated = dec.decode_frame(bad).unwrap();
            assert!(
                repeated.iter().any(|&s| s != 0.0),
                "repeat {i} should still synthesize"
            );
        }
        let muted = dec.decode_frame(bad).unwrap();
        assert!(
            muted.iter().all(|&s| s == 0.0),
            "the fourth consecutive bad frame mutes"
        );
        // A clean frame afterwards decodes normally again.
        assert!(dec.decode_frame(clean).unwrap().iter().any(|&s| s != 0.0));
    }
}
