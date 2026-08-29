// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The extended binary Golay code (24, 12, 8), which protects the four
//! 12-bit halves of a YSF FICH.
//!
//! Twelve data bits in, twenty-four out; it corrects any three bit errors
//! and detects four. The generator is the standard
//! `x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1` (`0xC75`): the systematic
//! (23, 12) codeword is the data shifted up eleven places plus that
//! remainder, and the twenty-fourth bit is overall even parity.
//!
//! Written out algebraically rather than as the 4,096-entry lookup table
//! the C implementations ship. The table is the same numbers — this
//! module's tests check several of its entries — but eight lines of long
//! division say *why* they are those numbers, and a table says nothing at
//! all.
//!
//! Decoding is nearest-codeword search over all 4,096 codewords, against a
//! table built once on first use. That is the definition of the decoder
//! rather than a clever route to it, and the arithmetic is unambitious:
//! four Golay blocks per FICH, one FICH per 100 ms frame, so a fully loaded
//! link costs about 160,000 XOR-and-count operations a second. Syndrome
//! decoding would be faster and harder to read, and nothing here is waiting
//! on it.

use std::sync::OnceLock;

/// Generator polynomial of the (23, 12) cyclic Golay code, low bit first:
/// `x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1`.
const GENERATOR: u32 = 0xC75;

/// Encodes twelve data bits (the low bits of `data`) into a 24-bit codeword.
///
/// The data lands in bits 23..12 of the result, the eleven parity bits in
/// 12..1, and overall even parity in bit 0.
#[must_use]
pub fn encode(data: u16) -> u32 {
    let data = u32::from(data) & 0xFFF;
    // Long division of data·x^11 by the generator; what is left is the
    // parity word.
    let mut rem = data << 11;
    for i in (11..23).rev() {
        if rem >> i & 1 == 1 {
            rem ^= GENERATOR << (i - 11);
        }
    }
    let word23 = (data << 11) | (rem & 0x7FF);
    (word23 << 1) | (word23.count_ones() & 1)
}

/// Decodes a 24-bit codeword back to its twelve data bits, correcting up to
/// three bit errors.
///
/// Returns `None` when the received word is further than three bits from
/// every codeword — four errors are detectable but not correctable, and
/// guessing at that distance is how a corrupted FICH becomes a plausible
/// wrong one.
#[must_use]
pub fn decode(received: u32) -> Option<u16> {
    let received = received & 0x00FF_FFFF;
    for (data, &codeword) in codewords().iter().enumerate() {
        // The code's minimum distance is 8, so a word within three of one
        // codeword is within three of no other: the first hit is the only
        // hit, and there is nothing to compare it against.
        if (codeword ^ received).count_ones() <= 3 {
            return u16::try_from(data).ok();
        }
    }
    None
}

/// Every codeword, indexed by its data word. Built once; 16 KiB.
fn codewords() -> &'static [u32; 4096] {
    static TABLE: OnceLock<[u32; 4096]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 4096];
        for (data, slot) in table.iter_mut().enumerate() {
            // The index cannot exceed 4,095, so this never truncates.
            *slot = encode(u16::try_from(data).unwrap_or(0));
        }
        table
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entries of the reference implementations' `ENCODING_TABLE_24128`,
    /// spot-checked against this algebraic form. The full 4,096-entry table
    /// was compared once off-line and agreed everywhere; these are the
    /// standing guard.
    #[test]
    fn matches_the_reference_encoding_table() {
        assert_eq!(encode(0x000), 0x00_0000);
        assert_eq!(encode(0x001), 0x00_18EB);
        assert_eq!(encode(0x002), 0x00_293E);
        assert_eq!(encode(0xABC), 0xAB_C23C);
        assert_eq!(encode(0xFFF), 0xFF_FFFF);
    }

    #[test]
    fn the_data_sits_in_the_top_twelve_bits() {
        for data in [0x000u16, 0x001, 0x555, 0xAAA, 0xFFF] {
            assert_eq!(encode(data) >> 12, u32::from(data));
        }
    }

    #[test]
    fn every_codeword_has_even_weight() {
        for data in 0u16..4096 {
            assert_eq!(encode(data).count_ones() % 2, 0, "data {data:#05x}");
        }
    }

    #[test]
    fn distinct_codewords_are_at_least_eight_apart() {
        // The full 4,096 × 4,096 comparison is a minute of work; the code is
        // linear, so the minimum distance is the minimum weight of a nonzero
        // codeword, which is 4,095 comparisons.
        for data in 1u16..4096 {
            assert!(encode(data).count_ones() >= 8, "data {data:#05x}");
        }
    }

    #[test]
    fn a_clean_codeword_decodes_to_itself() {
        for data in 0u16..4096 {
            assert_eq!(decode(encode(data)), Some(data));
        }
    }

    #[test]
    fn every_single_and_double_error_is_corrected() {
        for data in [0x000u16, 0x001, 0x37F, 0xABC, 0xFFF] {
            let clean = encode(data);
            for a in 0..24 {
                for b in 0..24 {
                    let corrupt = clean ^ (1 << a) ^ (1 << b);
                    assert_eq!(decode(corrupt), Some(data), "data {data:#05x}");
                }
            }
        }
    }

    #[test]
    fn three_errors_are_corrected() {
        // Three is the code's guarantee, so this is the boundary that
        // matters. Sampled rather than exhaustive: every triple across
        // every data word is 13,824 × 4,096 decodes for no more assurance
        // than the spread below gives.
        let data = 0xABCu16;
        let clean = encode(data);
        for a in 0..24 {
            let b = (a + 7) % 24;
            let c = (a + 15) % 24;
            let corrupt = clean ^ (1 << a) ^ (1 << b) ^ (1 << c);
            assert_eq!(corrupt.count_ones().abs_diff(clean.count_ones()) % 2, 1);
            assert_eq!(decode(corrupt), Some(data));
        }
    }

    #[test]
    fn four_errors_are_refused_rather_than_guessed_at() {
        // Four errors leave the word equidistant from two codewords. The
        // decoder must say so: a wrong FICH that looks right is worse than
        // a dropped frame.
        let clean = encode(0xABC);
        let corrupt = clean ^ 0b1111;
        assert_eq!(decode(corrupt), None);
        assert_eq!(decode(encode(0x000) ^ 0xF0), None);
    }
}
