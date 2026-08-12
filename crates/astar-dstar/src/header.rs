// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The 41-byte IP-side D-Star RF header: flags + RPT2/RPT1/UR/MY callsigns
//! + a 4-byte MY suffix, terminated by a 2-byte CRC-CCITT (research §3).
//!
//! This is the header as carried over IP inside a DSVT "configuration"
//! packet (or DExtra/DPlus/slow-data repeats of it) — not the 660-bit
//! FEC-interleaved-scrambled form actually sent over RF, which a softclient
//! never needs to touch.

/// Length of the encoded RF header: 39 field bytes + 2 CRC bytes.
pub const HEADER_LEN: usize = 41;

/// Number of header bytes covered by the CRC (everything except the CRC
/// itself).
const CRC_COVERED_LEN: usize = 39;

/// The 41-byte D-Star RF header.
///
/// Field order matches the wire order exactly (research §3): 3 flag bytes,
/// then the four 8-byte callsign-shaped fields, then the 4-byte MY suffix,
/// then (added by [`RfHeader::encode`], not stored here) the 2-byte CRC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RfHeader {
    pub flags: [u8; 3],
    /// Destination repeater ("RPT2"), ASCII space-padded.
    pub rpt2: [u8; 8],
    /// Departure repeater ("RPT1"), ASCII space-padded.
    pub rpt1: [u8; 8],
    /// Companion/target callsign, e.g. `"CQCQCQ  "`.
    pub ur: [u8; 8],
    /// Your own callsign, ASCII space-padded.
    pub my: [u8; 8],
    /// MY callsign suffix, e.g. `"RPTR"` or blank.
    pub suffix: [u8; 4],
}

/// An error decoding an [`RfHeader`] from wire bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// The CRC-CCITT trailer didn't match the preceding 39 bytes.
    Crc { expected: u16, actual: u16 },
}

impl RfHeader {
    /// Encodes this header into its 41-byte wire form, appending the
    /// CRC-CCITT (low byte first) computed over the first 39 bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..3].copy_from_slice(&self.flags);
        buf[3..11].copy_from_slice(&self.rpt2);
        buf[11..19].copy_from_slice(&self.rpt1);
        buf[19..27].copy_from_slice(&self.ur);
        buf[27..35].copy_from_slice(&self.my);
        buf[35..39].copy_from_slice(&self.suffix);
        let crc = crc_ccitt(&buf[..CRC_COVERED_LEN]);
        buf[39..41].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decodes a 41-byte wire header, verifying its CRC-CCITT trailer.
    ///
    /// Strict: use for locally generated headers (encode round-trips, and
    /// the TX path when it lands). Traffic received from a reflector must
    /// use [`RfHeader::decode_lenient`] instead — see its docs for why.
    pub fn decode(buf: &[u8; HEADER_LEN]) -> Result<RfHeader, HeaderError> {
        let (header, crc_ok) = Self::decode_lenient(buf);
        if !crc_ok {
            return Err(HeaderError::Crc {
                expected: crc_ccitt(&buf[..CRC_COVERED_LEN]),
                actual: u16::from_le_bytes([buf[39], buf[40]]),
            });
        }
        Ok(header)
    }

    /// Decodes a 41-byte wire header, reporting CRC validity instead of
    /// enforcing it: `(header, crc_ok)`.
    ///
    /// The header CRC is an RF-layer artifact. Reflectors forwarding a
    /// stream over the network do NOT recompute it, and XLX-family
    /// reflectors send the field as `0x0000` outright — observed live on
    /// XLX458 (KC-Wide) on 2026-08-09, where 40 of 40 received headers
    /// carried a zero CRC while their callsign fields were perfectly
    /// well-formed. Rejecting those headers cost us every talker
    /// identification and (because a stream is keyed off its header) all
    /// received audio, for a checksum the network layer never fills in.
    ///
    /// So on RX the CRC is advisory: callers take the header and decide
    /// what a bad CRC means to them. A loopback reflector cannot surface
    /// this — it computes correct CRCs, so every test passed.
    #[must_use]
    pub fn decode_lenient(buf: &[u8; HEADER_LEN]) -> (RfHeader, bool) {
        let expected = crc_ccitt(&buf[..CRC_COVERED_LEN]);
        let actual = u16::from_le_bytes([buf[39], buf[40]]);
        let crc_ok = expected == actual;
        let mut flags = [0u8; 3];
        flags.copy_from_slice(&buf[0..3]);
        let mut rpt2 = [0u8; 8];
        rpt2.copy_from_slice(&buf[3..11]);
        let mut rpt1 = [0u8; 8];
        rpt1.copy_from_slice(&buf[11..19]);
        let mut ur = [0u8; 8];
        ur.copy_from_slice(&buf[19..27]);
        let mut my = [0u8; 8];
        my.copy_from_slice(&buf[27..35]);
        let mut suffix = [0u8; 4];
        suffix.copy_from_slice(&buf[35..39]);
        (
            RfHeader {
                flags,
                rpt2,
                rpt1,
                ur,
                my,
                suffix,
            },
            crc_ok,
        )
    }

    /// The MY callsign field, trimmed of ASCII padding.
    #[must_use]
    pub fn my_callsign(&self) -> String {
        String::from_utf8_lossy(&self.my).trim().to_string()
    }
}

/// D-Star's CRC-CCITT variant: `G(x) = x^16 + x^12 + x^5 + 1`, computed
/// reflected (poly `0x8408`), initial value `0xFFFF`, final XOR `0xFFFF`
/// (i.e. CRC-16/X-25). The cross-check vector below was produced by an
/// independent implementation of the same algorithm.
#[must_use]
pub fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x8408
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard check value for CRC-16/X-25 (poly 0x1021 reflected =
    /// 0x8408, init 0xFFFF, xorout 0xFFFF): "123456789" -> 0x906E. Also
    /// the standard self-check for this algorithm, so it pins `crc_ccitt`
    /// to CRC-16/X-25 rather than to one particular implementation.
    #[test]
    fn crc_ccitt_x25_check_value() {
        assert_eq!(crc_ccitt(b"123456789"), 0x906E);
    }

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

    /// Cross-check: the CRC-CCITT for `sample_header`'s 39 field bytes,
    /// computed once by a separate CRC-16/X-25 implementation on the
    /// identical 39-byte input (same reflected-0x8408 / init-0xFFFF /
    /// xorout-0xFFFF parameters), which printed `crc16 = 0x0afc`. The
    /// expected value is hard-coded here, so this test is self-contained:
    /// it matches this crate's own `crc_ccitt` bit-for-bit, and both agree
    /// with the standard CRC-16/X-25 check value above.
    #[test]
    fn crc_ccitt_matches_independent_x25_cross_check() {
        let h = sample_header();
        let mut fields = [0u8; CRC_COVERED_LEN];
        fields[0..3].copy_from_slice(&h.flags);
        fields[3..11].copy_from_slice(&h.rpt2);
        fields[11..19].copy_from_slice(&h.rpt1);
        fields[19..27].copy_from_slice(&h.ur);
        fields[27..35].copy_from_slice(&h.my);
        fields[35..39].copy_from_slice(&h.suffix);
        assert_eq!(crc_ccitt(&fields), 0x0AFC);
    }

    #[test]
    fn encode_decode_round_trip() {
        let h = sample_header();
        let encoded = h.encode();
        assert_eq!(encoded.len(), HEADER_LEN);
        let decoded = RfHeader::decode(&encoded).expect("valid CRC");
        assert_eq!(decoded, h);
    }

    #[test]
    fn encode_appends_expected_crc_bytes() {
        let encoded = sample_header().encode();
        // CRC computed above: 0x0AFC, transmitted low byte first.
        assert_eq!(encoded[39], 0xFC);
        assert_eq!(encoded[40], 0x0A);
    }

    #[test]
    fn decode_rejects_flipped_byte() {
        let mut encoded = sample_header().encode();
        encoded[5] ^= 0xFF; // flip a byte inside RPT2
        let err = RfHeader::decode(&encoded).unwrap_err();
        assert!(matches!(err, HeaderError::Crc { .. }));
    }

    #[test]
    fn decode_rejects_flipped_crc_byte() {
        let mut encoded = sample_header().encode();
        encoded[39] ^= 0x01;
        assert!(RfHeader::decode(&encoded).is_err());
    }

    #[test]
    fn my_callsign_trims_padding() {
        let h = sample_header();
        assert_eq!(h.my_callsign(), "AJ7HR");
    }
}
