// SPDX-License-Identifier: LGPL-3.0-or-later
//! A single, reusable decode of a whole RATET(27) channel frame's 8 sub-blocks, consolidating what
//! was previously scattered as repeated boilerplate (byte-to-wire-bit unpacking, then one
//! `decode_block` call per block) across roughly 15 different validation/probe examples. This ties
//! together `dvsi_p25fec::wire_format`, `dvsi_p25fec::fec`, `dvsi_p25fec::dtmf`, and `dvsi_p25fec::dtx` into one public
//! entry point -- [`decode_frame`] -- so a real consumer (or a future example) decodes a channel
//! frame once, not by re-deriving the same 8-block extraction from scratch.
//!
//! This deliberately stays a thin decode-and-tag layer: [`Ratet27Frame`] carries every block's
//! decoded value and FEC distance, plus two delegating helpers ([`Ratet27Frame::dtmf_digit`],
//! [`Ratet27Frame::is_dtx_silence`]) for the two interpretations this project has actually
//! confirmed. It does not attempt to classify `VOICE_ACTIVE` from wire bits alone -- section 35 of
//! `AMBE_CHIP_VALIDATION_FINDINGS.md` established that isn't possible from `g0` alone and is only a
//! partial, unvalidated signal from `g1` -- and it carries no `PKT_CHANFMT`/`ECMODE_OUT` parsing,
//! since that lives in the packet layer, not the frame's own 144 protected/unprotected bits.

use super::dtmf::decode_dtmf_digit;
use super::dtx::is_dtx_silence_frame;
use super::fec::decode_block;
use super::wire_format::Block;

/// One decoded value plus its FEC distance (corrected-bit count for Golay/Hamming blocks, always 0
/// for the unprotected [`Block::Raw`] `c7` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedBlock {
    pub value: u16,
    pub distance: u32,
}

/// Every one of RATET(27)'s 8 confirmed sub-blocks, decoded from one 144-bit channel frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratet27Frame {
    pub g0: DecodedBlock,
    pub g1: DecodedBlock,
    pub g2: DecodedBlock,
    pub g3: DecodedBlock,
    pub u4: DecodedBlock,
    pub u5: DecodedBlock,
    pub u6: DecodedBlock,
    pub c7: DecodedBlock,
}

/// Decodes a RATET(27) channel frame's 18 protected/unprotected data bytes (the `CHAND` field's own
/// 144 bits, MSB-first within each byte -- the same layout every capture tool in this project reads
/// starting at `BITS_OFFSET` in the raw DVSI packet) into all 8 confirmed sub-blocks.
pub fn decode_frame(wire_bytes: &[u8; 18]) -> Ratet27Frame {
    let mut wire_frame_bits = [false; 144];
    for (byte_idx, &byte) in wire_bytes.iter().enumerate() {
        for bit_idx in 0..8 {
            wire_frame_bits[byte_idx * 8 + bit_idx] = (byte >> (7 - bit_idx)) & 1 == 1;
        }
    }
    let decode = |block: Block| -> DecodedBlock {
        let (value, distance) = decode_block(&wire_frame_bits, block);
        DecodedBlock { value, distance }
    };
    Ratet27Frame {
        g0: decode(Block::Golay { index: 0 }),
        g1: decode(Block::Golay { index: 1 }),
        g2: decode(Block::Golay { index: 2 }),
        g3: decode(Block::Golay { index: 3 }),
        u4: decode(Block::Hamming { index: 0 }),
        u5: decode(Block::Hamming { index: 1 }),
        u6: decode(Block::Hamming { index: 2 }),
        c7: decode(Block::Raw),
    }
}

impl Ratet27Frame {
    /// Sum of every block's own FEC distance -- `0` means every block decoded to a valid codeword
    /// with no corrected errors, the same "zero-error" bar `ambe_chip_validate_ratet27.rs` applies.
    pub fn total_distance(&self) -> u32 {
        self.g0.distance
            + self.g1.distance
            + self.g2.distance
            + self.g3.distance
            + self.u4.distance
            + self.u5.distance
            + self.u6.distance
            + self.c7.distance
    }

    /// `true` if every block decoded with zero corrected errors.
    pub fn is_zero_error(&self) -> bool {
        self.total_distance() == 0
    }

    /// The DTMF `(row_index, column_index)` this frame encodes, if `g0`/`u4` fall in the confirmed
    /// DTMF range (section 25) -- delegates to [`super::dtmf::decode_dtmf_digit`].
    pub fn dtmf_digit(&self) -> Option<(u8, u8)> {
        decode_dtmf_digit(self.g0.value, self.u4.value)
    }

    /// Whether this frame's `g0` matches the confirmed genuine-silence constant -- delegates to
    /// [`super::dtx::is_dtx_silence_frame`]. See that function's own doc comment (and
    /// `AMBE_CHIP_VALIDATION_FINDINGS.md` section 35/36) for why this is deliberately narrower than
    /// "is this frame inactive."
    pub fn is_dtx_silence(&self) -> bool {
        is_dtx_silence_frame(self.g0.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_bytes_from_hex(hexstr: &str) -> [u8; 18] {
        let mut bytes = [0u8; 18];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hexstr[i * 2..i * 2 + 2], 16).unwrap();
        }
        bytes
    }

    /// One real captured DTMF digit ('1': row 697Hz, column 1209Hz) from `real_dtmf_sweep.tsv`,
    /// with known decoded values per section 25's own fully-resolved DTMF characterization.
    #[test]
    fn decode_frame_recovers_a_real_captured_dtmf_digit() {
        let wire = wire_bytes_from_hex("d08f00b08a00d00d00100400000108000400");
        let frame = decode_frame(&wire);
        assert_eq!(frame.g0.value, 4032);
        assert_eq!(frame.u4.value, 80);
        assert_eq!(frame.g1.value, 2944);
        assert_eq!(frame.g2.value, 0);
        assert_eq!(frame.g3.value, 0);
        assert_eq!(frame.u5.value, 0);
        assert_eq!(frame.u6.value, 0);
        assert_eq!(frame.c7.value, 0);
        assert!(frame.is_zero_error());
        assert_eq!(frame.dtmf_digit(), Some((0, 0)));
        assert!(!frame.is_dtx_silence());
    }

    /// One real captured genuine-silence frame from `dtx_silence_sweep.tsv` (`dtxon_silence`,
    /// section 31/35's own confirmed constant).
    #[test]
    fn decode_frame_recognizes_a_real_captured_dtx_silence_frame() {
        let wire = wire_bytes_from_hex("e0298bcddad32575cd6d44cd4c126302c88e");
        let frame = decode_frame(&wire);
        assert_eq!(frame.g0.value, 3841);
        assert!(frame.is_dtx_silence());
        assert_eq!(frame.dtmf_digit(), None);
    }

    /// `is_zero_error` genuinely reflects `total_distance`, not just `g0`/`g1`.
    #[test]
    fn is_zero_error_matches_total_distance() {
        let wire = wire_bytes_from_hex("e0298bcddad32575cd6d44cd4c126302c88e");
        let frame = decode_frame(&wire);
        assert_eq!(frame.is_zero_error(), frame.total_distance() == 0);
    }
}
