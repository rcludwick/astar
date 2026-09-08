// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `NXDNReflector` datagrams: what goes into a UDP datagram and what
//! comes back out.
//!
//! Every datagram opens with a five-byte tag, and there are only three of
//! them:
//!
//! | tag | bytes | direction | meaning |
//! |---|---|---|---|
//! | `NXDNP` | 17 | both | poll — register, stay registered, and, echoed back, the reply that says you are |
//! | `NXDNU` | 17 | to reflector | unlink |
//! | `NXDND` | 43 | both | one 33-byte network frame, with routing and flags |
//!
//! Two things separate this from `YSFReflector`'s wire. A poll carries the
//! **talkgroup** it is for — the reflector answers only polls for its own
//! (`NXDNReflector.cpp`: `unsigned short id = (buffer[15U] << 8) |
//! buffer[16U]; if (id == tg)`) — and there is no acknowledgement packet at
//! all: the reflector returns the client's own poll verbatim.
//!
//! Lengths are exact. `NXDNReflector/NXDNNetwork.cpp: CNXDNNetwork::read`
//! accepts 17 and 43 and nothing else, and `NXDNGateway/NXDNNetwork.cpp:
//! CNXDNNetwork::readData` is stricter still, pairing each tag with its one
//! length. So is [`parse`]: a truncated `NXDND` is not a frame with bytes
//! missing, it is not a frame.
//!
//! Callsigns are exactly ten bytes, space padded, never NUL terminated —
//! `CNXDNNetwork`'s constructor does `m_callsign.resize(10U, \' \')`.

/// Bytes in a callsign field.
pub const CALLSIGN_LEN: usize = 10;
/// Bytes in a poll or unlink datagram: tag, callsign, talkgroup.
pub const POLL_LEN: usize = 5 + CALLSIGN_LEN + 2;
/// Bytes in the network frame a data datagram carries.
///
/// Not the 384-bit over-the-air RTCH frame: `MMDVMHost` strips the frame sync,
/// the FEC and the interleave before a frame goes on the network. See
/// `docs/design/nxdn-wire.md`.
pub const FRAME_LEN: usize = 33;
/// Bytes in a data datagram: tag, two ids, flags, and the frame.
pub const DATA_LEN: usize = 5 + 2 + 2 + 1 + FRAME_LEN;

/// Every tag on this wire starts with these four bytes;
/// `CNXDNNetwork::read` refuses a datagram that does not.
const TAG_PREFIX: &[u8; 4] = b"NXDN";
const TAG_POLL: &[u8; 5] = b"NXDNP";
const TAG_UNLINK: &[u8; 5] = b"NXDNU";
const TAG_DATA: &[u8; 5] = b"NXDND";

/// `NXDND` flag bits, byte 9. `CNXDNNetwork::writeData`
/// (`NXDNGateway/NXDNNetwork.cpp`) sets them; `NXDNReflector.cpp` reads
/// `0x01` and `0x08` back out.
const FLAG_GROUP: u8 = 0x01;
const FLAG_DATA: u8 = 0x02;
const FLAG_START: u8 = 0x04;
const FLAG_END: u8 = 0x08;

/// A ten-byte, space-padded callsign field.
///
/// `astar_ysf::wire::Callsign` is the same idea with the same rules, written
/// out again rather than shared: neither crate depends on the other, and a
/// dependency in either direction to save forty lines would buy nothing.
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

impl std::fmt::Display for CallsignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => write!(f, "longer than {CALLSIGN_LEN} characters"),
            Self::NotPrintableAscii => write!(f, "contains a non-printable or non-ASCII character"),
        }
    }
}

impl std::error::Error for CallsignError {}

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
    #[must_use]
    pub fn to_trimmed_string(&self) -> String {
        String::from_utf8_lossy(&self.0)
            .trim_matches(|c: char| c == ' ' || c == '\0')
            .to_string()
    }

    /// Reads a callsign out of a wire field, replacing anything outside
    /// printable ASCII with a space.
    ///
    /// Inbound is deliberately more forgiving than [`Callsign::new`]: a
    /// neighbour's malformed callsign is a thing to display, not a reason to
    /// drop their audio.
    fn from_field(field: &[u8]) -> Callsign {
        let mut bytes = [b' '; CALLSIGN_LEN];
        for (slot, &b) in bytes.iter_mut().zip(field) {
            *slot = if (0x20..0x7F).contains(&b) { b } else { b' ' };
        }
        Callsign(bytes)
    }
}

/// An `NXDND` datagram: one 33-byte network frame, who it is from, where it
/// is going, and where it sits in a transmission.
// Four bools is four too many for a general-purpose struct and exactly right
// for this one: they are the four bits of the wire's flags byte, and folding
// them into a bitfield would only make callers do the masking the parser
// already did.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPacket {
    /// Who is talking. Sixteen bits — `NXDNGateway/Reflectors.h:
    /// CNXDNReflector::m_id` is an `unsigned short`.
    pub src_id: u16,
    /// Talkgroup for a group call, or the called station for a private one.
    pub dst_id: u16,
    /// Group call rather than private. The reflector relays only when this
    /// is set (`NXDNReflector.cpp`: `if (grp && dstId == tg)`).
    pub group: bool,
    /// Data rather than voice.
    pub data: bool,
    /// First frame of a transmission — the `VCALL` voice header.
    pub start: bool,
    /// Last frame of a transmission — the `TX_REL` trailer.
    pub end: bool,
    /// The network frame itself, carried and not decoded.
    pub frame: [u8; FRAME_LEN],
}

/// One received datagram, as far as a client cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// A poll. Echoed back by a reflector, this is the whole handshake.
    Poll {
        /// Who sent it.
        callsign: Callsign,
        /// The talkgroup it is for.
        talkgroup: u16,
    },
    /// An unlink.
    Unlink {
        /// Who sent it.
        callsign: Callsign,
        /// The talkgroup it is leaving.
        talkgroup: u16,
    },
    /// A network frame with its routing.
    Data(Box<DataPacket>),
    /// A datagram of an accepted length whose fifth tag byte this build does
    /// not recognise, kept only as its tag so a caller can say what it saw.
    Unknown([u8; 5]),
}

/// Parses a received datagram.
///
/// Returns `None` for anything that is not one of the three known tags at
/// its own exact length: `CNXDNNetwork::readData` pairs `"NXDNP"` with 17
/// and `"NXDND"` with 43 and refuses everything else, and there is nothing
/// useful to do with half a frame.
#[must_use]
pub fn parse(datagram: &[u8]) -> Option<Packet> {
    if datagram.len() < 5 || &datagram[..4] != TAG_PREFIX {
        return None;
    }
    let tag: [u8; 5] = datagram[..5].try_into().ok()?;
    let len = datagram.len();
    match &tag {
        TAG_POLL | TAG_UNLINK if len == POLL_LEN => {
            let callsign = Callsign::from_field(&datagram[5..15]);
            let talkgroup = u16::from_be_bytes([datagram[15], datagram[16]]);
            Some(if &tag == TAG_POLL {
                Packet::Poll {
                    callsign,
                    talkgroup,
                }
            } else {
                Packet::Unlink {
                    callsign,
                    talkgroup,
                }
            })
        }
        TAG_DATA if len == DATA_LEN => {
            let mut frame = [0u8; FRAME_LEN];
            frame.copy_from_slice(&datagram[10..DATA_LEN]);
            let flags = datagram[9];
            Some(Packet::Data(Box::new(DataPacket {
                src_id: u16::from_be_bytes([datagram[5], datagram[6]]),
                dst_id: u16::from_be_bytes([datagram[7], datagram[8]]),
                group: flags & FLAG_GROUP != 0,
                data: flags & FLAG_DATA != 0,
                start: flags & FLAG_START != 0,
                end: flags & FLAG_END != 0,
                frame,
            })))
        }
        // A known tag at the wrong length is not a short packet, it is not
        // a packet.
        TAG_POLL | TAG_UNLINK | TAG_DATA => None,
        _ if len == POLL_LEN || len == DATA_LEN => Some(Packet::Unknown(tag)),
        _ => None,
    }
}

/// Builds a poll: "let me into this talkgroup", and then "I am still here"
/// every few seconds.
#[must_use]
pub fn poll(callsign: &Callsign, talkgroup: u16) -> [u8; POLL_LEN] {
    tagged(*TAG_POLL, callsign, talkgroup)
}

/// Builds an unlink.
#[must_use]
pub fn unlink(callsign: &Callsign, talkgroup: u16) -> [u8; POLL_LEN] {
    tagged(*TAG_UNLINK, callsign, talkgroup)
}

fn tagged(tag: [u8; 5], callsign: &Callsign, talkgroup: u16) -> [u8; POLL_LEN] {
    let mut out = [0u8; POLL_LEN];
    out[..5].copy_from_slice(&tag);
    out[5..15].copy_from_slice(callsign.as_bytes());
    out[15..].copy_from_slice(&talkgroup.to_be_bytes());
    out
}

/// Builds a data datagram.
#[must_use]
pub fn data(packet: &DataPacket) -> [u8; DATA_LEN] {
    let mut out = [0u8; DATA_LEN];
    out[..5].copy_from_slice(TAG_DATA);
    out[5..7].copy_from_slice(&packet.src_id.to_be_bytes());
    out[7..9].copy_from_slice(&packet.dst_id.to_be_bytes());
    let mut flags = 0u8;
    if packet.group {
        flags |= FLAG_GROUP;
    }
    if packet.data {
        flags |= FLAG_DATA;
    }
    if packet.start {
        flags |= FLAG_START;
    }
    if packet.end {
        flags |= FLAG_END;
    }
    out[9] = flags;
    out[10..].copy_from_slice(&packet.frame);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(s: &str) -> Callsign {
        Callsign::new(s).expect("a legal callsign")
    }

    #[test]
    fn a_poll_is_seventeen_bytes_of_tag_callsign_and_talkgroup() {
        // NXDNGateway/NXDNNetwork.cpp: CNXDNNetwork::writePoll — tag at 0..5,
        // ten space-padded callsign bytes at 5..15, big-endian TG at 15..17.
        let bytes = poll(&call("KC0ABC"), 31313);
        assert_eq!(bytes.len(), 17);
        assert_eq!(&bytes[..5], b"NXDNP");
        assert_eq!(&bytes[5..15], b"KC0ABC    ");
        assert_eq!(bytes[15], 0x7A);
        assert_eq!(bytes[16], 0x51);
        assert_eq!(
            parse(&bytes),
            Some(Packet::Poll {
                callsign: call("KC0ABC"),
                talkgroup: 31313
            })
        );
    }

    #[test]
    fn an_unlink_differs_from_a_poll_only_in_its_tag() {
        // CNXDNNetwork::writeUnlink is writePoll with 'U' for 'P'.
        let p = poll(&call("W1AW"), 100);
        let u = unlink(&call("W1AW"), 100);
        assert_eq!(&u[..5], b"NXDNU");
        assert_eq!(&p[5..], &u[5..]);
        assert_eq!(
            parse(&u),
            Some(Packet::Unlink {
                callsign: call("W1AW"),
                talkgroup: 100
            })
        );
    }

    fn sample_data() -> DataPacket {
        let mut frame = [0u8; FRAME_LEN];
        for (i, slot) in frame.iter_mut().enumerate() {
            *slot = u8::try_from(i + 1).expect("small");
        }
        // Ids on this wire are sixteen bits — `NXDNGateway/Reflectors.h:
        // CNXDNReflector::m_id` is an `unsigned short` — so a six-digit id
        // borrowed from another network does not fit. 3153591 truncated to
        // what the wire has is 7863.
        DataPacket {
            src_id: 7863,
            dst_id: 31313,
            group: true,
            data: false,
            start: false,
            end: false,
            frame,
        }
    }

    #[test]
    fn a_data_packet_is_forty_three_bytes_and_round_trips() {
        // CNXDNNetwork::writeData: tag(5), srcId BE @5, dstId BE @7,
        // flags @9, 33 frame bytes @10.
        let packet = sample_data();
        let bytes = data(&packet);
        assert_eq!(bytes.len(), 43);
        assert_eq!(&bytes[..5], b"NXDND");
        assert_eq!(u16::from_be_bytes([bytes[5], bytes[6]]), packet.src_id);
        assert_eq!(u16::from_be_bytes([bytes[7], bytes[8]]), 31313);
        assert_eq!(bytes[9], 0x01);
        assert_eq!(&bytes[10..], &packet.frame);
        assert_eq!(parse(&bytes), Some(Packet::Data(Box::new(packet))));
    }

    #[test]
    fn the_flag_bits_are_the_ones_the_reflector_reads() {
        // NXDNReflector.cpp reads `(buffer[9U] & 0x08U) == 0x08U` for end of
        // transmission; CNXDNNetwork::writeData sets 0x01 grp, 0x02 data,
        // 0x04 start, 0x08 end.
        let mut p = sample_data();
        p.group = true;
        p.data = true;
        p.start = true;
        p.end = true;
        assert_eq!(data(&p)[9], 0x0F);
        let Some(Packet::Data(parsed)) = parse(&data(&p)) else {
            panic!("data")
        };
        assert!(parsed.group && parsed.data && parsed.start && parsed.end);
    }

    #[test]
    fn only_seventeen_and_forty_three_byte_datagrams_are_packets() {
        // NXDNReflector/NXDNNetwork.cpp: CNXDNNetwork::read refuses every
        // other length outright. A truncated NXDND is not a frame with bytes
        // missing; it is not a frame.
        let bytes = data(&sample_data());
        assert_eq!(parse(&bytes[..42]), None);
        assert_eq!(parse(&poll(&call("W1AW"), 1)[..16]), None);
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(b"NXDN"), None);
    }

    #[test]
    fn an_unknown_tag_is_kept_as_its_tag() {
        let mut d = [0u8; 17];
        d[..5].copy_from_slice(b"NXDNX");
        assert_eq!(parse(&d), Some(Packet::Unknown(*b"NXDNX")));
    }

    #[test]
    fn a_peers_control_bytes_are_scrubbed_not_refused() {
        // A neighbour's malformed callsign is a thing to display, not a
        // reason to drop their audio. Same rule astar_ysf::wire holds.
        let mut d = [b' '; 17];
        d[..5].copy_from_slice(b"NXDNP");
        d[5..15].copy_from_slice(&[b'K', b'C', 0x00, b'A', 0x1B, b'C', b' ', b' ', b' ', b' ']);
        let Some(Packet::Poll { callsign, .. }) = parse(&d) else {
            panic!("poll")
        };
        assert_eq!(callsign.to_trimmed_string(), "KC A C");
    }

    #[test]
    fn an_over_long_or_unprintable_callsign_is_refused() {
        assert_eq!(Callsign::new("01234567890"), Err(CallsignError::TooLong));
        assert_eq!(
            Callsign::new("KC0\nABC"),
            Err(CallsignError::NotPrintableAscii)
        );
    }
}
