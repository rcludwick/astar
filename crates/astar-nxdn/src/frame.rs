// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The 33-byte network frame: the LICH byte, the SACCH, and the two
//! 14-byte blocks a `NXDND` datagram carries.
//!
//! | offset | bytes | what |
//! |---|---|---|
//! | 0 | 1 | LICH raw — `NXDNControl.cpp`: `netData[0U] = lich.getRaw();` |
//! | 1 | 4 | SACCH raw, 26 bits + CRC-6 — `NXDNControl.cpp`: `sacch.getRaw(netData + 1U);` |
//! | 5 | 14 | block 0 — `NXDNControl.cpp`: `audio.decode(…, netData + 5U + 0U)` or `facch1.getRaw(netData + 5U + 0U)` |
//! | 19 | 14 | block 1 — `NXDNControl.cpp`: `… netData + 5U + 14U` |
//!
//! **This is not the over-the-air frame.** `MMDVMHost` strips the frame
//! sync, the FEC and the interleave before a frame goes on the network, so
//! there is no Golay, no PRNG and no de-interleave here — only the LICH's
//! four bit-fields and two blocks of already-de-FEC'd payload. See
//! `docs/design/nxdn-wire.md`.
//!
//! **What is in a block is not this crate's business.** [`NetFrame::kind`]
//! reads the LICH far enough to say *whether* a block holds voice or
//! signalling, which is routing, and hands the bytes over untouched.
//! Lifting the four 49-bit AMBE+2 frames out of them is the vocoder
//! layer's job and lives in `astar_codec::nxdn` — the same split
//! `astar-ysf` and `astar_codec::ysf` already have.
//!
//! Every constant below cites the file and function it was read out of.
//! No code was copied: the reference implementations are GPL-2.0 and this
//! is AGPL-3.0-only, so the rules are written out from their definitions.

use crate::wire;

/// Bytes in a network frame — the same [`crate::wire::FRAME_LEN`] a
/// `NXDND` datagram carries, named again here because this module is where
/// the 33 bytes acquire a shape.
pub const FRAME_LEN: usize = wire::FRAME_LEN;

/// Bytes in one payload block. `NXDNControl.cpp` writes them at
/// `netData + 5U + 0U` and `netData + 5U + 14U`.
pub const BLOCK_LEN: usize = 14;

/// Payload blocks in a frame.
pub const BLOCKS: usize = 2;

/// Byte offset of the LICH. `NXDNControl.cpp`: `netData[0U]`.
const LICH_OFFSET: usize = 0;

/// Byte offset of the SACCH. `NXDNControl.cpp`: `sacch.getRaw(netData + 1U)`.
const SACCH_OFFSET: usize = 1;

/// Bytes of SACCH: 26 bits of message plus a CRC-6, rounded up to four.
/// `NXDNSACCH.cpp: getRaw` copies four bytes then
/// `CNXDNCRC::encodeCRC6(data, 26U)`.
pub const SACCH_LEN: usize = 4;

/// Byte offset of block 0. `NXDNControl.cpp`: `netData + 5U + 0U`.
const BLOCK_OFFSET: usize = 5;

// ── LICH fields (MMDVMHost/NXDNDefines.h) ───────────────────────────────

/// RF channel type: RCCH, the control channel. `NXDNDefines.h`.
pub const RFCT_RCCH: u8 = 0;
/// RF channel type: RTCH, the traffic channel. `NXDNDefines.h`.
pub const RFCT_RTCH: u8 = 1;
/// RF channel type: RDCH, the direct/conventional channel — what a
/// reflector's traffic is. `NXDNDefines.h`.
pub const RFCT_RDCH: u8 = 2;
/// RF channel type: RTCH-C, the composite traffic channel. `NXDNDefines.h`.
pub const RFCT_RTCH_C: u8 = 3;

/// Functional channel type: non-superframe SACCH — the header and
/// terminator frames. `NXDNDefines.h`.
pub const USC_SACCH_NS: u8 = 0;
/// Functional channel type: UDCH, a user data channel. `NXDNDefines.h`.
pub const USC_UDCH: u8 = 1;
/// Functional channel type: superframe SACCH — voice, mid-transmission.
/// `NXDNDefines.h`.
pub const USC_SACCH_SS: u8 = 2;
/// Functional channel type: idle. `NXDNDefines.h`.
pub const USC_SACCH_IDLE: u8 = 3;

/// Steal option: both blocks are FACCH1. `NXDNDefines.h`.
pub const STEAL_FACCH: u8 = 0;
/// Steal option: block 0 is FACCH1, block 1 is voice.
/// `NXDNControl.cpp`'s AUDIO branch: `facch1.getRaw(netData + 5U + 0U);`
/// then `audio.decode(…, netData + 5U + 14U);`. `NXDNDefines.h`.
pub const STEAL_FACCH1_1: u8 = 1;
/// Steal option: block 0 is voice, block 1 is FACCH1 — the mirror of
/// [`STEAL_FACCH1_1`], same branch. `NXDNDefines.h`.
pub const STEAL_FACCH1_2: u8 = 2;
/// Steal option: nothing stolen, four AMBE+2 frames. `NXDNDefines.h`.
pub const STEAL_NONE: u8 = 3;

// ── Layer-3 message types (MMDVMHost/NXDNDefines.h) ─────────────────────

/// Layer-3 `VCALL`: the voice-call header. `NXDNGateway/NXDNNetwork.cpp:
/// writeData` reads it out of `data[5U]` to set the start-of-transmission
/// flag.
pub const MESSAGE_TYPE_VCALL: u8 = 0x01;
/// Layer-3 `TX_REL`: transmission release, the terminator. Same
/// `writeData`, setting the end-of-transmission flag.
pub const MESSAGE_TYPE_TX_REL: u8 = 0x08;

/// The Link Information Channel byte: four bit-fields in eight bits.
///
/// `MMDVMHost/NXDNLICH.cpp` reads them as bits 7–6 `RFCT`, 5–4 `FCT` (the
/// USC), 3–2 `Option` (the steal), bit 1 direction, bit 0 parity. Bit
/// numbering is MSB-first, as everywhere in NXDN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lich(u8);

impl Lich {
    /// Wraps a raw LICH byte. Every bit pattern is a `Lich`; whether it is
    /// a *valid* one is [`Lich::parity_ok`]'s question.
    #[must_use]
    pub const fn from_raw(raw: u8) -> Lich {
        Lich(raw)
    }

    /// The byte as it sits on the wire. `NXDNLICH.cpp: getRaw`.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// RF channel type, bits 7–6. `NXDNLICH.cpp: getRFCT`.
    #[must_use]
    pub const fn rfct(self) -> u8 {
        (self.0 >> 6) & 0x03
    }

    /// Functional channel type (USC), bits 5–4. `NXDNLICH.cpp: getFCT`.
    #[must_use]
    pub const fn fct(self) -> u8 {
        (self.0 >> 4) & 0x03
    }

    /// Steal option, bits 3–2. `NXDNLICH.cpp: getOption`.
    #[must_use]
    pub const fn option(self) -> u8 {
        (self.0 >> 2) & 0x03
    }

    /// Direction, bit 1: 0 inbound, 1 outbound. `NXDNLICH.cpp:
    /// getDirection`.
    #[must_use]
    pub const fn direction(self) -> u8 {
        (self.0 >> 1) & 0x01
    }

    /// The parity bit a well-formed LICH with this top nibble carries.
    ///
    /// `NXDNLICH.cpp: getParity` is a *generator*, not a check: it is true
    /// exactly when `m_lich[0] & 0xF0` is `0x80` or `0xB0`, and
    /// `CNXDNLICH::getRaw` calls it to set or clear bit 0 before handing
    /// the byte out. It is a two-entry lookup dressed as arithmetic, so it
    /// is written out here as the two cases it is rather than derived from
    /// a polynomial it does not use.
    #[must_use]
    pub const fn parity_expected(self) -> bool {
        matches!(self.0 & 0xF0, 0x80 | 0xB0)
    }

    /// Whether a *received* byte's parity bit agrees with
    /// [`Lich::parity_expected`].
    ///
    /// This is the receive-side half of the same rule. `0x81` and `0xB1`
    /// are consistent — the nibble says parity 1 and bit 0 is 1. `0xA1`
    /// (nibble says 0, bit is 1) and `0x80` (nibble says 1, bit is 0) are
    /// not, and neither is a byte `getRaw` would ever emit.
    #[must_use]
    pub const fn parity_ok(self) -> bool {
        (self.direction_and_parity() & 0x01 == 1) == self.parity_expected()
    }

    /// Bits 1-0 together, so [`Lich::parity_ok`] can stay `const` without
    /// repeating the mask.
    const fn direction_and_parity(self) -> u8 {
        self.0 & 0x03
    }
}

/// What a 33-byte frame is, as far as a voice client cares.
///
/// This is [`Lich::fct`] and [`Lich::option`] read together, and nothing
/// else — deliberately not a claim that the bytes inside a block are
/// well-formed. [`Lich::parity_ok`] is *not* consulted: a frame whose
/// parity bit is wrong still routes, and refusing audio over a single bit
/// no reference implementation checks on receive would be silence with a
/// bad reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Both blocks are FACCH1, and byte 0 of block 0 is the Layer-3
    /// message type.
    ///
    /// The header and terminator frames — LICH `0x81` inbound / `0x83`
    /// outbound, [`USC_SACCH_NS`] with [`STEAL_FACCH`] — are this, and
    /// [`MESSAGE_TYPE_VCALL`] / [`MESSAGE_TYPE_TX_REL`] are the message
    /// types `NXDNGateway/NXDNNetwork.cpp: writeData` looks for. So is
    /// mid-transmission [`USC_SACCH_SS`] with [`STEAL_FACCH`], whose
    /// `NXDNControl.cpp` branch also writes a FACCH1 raw into both blocks
    /// and no audio into either.
    Signalling {
        /// Byte 0 of block 0 — `data[5U]` in `writeData`.
        message_type: u8,
    },
    /// [`STEAL_NONE`]: four AMBE+2 frames, two per block.
    Voice,
    /// One block stolen for FACCH1, the other voice — [`STEAL_FACCH1_1`]
    /// and [`STEAL_FACCH1_2`].
    HalfVoice {
        /// Index of the block that still carries two voice frames.
        voice_block: usize,
    },
    /// UDCH, idle, or anything else this build does not read.
    Other,
}

/// A borrowed 33-byte network frame, read as fields.
///
/// Borrowed rather than owned because the bytes already exist: they arrive
/// inside [`crate::wire::DataPacket::frame`] and there is no reason to copy
/// 33 bytes to look at one of them.
#[derive(Debug, Clone, Copy)]
pub struct NetFrame<'a>(&'a [u8; FRAME_LEN]);

impl<'a> NetFrame<'a> {
    /// Views 33 bytes as a frame. Infallible — the length is the type, and
    /// every byte pattern parses; [`NetFrame::kind`] is where a frame gets
    /// to be one astar does not read.
    #[must_use]
    pub const fn new(bytes: &'a [u8; FRAME_LEN]) -> NetFrame<'a> {
        NetFrame(bytes)
    }

    /// The 33 bytes, as they arrived.
    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8; FRAME_LEN] {
        self.0
    }

    /// The LICH byte, byte 0.
    #[must_use]
    pub const fn lich(&self) -> Lich {
        Lich::from_raw(self.0[LICH_OFFSET])
    }

    /// The four SACCH bytes at offset 1 — 26 bits of message and a CRC-6,
    /// carried, not interpreted.
    #[must_use]
    pub fn sacch(&self) -> [u8; SACCH_LEN] {
        let mut out = [0u8; SACCH_LEN];
        out.copy_from_slice(&self.0[SACCH_OFFSET..][..SACCH_LEN]);
        out
    }

    /// One 14-byte payload block: block 0 at offset 5, block 1 at 19.
    ///
    /// # Panics
    ///
    /// If `index` is not less than [`BLOCKS`].
    #[must_use]
    pub fn block(&self, index: usize) -> [u8; BLOCK_LEN] {
        assert!(index < BLOCKS, "NXDN frame has {BLOCKS} blocks");
        let at = BLOCK_OFFSET + index * BLOCK_LEN;
        let mut out = [0u8; BLOCK_LEN];
        out.copy_from_slice(&self.0[at..][..BLOCK_LEN]);
        out
    }

    /// What this frame carries, read from the LICH.
    #[must_use]
    pub fn kind(&self) -> FrameKind {
        let lich = self.lich();
        match (lich.fct(), lich.option()) {
            // Header and terminator: both blocks FACCH1, byte 0 of block 0
            // is the Layer-3 message type `writeData` reads.
            (USC_SACCH_NS | USC_SACCH_SS, STEAL_FACCH) => FrameKind::Signalling {
                message_type: self.0[BLOCK_OFFSET],
            },
            (USC_SACCH_SS, STEAL_NONE) => FrameKind::Voice,
            (USC_SACCH_SS, STEAL_FACCH1_1) => FrameKind::HalfVoice { voice_block: 1 },
            (USC_SACCH_SS, STEAL_FACCH1_2) => FrameKind::HalfVoice { voice_block: 0 },
            _ => FrameKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_header_lich_is_0x81() {
        // NXDNGateway/NXDNNetwork.cpp: writeData branches on
        // `data[0] == 0x81 || data[0] == 0x83`. MMDVMHost/NXDNLICH.cpp says
        // why: RFCT_RDCH(2)<<6 | USC_SACCH_NS(0)<<4 | STEAL_FACCH(0)<<2 |
        // inbound(0)<<1 | parity(1).
        let lich = Lich::from_raw(0x81);
        assert_eq!(lich.rfct(), RFCT_RDCH);
        assert_eq!(lich.fct(), USC_SACCH_NS);
        assert_eq!(lich.option(), STEAL_FACCH);
        assert_eq!(lich.direction(), 0);
        assert!(lich.parity_ok());
    }

    #[test]
    fn parity_is_the_published_rule_not_a_checksum() {
        // MMDVMHost/NXDNLICH.cpp: getParity is true for 0x80 and 0xB0 only,
        // over the top nibble. Written out rather than tabulated.
        assert!(Lich::from_raw(0x81).parity_ok());
        assert!(Lich::from_raw(0xB1).parity_ok());
        assert!(!Lich::from_raw(0xA1).parity_ok());
        assert!(!Lich::from_raw(0x80).parity_ok());
    }

    #[test]
    fn a_voice_frame_lich_steals_nothing() {
        // NXDNControl.cpp's AUDIO branch: USC_SACCH_SS + the frame's own
        // steal option; STEAL_NONE is the four-AMBE case.
        let raw = (RFCT_RDCH << 6) | (USC_SACCH_SS << 4) | (STEAL_NONE << 2);
        let mut bytes = [0u8; FRAME_LEN];
        bytes[0] = raw;
        assert_eq!(NetFrame::new(&bytes).kind(), FrameKind::Voice);
    }

    #[test]
    fn a_signalling_frame_reports_its_message_type() {
        // NXDNGateway/NXDNNetwork.cpp reads `data[5U]` — netData[5], the
        // first byte of block 0's FACCH1 raw — for 0x01 VCALL / 0x08 TX_REL.
        let mut bytes = [0u8; FRAME_LEN];
        bytes[0] = 0x81;
        bytes[5] = MESSAGE_TYPE_TX_REL;
        assert_eq!(
            NetFrame::new(&bytes).kind(),
            FrameKind::Signalling {
                message_type: MESSAGE_TYPE_TX_REL
            }
        );
    }

    #[test]
    fn the_blocks_are_at_five_and_nineteen() {
        // MMDVMHost/NXDNControl.cpp: netData + 5U + 0U and netData + 5U + 14U.
        let mut bytes = [0u8; FRAME_LEN];
        bytes[5] = 0xAA;
        bytes[19] = 0xBB;
        let f = NetFrame::new(&bytes);
        assert_eq!(f.block(0)[0], 0xAA);
        assert_eq!(f.block(1)[0], 0xBB);
        assert_eq!(f.sacch(), [0, 0, 0, 0]);
    }
    /// Builds a LICH byte with the parity bit `getRaw` would stamp on it.
    fn lich_raw(fct: u8, option: u8) -> u8 {
        let raw = (RFCT_RDCH << 6) | (fct << 4) | (option << 2);
        raw | u8::from(Lich::from_raw(raw).parity_expected())
    }

    #[test]
    fn a_stolen_half_names_the_block_that_still_has_voice() {
        // MMDVMHost/NXDNControl.cpp, the AUDIO branch: STEAL_FACCH1_1 does
        // `facch1.getRaw(netData + 5U + 0U)` then
        // `audio.decode(..., netData + 5U + 14U)` — block 1 keeps the
        // audio. STEAL_FACCH1_2 is the mirror.
        let mut bytes = [0u8; FRAME_LEN];
        bytes[0] = lich_raw(USC_SACCH_SS, STEAL_FACCH1_1);
        assert_eq!(
            NetFrame::new(&bytes).kind(),
            FrameKind::HalfVoice { voice_block: 1 }
        );
        bytes[0] = lich_raw(USC_SACCH_SS, STEAL_FACCH1_2);
        assert_eq!(
            NetFrame::new(&bytes).kind(),
            FrameKind::HalfVoice { voice_block: 0 }
        );
    }

    #[test]
    fn a_superframe_frame_that_steals_both_halves_is_signalling_too() {
        // Same branch's else: `facch11.getRaw(netData + 5U + 0U);
        // facch12.getRaw(netData + 5U + 14U);` — no audio in either block,
        // so it is signalling for the same reason a header frame is.
        let mut bytes = [0u8; FRAME_LEN];
        bytes[0] = lich_raw(USC_SACCH_SS, STEAL_FACCH);
        bytes[5] = 0x2F;
        assert_eq!(
            NetFrame::new(&bytes).kind(),
            FrameKind::Signalling { message_type: 0x2F }
        );
    }

    #[test]
    fn udch_and_idle_are_neither_voice_nor_signalling() {
        // Nothing in this build reads a user-data channel, and saying so is
        // better than guessing at its bytes.
        for fct in [USC_UDCH, USC_SACCH_IDLE] {
            let mut bytes = [0u8; FRAME_LEN];
            bytes[0] = lich_raw(fct, STEAL_NONE);
            assert_eq!(NetFrame::new(&bytes).kind(), FrameKind::Other);
        }
    }

    #[test]
    fn the_channel_types_are_the_defined_ones() {
        // MMDVMHost/NXDNDefines.h, in order.
        assert_eq!([RFCT_RCCH, RFCT_RTCH, RFCT_RDCH, RFCT_RTCH_C], [0, 1, 2, 3]);
        assert_eq!(
            [USC_SACCH_NS, USC_UDCH, USC_SACCH_SS, USC_SACCH_IDLE],
            [0, 1, 2, 3]
        );
        assert_eq!(
            [STEAL_FACCH, STEAL_FACCH1_1, STEAL_FACCH1_2, STEAL_NONE],
            [0, 1, 2, 3]
        );
        assert_eq!(MESSAGE_TYPE_VCALL, 0x01);
        assert_eq!(MESSAGE_TYPE_TX_REL, 0x08);
    }

    #[test]
    fn a_frames_two_blocks_tile_the_bytes_after_the_sacch() {
        // 1 LICH + 4 SACCH + 2 * 14 = 33; nothing is left over and nothing
        // overlaps.
        assert_eq!(1 + SACCH_LEN + BLOCKS * BLOCK_LEN, FRAME_LEN);
        let bytes: [u8; FRAME_LEN] = core::array::from_fn(|i| u8::try_from(i).expect("< 33"));
        let f = NetFrame::new(&bytes);
        assert_eq!(f.lich().raw(), 0);
        assert_eq!(f.sacch(), [1, 2, 3, 4]);
        assert_eq!(f.block(0)[0], 5);
        assert_eq!(f.block(0)[BLOCK_LEN - 1], 18);
        assert_eq!(f.block(1)[0], 19);
        assert_eq!(f.block(1)[BLOCK_LEN - 1], 32);
    }

    #[test]
    fn an_outbound_voice_header_is_0x83() {
        // NXDNGateway/NXDNNetwork.cpp: writeData accepts 0x81 or 0x83 — the
        // same fields with the direction bit set.
        let lich = Lich::from_raw(0x83);
        assert_eq!(lich.rfct(), RFCT_RDCH);
        assert_eq!(lich.fct(), USC_SACCH_NS);
        assert_eq!(lich.option(), STEAL_FACCH);
        assert_eq!(lich.direction(), 1);
        assert!(lich.parity_ok());
        assert_eq!(lich.raw(), 0x83);
    }
}
