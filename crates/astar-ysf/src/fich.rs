// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The FICH — Frame Information Channel — that heads every YSF frame.
//!
//! Four bytes of fields plus a two-byte CRC, wrapped in three layers of
//! protection because it is the part of the frame a receiver must get right
//! before it can do anything with the rest:
//!
//! 1. the six bytes are split into four 12-bit halves, each Golay (24, 12)
//!    encoded — 96 bits;
//! 2. those 96 bits plus a four-bit flush tail go through the rate-1/2
//!    convolutional coder — 200 bits;
//! 3. the 200 bits are interleaved across the frame's 25-byte FICH field so
//!    a burst of RF noise lands on separated bits.
//!
//! Decoding runs that backwards and then checks the CRC. Everything before
//! the CRC can repair damage; the CRC is what decides whether the repair
//! is believable.

use crate::conv;
use crate::crc;
use crate::golay;

/// Bytes of the encoded FICH field inside a frame.
pub const FICH_LEN: usize = 25;
/// The four field bytes plus the two CRC bytes, before any coding.
const RAW_LEN: usize = 6;
/// Golay blocks per FICH.
const BLOCKS: usize = 4;
/// Data bits into the convolutional coder: four Golay codewords.
const CODED_BITS: usize = BLOCKS * 24;

/// Where interleaved bit-pair `i` lands in the 200-bit FICH field.
///
/// The published tables spell out all one hundred entries; they are this
/// expression. Five columns of twenty, written down the columns and read
/// across the rows, which is what an interleaver is.
const fn interleave(i: usize) -> usize {
    (i / 5) * 2 + (i % 5) * 40
}

/// Frame Information — what kind of frame this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameInfo {
    /// Opens a transmission and carries the callsigns.
    Header,
    /// Carries voice and data.
    Communications,
    /// Closes a transmission.
    Terminator,
    /// Test frame.
    Test,
}

impl FrameInfo {
    #[must_use]
    const fn from_bits(bits: u8) -> FrameInfo {
        match bits & 0x03 {
            0 => FrameInfo::Header,
            1 => FrameInfo::Communications,
            2 => FrameInfo::Terminator,
            _ => FrameInfo::Test,
        }
    }

    #[must_use]
    const fn bits(self) -> u8 {
        match self {
            FrameInfo::Header => 0,
            FrameInfo::Communications => 1,
            FrameInfo::Terminator => 2,
            FrameInfo::Test => 3,
        }
    }

    /// A stable lowercase name. Crosses the C ABI in the YSF link state, so
    /// treat these strings as fixed even if a variant is renamed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FrameInfo::Header => "header",
            FrameInfo::Communications => "communications",
            FrameInfo::Terminator => "terminator",
            FrameInfo::Test => "test",
        }
    }
}

/// Data Type — what the frame's 90-byte payload holds.
///
/// This is the field that decides whether astar can make a sound. `VDMode1`
/// and `VDMode2` are the two half-rate "DN" voice-and-data layouts, which
/// is what almost all traffic is; `VoiceFrMode` is full-rate "VW"; and
/// `DataFrMode` is not voice at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// Voice/Data mode 1 — half-rate voice (DN).
    VDMode1,
    /// Data, full rate.
    DataFrMode,
    /// Voice/Data mode 2 — half-rate voice (DN), the common one.
    VDMode2,
    /// Voice, full rate (VW).
    VoiceFrMode,
}

impl DataType {
    #[must_use]
    const fn from_bits(bits: u8) -> DataType {
        match bits & 0x03 {
            0 => DataType::VDMode1,
            1 => DataType::DataFrMode,
            2 => DataType::VDMode2,
            _ => DataType::VoiceFrMode,
        }
    }

    #[must_use]
    const fn bits(self) -> u8 {
        match self {
            DataType::VDMode1 => 0,
            DataType::DataFrMode => 1,
            DataType::VDMode2 => 2,
            DataType::VoiceFrMode => 3,
        }
    }

    /// A stable lowercase name; an ABI string, as [`FrameInfo::as_str`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            DataType::VDMode1 => "vd-mode-1",
            DataType::DataFrMode => "data-fr",
            DataType::VDMode2 => "vd-mode-2",
            DataType::VoiceFrMode => "voice-fr",
        }
    }

    /// Whether this payload carries voice astar could decode.
    ///
    /// Both half-rate modes do. Full-rate voice does too, in principle —
    /// but see [`crate`]'s note on VW: saying "no" here and telling the
    /// operator why beats emitting noise.
    #[must_use]
    pub const fn is_half_rate_voice(self) -> bool {
        matches!(self, DataType::VDMode1 | DataType::VDMode2)
    }
}

/// The decoded contents of a FICH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fich {
    /// What kind of frame this is.
    pub frame_info: FrameInfo,
    /// What the payload holds.
    pub data_type: DataType,
    /// Call mode — group, individual, and so on. Kept raw: astar has no
    /// behaviour that turns on it, and inventing an enum for a field
    /// nothing reads is how enums come to disagree with the wire.
    pub call_mode: u8,
    /// Block number and total, for multi-block headers.
    pub block_number: u8,
    /// Block total.
    pub block_total: u8,
    /// Frame number within the superframe, and how many there are.
    pub frame_number: u8,
    /// Frame total.
    pub frame_total: u8,
    /// Message routing / busy indication.
    pub message_route: u8,
    /// Set when the frame came from a network gateway rather than over RF.
    pub voip: bool,
    /// The "dev" bit.
    pub dev: bool,
    /// DG-ID — the room within a YCS reflector. Zero on plain YSF.
    pub dg_id: u8,
}

impl Default for Fich {
    fn default() -> Fich {
        Fich {
            frame_info: FrameInfo::Communications,
            data_type: DataType::VDMode2,
            call_mode: 0,
            block_number: 0,
            block_total: 0,
            frame_number: 0,
            frame_total: 0,
            message_route: 0,
            voip: false,
            dev: false,
            dg_id: 0,
        }
    }
}

/// Why a FICH could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FichError {
    /// The slice handed in was not [`FICH_LEN`] bytes.
    WrongLength,
    /// A Golay block was too damaged to correct.
    Uncorrectable,
    /// Everything decoded, but the CRC says the result is not what was sent.
    BadCrc,
}

impl Fich {
    /// Packs the fields into the six raw bytes, CRC included.
    fn to_raw(self) -> [u8; RAW_LEN] {
        let mut raw = [0u8; RAW_LEN];
        raw[0] = (self.frame_info.bits() << 6)
            | ((self.call_mode & 0x03) << 2)
            | (self.block_number & 0x03);
        raw[1] = ((self.block_total & 0x03) << 6)
            | ((self.frame_number & 0x07) << 3)
            | (self.frame_total & 0x07);
        raw[2] = (u8::from(self.dev) << 6)
            | ((self.message_route & 0x03) << 3)
            | (u8::from(self.voip) << 2)
            | self.data_type.bits();
        raw[3] = self.dg_id & 0x7F;
        crc::append(&mut raw);
        raw
    }

    /// Unpacks six raw bytes, whose CRC has already been checked.
    fn from_raw(raw: [u8; RAW_LEN]) -> Fich {
        Fich {
            frame_info: FrameInfo::from_bits(raw[0] >> 6),
            call_mode: (raw[0] >> 2) & 0x03,
            block_number: raw[0] & 0x03,
            block_total: (raw[1] >> 6) & 0x03,
            frame_number: (raw[1] >> 3) & 0x07,
            frame_total: raw[1] & 0x07,
            dev: raw[2] & 0x40 != 0,
            message_route: (raw[2] >> 3) & 0x03,
            voip: raw[2] & 0x04 != 0,
            data_type: DataType::from_bits(raw[2]),
            dg_id: raw[3] & 0x7F,
        }
    }

    /// Encodes this FICH into the 25-byte field of a frame.
    ///
    /// # Panics
    /// If `field` is not [`FICH_LEN`] bytes.
    pub fn encode(self, field: &mut [u8]) {
        assert_eq!(field.len(), FICH_LEN, "FICH field is 25 bytes");
        let raw = self.to_raw();

        // Four 12-bit halves, Golay encoded, packed MSB-first.
        let mut coded = [0u8; CODED_BITS / 8 + 1];
        for block in 0..BLOCKS {
            let half = if block % 2 == 0 {
                (u16::from(raw[block / 2 * 3]) << 4) | u16::from(raw[block / 2 * 3 + 1] >> 4)
            } else {
                (u16::from(raw[block / 2 * 3 + 1] & 0x0F) << 8) | u16::from(raw[block / 2 * 3 + 2])
            };
            let word = golay::encode(half);
            for b in 0..24 {
                conv::set_bit(&mut coded, block * 24 + b, word >> (23 - b) & 1 == 1);
            }
        }

        // Convolutional coding with a four-bit flush tail, then interleave.
        let mut convolved = [0u8; FICH_LEN];
        conv::encode(&coded, &mut convolved, 100);
        field.fill(0);
        for i in 0..100 {
            let n = interleave(i);
            conv::set_bit(field, n, conv::bit(&convolved, i * 2));
            conv::set_bit(field, n + 1, conv::bit(&convolved, i * 2 + 1));
        }
    }

    /// Decodes a FICH from the 25-byte field of a frame.
    pub fn decode(field: &[u8]) -> Result<Fich, FichError> {
        if field.len() != FICH_LEN {
            return Err(FichError::WrongLength);
        }

        // Deinterleave back into convolutional-coder order.
        let mut convolved = [0u8; FICH_LEN];
        for i in 0..100 {
            let n = interleave(i);
            conv::set_bit(&mut convolved, i * 2, conv::bit(field, n));
            conv::set_bit(&mut convolved, i * 2 + 1, conv::bit(field, n + 1));
        }

        let bits = conv::decode(&convolved, 100);
        let mut coded = [0u8; CODED_BITS / 8];
        for (i, value) in bits.iter().take(CODED_BITS).enumerate() {
            conv::set_bit(&mut coded, i, *value);
        }

        let mut raw = [0u8; RAW_LEN];
        for block in 0..BLOCKS {
            let mut word = 0u32;
            for b in 0..24 {
                word = (word << 1) | u32::from(conv::bit(&coded, block * 24 + b));
            }
            let half = golay::decode(word).ok_or(FichError::Uncorrectable)?;
            if block % 2 == 0 {
                raw[block / 2 * 3] = u8::try_from(half >> 4).unwrap_or(0);
                raw[block / 2 * 3 + 1] = u8::try_from((half & 0x0F) << 4).unwrap_or(0);
            } else {
                raw[block / 2 * 3 + 1] |= u8::try_from(half >> 8).unwrap_or(0);
                raw[block / 2 * 3 + 2] = u8::try_from(half & 0xFF).unwrap_or(0);
            }
        }

        if !crc::check(&raw) {
            return Err(FichError::BadCrc);
        }
        Ok(Fich::from_raw(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interleave_expression_matches_the_published_table() {
        // The first two rows and the last entry of the hundred-entry table
        // every implementation writes out longhand.
        assert_eq!(
            (0..10).map(interleave).collect::<Vec<_>>(),
            vec![0, 40, 80, 120, 160, 2, 42, 82, 122, 162]
        );
        assert_eq!(interleave(99), 198);
    }

    #[test]
    fn the_interleave_covers_every_bit_pair_exactly_once() {
        let mut seen = [false; 200];
        for i in 0..100 {
            let n = interleave(i);
            assert!(!seen[n] && !seen[n + 1], "bit pair {n} reused");
            seen[n] = true;
            seen[n + 1] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    fn sample() -> Fich {
        Fich {
            frame_info: FrameInfo::Communications,
            data_type: DataType::VDMode2,
            call_mode: 2,
            block_number: 1,
            block_total: 3,
            frame_number: 5,
            frame_total: 6,
            message_route: 1,
            voip: true,
            dev: false,
            dg_id: 0x42,
        }
    }

    #[test]
    fn every_field_survives_a_round_trip() {
        let mut field = [0u8; FICH_LEN];
        sample().encode(&mut field);
        assert_eq!(Fich::decode(&field), Ok(sample()));
    }

    #[test]
    fn every_frame_info_and_data_type_survives() {
        for frame_info in [
            FrameInfo::Header,
            FrameInfo::Communications,
            FrameInfo::Terminator,
            FrameInfo::Test,
        ] {
            for data_type in [
                DataType::VDMode1,
                DataType::DataFrMode,
                DataType::VDMode2,
                DataType::VoiceFrMode,
            ] {
                let fich = Fich {
                    frame_info,
                    data_type,
                    ..Fich::default()
                };
                let mut field = [0u8; FICH_LEN];
                fich.encode(&mut field);
                assert_eq!(Fich::decode(&field), Ok(fich));
            }
        }
    }

    #[test]
    fn a_dg_id_survives_the_full_seven_bit_range() {
        for dg_id in 0..=127u8 {
            let fich = Fich {
                dg_id,
                ..Fich::default()
            };
            let mut field = [0u8; FICH_LEN];
            fich.encode(&mut field);
            assert_eq!(Fich::decode(&field).map(|f| f.dg_id), Ok(dg_id));
        }
    }

    #[test]
    fn a_burst_of_channel_errors_is_repaired() {
        // The whole point of the three coding layers: the interleave spreads
        // a burst, the convolutional coder eats the spread bits, and the
        // Golay blocks mop up.
        let mut field = [0u8; FICH_LEN];
        sample().encode(&mut field);
        for start in 0..190 {
            let mut damaged = field;
            for b in start..start + 4 {
                let flipped = !conv::bit(&damaged, b);
                conv::set_bit(&mut damaged, b, flipped);
            }
            assert_eq!(Fich::decode(&damaged), Ok(sample()), "burst at {start}");
        }
    }

    #[test]
    fn a_field_of_noise_is_refused_rather_than_believed() {
        // Not "always" — random bits do occasionally Golay-decode and pass a
        // 16-bit CRC — but a decoder that accepted noise routinely would let
        // one through per frame, and this pins that it does not.
        let mut accepted = 0;
        for seed in 0u32..64 {
            let mut field = [0u8; FICH_LEN];
            let mut x = seed.wrapping_mul(0x9E37_79B9) | 1;
            for byte in &mut field {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *byte = u8::try_from(x & 0xFF).unwrap_or(0);
            }
            if Fich::decode(&field).is_ok() {
                accepted += 1;
            }
        }
        assert!(accepted <= 1, "{accepted} of 64 noise fields were believed");
    }

    #[test]
    fn a_wrong_length_field_is_an_error_not_a_panic() {
        assert_eq!(Fich::decode(&[0u8; 24]), Err(FichError::WrongLength));
        assert_eq!(Fich::decode(&[]), Err(FichError::WrongLength));
    }
}
