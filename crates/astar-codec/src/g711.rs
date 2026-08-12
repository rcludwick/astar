// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! ITU-T G.711 PCM companding codecs (μ-law and A-law).
//!
//! Bit layouts follow ITU-T Recommendation G.711 (11/88). The implementation
//! is byte-identical to the vendored `iaxclient` C reference (`codec_ulaw.c`,
//! `codec_alaw.c`) so it interoperates with Asterisk / `AllStarLink`.
//!
//! All encode/decode functions are pure, branch-light, and allocation-free.
//! Decode uses 256-entry compile-time tables; encode is direct bit math
//! (cheap enough that a lookup table isn't worth the cache footprint).

// G.711 bit-twiddling inherently mixes signed/unsigned narrow widths.
// These two lints fire on virtually every line of correct codec code, so we
// allow them at the module level rather than scattering local `#[allow]`s.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

// ---------- μ-law ----------

/// μ-law decode table: index by the received byte, get back i16 PCM.
///
/// Built per G.711 §2: invert the byte, split into sign / 3-bit exponent /
/// 4-bit mantissa, then `sample = exp_lut[exp] + (mantissa << (exp + 3))`,
/// negated when the sign bit is set. Matches the vendored C verbatim.
const ULAW_TO_LIN: [i16; 256] = {
    let mut table = [0_i16; 256];
    let exp_lut: [i32; 8] = [0, 132, 396, 924, 1980, 4092, 8316, 16764];
    let mut i = 0;
    while i < 256 {
        let b = (!i) & 0xFF;
        let sign = b & 0x80;
        let exponent = (b >> 4) & 0x07;
        let mantissa = b & 0x0F;
        let mut sample = exp_lut[exponent] + ((mantissa as i32) << (exponent + 3));
        if sign != 0 {
            sample = -sample;
        }
        table[i] = sample as i16;
        i += 1;
    }
    table
};

/// Encode one 16-bit linear PCM sample to a μ-law byte.
///
/// Algorithm (G.711 §2): take sign-magnitude, bias by 0x84, find the
/// segment (position of MSB), pack `~(sign|seg|mantissa)`. The optional
/// CCITT "0x00 -> 0x02" trap matches the vendored implementation.
#[must_use]
pub fn ulaw_encode(pcm: i16) -> u8 {
    // Sign-magnitude form. Note: -i16::MIN overflows; clip first.
    let mut sample = i32::from(pcm);
    let sign = if sample < 0 {
        sample = -sample;
        0x80
    } else {
        0x00
    };
    if sample > 32635 {
        sample = 32635;
    }
    sample += 0x84;

    // exponent = position of highest set bit in (sample >> 7), clamped to 7.
    let exponent = i32::from(ULAW_EXP_LUT[((sample >> 7) & 0xFF) as usize]);
    let mantissa = (sample >> (exponent + 3)) & 0x0F;
    let byte = !(sign | (exponent << 4) | mantissa) as u8;
    // CCITT trap: 0x00 is reserved for signalling; remap to next quietest code.
    if byte == 0 { 0x02 } else { byte }
}

/// Decode one μ-law byte to 16-bit linear PCM.
#[must_use]
pub fn ulaw_decode(byte: u8) -> i16 {
    ULAW_TO_LIN[byte as usize]
}

/// 256-entry segment-index lookup keyed by `(biased_sample >> 7) & 0xFF`.
/// Equivalent to "find the index of the high byte's MSB, clamped to 7".
const ULAW_EXP_LUT: [u8; 256] = {
    let mut t = [0_u8; 256];
    let mut i = 0;
    while i < 256 {
        // Replicates Reese's hand-built table: 0,0, then 1,1, then 2x4,
        // 3x8, ... up to 7 filling the remaining 128 entries.
        t[i] = match i {
            0..=1 => 0,
            2..=3 => 1,
            4..=7 => 2,
            8..=15 => 3,
            16..=31 => 4,
            32..=63 => 5,
            64..=127 => 6,
            _ => 7,
        };
        i += 1;
    }
    t
};

/// μ-law encode a buffer of PCM samples into the output byte slice.
///
/// # Panics
/// Panics if `pcm.len() != out.len()`.
pub fn ulaw_encode_slice(pcm: &[i16], out: &mut [u8]) {
    assert_eq!(
        pcm.len(),
        out.len(),
        "g711: ulaw_encode_slice length mismatch"
    );
    for (s, o) in pcm.iter().zip(out.iter_mut()) {
        *o = ulaw_encode(*s);
    }
}

/// μ-law decode a buffer of bytes into the output PCM slice.
///
/// # Panics
/// Panics if `bytes.len() != out.len()`.
pub fn ulaw_decode_slice(bytes: &[u8], out: &mut [i16]) {
    assert_eq!(
        bytes.len(),
        out.len(),
        "g711: ulaw_decode_slice length mismatch"
    );
    for (b, o) in bytes.iter().zip(out.iter_mut()) {
        *o = ulaw_decode(*b);
    }
}

// ---------- A-law ----------

/// A-law decode table.
///
/// Per G.711 §1: XOR the byte with 0x55, split into sign / 3-bit segment /
/// 4-bit quantization, then reconstruct linear magnitude with the
/// segment-dependent shift. Sign bit set = positive (note: opposite of μ-law).
const ALAW_TO_LIN: [i16; 256] = {
    let mut table = [0_i16; 256];
    let mut i = 0;
    while i < 256 {
        let alaw = (i ^ 0x55) & 0xFF;
        let mut value = ((alaw & 0x0F) << 4) as i32;
        let segment = (alaw & 0x70) >> 4;
        match segment {
            0 => {}
            1 => value += 0x100,
            _ => {
                value += 0x100;
                value <<= segment - 1;
            }
        }
        table[i] = if (alaw & 0x80) != 0 {
            value as i16
        } else {
            -value as i16
        };
        i += 1;
    }
    table
};

/// Encode one 16-bit linear PCM sample to an A-law byte.
#[must_use]
pub fn alaw_encode(pcm: i16) -> u8 {
    // G.711 §1: sign bit is 1 for positive samples (mask 0x55 always; |= 0x80
    // when non-negative). Magnitude is compared against segment boundaries.
    const SEGMENTS: [i32; 8] = [0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF];

    let mut mask: i32 = 0x55;
    let mut linear = i32::from(pcm);
    if linear >= 0 {
        mask |= 0x80;
    } else {
        // -i16::MIN doesn't fit in i16; widen to i32 first (already done).
        linear = -linear;
    }

    let mut segment = 8;
    for (i, &boundary) in SEGMENTS.iter().enumerate() {
        if linear <= boundary {
            segment = i;
            break;
        }
    }

    if segment >= 8 {
        // Out of range: maximum-magnitude code.
        return (0x7F ^ mask) as u8;
    }

    let alaw = if segment < 2 {
        (linear >> 4) & 0x0F
    } else {
        (linear >> (segment + 3)) & 0x0F
    };
    ((alaw | ((segment as i32) << 4)) ^ mask) as u8
}

/// Decode one A-law byte to 16-bit linear PCM.
#[must_use]
pub fn alaw_decode(byte: u8) -> i16 {
    ALAW_TO_LIN[byte as usize]
}

/// A-law encode a buffer of PCM samples into the output byte slice.
///
/// # Panics
/// Panics if `pcm.len() != out.len()`.
pub fn alaw_encode_slice(pcm: &[i16], out: &mut [u8]) {
    assert_eq!(
        pcm.len(),
        out.len(),
        "g711: alaw_encode_slice length mismatch"
    );
    for (s, o) in pcm.iter().zip(out.iter_mut()) {
        *o = alaw_encode(*s);
    }
}

/// A-law decode a buffer of bytes into the output PCM slice.
///
/// # Panics
/// Panics if `bytes.len() != out.len()`.
pub fn alaw_decode_slice(bytes: &[u8], out: &mut [i16]) {
    assert_eq!(
        bytes.len(),
        out.len(),
        "g711: alaw_decode_slice length mismatch"
    );
    for (b, o) in bytes.iter().zip(out.iter_mut()) {
        *o = alaw_decode(*b);
    }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    /// G.711 is lossy on the first pass but idempotent on the second:
    /// `decode(encode(decode(encode(x)))) == decode(encode(x))`.
    #[test]
    fn ulaw_double_encode_is_idempotent() {
        for &pcm in &[
            0_i16,
            1,
            -1,
            128,
            -128,
            1024,
            -1024,
            i16::MAX,
            i16::MIN + 1,
            i16::MIN,
            12345,
            -12345,
        ] {
            let once = ulaw_decode(ulaw_encode(pcm));
            let twice = ulaw_decode(ulaw_encode(once));
            assert_eq!(once, twice, "ulaw not idempotent on second pass at {pcm}");
        }
    }

    #[test]
    fn alaw_double_encode_is_idempotent() {
        for &pcm in &[
            0_i16,
            1,
            -1,
            128,
            -128,
            1024,
            -1024,
            i16::MAX,
            i16::MIN + 1,
            i16::MIN,
            12345,
            -12345,
        ] {
            let once = alaw_decode(alaw_encode(pcm));
            let twice = alaw_decode(alaw_encode(once));
            assert_eq!(once, twice, "alaw not idempotent on second pass at {pcm}");
        }
    }

    /// μ-law silence byte: by convention 0xFF is "digital silence" (decodes to 0).
    /// 0x7F is the "negative zero" silence code (decodes to 0 with opposite sign).
    #[test]
    fn ulaw_silence_bytes_decode_to_zero() {
        assert_eq!(ulaw_decode(0xFF), 0);
        assert_eq!(ulaw_decode(0x7F), 0);
    }

    /// A-law silence: 0xD5 (positive zero) and 0x55 (negative zero).
    #[test]
    fn alaw_silence_bytes_decode_to_zero() {
        assert_eq!(alaw_decode(0xD5), 0);
        assert_eq!(alaw_decode(0x55), 0);
    }

    /// μ-law CCITT trap: encoding any sample must never produce 0x00.
    #[test]
    fn ulaw_never_emits_zero_byte() {
        for pcm in i16::MIN..=i16::MAX {
            assert_ne!(ulaw_encode(pcm), 0x00, "ulaw emitted 0x00 for {pcm}");
        }
    }

    /// Known μ-law reference values computed from the canonical Sun Microsystems
    /// reference implementation (and matching the vendored iaxclient tables).
    #[test]
    fn ulaw_known_encode_pairs() {
        // (pcm, expected_ulaw_byte). Values verified against the vendored
        // C reference by hand-tracing the algorithm.
        let cases: &[(i16, u8)] = &[
            (0, 0xFF),
            (1, 0xFF),
            (-1, 0x7F),
            (132, 0xEF), // first sample where the exponent kicks up
            (32635, 0x80),
            // -32635 saturates to magnitude 32635 -> would encode as 0x00,
            // but the CCITT trap remaps 0x00 to 0x02.
            (-32635, 0x02),
            (i16::MAX, 0x80),
            (i16::MIN + 1, 0x02),
        ];
        for &(pcm, expected) in cases {
            assert_eq!(
                ulaw_encode(pcm),
                expected,
                "ulaw_encode({pcm}) wrong: got 0x{:02X}, want 0x{expected:02X}",
                ulaw_encode(pcm)
            );
        }
    }

    /// Round-trip every byte through decode/encode and confirm it's stable.
    /// Byte 0x00 is excluded: encode never emits 0x00 (CCITT trap remaps it
    /// to 0x02), so decode(0x00) re-encodes to 0x02 by design.
    #[test]
    fn ulaw_byte_roundtrip_stable() {
        for b in 1_u8..=255 {
            let pcm = ulaw_decode(b);
            let b2 = ulaw_encode(pcm);
            let pcm2 = ulaw_decode(b2);
            assert_eq!(pcm, pcm2, "byte 0x{b:02X} -> {pcm} -> 0x{b2:02X} -> {pcm2}");
        }
    }

    /// A-law known reference values. Derived from the G.711 spec and the
    /// vendored implementation; verified by running these inputs through
    /// the C reference logic by hand.
    #[test]
    fn alaw_known_encode_pairs() {
        // Values verified by hand-tracing the G.711 §1 algorithm:
        // mask = 0x55 (negative) or 0xD5 (non-negative), find smallest segment
        // s such that |linear| <= segments[s], pack (mantissa | s<<4) ^ mask.
        let cases: &[(i16, u8)] = &[
            (0, 0xD5),        // positive zero
            (-1, 0x55),       // negative zero
            (1, 0xD5),        // segment 0, mantissa 0
            (16, 0xD4),       // segment 0, mantissa 1
            (32, 0xD7),       // segment 0, mantissa 2
            (1000, 0xFA),     // segment 3
            (2000, 0xEA),     // segment 4
            (4096, 0x85),     // segment 5, mantissa 0
            (8000, 0x8A),     // segment 5, mantissa f
            (i16::MAX, 0xAA), // saturation positive (segment 7, mantissa f)
            (i16::MIN, 0x2A), // saturation negative
            (i16::MIN + 1, 0x2A),
        ];
        for &(pcm, expected) in cases {
            let got = alaw_encode(pcm);
            assert_eq!(
                got, expected,
                "alaw_encode({pcm}) got 0x{got:02X}, want 0x{expected:02X}"
            );
        }
    }

    #[test]
    fn alaw_byte_roundtrip_stable() {
        for b in 0_u8..=255 {
            let pcm = alaw_decode(b);
            let b2 = alaw_encode(pcm);
            let pcm2 = alaw_decode(b2);
            assert_eq!(pcm, pcm2, "byte 0x{b:02X} -> {pcm} -> 0x{b2:02X} -> {pcm2}");
        }
    }

    /// One IAX2 mini-frame: 160 samples @ 8 kHz = 20 ms of audio.
    #[test]
    fn ulaw_slice_matches_scalar() {
        let mut pcm = [0_i16; 160];
        for (i, s) in pcm.iter_mut().enumerate() {
            // A modest sine-ish ramp to exercise multiple segments.
            *s = ((i as i32 * 211) - 16384) as i16;
        }

        let mut encoded = [0_u8; 160];
        ulaw_encode_slice(&pcm, &mut encoded);

        for (i, &s) in pcm.iter().enumerate() {
            assert_eq!(encoded[i], ulaw_encode(s));
        }

        let mut decoded = [0_i16; 160];
        ulaw_decode_slice(&encoded, &mut decoded);
        for (i, &b) in encoded.iter().enumerate() {
            assert_eq!(decoded[i], ulaw_decode(b));
        }
    }

    #[test]
    fn alaw_slice_matches_scalar() {
        let mut pcm = [0_i16; 160];
        for (i, s) in pcm.iter_mut().enumerate() {
            *s = ((i as i32 * 211) - 16384) as i16;
        }

        let mut encoded = [0_u8; 160];
        alaw_encode_slice(&pcm, &mut encoded);

        for (i, &s) in pcm.iter().enumerate() {
            assert_eq!(encoded[i], alaw_encode(s));
        }

        let mut decoded = [0_i16; 160];
        alaw_decode_slice(&encoded, &mut decoded);
        for (i, &b) in encoded.iter().enumerate() {
            assert_eq!(decoded[i], alaw_decode(b));
        }
    }

    #[test]
    #[should_panic(expected = "length mismatch")]
    fn ulaw_encode_slice_length_mismatch_panics() {
        let pcm = [0_i16; 160];
        let mut out = [0_u8; 159];
        ulaw_encode_slice(&pcm, &mut out);
    }

    #[test]
    #[should_panic(expected = "length mismatch")]
    fn alaw_decode_slice_length_mismatch_panics() {
        let bytes = [0_u8; 10];
        let mut out = [0_i16; 11];
        alaw_decode_slice(&bytes, &mut out);
    }
}
