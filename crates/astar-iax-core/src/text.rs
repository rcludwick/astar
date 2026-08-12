// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Typed decoder / encoder for IAX2 `AST_FRAME_TEXT` (frametype 7) payloads
//! as used by `AllStarLink`'s `app_rpt`.
//!
//! The wire-level framing is already handled by [`crate::frame`] —
//! [`FrameType::Text`](crate::FrameType::Text) frames have an unused subclass
//! (`Raw(0)`) and carry the UTF-8 string directly in the payload. This module
//! decodes the *payload string itself* into the small set of structured
//! events `app_rpt` broadcasts over the text channel.
//!
//! # `AllStar` K-status
//!
//! Documented in [`docs/spec/allstar-dialect.md` §5](../../../docs/spec/allstar-dialect.md):
//! `app_rpt` emits keyed-state changes on the IAX2 text channel using the
//! format string
//!
//! ```text
//! K %s %s %d %d   →   K <src> <name> <keyed:0|1> <since-seconds>
//! ```
//!
//! produced by `handle_link_data()` in upstream
//! [`AllStarLink/app_rpt/apps/app_rpt.c`](https://github.com/AllStarLink/app_rpt/blob/master/apps/app_rpt.c)
//! around line 3461 (`snprintf(tmp1, sizeof(tmp1), "K %s %s %d %d", src,
//! myrpt->name, myrpt->keyed, n)`). The full `app_rpt.c` source is not
//! vendored locally; the format is reproduced from prior research and
//! pinned in the dialect spec. Any additional structured text payloads
//! (link-status, connection events, …) are deliberately not encoded here
//! until their upstream format strings can be cited; unrecognised payloads
//! fall through to [`TextEvent::Raw`] so callers can still observe them.
//!
//! Parsing is zero-copy: [`TextEvent::parse`] returns string fields that
//! borrow from the input slice. Use [`TextEvent::to_owned`] for handoff
//! across borrow boundaries.

use bytes::BufMut;
use thiserror::Error;

/// Errors produced when decoding a TEXT-channel payload.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TextParseError {
    /// Payload bytes were not valid UTF-8. TEXT channel payloads are
    /// specified as text strings (see `app_rpt`'s use of `ast_sendtext`),
    /// so a non-UTF-8 payload is a protocol violation.
    #[error("text payload is not valid UTF-8")]
    InvalidUtf8,
}

/// Structured view of a TEXT-channel payload.
///
/// String fields borrow from the parser's input slice; use
/// [`TextEvent::to_owned`] to detach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextEvent<'a> {
    /// `AllStar` `app_rpt` keyed-status broadcast.
    ///
    /// Format: `K <src> <name> <keyed:0|1> <since>` where
    /// - `src` is the remote node identifier sending the transition,
    /// - `name` is the local repeater name (from `rpt.conf`),
    /// - `keyed` is the new PTT state (`true` = keyed),
    /// - `since` is seconds since the previous transition.
    KStatus {
        /// Source node identifier (whitespace-free string).
        src: &'a str,
        /// Local repeater name from `rpt.conf`.
        name: &'a str,
        /// New keyed state: `true` if the radio is now keyed, `false` if unkeyed.
        keyed: bool,
        /// Seconds since the previous key/unkey transition.
        since: u32,
    },
    /// Any other text payload — preserved verbatim so the caller can
    /// observe formats this module doesn't yet recognise.
    Raw(&'a str),
}

impl<'a> TextEvent<'a> {
    /// Parse a TEXT-channel payload.
    ///
    /// Empty input is reported as `Raw("")` rather than an error: an empty
    /// TEXT payload is legal on the wire (the frame's payload length may be
    /// zero) and carries no structured meaning.
    ///
    /// # Errors
    ///
    /// Returns [`TextParseError::InvalidUtf8`] if `bytes` is not valid
    /// UTF-8.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, TextParseError> {
        let s = core::str::from_utf8(bytes).map_err(|_| TextParseError::InvalidUtf8)?;
        Ok(Self::parse_str(s))
    }

    /// Parse a TEXT-channel payload from an already-validated `&str`.
    #[must_use]
    pub fn parse_str(s: &'a str) -> Self {
        if let Some(ev) = parse_k_status(s) {
            ev
        } else {
            Self::Raw(s)
        }
    }

    /// Serialise this event into its canonical text form.
    ///
    /// The output is exactly what `app_rpt` would emit on the wire for a
    /// [`KStatus`](Self::KStatus); for [`Raw`](Self::Raw) it is the inner
    /// string verbatim. The result is guaranteed to round-trip through
    /// [`TextEvent::parse`] back to an equal value, provided the
    /// [`Raw`](Self::Raw) payload does not itself match a structured
    /// variant's syntax (callers shouldn't construct such ambiguous raws).
    pub fn encode(&self, buf: &mut dyn BufMut) {
        match *self {
            Self::KStatus {
                src,
                name,
                keyed,
                since,
            } => {
                buf.put_slice(b"K ");
                buf.put_slice(src.as_bytes());
                buf.put_u8(b' ');
                buf.put_slice(name.as_bytes());
                buf.put_u8(b' ');
                buf.put_slice(if keyed { b"1" } else { b"0" });
                buf.put_u8(b' ');
                // u32 max is 10 digits — write decimal without allocating.
                let mut scratch = [0u8; 10];
                let n = write_decimal(since, &mut scratch);
                buf.put_slice(&scratch[..n]);
            }
            Self::Raw(s) => buf.put_slice(s.as_bytes()),
        }
    }

    /// Detach from the borrowed input by copying owned strings.
    #[must_use]
    pub fn to_owned(&self) -> OwnedTextEvent {
        match *self {
            Self::KStatus {
                src,
                name,
                keyed,
                since,
            } => OwnedTextEvent::KStatus {
                src: src.to_owned(),
                name: name.to_owned(),
                keyed,
                since,
            },
            Self::Raw(s) => OwnedTextEvent::Raw(s.to_owned()),
        }
    }
}

/// Owned counterpart of [`TextEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedTextEvent {
    /// Owned form of [`TextEvent::KStatus`].
    KStatus {
        /// Source node identifier.
        src: String,
        /// Local repeater name.
        name: String,
        /// New keyed state.
        keyed: bool,
        /// Seconds since the previous transition.
        since: u32,
    },
    /// Owned form of [`TextEvent::Raw`].
    Raw(String),
}

impl OwnedTextEvent {
    /// Borrow back as a [`TextEvent`] for re-encoding.
    #[must_use]
    pub fn as_event(&self) -> TextEvent<'_> {
        match self {
            Self::KStatus {
                src,
                name,
                keyed,
                since,
            } => TextEvent::KStatus {
                src,
                name,
                keyed: *keyed,
                since: *since,
            },
            Self::Raw(s) => TextEvent::Raw(s),
        }
    }
}

// --- internals -----------------------------------------------------------

/// Try to decode a `K <src> <name> <keyed> <since>` payload. Returns
/// `None` if `s` doesn't match the K-status grammar — caller falls back
/// to [`TextEvent::Raw`].
fn parse_k_status(s: &str) -> Option<TextEvent<'_>> {
    // Must start with literal "K " — not just any token beginning with K
    // (e.g. "Keep-alive" would otherwise match).
    let rest = s.strip_prefix("K ")?;
    // Exactly four whitespace-separated fields; reject extra trailing
    // tokens so unrecognised K-prefixed messages survive as Raw. L4
    // (iax-f755): `split_whitespace` collapses runs of spaces to match
    // Asterisk's `sscanf("%s %s %d %d")`, which a literal `split(' ')` did
    // not — a double space between fields previously yielded an empty token
    // and dropped the K-status to Raw.
    let mut it = rest.split_whitespace();
    let src = it.next()?;
    let name = it.next()?;
    let keyed_tok = it.next()?;
    let since_tok = it.next()?;
    if it.next().is_some() {
        return None;
    }
    if src.is_empty() || name.is_empty() {
        return None;
    }
    let keyed = match keyed_tok {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let since: u32 = since_tok.parse().ok()?;
    Some(TextEvent::KStatus {
        src,
        name,
        keyed,
        since,
    })
}

/// Write `n` in decimal into `out`, returning the byte count written.
/// `out` must hold at least 10 bytes (max width of `u32::MAX`).
fn write_decimal(n: u32, out: &mut [u8; 10]) -> usize {
    if n == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut len = 0;
    let mut v = n;
    while v > 0 {
        tmp[len] = b'0' + u8::try_from(v % 10).expect("digit fits u8");
        v /= 10;
        len += 1;
    }
    for i in 0..len {
        out[i] = tmp[len - 1 - i];
    }
    len
}

// --- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_to_vec(ev: &TextEvent<'_>) -> Vec<u8> {
        let mut buf = Vec::new();
        ev.encode(&mut buf);
        buf
    }

    #[test]
    fn parses_keyed_k_status() {
        let ev = TextEvent::parse(b"K alice alice-node 1 42").unwrap();
        assert_eq!(
            ev,
            TextEvent::KStatus {
                src: "alice",
                name: "alice-node",
                keyed: true,
                since: 42,
            }
        );
    }

    // L4 (iax-f755): runs of whitespace collapse like Asterisk's sscanf.
    #[test]
    fn parses_k_status_with_double_spaces() {
        let ev = TextEvent::parse(b"K  alice   alice-node  1  42").unwrap();
        assert_eq!(
            ev,
            TextEvent::KStatus {
                src: "alice",
                name: "alice-node",
                keyed: true,
                since: 42,
            }
        );
    }

    #[test]
    fn parses_unkeyed_k_status() {
        let ev = TextEvent::parse(b"K alice alice-node 0 0").unwrap();
        assert_eq!(
            ev,
            TextEvent::KStatus {
                src: "alice",
                name: "alice-node",
                keyed: false,
                since: 0,
            }
        );
    }

    #[test]
    fn round_trip_k_status_keyed() {
        let ev = TextEvent::KStatus {
            src: "27225",
            name: "W6XYZ",
            keyed: true,
            since: 4,
        };
        let encoded = encode_to_vec(&ev);
        assert_eq!(encoded, b"K 27225 W6XYZ 1 4");
        let reparsed = TextEvent::parse(&encoded).unwrap();
        assert_eq!(reparsed, ev);
    }

    #[test]
    fn round_trip_k_status_unkeyed_zero_since() {
        let ev = TextEvent::KStatus {
            src: "1",
            name: "n",
            keyed: false,
            since: 0,
        };
        let encoded = encode_to_vec(&ev);
        assert_eq!(encoded, b"K 1 n 0 0");
        assert_eq!(TextEvent::parse(&encoded).unwrap(), ev);
    }

    #[test]
    fn round_trip_k_status_max_since() {
        let ev = TextEvent::KStatus {
            src: "src",
            name: "name",
            keyed: true,
            since: u32::MAX,
        };
        let encoded = encode_to_vec(&ev);
        assert_eq!(
            core::str::from_utf8(&encoded).unwrap(),
            format!("K src name 1 {}", u32::MAX),
        );
        assert_eq!(TextEvent::parse(&encoded).unwrap(), ev);
    }

    #[test]
    fn unknown_payload_falls_back_to_raw() {
        let ev = TextEvent::parse(b"hello world").unwrap();
        assert_eq!(ev, TextEvent::Raw("hello world"));
    }

    #[test]
    fn empty_payload_is_raw() {
        let ev = TextEvent::parse(b"").unwrap();
        assert_eq!(ev, TextEvent::Raw(""));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let err = TextEvent::parse(&[0xff, 0xfe, 0xfd]).unwrap_err();
        assert_eq!(err, TextParseError::InvalidUtf8);
    }

    #[test]
    fn k_prefixed_but_wrong_arity_is_raw() {
        // Three fields after "K " — not four.
        let ev = TextEvent::parse(b"K alice alice-node 1").unwrap();
        assert_eq!(ev, TextEvent::Raw("K alice alice-node 1"));
    }

    #[test]
    fn k_prefixed_with_extra_trailing_token_is_raw() {
        let ev = TextEvent::parse(b"K alice alice-node 1 42 extra").unwrap();
        assert_eq!(ev, TextEvent::Raw("K alice alice-node 1 42 extra"));
    }

    #[test]
    fn k_prefixed_non_boolean_keyed_is_raw() {
        let ev = TextEvent::parse(b"K alice alice-node 2 0").unwrap();
        assert_eq!(ev, TextEvent::Raw("K alice alice-node 2 0"));
    }

    #[test]
    fn k_prefixed_negative_since_is_raw() {
        let ev = TextEvent::parse(b"K alice alice-node 1 -1").unwrap();
        assert_eq!(ev, TextEvent::Raw("K alice alice-node 1 -1"));
    }

    #[test]
    fn k_without_space_is_raw() {
        let ev = TextEvent::parse(b"Keep-alive").unwrap();
        assert_eq!(ev, TextEvent::Raw("Keep-alive"));
    }

    #[test]
    fn raw_round_trips_verbatim() {
        let ev = TextEvent::Raw("arbitrary payload");
        let encoded = encode_to_vec(&ev);
        assert_eq!(encoded, b"arbitrary payload");
        assert_eq!(TextEvent::parse(&encoded).unwrap(), ev);
    }

    #[test]
    fn to_owned_and_as_event_round_trip() {
        let ev = TextEvent::KStatus {
            src: "alice",
            name: "alice-node",
            keyed: true,
            since: 42,
        };
        let owned = ev.to_owned();
        assert_eq!(owned.as_event(), ev);

        let raw = TextEvent::Raw("hi");
        let owned_raw = raw.to_owned();
        assert_eq!(owned_raw.as_event(), raw);
    }

    #[test]
    fn write_decimal_known_values() {
        let mut out = [0u8; 10];
        assert_eq!(write_decimal(0, &mut out), 1);
        assert_eq!(&out[..1], b"0");
        let mut out = [0u8; 10];
        let n = write_decimal(12_345, &mut out);
        assert_eq!(&out[..n], b"12345");
        let mut out = [0u8; 10];
        let n = write_decimal(u32::MAX, &mut out);
        assert_eq!(&out[..n], b"4294967295");
    }

    // --- proptest -------------------------------------------------------

    use proptest::prelude::*;

    /// Strategy for a single K-status token (`src` / `name`) — non-empty,
    /// ASCII, no whitespace, length 1..=16 so test cases stay small.
    fn k_token() -> impl Strategy<Value = String> {
        "[A-Za-z0-9_.\\-]{1,16}"
    }

    proptest! {
        #[test]
        fn proptest_k_status_round_trip(
            src in k_token(),
            name in k_token(),
            keyed: bool,
            since: u32,
        ) {
            let ev = TextEvent::KStatus {
                src: &src,
                name: &name,
                keyed,
                since,
            };
            let mut buf = Vec::new();
            ev.encode(&mut buf);
            let reparsed = TextEvent::parse(&buf).unwrap();
            prop_assert_eq!(reparsed, ev);
        }

        #[test]
        fn proptest_arbitrary_utf8_is_either_kstatus_or_raw(s in ".{0,64}") {
            // For any UTF-8 input, parse succeeds. If it round-trips
            // through encode → parse, the result must equal the original
            // parsed event (idempotence under canonicalisation).
            let ev = TextEvent::parse(s.as_bytes()).unwrap();
            let mut buf = Vec::new();
            ev.encode(&mut buf);
            let reparsed = TextEvent::parse(&buf).unwrap();
            prop_assert_eq!(reparsed, ev);
        }
    }
}
