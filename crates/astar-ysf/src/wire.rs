// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `YSFReflector` packets: what goes into a UDP datagram and what comes
//! back out.
//!
//! Every packet opens with a four-byte tag. The ones that matter to a
//! client:
//!
//! | tag | bytes | direction | meaning |
//! |---|---|---|---|
//! | `YSFP` | 14 | both | poll — register, stay registered, and the reply that says you are |
//! | `YSFU` | 14 | to reflector | unlink |
//! | `YSFD` | 155 | both | one 120-byte radio frame, with routing |
//! | `YSFO` | 50 | to reflector | options string |
//! | `YSFS` | 4 / 42 | both | status request and reply |
//! | `YSFI` | — | from reflector | information; ignored |
//!
//! Callsigns on this wire are exactly ten bytes, space padded, never NUL
//! terminated.

use crate::frame::FRAME_LEN;

/// Bytes in a callsign field.
pub const CALLSIGN_LEN: usize = 10;
/// Bytes in a poll or unlink packet.
pub const POLL_LEN: usize = 4 + CALLSIGN_LEN;
/// Bytes in a data packet: tag, three callsigns, a counter, and the frame.
pub const DATA_LEN: usize = 4 + CALLSIGN_LEN * 3 + 1 + FRAME_LEN;
/// Bytes in an options packet.
pub const OPTIONS_LEN: usize = 50;

const TAG_POLL: &[u8; 4] = b"YSFP";
const TAG_UNLINK: &[u8; 4] = b"YSFU";
const TAG_DATA: &[u8; 4] = b"YSFD";
const TAG_OPTIONS: &[u8; 4] = b"YSFO";
const TAG_STATUS: &[u8; 4] = b"YSFS";
const TAG_INFO: &[u8; 4] = b"YSFI";

/// A ten-byte, space-padded callsign field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Callsign([u8; CALLSIGN_LEN]);

/// Why a callsign could not be used on this wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallsignError {
    /// Longer than the wire format's ten-byte field.
    TooLong,
    /// Contains a byte outside printable ASCII.
    NotPrintableAscii,
}

impl Callsign {
    /// Builds a callsign field, space padding to ten bytes.
    ///
    /// Refuses anything that is not printable ASCII: this field is read by
    /// other people's dashboards, and a control byte in it is either a bug
    /// or an attempt to make one reflector's log say something it should
    /// not.
    pub fn new(callsign: &str) -> Result<Callsign, CallsignError> {
        if callsign.len() > CALLSIGN_LEN {
            return Err(CallsignError::TooLong);
        }
        if !callsign.bytes().all(|b| (0x20..0x7F).contains(&b)) {
            return Err(CallsignError::NotPrintableAscii);
        }
        let mut field = [b' '; CALLSIGN_LEN];
        field[..callsign.len()].copy_from_slice(callsign.as_bytes());
        Ok(Callsign(field))
    }

    /// The ten bytes, as they go on the wire.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; CALLSIGN_LEN] {
        &self.0
    }

    /// The callsign with its padding trimmed.
    ///
    /// Non-ASCII bytes from a peer are replaced rather than refused — a
    /// neighbour's malformed callsign is a thing to display, not a reason
    /// to drop their audio.
    #[must_use]
    pub fn to_trimmed_string(&self) -> String {
        String::from_utf8_lossy(&self.0)
            .trim_matches(|c: char| c == ' ' || c == '\0')
            .to_string()
    }

    /// Reads a callsign out of a wire field, sanitising as
    /// [`Callsign::to_trimmed_string`] does.
    #[must_use]
    fn from_field(field: &[u8]) -> Callsign {
        let mut bytes = [b' '; CALLSIGN_LEN];
        for (slot, &b) in bytes.iter_mut().zip(field) {
            *slot = if (0x20..0x7F).contains(&b) { b } else { b' ' };
        }
        Callsign(bytes)
    }
}

/// One received packet, as far as a client cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// A poll. From a reflector this is the acknowledgement that a link is up.
    Poll { callsign: Callsign },
    /// An unlink.
    Unlink { callsign: Callsign },
    /// A radio frame with its routing.
    Data(Box<DataPacket>),
    /// An options string.
    Options,
    /// A status request or reply.
    Status,
    /// An information message.
    Info,
    /// A packet this build does not recognise, kept only as its tag so a
    /// caller can say what it saw.
    Unknown([u8; 4]),
}

/// A `YSFD` packet: one 120-byte frame plus who it is from and where it
/// is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPacket {
    /// The gateway or node that put this frame on the network.
    pub gateway: Callsign,
    /// Who is talking.
    pub source: Callsign,
    /// Who they are talking to; usually blank or `ALL`.
    pub destination: Callsign,
    /// Frame counter, 0..=127, wrapping.
    pub counter: u8,
    /// Set on the last frame of a transmission.
    pub end: bool,
    /// The radio frame itself.
    pub frame: [u8; FRAME_LEN],
}

/// Parses a received datagram.
///
/// Returns `None` for a datagram too short to carry a tag, or one whose tag
/// is known but whose length is wrong — a truncated `YSFD` is not a frame
/// with some bytes missing, it is not a frame.
#[must_use]
pub fn parse(datagram: &[u8]) -> Option<Packet> {
    if datagram.len() < 4 {
        return None;
    }
    let tag: [u8; 4] = datagram[..4].try_into().ok()?;
    match &tag {
        TAG_POLL | TAG_UNLINK => {
            if datagram.len() < POLL_LEN {
                return None;
            }
            let callsign = Callsign::from_field(&datagram[4..POLL_LEN]);
            Some(if &tag == TAG_POLL {
                Packet::Poll { callsign }
            } else {
                Packet::Unlink { callsign }
            })
        }
        TAG_DATA => {
            if datagram.len() < DATA_LEN {
                return None;
            }
            let mut frame = [0u8; FRAME_LEN];
            frame.copy_from_slice(&datagram[35..DATA_LEN]);
            Some(Packet::Data(Box::new(DataPacket {
                gateway: Callsign::from_field(&datagram[4..14]),
                source: Callsign::from_field(&datagram[14..24]),
                destination: Callsign::from_field(&datagram[24..34]),
                counter: (datagram[34] >> 1) & 0x7F,
                end: datagram[34] & 0x01 == 1,
                frame,
            })))
        }
        TAG_OPTIONS => Some(Packet::Options),
        TAG_STATUS => Some(Packet::Status),
        TAG_INFO => Some(Packet::Info),
        _ => Some(Packet::Unknown(tag)),
    }
}

/// Builds a poll: "let me in", and then "I am still here" every few seconds.
#[must_use]
pub fn poll(callsign: &Callsign) -> [u8; POLL_LEN] {
    tagged(*TAG_POLL, callsign)
}

/// Builds an unlink.
#[must_use]
pub fn unlink(callsign: &Callsign) -> [u8; POLL_LEN] {
    tagged(*TAG_UNLINK, callsign)
}

fn tagged(tag: [u8; 4], callsign: &Callsign) -> [u8; POLL_LEN] {
    let mut out = [0u8; POLL_LEN];
    out[..4].copy_from_slice(&tag);
    out[4..].copy_from_slice(callsign.as_bytes());
    out
}

/// Builds an options packet. `options` is truncated to fit and space padded.
#[must_use]
pub fn options(callsign: &Callsign, options: &str) -> [u8; OPTIONS_LEN] {
    let mut out = [b' '; OPTIONS_LEN];
    out[..4].copy_from_slice(TAG_OPTIONS);
    out[4..4 + CALLSIGN_LEN].copy_from_slice(callsign.as_bytes());
    let room = OPTIONS_LEN - 4 - CALLSIGN_LEN;
    let text = options.as_bytes();
    let n = text.len().min(room);
    out[4 + CALLSIGN_LEN..4 + CALLSIGN_LEN + n].copy_from_slice(&text[..n]);
    out
}

/// Builds a data packet.
#[must_use]
pub fn data(packet: &DataPacket) -> [u8; DATA_LEN] {
    let mut out = [0u8; DATA_LEN];
    out[..4].copy_from_slice(TAG_DATA);
    out[4..14].copy_from_slice(packet.gateway.as_bytes());
    out[14..24].copy_from_slice(packet.source.as_bytes());
    out[24..34].copy_from_slice(packet.destination.as_bytes());
    out[34] = ((packet.counter & 0x7F) << 1) | u8::from(packet.end);
    out[35..].copy_from_slice(&packet.frame);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Callsign {
        Callsign::new(s).expect("a legal callsign")
    }

    #[test]
    fn a_callsign_is_space_padded_to_ten() {
        assert_eq!(call("KC0ABC").as_bytes(), b"KC0ABC    ");
        assert_eq!(call("").as_bytes(), b"          ");
        assert_eq!(call("0123456789").as_bytes(), b"0123456789");
    }

    #[test]
    fn an_over_long_or_unprintable_callsign_is_refused() {
        assert_eq!(Callsign::new("01234567890"), Err(CallsignError::TooLong));
        assert_eq!(
            Callsign::new("KC0\nABC"),
            Err(CallsignError::NotPrintableAscii)
        );
        assert_eq!(
            Callsign::new("KC0\u{0}ABC"),
            Err(CallsignError::NotPrintableAscii)
        );
    }

    #[test]
    fn a_peers_control_bytes_are_scrubbed_not_refused() {
        let field = [b'K', b'C', 0x00, b'A', 0x1B, b'C', b' ', b' ', b' ', b' '];
        assert_eq!(Callsign::from_field(&field).to_trimmed_string(), "KC A C");
    }

    #[test]
    fn a_poll_round_trips() {
        let bytes = poll(&call("KC0ABC"));
        assert_eq!(bytes.len(), 14);
        assert_eq!(&bytes[..4], b"YSFP");
        assert_eq!(
            parse(&bytes),
            Some(Packet::Poll {
                callsign: call("KC0ABC")
            })
        );
    }

    #[test]
    fn an_unlink_round_trips() {
        let bytes = unlink(&call("KC0ABC"));
        assert_eq!(&bytes[..4], b"YSFU");
        assert_eq!(
            parse(&bytes),
            Some(Packet::Unlink {
                callsign: call("KC0ABC")
            })
        );
    }

    fn sample_data() -> DataPacket {
        let mut frame = [0u8; FRAME_LEN];
        for (i, slot) in frame.iter_mut().enumerate() {
            *slot = u8::try_from(i % 256).unwrap_or(0);
        }
        DataPacket {
            gateway: call("KC0ABC"),
            source: call("W1AW"),
            destination: call("ALL"),
            counter: 37,
            end: false,
            frame,
        }
    }

    #[test]
    fn a_data_packet_round_trips() {
        let packet = sample_data();
        let bytes = data(&packet);
        assert_eq!(bytes.len(), 155);
        assert_eq!(&bytes[..4], b"YSFD");
        assert_eq!(parse(&bytes), Some(Packet::Data(Box::new(packet))));
    }

    #[test]
    fn the_end_flag_and_counter_share_a_byte() {
        let mut packet = sample_data();
        packet.counter = 127;
        packet.end = true;
        let bytes = data(&packet);
        assert_eq!(bytes[34], 0xFF);
        let Some(Packet::Data(parsed)) = parse(&bytes) else {
            panic!("expected data");
        };
        assert_eq!(parsed.counter, 127);
        assert!(parsed.end);
    }

    #[test]
    fn a_counter_that_would_overflow_the_field_is_masked_not_smeared() {
        // Seven bits is what the wire has. A caller counting past 127 must
        // not be able to set the end-of-transmission flag by accident.
        let mut packet = sample_data();
        packet.counter = 0xFF;
        packet.end = false;
        let bytes = data(&packet);
        assert_eq!(bytes[34] & 0x01, 0, "end flag must stay clear");
    }

    #[test]
    fn a_truncated_packet_is_not_half_a_packet() {
        let bytes = data(&sample_data());
        assert_eq!(parse(&bytes[..DATA_LEN - 1]), None);
        assert_eq!(parse(&poll(&call("KC0ABC"))[..13]), None);
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(b"YSF"), None);
    }

    #[test]
    fn the_chatty_tags_are_recognised_and_ignorable() {
        assert_eq!(parse(b"YSFI and then some"), Some(Packet::Info));
        assert_eq!(parse(b"YSFS"), Some(Packet::Status));
        assert_eq!(
            parse(&options(&call("KC0ABC"), "hello")),
            Some(Packet::Options)
        );
        assert_eq!(parse(b"WXYZ...."), Some(Packet::Unknown(*b"WXYZ")));
    }

    #[test]
    fn options_are_padded_and_truncated_to_the_wire_size() {
        let bytes = options(&call("KC0ABC"), "room 42");
        assert_eq!(bytes.len(), 50);
        assert_eq!(&bytes[..4], b"YSFO");
        assert_eq!(&bytes[4..14], b"KC0ABC    ");
        assert_eq!(&bytes[14..21], b"room 42");
        assert!(bytes[21..].iter().all(|&b| b == b' '));

        let long = options(&call("KC0ABC"), &"x".repeat(200));
        assert_eq!(long.len(), 50);
        assert!(long[14..].iter().all(|&b| b == b'x'));
    }
}
