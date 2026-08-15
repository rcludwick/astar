// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Parse-time errors for the IAX2 wire codec.
//!
//! All variants borrow from `'static` strings; no allocation on the error path.

use thiserror::Error;

use crate::subclass::FrameType;

/// Errors produced by parsing IAX2 frames or information elements.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// Input slice ended before the parser had read all required bytes.
    #[error("truncated input: needed {needed} bytes, had {available}")]
    Truncated {
        /// Bytes the parser expected to be present.
        needed: usize,
        /// Bytes actually available at the point of failure.
        available: usize,
    },

    /// Frame `type` byte does not correspond to any `AST_FRAME_*` value the
    /// parser understands. The wire byte is preserved for diagnostics.
    #[error("unknown frame type: {0}")]
    UnknownFrameType(u8),

    /// Subclass (after uncompression) did not match any known variant for
    /// the surrounding frame type. The decoded logical value is preserved.
    #[error("unknown subclass {subclass} for frame type {frame_type:?}")]
    UnknownSubclass {
        /// The frame type whose subclass space was being decoded.
        frame_type: FrameType,
        /// The logical (uncompressed) subclass value.
        subclass: u32,
    },

    /// IE id byte did not match any known IE constant.
    /// Per RFC 5456 §8.6 unknown IEs should be tolerated by the state
    /// machine; the parser surfaces them so the caller can decide.
    #[error("unknown information element id: {0}")]
    UnknownIe(u8),

    /// IE payload was syntactically wrong (e.g. CAPABILITY length != 4).
    #[error("malformed IE {ie}: {reason}")]
    MalformedIe {
        /// The IE id whose payload failed validation.
        ie: u8,
        /// Static description of the violation.
        reason: &'static str,
    },

    /// A DTMF frame (`AST_FRAME_DTMF_BEGIN`/`_END`) carried a subclass byte
    /// that is not a valid DTMF digit. RFC 5456 §6.7 restricts the digit to
    /// `0-9`, `*`, `#`, and `A-D`. The offending byte is preserved.
    #[error("invalid DTMF digit subclass byte: {0:#x}")]
    InvalidDtmfDigit(u32),

    /// Subclass byte had the `SC_LOG` bit set but the shift width was out of
    /// range. RFC 5456 §6.2 caps the shift at `IAX_MAX_SHIFT = 0x1f`; values
    /// beyond that are rejected to avoid silent truncation.
    #[error("invalid compressed subclass byte: {0:#x}")]
    InvalidCompressedSubclass(u8),
}

/// Errors produced when encoding IAX2 frames or information elements.
///
/// Encode-time failures are structural — the caller fed the encoder a value
/// the wire format cannot represent (e.g. an IE payload longer than the
/// 1-byte length prefix can describe). See iax-545b for the round-trip bug
/// these errors prevent: prior to introducing `EncodeError`, `write_str`
/// silently clamped to 255 bytes and could split a multi-byte UTF-8
/// codepoint, producing wire bytes that `Ies::parse` then rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EncodeError {
    /// IE payload exceeds the 255-byte limit imposed by the 1-byte length
    /// prefix in RFC 5456 §8. Callers should either shorten the value
    /// (UTF-8 strings must be truncated by codepoint, not by byte) or
    /// surface the error.
    #[error("IE {ie} payload is {len} bytes; max is 255")]
    IeTooLong {
        /// The IE id whose payload overflowed.
        ie: u8,
        /// The actual payload length the caller attempted to encode.
        len: usize,
    },
}
