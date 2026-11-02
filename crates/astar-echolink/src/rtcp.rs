// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The control-port datagrams: RTCP SDES (connect and keepalive) and BYE.
//!
//! Both are RTCP compound packets that start with an empty receiver report.
//! Every SSRC is zero. The header's version field is 3 on what we send, and
//! 1 or 3 is accepted on what we receive.

/// RTCP packet types.
const PT_RR: u8 = 201;
const PT_SDES: u8 = 202;
const PT_BYE: u8 = 203;

/// SDES item types.
const SDES_END: u8 = 0;
const SDES_CNAME: u8 = 1;
const SDES_NAME: u8 = 2;
const SDES_EMAIL: u8 = 3;
const SDES_PHONE: u8 = 4;

/// Version 3, no padding, no report blocks.
const HEADER_V3: u8 = 0xC0;
/// The empty receiver report that opens every compound packet we send.
const EMPTY_RR: [u8; 8] = [HEADER_V3, PT_RR, 0, 1, 0, 0, 0, 0];
/// The CNAME and EMAIL values: the literal word.
const CALLSIGN_WORD: &[u8] = b"CALLSIGN";
const PHONE: &[u8] = b"08:30";
const BYE_REASON: &[u8] = b"jan2002";
/// NAME is the callsign left-justified in this many characters, then the name.
const NAME_CALL_WIDTH: usize = 15;

/// The SDES compound packet sent to connect and every keepalive.
///
/// Items are CNAME, NAME, EMAIL and PHONE. No PRIV item is ever written, so
/// Speex is never offered.
#[must_use]
pub fn sdes(callsign: &str, name: &str) -> Vec<u8> {
    let full_name = format!("{callsign:<NAME_CALL_WIDTH$}{name}");
    let mut out = EMPTY_RR.to_vec();
    let start = out.len();
    out.extend_from_slice(&[HEADER_V3 | 1, PT_SDES, 0, 0, 0, 0, 0, 0]);
    push_item(&mut out, SDES_CNAME, CALLSIGN_WORD);
    push_item(&mut out, SDES_NAME, full_name.as_bytes());
    push_item(&mut out, SDES_EMAIL, CALLSIGN_WORD);
    push_item(&mut out, SDES_PHONE, PHONE);
    out.extend_from_slice(&[SDES_END, 0]);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    let words = u16::try_from((out.len() - start) / 4 - 1).unwrap_or(u16::MAX);
    out[start + 2..start + 4].copy_from_slice(&words.to_be_bytes());
    out
}

/// One SDES item; a value longer than 255 bytes is cut at 255.
fn push_item(out: &mut Vec<u8>, kind: u8, value: &[u8]) {
    let value = &value[..value.len().min(255)];
    out.push(kind);
    out.push(u8::try_from(value.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(value);
}

/// The BYE compound packet that ends a contact, with reason `jan2002`.
#[must_use]
pub fn bye() -> [u8; 24] {
    let mut out = [0u8; 24];
    out[..8].copy_from_slice(&EMPTY_RR);
    out[8..16].copy_from_slice(&[HEADER_V3 | 1, PT_BYE, 0, 3, 0, 0, 0, 0]);
    out[16] = 7;
    out[17..].copy_from_slice(BYE_REASON);
    out
}

/// What a control-port datagram turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rtcp {
    /// `callsign` is the first word of the NAME item (never empty), `name`
    /// the rest.
    Sdes {
        callsign: String,
        name: String,
    },
    Bye,
    /// Well formed, but neither SDES nor BYE (a bare report, say).
    Other,
}

/// Why a control-port datagram was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RtcpError {
    #[error("RTCP datagram is shorter than its headers say")]
    Truncated,
    #[error("RTCP version {0} is not accepted")]
    Version(u8),
}

/// Walk a compound packet. A BYE anywhere wins over an SDES.
///
/// # Errors
/// [`RtcpError::Truncated`] when a length runs past the datagram (or it is
/// empty); [`RtcpError::Version`] for a header that is not version 1 or 3.
pub fn parse(bytes: &[u8]) -> Result<Rtcp, RtcpError> {
    if bytes.is_empty() {
        return Err(RtcpError::Truncated);
    }
    let mut found = Rtcp::Other;
    let mut rest = bytes;
    while !rest.is_empty() {
        let header = rest.get(..4).ok_or(RtcpError::Truncated)?;
        let version = header[0] >> 6;
        if version != 1 && version != 3 {
            return Err(RtcpError::Version(version));
        }
        let len = (usize::from(u16::from_be_bytes([header[2], header[3]])) + 1) * 4;
        let packet = rest.get(..len).ok_or(RtcpError::Truncated)?;
        rest = &rest[len..];
        match header[1] {
            PT_BYE => found = Rtcp::Bye,
            PT_SDES => {
                let sdes = parse_sdes(packet, header[0] & 0x1f)?;
                if found == Rtcp::Other {
                    found = sdes;
                }
            }
            // Sender and receiver reports, and anything else, carry nothing we use.
            _ => {}
        }
    }
    Ok(found)
}

/// Read the NAME item of the first chunk, checking every item's length.
///
/// An SDES with no chunk, no NAME item or a blank NAME is [`Rtcp::Other`].
fn parse_sdes(packet: &[u8], chunks: u8) -> Result<Rtcp, RtcpError> {
    let mut name = None;
    if chunks > 0 {
        // Skip the 4-byte header and the chunk's SSRC.
        let mut items = packet.get(8..).ok_or(RtcpError::Truncated)?;
        loop {
            let &kind = items.first().ok_or(RtcpError::Truncated)?;
            if kind == SDES_END {
                break;
            }
            let len = usize::from(*items.get(1).ok_or(RtcpError::Truncated)?);
            let value = items.get(2..2 + len).ok_or(RtcpError::Truncated)?;
            if kind == SDES_NAME && name.is_none() {
                name = Some(String::from_utf8_lossy(value).into_owned());
            }
            items = &items[2 + len..];
        }
    }
    let Some(name) = name else {
        return Ok(Rtcp::Other);
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(Rtcp::Other);
    }
    let (callsign, rest) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    Ok(Rtcp::Sdes {
        callsign: callsign.to_owned(),
        name: rest.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDES_KX7ABC: [u8; 64] = [
        0xc0, 0xc9, 0x00, 0x01, 0, 0, 0, 0, // empty RR, version 3, SSRC 0
        0xc1, 0xca, 0x00, 0x0d, 0, 0, 0, 0, // SDES, one chunk, length 13
        0x01, 0x08, b'C', b'A', b'L', b'L', b'S', b'I', b'G', b'N', // CNAME "CALLSIGN"
        0x02, 0x0f, b'K', b'X', b'7', b'A', b'B', b'C', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
        b' ', b' ', // NAME %-15s
        0x03, 0x08, b'C', b'A', b'L', b'L', b'S', b'I', b'G', b'N', // EMAIL "CALLSIGN"
        0x04, 0x05, b'0', b'8', b':', b'3', b'0', // PHONE "08:30"
        0x00, 0x00, 0x00, 0x00, // END, one zero, pad to 4
    ];

    /// Item types of the first SDES chunk, walked type by type.
    fn sdes_item_types(bytes: &[u8]) -> Vec<u8> {
        let mut at = 0;
        loop {
            let len = (usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]])) + 1) * 4;
            if bytes[at + 1] == 202 {
                let mut i = at + 8;
                let mut types = Vec::new();
                while bytes[i] != 0 {
                    types.push(bytes[i]);
                    i += 2 + usize::from(bytes[i + 1]);
                }
                return types;
            }
            at += len;
        }
    }

    #[test]
    fn sdes_golden() {
        assert_eq!(sdes("KX7ABC", ""), SDES_KX7ABC);
    }

    #[test]
    fn bye_golden() {
        assert_eq!(
            bye(),
            [
                0xc0, 0xc9, 0, 1, 0, 0, 0, 0, 0xc1, 0xcb, 0, 3, 0, 0, 0, 0, 7, b'j', b'a', b'n',
                b'2', b'0', b'0', b'2'
            ]
        );
    }

    #[test]
    fn sdes_is_padded_to_four_bytes_for_any_length() {
        for call in ["", "W1AW", "KX7ABC-L", "ABCDEFGHIJKLMNOPQRSTUVWXYZ"] {
            for name in ["", "H", "Hiram Percy", &"x".repeat(400)] {
                let out = sdes(call, name);
                assert!(out.len().is_multiple_of(4));
                let words = usize::from(u16::from_be_bytes([out[10], out[11]]));
                assert_eq!(8 + (words + 1) * 4, out.len());
                assert!(parse(&out).is_ok());
            }
        }
    }

    #[test]
    fn sdes_parse_takes_the_first_token_of_name() {
        assert_eq!(
            parse(&sdes("W1AW", "Hiram")).unwrap(),
            Rtcp::Sdes {
                callsign: "W1AW".to_owned(),
                name: "Hiram".to_owned()
            }
        );
        assert_eq!(
            parse(&SDES_KX7ABC).unwrap(),
            Rtcp::Sdes {
                callsign: "KX7ABC".to_owned(),
                name: String::new()
            }
        );
    }

    #[test]
    fn sdes_never_offers_priv() {
        for name in ["", "Hiram", "SPEEX"] {
            let types = sdes_item_types(&sdes("KX7ABC", name));
            assert_eq!(types, vec![1, 2, 3, 4]);
            assert!(!types.contains(&8));
        }
    }

    #[test]
    fn parse_accepts_version_1_and_3_headers() {
        for version in [1u8, 3] {
            let mut bytes = SDES_KX7ABC;
            bytes[0] = (version << 6) | (bytes[0] & 0x3f);
            bytes[8] = (version << 6) | (bytes[8] & 0x3f);
            assert!(
                matches!(parse(&bytes), Ok(Rtcp::Sdes { .. })),
                "version {version}"
            );
        }
        let mut v2 = SDES_KX7ABC;
        v2[0] = 0x80;
        v2[8] = 0x81;
        assert!(parse(&v2).is_err());
    }

    #[test]
    fn a_length_past_the_datagram_is_an_error() {
        let mut bytes = SDES_KX7ABC;
        bytes[11] = 0xff;
        assert!(parse(&bytes).is_err());
        // An item length that runs past the packet is an error too.
        let mut bytes = SDES_KX7ABC;
        bytes[17] = 0xf0;
        assert!(parse(&bytes).is_err());
        // Every prefix of a good datagram parses or errs, never panics.
        for k in 0..SDES_KX7ABC.len() {
            let _ = parse(&SDES_KX7ABC[..k]);
        }
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn sdes_without_a_usable_name_is_other() {
        // Chunk count 0: only the 4-byte header.
        let mut no_chunk = EMPTY_RR.to_vec();
        no_chunk.extend_from_slice(&[0xc0, PT_SDES, 0, 0]);
        assert_eq!(parse(&no_chunk).unwrap(), Rtcp::Other);
        // One chunk with CNAME only.
        let mut no_name = EMPTY_RR.to_vec();
        no_name.extend_from_slice(&[0xc1, PT_SDES, 0, 4, 0, 0, 0, 0]);
        no_name.extend_from_slice(&[SDES_CNAME, 8]);
        no_name.extend_from_slice(CALLSIGN_WORD);
        no_name.extend_from_slice(&[SDES_END, 0]);
        assert_eq!(no_name.len(), 8 + 20);
        assert_eq!(parse(&no_name).unwrap(), Rtcp::Other);
        // A NAME of blanks, and an empty NAME.
        assert_eq!(parse(&sdes("", "")).unwrap(), Rtcp::Other);
        let mut empty_name = EMPTY_RR.to_vec();
        empty_name.extend_from_slice(&[0xc1, PT_SDES, 0, 2, 0, 0, 0, 0, SDES_NAME, 0, SDES_END, 0]);
        assert_eq!(parse(&empty_name).unwrap(), Rtcp::Other);
    }

    #[test]
    fn bye_parses_as_bye() {
        assert_eq!(parse(&bye()).unwrap(), Rtcp::Bye);
    }

    #[test]
    fn an_empty_receiver_report_alone_is_other() {
        assert_eq!(parse(&SDES_KX7ABC[..8]).unwrap(), Rtcp::Other);
    }
}
