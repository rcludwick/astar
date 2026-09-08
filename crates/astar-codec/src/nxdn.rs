// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! NXDN voice: the vocoder layer between a 33-byte network frame's two
//! 14-byte blocks and the AMBE-3000 in a `ThumbDV`.
//!
//! `astar-nxdn` carries the blocks and decodes nothing in them, on purpose
//! — the same split [`crate::ysf`] and `astar-ysf` already have. This
//! module is the other half: it lifts the four 20 ms AMBE+2 half-rate
//! voice frames out of a network frame and puts them back.
//!
//! # What the network frame is, and is not
//!
//! It is **already de-FEC'd**. The Golay, the PRNG scramble and the
//! interleave in `MMDVMHost/NXDNAudio.cpp` are the *on-air* RTCH coding;
//! `MMDVMHost` strips all of it — along with the frame sync — before a
//! frame goes on the `NXDNReflector` wire. So there is nothing to undo
//! here. Each 14-byte block is 112 bits holding **two 49-bit frames at bit
//! offsets 0 and 49**, 98 bits used and the last 14 zero:
//!
//! * `MMDVMHost/NXDNAudio.cpp: CNXDNAudio::decode(const unsigned char* in,
//!   unsigned char* out)` is exactly `decode(in + 0U, out, 0U); decode(in +
//!   9U, out, 49U);`.
//! * Inside one frame, `CNXDNAudio::decode(in, out, offset)` writes `a`
//!   (12 bits at `offset + 0`), `b` (12 bits at `offset + 12`) and `c` (25
//!   bits at `offset + 24`) — one contiguous 49-bit run, so lifting a
//!   frame out is a plain bit copy and not a field-by-field shuffle.
//! * `DroidStar nxdn.cpp: NXDN::process_udp` agrees from the other side:
//!   it reads frame 1 from datagram offset 15 aligned and frame 2 from
//!   offset 21 shifted left one bit, which is what bit 49 of a block looks
//!   like to a byte-aligned reader.
//!
//! # Why [`crate::ysf::DnFrame`] and not a new type
//!
//! Because the field order is identical. `DnFrame`'s doc says its 49 bits
//! are "the twelve bits the Golay (24, 12) word protects, then the twelve
//! the (23, 12) word protects, then the twenty-five that nothing
//! protects" — `a`, `b`, `c`, in that order, which is exactly what
//! `CNXDNAudio::decode` writes. `MMDVMHost/NXDNControl.cpp` corroborates
//! it by regenerating NXDN's on-air AMBE with `CAMBEFEC::regenerateYSFDN`,
//! the **YSF DN** function, and `DroidStar`'s `SerialAMBE::config_ambe`
//! sends one RATEP word for both `"YSF"` and `"NXDN"`. So
//! [`crate::ysf::channel_in_dn`] and [`crate::ysf::ratep_dn`] serve NXDN
//! unchanged, and `VocoderMode::YsfDn` is reused rather than duplicated —
//! the ruling is recorded in `docs/design/nxdn-wire.md`.
//!
//! # Where the numbers came from
//!
//! Nothing here is recalled. Every offset was read out of the deployed
//! reference implementations named above and cited at the line that uses
//! it, then re-derived locally; the tests at the bottom of this file are
//! that check. **No code was copied** — `MMDVMHost`, `NXDNClients` and
//! `DroidStar` are GPL-2.0/GPL-3.0 and this is AGPL-3.0-only, and the two
//! do not mix. The bit loops below are written from the field widths those
//! functions define, not transcribed from them.
//!
//! # What is *not* proven
//!
//! Bytes in and bytes out are proven. That the resulting audio is
//! intelligible is not, and cannot be without a `ThumbDV` on a live
//! reflector — Rob's checkpoint, not an agent's.

use astar_nxdn::frame::{BLOCK_LEN, BLOCKS, FrameKind, NetFrame};

use crate::ysf::{DnFrame, VOICE_BITS, VOICE_BYTES};

/// Frames of AMBE+2 half-rate voice one 33-byte network frame carries:
/// two per 14-byte block, four in all — 80 ms of audio.
pub const FRAMES_PER_FRAME: usize = BLOCKS * FRAMES_PER_BLOCK;

/// Frames of voice one 14-byte block carries.
/// `CNXDNAudio::decode(in, out)` is two `decode` calls and no more.
pub const FRAMES_PER_BLOCK: usize = 2;

/// Bit offset of the second frame inside a block. `CNXDNAudio::decode(in,
/// out)`: `decode(in + 9U, out, 49U)` — 49, not 56, so the second frame is
/// not byte-aligned.
const SECOND_FRAME_BIT: usize = VOICE_BITS;

/// Why a network frame yielded no voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NxdnVoiceError {
    /// The frame's blocks are FACCH1, not audio — a voice header
    /// (`VCALL`), a terminator (`TX_REL`), or a mid-transmission frame
    /// with both halves stolen.
    ///
    /// The message type is carried so a client can say *which* signalling
    /// it refused rather than going quiet for no stated reason.
    #[error("this NXDN frame carries signalling, not voice (message type {message_type:#04x})")]
    Signalling {
        /// Byte 0 of block 0 — the Layer-3 message type.
        message_type: u8,
    },
    /// A UDCH, an idle frame, or anything else this build does not read.
    #[error("this NXDN frame is not one astar decodes: LICH {lich:#04x}")]
    NotVoice {
        /// The raw LICH byte, so the refusal names what it saw.
        lich: u8,
    },
}

// ── Bit access ──────────────────────────────────────────────────────────
//
// Bit indices count MSB-first inside each byte, as everywhere in NXDN.

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

/// Lift the four 20 ms AMBE+2 frames out of one 33-byte network frame.
///
/// A frame with one half stolen for FACCH1 ([`FrameKind::HalfVoice`])
/// still yields four: the two the surviving block carries, and
/// [`DnFrame::MUTE`] for the two that were never sent. Substituting the
/// vocoder's own mute codeword is what a receiver does with a stolen half
/// — an all-zero frame is a valid set of voice parameters that decodes to
/// a click, which is the noise this module exists to avoid.
pub fn unpack_voice(
    frame: &[u8; astar_nxdn::FRAME_LEN],
) -> Result<[DnFrame; FRAMES_PER_FRAME], NxdnVoiceError> {
    let view = NetFrame::new(frame);
    let voice_blocks = match view.kind() {
        FrameKind::Voice => [true, true],
        FrameKind::HalfVoice { voice_block } => [voice_block == 0, voice_block == 1],
        FrameKind::Signalling { message_type } => {
            return Err(NxdnVoiceError::Signalling { message_type });
        }
        FrameKind::Other => {
            return Err(NxdnVoiceError::NotVoice {
                lich: view.lich().raw(),
            });
        }
    };

    let mut out = [DnFrame::MUTE; FRAMES_PER_FRAME];
    for (block, carries_voice) in voice_blocks.into_iter().enumerate() {
        if carries_voice {
            let pair = unpack_voice_block(&view.block(block));
            out[block * FRAMES_PER_BLOCK..(block + 1) * FRAMES_PER_BLOCK].copy_from_slice(&pair);
        }
    }
    Ok(out)
}

/// The inverse of one 14-byte block: two 49-bit frames at bit offsets 0
/// and 49, the remaining 14 bits zero.
///
/// Public because the session tests build frames with it; the full-frame
/// packer is Task 9's.
#[must_use]
pub fn pack_voice_block(a: DnFrame, b: DnFrame) -> [u8; BLOCK_LEN] {
    let mut block = [0u8; BLOCK_LEN];
    put_frame(&mut block, 0, a);
    put_frame(&mut block, SECOND_FRAME_BIT, b);
    block
}

/// The inverse of [`pack_voice_block`].
#[must_use]
pub fn unpack_voice_block(block: &[u8; BLOCK_LEN]) -> [DnFrame; FRAMES_PER_BLOCK] {
    [take_frame(block, 0), take_frame(block, SECOND_FRAME_BIT)]
}

/// Copies 49 bits out of a block at `offset` — `a` (12), `b` (12) and `c`
/// (25) are contiguous in both, so the three fields are one run.
fn take_frame(block: &[u8; BLOCK_LEN], offset: usize) -> DnFrame {
    let mut voice = [0u8; VOICE_BYTES];
    for i in 0..VOICE_BITS {
        write_bit(&mut voice, i, read_bit(block, offset + i));
    }
    DnFrame::from_bytes(voice)
}

/// The inverse of [`take_frame`].
fn put_frame(block: &mut [u8; BLOCK_LEN], offset: usize, frame: DnFrame) {
    let voice = *frame.as_bytes();
    for i in 0..VOICE_BITS {
        write_bit(block, offset + i, read_bit(&voice, i));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ysf::DnFrame;
    use astar_nxdn::frame::{BLOCK_LEN, RFCT_RDCH, STEAL_NONE, USC_SACCH_SS};

    fn distinct() -> [DnFrame; FRAMES_PER_FRAME] {
        core::array::from_fn(|i| {
            DnFrame::from_bytes([
                u8::try_from(i + 1).expect("small"),
                0x5A,
                0xA5,
                0x0F,
                0xF0,
                0x33,
                0x80,
            ])
        })
    }

    #[test]
    fn a_block_carries_two_frames_at_bit_offsets_zero_and_forty_nine() {
        // MMDVMHost/NXDNAudio.cpp: CNXDNAudio::decode(in, out) is exactly
        // `decode(in + 0U, out, 0U); decode(in + 9U, out, 49U);` — the
        // second frame starts at bit 49 of the 14-byte block, which is why
        // DroidStar's nxdn.cpp reads its bytes shifted left by one.
        let a = DnFrame::from_bytes([0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x80]);
        let b = DnFrame::from_bytes([0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00]);
        let block = pack_voice_block(a, b);
        assert_eq!(block.len(), BLOCK_LEN);
        assert_eq!(unpack_voice_block(&block), [a, b]);
        // The last 14 bits of the block are padding: 2 * 49 = 98 of 112.
        assert_eq!(block[12] & 0x03, 0);
        assert_eq!(block[13], 0);
    }

    #[test]
    fn droidstars_offsets_are_the_offsets_this_produces() {
        // DroidStar nxdn.cpp: NXDN::process_udp takes frame 1 from
        // `buf + 15` verbatim and frame 2 from `buf + 21` shifted left one
        // bit. Datagram byte 15 is block 0 byte 0; byte 21 is block 0 byte 6.
        let a = DnFrame::from_bytes([0xA5, 0x3C, 0x71, 0x0E, 0xC3, 0x96, 0x80]);
        let b = DnFrame::from_bytes([0x5A, 0xC3, 0x8E, 0xF1, 0x3C, 0x69, 0x00]);
        let block = pack_voice_block(a, b);
        let mut first = [0u8; 7];
        first.copy_from_slice(&block[..7]);
        assert_eq!(DnFrame::from_bytes(first), a);
        let mut second = [0u8; 7];
        for i in 0..6 {
            second[i] = (block[6 + i] << 1) | (block[7 + i] >> 7);
        }
        second[6] = block[12] << 1;
        assert_eq!(DnFrame::from_bytes(second), b);
    }

    #[test]
    fn a_voice_frame_yields_four_frames_in_wire_order() {
        let voice = distinct();
        let mut bytes = [0u8; astar_nxdn::FRAME_LEN];
        bytes[0] = (RFCT_RDCH << 6) | (USC_SACCH_SS << 4) | (STEAL_NONE << 2);
        bytes[5..19].copy_from_slice(&pack_voice_block(voice[0], voice[1]));
        bytes[19..33].copy_from_slice(&pack_voice_block(voice[2], voice[3]));
        assert_eq!(unpack_voice(&bytes).expect("voice"), voice);
    }

    #[test]
    fn a_voice_header_is_refused_with_its_message_type() {
        // Silence with a reason beats garbage: a header frame carries
        // signalling in both blocks and no audio at all.
        let mut bytes = [0u8; astar_nxdn::FRAME_LEN];
        bytes[0] = 0x81;
        bytes[5] = astar_nxdn::frame::MESSAGE_TYPE_VCALL;
        assert_eq!(
            unpack_voice(&bytes),
            Err(NxdnVoiceError::Signalling { message_type: 0x01 })
        );
    }

    #[test]
    fn a_udch_frame_is_refused_rather_than_read_as_voice() {
        let mut bytes = [0u8; astar_nxdn::FRAME_LEN];
        bytes[0] = (RFCT_RDCH << 6) | (astar_nxdn::frame::USC_UDCH << 4);
        assert!(matches!(
            unpack_voice(&bytes),
            Err(NxdnVoiceError::NotVoice { .. })
        ));
    }
    #[test]
    fn a_stolen_half_mutes_its_two_frames_and_keeps_the_others() {
        // MMDVMHost/NXDNControl.cpp, the AUDIO branch: STEAL_FACCH1_1 puts
        // a FACCH1 raw in block 0 and audio in block 1. There is no audio
        // in the stolen block to decode, so MUTE goes there — never zeros,
        // which are valid voice parameters an AMBE decoder renders as a
        // click.
        use astar_nxdn::frame::{STEAL_FACCH1_1, STEAL_FACCH1_2};
        let voice = distinct();

        let mut bytes = [0u8; astar_nxdn::FRAME_LEN];
        bytes[0] = (RFCT_RDCH << 6) | (USC_SACCH_SS << 4) | (STEAL_FACCH1_1 << 2);
        bytes[19..33].copy_from_slice(&pack_voice_block(voice[2], voice[3]));
        assert_eq!(
            unpack_voice(&bytes).expect("half voice"),
            [DnFrame::MUTE, DnFrame::MUTE, voice[2], voice[3]]
        );

        let mut bytes = [0u8; astar_nxdn::FRAME_LEN];
        bytes[0] = (RFCT_RDCH << 6) | (USC_SACCH_SS << 4) | (STEAL_FACCH1_2 << 2);
        bytes[5..19].copy_from_slice(&pack_voice_block(voice[0], voice[1]));
        assert_eq!(
            unpack_voice(&bytes).expect("half voice"),
            [voice[0], voice[1], DnFrame::MUTE, DnFrame::MUTE]
        );
    }

    #[test]
    fn packing_a_block_touches_no_bit_outside_its_ninety_eight() {
        // 2 * 49 = 98 of 112. The reference leaves the tail zero and so
        // does this; asserting the whole tail, not just the two bits the
        // offsets test happens to look at.
        let all_ones = DnFrame::from_bytes([0xFF; VOICE_BYTES]);
        let block = pack_voice_block(all_ones, all_ones);
        for i in 2 * VOICE_BITS..BLOCK_LEN * 8 {
            assert!(!read_bit(&block, i), "bit {i} of the block is padding");
        }
        for i in 0..2 * VOICE_BITS {
            assert!(read_bit(&block, i), "bit {i} of the block is voice");
        }
    }

    #[test]
    fn every_bit_of_a_frame_survives_the_round_trip() {
        // One bit set at a time, all 49, in both slots: a bit loop that
        // drops or shifts a single position cannot hide from this.
        for bit in 0..VOICE_BITS {
            let mut raw = [0u8; VOICE_BYTES];
            raw[bit >> 3] |= 0x80 >> (bit & 7);
            let one = DnFrame::from_bytes(raw);
            assert_eq!(
                unpack_voice_block(&pack_voice_block(one, DnFrame::default())),
                [one, DnFrame::default()]
            );
            assert_eq!(
                unpack_voice_block(&pack_voice_block(DnFrame::default(), one)),
                [DnFrame::default(), one]
            );
        }
    }

    #[test]
    fn nxdn_and_ysf_dn_ask_an_ambe_3000_for_the_same_thing() {
        // DroidStar serialambe.cpp: SerialAMBE::config_ambe sends
        // AMBE3000_2450_0000 for both "YSF" and "NXDN", packet_size 7. So
        // NXDN needs no RATEP word of its own — this is the assertion that
        // would break if someone ever gave it one.
        assert_eq!(VOICE_BITS, 49);
        assert_eq!(VOICE_BYTES, 7);
        assert_eq!(
            crate::ysf::ratep_dn(),
            vec![
                0x61, 0x00, 0x0D, 0x00, 0x0A, 0x04, 0x31, 0x07, 0x54, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x70, 0x31
            ]
        );
    }

    #[test]
    fn four_frames_is_two_blocks_of_two() {
        assert_eq!(FRAMES_PER_FRAME, 4);
        assert_eq!(FRAMES_PER_BLOCK, 2);
        assert_eq!(BLOCK_LEN, 14);
    }
}
