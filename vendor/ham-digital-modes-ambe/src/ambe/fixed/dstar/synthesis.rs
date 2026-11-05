// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`crate::ambe::float::dstar::synthesis`]: D-STAR bits-to-PCM decoding.
//! `parse_frame` (FEC + de-whitening, pure integer, reused from the float tree) -> fixed
//! `dequantize` -> the shared fixed [`MbeSynthesizer`]; tone frames (DTMF / single tone) go through
//! the fixed [`ToneSynthesizer`]. The mbelib bad-frame policy is identical to the float decoder: a
//! speech frame with more than 3 corrected errors repeats the previous parameters (3 times at
//! most) and then mutes and reinitializes. Output is `[i64; N]` Q16.16 PCM.

use super::decode::{
    classify_b0, classify_tone_index, dequantize, dtmf_digit_from_tone_index,
    extract_raw_parameters, DequantizedFrame, ToneKind,
};
use crate::ambe::fixed::general::concealment::{ConcealParams, Concealer, Decision, Descriptor};
use crate::ambe::fixed::general::mbe_speech::MbeDecoderState;
use crate::ambe::fixed::general::mbe_synthesis::MbeSynthesizer;
use crate::ambe::fixed::general::tone_synthesis::{dstar_tone_amplitude_q16, ToneSynthesizer};
use crate::ambe::fixed::general::unvoiced_synthesis::N;
use crate::ambe::float::dstar::decode::{parse_frame, FrameKind};
use crate::ambe::float::mbe_synthesis::{BadFrameAction, ErrorPolicy};

pub struct DStarSynthesisDecoder {
    dequant: MbeDecoderState,
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
            dequant: MbeDecoderState::initial(),
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

    /// The raw bits considered for single-bit correction (same set as the float decoder).
    const CORRECTABLE_BITS: [usize; 11] = [38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48];

    fn decode_concealing(
        &mut self,
        parsed: &crate::ambe::float::dstar::decode::ParsedFrame,
    ) -> Option<[i64; N]> {
        let mut candidates: Vec<(u32, Option<Descriptor>)> = Vec::new();
        let mut readings = Vec::new();
        for bit in std::iter::once(None).chain(Self::CORRECTABLE_BITS.iter().map(|&b| Some(b))) {
            let d = bit.map_or(parsed.d, |b| parsed.d ^ (1u64 << (48 - b)));
            let mut state = MbeDecoderState {
                l: self.dequant.l,
                log2_ml_q16: self.dequant.log2_ml_q16.clone(),
                gamma_q16: self.dequant.gamma_q16,
            };
            match dequantize(d, &mut state) {
                DequantizedFrame::Speech(p) => {
                    candidates.push((
                        bit.is_some() as u32,
                        Some(Descriptor::new(p.w0_q16, &p.voiced, &p.ml_q16)),
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
                    p.w0_q32,
                    &p.voiced,
                    &p.ml_q16,
                    parsed.epsilon_c0,
                    parsed.epsilon_c1,
                )
            }
            Decision::Repeat { scale_q16, reset } => {
                if reset {
                    self.dequant = MbeDecoderState::initial();
                    self.synth = MbeSynthesizer::new();
                    return Some([0; N]);
                }
                if scale_q16 == 0 {
                    return Some([0; N]);
                }
                self.synth.synthesize_repeat_scaled(scale_q16)
            }
        }
    }

    /// Decodes one logical 72-bit frame (see the float `interleave::wire_bytes_to_frame`) into 20 ms
    /// of Q16.16 PCM.
    pub fn decode_frame(&mut self, logical_frame: u128) -> Option<[i64; N]> {
        let parsed = parse_frame(logical_frame);
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
                    self.dequant = MbeDecoderState::initial();
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
                    p.w0_q32,
                    &p.voiced,
                    &p.ml_q16,
                    parsed.epsilon_c0,
                    parsed.epsilon_c1,
                )
            }
            DequantizedFrame::Tone(t) => {
                let amplitude_q16 = dstar_tone_amplitude_q16(t.volume);
                Some(match classify_tone_index(t.index) {
                    ToneKind::Single { hz_q16 } => self.tone.synthesize(&[hz_q16], amplitude_q16),
                    ToneKind::Dual => match dtmf_digit_from_tone_index(t.index) {
                        Some((row, col)) => self.tone.dtmf(row, col, amplitude_q16),
                        // Dual-tone codes 144..=163: meaning unidentified, so emit silence.
                        None => [0; N],
                    },
                    ToneKind::Invalid => [0; N],
                })
            }
        }
    }
}

impl Default for DStarSynthesisDecoder {
    fn default() -> Self {
        Self::new()
    }
}
