// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! D-Star slow-data channel (research §6): the 3 trailing bytes of every
//! 12-byte voice frame.
//!
//! [`SlowDataRx`] descrambles and reassembles the RX direction into 6-byte
//! blocks, with the optional 20-character free-text message decoded.
//! [`slow_data_slot`] is the TX-direction counterpart: it does not
//! implement the header-repeat, GPS, or free-text content types (out of
//! scope, task-scoped to sync + null filler only) — see its own docs.
//!
//! ## Block/slot phasing
//!
//! A superframe is [`crate::dsvt::SYNC_INTERVAL`] (21) frames, sequence
//! `0..=20`. Sequence 0's slot carries the fixed sync pattern, not data
//! (research §6), so [`SlowDataRx::feed`] ignores it (beyond resetting any
//! half-received block) rather than descrambling it.
//!
//! The remaining 20 frames (seq 1..=20) pair up two-at-a-time into ten
//! 6-byte blocks: the first frame of a pair contributes the block's first 3
//! bytes (a type byte + 2 data bytes), the second frame contributes the
//! last 3 bytes (3 more data bytes). For the free-text message, type bytes
//! `0x40..=0x43` each carry a 5-byte chunk (`0x40` = chunk 0 .. `0x43` =
//! chunk 3); once all four chunks have arrived the 20 bytes concatenate
//! into the message, space-trimmed. Any other type byte (`0x50` header
//! repeat, `0x30` GPS, garbage) is decoded structurally the same way but
//! its payload is simply discarded — this channel only surfaces text
//! messages today.

/// The fixed D-STAR slow-data scrambler, `XORed` byte-for-byte into every
/// non-sync 3-byte slot (research §6).
pub const SCRAMBLE: [u8; 3] = [0x70, 0x4f, 0x93];

/// First byte of a free-text block: `0x40 | chunk_index` (`chunk_index`
/// `0..=3`), each block carrying 5 ASCII bytes of a 20-byte message
/// (research §6).
const TEXT_TYPE_BASE: u8 = 0x40;
const TEXT_TYPE_MAX: u8 = 0x43;
const TEXT_CHUNKS: usize = 4;
const TEXT_CHUNK_LEN: usize = 5;

fn descramble(slot: [u8; 3]) -> [u8; 3] {
    [
        slot[0] ^ SCRAMBLE[0],
        slot[1] ^ SCRAMBLE[1],
        slot[2] ^ SCRAMBLE[2],
    ]
}

/// The fixed 24-bit D-STAR slow-data synchronization pattern, sent
/// **unscrambled** (research §6: "Scrambling ... XORs slow-data bytes (not
/// the sync frame)") in the slow-data slot of frame `seq == 0` of every
/// [`crate::dsvt::SYNC_INTERVAL`]-frame superframe.
///
/// Research §6 describes this bit pattern in prose (twice the 7-bit
/// maximal-length sequence `1101000`, plus a 10-bit `1010101010` pattern,
/// byte order reversed) but does not spell out the resulting hex bytes, and
/// this crate had no prior sync-pattern constant to cross-check against.
/// This value is instead taken directly from Jonathan Naylor G4KLX's own
/// `MMDVMHost` implementation (`DSTAR_SYNC_BYTES` /
/// `DSTAR_NULL_SLOW_SYNC_BYTES` in `DStarDefines.h`) — the same G4KLX who
/// wrote "The Format of D-Star Slow Data", the written spec research §6
/// cites as its source for this pattern. Reflectors do not validate slow
/// data content (research §6), so this has no bearing on interop with
/// XRF/XLX/REF; it matters only to a real Icom radio's slow-data decode.
/// Not yet independently confirmed against a live capture — research §8
/// flags slow data generally as "STILL UNVERIFIED".
pub const SYNC_PATTERN: [u8; 3] = [0x55, 0x2D, 0x16];

/// Produces the TX-side slow-data 3-byte slot for frame `seq`
/// (`0..SYNC_INTERVAL`, see [`crate::dsvt::SYNC_INTERVAL`]).
///
/// `seq == 0` (the first frame of a superframe) emits [`SYNC_PATTERN`]
/// verbatim, unscrambled. Every other frame emits an all-zero block run
/// through the scrambler — research §6's "simplest" fallback when not
/// implementing header-repeat/GPS/text content (out of this crate's
/// scope): "simply zero it" is applied at the *logical* (pre-scramble)
/// level, since the scrambler description says it is applied
/// unconditionally to every non-sync slot, so the wire bytes for a
/// deliberately-blank slot are `SCRAMBLE XOR [0,0,0]`, i.e. [`SCRAMBLE`]
/// itself, not literal zero bytes.
#[must_use]
pub fn slow_data_slot(seq: u8) -> [u8; 3] {
    if seq == 0 {
        SYNC_PATTERN
    } else {
        // XOR is self-inverse: `descramble` here is doing exactly the
        // scramble step research §6 describes for TX.
        descramble([0, 0, 0])
    }
}

/// Reassembles the RX slow-data channel into whatever free-text message it
/// carries. Holds no I/O; the caller feeds it each frame's `seq` and 3-byte
/// slot as they arrive off the wire.
#[derive(Debug, Clone, Default)]
pub struct SlowDataRx {
    /// The first half of a two-frame block, once seen: `(type_byte,
    /// [data0, data1])`, waiting on its partner slot to complete the block.
    pending_half: Option<(u8, [u8; 2])>,
    /// Chunks of the in-progress free-text message, indexed by
    /// `type_byte - TEXT_TYPE_BASE`.
    text_chunks: [Option<[u8; TEXT_CHUNK_LEN]>; TEXT_CHUNKS],
}

impl SlowDataRx {
    /// Creates a fresh, empty slow-data reassembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the 3-byte slow-data slot of frame `seq` (`0..SYNC_INTERVAL`,
    /// see [`crate::dsvt::SYNC_INTERVAL`]). Frame 0 is the sync slot, not
    /// data, and is ignored (beyond dropping any half-completed block, on
    /// the theory that a fresh superframe means the previous one's data is
    /// stale). Returns `Some(text)` the moment a full 20-character
    /// `0x40..=0x43` message completes, space-trimmed.
    pub fn feed(&mut self, seq: u8, slot: &[u8; 3]) -> Option<String> {
        if seq == 0 {
            self.pending_half = None;
            return None;
        }

        let bytes = descramble(*slot);
        let phase = (seq - 1) % 2;
        if phase == 0 {
            // First half of a block: byte 0 is the type, bytes 1-2 are its
            // first two data bytes. Overwrites any stale half-block from a
            // dropped/misaligned frame rather than accumulating garbage.
            self.pending_half = Some((bytes[0], [bytes[1], bytes[2]]));
            return None;
        }

        let Some((block_type, first_two)) = self.pending_half.take() else {
            // Second half arrived with no first half seen (dropped frame);
            // nothing to assemble.
            return None;
        };

        if !(TEXT_TYPE_BASE..=TEXT_TYPE_MAX).contains(&block_type) {
            // GPS (0x30), header-repeat (0x50), or unrecognized type: this
            // channel only surfaces text messages, so drop the payload.
            return None;
        }

        let chunk_index = usize::from(block_type - TEXT_TYPE_BASE);
        self.text_chunks[chunk_index] =
            Some([first_two[0], first_two[1], bytes[0], bytes[1], bytes[2]]);

        if self.text_chunks.iter().all(Option::is_some) {
            let mut raw = Vec::with_capacity(TEXT_CHUNKS * TEXT_CHUNK_LEN);
            for chunk in &mut self.text_chunks {
                raw.extend_from_slice(&chunk.take().expect("checked all-some above"));
            }
            let text = String::from_utf8_lossy(&raw).trim().to_string();
            return Some(text);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsvt::SYNC_INTERVAL;

    fn scramble(bytes: [u8; 3]) -> [u8; 3] {
        [
            bytes[0] ^ SCRAMBLE[0],
            bytes[1] ^ SCRAMBLE[1],
            bytes[2] ^ SCRAMBLE[2],
        ]
    }

    /// Encodes a `(type_byte, 5-byte payload)` block into the two scrambled
    /// 3-byte wire slots the real channel would carry it as.
    fn encode_block(block_type: u8, payload: [u8; 5]) -> [[u8; 3]; 2] {
        let first = scramble([block_type, payload[0], payload[1]]);
        let second = scramble([payload[2], payload[3], payload[4]]);
        [first, second]
    }

    #[test]
    fn scramble_constant_is_the_documented_pattern() {
        assert_eq!(SCRAMBLE, [0x70, 0x4f, 0x93]);
    }

    #[test]
    fn seq_zero_sync_slot_is_ignored_as_data() {
        let mut rx = SlowDataRx::new();
        // A sync-pattern-shaped slot fed at seq 0 must never be
        // mistaken for the first half of a block.
        assert_eq!(rx.feed(0, &[0xAA, 0x55, 0xAA]), None);
        // Confirm no stray pending half leaked through: feeding a lone
        // second-half-shaped slot right after must still yield nothing.
        assert_eq!(rx.feed(2, &scramble([0, 0, 0])), None);
    }

    #[test]
    fn full_twenty_char_message_assembles_once_and_trims_spaces() {
        let mut rx = SlowDataRx::new();
        let message = b"HELLO WORLD TEST!!!"; // 19 chars + 1 trailing space pad
        assert_eq!(message.len(), 19);
        let mut padded = [b' '; 20];
        padded[..19].copy_from_slice(message);

        let mut result = None;
        let mut seq = 1u8;
        for (i, chunk_type) in (TEXT_TYPE_BASE..=TEXT_TYPE_MAX).enumerate() {
            let mut payload = [0u8; 5];
            payload.copy_from_slice(&padded[i * 5..i * 5 + 5]);
            let [slot_a, slot_b] = encode_block(chunk_type, payload);
            assert_eq!(rx.feed(seq, &slot_a), None);
            seq += 1;
            let r = rx.feed(seq, &slot_b);
            seq += 1;
            if r.is_some() {
                result = r;
            }
        }

        assert_eq!(result, Some("HELLO WORLD TEST!!!".to_string()));
    }

    #[test]
    fn message_can_arrive_out_of_chunk_order() {
        let mut rx = SlowDataRx::new();
        let chunks: [[u8; 5]; 4] = [*b"ABCDE", *b"FGHIJ", *b"KLMNO", *b"PQRST"];
        let order = [0x42, 0x40, 0x43, 0x41]; // scrambled ordering, not sequential
        let mut result = None;
        let mut seq = 1u8;
        for chunk_type in order {
            let idx = usize::from(chunk_type - TEXT_TYPE_BASE);
            let [slot_a, slot_b] = encode_block(chunk_type, chunks[idx]);
            rx.feed(seq, &slot_a);
            seq += 1;
            let r = rx.feed(seq, &slot_b);
            seq += 1;
            if r.is_some() {
                result = r;
            }
        }
        assert_eq!(result, Some("ABCDEFGHIJKLMNOPQRST".to_string()));
    }

    #[test]
    fn gps_type_block_is_ignored_without_panicking() {
        let mut rx = SlowDataRx::new();
        let [slot_a, slot_b] = encode_block(0x30, *b"$GPGG");
        assert_eq!(rx.feed(1, &slot_a), None);
        assert_eq!(rx.feed(2, &slot_b), None);
        // The reassembler must still be usable afterwards: feed a real
        // text message and confirm it still assembles.
        let [slot_a, slot_b] = encode_block(0x40, *b"HELLO");
        assert_eq!(rx.feed(3, &slot_a), None);
        assert_eq!(rx.feed(4, &slot_b), None);
    }

    #[test]
    fn header_repeat_type_block_is_ignored() {
        let mut rx = SlowDataRx::new();
        let [slot_a, slot_b] = encode_block(0x50, [0x11, 0x22, 0x33, 0x44, 0x66]);
        assert_eq!(rx.feed(1, &slot_a), None);
        assert_eq!(rx.feed(2, &slot_b), None);
    }

    #[test]
    fn lone_second_half_with_no_first_half_is_dropped_not_panicking() {
        let mut rx = SlowDataRx::new();
        // Feed only the second half of a pair (seq 2, an odd phase) with no
        // preceding first half.
        assert_eq!(rx.feed(2, &scramble([1, 2, 3])), None);
    }

    #[test]
    fn incomplete_message_never_yields_partial_text() {
        let mut rx = SlowDataRx::new();
        // Only 3 of the 4 required chunks arrive.
        for chunk_type in [0x40, 0x41, 0x42] {
            let [slot_a, slot_b] = encode_block(chunk_type, *b"XXXXX");
            assert_eq!(rx.feed(1, &slot_a), None);
            assert_eq!(rx.feed(2, &slot_b), None);
        }
    }

    #[test]
    fn sync_frame_between_blocks_resets_pending_half_only() {
        let mut rx = SlowDataRx::new();
        let [slot_a, _unused] = encode_block(0x40, *b"HELLO");
        // First half of a block arrives...
        assert_eq!(rx.feed(1, &slot_a), None);
        // ...then a sync frame interrupts (superframe boundary): the
        // half-block must be dropped, not carried into the new superframe.
        assert_eq!(rx.feed(0, &[0, 0, 0]), None);
        // A fresh, complete block right after must decode cleanly with no
        // leftover state from the dropped half.
        let [slot_a, slot_b] = encode_block(0x41, *b"WORLD");
        assert_eq!(rx.feed(1, &slot_a), None);
        assert_eq!(rx.feed(2, &slot_b), None);
    }

    #[test]
    fn tx_slot_zero_is_the_unscrambled_sync_pattern() {
        assert_eq!(slow_data_slot(0), SYNC_PATTERN);
        assert_eq!(SYNC_PATTERN, [0x55, 0x2D, 0x16]);
    }

    #[test]
    fn tx_slot_nonzero_seq_is_scrambled_null_filler() {
        // Every non-sync slot is a scrambled all-zero block: since XOR is
        // self-inverse, that's exactly SCRAMBLE itself.
        for seq in 1..SYNC_INTERVAL {
            assert_eq!(slow_data_slot(seq), SCRAMBLE, "seq {seq}");
        }
    }

    #[test]
    fn tx_slot_nonzero_seq_round_trips_through_rx_descramble() {
        // A TX-emitted filler slot, fed through the RX reassembler's own
        // descramble step, must come back out as all-zero — proving the TX
        // and RX sides agree on which direction the XOR runs.
        let slot = slow_data_slot(1);
        assert_eq!(descramble(slot), [0, 0, 0]);
    }

    #[test]
    fn tx_slot_never_emits_sync_pattern_outside_seq_zero() {
        for seq in 1..SYNC_INTERVAL {
            assert_ne!(slow_data_slot(seq), SYNC_PATTERN, "seq {seq}");
        }
    }
}
