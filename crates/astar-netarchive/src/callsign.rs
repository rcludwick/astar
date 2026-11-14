// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Base callsigns: the part of a talker that names the station.
//!
//! Reflectors send a talker in several shapes: `KX0AAA  D` (callsign, spaces,
//! a one-letter module suffix), `KX0AAA E`, `N0CALL-1` (an SSID). All three
//! are the same station as the bare callsign. A `/` is part of the call
//! (`N0CALL/P` is a portable operation), so it never splits.

/// A talker split into its base callsign and optional suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// Uppercase, no spaces, no control characters.
    pub base: String,
    /// The text after the first space or `-`, uppercase.
    pub suffix: Option<String>,
}

/// Split a raw talker. `None` when no base is left.
#[must_use]
pub fn split_call(raw: &str) -> Option<Call> {
    let clean: String = raw
        .chars()
        .filter_map(|c| {
            if c.is_whitespace() {
                Some(' ')
            } else if c.is_control() {
                None
            } else {
                Some(c)
            }
        })
        .collect::<String>()
        .to_uppercase();
    let clean = clean.trim();
    let (base, rest) = match clean.find([' ', '-']) {
        Some(i) => (&clean[..i], clean[i + 1..].trim()),
        None => (clean, ""),
    };
    if base.is_empty() {
        return None;
    }
    Some(Call {
        base: base.to_owned(),
        suffix: (!rest.is_empty()).then(|| rest.to_owned()),
    })
}

/// The base callsign of a raw talker.
#[must_use]
pub fn base_call(raw: &str) -> Option<String> {
    split_call(raw).map(|c| c.base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_call_reads_suffixes() {
        let c = |b: &str, s: Option<&str>| {
            Some(Call {
                base: b.into(),
                suffix: s.map(Into::into),
            })
        };
        assert_eq!(split_call("KX0AAA  D"), c("KX0AAA", Some("D")));
        assert_eq!(split_call("KX0AAA E"), c("KX0AAA", Some("E")));
        assert_eq!(split_call("N0CALL-1"), c("N0CALL", Some("1")));
        assert_eq!(split_call("  kx0bbb "), c("KX0BBB", None));
        assert_eq!(split_call("N0CALL/P"), c("N0CALL/P", None));
        assert_eq!(split_call("K\u{1b}X0AAA"), c("KX0AAA", None));
        assert_eq!(split_call("   "), None);
        assert_eq!(split_call("-1"), None);
    }

    #[test]
    fn base_call_drops_the_suffix() {
        assert_eq!(base_call("KX0AAA  D").as_deref(), Some("KX0AAA"));
        assert_eq!(base_call(""), None);
    }
}
