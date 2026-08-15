// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The M17 dial-field grammar (iax-f2b8 Task 8) — a straight port of the
//! Mac's `M17Dial.parse` (`AstarCore/Sources/AstarCore/M17Dial.swift`):
//! `host[:port]/module` or `host[:port] module` — the module letter trails
//! `host[:port]` behind a `/` OR a ` ` (mirrors
//! `Network::M17::admits_dial_char`, the only network that admits both).
//! Port defaults to 17000 (the M17 reflector default) when omitted; the
//! module is a single ASCII letter, case-folded to uppercase — mirroring the
//! vendored `Station::m17_connect`'s own module validation.
//!
//! IPv6 is unsupported by design: a bracketed `[::1]:17000/A` form doesn't
//! parse here (more than one `:` before the separator is rejected, same as
//! the Mac and the AllStar/Hamlink `DialTarget`/`Network` grammars) — a
//! deliberate fail-closed rule, not an oversight.

/// Classify the M17 dial field's raw text into `(host, port, module)`.
/// Whitespace is trimmed at the ends first. Returns `None` for anything that
/// doesn't fit the grammar: no `/`/` ` separator, an empty host, more than one
/// `:` before the separator, an unparseable/zero/out-of-range port, or a
/// module that isn't exactly one ASCII letter. The module is uppercased.
#[must_use]
pub fn parse_m17_dial(raw: &str) -> Option<(String, u16, char)> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }

    // `host[:port]` never itself contains `/` or ` ` (host has no internal
    // whitespace, port is digits only), so the FIRST occurrence of either is
    // unambiguously the module separator — whichever grammar form was used.
    let sep = text.find(['/', ' '])?;
    let host_port = &text[..sep];
    let module_part = text[sep + 1..].trim();

    let mut module_chars = module_part.chars();
    let module = module_chars.next()?;
    if module_chars.next().is_some() || !module.is_ascii_alphabetic() {
        return None;
    }

    // At most one `:` — the part before it is the host, the part after (if
    // present) is the port.
    let mut parts = host_port.split(':');
    let host = parts.next().unwrap_or_default();
    let port_str = parts.next();
    if parts.next().is_some() || host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }

    let port = match port_str {
        Some(p) => {
            let parsed: u16 = p.parse().ok()?;
            if parsed == 0 {
                return None;
            }
            parsed
        }
        None => 17000,
    };

    Some((host.to_string(), port, module.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::parse_m17_dial;

    // Table tests mirroring the Mac's `M17DialTests` (astar-c2e5/iax-f2b8
    // Task 8) one for one.

    #[test]
    fn slash_separator_with_default_port() {
        assert_eq!(
            parse_m17_dial("m17.example.net/A"),
            Some(("m17.example.net".to_string(), 17000, 'A'))
        );
    }

    #[test]
    fn slash_separator_with_explicit_port() {
        assert_eq!(
            parse_m17_dial("m17.example.net:17001/A"),
            Some(("m17.example.net".to_string(), 17001, 'A'))
        );
    }

    #[test]
    fn space_separator_with_default_port() {
        assert_eq!(
            parse_m17_dial("m17.example.net A"),
            Some(("m17.example.net".to_string(), 17000, 'A'))
        );
    }

    #[test]
    fn space_separator_with_explicit_port() {
        assert_eq!(
            parse_m17_dial("m17.example.net:17001 A"),
            Some(("m17.example.net".to_string(), 17001, 'A'))
        );
    }

    #[test]
    fn extra_space_before_module_is_tolerated() {
        assert_eq!(
            parse_m17_dial("m17.example.net  A"),
            Some(("m17.example.net".to_string(), 17000, 'A'))
        );
    }

    #[test]
    fn module_is_uppercased() {
        assert_eq!(
            parse_m17_dial("m17.example.net/a"),
            Some(("m17.example.net".to_string(), 17000, 'A'))
        );
        assert_eq!(
            parse_m17_dial("m17.example.net z"),
            Some(("m17.example.net".to_string(), 17000, 'Z'))
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            parse_m17_dial("  m17.example.net/A  "),
            Some(("m17.example.net".to_string(), 17000, 'A'))
        );
    }

    #[test]
    fn empty_and_whitespace_only_rejected() {
        assert_eq!(parse_m17_dial(""), None);
        assert_eq!(parse_m17_dial("   "), None);
    }

    #[test]
    fn no_separator_rejected() {
        assert_eq!(parse_m17_dial("m17.example.net"), None, "no module at all");
        assert_eq!(
            parse_m17_dial("m17.example.net:17001"),
            None,
            "port but no module"
        );
    }

    #[test]
    fn empty_module_rejected() {
        assert_eq!(parse_m17_dial("m17.example.net/"), None);
        assert_eq!(parse_m17_dial("m17.example.net/ "), None);
    }

    #[test]
    fn multi_character_module_rejected() {
        assert_eq!(parse_m17_dial("m17.example.net/AB"), None);
        assert_eq!(parse_m17_dial("m17.example.net AB"), None);
    }

    #[test]
    fn non_letter_module_rejected() {
        assert_eq!(parse_m17_dial("m17.example.net/1"), None);
        assert_eq!(parse_m17_dial("m17.example.net/#"), None);
    }

    #[test]
    fn empty_host_rejected() {
        assert_eq!(parse_m17_dial("/A"), None);
        assert_eq!(parse_m17_dial(":17000/A"), None);
        assert_eq!(parse_m17_dial(" /A"), None);
    }

    #[test]
    fn host_with_internal_space_rejected() {
        // The first space is consumed as the module separator, leaving a
        // multi-character "module" — which is itself invalid.
        assert_eq!(parse_m17_dial("my host/A"), None);
    }

    #[test]
    fn more_than_one_colon_rejected() {
        assert_eq!(
            parse_m17_dial("host:1:2/A"),
            None,
            "no bracketed-IPv6 form yet"
        );
    }

    #[test]
    fn non_numeric_port_rejected() {
        assert_eq!(parse_m17_dial("host:abc/A"), None);
    }

    #[test]
    fn zero_port_rejected() {
        assert_eq!(parse_m17_dial("host:0/A"), None);
    }

    #[test]
    fn out_of_range_port_rejected() {
        assert_eq!(parse_m17_dial("host:99999/A"), None);
    }

    #[test]
    fn empty_port_after_colon_rejected() {
        assert_eq!(parse_m17_dial("host:/A"), None);
    }
}
