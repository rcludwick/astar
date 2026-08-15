// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Typed subclass enums per IAX2 frame type.
//!
//! IAX2 reuses the single "subclass" byte in the full-frame header for
//! frame-type-specific dispatch. The mapping of byte to meaning depends on
//! the surrounding `FrameType`. We expose one enum per frame type that
//! cares about it, hiding the wire-level compression scheme described in
//! RFC 5456 §6.2.
//!
//! Constants here are cross-checked against Asterisk master
//! (`channels/iax2/include/iax2.h` and `include/asterisk/frame.h`) and the
//! jaracil Go reference (`frame.go`). Where vendored astar diverges from
//! upstream Asterisk we follow upstream and note the divergence inline; the
//! conformance doc (`docs/spec/iax2-conformance.md` §2026-05-29) requires
//! that resolution.

use crate::error::ParseError;

/// Top-level frame type byte from the full-frame header. Values match
/// `enum ast_frame_type` in Asterisk's `include/asterisk/frame.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FrameType {
    /// `AST_FRAME_DTMF_END` (= `AST_FRAME_DTMF`). Subclass = ASCII digit.
    DtmfEnd = 1,
    /// `AST_FRAME_VOICE`. Subclass = codec bitmask (`AST_FORMAT_*`).
    Voice = 2,
    /// `AST_FRAME_VIDEO`. Subclass = video codec bitmask.
    Video = 3,
    /// `AST_FRAME_CONTROL`. Subclass = `AST_CONTROL_*`.
    Control = 4,
    /// `AST_FRAME_NULL`. Subclass unused.
    Null = 5,
    /// `AST_FRAME_IAX`. Subclass = `IAX_COMMAND_*`.
    Iax = 6,
    /// `AST_FRAME_TEXT`. Subclass = text format.
    Text = 7,
    /// `AST_FRAME_IMAGE`. Subclass = image format.
    Image = 8,
    /// `AST_FRAME_HTML`. Subclass = HTML opcode.
    Html = 9,
    /// `AST_FRAME_CNG`. Comfort-noise generation.
    Cng = 10,
    /// `AST_FRAME_MODEM`. Subclass = modem protocol.
    Modem = 11,
    /// `AST_FRAME_DTMF_BEGIN`. Subclass = ASCII digit.
    DtmfBegin = 12,
}

/// `true` if `c` is a valid DTMF subclass digit per RFC 5456 §6.7
/// (`0-9`, `*`, `#`, `A-D`).
#[must_use]
pub const fn is_dtmf_digit(c: char) -> bool {
    matches!(c, '0'..='9' | '*' | '#' | 'A'..='D')
}

/// Decode a DTMF subclass value (the raw ASCII byte, widened to `u32` on the
/// wire) into a validated digit `char`. `Err` ⇒ the byte is not one of the
/// RFC 5456 §6.7 digits and must not be silently kept as `Raw`.
pub fn dtmf_digit_from_u32(value: u32) -> Result<char, ParseError> {
    let byte = u8::try_from(value).map_err(|_| ParseError::InvalidDtmfDigit(value))?;
    let c = char::from(byte);
    if is_dtmf_digit(c) {
        Ok(c)
    } else {
        Err(ParseError::InvalidDtmfDigit(value))
    }
}

impl TryFrom<u8> for FrameType {
    type Error = ParseError;

    fn try_from(b: u8) -> Result<Self, ParseError> {
        Ok(match b {
            1 => Self::DtmfEnd,
            2 => Self::Voice,
            3 => Self::Video,
            4 => Self::Control,
            5 => Self::Null,
            6 => Self::Iax,
            7 => Self::Text,
            8 => Self::Image,
            9 => Self::Html,
            10 => Self::Cng,
            11 => Self::Modem,
            12 => Self::DtmfBegin,
            other => return Err(ParseError::UnknownFrameType(other)),
        })
    }
}

/// IAX control commands (subclass of an `AST_FRAME_IAX`). Values from
/// Asterisk `channels/iax2/include/iax2.h` `IAX_COMMAND_*`. Vendored astar
/// `iax2.h` stops at `FWDATA = 37`; upstream Asterisk additionally defines
/// `TXMEDIA = 38`, `RTKEY = 39`, and `CALLTOKEN = 40`. The dialect doc
/// confirms CALLTOKEN = 40 for ASL3 (RFC 5456 §8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum IaxCommand {
    New = 1,
    Ping = 2,
    Pong = 3,
    Ack = 4,
    Hangup = 5,
    Reject = 6,
    Accept = 7,
    AuthReq = 8,
    AuthRep = 9,
    Inval = 10,
    LagRq = 11,
    LagRp = 12,
    RegReq = 13,
    RegAuth = 14,
    RegAck = 15,
    RegRej = 16,
    RegRel = 17,
    Vnak = 18,
    DpReq = 19,
    DpRep = 20,
    Dial = 21,
    TxReq = 22,
    TxCnt = 23,
    TxAcc = 24,
    TxReady = 25,
    TxRel = 26,
    TxRej = 27,
    Quelch = 28,
    Unquelch = 29,
    Poke = 30,
    Page = 31,
    Mwi = 32,
    Unsupport = 33,
    Transfer = 34,
    Provision = 35,
    FwDownl = 36,
    FwData = 37,
    /// `IAX_COMMAND_TXMEDIA = 38` — present in upstream Asterisk but absent
    /// from vendored astar headers (see conformance doc 2026-05-29).
    TxMedia = 38,
    /// `IAX_COMMAND_RTKEY = 39` — upstream-only, rekey for encrypted media.
    RtKey = 39,
    /// `IAX_COMMAND_CALLTOKEN = 40` — RFC 5456 §8.6 anti-spoof handshake.
    CallToken = 40,
}

impl TryFrom<u8> for IaxCommand {
    type Error = ParseError;

    fn try_from(b: u8) -> Result<Self, ParseError> {
        Ok(match b {
            1 => Self::New,
            2 => Self::Ping,
            3 => Self::Pong,
            4 => Self::Ack,
            5 => Self::Hangup,
            6 => Self::Reject,
            7 => Self::Accept,
            8 => Self::AuthReq,
            9 => Self::AuthRep,
            10 => Self::Inval,
            11 => Self::LagRq,
            12 => Self::LagRp,
            13 => Self::RegReq,
            14 => Self::RegAuth,
            15 => Self::RegAck,
            16 => Self::RegRej,
            17 => Self::RegRel,
            18 => Self::Vnak,
            19 => Self::DpReq,
            20 => Self::DpRep,
            21 => Self::Dial,
            22 => Self::TxReq,
            23 => Self::TxCnt,
            24 => Self::TxAcc,
            25 => Self::TxReady,
            26 => Self::TxRel,
            27 => Self::TxRej,
            28 => Self::Quelch,
            29 => Self::Unquelch,
            30 => Self::Poke,
            31 => Self::Page,
            32 => Self::Mwi,
            33 => Self::Unsupport,
            34 => Self::Transfer,
            35 => Self::Provision,
            36 => Self::FwDownl,
            37 => Self::FwData,
            38 => Self::TxMedia,
            39 => Self::RtKey,
            40 => Self::CallToken,
            other => {
                return Err(ParseError::UnknownSubclass {
                    frame_type: FrameType::Iax,
                    subclass: u32::from(other),
                });
            }
        })
    }
}

/// `AST_CONTROL_*` subclass of an `AST_FRAME_CONTROL` frame. Values from
/// Asterisk `include/asterisk/frame.h`. `RADIO_KEY` / `RADIO_UNKEY` (12/13)
/// are the `AllStarLink` PTT signal — see `docs/spec/allstar-dialect.md`.
///
/// We model only the 0..255 range here; Asterisk reserves 1000+ for stream
/// control opcodes that are never sent on the IAX2 wire (they're channel-
/// internal). If we ever need them we'd widen this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ControlSubclass {
    Hangup = 1,
    Ring = 2,
    Ringing = 3,
    Answer = 4,
    Busy = 5,
    TakeOffHook = 6,
    OffHook = 7,
    Congestion = 8,
    Flash = 9,
    Wink = 10,
    Option = 11,
    /// `AllStarLink` PTT-down signal (`AST_CONTROL_RADIO_KEY = 12`).
    RadioKey = 12,
    /// `AllStarLink` PTT-up signal (`AST_CONTROL_RADIO_UNKEY = 13`).
    RadioUnkey = 13,
    Progress = 14,
    Proceeding = 15,
    Hold = 16,
    Unhold = 17,
    VidUpdate = 18,
    SrcUpdate = 20,
    Transfer = 21,
    ConnectedLine = 22,
    Redirecting = 23,
    T38Parameters = 24,
    Cc = 25,
    SrcChange = 26,
    ReadAction = 27,
    Aoc = 28,
    EndOfQ = 29,
    Incomplete = 30,
    Mcid = 31,
    UpdateRtpPeer = 32,
    PvtCauseCode = 33,
    MasqueradeNotify = 34,
    StreamTopologyRequestChange = 35,
    StreamTopologyChanged = 36,
    StreamTopologySourceChanged = 37,
}

impl TryFrom<u8> for ControlSubclass {
    type Error = ParseError;

    fn try_from(b: u8) -> Result<Self, ParseError> {
        Ok(match b {
            1 => Self::Hangup,
            2 => Self::Ring,
            3 => Self::Ringing,
            4 => Self::Answer,
            5 => Self::Busy,
            6 => Self::TakeOffHook,
            7 => Self::OffHook,
            8 => Self::Congestion,
            9 => Self::Flash,
            10 => Self::Wink,
            11 => Self::Option,
            12 => Self::RadioKey,
            13 => Self::RadioUnkey,
            14 => Self::Progress,
            15 => Self::Proceeding,
            16 => Self::Hold,
            17 => Self::Unhold,
            18 => Self::VidUpdate,
            20 => Self::SrcUpdate,
            21 => Self::Transfer,
            22 => Self::ConnectedLine,
            23 => Self::Redirecting,
            24 => Self::T38Parameters,
            25 => Self::Cc,
            26 => Self::SrcChange,
            27 => Self::ReadAction,
            28 => Self::Aoc,
            29 => Self::EndOfQ,
            30 => Self::Incomplete,
            31 => Self::Mcid,
            32 => Self::UpdateRtpPeer,
            33 => Self::PvtCauseCode,
            34 => Self::MasqueradeNotify,
            35 => Self::StreamTopologyRequestChange,
            36 => Self::StreamTopologyChanged,
            37 => Self::StreamTopologySourceChanged,
            other => {
                return Err(ParseError::UnknownSubclass {
                    frame_type: FrameType::Control,
                    subclass: u32::from(other),
                });
            }
        })
    }
}

/// `AST_FORMAT_*` codec bitmask used as the subclass of `AST_FRAME_VOICE`.
/// These are powers of two so they all serialize via the compressed
/// subclass encoding (RFC 5456 §6.2). Values from Asterisk
/// `include/asterisk/format.h`; mnemonic match the jaracil Go reference
/// codec table.
///
/// The subclass space here is `u32`-wide because Asterisk codec bitmasks
/// extend to `1 << 30` (e.g. SLIN16). The mini-frame and voice-frame
/// dispatcher in core only cares about the logical value, not the
/// compressed byte form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(u32)]
pub enum VoiceFormat {
    G723_1 = 1 << 0,
    Gsm = 1 << 1,
    G711U = 1 << 2,
    G711A = 1 << 3,
    G726 = 1 << 4,
    Adpcm = 1 << 5,
    Slin = 1 << 6,
    Lpc10 = 1 << 7,
    G729 = 1 << 8,
    Speex = 1 << 9,
    Ilbc = 1 << 10,
    G726Aal2 = 1 << 11,
    G722 = 1 << 12,
    Siren7 = 1 << 13,
    Siren14 = 1 << 14,
    Slin16 = 1 << 15,
    // Higher codecs (G719, SPEEX16, OPUS, ...) are negotiated via the
    // CAPABILITY2 / FORMAT2 IEs (id 55/56) rather than the subclass byte:
    // the 32-bit format word ran out of bits for new codecs around 2014.
    // See `docs/spec/allstar-dialect.md`.
}

impl TryFrom<u32> for VoiceFormat {
    type Error = ParseError;

    fn try_from(v: u32) -> Result<Self, ParseError> {
        Ok(match v {
            x if x == 1 << 0 => Self::G723_1,
            x if x == 1 << 1 => Self::Gsm,
            x if x == 1 << 2 => Self::G711U,
            x if x == 1 << 3 => Self::G711A,
            x if x == 1 << 4 => Self::G726,
            x if x == 1 << 5 => Self::Adpcm,
            x if x == 1 << 6 => Self::Slin,
            x if x == 1 << 7 => Self::Lpc10,
            x if x == 1 << 8 => Self::G729,
            x if x == 1 << 9 => Self::Speex,
            x if x == 1 << 10 => Self::Ilbc,
            x if x == 1 << 11 => Self::G726Aal2,
            x if x == 1 << 12 => Self::G722,
            x if x == 1 << 13 => Self::Siren7,
            x if x == 1 << 14 => Self::Siren14,
            x if x == 1 << 15 => Self::Slin16,
            other => {
                return Err(ParseError::UnknownSubclass {
                    frame_type: FrameType::Voice,
                    subclass: other,
                });
            }
        })
    }
}

impl VoiceFormat {
    /// Logical codec bitmask value (the value as it appears in a `CAPABILITY`
    /// or `FORMAT` IE, before any subclass-byte compression).
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Inverse of [`as_u32`](Self::as_u32): map a single-codec bitmask value
    /// (as carried in a `FORMAT` IE) back to a typed `VoiceFormat`, or `None`
    /// if the value isn't a recognised single codec. Used by the inbound NEW
    /// offer parser to read the peer's preferred codec (iax-8baf).
    #[must_use]
    pub fn from_u32(v: u32) -> Option<Self> {
        Self::try_from(v).ok()
    }

    /// Wire payload size of one 20 ms voice frame, for formats this library can
    /// carry on its media path. `None` for formats we can parse off the wire but
    /// never negotiate (iax-31f7).
    #[must_use]
    pub fn frame_bytes_20ms(self) -> Option<usize> {
        match self {
            Self::G711U | Self::G711A => Some(160),
            Self::Slin => Some(320),
            Self::Slin16 => Some(640),
            _ => None,
        }
    }

    /// Audio sample rate of a format this library can carry on its media path.
    /// `None` for formats we can parse but never negotiate (iax-4348).
    #[must_use]
    pub fn sample_rate(self) -> Option<u32> {
        match self {
            Self::G711U | Self::G711A | Self::Slin => Some(8000),
            Self::Slin16 => Some(16000),
            _ => None,
        }
    }
}

// --- Subclass byte compression (RFC 5456 §6.2) ----------------------------

/// `IAX_FLAG_SC_LOG` from `iax2.h`. When set in the wire subclass byte the
/// low 5 bits hold the bit-shift count for `1 << n`.
pub(crate) const SC_LOG: u8 = 0x80;

/// `IAX_MAX_SHIFT` from `iax2.h`: cap on the shift width that fits into the
/// 5 low bits of the compressed subclass byte.
pub(crate) const MAX_SHIFT: u8 = 0x1f;

/// Decode a wire subclass byte into its logical `u32` value.
///
/// Mirrors `uncompress_subclass()` in vendored
/// `lib/libiax2/src/iax.c:2559-2566`. If the `SC_LOG` bit is set the low 5
/// bits are a shift width; otherwise the byte is the value directly.
///
/// Errors only on shift widths beyond `MAX_SHIFT`; with the existing 0x1f
/// mask that branch is unreachable, but the explicit check guards against
/// future widening of the bit field.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn parse_subclass_byte(b: u8) -> Result<u32, ParseError> {
    if b & SC_LOG == 0 {
        return Ok(u32::from(b));
    }
    let shift = b & MAX_SHIFT;
    // shift is in 0..=31 by construction of MAX_SHIFT, so `1u32 << shift` is
    // always defined. We use checked_shl to make the intent explicit.
    1u32.checked_shl(u32::from(shift))
        .ok_or(ParseError::InvalidCompressedSubclass(b))
}

/// Encode a logical subclass value into its wire byte form.
///
/// Mirrors `compress_subclass()` in `lib/libiax2/src/iax.c:1068-1086`:
/// values < 128 emit verbatim; values >= 128 must be exact powers of two
/// (otherwise the byte cannot represent them and the caller has a bug).
///
/// We treat "not representable" as a panic rather than an error because the
/// only callers are the encoder for our own typed enums — by construction
/// every variant either fits in a `u8` byte or is a power of two within the
/// `MAX_SHIFT` range.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_subclass_byte(v: u32) -> u8 {
    if v < u32::from(SC_LOG) {
        return v as u8;
    }
    // Must be a single-bit value.
    debug_assert!(
        v.is_power_of_two(),
        "subclass {v} >= 128 must be a power of two to fit in a compressed byte"
    );
    let shift = v.trailing_zeros();
    debug_assert!(
        shift <= u32::from(MAX_SHIFT),
        "subclass {v} shift {shift} exceeds MAX_SHIFT {MAX_SHIFT}"
    );
    SC_LOG | (shift as u8 & MAX_SHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_subclass_round_trips_verbatim() {
        for b in 0u8..0x80 {
            assert_eq!(parse_subclass_byte(b).unwrap(), u32::from(b));
            assert_eq!(encode_subclass_byte(u32::from(b)), b);
        }
    }

    #[test]
    fn compressed_subclass_round_trips() {
        // Every power of two from 1<<7 (=128) up to 1<<31.
        for shift in 7u32..=31 {
            let value = 1u32 << shift;
            let wire = encode_subclass_byte(value);
            assert_eq!(wire & SC_LOG, SC_LOG, "SC_LOG bit set for {value}");
            assert_eq!(parse_subclass_byte(wire).unwrap(), value);
        }
    }

    #[test]
    fn compressed_subclass_examples_from_rfc() {
        // 1 << 7 = 128 encodes as 0x80 | 7 = 0x87
        assert_eq!(encode_subclass_byte(128), 0x87);
        assert_eq!(parse_subclass_byte(0x87).unwrap(), 128);
        // 1 << 15 = 32768 encodes as 0x80 | 15 = 0x8f
        assert_eq!(encode_subclass_byte(1 << 15), 0x8f);
        assert_eq!(parse_subclass_byte(0x8f).unwrap(), 1 << 15);
    }

    #[test]
    fn frame_type_round_trip() {
        for v in [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
            let ft = FrameType::try_from(v).unwrap();
            assert_eq!(ft as u8, v);
        }
        assert!(matches!(
            FrameType::try_from(0),
            Err(ParseError::UnknownFrameType(0))
        ));
        assert!(matches!(
            FrameType::try_from(13),
            Err(ParseError::UnknownFrameType(13))
        ));
    }

    #[test]
    fn iax_command_calltoken_is_40() {
        assert_eq!(IaxCommand::CallToken as u8, 40);
        assert_eq!(IaxCommand::try_from(40).unwrap(), IaxCommand::CallToken);
    }

    #[test]
    fn control_subclass_radio_key_unkey() {
        assert_eq!(ControlSubclass::RadioKey as u8, 12);
        assert_eq!(ControlSubclass::RadioUnkey as u8, 13);
    }

    #[test]
    fn voice_format_g711u_is_bit2() {
        assert_eq!(VoiceFormat::G711U.as_u32(), 1 << 2);
        // bit-2 fits in a non-compressed byte so encode is verbatim.
        assert_eq!(encode_subclass_byte(VoiceFormat::G711U.as_u32()), 4);
    }

    #[test]
    fn voice_format_from_u32_inverts_as_u32() {
        for f in [VoiceFormat::G711U, VoiceFormat::G711A, VoiceFormat::G722] {
            assert_eq!(VoiceFormat::from_u32(f.as_u32()), Some(f));
        }
        // A multi-codec bitmask or unknown value is not a single codec.
        assert_eq!(
            VoiceFormat::from_u32(VoiceFormat::G711U.as_u32() | VoiceFormat::G711A.as_u32()),
            None
        );
        assert_eq!(VoiceFormat::from_u32(0), None);
    }

    #[test]
    fn frame_bytes_20ms_for_negotiable_formats() {
        assert_eq!(VoiceFormat::G711U.frame_bytes_20ms(), Some(160));
        assert_eq!(VoiceFormat::G711A.frame_bytes_20ms(), Some(160));
        assert_eq!(VoiceFormat::Slin.frame_bytes_20ms(), Some(320));
        assert_eq!(VoiceFormat::Slin16.frame_bytes_20ms(), Some(640));
        assert_eq!(VoiceFormat::Gsm.frame_bytes_20ms(), None);
    }

    #[test]
    fn sample_rate_for_negotiable_formats() {
        assert_eq!(VoiceFormat::G711U.sample_rate(), Some(8000));
        assert_eq!(VoiceFormat::G711A.sample_rate(), Some(8000));
        assert_eq!(VoiceFormat::Slin.sample_rate(), Some(8000));
        assert_eq!(VoiceFormat::Slin16.sample_rate(), Some(16000));
        assert_eq!(VoiceFormat::Gsm.sample_rate(), None);
    }
}
