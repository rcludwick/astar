// SPDX-License-Identifier: LGPL-3.0-or-later
//! The real DVSI chip's DTMF encoding within RATET(27) full-rate frames, determined by feeding all
//! 16 real ITU-T Q.23 DTMF digit tone pairs to the live chip and decoding the response through
//! [`crate::ambe::dvsi_p25fec::fec::decode_block`] -- see `AMBE_CHIP_VALIDATION_FINDINGS.md` section 25 for
//! the full experimental record.
//!
//! When the chip's own tone-classification logic recognizes DTMF content (`ECMODE_IN`'s
//! `TD_ENABLE`, on by default, need not be forced), it reuses the *same* FEC/interleave format
//! every other RATET(27) frame uses, but almost entirely zeroed except two fields that cleanly and
//! linearly encode the digit's row and column tone frequencies:
//!
//! - `g0` (the first Golay(23,12) block) encodes the **row** frequency (697/770/852/941 Hz) as
//!   `4032 + row_index`, `row_index` 0..4 -- confirmed identical across all 4 digits sharing each
//!   row, in all 16 captured frames, with zero exceptions.
//! - `u4` (the first Hamming(15,11) block) encodes the **column** frequency (1209/1336/1477/1633
//!   Hz) as `80 + 128 * column_index`, `column_index` 0..4 -- likewise confirmed identical across
//!   all 4 digits sharing each column, zero exceptions.
//! - Every other block (`g1`, `g2`, `g3`, `u5`, `u6`, `c7`) reads a fixed constant
//!   (`g1`=2944, everything else 0) regardless of digit.
//!
//! This module implements only the row/column encode/decode this data actually establishes --
//! it does not claim to know why these specific constants were chosen, nor whether they hold for
//! any DTMF-adjacent classification (KNOX tones, call-progress tones) the manual also mentions but
//! this investigation has not tested.

/// The 4 DTMF row frequencies, ITU-T Q.23, index order matching [`row_index_from_g0`]/
/// [`g0_from_row_index`].
pub const DTMF_ROW_FREQUENCIES_HZ: [f64; 4] = [697.0, 770.0, 852.0, 941.0];
/// The 4 DTMF column frequencies, ITU-T Q.23, index order matching [`column_index_from_u4`]/
/// [`u4_from_column_index`].
pub const DTMF_COLUMN_FREQUENCIES_HZ: [f64; 4] = [1209.0, 1336.0, 1477.0, 1633.0];

const G0_ROW_BASE: u16 = 4032;
const U4_COLUMN_BASE: u16 = 80;
const U4_COLUMN_STEP: u16 = 128;

/// Computes the `g0` Golay-block data value the chip emits for a given DTMF row index (0..4,
/// indexing [`DTMF_ROW_FREQUENCIES_HZ`]).
pub const fn g0_from_row_index(row_index: u8) -> u16 {
    G0_ROW_BASE + row_index as u16
}

/// The inverse of [`g0_from_row_index`]: recovers the row index from a decoded `g0` value, or
/// `None` if it doesn't fall in the confirmed DTMF row range.
pub fn row_index_from_g0(g0: u16) -> Option<u8> {
    let offset = g0.checked_sub(G0_ROW_BASE)?;
    (offset < DTMF_ROW_FREQUENCIES_HZ.len() as u16).then_some(offset as u8)
}

/// Computes the `u4` Hamming-block data value the chip emits for a given DTMF column index (0..4,
/// indexing [`DTMF_COLUMN_FREQUENCIES_HZ`]).
pub const fn u4_from_column_index(column_index: u8) -> u16 {
    U4_COLUMN_BASE + U4_COLUMN_STEP * column_index as u16
}

/// The inverse of [`u4_from_column_index`]: recovers the column index from a decoded `u4` value,
/// or `None` if it doesn't fall in the confirmed DTMF column range.
pub fn column_index_from_u4(u4: u16) -> Option<u8> {
    let offset = u4.checked_sub(U4_COLUMN_BASE)?;
    if offset % U4_COLUMN_STEP != 0 {
        return None;
    }
    let index = offset / U4_COLUMN_STEP;
    (index < DTMF_COLUMN_FREQUENCIES_HZ.len() as u16).then_some(index as u8)
}

/// Recovers `(row_index, column_index)` from a decoded frame's `g0`/`u4` values, or `None` if
/// either doesn't fall in the confirmed DTMF range (i.e. this frame is very likely not DTMF
/// content). Does not itself decode a wire frame -- callers pass in `g0`/`u4` already decoded via
/// [`crate::ambe::dvsi_p25fec::fec::decode_block`].
pub fn decode_dtmf_digit(g0: u16, u4: u16) -> Option<(u8, u8)> {
    Some((row_index_from_g0(g0)?, column_index_from_u4(u4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All 16 real chip-captured `(g0, u4)` pairs from `real_dtmf_sweep.tsv`, in row-major digit
    /// order (row 0..4, column 0..4) -- ground truth this module's formulas were derived from and
    /// must reproduce exactly.
    const CONFIRMED_DTMF_G0_U4: [[(u16, u16); 4]; 4] = [
        [(4032, 80), (4032, 208), (4032, 336), (4032, 464)], // row 697Hz: digits 1,2,3,A
        [(4033, 80), (4033, 208), (4033, 336), (4033, 464)], // row 770Hz: digits 4,5,6,B
        [(4034, 80), (4034, 208), (4034, 336), (4034, 464)], // row 852Hz: digits 7,8,9,C
        [(4035, 80), (4035, 208), (4035, 336), (4035, 464)], // row 941Hz: digits *,0,#,D
    ];

    #[test]
    fn g0_from_row_index_matches_every_confirmed_chip_capture() {
        for (row_index, row) in CONFIRMED_DTMF_G0_U4.iter().enumerate() {
            for &(g0, _u4) in row {
                assert_eq!(g0_from_row_index(row_index as u8), g0);
            }
        }
    }

    #[test]
    fn u4_from_column_index_matches_every_confirmed_chip_capture() {
        for row in &CONFIRMED_DTMF_G0_U4 {
            for (column_index, &(_g0, u4)) in row.iter().enumerate() {
                assert_eq!(u4_from_column_index(column_index as u8), u4);
            }
        }
    }

    #[test]
    fn decode_dtmf_digit_round_trips_every_confirmed_chip_capture() {
        for (row_index, row) in CONFIRMED_DTMF_G0_U4.iter().enumerate() {
            for (column_index, &(g0, u4)) in row.iter().enumerate() {
                assert_eq!(
                    decode_dtmf_digit(g0, u4),
                    Some((row_index as u8, column_index as u8))
                );
            }
        }
    }

    #[test]
    fn decode_dtmf_digit_rejects_values_outside_the_confirmed_range() {
        assert_eq!(
            decode_dtmf_digit(0, 0),
            None,
            "g0=0 is nowhere near the DTMF row base"
        );
        assert_eq!(
            decode_dtmf_digit(4032, 81),
            None,
            "u4=81 is not a multiple-of-128 offset"
        );
        assert_eq!(
            decode_dtmf_digit(4036, 80),
            None,
            "row index 4 is out of the confirmed 0..4 range"
        );
        assert_eq!(
            decode_dtmf_digit(4032, 80 + 128 * 4),
            None,
            "column index 4 is out of range"
        );
    }

    #[test]
    fn row_and_column_index_round_trip_through_their_own_encode_decode_pair() {
        for row_index in 0u8..4 {
            assert_eq!(
                row_index_from_g0(g0_from_row_index(row_index)),
                Some(row_index)
            );
        }
        for column_index in 0u8..4 {
            assert_eq!(
                column_index_from_u4(u4_from_column_index(column_index)),
                Some(column_index)
            );
        }
    }
}
