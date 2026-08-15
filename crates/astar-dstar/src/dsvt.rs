// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! "DSVT" network framing (research §3): the wrapper `DExtra` and `DPlus`
//! both use to carry either a full RF header ("configuration" packet) or a
//! 12-byte voice/data frame ("voice" packet) over UDP. `DCS` does not use
//! this framing at all (see research §3) and is out of scope here.

use crate::header::{HEADER_LEN, HeaderError, RfHeader};

/// The 4-byte magic prefix of every DSVT packet.
pub const DSVT_MAGIC: [u8; 4] = *b"DSVT";

/// Packet-type byte at offset 4 for a header ("configuration") packet.
const TYPE_HEADER: u8 = 0x10;
/// Packet-type byte at offset 4 for a voice-frame packet.
const TYPE_VOICE: u8 = 0x20;

/// Stream-type byte at offset 8, and the band-routing bytes at offsets
/// 9-11: constant for every packet this crate emits or expects (voice,
/// direct routing) — research §3 gives no other observed values.
///
/// KNOWN DISCREPANCY (iax-2f6b review): the one piece of real traffic this
/// crate owns — the 56-byte XLX458 header pinned as a fixture in this file's
/// `parses_a_real_xlx_header_with_a_zero_crc` — carries `00 01 02` in bytes
/// 9-11, not the `00 01 01` research §3 documents and [`BAND_BYTES`] emits.
/// The third byte is a repeater-band selector; nothing validates it on
/// either side (this crate's parser ignores bytes 9-11 entirely, and xlxd's
/// `IsValidDvHeaderPacket` checks only length, magic, `buf[4]` and `buf[8]`),
/// so neither value is rejected on the wire. Recorded rather than silently
/// "corrected": one capture, relayed by one gateway, is not evidence about
/// what a transmitter should send, and research §8's lesson is precisely
/// that a fixture built from our own idea of correctness hides real-peer
/// behaviour. Stays pinned to the written spec until a second real sample —
/// ideally one captured from a live TX check — says otherwise.
const STREAM_TYPE_VOICE: u8 = 0x20;
const BAND_BYTES: [u8; 3] = [0x00, 0x01, 0x01];

/// Filler byte at offset 14 of a header packet (research §3; ON1ARF's and
/// pydv's layouts agree once this byte is accounted for).
const HEADER_FILLER: u8 = 0x80;

/// Bit set on a voice packet's frame-counter byte (offset 14) marking the
/// terminating frame of a stream (research §3: "the frame-counter is
/// increased by 0x40").
const END_OF_STREAM_BIT: u8 = 0x40;

/// Every 21st voice frame (sequence 0..=20) is a superframe: the slow-data
/// slot of sequence 0 carries a sync pattern instead of payload data
/// (research §6) rather than AMBE-adjacent user data.
pub const SYNC_INTERVAL: u8 = 21;

const HEADER_PACKET_LEN: usize = 15 + HEADER_LEN; // 56
const VOICE_PACKET_LEN: usize = 27;

/// A parsed DSVT packet: either an RF header ("configuration") packet or a
/// 12-byte voice/data frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsvtPacket {
    Header {
        stream_id: u16,
        header: RfHeader,
    },
    Voice {
        stream_id: u16,
        /// Frame sequence number within the current superframe, `0..SYNC_INTERVAL`.
        seq: u8,
        /// Set on the last frame of a stream.
        end: bool,
        ambe: [u8; 9],
        slow_data: [u8; 3],
    },
}

/// An error parsing a DSVT packet from wire bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsvtError {
    /// Buffer too short to even hold a common prefix, or the wrong length
    /// for the packet type its type byte claims to be.
    TooShort { expected: usize, actual: usize },
    /// The first 4 bytes weren't `"DSVT"`.
    BadMagic,
    /// The type byte at offset 4 was neither `0x10` (header) nor `0x20`
    /// (voice).
    UnknownType(u8),
    /// A header packet's embedded [`RfHeader`] failed to decode.
    Header(HeaderError),
}

impl From<HeaderError> for DsvtError {
    fn from(e: HeaderError) -> Self {
        DsvtError::Header(e)
    }
}

impl DsvtPacket {
    /// Parses a DSVT packet: a 56-byte header packet or a 27-byte voice
    /// packet (research §3).
    pub fn parse(buf: &[u8]) -> Result<DsvtPacket, DsvtError> {
        // Common prefix needed just to read the magic and type byte.
        if buf.len() < 5 {
            return Err(DsvtError::TooShort {
                expected: 5,
                actual: buf.len(),
            });
        }
        if buf[0..4] != DSVT_MAGIC {
            return Err(DsvtError::BadMagic);
        }
        match buf[4] {
            TYPE_HEADER => {
                if buf.len() != HEADER_PACKET_LEN {
                    return Err(DsvtError::TooShort {
                        expected: HEADER_PACKET_LEN,
                        actual: buf.len(),
                    });
                }
                let stream_id = u16::from_le_bytes([buf[12], buf[13]]);
                let mut header_bytes = [0u8; HEADER_LEN];
                header_bytes.copy_from_slice(&buf[15..15 + HEADER_LEN]);
                // RX is lenient about the header CRC: reflectors do not
                // recompute it when forwarding a stream, and XLX-family
                // reflectors send it as 0x0000 (observed live on XLX458 /
                // KC-Wide, 2026-08-09: 40 of 40 headers zero-CRC with
                // perfectly well-formed callsigns). Enforcing it here cost
                // every talker identification and, because a stream is keyed
                // off its header, all received audio. See
                // `RfHeader::decode_lenient`. Locally generated headers are
                // still verified through the strict `RfHeader::decode`.
                let (header, _crc_ok) = RfHeader::decode_lenient(&header_bytes);
                Ok(DsvtPacket::Header { stream_id, header })
            }
            TYPE_VOICE => {
                if buf.len() != VOICE_PACKET_LEN {
                    return Err(DsvtError::TooShort {
                        expected: VOICE_PACKET_LEN,
                        actual: buf.len(),
                    });
                }
                let stream_id = u16::from_le_bytes([buf[12], buf[13]]);
                let counter = buf[14];
                let seq = counter & !END_OF_STREAM_BIT;
                let end = counter & END_OF_STREAM_BIT != 0;
                let mut ambe = [0u8; 9];
                ambe.copy_from_slice(&buf[15..24]);
                let mut slow_data = [0u8; 3];
                slow_data.copy_from_slice(&buf[24..27]);
                Ok(DsvtPacket::Voice {
                    stream_id,
                    seq,
                    end,
                    ambe,
                    slow_data,
                })
            }
            other => Err(DsvtError::UnknownType(other)),
        }
    }

    /// Encodes this packet back into its wire form (used for parrot replay
    /// and future TX use).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self {
            DsvtPacket::Header { stream_id, header } => {
                let mut buf = Vec::with_capacity(HEADER_PACKET_LEN);
                buf.extend_from_slice(&DSVT_MAGIC);
                buf.push(TYPE_HEADER);
                buf.extend_from_slice(&[0x00, 0x00, 0x00]);
                buf.push(STREAM_TYPE_VOICE);
                buf.extend_from_slice(&BAND_BYTES);
                buf.extend_from_slice(&stream_id.to_le_bytes());
                buf.push(HEADER_FILLER);
                buf.extend_from_slice(&header.encode());
                debug_assert_eq!(buf.len(), HEADER_PACKET_LEN);
                buf
            }
            DsvtPacket::Voice {
                stream_id,
                seq,
                end,
                ambe,
                slow_data,
            } => {
                let mut buf = Vec::with_capacity(VOICE_PACKET_LEN);
                buf.extend_from_slice(&DSVT_MAGIC);
                buf.push(TYPE_VOICE);
                buf.extend_from_slice(&[0x00, 0x00, 0x00]);
                buf.push(STREAM_TYPE_VOICE);
                buf.extend_from_slice(&BAND_BYTES);
                buf.extend_from_slice(&stream_id.to_le_bytes());
                let counter = seq | if *end { END_OF_STREAM_BIT } else { 0 };
                buf.push(counter);
                buf.extend_from_slice(ambe);
                buf.extend_from_slice(slow_data);
                debug_assert_eq!(buf.len(), VOICE_PACKET_LEN);
                buf
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> RfHeader {
        RfHeader {
            flags: [0x00, 0x00, 0x00],
            rpt2: *b"XRF757 G",
            rpt1: *b"XRF757 A",
            ur: *b"CQCQCQ  ",
            my: *b"AJ7HR   ",
            suffix: *b"    ",
        }
    }

    /// Golden bytes for a header ("configuration") packet, per research §3:
    /// magic, type 0x10 at offset 4, zeros at 5-7, stream-type 0x20 at
    /// offset 8, band bytes at 9-11, stream id (little-endian, chosen —
    /// research doesn't pin an endianness) at 12-13, filler 0x80 at 14,
    /// then the 41-byte RF header at 15-55 (56 bytes total).
    #[test]
    fn header_packet_golden_bytes() {
        let header = sample_header();
        let packet = DsvtPacket::Header {
            stream_id: 0x1234,
            header,
        };
        let encoded = packet.encode();
        assert_eq!(encoded.len(), 56);
        assert_eq!(&encoded[0..4], b"DSVT");
        assert_eq!(encoded[4], 0x10);
        assert_eq!(&encoded[5..8], &[0x00, 0x00, 0x00]);
        assert_eq!(encoded[8], 0x20);
        assert_eq!(&encoded[9..12], &[0x00, 0x01, 0x01]);
        assert_eq!(&encoded[12..14], &[0x34, 0x12]); // 0x1234 little-endian
        assert_eq!(encoded[14], 0x80);
        assert_eq!(&encoded[15..56], &header.encode());
    }

    #[test]
    fn header_packet_round_trips_through_parse() {
        let header = sample_header();
        let packet = DsvtPacket::Header {
            stream_id: 0xBEEF,
            header,
        };
        let encoded = packet.encode();
        let parsed = DsvtPacket::parse(&encoded).expect("valid header packet");
        assert_eq!(parsed, packet);
    }

    /// Golden bytes for a voice packet, per research §3: magic, type 0x20
    /// at offset 4, zeros at 5-7, stream-type 0x20 at offset 8, band bytes
    /// at 9-11, stream id at 12-13, frame counter (with the 0x40 EOT bit)
    /// at 14, 9 bytes AMBE at 15-23, 3 bytes slow data at 24-26 (27 bytes
    /// total).
    #[test]
    fn voice_packet_golden_bytes() {
        let ambe = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99];
        let slow_data = [0xAA, 0xBB, 0xCC];
        let packet = DsvtPacket::Voice {
            stream_id: 0x1234,
            seq: 5,
            end: true,
            ambe,
            slow_data,
        };
        let encoded = packet.encode();
        assert_eq!(encoded.len(), 27);
        assert_eq!(&encoded[0..4], b"DSVT");
        assert_eq!(encoded[4], 0x20);
        assert_eq!(&encoded[5..8], &[0x00, 0x00, 0x00]);
        assert_eq!(encoded[8], 0x20);
        assert_eq!(&encoded[9..12], &[0x00, 0x01, 0x01]);
        assert_eq!(&encoded[12..14], &[0x34, 0x12]);
        assert_eq!(encoded[14], 0x45); // seq 5 | 0x40 EOT bit
        assert_eq!(&encoded[15..24], &ambe);
        assert_eq!(&encoded[24..27], &slow_data);
    }

    #[test]
    fn voice_packet_round_trips_through_parse() {
        let ambe = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        let slow_data = [10, 11, 12];
        let packet = DsvtPacket::Voice {
            stream_id: 42,
            seq: 0,
            end: false,
            ambe,
            slow_data,
        };
        let encoded = packet.encode();
        let parsed = DsvtPacket::parse(&encoded).expect("valid voice packet");
        assert_eq!(parsed, packet);
    }

    #[test]
    fn voice_packet_without_eot_bit_clears_end_flag() {
        let ambe = [0; 9];
        let slow_data = [0; 3];
        let packet = DsvtPacket::Voice {
            stream_id: 1,
            seq: 20,
            end: false,
            ambe,
            slow_data,
        };
        let encoded = packet.encode();
        assert_eq!(encoded[14], 20); // no 0x40 bit
        let parsed = DsvtPacket::parse(&encoded).unwrap();
        assert_eq!(parsed, packet);
    }

    #[test]
    fn parse_rejects_short_buffer() {
        let err = DsvtPacket::parse(&[0x44, 0x53, 0x56]).unwrap_err();
        assert!(matches!(err, DsvtError::TooShort { .. }));
    }

    #[test]
    fn parse_rejects_short_voice_packet() {
        // Valid magic/type/prefix but truncated before the full 27 bytes.
        let mut buf = vec![b'D', b'S', b'V', b'T', 0x20, 0x00, 0x00, 0x00];
        buf.resize(20, 0);
        let err = DsvtPacket::parse(&buf).unwrap_err();
        assert!(matches!(err, DsvtError::TooShort { .. }));
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut buf = vec![b'X', b'X', b'X', b'X', 0x20];
        buf.resize(27, 0);
        let err = DsvtPacket::parse(&buf).unwrap_err();
        assert_eq!(err, DsvtError::BadMagic);
    }

    #[test]
    fn parse_rejects_unknown_type_byte() {
        let mut buf = vec![b'D', b'S', b'V', b'T', 0x99];
        buf.resize(27, 0);
        let err = DsvtPacket::parse(&buf).unwrap_err();
        assert_eq!(err, DsvtError::UnknownType(0x99));
    }

    #[test]
    fn parse_accepts_a_header_whose_crc_does_not_verify() {
        // RX leniency (iax-7c19): a bad/absent header CRC must NOT cost us
        // the header. Reflectors forward headers without recomputing it.
        let header = sample_header();
        let packet = DsvtPacket::Header {
            stream_id: 1,
            header,
        };
        let mut encoded = packet.encode();
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF; // corrupt the RF header's CRC high byte
        let parsed = DsvtPacket::parse(&encoded).expect("bad CRC must still parse");
        assert_eq!(
            parsed,
            DsvtPacket::Header {
                stream_id: 1,
                header
            }
        );
    }

    /// Real 56-byte DSVT header packet captured off XLX458 (KC-Wide) module
    /// A on 2026-08-09 while W7VRY was transmitting, via a bridged M17 link.
    /// Its CRC trailer is `00 00` — the reflector never computed one — which
    /// is precisely the traffic our loopback parrot could not reproduce.
    #[test]
    fn parses_a_real_xlx_header_with_a_zero_crc() {
        const CAPTURED: [u8; 56] = [
            0x44, 0x53, 0x56, 0x54, 0x10, 0x00, 0x00, 0x00, 0x20, 0x00, 0x01, 0x02, 0x06, 0xe8,
            0x80, 0x00, 0x00, 0x00, 0x58, 0x52, 0x46, 0x34, 0x35, 0x38, 0x20, 0x41, 0x42, 0x4d,
            0x33, 0x31, 0x30, 0x34, 0x20, 0x42, 0x43, 0x51, 0x43, 0x51, 0x43, 0x51, 0x20, 0x20,
            0x57, 0x37, 0x56, 0x52, 0x59, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00,
        ];
        let parsed = DsvtPacket::parse(&CAPTURED).expect("real reflector header must parse");
        let DsvtPacket::Header { stream_id, header } = parsed else {
            panic!("expected a header packet, got {parsed:?}");
        };
        assert_eq!(stream_id, 0xe806);
        assert_eq!(header.my_callsign(), "W7VRY");
        assert_eq!(&header.rpt2, b"XRF458 A");
        assert_eq!(&header.rpt1, b"BM3104 B");
        assert_eq!(&header.ur, b"CQCQCQ  ");
        // The captured trailer really is zero, and strict decode still says so.
        let mut raw = [0u8; 41];
        raw.copy_from_slice(&CAPTURED[15..56]);
        assert!(matches!(
            RfHeader::decode(&raw),
            Err(HeaderError::Crc { actual: 0, .. })
        ));
        assert!(
            !RfHeader::decode_lenient(&raw).1,
            "crc_ok must report false"
        );
    }

    #[test]
    fn sync_interval_is_21() {
        assert_eq!(SYNC_INTERVAL, 21);
    }
}
