// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `P25Reflector` datagrams: what goes into a UDP datagram and what
//! comes back out.
//!
//! The first byte of every datagram is its tag, and the tag alone says what
//! the datagram is:
//!
//! | tag | bytes | direction | meaning |
//! |---|---|---|---|
//! | `F0` | 11 | both | poll: register, stay registered, and, echoed back, the reply that says you are |
//! | `F1` | 11 | to reflector | unlink |
//! | `62`–`73` | 14–22 | both | one voice record, carrying one IMBE frame (see [`crate::ldu`]) |
//! | `80` | 17 | both | end of transmission |
//!
//! There is no login and no handshake. A poll is `F0` and the callsign as
//! ten space-padded ASCII bytes (`P25Clients/P25Gateway/P25Network.cpp`,
//! `CP25Network::poll`); an unlink is the same with `F1`
//! (`CP25Network::unlink`). The reflector echoes a poll back verbatim
//! (`DVReflectors/P25Reflector/P25Reflector.cpp`), and that echo is the only
//! acknowledgement there is.
//!
//! Lengths are exact. A datagram whose tag is known but whose length is not
//! that tag's length is not a short record, it is not a record: [`parse`]
//! reports it as [`Packet::Unknown`]. Every byte of a datagram comes from
//! the internet, so nothing here indexes past what it has checked.

use crate::ldu::{self, VoiceRecord};

/// Bytes in a callsign field.
pub const CALLSIGN_LEN: usize = 10;
/// Bytes in a poll or unlink datagram: the tag, then the callsign.
pub const POLL_LEN: usize = 1 + CALLSIGN_LEN;
/// Bytes in the end-of-transmission record: `0x80`, then sixteen zeros
/// (`MMDVMHost/P25Network.cpp`, the `REC80` array, which
/// `CP25Network::writeLDU1` and `writeLDU2` send after the last LDU).
pub const END_LEN: usize = 17;
/// The poll tag (`P25Clients/P25Gateway/P25Network.cpp`, `CP25Network::poll`).
pub const TAG_POLL: u8 = 0xF0;
/// The unlink tag (`P25Clients/P25Gateway/P25Network.cpp`,
/// `CP25Network::unlink`).
pub const TAG_UNLINK: u8 = 0xF1;
/// The end-of-transmission tag (`MMDVMHost/P25Network.cpp`, `REC80`, sent
/// from `CP25Network::writeLDU1` and `writeLDU2`; `P25Reflector.cpp`
/// releases the talker on it).
pub const TAG_END: u8 = 0x80;
/// The port P25 reflectors listen on (`Port=41000` in
/// `DVReflectors/P25Reflector/P25Reflector.ini`; the published host list,
/// which `P25Clients/P25Gateway/P25HostsUpdate.sh` downloads, also has a
/// few on 41001–41003).
pub const DEFAULT_PORT: u16 = 41000;

/// A ten-byte, space-padded callsign field.
///
/// `astar_nxdn::wire::Callsign` is the same idea, written out again rather
/// than shared: neither crate depends on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Callsign([u8; CALLSIGN_LEN]);

/// Why a callsign could not be used on this wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallsignError {
    /// Nothing but spaces, or nothing at all.
    Empty,
    /// Longer than the wire format's ten-byte field.
    TooLong,
    /// Contains a byte outside printable ASCII.
    NotPrintableAscii,
}

impl std::fmt::Display for CallsignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty"),
            Self::TooLong => write!(f, "longer than {CALLSIGN_LEN} characters"),
            Self::NotPrintableAscii => write!(f, "contains a non-printable or non-ASCII character"),
        }
    }
}

impl std::error::Error for CallsignError {}

impl Callsign {
    /// Builds a callsign field, space padding to ten bytes.
    ///
    /// The rule is 1–10 printable ASCII bytes (`0x20`–`0x7E`). A callsign of
    /// only spaces counts as empty: on the wire it would be indistinguishable
    /// from padding. Case is kept as given; the reflector echoes the poll
    /// byte for byte, so whatever is sent is what comes back.
    pub fn new(callsign: &str) -> Result<Callsign, CallsignError> {
        if callsign.trim_matches(' ').is_empty() {
            return Err(CallsignError::Empty);
        }
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
    /// peer's malformed callsign is a thing to display, not a reason to drop
    /// the datagram.
    fn from_field(field: &[u8]) -> Callsign {
        let mut bytes = [b' '; CALLSIGN_LEN];
        for (slot, &b) in bytes.iter_mut().zip(field) {
            *slot = if (0x20..0x7F).contains(&b) { b } else { b' ' };
        }
        Callsign(bytes)
    }
}

/// One received datagram, as far as a client cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// A poll. Echoed back by a reflector, this is the whole handshake.
    Poll(Callsign),
    /// An unlink.
    Unlink(Callsign),
    /// One of the eighteen voice records of a superframe.
    Voice(VoiceRecord),
    /// The end-of-transmission record.
    End,
    /// Anything else: an unrecognised tag, or a known tag at a length that
    /// is not its own. Kept only as its tag, so a caller can say what it saw.
    Unknown(u8),
}

/// Parses a received datagram.
///
/// Returns `None` only for an empty datagram. A known tag at exactly its own
/// length becomes that packet; everything else becomes
/// [`Packet::Unknown`] with its first byte. Never panics, whatever the
/// input.
#[must_use]
pub fn parse(datagram: &[u8]) -> Option<Packet> {
    let (&tag, body) = datagram.split_first()?;
    let len = datagram.len();
    let packet = match tag {
        TAG_POLL if len == POLL_LEN => Packet::Poll(Callsign::from_field(body)),
        TAG_UNLINK if len == POLL_LEN => Packet::Unlink(Callsign::from_field(body)),
        TAG_END if len == END_LEN => Packet::End,
        ldu::FIRST_TAG..=ldu::LAST_TAG => match VoiceRecord::decode(datagram) {
            Some(record) => Packet::Voice(record),
            None => Packet::Unknown(tag),
        },
        _ => Packet::Unknown(tag),
    };
    Some(packet)
}

/// Builds a poll: "let me in", and then "I am still here" every five
/// seconds.
#[must_use]
pub fn poll(callsign: &Callsign) -> [u8; POLL_LEN] {
    tagged(TAG_POLL, callsign)
}

/// Builds an unlink.
#[must_use]
pub fn unlink(callsign: &Callsign) -> [u8; POLL_LEN] {
    tagged(TAG_UNLINK, callsign)
}

fn tagged(tag: u8, callsign: &Callsign) -> [u8; POLL_LEN] {
    let mut out = [0u8; POLL_LEN];
    out[0] = tag;
    out[1..].copy_from_slice(callsign.as_bytes());
    out
}

/// Builds the end-of-transmission record: `0x80` and sixteen zeros.
#[must_use]
pub fn end_record() -> [u8; END_LEN] {
    let mut out = [0u8; END_LEN];
    out[0] = TAG_END;
    out
}

/// True when `datagram` is media for the packet log: a voice record tag
/// (`0x62`–`0x73`) or the end record (`0x80`). Looks at the tag only.
#[must_use]
pub fn is_media(datagram: &[u8]) -> bool {
    datagram
        .first()
        .is_some_and(|&t| (ldu::FIRST_TAG..=ldu::LAST_TAG).contains(&t) || t == TAG_END)
}

/// Why a reflector address could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressError {
    /// No host: an empty string, only spaces, `:41000` or `[]`.
    Empty,
    /// A colon with no port after it, as in `host:` or `[::1]:`.
    MissingPort,
    /// A port that is not a number from 1 to 65535, or something other than
    /// `:port` after a closing bracket.
    BadPort,
    /// An IPv6 literal opened with `[` and never closed.
    UnclosedBracket,
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the reflector address has no host"),
            Self::MissingPort => write!(f, "the reflector address ends in a colon with no port"),
            Self::BadPort => write!(f, "the reflector port is not a number from 1 to 65535"),
            Self::UnclosedBracket => write!(f, "the IPv6 address is missing its closing ]"),
        }
    }
}

impl std::error::Error for AddressError {}

/// Adds [`DEFAULT_PORT`] to a reflector address that has none.
///
/// `host:port` and `[v6]:port` come back unchanged; `host` and `[v6]` get
/// `:41000`; a bare IPv6 literal (more than one colon, no brackets) is
/// bracketed first, so `::1` becomes `[::1]:41000`.
///
/// # Errors
/// A malformed address is refused rather than passed on: an empty host
/// (`""`, `":41000"`, `"[]"`), a colon with no port (`"host:"`), a port that
/// is not 1–65535, or an unclosed bracket (`"[::1"`).
pub fn with_default_port(host: &str) -> Result<String, AddressError> {
    if host.trim().is_empty() {
        return Err(AddressError::Empty);
    }
    if let Some(rest) = host.strip_prefix('[') {
        let (inside, after) = rest.split_once(']').ok_or(AddressError::UnclosedBracket)?;
        if inside.trim().is_empty() {
            return Err(AddressError::Empty);
        }
        return match after {
            "" => Ok(format!("{host}:{DEFAULT_PORT}")),
            _ => match after.strip_prefix(':') {
                Some(port) => check_port(port).map(|()| host.to_string()),
                None => Err(AddressError::BadPort),
            },
        };
    }
    match host.matches(':').count() {
        0 => Ok(format!("{host}:{DEFAULT_PORT}")),
        1 => {
            let (name, port) = host.split_once(':').ok_or(AddressError::MissingPort)?;
            if name.trim().is_empty() {
                return Err(AddressError::Empty);
            }
            check_port(port).map(|()| host.to_string())
        }
        _ => Ok(format!("[{host}]:{DEFAULT_PORT}")),
    }
}

fn check_port(port: &str) -> Result<(), AddressError> {
    if port.is_empty() {
        return Err(AddressError::MissingPort);
    }
    match port.parse::<u16>() {
        Ok(p) if p != 0 => Ok(()),
        _ => Err(AddressError::BadPort),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ldu::{VoiceRecord, record_len};

    fn call(s: &str) -> Callsign {
        Callsign::new(s).expect("a legal callsign")
    }

    #[test]
    fn a_poll_is_f0_and_ten_space_padded_bytes() {
        assert_eq!(&poll(&call("AJ7HR")), b"\xF0AJ7HR     ");
        assert_eq!(
            parse(&poll(&call("AJ7HR"))),
            Some(Packet::Poll(call("AJ7HR")))
        );
    }

    #[test]
    fn an_unlink_is_f1_and_the_same_callsign() {
        let u = unlink(&call("AJ7HR"));
        assert_eq!(&u, b"\xF1AJ7HR     ");
        assert_eq!(parse(&u), Some(Packet::Unlink(call("AJ7HR"))));
    }

    #[test]
    fn the_end_record_is_seventeen_bytes_of_0x80_then_zeros() {
        let e = end_record();
        assert_eq!(e.len(), 17);
        assert_eq!(e[0], 0x80);
        assert!(e[1..].iter().all(|&b| b == 0));
        assert_eq!(parse(&e), Some(Packet::End));
    }

    #[test]
    fn callsigns_follow_the_one_rule() {
        assert_eq!(Callsign::new(""), Err(CallsignError::Empty));
        assert_eq!(Callsign::new("AJ7HR/MOBIL"), Err(CallsignError::TooLong));
        assert_eq!(
            Callsign::new("AJ7HR\n"),
            Err(CallsignError::NotPrintableAscii)
        );
        assert!(Callsign::new("aj7hr").is_ok());
        assert!(Callsign::new("0123456789").is_ok());
    }

    #[test]
    fn callsign_errors_give_a_reason_a_person_can_act_on() {
        assert_eq!(
            CallsignError::TooLong.to_string(),
            "longer than 10 characters"
        );
        assert_eq!(CallsignError::Empty.to_string(), "empty");
        assert!(
            CallsignError::NotPrintableAscii
                .to_string()
                .contains("non-printable")
        );
    }

    #[test]
    fn a_peers_control_bytes_are_scrubbed_not_refused() {
        let mut d = [b' '; POLL_LEN];
        d[0] = TAG_POLL;
        d[1..].copy_from_slice(&[b'K', b'C', 0x00, b'A', 0x1B, b'C', 0xFF, b' ', b' ', b' ']);
        let Some(Packet::Poll(c)) = parse(&d) else {
            panic!("poll")
        };
        assert_eq!(c.to_trimmed_string(), "KC A C");
    }

    #[test]
    fn with_default_port_adds_41000_only_when_missing() {
        let ok = |s: &str| with_default_port(s).expect("a well-formed address");
        assert_eq!(ok("p25.example"), "p25.example:41000");
        assert_eq!(ok("p25.example:41001"), "p25.example:41001");
        assert_eq!(ok("::1"), "[::1]:41000");
        assert_eq!(ok("[::1]:41002"), "[::1]:41002");
        assert_eq!(ok("[::1]"), "[::1]:41000");
        assert_eq!(ok("127.0.0.1"), "127.0.0.1:41000");
    }

    #[test]
    fn with_default_port_refuses_a_malformed_address() {
        assert_eq!(with_default_port(""), Err(AddressError::Empty));
        assert_eq!(with_default_port("   "), Err(AddressError::Empty));
        assert_eq!(with_default_port(":41000"), Err(AddressError::Empty));
        assert_eq!(with_default_port("[]:41000"), Err(AddressError::Empty));
        assert_eq!(with_default_port("host:"), Err(AddressError::MissingPort));
        assert_eq!(with_default_port("[::1]:"), Err(AddressError::MissingPort));
        assert_eq!(
            with_default_port("[::1"),
            Err(AddressError::UnclosedBracket)
        );
        assert_eq!(with_default_port("host:abc"), Err(AddressError::BadPort));
        assert_eq!(with_default_port("host:70000"), Err(AddressError::BadPort));
        assert_eq!(with_default_port("host:0"), Err(AddressError::BadPort));
        assert_eq!(with_default_port("[::1]x"), Err(AddressError::BadPort));
        assert!(AddressError::MissingPort.to_string().contains("port"));
        assert!(AddressError::UnclosedBracket.to_string().contains(']'));
    }

    #[test]
    fn parse_is_none_only_for_an_empty_datagram() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0x42]), Some(Packet::Unknown(0x42)));
    }

    #[test]
    fn a_known_tag_at_the_wrong_length_is_not_that_packet() {
        // A 0x65 of 10 bytes is not a talkgroup record with bytes missing.
        let mut short = vec![0u8; 10];
        short[0] = 0x65;
        assert_eq!(parse(&short), Some(Packet::Unknown(0x65)));
        assert_eq!(
            parse(&poll(&call("W1AW"))[..10]),
            Some(Packet::Unknown(TAG_POLL))
        );
        assert_eq!(parse(&end_record()[..16]), Some(Packet::Unknown(TAG_END)));
        let mut long = end_record().to_vec();
        long.push(0);
        assert_eq!(parse(&long), Some(Packet::Unknown(TAG_END)));
    }

    #[test]
    fn parse_never_panics_on_any_tag_and_length() {
        for tag in 0u8..=255 {
            for len in 0..=64 {
                let mut d = vec![0u8; len];
                if len > 0 {
                    d[0] = tag;
                }
                match parse(&d) {
                    Some(Packet::Voice(r)) => assert_eq!(len, record_len(r.index)),
                    Some(Packet::End) => assert_eq!(len, END_LEN),
                    Some(Packet::Poll(_) | Packet::Unlink(_)) => assert_eq!(len, POLL_LEN),
                    Some(Packet::Unknown(_)) | None => {}
                }
            }
        }
    }

    /// A small deterministic generator (xorshift64*), so the sweep below is
    /// the same bytes on every run and needs no dependency.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    #[test]
    fn parse_never_panics_on_random_bytes_of_every_length() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for len in 0..=64usize {
            for _ in 0..512 {
                let mut d: Vec<u8> = (0..len)
                    .map(|_| next(&mut state).to_le_bytes()[0])
                    .collect();
                // Half the time, aim at a real tag so the voice paths are
                // exercised with garbage bodies, not just the Unknown arm.
                if len > 0 && next(&mut state) & 1 == 0 {
                    let tags = [0x62u8, 0x63, 0x65, 0x66, 0x6A, 0x70, 0x73, 0x80, 0xF0, 0xF1];
                    d[0] = tags[usize::try_from(next(&mut state) % 10).expect("small")];
                }
                let parsed = parse(&d);
                assert_eq!(parsed.is_none(), len == 0);
                if let Some(Packet::Voice(r)) = parsed {
                    assert_eq!(len, record_len(r.index));
                    assert_eq!(r.encode().len(), len);
                    let _ = (r.talkgroup(), r.source_id(), r.algid(), r.tag());
                }
                let _ = is_media(&d);
            }
        }
    }

    #[test]
    fn is_media_is_the_voice_tags_and_the_end_record() {
        for tag in 0u8..=255 {
            let expected = (0x62..=0x73).contains(&tag) || tag == 0x80;
            assert_eq!(is_media(&[tag, 0, 0]), expected, "{tag:#04x}");
        }
        assert!(!is_media(&[]));
        assert!(is_media(&end_record()));
        assert!(!is_media(&poll(&call("AJ7HR"))));
    }

    #[test]
    fn a_voice_record_parses_as_voice() {
        let rec = VoiceRecord {
            index: 1,
            imbe: [9; 11],
            fields: [0; 3],
        };
        assert_eq!(parse(&rec.encode()), Some(Packet::Voice(rec)));
    }
}
