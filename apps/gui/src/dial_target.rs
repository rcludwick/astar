// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The smart dial-field classifier (astar-3939) — a straight port of the
//! Mac's `DialTarget` (astar-427f): one field accepts a node number OR a
//! direct address; `parse` classifies the raw text and `None` keeps the
//! Connect button disabled.

/// What the smart dial field's text means: a node number to resolve through
/// the registrar, or a direct address to dial as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialTarget {
    /// An AllStar node string — digits plus optional `*`/`#` command-dial
    /// characters (e.g. "55553", "*355553"). Dialed via registrar lookup.
    Node(String),
    /// A direct dial address — "host" or "host:port" (IP or hostname). The
    /// port defaults to 4569 downstream when omitted.
    Address(String),
}

impl DialTarget {
    /// Classify the dial field's raw text. Whitespace is trimmed first; the
    /// associated value is always the trimmed string. Returns `None` for empty
    /// or malformed input (bare `*`, empty port, invalid host chars, …).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        if text.is_empty() {
            return None;
        }

        // Node path: only digits and the `*`/`#` command-dial characters, with
        // at least one digit ("*" alone is a prefix, not a dial).
        if text
            .chars()
            .all(|c| c.is_ascii_digit() || c == '*' || c == '#')
        {
            return text
                .chars()
                .any(|c| c.is_ascii_digit())
                .then(|| Self::Node(text.to_string()));
        }

        // Address path: "host" or "host:port". A single colon splits the port;
        // more than one is malformed (no bracketed-IPv6 form yet).
        let mut parts = text.split(':');
        let host = parts.next().unwrap_or_default();
        let port = parts.next();
        if parts.next().is_some() || !is_valid_host(host) {
            return None;
        }
        if let Some(port) = port {
            if !is_valid_port(port) {
                return None;
            }
        }
        Some(Self::Address(text.to_string()))
    }
}

/// Hostname/IPv4 shape: ASCII alphanumerics, dots, and hyphens; starts and
/// ends alphanumeric; no empty labels (".." → invalid). Deliberately loose —
/// the resolver downstream is the real authority.
fn is_valid_host(host: &str) -> bool {
    let bytes = host.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && last.is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-')
        && !host.contains("..")
}

/// Port shape: all digits, in 1..=65535.
fn is_valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u32>().is_ok_and(|v| (1..=65535).contains(&v))
}

#[cfg(test)]
mod tests {
    use super::DialTarget;

    // Table tests mirroring the Mac's `DialTargetTests` (astar-427f) one for
    // one: digits (with optional `*`/`#` command prefixes) dial as a node;
    // anything with a dot/colon/letters dials as host or host:port; garbage
    // parses to None so the Connect button stays disabled.

    #[test]
    fn node_numbers() {
        assert_eq!(
            DialTarget::parse("55553"),
            Some(DialTarget::Node("55553".into()))
        );
        assert_eq!(DialTarget::parse("2"), Some(DialTarget::Node("2".into())));
    }

    #[test]
    fn command_prefixes_stay_on_the_node_path() {
        // `*`/`#` are AllStar command-dial characters, not address syntax —
        // "*3" + digits must keep dialing as a node string (astar-75fc keys).
        assert_eq!(
            DialTarget::parse("*355553"),
            Some(DialTarget::Node("*355553".into()))
        );
        assert_eq!(DialTarget::parse("#1"), Some(DialTarget::Node("#1".into())));
        assert_eq!(
            DialTarget::parse("*70"),
            Some(DialTarget::Node("*70".into()))
        );
    }

    #[test]
    fn bare_ipv4_is_an_address() {
        assert_eq!(
            DialTarget::parse("127.0.0.1"),
            Some(DialTarget::Address("127.0.0.1".into()))
        );
    }

    #[test]
    fn host_port_is_an_address() {
        assert_eq!(
            DialTarget::parse("127.0.0.1:4569"),
            Some(DialTarget::Address("127.0.0.1:4569".into()))
        );
        assert_eq!(
            DialTarget::parse("10.0.0.5:14569"),
            Some(DialTarget::Address("10.0.0.5:14569".into()))
        );
    }

    #[test]
    fn hostname_is_an_address() {
        assert_eq!(
            DialTarget::parse("localhost"),
            Some(DialTarget::Address("localhost".into()))
        );
        assert_eq!(
            DialTarget::parse("node.example-net.com"),
            Some(DialTarget::Address("node.example-net.com".into()))
        );
    }

    #[test]
    fn hostname_with_port_is_an_address() {
        assert_eq!(
            DialTarget::parse("localhost:4569"),
            Some(DialTarget::Address("localhost:4569".into()))
        );
    }

    #[test]
    fn digits_with_port_is_an_address() {
        // A colon is address syntax even when the host part is all digits.
        assert_eq!(
            DialTarget::parse("12345:4569"),
            Some(DialTarget::Address("12345:4569".into()))
        );
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            DialTarget::parse("  55553  "),
            Some(DialTarget::Node("55553".into()))
        );
        assert_eq!(
            DialTarget::parse(" example.com "),
            Some(DialTarget::Address("example.com".into()))
        );
    }

    #[test]
    fn empty_and_garbage_parse_to_none() {
        assert_eq!(DialTarget::parse(""), None);
        assert_eq!(DialTarget::parse("   "), None);
        assert_eq!(
            DialTarget::parse("*"),
            None,
            "command prefix with no digits is not a dial"
        );
        assert_eq!(DialTarget::parse("#*#"), None);
        assert_eq!(
            DialTarget::parse("hello world"),
            None,
            "inner whitespace is never valid"
        );
        assert_eq!(DialTarget::parse("."), None);
        assert_eq!(
            DialTarget::parse(".example.com"),
            None,
            "host can't start with a dot"
        );
        assert_eq!(
            DialTarget::parse("example.com."),
            None,
            "host can't end with a dot"
        );
        assert_eq!(
            DialTarget::parse("-host"),
            None,
            "host can't start with a hyphen"
        );
        assert_eq!(
            DialTarget::parse("host-"),
            None,
            "host can't end with a hyphen"
        );
        assert_eq!(DialTarget::parse("a..b"), None, "empty hostname label");
        assert_eq!(DialTarget::parse("host:"), None, "colon requires a port");
        assert_eq!(DialTarget::parse(":4569"), None, "port requires a host");
        assert_eq!(DialTarget::parse("host:abc"), None, "port must be numeric");
        assert_eq!(DialTarget::parse("host:0"), None, "port 0 is not dialable");
        assert_eq!(
            DialTarget::parse("host:99999"),
            None,
            "port must fit 1...65535"
        );
        assert_eq!(
            DialTarget::parse("a:1:2"),
            None,
            "at most one colon (no IPv6 form yet)"
        );
        assert_eq!(
            DialTarget::parse("*3.example.com"),
            None,
            "command chars never mix with hosts"
        );
    }
}
