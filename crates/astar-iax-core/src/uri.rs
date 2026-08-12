// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `iax:` URI scheme parser and serializer, per [RFC 5456 §5].
//!
//! RFC 5456 §5.1 defines the grammar:
//!
//! ```text
//! iax-uri  = "iax:" [ userinfo "@" ] host [ ":" port ]
//!            [ "/" number [ "?" context ] ]
//! userinfo = <as specified in RFC 3986>
//! host     = <as specified in RFC 3986>
//! port     = <as specified in RFC 3986>
//! number   = *(unreserved / sub-delims / pct-encoded)
//! context  = *(unreserved / sub-delims / pct-encoded)
//! ```
//!
//! Examples from §5:
//! - `iax:example.com/alice`
//! - `iax:example.com:4569/alice`
//! - `iax:example.com:4570/alice?friends`
//! - `iax:192.0.2.4:4569/alice?friends`
//! - `iax:[2001:db8::1]:4569/alice?friends`
//! - `iax:example.com/12022561414`
//! - `iax:johnQ@example.com/12022561414`
//!
//! Per RFC 3986 the `userinfo` production permits a colon, so this parser
//! splits `userinfo` into an optional `user:password` pair (the colon is
//! reserved for that split; everything before the first `:` is the user,
//! the remainder is the password).
//!
//! # Deviation from the ticket
//!
//! Ticket iax-3ffa asks for an `iax2:` scheme. RFC 5456 §5 registers the
//! scheme as `iax:` (verified against the published RFC text and its ABNF
//! examples), so this parser is canonical against `iax:`. The legacy /
//! colloquial `iax2:` prefix is also accepted on input for robustness, but
//! [`Display`](std::fmt::Display) always emits the RFC-canonical `iax:`.
//!
//! [RFC 5456 §5]: https://datatracker.ietf.org/doc/html/rfc5456#section-5

use core::fmt;
use core::str::FromStr;

use thiserror::Error;

/// The RFC-canonical scheme prefix.
const SCHEME: &str = "iax:";
/// Legacy/colloquial scheme prefix accepted on input only.
const SCHEME_LEGACY: &str = "iax2:";

/// A parsed `iax:` URI (RFC 5456 §5).
///
/// All string components are stored exactly as they appear on the wire
/// (percent-encoding is preserved verbatim; this type does not decode it).
/// [`Display`](fmt::Display) round-trips: `Iax2Uri::try_from(s).unwrap().to_string()`
/// reproduces the canonical form of `s`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Iax2Uri {
    /// Optional user portion of `userinfo` (before any `:`), e.g. `johnQ`.
    pub user: Option<String>,
    /// Optional password portion of `userinfo` (after the first `:`).
    /// Only set when a `user` is present and a `:` appeared in `userinfo`.
    pub password: Option<String>,
    /// Host: a reg-name, IPv4 address, or bracketed IPv6 literal.
    /// Stored *without* the surrounding `[` `]` for IPv6 (see [`is_ipv6`]).
    ///
    /// [`is_ipv6`]: Iax2Uri::is_ipv6
    pub host: String,
    /// Whether `host` was given as a bracketed IPv6 literal.
    pub is_ipv6: bool,
    /// Optional port.
    pub port: Option<u16>,
    /// Optional `number` (the dialed extension), following `/`.
    pub number: Option<String>,
    /// Optional `context`, following `?`. Only meaningful when `number` is set.
    pub context: Option<String>,
}

/// Errors returned when parsing an `iax:` URI string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Iax2UriError {
    /// Input did not begin with the `iax:` (or legacy `iax2:`) scheme.
    #[error("missing or unrecognized scheme (expected \"iax:\")")]
    BadScheme,

    /// The host component was empty (e.g. `iax:/alice` or `iax:@host`).
    #[error("empty host")]
    EmptyHost,

    /// A bracketed IPv6 literal was opened with `[` but not closed with `]`.
    #[error("unterminated IPv6 literal (missing ']')")]
    UnterminatedIpv6,

    /// The `:port` component was present but empty or not a valid `u16`.
    #[error("invalid port: {0:?}")]
    BadPort(String),

    /// A `?context` appeared without a preceding `/number`.
    #[error("context given without a number")]
    ContextWithoutNumber,
}

impl Iax2Uri {
    /// Construct a minimal URI from just a host.
    #[must_use]
    pub fn from_host(host: impl Into<String>) -> Self {
        Self {
            user: None,
            password: None,
            host: host.into(),
            is_ipv6: false,
            port: None,
            number: None,
            context: None,
        }
    }

    /// Parse an `iax:` URI string per RFC 5456 §5.
    ///
    /// # Errors
    ///
    /// Returns [`Iax2UriError`] for any malformed input. Never panics.
    pub fn parse(input: &str) -> Result<Self, Iax2UriError> {
        // 1. Strip the scheme (canonical first, then legacy).
        let rest = if let Some(r) = input.strip_prefix(SCHEME) {
            r
        } else if let Some(r) = input.strip_prefix(SCHEME_LEGACY) {
            r
        } else {
            return Err(Iax2UriError::BadScheme);
        };

        // 2. Split off the path/query tail at the first '/'.
        //    Everything before the first '/' is the authority
        //    ([userinfo@]host[:port]); after it is number[?context].
        let (authority, path_tail) = match rest.find('/') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };

        // 3. Authority: split optional userinfo at the LAST '@' so an '@' in
        //    a (percent-encoded) password edge case still binds host correctly.
        let (userinfo, hostport) = match authority.rfind('@') {
            Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
            None => (None, authority),
        };

        let (user, password) = match userinfo {
            Some(ui) => match ui.split_once(':') {
                Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
                None => (Some(ui.to_string()), None),
            },
            None => (None, None),
        };

        // 4. host[:port] — handle bracketed IPv6 literals specially so the
        //    colons inside the address are not mistaken for the port sep.
        let (host, is_ipv6, port) = Self::parse_hostport(hostport)?;

        if host.is_empty() {
            return Err(Iax2UriError::EmptyHost);
        }

        // 5. path tail: number[?context]
        let (number, context) = match path_tail {
            None => (None, None),
            Some(tail) => match tail.split_once('?') {
                Some((num, ctx)) => (Some(num.to_string()), Some(ctx.to_string())),
                None => (Some(tail.to_string()), None),
            },
        };

        // A '?context' with no number is malformed. This can only happen if
        // the tail existed (a '/' was present) but the number is empty while
        // a context is set, e.g. "iax:host/?ctx".
        if number.as_deref() == Some("") && context.is_some() {
            return Err(Iax2UriError::ContextWithoutNumber);
        }

        Ok(Self {
            user,
            password,
            host,
            is_ipv6,
            port,
            number: number.filter(|n| !n.is_empty()),
            context,
        })
    }

    /// Parse the `host[:port]` portion, returning `(host, is_ipv6, port)`.
    fn parse_hostport(s: &str) -> Result<(String, bool, Option<u16>), Iax2UriError> {
        if let Some(after_bracket) = s.strip_prefix('[') {
            // Bracketed IPv6 literal: "[...]" optionally followed by ":port".
            let close = after_bracket
                .find(']')
                .ok_or(Iax2UriError::UnterminatedIpv6)?;
            let host = after_bracket[..close].to_string();
            let remainder = &after_bracket[close + 1..];
            let port = match remainder.strip_prefix(':') {
                Some(p) => Some(parse_port(p)?),
                None if remainder.is_empty() => None,
                None => return Err(Iax2UriError::BadPort(remainder.to_string())),
            };
            Ok((host, true, port))
        } else {
            // reg-name or IPv4: split port at the (single) ':' from the right.
            match s.rsplit_once(':') {
                Some((host, port)) => Ok((host.to_string(), false, Some(parse_port(port)?))),
                None => Ok((s.to_string(), false, None)),
            }
        }
    }
}

/// Parse a port string into a `u16`, mapping any failure to [`Iax2UriError::BadPort`].
fn parse_port(s: &str) -> Result<u16, Iax2UriError> {
    s.parse::<u16>()
        .map_err(|_| Iax2UriError::BadPort(s.to_string()))
}

impl fmt::Display for Iax2Uri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(SCHEME)?;

        if let Some(user) = &self.user {
            f.write_str(user)?;
            if let Some(pw) = &self.password {
                write!(f, ":{pw}")?;
            }
            f.write_str("@")?;
        }

        if self.is_ipv6 {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }

        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }

        if let Some(number) = &self.number {
            write!(f, "/{number}")?;
            if let Some(context) = &self.context {
                write!(f, "?{context}")?;
            }
        }

        Ok(())
    }
}

impl TryFrom<&str> for Iax2Uri {
    type Error = Iax2UriError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl FromStr for Iax2Uri {
    type Err = Iax2UriError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal examples from RFC 5456 §5 must all round-trip exactly.
    #[test]
    fn rfc_examples_round_trip() {
        let examples = [
            "iax:example.com/alice",
            "iax:example.com:4569/alice",
            "iax:example.com:4570/alice?friends",
            "iax:192.0.2.4:4569/alice?friends",
            "iax:[2001:db8::1]:4569/alice?friends",
            "iax:example.com/12022561414",
            "iax:johnQ@example.com/12022561414",
        ];
        for ex in examples {
            let uri = Iax2Uri::parse(ex).unwrap_or_else(|e| panic!("parse {ex:?}: {e}"));
            assert_eq!(uri.to_string(), ex, "round-trip mismatch for {ex:?}");
        }
    }

    #[test]
    fn host_only() {
        let uri = Iax2Uri::parse("iax:example.com").unwrap();
        assert_eq!(uri.host, "example.com");
        assert_eq!(uri.port, None);
        assert_eq!(uri.number, None);
        assert_eq!(uri.context, None);
        assert_eq!(uri.to_string(), "iax:example.com");
    }

    #[test]
    fn full_form_user_password() {
        let uri = Iax2Uri::parse("iax:alice:secret@host.example:4569/200?ctx").unwrap();
        assert_eq!(uri.user.as_deref(), Some("alice"));
        assert_eq!(uri.password.as_deref(), Some("secret"));
        assert_eq!(uri.host, "host.example");
        assert!(!uri.is_ipv6);
        assert_eq!(uri.port, Some(4569));
        assert_eq!(uri.number.as_deref(), Some("200"));
        assert_eq!(uri.context.as_deref(), Some("ctx"));
        assert_eq!(
            uri.to_string(),
            "iax:alice:secret@host.example:4569/200?ctx"
        );
    }

    #[test]
    fn user_without_password() {
        let uri = Iax2Uri::parse("iax:johnQ@example.com/12022561414").unwrap();
        assert_eq!(uri.user.as_deref(), Some("johnQ"));
        assert_eq!(uri.password, None);
    }

    #[test]
    fn ipv6_literal() {
        let uri = Iax2Uri::parse("iax:[2001:db8::1]:4569/alice?friends").unwrap();
        assert!(uri.is_ipv6);
        assert_eq!(uri.host, "2001:db8::1");
        assert_eq!(uri.port, Some(4569));
        assert_eq!(uri.to_string(), "iax:[2001:db8::1]:4569/alice?friends");
    }

    #[test]
    fn ipv6_no_port() {
        let uri = Iax2Uri::parse("iax:[2001:db8::1]/alice").unwrap();
        assert!(uri.is_ipv6);
        assert_eq!(uri.host, "2001:db8::1");
        assert_eq!(uri.port, None);
        assert_eq!(uri.to_string(), "iax:[2001:db8::1]/alice");
    }

    #[test]
    fn legacy_iax2_scheme_accepted_but_canonicalized() {
        let uri = Iax2Uri::parse("iax2:example.com/alice").unwrap();
        assert_eq!(uri.host, "example.com");
        // Display always emits the RFC-canonical "iax:".
        assert_eq!(uri.to_string(), "iax:example.com/alice");
    }

    #[test]
    fn from_str_and_try_from_agree() {
        let s = "iax:example.com:4569/alice";
        let a: Iax2Uri = s.parse().unwrap();
        let b = Iax2Uri::try_from(s).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn from_host_helper() {
        let uri = Iax2Uri::from_host("example.com");
        assert_eq!(uri.to_string(), "iax:example.com");
    }

    // --- error cases: must return a typed error, never panic ---

    #[test]
    fn err_bad_scheme() {
        assert_eq!(
            Iax2Uri::parse("sip:example.com"),
            Err(Iax2UriError::BadScheme)
        );
        assert_eq!(Iax2Uri::parse("example.com"), Err(Iax2UriError::BadScheme));
        assert_eq!(Iax2Uri::parse(""), Err(Iax2UriError::BadScheme));
        // Substring but not a prefix.
        assert_eq!(
            Iax2Uri::parse(" iax:example.com"),
            Err(Iax2UriError::BadScheme)
        );
    }

    #[test]
    fn err_empty_host() {
        assert_eq!(Iax2Uri::parse("iax:/alice"), Err(Iax2UriError::EmptyHost));
        assert_eq!(
            Iax2Uri::parse("iax:user@/alice"),
            Err(Iax2UriError::EmptyHost)
        );
        assert_eq!(Iax2Uri::parse("iax:"), Err(Iax2UriError::EmptyHost));
    }

    #[test]
    fn err_bad_port() {
        assert_eq!(
            Iax2Uri::parse("iax:example.com:notaport/alice"),
            Err(Iax2UriError::BadPort("notaport".into()))
        );
        // Out of u16 range.
        assert_eq!(
            Iax2Uri::parse("iax:example.com:99999/alice"),
            Err(Iax2UriError::BadPort("99999".into()))
        );
        // Empty port after ':'.
        assert_eq!(
            Iax2Uri::parse("iax:example.com:/alice"),
            Err(Iax2UriError::BadPort(String::new()))
        );
        // IPv6 bracketed with bad port.
        assert_eq!(
            Iax2Uri::parse("iax:[2001:db8::1]:bad/alice"),
            Err(Iax2UriError::BadPort("bad".into()))
        );
    }

    #[test]
    fn err_unterminated_ipv6() {
        assert_eq!(
            Iax2Uri::parse("iax:[2001:db8::1/alice"),
            Err(Iax2UriError::UnterminatedIpv6)
        );
    }

    #[test]
    fn err_ipv6_trailing_junk_after_bracket() {
        // Anything after ']' that is not ":port" is a bad port.
        assert_eq!(
            Iax2Uri::parse("iax:[2001:db8::1]junk/alice"),
            Err(Iax2UriError::BadPort("junk".into()))
        );
    }

    #[test]
    fn err_context_without_number() {
        assert_eq!(
            Iax2Uri::parse("iax:host/?ctx"),
            Err(Iax2UriError::ContextWithoutNumber)
        );
    }

    #[test]
    fn number_without_context() {
        let uri = Iax2Uri::parse("iax:host/100").unwrap();
        assert_eq!(uri.number.as_deref(), Some("100"));
        assert_eq!(uri.context, None);
    }

    #[test]
    fn pct_encoded_preserved_verbatim() {
        // Parser does not decode percent-encoding; it round-trips bytes as-is.
        let s = "iax:example.com/al%20ice?ctx%2Fa";
        let uri = Iax2Uri::parse(s).unwrap();
        assert_eq!(uri.number.as_deref(), Some("al%20ice"));
        assert_eq!(uri.context.as_deref(), Some("ctx%2Fa"));
        assert_eq!(uri.to_string(), s);
    }

    #[test]
    fn no_panic_on_arbitrary_short_inputs() {
        // Fuzz-lite: a spread of odd inputs must each return a Result, no panic.
        for s in [
            "iax:",
            "iax::",
            "iax:@",
            "iax:[",
            "iax:[]",
            "iax:host:",
            "iax:/",
            "iax:?",
            "iax2:",
            "iax:host//ctx",
            "iax:::::",
        ] {
            let _ = Iax2Uri::parse(s);
        }
    }
}
