//! Top-level bitstream decoder: the real inverse of [`super::encode_frame`], turning a received
//! 144-bit frame (the eight code vectors `c_hat_0..c_hat_7`, or the 72 interleaved dibit symbols an
//! actual P25 channel decoder hands over -- [`super::interleave::deinterleave_from_dibit_symbols`]
//! is the bridge between the two) into a synthesized 20 ms PCM frame.
//!
//! # Decode order, and why it isn't just `encode_frame` run backwards field by field
//!
//! 1. **Golay-decode `c_hat_0`** to recover `u_hat_0` (never modulated) and its own corrected-error
//!    count `epsilon_0`.
//! 2. **Demodulate `c_hat_1..c_hat_6`** with the pseudo-random vectors seeded from the corrected
//!    `u_hat_0` (Eq. 84-94, [`super::modulation`]), then FEC-decode `c_hat_1..c_hat_3` (Golay) and
//!    `c_hat_4..c_hat_6` (Hamming, the standard's `g_H`); `u_hat_7 = c_hat_7` has no FEC. Steps 1-2 are
//!    [`super::decode_code_vectors`]. This is the standard's wire layer and matches the mbelib and
//!    kchmck `imbe.rs` decoders frame for frame
//!    (`docs/references/tia_102_baba_cross_validation.md`).
//!
//!    **History, kept because it explains [`DecoderState::new_chip_wire`]**: this decoder used to skip
//!    the demodulation and use the DVSI chip's Hamming labelling, because the codec was assumed to be
//!    what the chip's `RATET(27)` setting produces. Live chip data (`docs/references/
//!    AMBE_CHIP_VALIDATION_FINDINGS.md` section 23: the wire blocks are plain FEC codewords with a
//!    different Hamming labelling; `tests/ambe_tia_102_baba_chip_hamming_labeling.rs`) showed that
//!    setting is a different, proprietary codec, so those adaptations no longer belong in the
//!    standard's decoder. [`DecoderState::new_chip_wire`] and [`DecoderState::new_chip`] keep the chip
//!    framing for the chip-comparison tools.
//! 3. Together with `epsilon_0`, the corrected-error counts from this step feed
//!    [`super::error_estimation::estimate_errors`].
//! 4. **Bootstrap `b_hat_0`** directly from `u_hat_0`/`u_hat_7` alone
//!    ([`super::bit_prioritization::extract_fundamental_frequency_quantizer`]) -- breaking the real
//!    chicken-and-egg problem [`super::bit_prioritization::deprioritize_bits`] can't solve on its own:
//!    that function needs `k_hat` and the Annex F/G column widths just to know where the bitstream
//!    splits, but `k_hat`/`L~` are only knowable *after* decoding `b_hat_0`.
//! 5. **Check for a mute or a frame repeat** -- section 7.8's own `epsilon_R > .0875` threshold
//!    first (checked independently and first, since it's a real, separate, stricter requirement:
//!    "the decoder is required to mute," not merely a worse repeat), then section 7.7 (Eq. 97-98
//!    plus an out-of-range `b_hat_0`) -- before doing any further, parameter-dependent decoding.
//! 6. If neither: dequantize `b_hat_0` into `omega0_tilde`/`L~`/`K~`, run the full
//!    [`super::bit_prioritization::deprioritize_bits`] now that `k_hat`/widths are known, decode the
//!    per-band V/UV bits into per-harmonic ones (Eq. 50-51), reconstruct the spectral amplitudes
//!    (section 6.4), and hand everything to [`super::synthesis::SynthesisState::synthesize_frame`].
//! 7. If repeating: skip straight to
//!    [`super::synthesis::SynthesisState::synthesize_repeated_frame`] (Eq. 99-104), and leave this
//!    decoder's own `l_hat_prev`/`spectral_amplitudes_prev` untouched (Eq. 100/103's own
//!    "current = previous" reduces to "don't reassign them" here).
//! 8. If muted: the same "don't reassign them" reasoning satisfies 7.8's own required "step (2)"
//!    parameter-copy update (confirmed directly from the spec's own text: 7.8 requires exactly the
//!    frame-repeat process's step (2), Eq. 99-104), but the actual output bypasses speech synthesis
//!    entirely -- [`super::synthesis::SynthesisState::synthesize_comfort_frame`] emits real,
//!    uniform-on-`[-5,5]` comfort noise per the spec's own literal Section 7.8 text, transcribed
//!    from a 600 DPI render of TIA-102.BABA_2003.pdf page 63.

use super::bit_prioritization::{
    deprioritize_bits, extract_fundamental_frequency_quantizer, DeprioritizedBits,
};
use super::error_estimation::{
    estimate_errors, should_mute_frame, should_repeat_frame, FrameErrors,
};
use super::parameter_encoding::{
    decode_voicing_decisions_per_harmonic, dequantize_fundamental_frequency,
};
use super::prediction::INITIAL_L_HAT_PREV;
use super::reconstruct::reconstruct_spectral_amplitudes;
use super::synthesis::SynthesisState;
use super::tables::{gain_bit_allocation, higher_order_bit_allocation};
use super::unvoiced_synthesis::N;
use super::vuv::{frequency_bands_count, harmonics_count};
use super::{decode_code_vectors, decode_code_vectors_chip};
use crate::ambe::dvsi_p25fec::pitch_map::dequantize_fundamental_frequency_chip;

/// Everything [`DecoderState::decode_parameters`] recovers from a non-repeat frame, exposed on its
/// own (rather than only fed straight into synthesis) so a caller -- notably this module's own
/// round-trip test -- can assert exact parameter fidelity against what was encoded, not just that
/// synthesis produced *some* finite PCM.
pub struct DecodedParameters {
    pub omega0_tilde: f64,
    pub l_hat: u32,
    pub k_hat: u32,
    pub bits: DeprioritizedBits,
    pub voiced: Vec<bool>,
    pub reconstructed_amplitudes: Vec<f64>,
    pub errors: FrameErrors,
}

/// The three things a received frame can resolve to before synthesis: a genuine decode, a section
/// 7.7 repeat, or a section 7.8 mute -- the latter two share the *same* "step (2)" parameter-copy
/// update (Eq. 99-104, confirmed directly: 7.8's own text says "first compute the update equations
/// as listed in step (2) of the frame repeat process"), but diverge on what gets synthesized: a
/// repeat reuses the last real frame's own final parameters for actual speech synthesis; a mute
/// bypasses speech synthesis entirely and emits real comfort noise instead
/// ([`SynthesisState::synthesize_comfort_frame`]).
pub enum FrameOutcome {
    Repeat,
    Mute,
    Decoded(DecodedParameters),
}

/// `b_hat_0` values `208..=255` are reserved/unused (section 6.1's own stated range, `0 <= b_hat_0 <=
/// 207`) -- section 7.7's own text: a value outside that range is *itself* a frame-repeat trigger,
/// independent of `error_estimation::should_repeat_frame`'s own Eq. 97-98 check.
const MAX_VALID_B0: u32 = 207;

/// Persistent decoder state across frames: the shared [`SynthesisState`] (noise generator, both
/// synthesis halves, enhancement-stage `S_E`/`tau_M`, and the last real frame's own final synthesis
/// inputs for a repeat), plus the two pieces [`reconstruct_spectral_amplitudes`] itself needs frame to
/// frame -- `l_hat_prev`/`spectral_amplitudes_prev`, the *unenhanced* `M~_l(-1)` history (Eq. 77's own
/// prediction input, a real, separate history from `SynthesisState`'s own enhanced `M_bar_l(-1)`), and
/// `error_rate_prev` (`epsilon_R`, Eq. 96's own running average). Initial values per Annex A:
/// `L~(-1) = 30` ([`INITIAL_L_HAT_PREV`]), `M~_l(-1) = 1` for all `l` (unity, not silent -- matching
/// this codebase's own encoder-side `FrameState::initial()` choice, itself independently confirmed
/// against Annex A when `voiced_synthesis.rs` was built), `epsilon_R(-1) = 0.0`.
pub struct DecoderState {
    synthesis: SynthesisState,
    l_hat_prev: u32,
    spectral_amplitudes_prev: Vec<f64>,
    error_rate_prev: f64,
    chip_pitch_map: bool,
    /// Decode the DVSI chip's framing (no demodulation, chip Hamming labelling) instead of the standard's.
    chip_wire: bool,
    forced_l_hat: Option<u32>,
    l_alpha: Option<f64>,
    fade_concealment: bool,
    consecutive_repeats: u32,
}

impl DecoderState {
    /// A decoder for the DVSI chip's `RATET(27)` streams: the chip's log-scale pitch index (see
    /// [`crate::ambe::dvsi_p25fec::pitch_map::CHIP_B0_STEPS_PER_OCTAVE`]) instead of the TIA linear one, accepting
    /// `b0` up to 255, and the chip's framing ([`Self::new_chip_wire`]).
    pub fn new_chip() -> Self {
        Self {
            chip_pitch_map: true,
            chip_wire: true,
            ..Self::new()
        }
    }

    /// The standard's linear pitch map, but the DVSI chip's framing of the eight code vectors (plain FEC
    /// codewords with the chip's Hamming labelling, no Eq. 84-94 modulation). What [`Self::new`] did before
    /// the standard's own wire layer was restored; kept for the chip-comparison tools.
    pub fn new_chip_wire() -> Self {
        Self {
            chip_wire: true,
            ..Self::new()
        }
    }

    /// Experiments only: decode every frame as if it carried `l_hat` harmonics regardless of `b0`.
    pub fn set_forced_l_hat(&mut self, l_hat: Option<u32>) {
        self.forced_l_hat = l_hat;
    }

    /// Experiments only: harmonic count `round(alpha*pi/omega0)` clamped to 9..=56 instead of Eq. 47.
    /// Measurement option, float only: replace the standard's flat repeat and comfort-noise mute with a repeat that fades to
    /// 0.8 of the previous level each consecutive damaged frame. Off by default because it was not a clear win
    /// (`examples/ambe_error_concealment_eval_tia.rs`).
    pub fn set_fade_concealment(&mut self, on: bool) {
        self.fade_concealment = on;
    }

    pub fn set_l_alpha(&mut self, alpha: Option<f64>) {
        self.l_alpha = alpha;
    }

    // [@ANCHOR: ambe:decoder_state_new]
    pub fn new() -> Self {
        let l_hat_prev = INITIAL_L_HAT_PREV;
        Self {
            synthesis: SynthesisState::new(),
            l_hat_prev,
            spectral_amplitudes_prev: vec![1.0; l_hat_prev as usize],
            error_rate_prev: 0.0,
            chip_pitch_map: false,
            chip_wire: false,
            forced_l_hat: None,
            l_alpha: None,
            fade_concealment: false,
            consecutive_repeats: 0,
        }
    }

    /// Advances this decoder's own real history (`l_hat_prev`, `spectral_amplitudes_prev`) to match
    /// a just-decoded frame's own parameters -- the same update [`Self::decode_frame`]'s own
    /// `FrameOutcome::Decoded` branch performs (unconditionally there on a successful synthesis;
    /// this method doesn't require a synthesis step at all), exposed separately so a caller that
    /// only needs decoded *parameters* (e.g. `examples/ambe_fixed_chip_validate_ratet27.rs`,
    /// cross-checking `reconstruct_spectral_amplitudes` against its fixed-point port) can still
    /// drive this decoder's own real, stateful history forward correctly across many frames, rather
    /// than silently reconstructing every frame against the same stale initial history because
    /// [`Self::decode_parameters`] alone never advances it (only [`Self::decode_frame`] does).
    pub fn advance_history(&mut self, params: &DecodedParameters) {
        self.l_hat_prev = params.l_hat;
        self.spectral_amplitudes_prev = params.reconstructed_amplitudes.clone();
    }

    /// Steps 1-6 of this module's own doc comment, stopping short of synthesis: recovers either a
    /// repeat decision or the frame's full decoded parameter set. Split out from
    /// [`Self::decode_frame`] so a caller can assert on the actual recovered parameters (`b0`, `b1`,
    /// `b2`, gain/higher-order bits, per-harmonic voicing) rather than only on synthesized PCM, which
    /// -- because Annex F/G's own bit budgets total 88 bits for *every* valid `(L, K)` pair -- stays
    /// `Some` and "looks fine" even when decode derived the wrong `L~`/`K~` for the frame.
    // [@ANCHOR: ambe:decode_parameters]
    pub fn decode_parameters(&mut self, c: [u32; 8]) -> Option<FrameOutcome> {
        // The standard's wire layer (demodulate with the corrected `u_hat_0`, textbook Hamming) or,
        // for the chip-comparison tools only, the DVSI chip's framing; `u_hat_7` has no FEC either way.
        let (u_vectors, corrected_error_counts) = if self.chip_wire {
            decode_code_vectors_chip(c)
        } else {
            decode_code_vectors(c)
        };
        let errors = estimate_errors(&corrected_error_counts, self.error_rate_prev);
        self.error_rate_prev = errors.rate;

        let b0 = extract_fundamental_frequency_quantizer(&u_vectors);

        // Section 7.8's own threshold (`epsilon_R > .0875`) is checked first and independently of
        // section 7.7's repeat conditions -- a real error rate severe enough to mute is also, in
        // practice, severe enough that `should_repeat_frame` would often also fire, but 7.8 is a
        // real, separate, stricter requirement ("the decoder is required to mute"), not merely a
        // more severe repeat.
        if should_mute_frame(&errors) {
            return Some(FrameOutcome::Mute);
        }

        if (!self.chip_pitch_map && b0 > MAX_VALID_B0) || should_repeat_frame(&errors) {
            return Some(FrameOutcome::Repeat);
        }

        let omega0_tilde = if self.chip_pitch_map {
            dequantize_fundamental_frequency_chip(b0)
        } else {
            dequantize_fundamental_frequency(b0)
        };
        let l_hat = self.forced_l_hat.unwrap_or_else(|| match self.l_alpha {
            Some(a) => ((a * std::f64::consts::PI / omega0_tilde).round() as u32).clamp(9, 56),
            // The chip's pitch map spans b0 0..=255, whose Eq. 47 count leaves the 9..=56 range Annex F/G define.
            None => harmonics_count(omega0_tilde).clamp(9, 56),
        });
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
        let reconstructed_amplitudes = reconstruct_spectral_amplitudes(
            bits.b2 as u8,
            gain_values,
            &higher_order_values,
            l_hat,
            self.l_hat_prev,
            &self.spectral_amplitudes_prev,
        )?;

        Some(FrameOutcome::Decoded(DecodedParameters {
            omega0_tilde,
            l_hat,
            k_hat,
            bits,
            voiced,
            reconstructed_amplitudes,
            errors,
        }))
    }

    /// Decodes and synthesizes one received frame's own eight modulated code vectors
    /// `c_hat_0..c_hat_7` (from [`super::interleave::deinterleave_from_dibit_symbols`]) into a 20 ms
    /// PCM frame. Returns `None` only for a genuinely unrecoverable repeat case: the very first
    /// frame of a stream itself needs a repeat, and there is no real previous frame for
    /// [`SynthesisState::synthesize_repeated_frame`] to reuse. A mute has no such limitation --
    /// [`SynthesisState::synthesize_comfort_frame`] needs no previous frame at all, so a muted
    /// first frame still produces real output.
    // [@ANCHOR: ambe:decode_frame]
    pub fn decode_frame(&mut self, c: [u32; 8]) -> Option<[f64; N]> {
        let outcome = self.decode_parameters(c)?;
        if matches!(outcome, FrameOutcome::Decoded(_)) {
            self.consecutive_repeats = 0;
        } else if self.fade_concealment {
            self.consecutive_repeats += 1;
            return self
                .synthesis
                .synthesize_repeated_frame_scaled(0.8f64.powi(self.consecutive_repeats as i32));
        }
        match outcome {
            FrameOutcome::Repeat => self.synthesis.synthesize_repeated_frame(),
            FrameOutcome::Mute => Some(self.synthesis.synthesize_comfort_frame()),
            FrameOutcome::Decoded(params) => {
                let pcm = self.synthesis.synthesize_frame(
                    &params.reconstructed_amplitudes,
                    params.omega0_tilde,
                    &params.voiced,
                    &params.errors,
                )?;

                self.l_hat_prev = params.l_hat;
                self.spectral_amplitudes_prev = params.reconstructed_amplitudes;

                Some(pcm)
            }
        }
    }
}

impl Default for DecoderState {
    // [@ANCHOR: ambe:decoder_state_default]
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambe::float::tia_102_baba::{
        bit_prioritization::prioritize_bits, encode_code_vectors, parameter_encoding, tables,
    };

    /// Builds a genuine synthetic voiced frame the same way `decode_parameters` will itself derive
    /// `L~`/`K~` on the receiving end: from `omega0_tilde = dequantize(quantize(omega0_hat))`, not
    /// from a hand-picked `L_hat` paired with an unrelated `omega0_hat`. An earlier draft of this test
    /// picked `L_hat=16` by hand while encoding `omega0_hat = 2*pi/100` -- but that pitch's own
    /// round-tripped `omega0_tilde` actually decodes to `L~=46` (Annex F/G's own per-`(L,K)` bit
    /// budgets all total exactly 88 bits, so the mismatched frame still parsed "successfully" into
    /// silently wrong parameters). Building against the decoder's own derivation here closes that
    /// gap, and `harmonics_count_agrees_with_itself_across_the_full_quantize_dequantize_round_trip`
    /// (`parameter_encoding.rs`) separately proves this agreement holds for every real pitch, not
    /// just this one.
    struct SyntheticFrame {
        c: [u32; 8],
        b0: u32,
        b1: u32,
        b2: u32,
        gain_vector: [(u32, u8); 5],
        higher_order: Vec<(u32, u8)>,
    }

    fn build_synthetic_voiced_frame() -> SyntheticFrame {
        let omega0_hat = 2.0 * std::f64::consts::PI / 100.0;
        let b0 = parameter_encoding::quantize_fundamental_frequency(omega0_hat);
        let omega0_tilde = parameter_encoding::dequantize_fundamental_frequency(b0);
        let l_hat = crate::ambe::float::tia_102_baba::vuv::harmonics_count(omega0_tilde);
        let k_hat = crate::ambe::float::tia_102_baba::vuv::frequency_bands_count(l_hat);

        let voiced_bands = vec![true; k_hat as usize];
        let b1 = parameter_encoding::encode_voicing_decisions(&voiced_bands);

        let gain: [u8; 5] =
            std::array::from_fn(|i| tables::gain_bit_allocation(l_hat, i as u32 + 2).unwrap().0);
        let higher = tables::higher_order_bit_allocation(l_hat).unwrap().to_vec();
        let b2 = 32u32; // Mid-range gain index.
        let gain_vector: [(u32, u8); 5] =
            std::array::from_fn(|i| ((1u32 << (gain[i] - 1)) & ((1 << gain[i]) - 1), gain[i]));
        let higher_order: Vec<(u32, u8)> = higher
            .iter()
            .filter(|&&w| w > 0)
            .map(|&w| (1u32 & ((1 << w) - 1), w))
            .collect();

        let u = prioritize_bits(b0, b1, k_hat, b2, gain_vector, &higher_order, false).unwrap();
        let c = encode_code_vectors(u);
        SyntheticFrame {
            c,
            b0,
            b1,
            b2,
            gain_vector,
            higher_order,
        }
    }

    /// A full, real encode -> decode round trip that asserts *exact* parameter fidelity, not just
    /// that synthesis produced finite, non-silent PCM (a much weaker claim that an earlier draft of
    /// this test relied on, and that would have passed even with the L~ mismatch documented on
    /// `build_synthetic_voiced_frame` above). `b0`/`b1`/`b2`/the gain vector/the higher-order
    /// coefficients are all recoverable exactly through this path -- no quantization loss between
    /// `prioritize_bits` and `deprioritize_bits` -- so this checks `assert_eq!`, not a tolerance.
    // Tests [@ANCHOR: ambe:decoder_state_new]
    // Tests [@ANCHOR: ambe:decode_parameters]
    // Tests [@ANCHOR: ambe:decode_frame]
    #[test]
    fn decode_frame_round_trips_a_real_synthetic_voiced_frame() {
        let frame = build_synthetic_voiced_frame();

        let mut params_decoder = DecoderState::new();
        let params = match params_decoder.decode_parameters(frame.c).unwrap() {
            FrameOutcome::Decoded(params) => params,
            FrameOutcome::Repeat => {
                panic!("expected a real decode for this synthetic frame, got a repeat")
            }
            FrameOutcome::Mute => {
                panic!("expected a real decode for this synthetic frame, got a mute")
            }
        };

        assert_eq!(params.bits.b0, frame.b0);
        assert_eq!(params.bits.b1, frame.b1);
        assert_eq!(params.bits.b2, frame.b2);
        assert_eq!(params.bits.gain_vector, frame.gain_vector);
        assert_eq!(params.bits.higher_order, frame.higher_order);
        assert!(!params.bits.sync_bit);
        assert!(
            params.voiced.iter().all(|&v| v),
            "every harmonic was encoded voiced, so every decoded one should be too"
        );

        let mut decoder = DecoderState::new();
        let pcm = decoder.decode_frame(frame.c).unwrap();
        assert_eq!(pcm.len(), N);
        for &sample in &pcm {
            assert!(sample.is_finite(), "non-finite decoded sample: {sample}");
        }
        assert!(
            pcm.iter().any(|&s| s.abs() > 1e-9),
            "expected real synthesized energy from a genuine voiced frame"
        );
    }

    /// The real P25 CAI-level integration gap this module's own doc comment already claims is
    /// closed ("the 72 interleaved dibit symbols an actual P25 channel decoder hands over --
    /// `deinterleave_from_dibit_symbols` is the bridge between the two") but that, until this test,
    /// had never actually been exercised end to end: `deinterleave_from_dibit_symbols` was only ever
    /// unit-tested against its own interleave step in isolation (`interleave.rs`'s own
    /// `deinterleave_is_the_exact_inverse_of_interleave_for_several_real_bit_patterns`), never
    /// against a genuine encoded frame flowing all the way through `decode_parameters`. This proves
    /// the whole chain: a real encoded frame's `c` vectors, interleaved into the 72 dibit symbols a
    /// real P25 channel decoder would actually deliver, deinterleaved back, and decoded -- produces
    /// byte-identical results to decoding `c` directly, so the interleave/deinterleave round trip is
    /// genuinely transparent to the parameter decoder on a clean channel (no bit errors), not just to
    /// the raw bit-position bijection `interleave.rs` already checked in isolation.
    // Tests [@ANCHOR: ambe:decode_parameters]
    #[test]
    fn decode_parameters_agrees_whether_fed_c_directly_or_via_a_real_interleave_deinterleave_round_trip(
    ) {
        let frame = build_synthetic_voiced_frame();

        let symbols =
            crate::ambe::float::tia_102_baba::interleave::interleave_to_dibit_symbols(frame.c);
        let recovered_c =
            crate::ambe::float::tia_102_baba::interleave::deinterleave_from_dibit_symbols(symbols);
        assert_eq!(
            recovered_c, frame.c,
            "a clean interleave/deinterleave round trip must recover the exact code vectors"
        );

        let mut direct_decoder = DecoderState::new();
        let direct_params = match direct_decoder.decode_parameters(frame.c).unwrap() {
            FrameOutcome::Decoded(params) => params,
            FrameOutcome::Repeat => {
                panic!("expected a real decode of the direct frame, got a repeat")
            }
            FrameOutcome::Mute => panic!("expected a real decode of the direct frame, got a mute"),
        };

        let mut via_channel_decoder = DecoderState::new();
        let via_channel_params = match via_channel_decoder.decode_parameters(recovered_c).unwrap() {
            FrameOutcome::Decoded(params) => params,
            FrameOutcome::Repeat => {
                panic!("expected a real decode of the channel-round-tripped frame, got a repeat")
            }
            FrameOutcome::Mute => {
                panic!("expected a real decode of the channel-round-tripped frame, got a mute")
            }
        };

        assert_eq!(direct_params.bits.b0, via_channel_params.bits.b0);
        assert_eq!(direct_params.bits.b1, via_channel_params.bits.b1);
        assert_eq!(direct_params.bits.b2, via_channel_params.bits.b2);
        assert_eq!(
            direct_params.bits.gain_vector,
            via_channel_params.bits.gain_vector
        );
        assert_eq!(
            direct_params.bits.higher_order,
            via_channel_params.bits.higher_order
        );
        assert_eq!(direct_params.omega0_tilde, via_channel_params.omega0_tilde);
        assert_eq!(direct_params.l_hat, via_channel_params.l_hat);
        assert_eq!(direct_params.k_hat, via_channel_params.k_hat);
        assert_eq!(direct_params.voiced, via_channel_params.voiced);
        assert_eq!(
            direct_params.reconstructed_amplitudes,
            via_channel_params.reconstructed_amplitudes
        );
    }

    #[test]
    fn decode_frame_does_not_panic_on_an_all_zero_first_frame() {
        let mut decoder = DecoderState::new();
        // c[0] deliberately garbage enough that Golay-decoding it, taken together with a
        // deliberately-invalid b0, may force a repeat with no real previous frame to fall back on.
        // Whether all-zero input actually triggers that path depends on golay_decode's own behavior
        // on an all-zero (technically valid-looking) codeword -- the only thing this test asserts is
        // that decode_frame never panics on a first-frame edge case, whichever branch it takes.
        let c = [0u32, 0, 0, 0, 0, 0, 0, 0];
        let _ = decoder.decode_frame(c);
    }

    /// Real, direct test of section 7.8's own mute requirement: a persistently high running error
    /// rate (`error_rate_prev` seeded above the `.0875` threshold -- `should_mute_frame` is checked
    /// before anything about `b0`/`l_hat_prev`, so the specific bitstream content doesn't matter
    /// here) must produce real, bounded comfort noise, not a repeat and not `None` -- confirming the
    /// real improvement this fix makes over the old "mute treated as repeat" placeholder: a muted
    /// *first* frame (no real previous frame at all) still produces output, since
    /// `synthesize_comfort_frame` has no such dependency the way `synthesize_repeated_frame` does.
    #[test]
    fn a_persistently_high_error_rate_forces_a_mute_producing_real_comfort_noise_even_on_the_first_frame(
    ) {
        let mut decoder = DecoderState {
            synthesis: SynthesisState::new(),
            l_hat_prev: INITIAL_L_HAT_PREV,
            spectral_amplitudes_prev: vec![1.0; INITIAL_L_HAT_PREV as usize],
            chip_pitch_map: false,
            chip_wire: false,
            forced_l_hat: None,
            l_alpha: None,
            fade_concealment: false,
            consecutive_repeats: 0,
            error_rate_prev: 0.2, // 0.95*0.2 = 0.19, comfortably over the 0.0875 threshold
                                  // regardless of this frame's own corrected error count.
        };
        let c = [0u32, 0, 0, 0, 0, 0, 0, 0];
        let pcm = decoder
            .decode_frame(c)
            .expect("a mute must still produce real output, even on the first frame");
        assert_eq!(pcm.len(), N);
        for &sample in &pcm {
            assert!(
                (-5.0..5.0).contains(&sample),
                "comfort-noise sample {sample} outside the spec's own [-5, 5) interval"
            );
        }
    }

    /// `Default::default()` is a trivial one-line delegation to `Self::new()` -- checked for real
    /// rather than left implicitly covered by `new()`'s own tests above, since a future edit could
    /// make the two diverge without either test noticing on its own.
    // Tests [@ANCHOR: ambe:decoder_state_default]
    #[test]
    fn default_produces_the_same_initial_state_as_new() {
        let frame = build_synthetic_voiced_frame();
        let mut via_default = DecoderState::default();
        let mut via_new = DecoderState::new();
        let pcm_default = via_default.decode_frame(frame.c).unwrap();
        let pcm_new = via_new.decode_frame(frame.c).unwrap();
        assert_eq!(pcm_default, pcm_new);
    }
}
