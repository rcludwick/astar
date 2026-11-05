//! Fundamental frequency and voiced/unvoiced decision encoding/decoding (TIA-102.BABA_2003.pdf
//! sections 6.1-6.2, Eq. 45-49) -- the two model parameters that get their own dedicated quantizer
//! values `b_hat_0` and `b_hat_1`, ahead of the gain vector (`b_hat_2..b_hat_7`, [`super::tables`]/
//! [`super::quantize`]) and higher-order DCT coefficients (`b_hat_8..b_hat_{L+1}`, same modules).
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 38-40, following the same
//! discipline as every other body-text equation in this spec (Type3 digit font defeats
//! `pdftotext`).
//!
//! `L~` (Eq. 47, the decoder's own harmonic count from the *reconstructed* `omega0_tilde`) turned out
//! to be textually identical to [`super::vuv::harmonics_count`]'s own Eq. 31 (the encoder's harmonic
//! count from the *estimated* `omega0_hat`) -- same `floor(0.9254 * floor(pi/omega0 + 0.25))` formula,
//! just fed a different frequency -- so that function is reused directly rather than duplicated here.
//! Likewise `K~` (Eq. 48) is textually identical to [`super::vuv::frequency_bands_count`]'s own
//! Eq. 34 (`floor((L+2)/3)` for `L<=36`, else `12` -- the same identity as that function's own
//! `div_ceil(3)`, checked when Eq. 34 was first transcribed).

use std::f64::consts::PI;

/// `b_hat_0`'s own fixed bit width (section 6.1's own stated "the quantizer value `b_hat_0` is
/// represented with 8 bits", Table 2) -- unlike every other quantizer value in this codec, this one
/// doesn't depend on `L_hat`.
pub const FUNDAMENTAL_FREQUENCY_BITS: u32 = 8;

/// The fundamental frequency quantizer value `b_hat_0` (Eq. 45): `omega0_hat` (already estimated to
/// quarter-sample resolution by [`super::pitch_refinement::refine_pitch`]) is encoded at half-sample
/// resolution instead, since only 8 bits are budgeted for it. The spec's own stated valid range is
/// `0 <= b_hat_0 <= 207` (leaving 48 reserved/unused values in the 8-bit range for future use) --
/// checked by the test below against the pitch range this codec actually estimates over
/// ([`super::pitch::candidate_pitches`]), not merely asserted.
pub fn quantize_fundamental_frequency(omega0_hat: f64) -> u32 {
    // The estimated period is a multiple of an eighth sample, so `4*pi/omega0` is often exactly an integer; the tiny
    // bias keeps floating-point rounding from turning such a boundary into the next lower index.
    ((4.0 * PI / omega0_hat) - 39.0 + 1e-9).floor() as u32
}

/// `omega0_tilde` (Eq. 46): reconstructs the fundamental frequency from the received quantizer value
/// `b_hat_0` -- bin-center dequantization (`+39.5`, half a step above the encoder's own `-39` floor
/// offset in Eq. 45), matching this codebase's own established bin-center convention elsewhere (e.g.
/// [`super::reconstruct::dequantize_uniform`]'s own `+0.5`).
pub fn dequantize_fundamental_frequency(b0_tilde: u32) -> f64 {
    4.0 * PI / (b0_tilde as f64 + 39.5)
}

/// `v_bar_k` (the decoder-side counterpart of Eq. 49, section 6.2's own decoding half): unpacks
/// `b_hat_1`'s own `k_hat` bits back into per-band voiced/unvoiced decisions, MSB-first, the exact
/// inverse of [`encode_voicing_decisions`].
pub fn decode_voicing_decisions(b1_tilde: u32, k_hat: u32) -> Vec<bool> {
    (1..=k_hat)
        .map(|k| (b1_tilde >> (k_hat - k)) & 1 == 1)
        .collect()
}

/// `v_tilde_l` (Eq. 50-51, section 6.2's own decoding half): expands the `k_hat` per-band voicing
/// decisions (from [`decode_voicing_decisions`]) into `l_hat` per-harmonic decisions -- the real
/// conversion the decoder needs that the encoder never had to make (the encoder works in bands the
/// whole time; only the decoder needs a decision "for each spectral amplitude," per the spec's own
/// text explaining this is "a departure from the V/UV convention used by the encoder").
///
/// Two real reuse discoveries made while transcribing this, not assumed: **Eq. 50's own `kappa_l`**
/// (harmonic `l`'s own band index, `floor((l+2)/3)` for `l<=36`, else `12`) is textually identical to
/// Eq. 48's own `K~` formula ([`super::vuv::frequency_bands_count`]) -- same formula, just fed a
/// harmonic index instead of a harmonic *count* -- so that function is reused directly here instead
/// of a duplicate. **Eq. 51 itself** (`floor(b1/2^(K-kappa_l)) - 2*floor(b1/2^(K+1-kappa_l))`) is
/// exactly a single-bit extraction of `b1`'s own bit `(K_hat - kappa_l)`, which is precisely what
/// [`decode_voicing_decisions`] already computes for band `kappa_l` -- so `v_tilde_l` reduces to
/// "look up band `kappa_l`'s own decision," not a separate bit-extraction formula.
///
/// `kappa_l` is always `<= k_hat` for every `l` in `1..=l_hat` (both Eq. 48 and Eq. 50 are the same
/// non-decreasing step function of their own input, and `l <= l_hat` always here), so the per-band
/// lookup never panics -- checked directly by the test below, not just argued.
pub fn decode_voicing_decisions_per_harmonic(b1_tilde: u32, k_hat: u32, l_hat: u32) -> Vec<bool> {
    let per_band = decode_voicing_decisions(b1_tilde, k_hat);
    (1..=l_hat)
        .map(|l| {
            let kappa_l = super::vuv::frequency_bands_count(l);
            per_band[(kappa_l - 1) as usize]
        })
        .collect()
}

/// `b_hat_1` (Eq. 49): packs the `K_hat` per-band voiced/unvoiced decisions (from
/// [`super::vuv::determine_voicing`]) into a single unsigned integer, MSB-first (`v_hat_1` is the
/// most significant of the `K_hat` bits used to represent this value, `v_hat_{K_hat}` the least).
// [@ANCHOR: encode_voicing_decisions]
pub fn encode_voicing_decisions(voiced: &[bool]) -> u32 {
    let k_hat = voiced.len() as u32;
    voiced
        .iter()
        .enumerate()
        .map(|(idx, &v)| {
            let k = idx as u32 + 1; // 1-indexed k, matching the spec's own v_hat_k
            if v {
                1u32 << (k_hat - k)
            } else {
                0
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambe::float::tia_102_baba::pitch::candidate_pitches;

    /// Every eighth-sample period (the estimator's resolution) quantizes to the exact rational answer
    /// `floor(2*P - 39)`, including the periods where `4*pi/omega0` is an exact integer.
    #[test]
    fn quantize_fundamental_frequency_is_exact_at_every_eighth_sample_period() {
        for eighths in 168u32..=976 {
            let period = eighths as f64 / 8.0;
            let expected = eighths / 4 - 39; // floor(2 * eighths / 8 - 39)
            assert_eq!(
                quantize_fundamental_frequency(2.0 * PI / period),
                expected,
                "period {period}"
            );
        }
    }

    #[test]
    fn quantize_fundamental_frequency_matches_eq45_at_a_hand_computed_value() {
        // omega0_hat = 4*pi/(207 + 39) = 4*pi/246, chosen so Eq. 45 lands exactly on the
        // spec's own stated upper bound b_hat_0 = 207 without any floor-boundary ambiguity.
        let omega0_hat = 4.0 * PI / 246.0;
        assert_eq!(quantize_fundamental_frequency(omega0_hat), 207);
    }

    /// Every real quarter-sample period this codec's pitch estimator can actually hand to
    /// [`quantize_fundamental_frequency`] -- not [`candidate_pitches`]'s own half-sample set
    /// directly, since [`super::pitch_refinement::refine_pitch`] (the real caller, one stage later)
    /// perturbs each candidate by one of ten quarter-sample offsets in `-9/8..=9/8`. That widens the
    /// true domain to `19.875..=123.125`, which is exactly the spec's own stated `omega0_hat`
    /// interval boundary (`2*19.875 - 39 = 0.75` floors to `0`; `2*123.125 - 39 = 207.25` floors to
    /// `207`) -- deriving the test domain this way turns the spec's own quoted `0..=207` bound into
    /// a checked consequence of the real pitch range, not a separately re-asserted fact.
    fn real_refined_pitch_range() -> impl Iterator<Item = f64> {
        let offsets = [
            -9.0 / 8.0,
            -7.0 / 8.0,
            -5.0 / 8.0,
            -3.0 / 8.0,
            -1.0 / 8.0,
            1.0 / 8.0,
            3.0 / 8.0,
            5.0 / 8.0,
            7.0 / 8.0,
            9.0 / 8.0,
        ];
        candidate_pitches().flat_map(move |p| offsets.into_iter().map(move |offset| p + offset))
    }

    #[test]
    fn quantize_fundamental_frequency_stays_within_the_specs_own_0_to_207_range_across_the_real_pitch_range(
    ) {
        // The spec's own stated valid range (section 6.1: "the value of b_hat_0 ... is limited
        // to the range 0 <= b_hat_0 <= 207"), checked against every omega0_hat this codec's own
        // pitch estimator can actually produce end to end (candidate_pitches, further refined by
        // refine_pitch's own quarter-sample offsets -- see real_refined_pitch_range above) rather
        // than merely asserted from the spec text.
        for p in real_refined_pitch_range() {
            let omega0_hat = 2.0 * PI / p;
            let b0 = quantize_fundamental_frequency(omega0_hat);
            assert!(
                b0 <= 207,
                "P={p}: expected b_hat_0 <= 207 per the spec's own stated range, got {b0}"
            );
        }
    }

    #[test]
    fn quantize_fundamental_frequency_fits_in_8_bits() {
        for p in real_refined_pitch_range() {
            let omega0_hat = 2.0 * PI / p;
            let b0 = quantize_fundamental_frequency(omega0_hat);
            assert!(b0 < (1u32 << FUNDAMENTAL_FREQUENCY_BITS));
        }
    }

    /// The encoder's own `-39` (Eq. 45) and the decoder's own `+39.5` (Eq. 46) are not an obviously
    /// matched inverse pair -- the `.5` is bin-center dequantization for the half-sample grid the
    /// 8-bit budget forces `b_hat_0` onto, not a typo. Checked algebraically (`b0 = floor(2P-39)`,
    /// `P_tilde = (b0+39.5)/2` gives `|P_tilde - P|` strictly within a quarter-sample for any real
    /// `P`) and then empirically here across the same real pitch-period range this codec's own
    /// pitch estimator can actually produce, rather than trusted from the algebra alone.
    #[test]
    fn dequantize_fundamental_frequency_recovers_the_period_within_a_half_sample() {
        for p in real_refined_pitch_range() {
            let omega0_hat = 2.0 * PI / p;
            let b0 = quantize_fundamental_frequency(omega0_hat);
            let omega0_tilde = dequantize_fundamental_frequency(b0);
            let p_tilde = 2.0 * PI / omega0_tilde;
            assert!(
                (p_tilde - p).abs() < 0.5,
                "P={p}: round-tripped to P~={p_tilde} via b0={b0}, off by {}",
                (p_tilde - p).abs()
            );
        }
    }

    /// The real interop property `decode.rs`'s own round-trip test depends on: the decoder never
    /// learns `L~` from the encoder's own `omega0_hat` directly, only from `omega0_tilde =
    /// dequantize(quantize(omega0_hat))` (a lossy round trip through the 8-bit `b_hat_0`). If that
    /// round trip ever pushed `harmonics_count`'s own floor across a boundary relative to the
    /// encoder's `L_hat = harmonics_count(omega0_hat)`, the two sides would disagree on Annex F/G's
    /// own bit-column widths for the frame -- and because every valid `(L, K)` pair in Annex F/G
    /// totals exactly 88 bits, that disagreement parses "successfully" into entirely wrong parameter
    /// values rather than failing loudly. Checked across every real pitch this codec's own estimator
    /// can produce, not assumed from the quarter-sample bound above.
    #[test]
    fn harmonics_count_agrees_with_itself_across_the_full_quantize_dequantize_round_trip() {
        use crate::ambe::float::tia_102_baba::vuv::harmonics_count;

        for p in real_refined_pitch_range() {
            let omega0_hat = 2.0 * PI / p;
            let l_hat_encoder = harmonics_count(omega0_hat);
            let b0 = quantize_fundamental_frequency(omega0_hat);
            let omega0_tilde = dequantize_fundamental_frequency(b0);
            let l_hat_decoder = harmonics_count(omega0_tilde);
            assert_eq!(
                l_hat_encoder, l_hat_decoder,
                "P={p}: encoder derived L_hat={l_hat_encoder} from omega0_hat={omega0_hat}, but \
                 decoder would derive L~={l_hat_decoder} from the round-tripped omega0_tilde={omega0_tilde} \
                 (via b0={b0}) -- frame layout disagreement"
            );
        }
    }

    #[test]
    fn decode_voicing_decisions_is_the_exact_inverse_of_encode_voicing_decisions() {
        for k_hat in 1u32..=12 {
            // Exercise every real bit pattern for small k_hat, and a handful of representative
            // patterns for larger k_hat (2^12 is cheap enough to do fully too, but no need).
            let patterns: Vec<u32> = if k_hat <= 8 {
                (0..(1u32 << k_hat)).collect()
            } else {
                vec![
                    0,
                    1,
                    (1 << k_hat) - 1,
                    0b1010_1010_1010 & ((1 << k_hat) - 1),
                ]
            };
            for b1 in patterns {
                let voiced = decode_voicing_decisions(b1, k_hat);
                assert_eq!(voiced.len(), k_hat as usize);
                assert_eq!(
                    encode_voicing_decisions(&voiced),
                    b1,
                    "k_hat={k_hat}, b1={b1}"
                );
            }
        }
    }

    /// `kappa_l` (Eq. 50) must always index a real band -- checked directly across every real
    /// `(l_hat, l)` pair the codec can actually produce (`l_hat` in the spec's own stated `9..=56`
    /// range, `l` in `1..=l_hat`), not just argued from the two formulas' shared shape.
    #[test]
    fn kappa_l_never_exceeds_k_hat_for_any_real_l_hat_and_l() {
        for l_hat in 9u32..=56 {
            let k_hat = super::super::vuv::frequency_bands_count(l_hat);
            for l in 1..=l_hat {
                let kappa_l = super::super::vuv::frequency_bands_count(l);
                assert!(
                    kappa_l >= 1 && kappa_l <= k_hat,
                    "l_hat={l_hat}, l={l}: kappa_l={kappa_l} out of range 1..={k_hat}"
                );
            }
        }
    }

    /// Eq. 51 itself, hand-computed directly from its own literal formula (not via
    /// `decode_voicing_decisions`, which is what [`decode_voicing_decisions_per_harmonic`] actually
    /// uses internally) -- an independent check that the "reduces to a bit lookup" claim in this
    /// function's own doc comment is really an identity, not just a plausible-looking shortcut.
    #[test]
    fn decode_voicing_decisions_per_harmonic_matches_eq51s_own_literal_formula() {
        let b1 = 0b10_1101u32; // k_hat = 6.
        let k_hat = 6u32;
        let l_hat = 16u32;
        let per_harmonic = decode_voicing_decisions_per_harmonic(b1, k_hat, l_hat);
        assert_eq!(per_harmonic.len(), l_hat as usize);

        for l in 1..=l_hat {
            let kappa_l = super::super::vuv::frequency_bands_count(l);
            // Eq. 51, transcribed literally rather than reusing decode_voicing_decisions.
            let expected = (b1 as i64 / 2i64.pow(k_hat - kappa_l))
                - 2 * (b1 as i64 / 2i64.pow(k_hat + 1 - kappa_l));
            assert_eq!(
                per_harmonic[(l - 1) as usize],
                expected == 1,
                "l={l}, kappa_l={kappa_l}: expected {expected}"
            );
        }
    }

    #[test]
    // Tests [@ANCHOR: encode_voicing_decisions]
    fn encode_voicing_decisions_matches_eq49_at_a_hand_computed_value() {
        // K_hat=4, v = [1,0,1,1] -> b_hat_1 = 1*2^3 + 0*2^2 + 1*2^1 + 1*2^0 = 8+0+2+1 = 11.
        let voiced = [true, false, true, true];
        assert_eq!(encode_voicing_decisions(&voiced), 11);
    }

    #[test]
    fn encode_voicing_decisions_of_all_voiced_is_the_all_ones_bit_pattern() {
        for k_hat in 1u32..=12 {
            let voiced = vec![true; k_hat as usize];
            let expected = (1u32 << k_hat) - 1;
            assert_eq!(encode_voicing_decisions(&voiced), expected);
        }
    }

    #[test]
    fn encode_voicing_decisions_of_all_unvoiced_is_zero() {
        let voiced = vec![false; 7];
        assert_eq!(encode_voicing_decisions(&voiced), 0);
    }
}
