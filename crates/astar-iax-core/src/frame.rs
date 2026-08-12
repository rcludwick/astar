// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! IAX2 frame parser and serializer (no I/O, no state).
//!
//! Two wire forms exist (RFC 5456 §6):
//!
//! 1. **Full frames** (12-byte header + IE TLV stream) — reliable, used for
//!    every control message and the first voice frame after any subclass
//!    change.
//! 2. **Mini frames** (4-byte header + raw payload, payload format and
//!    high-16-bits-of-timestamp implied by the most recent full voice
//!    frame) — unreliable, used for the steady-state media stream.
//!
//! The discriminator is the top bit of the first byte: `0x80` => full,
//! `0x00` => mini.

use bytes::BufMut;

use crate::error::{EncodeError, ParseError};
use crate::ie::Ies;
use crate::subclass::{
    ControlSubclass, FrameType, IaxCommand, VoiceFormat, encode_subclass_byte, parse_subclass_byte,
};

/// `IAX_FLAG_FULL` (and `IAX_FLAG_RETRANS`) from `iax2.h` — the high bit of
/// the first 16-bit word of any IAX2 packet.
const FLAG_FULL: u16 = 0x8000;

/// Mask for the 15-bit call number that occupies the low bits of `scallno`
/// (or the `callno` of a mini header).
const CALLNO_MASK: u16 = 0x7fff;

/// Full-frame fixed header size (excluding IE payload).
pub const FULL_HEADER_LEN: usize = 12;

/// Mini-frame fixed header size (excluding payload).
pub const MINI_HEADER_LEN: usize = 4;

/// Parsed full-frame header + decoded subclass + IE set, borrowed from the
/// input slice.
///
/// Per RFC 5456 §6.4 the bytes after the 12-byte header are interpreted
/// differently depending on `frame_type`: for [`FrameType::Voice`],
/// [`FrameType::Video`], and [`FrameType::Image`] they are raw codec
/// samples and live in [`Self::payload`]; for every other frame type they
/// are a TLV IE stream and live in [`Self::ies`]. Exactly one of the two
/// is populated per frame — the other is the empty default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullFrame<'a> {
    /// 15-bit source call number assigned by the sender.
    pub source_call: u16,
    /// 15-bit destination call number (i.e. the sender's local id from the
    /// peer's perspective).
    pub dest_call: u16,
    /// Whether the high bit of `dcallno` is set, signalling a retransmission.
    pub retransmission: bool,
    /// 32-bit timestamp, milliseconds since first transmission.
    pub timestamp: u32,
    /// Outgoing sequence number (sender's own counter).
    pub oseqno: u8,
    /// Next-expected incoming sequence number (the sender's view).
    pub iseqno: u8,
    /// Frame type (`AST_FRAME_*`).
    pub frame_type: FrameType,
    /// Logical (uncompressed) subclass value.
    pub subclass: Subclass,
    /// Decoded information elements. Empty for media-bearing frame types
    /// (Voice/Video/Image), which carry raw samples in [`Self::payload`].
    pub ies: Ies<'a>,
    /// Raw codec samples for media-bearing frame types (Voice/Video/Image).
    /// Empty for IE-bearing frame types. iax-bf0b: previously these bytes
    /// were silently dropped because the parser handed them to [`Ies::parse`].
    pub payload: &'a [u8],
}

/// Frame types whose post-header bytes are raw bytes (codec samples or text)
/// rather than an IE TLV stream. Centralised here so parse/encode/round-trip
/// all agree on the split. RFC 5456 §6.4: Voice/Video/Image carry codec
/// samples; Text carries a raw UTF-8 string (`app_rpt` link-layer protocol).
#[must_use]
pub fn carries_payload(frame_type: FrameType) -> bool {
    matches!(
        frame_type,
        FrameType::Voice | FrameType::Video | FrameType::Image | FrameType::Text
    )
}

/// Typed subclass discriminated by the surrounding frame type. Unknown
/// values for known frame types are kept as `Raw` so the parser doesn't
/// reject every new control opcode upstream Asterisk invents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subclass {
    /// `AST_FRAME_IAX` subclass (`IAX_COMMAND_*`).
    Iax(IaxCommand),
    /// `AST_FRAME_CONTROL` subclass (`AST_CONTROL_*`).
    Control(ControlSubclass),
    /// `AST_FRAME_VOICE` subclass (`AST_FORMAT_*` codec bitmask).
    Voice(VoiceFormat),
    /// `AST_FRAME_DTMF_BEGIN`/`_END` subclass: the ASCII DTMF digit
    /// (RFC 5456 §6.7, `0-9 * # A-D`). Validated at parse time so the digit
    /// reaches the FSM as a `char`, not a raw byte.
    Dtmf(char),
    /// Logical value preserved when the surrounding frame type does not
    /// use a typed subclass enum (NULL, TEXT, ...) or when the value
    /// is recognised by neither side. Callers that care can match.
    Raw(u32),
}

/// Mini frame: short header plus a borrowed payload slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniFrame<'a> {
    /// 15-bit source call number (high bit cleared per RFC 5456 §6.4).
    pub source_call: u16,
    /// Low 16 bits of the timestamp; the high 16 are inherited from the
    /// last full voice frame seen on this call.
    pub timestamp: u16,
    /// Payload bytes — borrowed from the input.
    pub payload: &'a [u8],
}

/// Borrowed top-level frame discriminator.
///
/// `FullFrame` is large (~600 bytes — it carries the full typed [`Ies`]
/// struct with one `Option` per known IE), so we box it to keep the
/// `Frame` enum small. Boxing here costs one allocation per parsed full
/// frame; that's an acceptable trade vs. the alternative of a dispatch
/// table on every IE field access, and the IE string/byte slices still
/// borrow into the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame<'a> {
    Full(Box<FullFrame<'a>>),
    Mini(MiniFrame<'a>),
}

/// Owned counterpart of [`Frame`] for handoff across borrow boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedFrame {
    Full(OwnedFullFrame),
    Mini(OwnedMiniFrame),
}

/// Owned full frame. IE strings/bytes are copied into owned buffers.
///
/// As with [`FullFrame`], exactly one of `ie_bytes` / `payload` is non-empty
/// depending on `frame_type` ([`carries_payload`]).
///
/// `ie_bytes` is sealed (`pub(crate)`) — every well-formed `OwnedFullFrame`
/// has IE bytes that round-trip through [`Ies::parse`]. The invariant is
/// preserved by construction: outside the crate the only way to populate
/// `ie_bytes` is via [`OwnedFullFrame::with_ies`] or [`Frame::into_owned`],
/// both of which feed an already-typed [`Ies`] through [`Ies::encode`].
/// This closes the iax-2cea misuse path where an arbitrary `Vec<u8>` could
/// be stuffed into the field and crash the reliability layer on enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFullFrame {
    pub source_call: u16,
    pub dest_call: u16,
    pub retransmission: bool,
    pub timestamp: u32,
    pub oseqno: u8,
    pub iseqno: u8,
    pub frame_type: FrameType,
    pub subclass: Subclass,
    /// Re-encoded IE byte stream; cheaper to round-trip than to project
    /// every `Option<&str>` field into an `Option<String>` clone. Re-parse
    /// with `Ies::parse(&self.ie_bytes())` when needed. Empty for media
    /// frames. Sealed to enforce parse round-trip — see struct docs.
    pub(crate) ie_bytes: Vec<u8>,
    /// Raw codec samples for Voice/Video/Image frames. Empty for IE-bearing
    /// frame types.
    pub payload: Vec<u8>,
}

impl OwnedFullFrame {
    /// Build an [`OwnedFullFrame`] from typed [`Ies`], encoding the IE
    /// stream internally. The only public constructor that produces a
    /// frame with a non-empty `ie_bytes` — guarantees the parse-on-reuse
    /// invariant the reliability layer relies on (iax-2cea).
    ///
    /// `payload` is left empty; use this constructor for IE-bearing frame
    /// types ([`carries_payload`] returns `false`). Media-frame callers
    /// build the struct via the in-crate path through [`Frame::into_owned`].
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::IeTooLong`] if any IE payload exceeds the
    /// 255-byte wire limit (RFC 5456 §8). See iax-545b.
    ///
    /// # Sealed-field compile-time check (iax-2cea)
    ///
    /// `ie_bytes` is `pub(crate)`; out-of-crate callers cannot stuff
    /// arbitrary bytes into it. The following doctest documents the
    /// seal — if it ever starts compiling, the field has been made
    /// public again and the reliability-layer invariants no longer hold.
    ///
    /// ```compile_fail
    /// use astar_iax_core::frame::{OwnedFullFrame, Subclass};
    /// use astar_iax_core::subclass::{FrameType, IaxCommand};
    /// let _ = OwnedFullFrame {
    ///     source_call: 0,
    ///     dest_call: 0,
    ///     retransmission: false,
    ///     timestamp: 0,
    ///     oseqno: 0,
    ///     iseqno: 0,
    ///     frame_type: FrameType::Iax,
    ///     subclass: Subclass::Iax(IaxCommand::New),
    ///     ie_bytes: vec![0xff, 0xff, 0xff], // <-- private field
    ///     payload: Vec::new(),
    /// };
    /// ```
    #[allow(clippy::too_many_arguments)] // matches the wire-level frame fields 1:1
    pub fn with_ies(
        source_call: u16,
        dest_call: u16,
        retransmission: bool,
        timestamp: u32,
        oseqno: u8,
        iseqno: u8,
        frame_type: FrameType,
        subclass: Subclass,
        ies: &Ies<'_>,
    ) -> Result<Self, EncodeError> {
        let mut ie_bytes = Vec::new();
        ies.encode(&mut ie_bytes)?;
        Ok(Self {
            source_call,
            dest_call,
            retransmission,
            timestamp,
            oseqno,
            iseqno,
            frame_type,
            subclass,
            ie_bytes,
            payload: Vec::new(),
        })
    }

    /// Borrow the encoded IE byte stream. Always parseable by
    /// [`Ies::parse`] — see the struct-level invariant.
    #[must_use]
    pub fn ie_bytes(&self) -> &[u8] {
        &self.ie_bytes
    }
}

/// Owned mini frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedMiniFrame {
    pub source_call: u16,
    pub timestamp: u16,
    pub payload: Vec<u8>,
}

impl Frame<'_> {
    /// Convert a borrowed frame into an owned one by copying.
    ///
    /// # Panics
    ///
    /// Panics if any IE payload exceeds 255 bytes. In practice the only
    /// way to reach this is to hand-construct a [`FullFrame`] with an
    /// oversized string IE — any [`Frame`] returned by [`parse`] is
    /// bounded by the wire's 1-byte length prefix and therefore round-
    /// trips cleanly. Callers that need to surface the error rather
    /// than panic should encode IEs directly via [`Ies::encode`].
    #[must_use]
    pub fn into_owned(self) -> OwnedFrame {
        match self {
            Self::Full(f) => {
                // Media frames carry payload (and no IEs); IE-bearing
                // frames carry IEs (and no payload). See `carries_payload`.
                let mut ie_bytes = Vec::new();
                if !carries_payload(f.frame_type) {
                    f.ies
                        .encode(&mut ie_bytes)
                        .expect("Frame IEs came from parse() — payloads bounded by u8 len prefix");
                }
                OwnedFrame::Full(OwnedFullFrame {
                    source_call: f.source_call,
                    dest_call: f.dest_call,
                    retransmission: f.retransmission,
                    timestamp: f.timestamp,
                    oseqno: f.oseqno,
                    iseqno: f.iseqno,
                    frame_type: f.frame_type,
                    subclass: f.subclass,
                    ie_bytes,
                    payload: f.payload.to_vec(),
                })
            }
            Self::Mini(m) => OwnedFrame::Mini(OwnedMiniFrame {
                source_call: m.source_call,
                timestamp: m.timestamp,
                payload: m.payload.to_vec(),
            }),
        }
    }
}

impl OwnedFrame {
    /// Borrow back as a [`Frame`] for re-encoding. The returned `Frame`
    /// borrows from `self`.
    ///
    /// Errors only if the stored IE bytes are malformed, which can only
    /// happen if a caller mutates `ie_bytes` directly — round-tripping
    /// through `into_owned` preserves well-formedness.
    pub fn as_frame(&self) -> Result<Frame<'_>, ParseError> {
        match self {
            Self::Full(f) => Ok(Frame::Full(Box::new(FullFrame {
                source_call: f.source_call,
                dest_call: f.dest_call,
                retransmission: f.retransmission,
                timestamp: f.timestamp,
                oseqno: f.oseqno,
                iseqno: f.iseqno,
                frame_type: f.frame_type,
                subclass: f.subclass,
                ies: if carries_payload(f.frame_type) {
                    Ies::empty()
                } else {
                    Ies::parse(&f.ie_bytes)?
                },
                payload: &f.payload,
            }))),
            Self::Mini(m) => Ok(Frame::Mini(MiniFrame {
                source_call: m.source_call,
                timestamp: m.timestamp,
                payload: &m.payload,
            })),
        }
    }
}

// --- Parse ---------------------------------------------------------------

/// Parse a single IAX2 packet (full or mini) from `input`.
///
/// Per RFC 5456 §6 the high bit of the first byte distinguishes the two
/// header forms.
pub fn parse(input: &[u8]) -> Result<Frame<'_>, ParseError> {
    parse_with(input, IeMode::Strict)
}

/// Lenient variant: tolerate unknown frame subclasses and unknown/malformed
/// IEs by skipping them. Use for runtime consumption of peer traffic; use
/// [`parse`] for fixture round-trips where strictness is desired.
pub fn parse_lenient(input: &[u8]) -> Result<Frame<'_>, ParseError> {
    parse_with(input, IeMode::Lenient)
}

#[derive(Clone, Copy)]
enum IeMode {
    Strict,
    Lenient,
}

fn parse_with(input: &[u8], mode: IeMode) -> Result<Frame<'_>, ParseError> {
    if input.is_empty() {
        return Err(ParseError::Truncated {
            needed: 1,
            available: 0,
        });
    }
    if input[0] & 0x80 == 0 {
        parse_mini(input).map(Frame::Mini)
    } else {
        parse_full_with(input, mode).map(|f| Frame::Full(Box::new(f)))
    }
}

fn parse_mini(input: &[u8]) -> Result<MiniFrame<'_>, ParseError> {
    if input.len() < MINI_HEADER_LEN {
        return Err(ParseError::Truncated {
            needed: MINI_HEADER_LEN,
            available: input.len(),
        });
    }
    let source_call = u16::from_be_bytes([input[0], input[1]]) & CALLNO_MASK;
    let timestamp = u16::from_be_bytes([input[2], input[3]]);
    Ok(MiniFrame {
        source_call,
        timestamp,
        payload: &input[MINI_HEADER_LEN..],
    })
}

fn parse_full_with(input: &[u8], mode: IeMode) -> Result<FullFrame<'_>, ParseError> {
    if input.len() < FULL_HEADER_LEN {
        return Err(ParseError::Truncated {
            needed: FULL_HEADER_LEN,
            available: input.len(),
        });
    }
    let scallno_raw = u16::from_be_bytes([input[0], input[1]]);
    let source_call = scallno_raw & CALLNO_MASK;
    debug_assert_eq!(
        scallno_raw & FLAG_FULL,
        FLAG_FULL,
        "full-frame discriminator checked by caller"
    );
    let dcallno_raw = u16::from_be_bytes([input[2], input[3]]);
    let dest_call = dcallno_raw & CALLNO_MASK;
    let retransmission = dcallno_raw & FLAG_FULL != 0;
    let timestamp = u32::from_be_bytes([input[4], input[5], input[6], input[7]]);
    let oseqno = input[8];
    let iseqno = input[9];
    let frame_type = FrameType::try_from(input[10])?;
    let logical_subclass = parse_subclass_byte(input[11])?;
    let subclass = match decode_typed_subclass(frame_type, logical_subclass) {
        Ok(s) => s,
        Err(e) => match mode {
            IeMode::Strict => return Err(e),
            // Preserve the wire value as Raw so FSM/runtime can introspect
            // without rejecting the whole frame. The FSM's match arms only
            // care about specific known subclasses; unknown ones fall through
            // to the catch-all (LogInvalid) which is the right behavior.
            IeMode::Lenient => Subclass::Raw(logical_subclass),
        },
    };
    // RFC 5456 §6.4: the bytes after the header are codec samples for
    // Voice/Video/Image frames and an IE TLV stream for everything else.
    // iax-bf0b: previously we fed those samples to `Ies::parse`, which
    // either failed or — in lenient mode — silently produced an empty IE
    // set, dropping the audio. Split the dispatch on `frame_type`.
    let (ies, payload) = if carries_payload(frame_type) {
        (Ies::empty(), &input[FULL_HEADER_LEN..])
    } else {
        let ies = match mode {
            IeMode::Strict => Ies::parse(&input[FULL_HEADER_LEN..])?,
            IeMode::Lenient => Ies::parse_lenient(&input[FULL_HEADER_LEN..])?,
        };
        (ies, &input[input.len()..])
    };
    Ok(FullFrame {
        source_call,
        dest_call,
        retransmission,
        timestamp,
        oseqno,
        iseqno,
        frame_type,
        subclass,
        ies,
        payload,
    })
}

fn decode_typed_subclass(frame_type: FrameType, value: u32) -> Result<Subclass, ParseError> {
    Ok(match frame_type {
        FrameType::Iax => {
            // IAX commands fit in a u8 by construction; reject values that
            // don't (which would only happen if a peer set SC_LOG with a
            // shift >= 8, which Asterisk never does for AST_FRAME_IAX).
            let byte = u8::try_from(value).map_err(|_| ParseError::UnknownSubclass {
                frame_type,
                subclass: value,
            })?;
            Subclass::Iax(IaxCommand::try_from(byte)?)
        }
        FrameType::Control => {
            let byte = u8::try_from(value).map_err(|_| ParseError::UnknownSubclass {
                frame_type,
                subclass: value,
            })?;
            Subclass::Control(ControlSubclass::try_from(byte)?)
        }
        FrameType::Voice => Subclass::Voice(VoiceFormat::try_from(value)?),
        // RFC 5456 §6.7: DTMF subclass is the ASCII digit. Validate it here
        // so the FSM gets a typed `char`, not a raw byte; an out-of-range
        // value is a typed parse error (no silent `Raw` fallthrough).
        FrameType::DtmfBegin | FrameType::DtmfEnd => {
            Subclass::Dtmf(crate::subclass::dtmf_digit_from_u32(value)?)
        }
        _ => Subclass::Raw(value),
    })
}

// --- Encode --------------------------------------------------------------

/// Encode a frame into `out`. No allocation: the caller owns the buffer.
///
/// Returns [`EncodeError::IeTooLong`] (via [`Ies::encode`]) when an IE
/// payload exceeds the 255-byte wire limit (RFC 5456 §8). See iax-545b
/// for why silent truncation is no longer the default.
pub fn encode(frame: &Frame<'_>, out: &mut impl BufMut) -> Result<(), crate::error::EncodeError> {
    match frame {
        Frame::Full(f) => encode_full(f, out),
        Frame::Mini(m) => {
            encode_mini(m, out);
            Ok(())
        }
    }
}

fn encode_full(f: &FullFrame<'_>, out: &mut impl BufMut) -> Result<(), crate::error::EncodeError> {
    let scallno = (f.source_call & CALLNO_MASK) | FLAG_FULL;
    let dcallno = (f.dest_call & CALLNO_MASK) | if f.retransmission { FLAG_FULL } else { 0 };
    out.put_u16(scallno);
    out.put_u16(dcallno);
    out.put_u32(f.timestamp);
    out.put_u8(f.oseqno);
    out.put_u8(f.iseqno);
    out.put_u8(f.frame_type as u8);
    out.put_u8(encode_subclass_byte(logical_subclass_value(f.subclass)));
    // RFC 5456 §6.4: media frames carry codec samples after the header
    // (no IEs); IE-bearing frames carry IEs (no payload). The invariant
    // is that exactly one of `ies` / `payload` is non-empty per
    // [`carries_payload`]; for robustness we emit whichever side the
    // caller populated and ignore the other when it doesn't match the
    // frame type.
    if carries_payload(f.frame_type) {
        out.put_slice(f.payload);
        Ok(())
    } else {
        f.ies.encode(out)
    }
}

fn encode_mini(m: &MiniFrame<'_>, out: &mut impl BufMut) {
    // The mini header high bit (FLAG_FULL on scallno) is always 0.
    out.put_u16(m.source_call & CALLNO_MASK);
    out.put_u16(m.timestamp);
    out.put_slice(m.payload);
}

/// Serialize an [`OwnedFullFrame`] to the wire WITHOUT re-parsing its IEs.
///
/// `OwnedFullFrame` already stores its IE stream pre-encoded in `ie_bytes`
/// (sealed, produced by [`Ies::encode`] — iax-2cea) and its codec samples in
/// `payload`, so we assemble the 12-byte header and append the body directly.
/// This is the M6 hot-path serializer (iax-f755): the reliability layer calls
/// it once per `enqueue` and avoids the `Ies::parse` → `Ies::encode` round-trip
/// (and its `.expect()`/`unreachable!()` panic surface) on every send. The
/// `retransmission` bit is left clear; callers patch it on the returned bytes.
#[must_use]
pub(crate) fn encode_owned_full(f: &OwnedFullFrame) -> Vec<u8> {
    let body: &[u8] = if carries_payload(f.frame_type) {
        &f.payload
    } else {
        &f.ie_bytes
    };
    let mut out = Vec::with_capacity(FULL_HEADER_LEN + body.len());
    let scallno = (f.source_call & CALLNO_MASK) | FLAG_FULL;
    let dcallno = f.dest_call & CALLNO_MASK; // retransmission bit left clear
    out.put_u16(scallno);
    out.put_u16(dcallno);
    out.put_u32(f.timestamp);
    out.put_u8(f.oseqno);
    out.put_u8(f.iseqno);
    out.put_u8(f.frame_type as u8);
    out.put_u8(encode_subclass_byte(logical_subclass_value(f.subclass)));
    out.put_slice(body);
    out
}

fn logical_subclass_value(s: Subclass) -> u32 {
    match s {
        Subclass::Iax(c) => u32::from(c as u8),
        Subclass::Control(c) => u32::from(c as u8),
        Subclass::Voice(v) => v.as_u32(),
        Subclass::Dtmf(c) => u32::from(c as u8),
        Subclass::Raw(v) => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ie::IE_CALLTOKEN;

    #[test]
    fn mini_frame_round_trip() {
        let payload = [0xffu8; 160];
        let m = MiniFrame {
            source_call: 0x1234,
            timestamp: 0x5678,
            payload: &payload,
        };
        let frame = Frame::Mini(m.clone());
        let mut buf = Vec::new();
        encode(&frame, &mut buf).expect("test frame must encode");
        assert_eq!(buf[0] & 0x80, 0, "mini-frame discriminator");
        let parsed = parse(&buf).unwrap();
        match parsed {
            Frame::Mini(p) => {
                assert_eq!(p.source_call, m.source_call);
                assert_eq!(p.timestamp, m.timestamp);
                assert_eq!(p.payload, &payload[..]);
            }
            Frame::Full(_) => panic!("expected mini frame"),
        }
    }

    #[test]
    fn full_frame_round_trip_with_calltoken() {
        let f = FullFrame {
            source_call: 1,
            dest_call: 0,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 0,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::New),
            ies: Ies {
                username: Some("rob"),
                calltoken: Some(&[]),
                ..Ies::empty()
            },
            payload: &[],
        };
        let frame = Frame::Full(Box::new(f.clone()));
        let mut buf = Vec::new();
        encode(&frame, &mut buf).expect("test frame must encode");
        // First byte: high bit set (full frame), call number = 1.
        assert_eq!(buf[0] & 0x80, 0x80);
        // CALLTOKEN IE present with zero-length payload.
        let pos = buf.iter().position(|&b| b == IE_CALLTOKEN).unwrap();
        assert_eq!(buf[pos + 1], 0);
        let parsed = parse(&buf).unwrap();
        match parsed {
            Frame::Full(p) => {
                assert_eq!(p.source_call, 1);
                assert_eq!(p.frame_type, FrameType::Iax);
                assert_eq!(p.subclass, Subclass::Iax(IaxCommand::New));
                assert_eq!(p.ies.username, Some("rob"));
                assert_eq!(p.ies.calltoken, Some(&[][..]));
            }
            Frame::Mini(_) => panic!("expected full frame"),
        }
    }

    // iax-a9b8: DTMF subclass decodes to a typed `char` and round-trips
    // bit-identically for every valid RFC 5456 §6.7 digit.
    #[test]
    fn dtmf_subclass_round_trips_for_valid_digits() {
        for digit in "0123456789*#ABCD".chars() {
            for kind in [FrameType::DtmfBegin, FrameType::DtmfEnd] {
                let f = FullFrame {
                    source_call: 1,
                    dest_call: 2,
                    retransmission: false,
                    timestamp: 0,
                    oseqno: 0,
                    iseqno: 0,
                    frame_type: kind,
                    subclass: Subclass::Dtmf(digit),
                    ies: Ies::empty(),
                    payload: &[],
                };
                let mut buf = Vec::new();
                encode(&Frame::Full(Box::new(f.clone())), &mut buf)
                    .expect("DTMF frame has empty IE set");
                let Frame::Full(p) = parse(&buf).unwrap() else {
                    panic!("expected full frame")
                };
                assert_eq!(p.frame_type, kind);
                assert_eq!(
                    p.subclass,
                    Subclass::Dtmf(digit),
                    "digit {digit} round-trip"
                );
                // parse -> encode -> parse is byte-identical.
                let mut buf2 = Vec::new();
                encode(&Frame::Full(p), &mut buf2).unwrap();
                assert_eq!(buf, buf2, "digit {digit} bit-identical re-encode");
            }
        }
    }

    // iax-a9b8: a non-DTMF subclass byte is a typed parse error in strict
    // mode (no silent `Raw` fallthrough).
    #[test]
    fn dtmf_invalid_digit_is_parse_error() {
        // Build a DTMF_END frame carrying ASCII 'Z' (0x5a, not a DTMF digit).
        let f = FullFrame {
            source_call: 1,
            dest_call: 2,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 0,
            frame_type: FrameType::DtmfEnd,
            subclass: Subclass::Raw(u32::from(b'Z')),
            ies: Ies::empty(),
            payload: &[],
        };
        let mut buf = Vec::new();
        encode(&Frame::Full(Box::new(f)), &mut buf).unwrap();
        let err = parse(&buf).unwrap_err();
        assert_eq!(
            err,
            crate::error::ParseError::InvalidDtmfDigit(u32::from(b'Z'))
        );
    }

    #[test]
    fn full_frame_retransmission_bit_round_trips() {
        let f = FullFrame {
            source_call: 5,
            dest_call: 7,
            retransmission: true,
            timestamp: 1000,
            oseqno: 4,
            iseqno: 3,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::Ack),
            ies: Ies::empty(),
            payload: &[],
        };
        let frame = Frame::Full(Box::new(f.clone()));
        let mut buf = Vec::new();
        encode(&frame, &mut buf).expect("test frame must encode");
        // dcallno is bytes [2..4]; high bit of byte 2 must be set.
        assert_eq!(buf[2] & 0x80, 0x80);
        let parsed = parse(&buf).unwrap();
        let Frame::Full(p) = parsed else {
            panic!("expected full frame");
        };
        assert!(p.retransmission);
        assert_eq!(p.dest_call, 7);
    }

    #[test]
    fn into_owned_round_trips_through_re_encode() {
        let f = FullFrame {
            source_call: 1,
            dest_call: 2,
            retransmission: false,
            timestamp: 42,
            oseqno: 0,
            iseqno: 1,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::New),
            ies: Ies {
                username: Some("rob"),
                ..Ies::empty()
            },
            payload: &[],
        };
        let borrowed = Frame::Full(Box::new(f));
        let mut wire1 = Vec::new();
        encode(&borrowed, &mut wire1).expect("test frame must encode");
        let owned = borrowed.into_owned();
        let reborrow = owned.as_frame().unwrap();
        let mut wire2 = Vec::new();
        encode(&reborrow, &mut wire2).expect("test frame must encode");
        assert_eq!(wire1, wire2);
    }

    /// iax-2cea: `with_ies` is the only public route to a populated
    /// `ie_bytes` outside the crate, and its output must round-trip
    /// through `Ies::parse` (this is the invariant the reliability
    /// layer's `unreachable!` clauses depend on).
    #[test]
    fn with_ies_round_trips_through_parse() {
        let ies = Ies {
            username: Some("rob"),
            calltoken: Some(b"server-token"),
            ..Ies::empty()
        };
        let owned = OwnedFullFrame::with_ies(
            1,
            0,
            false,
            0,
            0,
            0,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::New),
            &ies,
        )
        .expect("typed IEs within wire bounds");
        let parsed = Ies::parse(owned.ie_bytes()).expect("with_ies output is always parseable");
        assert_eq!(parsed.username, Some("rob"));
        assert_eq!(parsed.calltoken, Some(&b"server-token"[..]));
    }

    /// iax-2cea / iax-545b: the `EncodeError` from `Ies::encode` must
    /// propagate out of `with_ies` rather than panicking, so callers that
    /// can produce oversize IE payloads (e.g. arbitrary user input) get a
    /// `Result` to react to.
    #[test]
    fn with_ies_returns_err_when_ie_payload_exceeds_255_bytes() {
        // CALLTOKEN holds an arbitrary byte slice — easy to overflow.
        let oversized = vec![0u8; 256];
        let ies = Ies {
            calltoken: Some(&oversized),
            ..Ies::empty()
        };
        let result = OwnedFullFrame::with_ies(
            1,
            0,
            false,
            0,
            0,
            0,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::New),
            &ies,
        );
        assert!(
            matches!(result, Err(crate::error::EncodeError::IeTooLong { .. })),
            "256-byte IE payload must surface IeTooLong, got {result:?}",
        );
    }
}
