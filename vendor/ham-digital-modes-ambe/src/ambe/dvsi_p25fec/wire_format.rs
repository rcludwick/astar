// SPDX-License-Identifier: LGPL-3.0-or-later
//! The real DVSI chip's P25 full-rate (`RATET` Rate Index 27, "APCO Project 25 with FEC") 144-bit
//! wire format, reverse-engineered by direct chip experiment against a real AMBE3003 -- this is a
//! **different, proprietary format from TIA-102.BABA_2003 Annex H** ([`super::interleave`], which
//! implements the *textbook* interleave and is confirmed NOT to match this chip's actual wire bytes;
//! see `docs/references/AMBE_CHIP_VALIDATION_FINDINGS.md` sections 15-22 for the full experimental
//! record this module is built from).
//!
//! # What is fully established (verified against the real chip, zero assumptions)
//!
//! 1. **The 12x12 block-interleave transform.** Wire (transmitted) bit position `m` (0..144)
//!    corresponds to natural (pre-interleave) position [`natural_position(m)`]. This is a matrix
//!    transpose -- write the natural bitstream down 12 columns, read it back out across 12 rows --
//!    and is self-inverse: applying the same formula to a natural position recovers the wire
//!    position. Confirmed by: mapping each of `u4`/`u5`/`u6`/`c7`'s independently, directly
//!    chip-confirmed wire memberships (found by pure black-box anchor sweeps, no interleave table
//!    assumed at all) through this formula and getting, every time, one contiguous natural range --
//!    see the `*_transforms_to_its_confirmed_natural_range` tests below, which check this against
//!    the literal wire-position sets recorded in the findings doc.
//!
//! 2. **The 8 sub-block natural order and boundaries.** The blocks are simply concatenated by
//!    decreasing error-protection strength: `g0..g3` (four Golay(23,12) blocks, natural 0-91),
//!    `u4..u6` (three Hamming(15,11) blocks, natural 92-136), `c7` (7 unprotected raw bits, natural
//!    137-143). This matches DVSI's own AMBE-3000R Vocoder Chip Users Manual (section 6.9), which
//!    states plainly that low-index bits are "the bits which are most sensitive to bit errors" and
//!    high-index bits "the bits which are least sensitive."
//!
//! 3. **[`block_wire_members`] gives every block's exact wire-bit membership**, computed directly
//!    from points 1+2 (not a separate hardcoded guess) -- and for `u4`/`u5`/`u6`/`c7` this exactly
//!    matches the independently, directly chip-confirmed sets (found with no interleave assumed at
//!    all), with zero mismatches. The 4 Golay blocks' *boundaries* were separately confirmed directly
//!    against the chip (in-block vs. straddling multi-bit-flip tests, section 21) but not walked
//!    bit-by-bit the way the 3 Hamming blocks were -- see the open item below.
//!
//! # What is NOT yet established (open, tracked in the findings doc)
//!
//! The exact bit-index-*within*-codeword permutation -- i.e. given a block's 15 (or 23, or 7) wire
//! bits in natural order, which specific position feeds [`super::fec::hamming_decode`]'s bit 14 vs
//! bit 3 vs bit 0, etc. -- is **not yet uniquely determined**. Black-box pairwise/triple bit-flip
//! experiments alone cannot fully pin this down: the Hamming(15,11) and Golay(23,12) codes' own
//! automorphism groups mean multiple distinct physical-bit-to-codeword-index permutations can
//! satisfy the same observed weight-3/weight-7 codeword relationships from a single anchor bit.
//! Resolving this needs constraints from multiple non-overlapping anchor bits (in progress -- see
//! the findings doc's second/third-anchor sweeps) or direct validation against real captured wire
//! frames. Until confirmed, do not trust decoded FEC data-bit *values* (actual voice-parameter
//! content) against the real chip -- only the block *membership* and *natural ordering* facts above
//! are chip-verified today. `g1`-`g3`'s bit-by-bit membership (as opposed to their confirmed
//! boundaries) is also not yet independently walked the way `u4`-`u6` and `g0` were.

/// Total wire-format frame size in bits (RATET Rate Index 27's own CHAND field length).
pub const TOTAL_BITS: usize = 144;

/// The 12x12 block-interleave transform (self-inverse: applying it twice returns the original
/// position). `144 = 12 * 12`; this is a matrix transpose over that grid.
pub const fn natural_position(m: usize) -> usize {
    12 * (m % 12) + (m / 12)
}

/// One of the 8 FEC sub-blocks that RATET(27)'s 144 bits split into, in natural (pre-interleave)
/// order, ordered by decreasing error-protection strength per DVSI's own manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// Golay(23,12), one of `g0..g3` (`index` 0..4).
    Golay { index: u8 },
    /// Hamming(15,11), one of `u4..u6` (`index` 0..3).
    Hamming { index: u8 },
    /// The 7 unprotected raw bits.
    Raw,
}

/// The natural-order `(start, len)` range for a given block.
const fn natural_range(block: Block) -> (usize, usize) {
    // An out-of-range `index` would address past the 144-bit frame; it is a programming error, reported here
    // instead of surfacing later as an out-of-bounds array access.
    match block {
        Block::Golay { index } => {
            assert!(index < 4, "Golay block index must be 0..4");
            (23 * index as usize, 23)
        }
        Block::Hamming { index } => {
            assert!(index < 3, "Hamming block index must be 0..3");
            (92 + 15 * index as usize, 15)
        }
        Block::Raw => (137, 7),
    }
}

/// Which of the 8 sub-blocks a given *natural* (pre-interleave) bit position belongs to, and its
/// offset within that block (0-indexed from the block's own start).
pub fn block_for_natural(n: usize) -> (Block, usize) {
    assert!(n < TOTAL_BITS, "natural position {n} out of range");
    if n < 92 {
        (
            Block::Golay {
                index: (n / 23) as u8,
            },
            n % 23,
        )
    } else if n < 137 {
        let offset = n - 92;
        (
            Block::Hamming {
                index: (offset / 15) as u8,
            },
            offset % 15,
        )
    } else {
        (Block::Raw, n - 137)
    }
}

/// The wire-bit positions belonging to `block`, in natural (increasing) order -- i.e.
/// `result[i]` is the wire position whose natural offset within the block is `i`. Computed directly
/// from the validated transform and block boundaries, not a separately hand-maintained list.
pub fn block_wire_members(block: Block) -> Vec<usize> {
    let (start, len) = natural_range(block);
    (start..start + len).map(natural_position).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_position_is_self_inverse() {
        for m in 0..TOTAL_BITS {
            assert_eq!(natural_position(natural_position(m)), m, "m={m}");
        }
    }

    #[test]
    fn natural_position_is_a_bijection() {
        let mut seen = [false; TOTAL_BITS];
        for m in 0..TOTAL_BITS {
            let n = natural_position(m);
            assert!(!seen[n], "natural position {n} hit twice (from wire {m})");
            seen[n] = true;
        }
        assert!(
            seen.iter().all(|&s| s),
            "not every natural position was covered"
        );
    }

    #[test]
    fn block_for_natural_agrees_with_the_established_boundaries() {
        assert_eq!(block_for_natural(0).0, Block::Golay { index: 0 });
        assert_eq!(block_for_natural(91).0, Block::Golay { index: 3 });
        assert_eq!(block_for_natural(92).0, Block::Hamming { index: 0 });
        assert_eq!(block_for_natural(136).0, Block::Hamming { index: 2 });
        assert_eq!(block_for_natural(137).0, Block::Raw);
        assert_eq!(block_for_natural(143).0, Block::Raw);
    }

    #[test]
    fn every_block_partitions_the_144_bits_with_no_gaps_or_overlaps() {
        let mut seen = [false; TOTAL_BITS];
        let blocks = [
            Block::Golay { index: 0 },
            Block::Golay { index: 1 },
            Block::Golay { index: 2 },
            Block::Golay { index: 3 },
            Block::Hamming { index: 0 },
            Block::Hamming { index: 1 },
            Block::Hamming { index: 2 },
            Block::Raw,
        ];
        for block in blocks {
            for wire in block_wire_members(block) {
                assert!(
                    !seen[wire],
                    "wire bit {wire} claimed by more than one block"
                );
                seen[wire] = true;
            }
        }
        assert!(
            seen.iter().all(|&s| s),
            "some wire bit not claimed by any block"
        );
    }

    /// `u4`'s wire membership, found by a pure black-box anchor-127 sweep against the real chip with
    /// **no interleave table or transform assumed at all** (`AMBE_CHIP_VALIDATION_FINDINGS.md`
    /// section 18). This is the ground truth the transform is checked against, not derived from it.
    const U4_CHIP_CONFIRMED: [usize; 15] = [
        8, 20, 32, 44, 56, 68, 80, 92, 103, 104, 115, 116, 127, 128, 139,
    ];

    /// `u5`'s wire membership, found the same way (section 20), independently of the transform's own
    /// prediction (which happened to match it exactly, zero mismatches).
    const U5_CHIP_CONFIRMED: [usize; 15] = [
        9, 10, 21, 22, 33, 45, 57, 69, 81, 93, 105, 117, 129, 140, 141,
    ];

    /// `u6`'s wire membership (section 21), same technique, same zero-mismatch outcome.
    const U6_CHIP_CONFIRMED: [usize; 15] = [
        11, 23, 34, 35, 46, 47, 58, 59, 70, 82, 94, 106, 118, 130, 142,
    ];

    /// `c7`'s wire membership (section 20): the 7 unprotected raw bits, each independently
    /// identifiable because raw bits show their own distinct effect rather than pairing up like
    /// FEC-protected bits do.
    const C7_CHIP_CONFIRMED: [usize; 7] = [71, 83, 95, 107, 119, 131, 143];

    fn assert_same_set(mut a: Vec<usize>, mut b: Vec<usize>, label: &str) {
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(
            a, b,
            "{label}: computed membership does not match chip-confirmed set"
        );
    }

    #[test]
    fn u4_computed_membership_matches_the_chip_confirmed_set_with_zero_mismatches() {
        assert_same_set(
            block_wire_members(Block::Hamming { index: 0 }),
            U4_CHIP_CONFIRMED.to_vec(),
            "u4",
        );
    }

    #[test]
    fn u5_computed_membership_matches_the_chip_confirmed_set_with_zero_mismatches() {
        assert_same_set(
            block_wire_members(Block::Hamming { index: 1 }),
            U5_CHIP_CONFIRMED.to_vec(),
            "u5",
        );
    }

    #[test]
    fn u6_computed_membership_matches_the_chip_confirmed_set_with_zero_mismatches() {
        assert_same_set(
            block_wire_members(Block::Hamming { index: 2 }),
            U6_CHIP_CONFIRMED.to_vec(),
            "u6",
        );
    }

    #[test]
    fn c7_computed_membership_matches_the_chip_confirmed_set_with_zero_mismatches() {
        assert_same_set(
            block_wire_members(Block::Raw),
            C7_CHIP_CONFIRMED.to_vec(),
            "c7",
        );
    }

    /// `g0`'s wire membership was directly predicted by the transform and its *boundaries* confirmed
    /// against the chip (in-block vs. straddling multi-bit-flip tests, section 21), though not walked
    /// bit-by-bit exhaustively the way the 3 Hamming blocks were.
    const G0_TRANSFORM_PREDICTED: [usize; 23] = [
        0, 1, 12, 13, 24, 25, 36, 37, 48, 49, 60, 61, 72, 73, 84, 85, 96, 97, 108, 109, 120, 121,
        132,
    ];

    #[test]
    fn g0_computed_membership_matches_the_transform_prediction_confirmed_via_chip_boundary_tests() {
        assert_same_set(
            block_wire_members(Block::Golay { index: 0 }),
            G0_TRANSFORM_PREDICTED.to_vec(),
            "g0",
        );
    }
}
