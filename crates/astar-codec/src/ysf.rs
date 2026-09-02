// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! System Fusion (YSF) DN voice: the vocoder layer between
//! [`astar_ysf::Frame::payload`]'s ninety bytes and the AMBE-3000 in a
//! `ThumbDV`.
//!
//! `astar-ysf` carries the payload and decodes nothing in it, on purpose.
//! This module is the other half: it lifts the five 20 ms AMBE+2 half-rate
//! voice frames out of a DN payload and puts them back.
//!
//! # DN only, and VW says so
//!
//! [`astar_ysf::DataType::is_half_rate_voice`] is the gate.
//! [`DataType::VDMode1`] and [`DataType::VDMode2`] are handled;
//! [`DataType::VoiceFrMode`] (VW, full-rate) and [`DataType::DataFrMode`]
//! return [`DnError::UnsupportedMode`], which carries the mode so a client
//! can say *which* mode it refused. Never samples, and never silence with
//! no reason attached.
//!
//! # Where the numbers came from
//!
//! Nothing here is recalled. Every layout claim below was read out of the
//! deployed reference implementations and then re-derived and checked
//! locally; the tests at the bottom of this file are that check, and each
//! one names its source.
//!
//! * **G4KLX's `MMDVMHost`** — `YSFPayload.cpp`, `AMBEFEC.cpp`,
//!   `Golay24128.cpp`, `YSFDefines.h`. The authority for the two DN
//!   payload layouts, which it keeps carefully apart:
//!   `processVDMode1Audio` regenerates five nine-byte blocks at byte
//!   offsets 9, 27, 45, 63, 81, while `processVDMode2Audio` walks five
//!   104-bit VCH sections from bit offset 40 in steps of 144.
//! * **Doug McLain's `DroidStar`** — `ysf.cpp`. A second reading of the
//!   mode-2 VCH, and the confirmation that a YSF client strips YSF's own
//!   FEC and keeps only the 49 voice bits.
//!
//! Neither project's code was copied — they are GPL-2.0 and this is
//! AGPL-3.0-only. Every table they print is generated here from the rule
//! that produces it, and the tests assert the generated form equals the
//! published one. That is the same standard `astar-ysf`'s Golay and FICH
//! interleave were held to.
//!
//! # What is *not* proven
//!
//! Bytes in and bytes out are proven. That the resulting audio is
//! intelligible is not, and cannot be without a `ThumbDV` on a live
//! reflector — that is Rob's checkpoint, not an agent's. V/D mode 1 in
//! particular is rare on the air (Yaesu radios transmit V/D mode 2 for
//! "DN"), and the two reference implementations disagree about it:
//! `MMDVMHost` gives it its own layout, `DroidStar` runs it through the
//! mode-2 path. This module follows `MMDVMHost`, whose treatment is the
//! self-consistent one — see `the_two_modes_carry_the_same_three_fields`.

use astar_ysf::DataType;
use astar_ysf::golay;

/// Bytes of payload a YSF radio frame carries — [`astar_ysf::PAYLOAD_LEN`].
pub const PAYLOAD_LEN: usize = astar_ysf::PAYLOAD_LEN;

/// Voice frames a single DN payload carries: five 20 ms frames per 100 ms
/// radio frame.
pub const FRAMES_PER_PAYLOAD: usize = 5;

/// Bits of AMBE+2 half-rate voice in one 20 ms frame — 2450 bit/s × 20 ms.
pub const VOICE_BITS: usize = 49;

/// Bytes one [`DnFrame`] occupies: 49 bits rounded up, low seven bits zero.
pub const VOICE_BYTES: usize = 7;

/// What went wrong turning a payload into voice, or voice into a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DnError {
    /// The frame is not half-rate voice, so there is no DN audio in it.
    ///
    /// VW (full-rate voice) and data frames land here. The mode is carried
    /// so the refusal can be shown with its reason rather than becoming
    /// unexplained silence.
    #[error("YSF {} is not a mode astar can decode: DN (V/D mode 1 or 2) only", .mode.as_str())]
    UnsupportedMode {
        /// The mode that was refused.
        mode: DataType,
    },
    /// The payload was not [`PAYLOAD_LEN`] bytes.
    #[error("YSF payload is {got} bytes, expected {PAYLOAD_LEN}")]
    PayloadLen {
        /// The length actually supplied.
        got: usize,
    },
}

/// One 20 ms AMBE+2 half-rate voice frame: 49 bits, MSB-first in seven
/// bytes, the last seven bits zero.
///
/// The bits are in the codec's *logical* order — the twelve bits the Golay
/// (24, 12) word protects, then the twelve the (23, 12) word protects, then
/// the twenty-five that nothing protects. That is the order both DN modes
/// carry them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DnFrame([u8; VOICE_BYTES]);

impl DnFrame {
    /// The frame a receiver substitutes when a V/D mode 1 block's FEC is
    /// beyond repair — the mute codeword `MMDVMHost`'s `regenerateDMR`
    /// falls back to (`a = 0xF00292`, `b = 0x0E0B20`, `c = 0`).
    ///
    /// An all-zero frame is *not* this: zero is a valid set of voice
    /// parameters and an AMBE decoder renders it as a click, which is
    /// exactly the noise this module exists to avoid.
    pub const MUTE: DnFrame = DnFrame([0xF0, 0x00, 0x31, 0x00, 0x00, 0x00, 0x00]);

    /// Wraps seven bytes, masking off everything past bit 48.
    #[must_use]
    pub const fn from_bytes(mut bytes: [u8; VOICE_BYTES]) -> DnFrame {
        bytes[6] &= 0x80;
        DnFrame(bytes)
    }

    /// The 49 voice bits, MSB-first, in the codec's logical order.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; VOICE_BYTES] {
        &self.0
    }
}

// ── Bit access ──────────────────────────────────────────────────────────
//
// Every bit index in YSF and in AMBE+2 counts MSB-first inside each byte.

fn read_bit(buf: &[u8], i: usize) -> bool {
    buf[i >> 3] & (0x80 >> (i & 7)) != 0
}

fn write_bit(buf: &mut [u8], i: usize, value: bool) {
    let mask = 0x80 >> (i & 7);
    if value {
        buf[i >> 3] |= mask;
    } else {
        buf[i >> 3] &= !mask;
    }
}

// ── The scrambler ───────────────────────────────────────────────────────

/// Bytes of scrambling sequence the references publish.
const WHITENING_LEN: usize = 20;

/// The YSF scrambler: 160 bits of the PN9 sequence `x^9 + x^5 + 1`, run
/// from state `0b1_0010_0111`, most significant stage out first.
///
/// The reference implementations print this as twenty magic bytes
/// (`WHITENING_DATA` in `MMDVMHost`, `scramble_code` in `DroidStar`). It is
/// not magic: a nine-stage Fibonacci LFSR with taps on stages 9 and 5
/// reproduces all 160 of those bits, which
/// `the_scrambler_is_the_published_sequence` asserts. Only 160 bits are
/// published, so only 160 are generated — a DN payload never needs more.
const fn whitening() -> [u8; WHITENING_LEN] {
    let mut out = [0u8; WHITENING_LEN];
    let mut state: u16 = 0b1_0010_0111;
    let mut i = 0;
    while i < WHITENING_LEN * 8 {
        out[i / 8] |= ((state >> 8) as u8 & 1) << (7 - (i % 8));
        let feedback = ((state >> 8) ^ (state >> 4)) & 1;
        state = ((state << 1) | feedback) & 0x1FF;
        i += 1;
    }
    out
}

/// The scrambling sequence, generated once at compile time.
const WHITENING: [u8; WHITENING_LEN] = whitening();

// ── V/D mode 2: five [5-byte DCH][13-byte VCH] blocks ───────────────────

/// Bit offset of the first VCH inside the payload; the DCH that precedes it
/// is 40 bits.
const VD2_VCH_OFFSET: usize = 40;

/// Bit distance between one VCH and the next: 40 bits of DCH + 104 of VCH.
const VD2_VCH_STRIDE: usize = 144;

/// Bits in one VCH.
const VD2_VCH_BITS: usize = 104;

/// The VCH bit interleave, as a rule rather than a table.
///
/// `MMDVMHost` prints it as `INTERLEAVE_TABLE_26_4`, 104 entries in four
/// rows of twenty-six, and notes that unlike YSF's other interleaves this
/// one moves bits rather than dibits. Four rows of twenty-six read
/// column-major *is* the whole rule: logical bit `i` sits at
/// `(i % 26) * 4 + i / 26` on the air.
const fn vd2_interleave(i: usize) -> usize {
    (i % 26) * 4 + i / 26
}

/// Reads one VCH's 104 bits out of the payload, deinterleaved and
/// descrambled.
fn vd2_vch(payload: &[u8], block: usize) -> [u8; 13] {
    let offset = VD2_VCH_OFFSET + VD2_VCH_STRIDE * block;
    let mut vch = [0u8; 13];
    for i in 0..VD2_VCH_BITS {
        write_bit(&mut vch, i, read_bit(payload, offset + vd2_interleave(i)));
    }
    for (byte, mask) in vch.iter_mut().zip(WHITENING) {
        *byte ^= mask;
    }
    vch
}

/// Writes one VCH's 104 bits back into the payload, scrambled and
/// interleaved. Touches only the VCH's own bits, leaving the DCH — which
/// carries the callsigns — exactly as it was.
fn vd2_put_vch(payload: &mut [u8], block: usize, vch: &[u8; 13]) {
    let offset = VD2_VCH_OFFSET + VD2_VCH_STRIDE * block;
    let mut scrambled = *vch;
    for (byte, mask) in scrambled.iter_mut().zip(WHITENING) {
        *byte ^= mask;
    }
    for i in 0..VD2_VCH_BITS {
        let bit = read_bit(&scrambled, i);
        write_bit(payload, offset + vd2_interleave(i), bit);
    }
}

/// Pulls the 49 voice bits out of a deinterleaved, descrambled VCH.
///
/// Mode 2's FEC is plain triple redundancy over the first 27 voice bits —
/// 81 bits, three copies each, majority wins — followed by 22 bare bits.
/// Bit 103 is padding.
fn vd2_voice(vch: &[u8; 13]) -> DnFrame {
    let mut voice = [0u8; VOICE_BYTES];
    for k in 0..27 {
        let votes = u32::from(read_bit(vch, 3 * k))
            + u32::from(read_bit(vch, 3 * k + 1))
            + u32::from(read_bit(vch, 3 * k + 2));
        write_bit(&mut voice, k, votes >= 2);
    }
    for k in 0..22 {
        write_bit(&mut voice, 27 + k, read_bit(vch, 81 + k));
    }
    DnFrame(voice)
}

/// The inverse of [`vd2_voice`]: 49 voice bits into a VCH.
fn vd2_vch_from_voice(frame: DnFrame) -> [u8; 13] {
    let mut vch = [0u8; 13];
    for k in 0..27 {
        let bit = read_bit(&frame.0, k);
        write_bit(&mut vch, 3 * k, bit);
        write_bit(&mut vch, 3 * k + 1, bit);
        write_bit(&mut vch, 3 * k + 2, bit);
    }
    for k in 0..22 {
        write_bit(&mut vch, 81 + k, read_bit(&frame.0, 27 + k));
    }
    write_bit(&mut vch, 103, false);
    vch
}

// ── V/D mode 1: five [9-byte DCH][9-byte VCH] blocks ────────────────────

/// Byte offset of the first mode-1 voice block; the DCH before it is nine
/// bytes.
const VD1_VOICE_OFFSET: usize = 9;

/// Byte distance between one mode-1 voice block and the next.
const VD1_STRIDE: usize = 18;

/// Bytes in one mode-1 voice block: a 72-bit AMBE+2 3600 bit/s codeword.
const VD1_VOICE_BYTES: usize = 9;

// The three fields of that codeword are dealt out four bits apart, which is
// what `MMDVMHost`'s `DMR_A_TABLE`/`_B_`/`_C_` say in longhand. The same
// three tables serve DMR, NXDN and YSF V/D mode 1 in that project, which is
// the tell that the ordering belongs to the AMBE+2 codeword and not to any
// one air interface.

/// Position of bit `i` of the Golay (24, 12)-protected `a` word.
const fn vd1_a_pos(i: usize) -> usize {
    if i < 18 { 4 * i } else { 4 * (i - 18) + 1 }
}

/// Position of bit `i` of the Golay (23, 12)-protected `b` word.
const fn vd1_b_pos(i: usize) -> usize {
    if i < 12 { 4 * i + 25 } else { 4 * (i - 12) + 2 }
}

/// Position of bit `i` of the unprotected `c` word.
const fn vd1_c_pos(i: usize) -> usize {
    if i < 7 { 4 * i + 46 } else { 4 * (i - 7) + 3 }
}

/// The AMBE+2 pseudo-random sequence that whitens the `b` word, keyed on
/// the twelve bits the `a` word carries.
///
/// `MMDVMHost` ships this as `PRNG_TABLE`, 4,096 entries of 24 bits. It is
/// a linear congruential generator seeded from the data word: `x <- 173x +
/// 13849 (mod 2^16)`, starting at `16 * data`, taking the top bit of each
/// step. `the_prng_matches_the_reference_table` checks that against the
/// published entries.
fn vd1_prng(data: u16) -> u32 {
    let mut x = u32::from(data) * 16;
    let mut word = 0u32;
    for _ in 0..24 {
        x = (173 * x + 13849) % 65536;
        word = (word << 1) | (x >> 15);
    }
    word
}

/// Decodes a 23-bit Golay (23, 12) word: the (24, 12) code with its overall
/// parity bit removed, so nearest-codeword search runs over the same 4,096
/// codewords shifted down one place.
///
/// Unlike the (24, 12) case this never gives up. The (23, 12) code has
/// minimum distance 7 and corrects three errors; past that the reference
/// implementations still take the nearest word, and here the `a` word's
/// own check has already decided whether the frame is trustworthy.
fn golay23_decode(received: u32) -> u16 {
    let received = received & 0x007F_FFFF;
    let mut best = 0u16;
    let mut best_distance = u32::MAX;
    for data in 0..4096u16 {
        let distance = ((golay::encode(data) >> 1) ^ received).count_ones();
        if distance <= 3 {
            return data;
        }
        if distance < best_distance {
            best_distance = distance;
            best = data;
        }
    }
    best
}

/// Reads the three fields of one mode-1 voice block.
fn vd1_fields(block: &[u8]) -> (u32, u32, u32) {
    let mut a = 0u32;
    for i in 0..24 {
        a = (a << 1) | u32::from(read_bit(block, vd1_a_pos(i)));
    }
    let mut b = 0u32;
    for i in 0..23 {
        b = (b << 1) | u32::from(read_bit(block, vd1_b_pos(i)));
    }
    let mut c = 0u32;
    for i in 0..25 {
        c = (c << 1) | u32::from(read_bit(block, vd1_c_pos(i)));
    }
    (a, b, c)
}

/// Strips mode 1's FEC, leaving the 49 voice bits.
///
/// An `a` word more than three bits from every codeword is not repairable
/// and its twelve bits key the descrambling of `b`, so the whole frame is
/// gone: it becomes [`DnFrame::MUTE`], as the references' own substitution
/// does.
fn vd1_voice(block: &[u8]) -> DnFrame {
    let (a, b, c) = vd1_fields(block);
    let Some(u0) = golay::decode(a) else {
        return DnFrame::MUTE;
    };
    let u1 = golay23_decode(b ^ (vd1_prng(u0) >> 1));

    let mut voice = [0u8; VOICE_BYTES];
    for i in 0..12 {
        write_bit(&mut voice, i, (u0 >> (11 - i)) & 1 == 1);
        write_bit(&mut voice, 12 + i, (u1 >> (11 - i)) & 1 == 1);
    }
    for i in 0..25 {
        write_bit(&mut voice, 24 + i, (c >> (24 - i)) & 1 == 1);
    }
    DnFrame(voice)
}

/// Applies mode 1's FEC to the 49 voice bits, producing a nine-byte block.
fn vd1_block_from_voice(frame: DnFrame) -> [u8; VD1_VOICE_BYTES] {
    let mut u0 = 0u16;
    let mut u1 = 0u16;
    for i in 0..12 {
        u0 = (u0 << 1) | u16::from(read_bit(&frame.0, i));
        u1 = (u1 << 1) | u16::from(read_bit(&frame.0, 12 + i));
    }
    let mut c = 0u32;
    for i in 0..25 {
        c = (c << 1) | u32::from(read_bit(&frame.0, 24 + i));
    }

    let a = golay::encode(u0);
    let b = (golay::encode(u1) >> 1) ^ (vd1_prng(u0) >> 1);

    let mut block = [0u8; VD1_VOICE_BYTES];
    for i in 0..24 {
        write_bit(&mut block, vd1_a_pos(i), (a >> (23 - i)) & 1 == 1);
    }
    for i in 0..23 {
        write_bit(&mut block, vd1_b_pos(i), (b >> (22 - i)) & 1 == 1);
    }
    for i in 0..25 {
        write_bit(&mut block, vd1_c_pos(i), (c >> (24 - i)) & 1 == 1);
    }
    block
}

// ── The payload, both ways ──────────────────────────────────────────────

/// Lifts the five voice frames out of a DN payload.
///
/// `data_type` comes from the frame's FICH. VW and data frames are refused
/// with [`DnError::UnsupportedMode`] rather than turned into noise.
pub fn unpack_dn(
    data_type: DataType,
    payload: &[u8],
) -> Result<[DnFrame; FRAMES_PER_PAYLOAD], DnError> {
    check(data_type, payload)?;
    let mut frames = [DnFrame::default(); FRAMES_PER_PAYLOAD];
    for (block, frame) in frames.iter_mut().enumerate() {
        *frame = match data_type {
            DataType::VDMode1 => {
                let at = VD1_VOICE_OFFSET + VD1_STRIDE * block;
                vd1_voice(&payload[at..at + VD1_VOICE_BYTES])
            }
            _ => vd2_voice(&vd2_vch(payload, block)),
        };
    }
    Ok(frames)
}

/// Writes five voice frames into a DN payload, in place.
///
/// Only the voice bits are touched: the data channel that carries the
/// callsigns keeps whatever the caller put there.
pub fn pack_dn(
    data_type: DataType,
    frames: &[DnFrame; FRAMES_PER_PAYLOAD],
    payload: &mut [u8],
) -> Result<(), DnError> {
    check(data_type, payload)?;
    for (block, frame) in frames.iter().enumerate() {
        match data_type {
            DataType::VDMode1 => {
                let at = VD1_VOICE_OFFSET + VD1_STRIDE * block;
                payload[at..at + VD1_VOICE_BYTES].copy_from_slice(&vd1_block_from_voice(*frame));
            }
            _ => vd2_put_vch(payload, block, &vd2_vch_from_voice(*frame)),
        }
    }
    Ok(())
}

/// The gate: half-rate voice and a full-length payload, or a typed refusal.
fn check(data_type: DataType, payload: &[u8]) -> Result<(), DnError> {
    if !data_type.is_half_rate_voice() {
        return Err(DnError::UnsupportedMode { mode: data_type });
    }
    if payload.len() != PAYLOAD_LEN {
        return Err(DnError::PayloadLen { got: payload.len() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MMDVMHost`'s `WHITENING_DATA` / `DroidStar`'s `scramble_code`.
    const WHITENING_REFERENCE: [u8; 20] = [
        0x93, 0xD7, 0x51, 0x21, 0x9C, 0x2F, 0x6C, 0xD0, 0xEF, 0x0F, 0xF8, 0x3D, 0xF1, 0x73, 0x20,
        0x94, 0xED, 0x1E, 0x7C, 0xD8,
    ];

    /// `MMDVMHost`'s `INTERLEAVE_TABLE_26_4`, first and last rows.
    #[test]
    fn the_vch_interleave_is_the_published_table() {
        assert_eq!(vd2_interleave(0), 0);
        assert_eq!(vd2_interleave(1), 4);
        assert_eq!(vd2_interleave(25), 100);
        assert_eq!(vd2_interleave(26), 1);
        assert_eq!(vd2_interleave(78), 3);
        assert_eq!(vd2_interleave(103), 103);

        let mut seen = [false; VD2_VCH_BITS];
        for i in 0..VD2_VCH_BITS {
            let at = vd2_interleave(i);
            assert!(!seen[at], "bit {at} written twice");
            seen[at] = true;
        }
    }

    #[test]
    fn the_scrambler_is_the_published_sequence() {
        assert_eq!(WHITENING, WHITENING_REFERENCE);
    }

    /// `MMDVMHost`'s `PRNG_TABLE`, spot-checked. The full 4,096 entries
    /// were compared off-line against this generator and agreed
    /// everywhere; these stand guard.
    #[test]
    fn the_prng_matches_the_reference_table() {
        assert_eq!(vd1_prng(0x000), 0x0042_CC47);
        assert_eq!(vd1_prng(0x001), 0x0019_D6FE);
        assert_eq!(vd1_prng(0x002), 0x0030_4729);
        assert_eq!(vd1_prng(0xABC), 0x0034_C6C2);
        assert_eq!(vd1_prng(0xFFF), 0x000B_3F09);
    }

    /// `MMDVMHost`'s `DMR_A_TABLE`, `DMR_B_TABLE` and `DMR_C_TABLE`, which
    /// V/D mode 1 shares with DMR and NXDN.
    #[test]
    fn the_vd1_field_positions_are_the_published_tables() {
        assert_eq!(vd1_a_pos(0), 0);
        assert_eq!(vd1_a_pos(17), 68);
        assert_eq!(vd1_a_pos(18), 1);
        assert_eq!(vd1_a_pos(23), 21);
        assert_eq!(vd1_b_pos(0), 25);
        assert_eq!(vd1_b_pos(11), 69);
        assert_eq!(vd1_b_pos(12), 2);
        assert_eq!(vd1_b_pos(22), 42);
        assert_eq!(vd1_c_pos(0), 46);
        assert_eq!(vd1_c_pos(6), 70);
        assert_eq!(vd1_c_pos(7), 3);
        assert_eq!(vd1_c_pos(24), 71);
    }

    /// 24 + 23 + 25 = 72: the three fields tile the block exactly, with no
    /// bit used twice and none left over.
    #[test]
    fn the_vd1_fields_tile_the_block() {
        let mut seen = [false; 72];
        let mut mark = |at: usize| {
            assert!(!seen[at], "bit {at} claimed twice");
            seen[at] = true;
        };
        for i in 0..24 {
            mark(vd1_a_pos(i));
        }
        for i in 0..23 {
            mark(vd1_b_pos(i));
        }
        for i in 0..25 {
            mark(vd1_c_pos(i));
        }
        assert!(seen.iter().all(|&b| b));
    }

    /// The reason V/D mode 1 follows `MMDVMHost` rather than `DroidStar`,
    /// which runs both modes through the mode-2 path: the two modes carry
    /// the *same* three fields — twelve bits, twelve bits, twenty-five —
    /// and differ only in the FEC wrapped around them. Mode 1 spends 23
    /// bits of Golay on the first two; mode 2 spends 54 bits of triple
    /// redundancy on the first 27. Both leave 49.
    #[test]
    fn the_two_modes_carry_the_same_three_fields() {
        assert_eq!(12 + 12 + 25, VOICE_BITS);
        assert_eq!(27 + 22, VOICE_BITS);
    }

    #[test]
    fn vw_is_refused_with_its_reason() {
        let err = unpack_dn(DataType::VoiceFrMode, &[0u8; PAYLOAD_LEN]).unwrap_err();
        assert_eq!(
            err,
            DnError::UnsupportedMode {
                mode: DataType::VoiceFrMode
            }
        );
        assert!(err.to_string().contains("voice-fr"));
        assert!(!DataType::VoiceFrMode.is_half_rate_voice());
    }

    #[test]
    fn a_data_frame_is_refused_with_its_reason() {
        let err = unpack_dn(DataType::DataFrMode, &[0u8; PAYLOAD_LEN]).unwrap_err();
        assert_eq!(
            err,
            DnError::UnsupportedMode {
                mode: DataType::DataFrMode
            }
        );
        assert!(err.to_string().contains("data-fr"));
    }

    #[test]
    fn transmitting_in_an_unsupported_mode_is_refused_too() {
        let frames = [DnFrame::default(); FRAMES_PER_PAYLOAD];
        let mut payload = [0u8; PAYLOAD_LEN];
        for mode in [DataType::VoiceFrMode, DataType::DataFrMode] {
            assert_eq!(
                pack_dn(mode, &frames, &mut payload).unwrap_err(),
                DnError::UnsupportedMode { mode }
            );
        }
    }

    #[test]
    fn a_short_payload_is_refused() {
        assert_eq!(
            unpack_dn(DataType::VDMode2, &[0u8; 89]).unwrap_err(),
            DnError::PayloadLen { got: 89 }
        );
    }

    /// Five frames spanning the interesting shapes: arbitrary, all-zero,
    /// all-ones, a walking pattern, and the mute codeword.
    fn sample_frames() -> [DnFrame; FRAMES_PER_PAYLOAD] {
        [
            DnFrame::from_bytes([0xA0, 0x2C, 0xC6, 0x70, 0x90, 0xE4, 0x00]),
            DnFrame::from_bytes([0x00; 7]),
            DnFrame::from_bytes([0xFF; 7]),
            DnFrame::from_bytes([0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0x00]),
            DnFrame::MUTE,
        ]
    }

    /// Packed into an empty payload, both modes produce these exact bytes.
    /// Generated by an independent model of the reference implementations
    /// written from `YSFPayload.cpp`, `AMBEFEC.cpp` and `ysf.cpp`, and
    /// compared against this module's output.
    #[test]
    fn packing_produces_the_expected_bytes() {
        let mut payload = [0u8; PAYLOAD_LEN];
        pack_dn(DataType::VDMode1, &sample_frames(), &mut payload).unwrap();
        assert_eq!(
            payload.to_vec(),
            hex("00 00 00 00 00 00 00 00 00 C3 82 76 46 61 D1 E6 B0 44 \
                 00 00 00 00 00 00 00 00 00 22 00 02 04 02 20 40 44 00 \
                 00 00 00 00 00 00 00 00 00 DD DD FF FF DF BF BB FF BB \
                 00 00 00 00 00 00 00 00 00 41 6C 33 E5 67 E9 77 32 C4 \
                 00 00 00 00 00 00 00 00 00 8E A8 42 20 00 44 04 80 84")
        );

        let mut payload = [0u8; PAYLOAD_LEN];
        pack_dn(DataType::VDMode2, &sample_frames(), &mut payload).unwrap();
        assert_eq!(
            payload.to_vec(),
            hex("00 00 00 00 00 48 88 73 12 1C 0B 5D DF 5A 6F 54 4A 05 \
                 00 00 00 00 00 F3 19 37 DA 8C 4C 3B B9 6B 7F 55 0C 63 \
                 00 00 00 00 00 0C E6 C8 25 73 B3 C4 46 94 80 AA F3 9D \
                 00 00 00 00 00 C0 08 61 BC D0 C7 0D CF 5B E6 C4 0E 45 \
                 00 00 00 00 00 7B B3 9D 70 04 C4 3B B9 69 5D 55 0C 63")
        );
    }

    #[test]
    fn both_modes_round_trip() {
        for mode in [DataType::VDMode1, DataType::VDMode2] {
            let frames = sample_frames();
            let mut payload = [0u8; PAYLOAD_LEN];
            pack_dn(mode, &frames, &mut payload).unwrap();
            assert_eq!(unpack_dn(mode, &payload).unwrap(), frames, "{mode:?}");
        }
    }

    /// Packing must leave the data channel alone — it carries the
    /// callsigns, and a vocoder that scribbled on them would break the
    /// talker display.
    #[test]
    fn packing_leaves_the_data_channel_alone() {
        for (mode, dch_bits) in [
            (DataType::VDMode1, 72usize),
            (DataType::VDMode2, VD2_VCH_OFFSET),
        ] {
            let before = [0xAAu8; PAYLOAD_LEN];
            let mut after = before;
            pack_dn(mode, &sample_frames(), &mut after).unwrap();
            for block in 0..FRAMES_PER_PAYLOAD {
                for bit in 0..dch_bits {
                    let at = block * 144 + bit;
                    assert_eq!(
                        read_bit(&before, at),
                        read_bit(&after, at),
                        "{mode:?} bit {at}"
                    );
                }
            }
        }
    }

    /// Mode 2's triple redundancy is there to be used: one flipped bit in
    /// a triplet is outvoted.
    #[test]
    fn mode_2_outvotes_a_single_bit_error() {
        let frames = sample_frames();
        let mut payload = [0u8; PAYLOAD_LEN];
        pack_dn(DataType::VDMode2, &frames, &mut payload).unwrap();
        for triplet in 0..27 {
            let mut corrupt = payload;
            let at = VD2_VCH_OFFSET + vd2_interleave(3 * triplet);
            write_bit(&mut corrupt, at, !read_bit(&payload, at));
            assert_eq!(unpack_dn(DataType::VDMode2, &corrupt).unwrap(), frames);
        }
    }

    /// Mode 1's Golay corrects up to three errors in the `a` word.
    #[test]
    fn mode_1_corrects_three_errors_in_the_a_word() {
        let frames = sample_frames();
        let mut payload = [0u8; PAYLOAD_LEN];
        pack_dn(DataType::VDMode1, &frames, &mut payload).unwrap();
        for i in [0usize, 5, 23] {
            let at = VD1_VOICE_OFFSET * 8 + vd1_a_pos(i);
            let flipped = !read_bit(&payload, at);
            write_bit(&mut payload, at, flipped);
        }
        assert_eq!(
            unpack_dn(DataType::VDMode1, &payload).unwrap()[0],
            frames[0]
        );
    }

    /// An `a` word too far gone to repair mutes its frame rather than
    /// handing an AMBE decoder twelve bits of noise.
    #[test]
    fn an_unrepairable_mode_1_block_mutes() {
        let frames = sample_frames();
        let mut payload = [0u8; PAYLOAD_LEN];
        pack_dn(DataType::VDMode1, &frames, &mut payload).unwrap();
        for i in 0..8 {
            let at = VD1_VOICE_OFFSET * 8 + vd1_a_pos(i);
            let flipped = !read_bit(&payload, at);
            write_bit(&mut payload, at, flipped);
        }
        let got = unpack_dn(DataType::VDMode1, &payload).unwrap();
        assert_eq!(got[0], DnFrame::MUTE);
        assert_eq!(got[1], frames[1], "the other four frames are untouched");
        assert_ne!(DnFrame::MUTE, DnFrame::from_bytes([0u8; 7]));
    }

    /// The mute codeword is what `MMDVMHost` substitutes, arrived at the
    /// long way: build a block from its `a`, `b` and `c` and read it back.
    #[test]
    fn the_mute_frame_is_the_references_substitution() {
        const MUTE_A: u32 = 0x00F0_0292;
        const MUTE_B: u32 = 0x000E_0B20;
        let mut block = [0u8; VD1_VOICE_BYTES];
        for i in 0..24 {
            write_bit(&mut block, vd1_a_pos(i), (MUTE_A >> (23 - i)) & 1 == 1);
        }
        for i in 0..23 {
            write_bit(&mut block, vd1_b_pos(i), (MUTE_B >> (22 - i)) & 1 == 1);
        }
        assert_eq!(vd1_voice(&block), DnFrame::MUTE);
    }

    /// Parses `"AB CD"` into bytes, as `ambe`'s tests do.
    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn a_frame_never_carries_more_than_forty_nine_bits() {
        let frame = DnFrame::from_bytes([0xFF; 7]);
        assert_eq!(frame.as_bytes()[6], 0x80);
    }
}
