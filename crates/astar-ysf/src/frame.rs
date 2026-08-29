// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The 120-byte YSF radio frame: five bytes of sync, a 25-byte FICH, and
//! five 18-byte payload blocks.
//!
//! This module reads and writes the frame's *shape*. What is inside the
//! ninety payload bytes — the AMBE+2 voice bits and the data interleaved
//! with them — is deliberately not decoded here: astar cannot make a sound
//! out of them without a vocoder, and a half-built payload parser that
//! nothing calls is worse than an honest `payload()` that hands the bytes
//! over intact.

use crate::fich::{FICH_LEN, Fich, FichError};

/// A whole YSF frame.
pub const FRAME_LEN: usize = 120;
/// The sync pattern every frame opens with.
pub const SYNC: [u8; 5] = [0xD4, 0x71, 0xC9, 0x63, 0x4D];
/// Bytes of sync.
pub const SYNC_LEN: usize = SYNC.len();
/// Bytes of payload after the sync and FICH.
pub const PAYLOAD_LEN: usize = FRAME_LEN - SYNC_LEN - FICH_LEN;

/// A borrowed view over one 120-byte frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a>(&'a [u8; FRAME_LEN]);

impl<'a> Frame<'a> {
    /// Borrows `bytes` as a frame, checking only the length.
    ///
    /// The sync pattern is *not* required: a frame arriving over the
    /// network has already been framed by the packet around it, and
    /// refusing one whose sync bytes are scuffed would throw away audio a
    /// receiver could have used. Use [`Frame::has_sync`] where it matters.
    pub fn new(bytes: &'a [u8]) -> Option<Frame<'a>> {
        bytes.try_into().ok().map(Frame)
    }

    /// The whole frame.
    #[must_use]
    pub const fn as_bytes(self) -> &'a [u8; FRAME_LEN] {
        self.0
    }

    /// Whether the frame opens with [`SYNC`].
    #[must_use]
    pub fn has_sync(self) -> bool {
        self.0[..SYNC_LEN] == SYNC
    }

    /// The raw, still-coded FICH field.
    #[must_use]
    pub fn fich_field(self) -> &'a [u8] {
        &self.0[SYNC_LEN..SYNC_LEN + FICH_LEN]
    }

    /// Decodes the FICH.
    pub fn fich(self) -> Result<Fich, FichError> {
        Fich::decode(self.fich_field())
    }

    /// The ninety payload bytes, undecoded.
    #[must_use]
    pub fn payload(self) -> &'a [u8] {
        &self.0[SYNC_LEN + FICH_LEN..]
    }
}

/// Builds a frame from a FICH and a payload.
///
/// # Panics
/// If `payload` is not [`PAYLOAD_LEN`] bytes.
#[must_use]
pub fn build(fich: Fich, payload: &[u8]) -> [u8; FRAME_LEN] {
    assert_eq!(payload.len(), PAYLOAD_LEN, "payload is 90 bytes");
    let mut frame = [0u8; FRAME_LEN];
    frame[..SYNC_LEN].copy_from_slice(&SYNC);
    fich.encode(&mut frame[SYNC_LEN..SYNC_LEN + FICH_LEN]);
    frame[SYNC_LEN + FICH_LEN..].copy_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fich::{DataType, FrameInfo};

    #[test]
    fn the_pieces_add_up_to_a_frame() {
        assert_eq!(SYNC_LEN + FICH_LEN + PAYLOAD_LEN, FRAME_LEN);
        assert_eq!(PAYLOAD_LEN, 90);
    }

    #[test]
    fn a_built_frame_reads_back() {
        let fich = Fich {
            frame_info: FrameInfo::Communications,
            data_type: DataType::VDMode2,
            frame_number: 3,
            frame_total: 6,
            ..Fich::default()
        };
        let payload: Vec<u8> = (0..PAYLOAD_LEN)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let bytes = build(fich, &payload);

        let frame = Frame::new(&bytes).expect("a 120-byte frame");
        assert!(frame.has_sync());
        assert_eq!(frame.fich(), Ok(fich));
        assert_eq!(frame.payload(), &payload[..]);
    }

    #[test]
    fn a_frame_of_the_wrong_length_is_refused() {
        assert!(Frame::new(&[0u8; 119]).is_none());
        assert!(Frame::new(&[0u8; 121]).is_none());
        assert!(Frame::new(&[]).is_none());
    }

    #[test]
    fn a_scuffed_sync_still_gives_up_its_fich() {
        // Sync is how a radio finds a frame in a bit stream. Over the
        // network the packet has already done that job, so a frame whose
        // sync is wrong is still worth decoding rather than dropping.
        let fich = Fich::default();
        let mut bytes = build(fich, &[0u8; PAYLOAD_LEN]);
        bytes[0] ^= 0xFF;
        let frame = Frame::new(&bytes).expect("a 120-byte frame");
        assert!(!frame.has_sync());
        assert_eq!(frame.fich(), Ok(fich));
    }
}
