// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! 16-bit signed linear PCM ("slin" / "slin16") wire framing.
//!
//! RFC 5456 does not define the byte order, and **Asterisk's chan_iax2 uses
//! DIFFERENT orders for the two slin formats**. Both verified live against
//! Asterisk 20's own generated audio (`Milliwatt()`, 1004 Hz) with the
//! first-difference smoothness oracle in `examples/slin_probe.rs`:
//!
//! - **slin (8 kHz, `AST_FORMAT_SLINEAR`, 1<<6): BIG-endian** (iax-31f7,
//!   2026-07-07). Correct reading: smoothness 0.590 across 39/39 one-second
//!   windows = theoretical `(2·sin(π·1004/8000))² ≈ 0.59` exactly; the
//!   little-endian reading is broadband noise (2.02).
//! - **slin16 (16 kHz, `AST_FORMAT_SLINEAR16`, 1<<15): LITTLE-endian**
//!   (iax-4348, 2026-07-10, same method). Correct reading: smoothness 0.153
//!   across 38/38 windows = theoretical `(2·sin(π·1004/16000))² ≈ 0.155`,
//!   1004 Hz Goertzel concentration 1.0000; the big-endian reading is
//!   broadband noise (2.10, concentration 0.0022).
//!
//! This asymmetry is an Asterisk chan_iax2 quirk (the transmit byteswap is
//! applied to slin but not slin16). Do NOT "unify" the two orders: use
//! [`encode`]/[`decode`] (big-endian) for slin and [`encode_le`]/[`decode_le`]
//! for slin16.

#[inline]
fn to_wire(pcm: i16) -> [u8; 2] {
    pcm.to_be_bytes()
}

#[inline]
fn from_wire(b: [u8; 2]) -> i16 {
    i16::from_be_bytes(b)
}

/// Encode PCM samples into wire bytes (big-endian — slin, 8 kHz). `out` must
/// be exactly `2 * pcm.len()`.
///
/// # Panics
/// Panics if `out.len() != 2 * pcm.len()`.
pub fn encode_slice(pcm: &[i16], out: &mut [u8]) {
    assert_eq!(
        out.len(),
        2 * pcm.len(),
        "slin: encode_slice length mismatch"
    );
    for (o, &s) in out.chunks_exact_mut(2).zip(pcm) {
        o.copy_from_slice(&to_wire(s));
    }
}

/// Decode wire bytes (big-endian — slin, 8 kHz) into PCM. `bytes.len()` must
/// be even and equal `2 * out.len()`.
///
/// # Panics
/// Panics if `bytes.len() != 2 * out.len()`.
pub fn decode_slice(bytes: &[u8], out: &mut [i16]) {
    assert_eq!(
        bytes.len(),
        2 * out.len(),
        "slin: decode_slice length mismatch"
    );
    for (s, b) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        *s = from_wire([b[0], b[1]]);
    }
}

/// Allocating convenience over [`encode_slice`] (big-endian — slin, 8 kHz).
#[must_use]
pub fn encode(pcm: &[i16]) -> Vec<u8> {
    let mut out = vec![0u8; pcm.len() * 2];
    encode_slice(pcm, &mut out);
    out
}

/// Allocating convenience over [`decode_slice`] (big-endian — slin, 8 kHz).
/// `bytes.len()` must be even.
#[must_use]
pub fn decode(bytes: &[u8]) -> Vec<i16> {
    let mut out = vec![0i16; bytes.len() / 2];
    decode_slice(bytes, &mut out);
    out
}

/// Encode PCM samples into wire bytes (little-endian — slin16, 16 kHz).
/// `out` must be exactly `2 * pcm.len()`.
///
/// # Panics
/// Panics if `out.len() != 2 * pcm.len()`.
pub fn encode_slice_le(pcm: &[i16], out: &mut [u8]) {
    assert_eq!(
        out.len(),
        2 * pcm.len(),
        "slin: encode_slice_le length mismatch"
    );
    for (o, &s) in out.chunks_exact_mut(2).zip(pcm) {
        o.copy_from_slice(&s.to_le_bytes());
    }
}

/// Decode wire bytes (little-endian — slin16, 16 kHz) into PCM. `bytes.len()`
/// must be even and equal `2 * out.len()`.
///
/// # Panics
/// Panics if `bytes.len() != 2 * out.len()`.
pub fn decode_slice_le(bytes: &[u8], out: &mut [i16]) {
    assert_eq!(
        bytes.len(),
        2 * out.len(),
        "slin: decode_slice_le length mismatch"
    );
    for (s, b) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        *s = i16::from_le_bytes([b[0], b[1]]);
    }
}

/// Allocating convenience over [`encode_slice_le`] (little-endian — slin16,
/// 16 kHz).
#[must_use]
pub fn encode_le(pcm: &[i16]) -> Vec<u8> {
    let mut out = vec![0u8; pcm.len() * 2];
    encode_slice_le(pcm, &mut out);
    out
}

/// Allocating convenience over [`decode_slice_le`] (little-endian — slin16,
/// 16 kHz). `bytes.len()` must be even.
#[must_use]
pub fn decode_le(bytes: &[u8]) -> Vec<i16> {
    let mut out = vec![0i16; bytes.len() / 2];
    decode_slice_le(bytes, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_bit_exact() {
        let pcm: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 12345, -12345];
        let mut wire = vec![0u8; pcm.len() * 2];
        encode_slice(&pcm, &mut wire);
        let mut back = vec![0i16; pcm.len()];
        decode_slice(&wire, &mut back);
        assert_eq!(pcm, back);
    }

    #[test]
    fn le_round_trip_is_bit_exact() {
        let pcm: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 12345, -12345];
        let mut wire = vec![0u8; pcm.len() * 2];
        encode_slice_le(&pcm, &mut wire);
        let mut back = vec![0i16; pcm.len()];
        decode_slice_le(&wire, &mut back);
        assert_eq!(pcm, back);
    }

    #[test]
    fn wire_is_big_endian() {
        // 0x0102 serializes MSB first — Asterisk's slin wire order,
        // live-verified against Asterisk 20 Milliwatt (iax-31f7 Task 10;
        // see module doc).
        let mut wire = [0u8; 2];
        encode_slice(&[0x0102_i16], &mut wire);
        assert_eq!(wire, [0x01, 0x02]);
    }

    #[test]
    fn le_wire_is_little_endian() {
        // 0x0102 serializes LSB first — Asterisk's slin16 wire order,
        // live-verified against Asterisk 20 Milliwatt (iax-4348 Task 9;
        // see module doc). Deliberately the OPPOSITE of `wire_is_big_endian`.
        let mut wire = [0u8; 2];
        encode_slice_le(&[0x0102_i16], &mut wire);
        assert_eq!(wire, [0x02, 0x01]);
        assert_eq!(decode_le(&wire), vec![0x0102_i16]);
    }

    #[test]
    fn vec_helpers_match_slice_forms() {
        let pcm: Vec<i16> = (0..160_i16).map(|i| i * 100).collect();
        let wire = encode(&pcm);
        assert_eq!(wire.len(), 320);
        assert_eq!(decode(&wire), pcm);
    }

    #[test]
    fn le_vec_helpers_match_slice_forms() {
        let pcm: Vec<i16> = (0..160_i16).map(|i| i * 100).collect();
        let wire = encode_le(&pcm);
        assert_eq!(wire.len(), 320);
        assert_eq!(decode_le(&wire), pcm);
    }
}
