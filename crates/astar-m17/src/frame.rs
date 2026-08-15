// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! M17 IP framing: the Link Setup Frame (LSF) fields embedded in each stream
//! packet, and the 54-byte "M17 " voice stream packet itself.

use crate::crc::crc16_m17;

/// The `"M17 "` (trailing space) magic that opens every stream packet.
const MAGIC: &[u8; 4] = b"M17 ";

/// Wire size of a stream packet: 4 (magic) + 2 (`StreamID`) + 28 (LSF fields)
/// + 2 (frame number) + 16 (payload) + 2 (CRC).
const PACKET_LEN: usize = 54;

/// Link Setup Frame fields carried inline in each stream packet (destination,
/// source, stream type, and metadata).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lsf {
    /// Destination address (base-40 encoded callsign, or [`crate::BROADCAST`]).
    pub dst: [u8; 6],
    /// Source address (base-40 encoded callsign).
    pub src: [u8; 6],
    /// Stream type field; see [`Lsf::TYPE_VOICE_3200_STREAM`].
    pub type_field: u16,
    /// 14 bytes of stream metadata (e.g. encryption info); all-zero when unused.
    pub meta: [u8; 14],
}

impl Lsf {
    /// TYPE value for a voice-only 3200-bit/s stream.
    pub const TYPE_VOICE_3200_STREAM: u16 = 0x0005;
}

/// A single 54-byte M17 IP voice stream packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPacket {
    /// Random ID identifying all packets belonging to one transmission.
    pub stream_id: u16,
    /// Link setup fields (dst/src/type/meta), repeated in every packet.
    pub lsf: Lsf,
    /// Bit 15 ([`StreamPacket::EOS_BIT`]) marks the last frame; the low 15
    /// bits are a counter from 0.
    pub frame_number: u16,
    /// 16 bytes of Codec 2 3200 payload (two 8-byte voice+FEC chunks).
    pub payload: [u8; 16],
}

impl StreamPacket {
    /// Bit 15 of `frame_number`: set on the final frame of a transmission.
    pub const EOS_BIT: u16 = 0x8000;

    /// Serializes this packet to its 54-byte wire form, computing and
    /// appending the trailing CRC-16/M17.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; PACKET_LEN] {
        let mut buf = [0u8; PACKET_LEN];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4..6].copy_from_slice(&self.stream_id.to_be_bytes());
        buf[6..12].copy_from_slice(&self.lsf.dst);
        buf[12..18].copy_from_slice(&self.lsf.src);
        buf[18..20].copy_from_slice(&self.lsf.type_field.to_be_bytes());
        buf[20..34].copy_from_slice(&self.lsf.meta);
        buf[34..36].copy_from_slice(&self.frame_number.to_be_bytes());
        buf[36..52].copy_from_slice(&self.payload);
        let crc = crc16_m17(&buf[0..52]);
        buf[52..54].copy_from_slice(&crc.to_be_bytes());
        buf
    }

    /// Parses a 54-byte buffer into a [`StreamPacket`], validating the magic,
    /// length, and CRC. Returns `None` on any mismatch.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<StreamPacket> {
        if buf.len() != PACKET_LEN {
            return None;
        }
        if &buf[0..4] != MAGIC {
            return None;
        }
        if crc16_m17(buf) != 0 {
            return None;
        }
        let stream_id = u16::from_be_bytes([buf[4], buf[5]]);
        let mut dst = [0u8; 6];
        dst.copy_from_slice(&buf[6..12]);
        let mut src = [0u8; 6];
        src.copy_from_slice(&buf[12..18]);
        let type_field = u16::from_be_bytes([buf[18], buf[19]]);
        let mut meta = [0u8; 14];
        meta.copy_from_slice(&buf[20..34]);
        let frame_number = u16::from_be_bytes([buf[34], buf[35]]);
        let mut payload = [0u8; 16];
        payload.copy_from_slice(&buf[36..52]);
        Some(StreamPacket {
            stream_id,
            lsf: Lsf {
                dst,
                src,
                type_field,
                meta,
            },
            frame_number,
            payload,
        })
    }

    /// Whether [`StreamPacket::EOS_BIT`] is set on `frame_number`.
    #[must_use]
    pub fn is_last(&self) -> bool {
        self.frame_number & Self::EOS_BIT != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{BROADCAST, encode_callsign};

    #[test]
    fn stream_packet_round_trips_54_bytes_with_valid_crc() {
        let p = StreamPacket {
            stream_id: 0xBEEF,
            lsf: Lsf {
                dst: encode_callsign("M17-KCW A").unwrap_or(BROADCAST), // reflector-style dst
                src: encode_callsign("N0CALL").unwrap(),
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number: 3,
            payload: [0xAB; 16],
        };
        let bytes = p.to_bytes();
        assert_eq!(bytes.len(), 54);
        assert_eq!(&bytes[0..4], b"M17 ");
        assert_eq!(
            crc16_m17(&bytes),
            0,
            "CRC over the full packet incl. CRC field is 0"
        );
        let back = StreamPacket::parse(&bytes).unwrap();
        assert_eq!(back.stream_id, 0xBEEF);
        assert_eq!(back.frame_number, 3);
        assert!(!back.is_last());
        let mut last = p;
        last.frame_number |= StreamPacket::EOS_BIT;
        assert!(StreamPacket::parse(&last.to_bytes()).unwrap().is_last());
    }

    #[test]
    fn parse_rejects_bad_magic_length_and_crc() {
        let p = StreamPacket {
            stream_id: 1,
            lsf: Lsf {
                dst: BROADCAST,
                src: encode_callsign("N0CALL").unwrap(),
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number: 0,
            payload: [0; 16],
        };
        let mut bytes = p.to_bytes();

        // Wrong magic.
        let mut bad_magic = bytes;
        bad_magic[0] = b'X';
        assert!(StreamPacket::parse(&bad_magic).is_none());

        // Wrong length (53 bytes).
        assert!(StreamPacket::parse(&bytes[0..53]).is_none());

        // Flipped payload bit invalidates the CRC.
        bytes[36] ^= 0x01;
        assert!(StreamPacket::parse(&bytes).is_none());
    }
}
