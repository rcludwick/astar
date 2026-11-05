// SPDX-License-Identifier: LGPL-3.0-or-later
//! The real DVSI chip's FEC codes for RATET(27), determined directly by sampling the live chip's
//! own encoder output rather than by black-box bit-flip relational testing -- see
//! `dvsi_p25fec::wire_format`'s module doc for why relational testing (this investigation's original
//! approach) hits a hard mathematical wall: any weight-3/weight-7 codeword relationship is
//! preserved by the whole automorphism group of the abstract code (order 20160 for Hamming(15,11),
//! roughly 10^7 for Golay(23,12)), so no amount of flip-sweep data can ever pin down which specific
//! physical bit is which codeword coordinate. The fix, per `AMBE_CHIP_VALIDATION_FINDINGS.md`'s
//! own record of this pivot: capture real chip frames directly (`examples/p25_ratet27_capture_*`,
//! ~2200 distinct frames across synthetic tones, noise at 8 amplitudes, and 16 seconds of real
//! recorded speech), deinterleave via [`super::wire_format::natural_position`], and treat
//! each block's observed natural-order bit patterns as vectors in a GF(2) linear code -- their
//! **rank** and **row-reduced basis** directly give the real generator matrix, no permutation
//! puzzle involved.
//!
//! # Results
//!
//! - **`g0`, `g1`, `g2` (Golay(23,12))**: rank exactly 12 (matching a pure, unwhitened codeword,
//!   no extra data-dependent modulation mixed into the wire bits), and the row-reduced generator
//!   basis is **bit-for-bit identical** to [`super::fec::golay_encode`]'s own systematic
//!   construction -- same data/parity split, same bit order, same `fec.rs`'s own `GOLAY_PARITY`
//!   values in every one of 36 compared rows (12 rows x 3 blocks). This chip's real Golay code for
//!   these 3 blocks needs no new implementation at all: [`super::fec::golay_encode`] and
//!   [`super::fec::golay_decode`] apply directly to each block's 23 natural-order bits.
//! - **`u4`, `u5`, `u6` (Hamming(15,11))**: rank exactly 11 each, and all three blocks share one
//!   **identical** row-reduced generator basis (one FEC routine used 3 times, as expected) -- but
//!   this basis's parity submatrix is a *different*, though equally valid, assignment of the same
//!   11 nonzero 4-bit column values `fec.rs`'s own `HAMMING_PARITY` uses, in a different data-bit
//!   order. [`HAMMING_PARITY_CHIP`] below is that real, chip-derived table, validated against 32
//!   independently-obtained empirical weight-3 codeword relationships (18 through `u4`, 7 through
//!   `u5`, 7 through `u6`, from three separate anchor-bit sweeps run earlier in this investigation)
//!   with zero mismatches -- every single one of those 32 relationships holds exactly under this
//!   generator matrix.
//! - **`c7`**: rank exactly 7 (its full width) -- confirms these 7 bits are genuinely unprotected
//!   raw data, not run through any code at all, matching the DVSI manual's own description.
//! - **`g3`**: **its real codeword space (an 8-dimensional subcode of Golay(23,12): every one of its 256 codewords
//!   also decodes through `golay_decode` at distance 0) is now implemented, though its semantic
//!   content remains open.** Across ~3500 distinct captured frames spanning pure tones, 8 noise
//!   amplitudes, real recorded speech, DTMF, dual-tones, and chirps, `g3`'s observed wire bits
//!   plateau at GF(2) rank 8 (not the expected 12), with 4 of its 23 natural-order bits (offsets
//!   2-5) *provably* always 0 -- confirmed structurally (every one of [`G3_GENERATOR`]'s own 8
//!   basis rows is 0 at those positions, not merely unobserved-as-1 in the sample). Unlike
//!   `g0`-`g2`, `g3`'s independent bits are natural offsets `{0,1,6,7,8,9,10,11}`, not a
//!   contiguous systematic prefix. [`g3_encode`]/[`g3_decode`] implement this real, validated
//!   (every captured frame confirmed within its span) 8-bit codeword space -- a genuine software
//!   duplicate of `g3`'s actual behavior, even though *why* only 8 of its 12 nominal Golay data
//!   bits carry real information, or what real-world parameter this 8-bit subspace represents,
//!   remains unresolved (see `AMBE_CHIP_VALIDATION_FINDINGS.md` sections 23-29 for the full
//!   experimental record, including several tested-and-refuted semantic hypotheses).

use super::wire_format::{block_wire_members, Block};
use crate::ambe::general::fec::golay_decode;

/// The real DVSI chip's Hamming(15,11) parity submatrix for RATET(27)'s `u4`/`u5`/`u6` blocks, in
/// natural-offset order (row `i` is the codeword this block's chip encoder produces when only data
/// bit `i`, 0-indexed from the block's first natural offset, is set) -- derived directly from
/// ~2200 captured real chip frames (GF(2) row-reduction of the observed codeword space), and
/// cross-validated against 32 independently-obtained empirical weight-3 relationships with zero
/// mismatches (see this module's own doc comment). A genuinely different data-bit-to-column
/// assignment from `fec.rs`'s own `HAMMING_PARITY`, though the same underlying 11 nonzero 4-bit
/// column values (confirmed by the `chip_hamming_parity_uses_the_same_15_nonzero_columns_as_fec_rs`
/// test below) -- i.e. the same abstract [15,11,3] Hamming code, differently labeled.
pub const HAMMING_PARITY_CHIP: [u8; 11] = [
    0b1001, 0b1101, 0b1111, 0b1110, 0b0111, 0b1010, 0b0101, 0b1011, 0b1100, 0b0110, 0b0011,
];

/// Encodes 11 data bits (low 11 bits of `data`, MSB-first -- bit 10 is natural offset 0) into this
/// chip's real 15-bit Hamming codeword, systematic (`data` in the high 11 bits, parity in the low
/// 4), using [`HAMMING_PARITY_CHIP`] rather than `fec.rs`'s own `HAMMING_PARITY`.
pub fn hamming_encode_chip(data: u16) -> u16 {
    let data = data & 0x07FF;
    let mut parity: u8 = 0;
    for (i, &row) in HAMMING_PARITY_CHIP.iter().enumerate() {
        if (data >> (10 - i)) & 1 == 1 {
            parity ^= row;
        }
    }
    (data << 4) | (parity as u16)
}

/// Minimum-distance decoding for [`hamming_encode_chip`], same brute-force technique as
/// [`super::fec::hamming_decode`].
pub fn hamming_decode_chip(received: u16) -> (u16, u32) {
    let received = received & 0x7FFF;
    let mut best_data = 0u16;
    let mut best_distance = u32::MAX;
    for data in 0u16..2048 {
        let distance = (hamming_encode_chip(data) ^ received).count_ones();
        if distance < best_distance {
            best_distance = distance;
            best_data = data;
        }
    }
    (best_data, best_distance)
}

/// `g3`'s own real generator matrix (rank 8, not the full rank-12 systematic Golay generator that
/// `g0`-`g2` use), derived by GF(2) row-reduction of ~3500 distinct real chip frames spanning every
/// stimulus type this investigation tried (tones, noise, real speech, DTMF, dual-tones, chirps).
/// Row `i` is the 23-bit codeword (natural offset order, MSB-first) `g3` emits when only data bit
/// `i` (0-indexed) is set -- **not** a standard Golay(23,12) codeword pattern: the independent data
/// bits correspond to natural offsets `{0,1,6,7,8,9,10,11}` (confirmed by this basis's own
/// row-reduced pivot columns), not the contiguous `0..8` a systematic code would put them at.
/// Validated: every one of those ~3500 real captured `g3` values lies exactly in this basis's span
/// (see `AMBE_CHIP_VALIDATION_FINDINGS.md` section 29) -- this is a complete, real description of
/// `g3`'s actual codeword space, not a partial approximation, even though what real-world parameter
/// this 8-bit subspace semantically represents remains open (see the module doc's own disclosure).
const G3_GENERATOR: [u32; 8] = [
    0b10000000000011000111010,
    0b01000000000001100011101,
    0b00000010000001101100110,
    0b00000001000000110110011,
    0b00000000100011011100011,
    0b00000000010010101001011,
    0b00000000001010010011111,
    0b00000000000110001110101,
];

/// Encodes 8 data bits (low 8 bits of `data`, MSB-first) into a `g3` codeword using
/// [`G3_GENERATOR`]. Unlike [`super::fec::golay_encode`], this is not a standard systematic
/// construction -- it's `data`'s own linear combination of `G3_GENERATOR`'s 8 basis rows.
pub fn g3_encode(data: u8) -> u32 {
    let mut codeword = 0u32;
    for (i, &row) in G3_GENERATOR.iter().enumerate() {
        if (data >> (7 - i)) & 1 == 1 {
            codeword ^= row;
        }
    }
    codeword
}

/// Minimum-distance decoding for [`g3_encode`]: brute-force search over all 256 real codewords
/// (`g3`'s actual, confirmed rank-8 codeword space, not the full 4096-codeword Golay space
/// `g0`-`g2` use) -- exact for this linear code at this size, same technique as
/// [`super::fec::golay_decode`]/[`super::fec::hamming_decode`].
pub fn g3_decode(received: u32) -> (u8, u32) {
    let received = received & 0x7F_FFFF;
    let mut best_data = 0u8;
    let mut best_distance = u32::MAX;
    for data in 0u16..256 {
        let distance = (g3_encode(data as u8) ^ received).count_ones();
        if distance < best_distance {
            best_distance = distance;
            best_data = data as u8;
        }
    }
    (best_data, best_distance)
}

/// Extracts a block's natural-order bits from a full 144-bit wire frame (MSB-first within each of
/// the 18 bytes, matching every `p25_ratet27_*` chip-test tool's own `BITS_OFFSET` convention) and
/// decodes it with the appropriate real chip FEC. Returns the recovered data bits and the number of
/// bit errors corrected (for `g3`, out of its own real 8-bit codeword space, not a full Golay
/// decode -- see [`G3_GENERATOR`]'s own doc comment for why).
pub fn decode_block(wire_frame_bits: &[bool; 144], block: Block) -> (u16, u32) {
    let members = block_wire_members(block);
    let mut received: u32 = 0;
    for (offset, &wire) in members.iter().enumerate() {
        if wire_frame_bits[wire] {
            received |= 1 << (members.len() - 1 - offset);
        }
    }
    match block {
        Block::Golay { index: 3 } => {
            let (data, distance) = g3_decode(received);
            (data as u16, distance)
        }
        Block::Golay { .. } => {
            let (data, distance) = golay_decode(received);
            (data, distance)
        }
        Block::Hamming { .. } => {
            let (data, distance) = hamming_decode_chip(received as u16);
            (data, distance)
        }
        Block::Raw => (received as u16, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire_format::block_for_natural;
    use super::*;
    use crate::ambe::general::fec::golay_encode;

    #[test]
    fn hamming_encode_chip_of_zero_is_zero() {
        assert_eq!(hamming_encode_chip(0), 0);
    }

    #[test]
    fn hamming_chip_weight_distribution_matches_the_known_enumerator() {
        // Same real, independent check as fec.rs's own: a different labeling of a genuine
        // [15,11,3] Hamming code has the identical weight distribution as any other labeling,
        // since permuting coordinates never changes a linear code's weight distribution -- this
        // confirms HAMMING_PARITY_CHIP really does describe a valid Hamming(15,11) code and not
        // some transcription slip in extracting it from the captured frames.
        let mut counts = std::collections::BTreeMap::new();
        for data in 0u32..2048 {
            let codeword = hamming_encode_chip(data as u16);
            *counts.entry(codeword.count_ones()).or_insert(0u32) += 1;
        }
        let expected: std::collections::BTreeMap<u32, u32> = [
            (0, 1),
            (3, 35),
            (4, 105),
            (5, 168),
            (6, 280),
            (7, 435),
            (8, 435),
            (9, 280),
            (10, 168),
            (11, 105),
            (12, 35),
            (15, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(counts, expected);
    }

    #[test]
    fn chip_hamming_parity_uses_the_same_15_nonzero_columns_as_fec_rs() {
        // HAMMING_PARITY_CHIP's 11 entries, plus the 4 implicit unit-vector parity columns
        // (0b1000, 0b0100, 0b0010, 0b0001), must be all 15 nonzero 4-bit values exactly once --
        // the defining structural property of a [15,11] Hamming code's parity-check matrix,
        // independent of which specific column goes with which data bit.
        let mut all_columns: Vec<u8> = HAMMING_PARITY_CHIP.to_vec();
        all_columns.extend_from_slice(&[0b1000, 0b0100, 0b0010, 0b0001]);
        all_columns.sort_unstable();
        let expected: Vec<u8> = (1u8..=15).collect();
        assert_eq!(all_columns, expected);
    }

    /// Every empirical weight-3 codeword relationship this investigation found by direct chip
    /// bit-flip experiment (three independent anchor sweeps: wire bit 127 for `u4`, bit 9 for
    /// `u5`, bit 11 for `u6`), re-expressed as a constraint on `HAMMING_PARITY_CHIP` -- if the
    /// derived generator matrix is right, `column(a) XOR column(b) XOR column(c)` must be zero
    /// for every one of these 32 triples. This is the same validation performed offline in Python
    /// during the investigation (zero mismatches), kept here as a permanent regression test.
    fn chip_column(offset: usize) -> u8 {
        if offset < 11 {
            HAMMING_PARITY_CHIP[offset]
        } else {
            [0b1000, 0b0100, 0b0010, 0b0001][offset - 11]
        }
    }

    fn assert_weight3_relationship(wire_triple: (usize, usize, usize), block_start: usize) {
        use super::super::wire_format::natural_position;
        let (a, b, c) = wire_triple;
        let offsets: Vec<usize> = [a, b, c]
            .iter()
            .map(|&w| natural_position(w) - block_start)
            .collect();
        let xor = chip_column(offsets[0]) ^ chip_column(offsets[1]) ^ chip_column(offsets[2]);
        assert_eq!(xor, 0, "triple {wire_triple:?} (block start {block_start}) not a valid codeword under HAMMING_PARITY_CHIP");
    }

    #[test]
    fn all_18_confirmed_u4_weight3_triples_are_valid_codewords_under_the_chip_generator() {
        let triples = [
            (127, 8, 92),
            (127, 20, 32),
            (127, 128, 139),
            (127, 68, 103),
            (127, 44, 104),
            (127, 56, 80),
            (127, 115, 116),
            (8, 20, 115),
            (8, 32, 116),
            (8, 44, 56),
            (8, 68, 128),
            (8, 80, 104),
            (8, 103, 139),
            (92, 20, 116),
            (92, 32, 115),
            (92, 44, 80),
            (92, 56, 104),
            (92, 68, 139),
        ];
        for t in triples {
            assert_weight3_relationship(t, 92);
        }
    }

    #[test]
    fn all_7_confirmed_u5_weight3_triples_are_valid_codewords_under_the_chip_generator() {
        let triples = [
            (9, 10, 21),
            (9, 22, 93),
            (9, 33, 117),
            (9, 45, 57),
            (9, 69, 129),
            (9, 81, 105),
            (9, 140, 141),
        ];
        for t in triples {
            assert_weight3_relationship(t, 107);
        }
    }

    #[test]
    fn all_7_confirmed_u6_weight3_triples_are_valid_codewords_under_the_chip_generator() {
        let triples = [
            (11, 23, 118),
            (11, 34, 94),
            (11, 35, 82),
            (11, 46, 70),
            (11, 47, 59),
            (11, 58, 130),
            (11, 106, 142),
        ];
        for t in triples {
            assert_weight3_relationship(t, 122);
        }
    }

    /// Sanity check that [`decode_block`] round-trips: encoding arbitrary data through the chip's
    /// real Golay code (for `g0`, known bit-for-bit identical to `fec.rs`) and placing it on a
    /// synthetic wire frame at `g0`'s confirmed positions must decode back to the same data with
    /// zero corrected errors.
    #[test]
    fn decode_block_round_trips_g0_with_the_real_golay_code() {
        for data in [0u16, 1, 0xABC, 0xFFF] {
            let codeword = golay_encode(data & 0x0FFF);
            let members = block_wire_members(Block::Golay { index: 0 });
            let mut wire_frame_bits = [false; 144];
            for (offset, &wire) in members.iter().enumerate() {
                wire_frame_bits[wire] = (codeword >> (members.len() - 1 - offset)) & 1 == 1;
            }
            let (decoded, distance) = decode_block(&wire_frame_bits, Block::Golay { index: 0 });
            assert_eq!(distance, 0);
            assert_eq!(decoded, data & 0x0FFF);
        }
    }

    /// Same round-trip check for a Hamming block (`u4`), using the chip's own real
    /// `HAMMING_PARITY_CHIP`-based codec.
    #[test]
    fn decode_block_round_trips_u4_with_the_real_chip_hamming_code() {
        for data in [0u16, 1, 0x2AB, 0x7FF] {
            let codeword = hamming_encode_chip(data & 0x07FF);
            let members = block_wire_members(Block::Hamming { index: 0 });
            let mut wire_frame_bits = [false; 144];
            for (offset, &wire) in members.iter().enumerate() {
                wire_frame_bits[wire] = (codeword >> (members.len() - 1 - offset)) & 1 == 1;
            }
            let (decoded, distance) = decode_block(&wire_frame_bits, Block::Hamming { index: 0 });
            assert_eq!(distance, 0);
            assert_eq!(decoded, data & 0x07FF);
        }
    }

    #[test]
    fn g3_encode_of_zero_is_zero() {
        assert_eq!(g3_encode(0), 0);
    }

    #[test]
    fn g3_decode_round_trips_every_one_of_its_256_real_codewords_with_zero_distance() {
        for data in 0u16..256 {
            let codeword = g3_encode(data as u8);
            let (decoded, distance) = g3_decode(codeword);
            assert_eq!(distance, 0, "data={data}");
            assert_eq!(decoded, data as u8, "data={data}");
        }
    }

    #[test]
    fn g3_generator_never_sets_the_4_confirmed_always_zero_bit_positions() {
        // Natural offsets 2-5 within g3 are confirmed always 0 across ~3500 real captured frames
        // (AMBE_CHIP_VALIDATION_FINDINGS.md section 23) -- verify the generator structurally
        // agrees, not just on observed samples.
        for data in 0u16..256 {
            let codeword = g3_encode(data as u8);
            for offset in 2..6 {
                let bit = (codeword >> (22 - offset)) & 1;
                assert_eq!(bit, 0, "data={data} offset={offset}");
            }
        }
    }

    /// A representative sample of 15 real `g3` codewords, captured directly from the chip across
    /// this session's full accumulated dataset (~3500 distinct frames spanning tones, noise, real
    /// speech, DTMF, dual-tones, and chirps -- see `docs/references/ratet27_captures/`). Kept as a
    /// permanent regression test: every one of these must decode with zero distance under
    /// [`G3_GENERATOR`], confirming the derived basis genuinely spans the chip's own real codeword
    /// space rather than just this file's own synthetic test vectors.
    const REAL_CAPTURED_G3_CODEWORDS: [u32; 15] = [
        0b00000011111010111100010,
        0b11000011110010001011010,
        0b00000010101110101101111,
        0b01000011101001110110100,
        0b00000011101000010101001,
        0b11000010100000010100010,
        0b11000000100001111000100,
        0b11000010001001011011110,
        0b10000011000010011101111,
        0b01000010110111110100110,
        0b00000011010011110011110,
        0b11000010011011110010101,
        0b00000010000001101100110,
        0b10000011001000001110000,
        0b10000011110101100110010,
    ];

    #[test]
    fn g3_decode_recovers_every_real_captured_codeword_with_zero_distance() {
        for &codeword in &REAL_CAPTURED_G3_CODEWORDS {
            let (_data, distance) = g3_decode(codeword);
            assert_eq!(
                distance, 0,
                "codeword=0b{codeword:023b} not in G3_GENERATOR's span"
            );
        }
    }

    #[test]
    fn decode_block_round_trips_g3_with_its_own_real_8_bit_codeword_space() {
        for data in [0u16, 1, 0xAB, 0xFF] {
            let codeword = g3_encode(data as u8);
            let members = block_wire_members(Block::Golay { index: 3 });
            let mut wire_frame_bits = [false; 144];
            for (offset, &wire) in members.iter().enumerate() {
                wire_frame_bits[wire] = (codeword >> (members.len() - 1 - offset)) & 1 == 1;
            }
            let (decoded, distance) = decode_block(&wire_frame_bits, Block::Golay { index: 3 });
            assert_eq!(distance, 0);
            assert_eq!(decoded, data);
        }
    }

    #[test]
    fn block_for_natural_still_agrees_with_block_wire_members_round_trip() {
        // Cross-module sanity: every wire member of a block, run through block_for_natural, must
        // report that same block back.
        for (block, _) in [
            (Block::Golay { index: 0 }, 0),
            (Block::Hamming { index: 1 }, 0),
            (Block::Raw, 0),
        ] {
            for wire in block_wire_members(block) {
                use super::super::wire_format::natural_position;
                let (found_block, _offset) = block_for_natural(natural_position(wire));
                assert_eq!(found_block, block);
            }
        }
    }
}
