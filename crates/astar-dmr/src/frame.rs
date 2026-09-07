// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The 33-byte DMR burst: sync, EMB, embedded and full link control, the slot
//! type, and the three 72-bit AMBE+2 frames a voice burst carries.
//!
//! Specified byte by byte in `docs/design/dmr-wire.md` §5–§7. 264 bits, split
//! `[108 info][48 sync-or-EMB][108 info]`, and the split is **nibble-aligned**:
//! the middle field starts at burst byte 13's low nibble and ends at byte 19's
//! high nibble. That is the whole reason this module exists as arithmetic
//! rather than as slice copies — two writers share bytes 13 and 19, and each
//! has to leave the other's nibble alone.
//!
//! | burst bits | burst bytes | what |
//! |---|---|---|
//! | 0–107 | 0..12 whole, 13 high nibble | information half 1 |
//! | 108–155 | 13 low nibble, 14..18 whole, 19 high nibble | sync, or EMB + embedded LC |
//! | 156–263 | 19 low nibble, 20..32 whole | information half 2 |
//!
//! **This module says nothing about what the voice bits mean.** [`ambe`] and
//! [`write_ambe`] rearrange 216 bits and stop there; that they are AMBE+2 is
//! `astar-codec`'s business, and the dependency runs that way round — this
//! crate never depends on the codec.
//!
//! # On the layout
//!
//! Read out of the deployed reference implementations as a specification of
//! the wire, the same way the rest of this crate was: `MMDVMHost/DMRDefines.h`
//! for the constants, `MMDVMHost/Sync.cpp` and `DroidStar dmr.cpp`'s
//! `addDMRAudioSync`/`addDMRDataSync` for the sync write, `DroidStar dmr.cpp`'s
//! `send_frame`/`process_udp` and `MMDVMHost/AMBEFEC.cpp`'s `regenerateDMR`
//! for the three-frame split, `MMDVMHost/DMREMB.cpp` and
//! `MMDVMHost/DMREmbeddedData.cpp` for the EMB and fragment nibbles,
//! `MMDVMHost/DMRLC.cpp`/`DMRFullLC.cpp` for the LC bytes and the reversed
//! parity order, and `MMDVMHost/DMRSlotType.cpp` for the slot type. Every
//! constant below carries the file and function it was read out of. No code
//! was copied; `docs/design/dmr-wire.md` records the fetch and how the
//! disagreements were settled.

use crate::fec;

/// The burst is 33 bytes, and it is defined once — in [`crate::wire`], where
/// the `DMRD` packet that carries it lives. Re-exported here so the frame
/// layer reads naturally without a second, driftable definition.
pub use crate::wire::BURST_LEN;

/// The middle field spans seven bytes even though it is 48 bits, because the
/// first and last are half-masked. `MMDVMHost/DMRDefines.h`:
/// `DMR_SYNC_LENGTH_BYTES` is `6U` — the 48 bits — while the write at
/// `data + 13U` touches seven.
pub const SYNC_LEN: usize = 7;

/// The 216 information bits of a voice burst, as bytes.
pub const AMBE_BYTES: usize = 27;

/// Three 20 ms AMBE+2 frames: 60 ms of audio, sent in the burst that fills
/// one 30 ms TDMA timeslot.
pub const AMBE_FRAMES: usize = 3;

/// 72 bits each — 2450 bit/s of voice plus 1150 bit/s of the chip's own FEC.
pub const AMBE_FRAME_BYTES: usize = 9;

/// `MMDVMHost/DMRDefines.h`. Seven bytes each, written into burst bytes
/// 13..20 under [`SYNC_MASK`]; the first and last nibbles belong to the
/// information halves and must never be disturbed.
pub const SYNC_MASK: [u8; SYNC_LEN] = [0x0F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xF0];
/// `MMDVMHost/DMRDefines.h`: `MS_SOURCED_AUDIO_SYNC`. astar sends this one.
pub const MS_SOURCED_AUDIO_SYNC: [u8; SYNC_LEN] = [0x07, 0xF7, 0xD5, 0xDD, 0x57, 0xDF, 0xD0];
/// `MMDVMHost/DMRDefines.h`: `MS_SOURCED_DATA_SYNC`. astar sends this one.
pub const MS_SOURCED_DATA_SYNC: [u8; SYNC_LEN] = [0x0D, 0x5D, 0x7F, 0x77, 0xFD, 0x75, 0x70];
/// `MMDVMHost/DMRDefines.h`: `BS_SOURCED_AUDIO_SYNC` — recognised, never sent.
pub const BS_SOURCED_AUDIO_SYNC: [u8; SYNC_LEN] = [0x07, 0x55, 0xFD, 0x7D, 0xF7, 0x5F, 0x70];
/// `MMDVMHost/DMRDefines.h`: `BS_SOURCED_DATA_SYNC` — recognised, never sent.
pub const BS_SOURCED_DATA_SYNC: [u8; SYNC_LEN] = [0x0D, 0xFF, 0x57, 0xD7, 0x5D, 0xF5, 0xD0];
/// `MMDVMHost/DMRDefines.h`: `DIRECT_SLOT1_AUDIO_SYNC`.
pub const DIRECT_SLOT1_AUDIO_SYNC: [u8; SYNC_LEN] = [0x05, 0xD5, 0x77, 0xF7, 0x75, 0x7F, 0xF0];
/// `MMDVMHost/DMRDefines.h`: `DIRECT_SLOT1_DATA_SYNC`.
pub const DIRECT_SLOT1_DATA_SYNC: [u8; SYNC_LEN] = [0x0F, 0x7F, 0xDD, 0x5D, 0xDF, 0xD5, 0x50];
/// `MMDVMHost/DMRDefines.h`: `DIRECT_SLOT2_AUDIO_SYNC`.
pub const DIRECT_SLOT2_AUDIO_SYNC: [u8; SYNC_LEN] = [0x07, 0xDF, 0xFD, 0x5F, 0x55, 0xD5, 0xF0];
/// `MMDVMHost/DMRDefines.h`: `DIRECT_SLOT2_DATA_SYNC`.
pub const DIRECT_SLOT2_DATA_SYNC: [u8; SYNC_LEN] = [0x0D, 0x75, 0x57, 0xF5, 0xFF, 0x7F, 0x50];

/// `MMDVMHost/DMRDefines.h` — the wire code points only. `DT_VOICE_SYNC 0xF0`
/// and `DT_VOICE 0xF1` are MMDVMHost-local sentinels and are deliberately
/// absent: they never appear on the wire.
pub const DT_VOICE_PI_HEADER: u8 = 0x00;
/// `MMDVMHost/DMRDefines.h`: `DT_VOICE_LC_HEADER`.
pub const DT_VOICE_LC_HEADER: u8 = 0x01;
/// `MMDVMHost/DMRDefines.h`: `DT_TERMINATOR_WITH_LC`.
pub const DT_TERMINATOR_WITH_LC: u8 = 0x02;
/// `MMDVMHost/DMRDefines.h`: `DT_CSBK`.
pub const DT_CSBK: u8 = 0x03;
/// `MMDVMHost/DMRDefines.h`: `DT_DATA_HEADER`.
pub const DT_DATA_HEADER: u8 = 0x06;
/// `MMDVMHost/DMRDefines.h`: `DT_IDLE`.
pub const DT_IDLE: u8 = 0x09;

/// `MMDVMHost/DMRDefines.h`: `enum class FLCO`, `GROUP = 0`.
pub const FLCO_GROUP: u8 = 0;
/// `MMDVMHost/DMRDefines.h`: `enum class FLCO`, `USER_USER = 3`.
pub const FLCO_USER_USER: u8 = 3;
/// `MMDVMHost/DMRDefines.h`: `FID_ETSI`.
pub const FID_ETSI: u8 = 0;
/// `MMDVMHost/DMRDefines.h`: `VOICE_LC_HEADER_CRC_MASK`.
pub const VOICE_LC_HEADER_CRC_MASK: [u8; 3] = [0x96, 0x96, 0x96];
/// `MMDVMHost/DMRDefines.h`: `TERMINATOR_WITH_LC_CRC_MASK`.
pub const TERMINATOR_WITH_LC_CRC_MASK: [u8; 3] = [0x99, 0x99, 0x99];

/// The first byte of the middle field, and the first the AMBE stream shares.
const MIDDLE: usize = 13;

/// Which published 48-bit pattern a burst's middle field holds.
///
/// `MMDVMHost/Sync.cpp` selects `BS_SOURCED_*` when `duplex` is true and
/// `MS_SOURCED_*` when it is false; `DroidStar dmr.cpp` always passes `0`.
/// **astar is a mobile station and sends `Ms*`** — a softclient is not a base
/// station and must not claim to be one. The rest are recognised on receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sync {
    /// `MS_SOURCED_AUDIO_SYNC`.
    MsAudio,
    /// `MS_SOURCED_DATA_SYNC`.
    MsData,
    /// `BS_SOURCED_AUDIO_SYNC`.
    BsAudio,
    /// `BS_SOURCED_DATA_SYNC`.
    BsData,
    /// `DIRECT_SLOT1_AUDIO_SYNC`.
    DirectSlot1Audio,
    /// `DIRECT_SLOT1_DATA_SYNC`.
    DirectSlot1Data,
    /// `DIRECT_SLOT2_AUDIO_SYNC`.
    DirectSlot2Audio,
    /// `DIRECT_SLOT2_DATA_SYNC`.
    DirectSlot2Data,
    /// No published pattern matched. Bursts B-F carry EMB here, not sync.
    None,
}

impl Sync {
    /// The seven bytes this pattern writes, or `None` for [`Sync::None`],
    /// which names the absence of one.
    #[must_use]
    pub const fn pattern(self) -> Option<[u8; SYNC_LEN]> {
        match self {
            Self::MsAudio => Some(MS_SOURCED_AUDIO_SYNC),
            Self::MsData => Some(MS_SOURCED_DATA_SYNC),
            Self::BsAudio => Some(BS_SOURCED_AUDIO_SYNC),
            Self::BsData => Some(BS_SOURCED_DATA_SYNC),
            Self::DirectSlot1Audio => Some(DIRECT_SLOT1_AUDIO_SYNC),
            Self::DirectSlot1Data => Some(DIRECT_SLOT1_DATA_SYNC),
            Self::DirectSlot2Audio => Some(DIRECT_SLOT2_AUDIO_SYNC),
            Self::DirectSlot2Data => Some(DIRECT_SLOT2_DATA_SYNC),
            Self::None => None,
        }
    }
}

/// Every pattern, in the order [`sync_of`] tries them.
const PATTERNS: [Sync; 8] = [
    Sync::MsAudio,
    Sync::MsData,
    Sync::BsAudio,
    Sync::BsData,
    Sync::DirectSlot1Audio,
    Sync::DirectSlot1Data,
    Sync::DirectSlot2Audio,
    Sync::DirectSlot2Data,
];

/// The 48-bit middle field as its own seven bytes, masked.
///
/// The flanking nibbles — byte 13's high, byte 19's low — are information,
/// so they read back as zero here whatever the burst holds.
#[must_use]
pub fn middle(burst: &[u8; BURST_LEN]) -> [u8; SYNC_LEN] {
    let mut out = [0u8; SYNC_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = burst[MIDDLE + i] & SYNC_MASK[i];
    }
    out
}

/// Read the 48-bit middle field and say which published pattern it is,
/// under [`SYNC_MASK`] so the flanking information nibbles are ignored.
///
/// Exact match only. DMR's sync is 48 bits and a receiver that accepts a near
/// miss accepts an EMB, which is what bursts B-F put here instead.
#[must_use]
pub fn sync_of(burst: &[u8; BURST_LEN]) -> Sync {
    let got = middle(burst);
    for candidate in PATTERNS {
        if candidate.pattern() == Some(got) {
            return candidate;
        }
    }
    Sync::None
}

/// Write a sync pattern into the middle field, byte 13's high nibble and byte
/// 19's low nibble untouched.
///
/// `MMDVMHost/Sync.cpp: CSync::addDMRAudioSync`, and the identical private
/// copies in `DroidStar dmr.cpp: addDMRAudioSync`/`addDMRDataSync`: seven
/// bytes at `data + 13U`, each `(data[i] & ~SYNC_MASK[i]) | pattern[i]`.
/// [`Sync::None`] clears the field instead, which is what a burst about to
/// carry an EMB wants.
pub fn write_sync(burst: &mut [u8; BURST_LEN], sync: Sync) {
    let pattern = sync.pattern().unwrap_or([0u8; SYNC_LEN]);
    for i in 0..SYNC_LEN {
        burst[MIDDLE + i] = (burst[MIDDLE + i] & !SYNC_MASK[i]) | (pattern[i] & SYNC_MASK[i]);
    }
}

/// The three 72-bit AMBE frames, in wire order. Frame 2 straddles the
/// middle field: this puts it back together.
///
/// `DroidStar dmr.cpp: process_udp` — copy 14 bytes from the burst, mask byte
/// 13 to `0xF0`, OR in `burst[19] & 0x0F`, then copy burst bytes 20..32.
/// Corroborated bit for bit by `MMDVMHost/AMBEFEC.cpp: regenerateDMR`, which
/// addresses the frames at burst bits 0, 72 (skipping 108..155) and 192.
#[must_use]
pub fn ambe(burst: &[u8; BURST_LEN]) -> [[u8; AMBE_FRAME_BYTES]; AMBE_FRAMES] {
    let mut flat = [0u8; AMBE_BYTES];
    flat[..13].copy_from_slice(&burst[..13]);
    flat[13] = (burst[13] & 0xF0) | (burst[19] & 0x0F);
    flat[14..].copy_from_slice(&burst[20..]);

    let mut frames = [[0u8; AMBE_FRAME_BYTES]; AMBE_FRAMES];
    for (frame, chunk) in frames.iter_mut().zip(flat.chunks(AMBE_FRAME_BYTES)) {
        frame.copy_from_slice(chunk);
    }
    frames
}

/// Lay three 72-bit frames across the two information halves, the middle
/// field untouched.
///
/// `DroidStar dmr.cpp: send_frame` — 13 bytes to the burst, then
/// `ambe[13] & 0xF0` into byte 13 and `ambe[13] & 0x0F` into byte 19, then
/// `ambe[14..26]` to byte 20 onward. The two nibble writes are read-modify-
/// write on purpose: byte 13's low nibble and byte 19's high nibble are the
/// ends of the middle field, and clobbering either loses the sync or the EMB
/// while the audio still decodes — a failure that sounds fine and links to
/// nothing.
pub fn write_ambe(burst: &mut [u8; BURST_LEN], frames: &[[u8; AMBE_FRAME_BYTES]; AMBE_FRAMES]) {
    let mut flat = [0u8; AMBE_BYTES];
    for (chunk, frame) in flat.chunks_mut(AMBE_FRAME_BYTES).zip(frames) {
        chunk.copy_from_slice(frame);
    }

    burst[..13].copy_from_slice(&flat[..13]);
    burst[13] = (burst[13] & 0x0F) | (flat[13] & 0xF0);
    burst[19] = (burst[19] & 0xF0) | (flat[13] & 0x0F);
    burst[20..].copy_from_slice(&flat[14..]);
}

/// The EMB of a voice burst B-F: colour code and the embedded-LC fragment
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emb {
    /// The colour code, 0..15 — DMR's "which system is this", not a talkgroup.
    pub colour_code: u8,
    /// The PI (privacy indicator) flag. astar leaves it clear: it sends no
    /// PI header. `DroidStar dmr.cpp` has the line commented out entirely.
    pub pi: bool,
    /// Where this burst's embedded-LC fragment sits: 1 first, 3 continuation,
    /// 2 last, 0 none.
    pub lcss: u8,
}

/// The two EMB bytes: `EMB[0] = (cc << 4) | (PI ? 0x08 : 0) | (lcss << 1)`,
/// `EMB[1] = 0`, then QR(16,7,6) over the pair.
///
/// `MMDVMHost/DMREMB.cpp: CDMREMB::getData`; `DroidStar dmr.cpp: get_emb_data`
/// is the same. The low bit of `EMB[0]` is not data — QR(16,7,6) takes its
/// seven data bits from `(data[0] >> 1) & 0x7F`.
fn emb_bytes(emb: Emb) -> [u8; 2] {
    let mut bytes = [
        ((emb.colour_code << 4) & 0xF0) | u8::from(emb.pi) << 3 | ((emb.lcss << 1) & 0x06),
        0,
    ];
    fec::qr1676_encode(&mut bytes);
    bytes
}

/// The EMB of a voice burst B-F. `None` when the QR(16,7,6) check fails.
///
/// The eight bits at burst bits 108-115 and eight at 148-155:
/// `EMB[0]` is byte 13's low nibble then byte 14's high, `EMB[1]` is byte
/// 18's low nibble then byte 19's high. `MMDVMHost/DMREMB.cpp: putData`.
///
/// Refusing beyond two bit errors matters: the LCSS says where a fragment
/// belongs, and a wrong one reassembles the wrong LC into a confident,
/// wrong talker.
#[must_use]
pub fn emb(burst: &[u8; BURST_LEN]) -> Option<Emb> {
    let received = [
        ((burst[13] & 0x0F) << 4) | (burst[14] >> 4),
        ((burst[18] & 0x0F) << 4) | (burst[19] >> 4),
    ];
    let data = fec::qr1676_decode(&received)?;
    Some(Emb {
        colour_code: data >> 3,
        pi: data & 0x04 != 0,
        lcss: data & 0x03,
    })
}

/// Write an EMB into the two nibble pairs that flank the embedded-LC
/// fragment, leaving the fragment's own thirty-two bits alone — and with them
/// byte 13's high nibble and byte 19's low nibble, which are voice.
pub fn write_emb(burst: &mut [u8; BURST_LEN], emb: Emb) {
    let bytes = emb_bytes(emb);
    burst[13] = (burst[13] & 0xF0) | (bytes[0] >> 4);
    burst[14] = (burst[14] & 0x0F) | ((bytes[0] << 4) & 0xF0);
    burst[18] = (burst[18] & 0xF0) | (bytes[1] >> 4);
    burst[19] = (burst[19] & 0x0F) | ((bytes[1] << 4) & 0xF0);
}

/// The 32-bit embedded-LC fragment a burst carries: burst bits 116-147.
///
/// Byte 14's low nibble, bytes 15..17, byte 18's high nibble.
/// `MMDVMHost/DMREmbeddedData.cpp: CDMREmbeddedData::getData`;
/// `DroidStar dmr.cpp: get_embedded_data` is the same.
#[must_use]
pub fn embedded_fragment(burst: &[u8; BURST_LEN]) -> [bool; 32] {
    let mut out = [false; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let bit = 116 + i;
        *slot = burst[bit / 8] >> (7 - bit % 8) & 1 == 1;
    }
    out
}

/// Write those same 32 bits, leaving the EMB nibbles either side untouched.
pub fn write_embedded_fragment(burst: &mut [u8; BURST_LEN], fragment: &[bool; 32]) {
    for (i, &bit) in fragment.iter().enumerate() {
        let index = 116 + i;
        let mask = 1u8 << (7 - index % 8);
        let byte = &mut burst[index / 8];
        if bit {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}

/// Link Control: who is talking to whom.
///
/// `MMDVMHost/DMRLC.cpp: CDMRLC::getData`; `DroidStar dmr.cpp: lc_get_data`.
/// Nine bytes: FLCO, FID, service options, **destination** then **source**,
/// each a big-endian u24. Reversed, every last-heard display names the
/// talkgroup as the talker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkControl {
    /// Full Link Control Opcode. `MMDVMHost/DMRLC.cpp` reads it as
    /// `lc[0] & 0x3F`: bit 7 is PF (protect) and bit 6 R (reserved), and
    /// astar sends both clear. Held whole here so the byte round-trips.
    pub flco: u8,
    /// Feature set ID. [`FID_ETSI`] unless a vendor extension is in play.
    pub fid: u8,
    /// Service options — emergency, privacy, priority.
    pub options: u8,
    /// Talkgroup for [`FLCO_GROUP`], radio id for [`FLCO_USER_USER`].
    pub dst_id: u32,
    /// The talker's radio id, registered at radioid.net.
    pub src_id: u32,
}

impl LinkControl {
    /// The nine LC bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 9] {
        let dst = self.dst_id.to_be_bytes();
        let src = self.src_id.to_be_bytes();
        [
            self.flco,
            self.fid,
            self.options,
            dst[1],
            dst[2],
            dst[3],
            src[1],
            src[2],
            src[3],
        ]
    }

    /// The inverse. The top byte of each id is zero on the way back, and
    /// [`to_bytes`](Self::to_bytes) drops it on the way out: DMR ids are 24
    /// bits, and an id wider than that is refused where it enters —
    /// [`crate::wire::RadioId`] — not silently narrowed here.
    #[must_use]
    pub const fn from_bytes(bytes: &[u8; 9]) -> LinkControl {
        LinkControl {
            flco: bytes[0],
            fid: bytes[1],
            options: bytes[2],
            dst_id: u32::from_be_bytes([0, bytes[3], bytes[4], bytes[5]]),
            src_id: u32::from_be_bytes([0, bytes[6], bytes[7], bytes[8]]),
        }
    }
}

/// Decode a voice-LC-header or terminator burst's full LC: BPTC(196,96),
/// then RS(12,9) against the data type's mask. `None` when either fails.
///
/// The parity is stored **reversed** — `lc[9..12]` is `parity[2]`,
/// `parity[1]`, `parity[0]` — under [`VOICE_LC_HEADER_CRC_MASK`] or
/// [`TERMINATOR_WITH_LC_CRC_MASK`]. `MMDVMHost/DMRFullLC.cpp:
/// CDMRFullLC::encode`/`decode` and `DroidStar dmr.cpp: full_lc_encode` are
/// identical on this, and it is the easiest thing here to get backwards.
///
/// A data type that carries no LC gets `None` rather than a guess, and so
/// does the right burst read under the wrong mask: three parity symbols out
/// is past what RS(12,9) can repair, which is exactly the intended answer.
#[must_use]
pub fn full_lc(burst: &[u8; BURST_LEN], data_type: u8) -> Option<LinkControl> {
    let mask = match data_type {
        DT_VOICE_LC_HEADER => VOICE_LC_HEADER_CRC_MASK,
        DT_TERMINATOR_WITH_LC => TERMINATOR_WITH_LC_CRC_MASK,
        _ => return None,
    };
    let mut codeword = fec::bptc19696_decode(burst)?;
    for (byte, m) in codeword[9..].iter_mut().zip(mask) {
        *byte ^= m;
    }
    fec::rs129_decode(&codeword).map(|lc| LinkControl::from_bytes(&lc))
}

/// Reassemble the embedded LC from four consecutive fragments (bursts B-E).
///
/// `MMDVMHost/DMREmbeddedData.cpp: CDMREmbeddedData::addData` — `lcss == 1`
/// starts, two `lcss == 3` continue, `lcss == 2` finishes and triggers the
/// decode. This is how a late-joining receiver learns who is talking without
/// waiting for the next header.
///
/// It refuses to start mid-sequence, and a run that does not arrive as
/// 1-3-3-2 is dropped rather than decoded from whatever is held: a
/// plausible-looking wrong source id is worse than no talker at all. Feed it
/// every burst of the timeslot — [`push`](Self::push) recognises the ones
/// that carry a sync pattern instead of an EMB and ends the run on them.
#[derive(Debug)]
pub struct EmbeddedLcAssembler {
    raw: [bool; 128],
    held: usize,
}

impl Default for EmbeddedLcAssembler {
    fn default() -> EmbeddedLcAssembler {
        EmbeddedLcAssembler::new()
    }
}

impl EmbeddedLcAssembler {
    /// A fresh assembler, holding nothing.
    #[must_use]
    pub fn new() -> EmbeddedLcAssembler {
        EmbeddedLcAssembler {
            raw: [false; 128],
            held: 0,
        }
    }

    /// Feed one burst — **any** burst of the timeslot, sync or voice.
    /// Returns the LC on the fragment that completes it.
    ///
    /// Bursts carrying a sync pattern are not fragments and are rejected
    /// before their middle field is read at all, because a sync pattern read
    /// as an EMB is not reliably nonsense: QR(16,7,6) over
    /// `MS_SOURCED_AUDIO_SYNC`'s EMB nibbles decodes as a valid
    /// `cc = 7, lcss = 3` continuation, and over `MS_SOURCED_DATA_SYNC`'s as
    /// a valid `cc = 13, lcss = 2` *last fragment*. Without this guard a
    /// stream truncated after burst D, followed by the next stream's
    /// voice-LC-header burst, would push 32 bits of the sync pattern into the
    /// fourth slot and hand the result to BPTC(128,77) — which is then the
    /// only thing standing between the operator and a wrong talker. Dropping
    /// a torn sequence has to be by construction, not by probability.
    pub fn push(&mut self, burst: &[u8; BURST_LEN]) -> Option<LinkControl> {
        // Burst A and every signalling burst put a sync pattern where the EMB
        // would be. Neither carries a fragment, and both end whatever run was
        // in progress.
        if sync_of(burst) != Sync::None {
            self.reset();
            return None;
        }
        // No readable EMB means no LCSS, and an LCSS guessed from a failed
        // QR check is how the wrong fragment lands in the right slot.
        let Some(emb) = emb(burst) else {
            self.reset();
            return None;
        };
        let expected = match emb.lcss {
            // First fragment: always accepted, always restarts.
            1 => 0,
            // Continuation: only after the first and only twice.
            3 => {
                if self.held == 1 || self.held == 2 {
                    self.held
                } else {
                    self.reset();
                    return None;
                }
            }
            // Last fragment: only after all three of the others.
            2 => {
                if self.held == 3 {
                    3
                } else {
                    self.reset();
                    return None;
                }
            }
            // 0 is burst F, which carries no fragment; anything else is not
            // a state this sequence has. Neither disturbs what is held.
            _ => return None,
        };

        self.raw[expected * 32..(expected + 1) * 32].copy_from_slice(&embedded_fragment(burst));
        self.held = expected + 1;
        if self.held < 4 {
            return None;
        }

        let raw = self.raw;
        self.reset();
        fec::bptc12877_decode(&raw).map(|lc| LinkControl::from_bytes(&lc))
    }

    /// Forget what is held — a new transmission, or a gap in the superframe.
    pub fn reset(&mut self) {
        self.raw = [false; 128];
        self.held = 0;
    }
}

/// The slot type of a signalling burst: colour code and data type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotType {
    /// The colour code, 0..15.
    pub colour_code: u8,
    /// One of the `DT_*` code points.
    pub data_type: u8,
}

/// The twenty Golay(20,8) codeword bits, left-aligned in three bytes, that a
/// slot type's `(cc << 4) | dataType` byte becomes.
fn slot_type_codeword(slot: SlotType) -> [u8; 3] {
    let mut bytes = [
        ((slot.colour_code << 4) & 0xF0) | (slot.data_type & 0x0F),
        0,
        0,
    ];
    fec::golay2087_encode(&mut bytes);
    bytes
}

/// The slot type of a signalling burst. `None` when Golay(20,8) cannot
/// recover it.
///
/// `MMDVMHost/DMRSlotType.cpp: CDMRSlotType::putData` — the codeword's twenty
/// bits sit contiguously either side of the sync field: bits 19..14 in burst
/// byte 12's low six, 13..10 in byte 13's high nibble, 9..6 in byte 19's low
/// nibble, 5..0 in byte 20's top six. Everything it does not touch is the
/// sync field or BPTC(196,96)'s two stray payload bits.
#[must_use]
pub fn slot_type(burst: &[u8; BURST_LEN]) -> Option<SlotType> {
    let codeword = [
        ((burst[12] & 0x3F) << 2) | ((burst[13] & 0xC0) >> 6),
        ((burst[13] & 0x30) << 2) | ((burst[19] & 0x0F) << 2) | ((burst[20] & 0xC0) >> 6),
        (burst[20] & 0x3C) << 2,
    ];
    let data = fec::golay2087_decode(&codeword)?;
    Some(SlotType {
        colour_code: data >> 4,
        data_type: data & 0x0F,
    })
}

/// Write a slot type into the ten bits either side of the sync field.
///
/// `MMDVMHost/DMRSlotType.cpp: CDMRSlotType::getData`; `DroidStar dmr.cpp:
/// get_slot_data` performs the identical four masked writes. Every one is
/// read-modify-write: byte 12's top two bits and byte 20's low two are
/// BPTC(196,96) payload, byte 13's low nibble and byte 19's high nibble are
/// the sync field.
pub fn write_slot_type(burst: &mut [u8; BURST_LEN], slot: SlotType) {
    let codeword = slot_type_codeword(slot);
    burst[12] = (burst[12] & 0xC0) | (codeword[0] >> 2);
    burst[13] = (burst[13] & 0x0F) | ((codeword[0] << 6) & 0xC0) | ((codeword[1] >> 2) & 0x30);
    burst[19] = (burst[19] & 0xF0) | ((codeword[1] >> 2) & 0x0F);
    burst[20] = (burst[20] & 0x03) | ((codeword[1] << 6) & 0xC0) | ((codeword[2] >> 2) & 0x3C);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_middle_field_is_the_forty_eight_bits_between_the_information_halves() {
        // MMDVMHost/DMRDefines.h: SYNC_MASK = 0F FF FF FF FF FF F0 applied at
        // data + 13U for seven bytes -- burst byte 13's low nibble through
        // byte 19's high nibble. The flanking nibbles are voice.
        let mut burst = [0xFFu8; BURST_LEN];
        write_sync(&mut burst, Sync::MsAudio);
        assert_eq!(middle(&burst), MS_SOURCED_AUDIO_SYNC);
        assert_eq!(burst[13] & 0xF0, 0xF0, "the voice nibble at 13 survived");
        assert_eq!(burst[19] & 0x0F, 0x0F, "the voice nibble at 19 survived");
    }

    #[test]
    fn every_published_sync_pattern_round_trips_and_nothing_else_matches() {
        for (want, bytes) in [
            (Sync::MsAudio, MS_SOURCED_AUDIO_SYNC),
            (Sync::MsData, MS_SOURCED_DATA_SYNC),
            (Sync::BsAudio, BS_SOURCED_AUDIO_SYNC),
            (Sync::BsData, BS_SOURCED_DATA_SYNC),
            (Sync::DirectSlot1Audio, DIRECT_SLOT1_AUDIO_SYNC),
            (Sync::DirectSlot1Data, DIRECT_SLOT1_DATA_SYNC),
            (Sync::DirectSlot2Audio, DIRECT_SLOT2_AUDIO_SYNC),
            (Sync::DirectSlot2Data, DIRECT_SLOT2_DATA_SYNC),
        ] {
            let mut burst = [0u8; BURST_LEN];
            write_sync(&mut burst, want);
            assert_eq!(sync_of(&burst), want);
            assert_eq!(middle(&burst), bytes);
        }
        let mut burst = [0u8; BURST_LEN];
        burst[15] = 0x12;
        assert_eq!(sync_of(&burst), Sync::None);
    }

    #[test]
    fn astar_writes_the_ms_patterns_because_astar_is_a_mobile_station() {
        // DroidStar dmr.cpp: addDMRAudioSync / addDMRDataSync take
        // `duplex = 0`, whose branch is MS_SOURCED_*. A softclient is not a
        // base station and must not claim to be one.
        assert_ne!(MS_SOURCED_AUDIO_SYNC, BS_SOURCED_AUDIO_SYNC);
        assert_ne!(MS_SOURCED_DATA_SYNC, BS_SOURCED_DATA_SYNC);
    }

    #[test]
    fn three_ambe_frames_round_trip_and_the_middle_frame_straddles_the_sync() {
        // DroidStar dmr.cpp: send_frame writes ambe[0..12] to burst[0..12],
        // ambe[13]'s high nibble to burst[13], its LOW nibble to burst[19],
        // and ambe[14..26] to burst[20..32]; process_udp reads exactly that
        // back. Frame 2 is bits 72..143 of the AMBE stream, so it spans the
        // 48-bit middle field -- the detail research-dmr.md flagged as
        // unverified and this pins.
        //
        // The 27 AMBE bytes are three 9-byte frames at 0..8, 9..17 and
        // 18..26 (docs/design/dmr-wire.md §5), so frame 2 has *already*
        // started by burst byte 9: burst bytes 9..12 and byte 13's high
        // nibble are its first 36 bits, and byte 19's low nibble with bytes
        // 20..23 are the rest.
        let frames: [[u8; 9]; 3] = [[0x11; 9], [0x22; 9], [0x33; 9]];
        let mut burst = [0u8; BURST_LEN];
        write_sync(&mut burst, Sync::MsAudio);
        write_ambe(&mut burst, &frames);
        assert_eq!(ambe(&burst), frames);
        assert_eq!(
            sync_of(&burst),
            Sync::MsAudio,
            "the sync field survived the voice write"
        );
        assert_eq!(
            &burst[..13],
            &[
                0x11u8, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22
            ][..]
        );
        assert_eq!(burst[13] & 0xF0, 0x20);
        assert_eq!(burst[19] & 0x0F, 0x02);
        assert_eq!(
            &burst[20..33],
            &[
                0x22u8, 0x22, 0x22, 0x22, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33
            ][..]
        );
    }

    #[test]
    fn writing_voice_never_disturbs_the_middle_field_and_the_reverse() {
        // The two writers overlap in bytes 13 and 19 at nibble granularity.
        // If either clobbered the other's nibble the audio would decode and
        // the sync would not, or the sync would match and every burst would
        // carry one corrupted AMBE bit -- both hard to see, both fatal.
        let frames: [[u8; 9]; 3] = [[0xAB; 9], [0xCD; 9], [0xEF; 9]];
        let mut burst = [0u8; BURST_LEN];
        write_ambe(&mut burst, &frames);
        write_sync(&mut burst, Sync::MsData);
        assert_eq!(ambe(&burst), frames);
        assert_eq!(sync_of(&burst), Sync::MsData);
    }

    #[test]
    fn an_emb_round_trips_its_colour_code_and_lcss() {
        // DroidStar dmr.cpp: get_emb_data -- EMB[0] = (cc << 4) | (lcss << 1),
        // EMB[1] = 0, QR(16,7,6) over the pair, then eight bits at burst bits
        // 108-115 and eight at 148-155.
        for cc in 0u8..16 {
            for lcss in 0u8..4 {
                let mut burst = [0u8; BURST_LEN];
                write_emb(
                    &mut burst,
                    Emb {
                        colour_code: cc,
                        pi: false,
                        lcss,
                    },
                );
                assert_eq!(
                    emb(&burst),
                    Some(Emb {
                        colour_code: cc,
                        pi: false,
                        lcss
                    })
                );
            }
        }
    }

    #[test]
    fn an_emb_and_a_fragment_share_the_middle_field_without_colliding() {
        let mut burst = [0u8; BURST_LEN];
        let mut fragment = [false; 32];
        for (i, bit) in fragment.iter_mut().enumerate() {
            *bit = i % 3 == 0;
        }
        write_embedded_fragment(&mut burst, &fragment);
        write_emb(
            &mut burst,
            Emb {
                colour_code: 1,
                pi: false,
                lcss: 1,
            },
        );
        assert_eq!(embedded_fragment(&burst), fragment);
        assert_eq!(
            emb(&burst),
            Some(Emb {
                colour_code: 1,
                pi: false,
                lcss: 1
            })
        );
    }

    fn lc() -> LinkControl {
        LinkControl {
            flco: FLCO_GROUP,
            fid: FID_ETSI,
            options: 0,
            dst_id: 31_313,
            src_id: 3_153_591,
        }
    }

    #[test]
    fn link_control_bytes_are_flco_fid_options_then_dst_then_src() {
        // DroidStar dmr.cpp: lc_get_data -- bytes[3..6] is the DESTINATION
        // and bytes[6..9] the SOURCE, in that order. Reversed, every last-heard
        // display names the talkgroup as the talker.
        let bytes = lc().to_bytes();
        assert_eq!(bytes[0], FLCO_GROUP);
        assert_eq!(bytes[1], FID_ETSI);
        assert_eq!(&bytes[3..6], &[0x00, 0x7A, 0x51]);
        assert_eq!(&bytes[6..9], &[0x30, 0x1E, 0xB7]);
        assert_eq!(LinkControl::from_bytes(&bytes), lc());
    }

    #[test]
    fn an_embedded_lc_reassembles_from_four_fragments() {
        // MMDVMHost/DMREmbeddedData.cpp: the 128-bit BPTC matrix is read out
        // 32 bits at a time across bursts B-E. This is how a late-joining
        // receiver learns who is talking without waiting for the next header.
        let raw = crate::fec::bptc12877_encode(&lc().to_bytes());
        let mut assembler = EmbeddedLcAssembler::new();
        let mut got = None;
        for n in 0..4usize {
            let mut burst = [0u8; BURST_LEN];
            let mut fragment = [false; 32];
            fragment.copy_from_slice(&raw[n * 32..(n + 1) * 32]);
            write_embedded_fragment(&mut burst, &fragment);
            let lcss = match n {
                0 => 1,
                3 => 2,
                _ => 3,
            };
            write_emb(
                &mut burst,
                Emb {
                    colour_code: 1,
                    pi: false,
                    lcss,
                },
            );
            got = assembler.push(&burst).or(got);
        }
        assert_eq!(got, Some(lc()));
    }

    #[test]
    fn a_torn_embedded_lc_yields_nothing_rather_than_a_wrong_talker() {
        // Missing the first fragment and starting mid-sequence must NOT
        // assemble: a plausible-looking wrong source id is worse than none.
        let raw = crate::fec::bptc12877_encode(&lc().to_bytes());
        let mut assembler = EmbeddedLcAssembler::new();
        for n in 1..4usize {
            let mut burst = [0u8; BURST_LEN];
            let mut fragment = [false; 32];
            fragment.copy_from_slice(&raw[n * 32..(n + 1) * 32]);
            write_embedded_fragment(&mut burst, &fragment);
            write_emb(
                &mut burst,
                Emb {
                    colour_code: 1,
                    pi: false,
                    lcss: if n == 3 { 2 } else { 3 },
                },
            );
            assert_eq!(assembler.push(&burst), None);
        }
    }

    /// A burst carrying one 32-bit slice of `raw` and the EMB that says where
    /// it belongs.
    fn fragment_burst(raw: &[bool; 128], n: usize, lcss: u8) -> [u8; BURST_LEN] {
        let mut burst = [0u8; BURST_LEN];
        let mut fragment = [false; 32];
        fragment.copy_from_slice(&raw[n * 32..(n + 1) * 32]);
        write_embedded_fragment(&mut burst, &fragment);
        write_emb(
            &mut burst,
            Emb {
                colour_code: 1,
                pi: false,
                lcss,
            },
        );
        burst
    }

    /// A real voice-LC-header burst: MS data sync, slot type, full LC.
    fn voice_lc_header_burst() -> [u8; BURST_LEN] {
        let mut burst = [0u8; BURST_LEN];
        let mut payload = [0u8; 12];
        payload[..9].copy_from_slice(&lc().to_bytes());
        let parity = crate::fec::rs129_parity(&lc().to_bytes());
        payload[9] = parity[2] ^ VOICE_LC_HEADER_CRC_MASK[0];
        payload[10] = parity[1] ^ VOICE_LC_HEADER_CRC_MASK[1];
        payload[11] = parity[0] ^ VOICE_LC_HEADER_CRC_MASK[2];
        crate::fec::bptc19696_encode(&payload, &mut burst);
        write_sync(&mut burst, Sync::MsData);
        write_slot_type(
            &mut burst,
            SlotType {
                colour_code: 1,
                data_type: DT_VOICE_LC_HEADER,
            },
        );
        burst
    }

    #[test]
    fn a_sync_pattern_read_as_an_emb_is_not_reliably_nonsense() {
        // Why the guard in push() has to exist. Six of the eight patterns fail
        // the QR(16,7,6) check in the EMB nibble positions, and two do not:
        // MS_SOURCED_AUDIO_SYNC reads as a valid continuation and
        // MS_SOURCED_DATA_SYNC -- which every signalling burst carries -- as a
        // valid LAST fragment, the one LCSS that triggers a decode.
        let mut audio = [0u8; BURST_LEN];
        write_sync(&mut audio, Sync::MsAudio);
        assert_eq!(
            emb(&audio),
            Some(Emb {
                colour_code: 7,
                pi: false,
                lcss: 3
            })
        );
        assert_eq!(
            emb(&voice_lc_header_burst()),
            Some(Emb {
                colour_code: 13,
                pi: false,
                lcss: 2
            })
        );
        for sync in [
            Sync::BsAudio,
            Sync::BsData,
            Sync::DirectSlot1Audio,
            Sync::DirectSlot1Data,
            Sync::DirectSlot2Audio,
            Sync::DirectSlot2Data,
        ] {
            let mut burst = [0u8; BURST_LEN];
            write_sync(&mut burst, sync);
            assert_eq!(emb(&burst), None, "{sync:?}");
        }
    }

    #[test]
    fn a_sync_burst_ends_a_run_rather_than_being_taken_for_a_fragment() {
        // The scenario the guard is for. A run in progress, then the next
        // transmission starts: burst A carries a sync pattern, and two of the
        // eight read back through QR(16,7,6) as a valid LCSS. Whether the
        // resulting matrix happens to fail BPTC(128,77) depends on the LC,
        // which is no guarantee at all -- so the count of fragments held is
        // what this asserts, not just the absence of an answer.
        let raw = crate::fec::bptc12877_encode(&lc().to_bytes());
        let mut assembler = EmbeddedLcAssembler::new();

        assert_eq!(assembler.push(&fragment_burst(&raw, 0, 1)), None);
        assert_eq!(assembler.held, 1);
        let mut audio = [0u8; BURST_LEN];
        write_sync(&mut audio, Sync::MsAudio);
        assert_eq!(assembler.push(&audio), None);
        assert_eq!(
            assembler.held, 0,
            "MS_SOURCED_AUDIO_SYNC reads as lcss = 3 and must not be taken \
             for the second fragment"
        );

        // Now the worse one: three fragments held, and the next stream's
        // voice-LC-header burst arrives carrying MS_SOURCED_DATA_SYNC, which
        // reads as lcss = 2 -- the LAST fragment, the one that decodes.
        for (n, lcss) in [(0usize, 1u8), (1, 3), (2, 3)] {
            assert_eq!(assembler.push(&fragment_burst(&raw, n, lcss)), None);
        }
        assert_eq!(assembler.held, 3);
        assert_eq!(assembler.push(&voice_lc_header_burst()), None);
        assert_eq!(
            assembler.held, 0,
            "a signalling burst must not fill the fourth slot"
        );

        // A clean B-E after all that still decodes, so the guard cost nothing.
        let mut got = None;
        for (n, lcss) in [(0usize, 1u8), (1, 3), (2, 3), (3, 2)] {
            got = assembler.push(&fragment_burst(&raw, n, lcss)).or(got);
        }
        assert_eq!(got, Some(lc()));
        assert_eq!(assembler.held, 0);
    }

    #[test]
    fn a_voice_lc_header_burst_decodes_to_its_link_control() {
        let mut burst = [0u8; BURST_LEN];
        write_sync(&mut burst, Sync::MsData);
        let mut payload = [0u8; 12];
        payload[..9].copy_from_slice(&lc().to_bytes());
        let parity = crate::fec::rs129_parity(&lc().to_bytes());
        payload[9] = parity[2] ^ VOICE_LC_HEADER_CRC_MASK[0];
        payload[10] = parity[1] ^ VOICE_LC_HEADER_CRC_MASK[1];
        payload[11] = parity[0] ^ VOICE_LC_HEADER_CRC_MASK[2];
        crate::fec::bptc19696_encode(&payload, &mut burst);
        assert_eq!(full_lc(&burst, DT_VOICE_LC_HEADER), Some(lc()));
        // The terminator mask is different, so reading it as a terminator
        // must fail rather than return a plausible LC.
        assert_eq!(full_lc(&burst, DT_TERMINATOR_WITH_LC), None);
        // And a data type that carries no LC at all has no answer to give.
        assert_eq!(full_lc(&burst, DT_CSBK), None);
    }

    #[test]
    fn a_slot_type_round_trips_beside_the_sync_field_it_flanks() {
        // docs/design/dmr-wire.md §7: (cc << 4) | dataType through
        // Golay(20,8), then the twenty codeword bits laid contiguously into
        // burst byte 12's low six, byte 13's high nibble, byte 19's low
        // nibble and byte 20's top six -- everything the sync field and the
        // BPTC payload do not own. MMDVMHost/DMRSlotType.cpp: getData.
        for cc in 0u8..16 {
            for data_type in [
                DT_VOICE_PI_HEADER,
                DT_VOICE_LC_HEADER,
                DT_TERMINATOR_WITH_LC,
                DT_CSBK,
                DT_DATA_HEADER,
                DT_IDLE,
            ] {
                let mut burst = [0u8; BURST_LEN];
                write_sync(&mut burst, Sync::MsData);
                write_slot_type(
                    &mut burst,
                    SlotType {
                        colour_code: cc,
                        data_type,
                    },
                );
                assert_eq!(
                    slot_type(&burst),
                    Some(SlotType {
                        colour_code: cc,
                        data_type
                    })
                );
                assert_eq!(sync_of(&burst), Sync::MsData, "cc {cc} type {data_type}");
            }
        }
    }

    #[test]
    fn a_slot_type_and_a_full_lc_share_a_signalling_burst() {
        // The slot type owns burst byte 12's low six bits and byte 20's top
        // six; BPTC(196,96) owns byte 12's top two and byte 20's low two.
        // Neither may write the other's, in either order.
        let mut burst = [0u8; BURST_LEN];
        let mut payload = [0u8; 12];
        payload[..9].copy_from_slice(&lc().to_bytes());
        let parity = crate::fec::rs129_parity(&lc().to_bytes());
        payload[9] = parity[2] ^ VOICE_LC_HEADER_CRC_MASK[0];
        payload[10] = parity[1] ^ VOICE_LC_HEADER_CRC_MASK[1];
        payload[11] = parity[0] ^ VOICE_LC_HEADER_CRC_MASK[2];
        crate::fec::bptc19696_encode(&payload, &mut burst);
        write_sync(&mut burst, Sync::MsData);
        write_slot_type(
            &mut burst,
            SlotType {
                colour_code: 1,
                data_type: DT_VOICE_LC_HEADER,
            },
        );
        assert_eq!(
            slot_type(&burst),
            Some(SlotType {
                colour_code: 1,
                data_type: DT_VOICE_LC_HEADER
            })
        );
        assert_eq!(sync_of(&burst), Sync::MsData);
        assert_eq!(full_lc(&burst, DT_VOICE_LC_HEADER), Some(lc()));
    }
}
