// SPDX-License-Identifier: LGPL-3.0-or-later
//! D-STAR's real wire-format bit interleave, confirmed empirically against the live DVSI chip (see
//! `AMBE_CHIP_VALIDATION_FINDINGS.md`'s D-STAR section): the 9 raw CHAND bytes exchanged with the
//! chip are **not** a simple contiguous `C0||C1||C2||C3` bitstream -- they carry the same
//! block-interleave D-STAR uses over the air, byte-for-byte. That interleave table is confirmed
//! against `szechyjs/dsd`'s real, working GMSK demodulator (`include/dstar_const.h`'s `dW`/`dX`
//! tables, used by `processDSTAR` in `dstar.c` to build mbelib's own `ambe_fr[4][24]` from a raw
//! bit stream) -- not transcribed from memory, and not something mbelib's own `ambe3600x2400.c`
//! shows on its own, since that file only ever operates on an already-deinterleaved `ambe_fr` array.
//!
//! The practical implication, confirmed on the real chip: DVSI's AMBE3000/3003 apparently transmits
//! and receives a D-STAR-mode CHAND frame pre-interleaved in exactly the same order the bits go out
//! over D-STAR's own RF -- letting a repeater relay CHAND bits to/from RF directly, with no separate
//! interleave step of its own. Each byte's bits are read LSB-first (confirmed empirically: this was
//! the only byte/bit-order convention, out of byte-order x bit-order x interleave-choice combinations
//! tested, that Golay-decoded 150 consecutive real chip-encoded frames at three different test
//! frequencies with zero corrected errors on both `C0` and `C1`).
//!
//! `dW[i]`/`dX[i]` give the `(block, column)` that wire bit `i` (0..=71, in the byte/bit order above)
//! lands on in mbelib's own `ambe_fr[4][24]` array. Block 0 has 24 columns (`C0`), block 1 has 23
//! (`C1`), block 2 has 11 (`C2`), block 3 has 14 (`C3`) -- confirmed by counting each block's own
//! distinct column set directly from these two tables, exactly matching this module's own
//! independently-sourced `C0`/`C1`/`C2`/`C3` sizes.
//!
//! Within each block, column `N-1` (the highest) is the field's own MSB and column `0` is its LSB --
//! confirmed directly against mbelib's real `mbe_eccAmbe3600x2400C0` (`in[j] = ambe_fr[0][j+1]`,
//! feeding `mbe_golay2312` whose own `in[22]` is the codeword MSB, so `ambe_fr[0][23]` is `C0`'s data
//! MSB) and its `mbe_dumpAmbe3600x2400Frame` (prints every block descending from its own top column).
//! **`C0`'s spare bit is `ambe_fr[0][0]` -- the bottom of the block, not the top** -- an easy mistake
//! this crate's own first version of `decode::parse_frame` made (it masked off `C0`'s top bit as
//! the spare instead, which happened to still Golay-decode "successfully" on some frames by pure
//! coincidence to the *wrong* data, corrupting every downstream `C1` whitening seed).

const BLOCK_LENS: [usize; 4] = [24, 23, 11, 14];

const D_W: [usize; 72] = [
    0, 0, 3, 2, 1, 1, 0, 0, 1, 1, 0, 0, 3, 2, 1, 1, 3, 2, 1, 1, 0, 0, 3, 2, 0, 0, 3, 2, 1, 1, 0, 0,
    1, 1, 0, 0, 3, 2, 1, 1, 3, 2, 1, 1, 0, 0, 3, 2, 0, 0, 3, 2, 1, 1, 0, 0, 1, 1, 0, 0, 3, 2, 1, 1,
    3, 3, 2, 1, 0, 0, 3, 3,
];
const D_X: [usize; 72] = [
    10, 22, 11, 9, 10, 22, 11, 23, 8, 20, 9, 21, 10, 8, 9, 21, 8, 6, 7, 19, 8, 20, 9, 7, 6, 18, 7,
    5, 6, 18, 7, 19, 4, 16, 5, 17, 6, 4, 5, 17, 4, 2, 3, 15, 4, 16, 5, 3, 2, 14, 3, 1, 2, 14, 3,
    15, 0, 12, 1, 13, 2, 0, 1, 13, 0, 12, 10, 11, 0, 12, 1, 13,
];

/// Converts 9 raw CHAND bytes (chip wire order: bytes as received, each byte's bits LSB-first) into
/// this crate's logical 72-bit frame value -- `C0(24)||C1(23)||C2(11)||C3(14)`, each block packed
/// MSB-first, matching [`super::decode::parse_frame`]'s own input convention.
pub fn wire_bytes_to_frame(bytes: &[u8; 9]) -> u128 {
    let mut ambe_fr = [[false; 24]; 4];
    let mut wire_bit = 0usize;
    for &byte in bytes {
        for i in 0..8 {
            let bit = (byte >> i) & 1 == 1;
            ambe_fr[D_W[wire_bit]][D_X[wire_bit]] = bit;
            wire_bit += 1;
        }
    }
    let mut frame: u128 = 0;
    for (block, &len) in BLOCK_LENS.iter().enumerate() {
        for col in (0..len).rev() {
            frame = (frame << 1) | (ambe_fr[block][col] as u128);
        }
    }
    frame
}

/// The exact inverse of [`wire_bytes_to_frame`].
pub fn frame_to_wire_bytes(frame: u128) -> [u8; 9] {
    let frame = frame & ((1u128 << 72) - 1);
    let mut ambe_fr = [[false; 24]; 4];
    let mut shift = 72usize;
    for (block, &len) in BLOCK_LENS.iter().enumerate() {
        for col in (0..len).rev() {
            shift -= 1;
            ambe_fr[block][col] = (frame >> shift) & 1 == 1;
        }
    }
    let mut bytes = [0u8; 9];
    for wire_bit in 0..72 {
        if ambe_fr[D_W[wire_bit]][D_X[wire_bit]] {
            bytes[wire_bit / 8] |= 1 << (wire_bit % 8);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dW`/`dX` must partition the 72 wire bits into exactly the four real block sizes -- catches a
    /// transcription error in either table immediately, before it can hide behind a subtler bug.
    #[test]
    fn interleave_tables_partition_into_the_real_block_sizes() {
        let mut counts = [0usize; 4];
        let mut seen_cols: [std::collections::HashSet<usize>; 4] = Default::default();
        for i in 0..72 {
            counts[D_W[i]] += 1;
            seen_cols[D_W[i]].insert(D_X[i]);
        }
        assert_eq!(counts, [24, 23, 11, 14]);
        for (block, &len) in BLOCK_LENS.iter().enumerate() {
            assert_eq!(seen_cols[block].len(), len, "block {block} column count");
            for col in 0..len {
                assert!(
                    seen_cols[block].contains(&col),
                    "block {block} missing column {col}"
                );
            }
        }
    }

    /// `frame_to_wire_bytes` must be the exact inverse of `wire_bytes_to_frame` in both directions --
    /// the real property this whole module exists for.
    #[test]
    fn wire_bytes_and_frame_round_trip() {
        let bytes: [u8; 9] = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x0F];
        let frame = wire_bytes_to_frame(&bytes);
        let back = frame_to_wire_bytes(frame);
        assert_eq!(back, bytes);

        let frame: u128 = 0x0012_3456_789A_BCDE_F012_u128 & ((1u128 << 72) - 1);
        let bytes = frame_to_wire_bytes(frame);
        let back = wire_bytes_to_frame(&bytes);
        assert_eq!(back, frame);
    }
}
