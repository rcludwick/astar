//! Assembles a transmittable 72-bit D-STAR AMBE frame from `decode::RawParameters` -- the exact
//! inverse of `decode::extract_raw_parameters` (scattering `b0..b8` back into the 49-bit `d[]`
//! layout) followed by `decode::parse_frame`'s own real FEC/whitening steps run forward (Golay-
//! encode `C0`'s data, whiten and Golay-encode `C1`'s data, concatenate with `C2`/`C3` raw).

use super::decode::RawParameters;
use crate::ambe::general::fec::golay_encode;

/// Scatters `b0..b8` back into the 49-bit `d[]` layout `decode::extract_raw_parameters` reads from
/// -- each assignment here is the direct algebraic inverse of that function's own extraction, over
/// the identical bit ranges (see `mod.rs`'s own doc-comment table).
// [@ANCHOR: dstar_pack_raw_parameters]
pub fn pack_raw_parameters(raw: &RawParameters) -> u64 {
    let mut d: u64 = 0;
    let mut set = |msb_index: usize, width: usize, value: u32| {
        let shift = 49 - msb_index - width;
        let mask = ((1u64 << width) - 1) << shift;
        d = (d & !mask) | (((value as u64) << shift) & mask);
    };

    set(0, 6, raw.b0 >> 1);
    set(48, 1, raw.b0 & 1);
    set(38, 4, raw.b1);
    set(6, 4, raw.b2 >> 2);
    set(42, 2, raw.b2 & 0b11);
    set(10, 2, raw.b3 >> 7);
    set(12, 5, (raw.b3 >> 2) & 0b1_1111);
    set(44, 2, raw.b3 & 0b11);
    set(17, 5, raw.b4 >> 2);
    set(46, 2, raw.b4 & 0b11);
    set(22, 2, raw.b5 >> 2);
    set(25, 2, raw.b5 & 0b11);
    set(27, 4, raw.b6);
    set(31, 4, raw.b7);
    set(35, 3, raw.b8 >> 1);
    // d[24] (C2's own first bit) is never read by any known decoder, per mod.rs's own doc comment
    // -- left at 0 here since it carries no real information to set.

    d
}

/// Packs a tone frame's 49-bit `d[]`: the exact inverse of [`super::decode::decode_tone`] (with `b0 = 126`, which
/// `classify_b0` reads as a tone). `index` and `volume` are the 8-bit payload fields that function returns; the
/// three per-value lookup tables it uses (`T7TAB`/`T6TAB`/`T5TAB`, keyed on `d[6..9)`) are all distinct across the
/// eight selector values, so `index`'s top three bits pick the selector uniquely.
// [@ANCHOR: dstar_pack_tone_parameters]
pub fn pack_tone_parameters(index: u32, volume: u32) -> u64 {
    const T7TAB: [u32; 8] = [1, 0, 0, 0, 0, 1, 1, 1];
    const T6TAB: [u32; 8] = [0, 0, 0, 1, 1, 1, 1, 0];
    const T5TAB: [u32; 8] = [0, 0, 1, 0, 1, 1, 0, 1];
    let (i7, i6, i5) = ((index >> 7) & 1, (index >> 6) & 1, (index >> 5) & 1);
    let sel = (0..8usize)
        .find(|&s| T7TAB[s] == i7 && T6TAB[s] == i6 && T5TAB[s] == i5)
        .expect("the three tone lookup tables have eight distinct rows");
    let mut d: u64 = 0;
    let mut set = |msb_index: usize, width: usize, value: u32| {
        let shift = 49 - msb_index - width;
        let mask = ((1u64 << width) - 1) << shift;
        d = (d & !mask) | (((value as u64) << shift) & mask);
    };
    set(0, 6, 126 >> 1); // b0 = 126: top six bits 63, low bit (d[48]) 0
    set(6, 3, sel as u32);
    set(9, 1, (index >> 4) & 1);
    set(42, 1, (index >> 3) & 1);
    set(43, 1, (index >> 2) & 1);
    set(10, 1, (index >> 1) & 1);
    set(11, 1, index & 1);
    set(12, 5, (volume >> 3) & 0b1_1111);
    set(44, 1, (volume >> 2) & 1);
    set(45, 1, (volume >> 1) & 1);
    set(17, 1, volume & 1);
    d
}

/// The transmittable 72-bit logical frame for a tone: `index` is the tone code
/// ([`super::decode::classify_tone_index`]: a single tone at `index * 31.25` Hz for `5..=122`, a DTMF digit at
/// `128 + row + 4*col`), `volume` its 8-bit level.
pub fn build_tone_frame(index: u32, volume: u32) -> u128 {
    build_frame(pack_tone_parameters(index, volume))
}

/// Builds the full transmittable 72-bit logical frame (packed MSB-first into the low 72 bits of the
/// return value, matching `decode::parse_frame`'s own input convention -- see `interleave.rs` for
/// converting this into real 9-byte chip/wire data) from the 49-bit `d[]` layout: Golay-encodes
/// `C0`'s 12 data bits (with the even-parity bit of the extended Golay code appended as `C0`'s own LSB, `ambe_fr[0][0]`, not
/// `ambe_fr[0][23]`; mbelib leaves it unchecked), whitens and Golay-encodes `C1`'s 12 data
/// bits using `C0`'s own data as the whitening seed, and carries `C2`/`C3` raw.
pub fn build_frame(d: u64) -> u128 {
    let c0_data = ((d >> 37) & 0xFFF) as u16;
    let c1_data = ((d >> 25) & 0xFFF) as u16;
    let c2 = ((d >> 14) & 0x7FF) as u32;
    let c3 = (d & 0x3FFF) as u32;

    let c0_codeword = golay_encode(c0_data); // 23 bits
    let c1_codeword = golay_encode(c1_data);
    let c1_whitened = super::whiten_c1(c1_codeword, c0_data);

    // C0's 24-bit field is the 23-bit codeword (MSB-first) followed by the spare bit as its own LSB (shifted left by 49, not
    // 48). The chip and JMBE treat C0 as an extended Golay code: every chip frame captured (200 of 200, both modes) has even
    // parity over the 24 bits, so the spare bit is the parity of the codeword. Decoders here never check it.
    let spare = (c0_codeword.count_ones() & 1) as u128;
    ((c0_codeword as u128) << 49)
        | (spare << 48)
        | ((c1_whitened as u128) << 25)
        | ((c2 as u128) << 14)
        | (c3 as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c0_field_has_even_parity_like_the_chips_extended_golay_code() {
        for d in [
            0u64,
            1,
            0x1_FFFF_FFFF_FFFF,
            0x0123_4567_89AB,
            0x1555_5555_5555,
            0x0AAA_AAAA_AAAA,
        ] {
            let frame = build_frame(d & ((1u64 << 49) - 1));
            assert_eq!(
                ((frame >> 48) & 0xFF_FFFF).count_ones() % 2,
                0,
                "d = {d:#x}"
            );
        }
    }
    use crate::ambe::float::dstar::decode::{extract_raw_parameters, parse_frame};

    /// The real, end-to-end round trip this whole module exists for: pack a set of raw parameters
    /// into a transmittable frame, parse that frame back, and confirm both the FEC layer reports
    /// zero corrected errors (a real, meaningful signal for D-STAR's non-perfect Golay(23,12,7) code
    /// -- unlike the P25 investigation's own perfect-code caveat) and the extracted parameters
    /// exactly match the originals.
    #[test]
    // Tests [@ANCHOR: dstar_pack_raw_parameters]
    // Tests [@ANCHOR: dstar_extract_raw_parameters]
    fn pack_and_parse_round_trips_a_full_set_of_raw_parameters() {
        let original = RawParameters {
            b0: 0b101_0110,    // 7 bits
            b1: 0b1010,        // 4 bits
            b2: 0b10_1101,     // 6 bits
            b3: 0b1_0110_1101, // 9 bits
            b4: 0b101_1010,    // 7 bits
            b5: 0b1101,        // 4 bits
            b6: 0b0110,        // 4 bits
            b7: 0b1001,        // 4 bits
            b8: 0b1010,        // 4 bits, low bit already 0 per this field's own real convention
        };

        let d = pack_raw_parameters(&original);
        let frame = build_frame(d);
        let parsed = parse_frame(frame);

        assert_eq!(
            parsed.epsilon_c0, 0,
            "a cleanly built C0 must decode with zero errors"
        );
        assert_eq!(
            parsed.epsilon_c1, 0,
            "a cleanly built, correctly whitened C1 must decode with zero errors"
        );

        let recovered = extract_raw_parameters(parsed.d);
        assert_eq!(recovered.b0, original.b0, "b0");
        assert_eq!(recovered.b1, original.b1, "b1");
        assert_eq!(recovered.b2, original.b2, "b2");
        assert_eq!(recovered.b3, original.b3, "b3");
        assert_eq!(recovered.b4, original.b4, "b4");
        assert_eq!(recovered.b5, original.b5, "b5");
        assert_eq!(recovered.b6, original.b6, "b6");
        assert_eq!(recovered.b7, original.b7, "b7");
        assert_eq!(recovered.b8, original.b8, "b8");
    }

    /// The full real-world round trip: pack parameters, build the logical frame, convert to the
    /// real 9-byte chip/wire format and back (`interleave.rs`), and confirm parsing the result still
    /// recovers a zero-error, exactly-matching frame -- catches a mismatch between `build_frame`'s
    /// own spare-bit convention and `interleave::frame_to_wire_bytes`'s that a purely-logical round
    /// trip (which never leaves `u128` frame space) wouldn't exercise.
    #[test]
    fn build_frame_round_trips_through_real_wire_bytes() {
        use crate::ambe::float::dstar::interleave::{frame_to_wire_bytes, wire_bytes_to_frame};

        let original = RawParameters {
            b0: 0b101_0110,
            b1: 0b1010,
            b2: 0b10_1101,
            b3: 0b1_0110_1101,
            b4: 0b101_1010,
            b5: 0b1101,
            b6: 0b0110,
            b7: 0b1001,
            b8: 0b1010,
        };
        let d = pack_raw_parameters(&original);
        let frame = build_frame(d);

        let wire_bytes = frame_to_wire_bytes(frame);
        let frame_back = wire_bytes_to_frame(&wire_bytes);
        assert_eq!(frame_back, frame, "wire byte conversion must be lossless");

        let parsed = parse_frame(frame_back);
        assert_eq!(parsed.epsilon_c0, 0);
        assert_eq!(parsed.epsilon_c1, 0);
        let recovered = extract_raw_parameters(parsed.d);
        assert_eq!(recovered.b0, original.b0, "b0");
    }
}

#[cfg(test)]
mod tone_tests {
    use super::*;
    use crate::ambe::float::dstar::decode::{
        classify_b0, classify_tone_index, decode_tone, dtmf_digit_from_tone_index, parse_frame,
        FrameKind, ToneKind,
    };

    #[test]
    // Tests [@ANCHOR: dstar_pack_tone_parameters]
    fn every_tone_index_and_volume_round_trips() {
        for index in 0u32..256 {
            for &volume in &[0u32, 1, 37, 128, 255] {
                let d = pack_tone_parameters(index, volume);
                let payload = decode_tone(d);
                assert_eq!(
                    (payload.index, payload.volume),
                    (index, volume),
                    "index {index} volume {volume}"
                );
            }
        }
    }

    #[test]
    fn a_built_tone_frame_is_error_free_classified_as_a_tone_and_decodes() {
        for (index, expect_single_hz) in [(6u32, Some(187.5)), (32, Some(1000.0)), (144 - 16, None)]
        {
            let frame = build_tone_frame(index, 200);
            let parsed = parse_frame(frame);
            assert_eq!(parsed.epsilon_c0 + parsed.epsilon_c1, 0);
            let raw = crate::ambe::float::dstar::decode::extract_raw_parameters(parsed.d);
            assert_eq!(classify_b0(raw.b0), FrameKind::Tone);
            let payload = decode_tone(parsed.d);
            assert_eq!(payload.index, index);
            match (classify_tone_index(payload.index), expect_single_hz) {
                (ToneKind::Single { hz }, Some(want)) => assert!((hz - want).abs() < 1e-9),
                (ToneKind::Dual, None) => {
                    assert!(dtmf_digit_from_tone_index(payload.index).is_some())
                }
                other => panic!("unexpected classification {other:?}"),
            }
        }
    }
}
