// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The networks astar can dial on (astar-9b3e) — a straight port of the
//! Mac's `Network` (`AstarCore/Sources/AstarCore/Network.swift`). One
//! connection at a time — this is "where the next dial goes", not a
//! multi-link manager. AllStar is the founding network; `Hamlink`
//! (SvxReflector, iax-b3d7) and `M17` (iax-f2b8 Task 8) exist as variants now
//! so favorites/persistence are future-proof, but each stays unavailable
//! until the engine reports its own capability
//! ([`crate::conn::Conn::hamlink_available`] /
//! [`crate::conn::Conn::m17_available`]). Later families (DMR, D-Star) follow
//! the same pattern: new variant + engine capability.

use serde::{Deserialize, Deserializer, Serialize};

/// The engine capabilities [`Network::available`]/[`Network::resolve`] gate
/// on — one bool per non-AllStar network, mirroring the Mac's
/// `available(m17:)` (which folds Hamlink permanently off, astar-9b3e, into
/// the signature) except gui-rs's Hamlink gate is still live, so both ride
/// together in one small copy struct rather than two positional bools.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkCaps {
    /// Whether the engine reports Hamlink/SvxReflector capability (iax-b3d7).
    pub hamlink: bool,
    /// Whether the engine reports M17 capability (iax-f2b8 Task 4:
    /// `Station::m17_available`).
    pub m17: bool,
}

/// A network astar can dial on. Persisted as the lowercase strings
/// `"allstar"` / `"hamlink"` / `"m17"` (mirrors the Mac's `String`-backed
/// `Codable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Allstar,
    Hamlink,
    M17,
}

impl<'de> Deserialize<'de> for Network {
    /// Tolerant decode: an unknown-but-present string (e.g. a network from a
    /// build newer than this one, written then read after a downgrade) must
    /// fall back to `Allstar` rather than fail the whole TOML document — a
    /// derived decode would take the entire `Settings` parse down with it
    /// (`parse` refuses the file, boot falls back to defaults, and the next
    /// save clobbers it). `Serialize` stays derived (lowercase strings,
    /// unchanged).
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "hamlink" => Network::Hamlink,
            "m17" => Network::M17,
            _ => Network::Allstar,
        })
    }
}

impl Network {
    /// The networks the engine can actually drive right now. Today: AllStar
    /// always, `Hamlink`/`M17` only when `caps` says the engine can drive
    /// them — every "show the picker / badge?" decision derives from whether
    /// this returns more than one entry, so the UI is pixel-identical to
    /// pre-9b3e until the vendored Station reports the matching capability.
    #[must_use]
    pub fn available(caps: NetworkCaps) -> Vec<Network> {
        let mut networks = vec![Network::Allstar];
        if caps.hamlink {
            networks.push(Network::Hamlink);
        }
        if caps.m17 {
            networks.push(Network::M17);
        }
        networks
    }

    /// Map a persisted raw value to an AVAILABLE network. Unknown strings and
    /// known-but-unavailable networks both fall back to `Allstar` (always the
    /// default; nothing user-actionable in the mismatch).
    #[must_use]
    pub fn resolve(raw: &str, caps: NetworkCaps) -> Network {
        let parsed = match raw {
            "allstar" => Some(Network::Allstar),
            "hamlink" => Some(Network::Hamlink),
            "m17" => Some(Network::M17),
            _ => None,
        };
        match parsed {
            Some(network) if Network::available(caps).contains(&network) => network,
            _ => Network::Allstar,
        }
    }

    /// The picker segment / favorites tooltip title.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Network::Allstar => "AllStar",
            Network::Hamlink => "Hamlink",
            Network::M17 => "M17",
        }
    }

    /// The short capsule tag (status card, favorites rows) — same visual
    /// family as the codec badge.
    #[must_use]
    pub fn badge(self) -> &'static str {
        match self {
            Network::Allstar => "ASL",
            Network::Hamlink => "SVX",
            Network::M17 => "M17",
        }
    }

    /// The dial field's placeholder for this network.
    #[must_use]
    pub fn dial_placeholder(self) -> &'static str {
        match self {
            Network::Allstar => "Node or IP address",
            Network::Hamlink => "Reflector host / talkgroup",
            Network::M17 => "Reflector host:port / module",
        }
    }

    /// Whether the dial field admits `c` — the per-network input filter.
    /// AllStar keeps the smart-field rules verbatim (astar-427f / the
    /// existing `NodeEntryChanged` handler in `app.rs`): ASCII node digits,
    /// `* #` command dials, and hostname/IP characters. M17's grammar
    /// (`host[:port]/module` or `host[:port] module`, [`crate::m17_dial`])
    /// uniquely admits SPACE too — the alternate separator, not something to
    /// drop.
    #[must_use]
    pub fn admits_dial_char(self, c: char) -> bool {
        match self {
            Network::Allstar => c.is_ascii_alphanumeric() || ".:-*#".contains(c),
            Network::Hamlink => c.is_ascii_alphanumeric() || ".:-/#".contains(c),
            Network::M17 => c.is_ascii_alphanumeric() || ".:-/ ".contains(c),
        }
    }

    /// Whether the DTMF dialpad disclosure applies — an AllStar concern;
    /// reflector networks will bring their own sections later.
    #[must_use]
    pub fn shows_dialpad(self) -> bool {
        self == Network::Allstar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for a caps struct in tests.
    fn caps(hamlink: bool, m17: bool) -> NetworkCaps {
        NetworkCaps { hamlink, m17 }
    }

    #[test]
    fn allstar_is_default_and_only_network_with_no_caps() {
        assert_eq!(Network::default(), Network::Allstar);
        assert_eq!(
            Network::available(caps(false, false)),
            vec![Network::Allstar]
        );
        assert_eq!(
            Network::available(caps(true, false)),
            vec![Network::Allstar, Network::Hamlink]
        );
        assert_eq!(
            Network::available(caps(false, true)),
            vec![Network::Allstar, Network::M17]
        );
        assert_eq!(
            Network::available(caps(true, true)),
            vec![Network::Allstar, Network::Hamlink, Network::M17],
            "both caps on lists AllStar, then Hamlink, then M17"
        );
    }

    #[test]
    fn resolve_falls_back_to_allstar() {
        assert_eq!(
            Network::resolve("allstar", caps(false, false)),
            Network::Allstar
        );
        assert_eq!(
            Network::resolve("hamlink", caps(false, false)),
            Network::Allstar,
            "unavailable → fallback"
        );
        assert_eq!(
            Network::resolve("hamlink", caps(true, false)),
            Network::Hamlink
        );
        assert_eq!(
            Network::resolve("m17", caps(true, false)),
            Network::Allstar,
            "known but unavailable → fallback"
        );
        assert_eq!(Network::resolve("m17", caps(false, true)), Network::M17);
        assert_eq!(
            Network::resolve("bogus", caps(true, true)),
            Network::Allstar,
            "unknown string → fallback"
        );
    }

    #[test]
    fn allstar_filter_matches_todays_node_entry_filter_verbatim() {
        for c in ['a', 'Z', '5', '.', ':', '-', '*', '#'] {
            assert!(Network::Allstar.admits_dial_char(c), "{c} must be admitted");
        }
        for c in [' ', '+', '/', 'é'] {
            assert!(!Network::Allstar.admits_dial_char(c), "{c} must be dropped");
        }
    }

    #[test]
    fn hamlink_filter_admits_slash_instead_of_star() {
        for c in ['a', 'Z', '5', '.', ':', '-', '/', '#'] {
            assert!(Network::Hamlink.admits_dial_char(c), "{c} must be admitted");
        }
        for c in [' ', '+', '*', 'é'] {
            assert!(!Network::Hamlink.admits_dial_char(c), "{c} must be dropped");
        }
    }

    #[test]
    fn m17_filter_admits_slash_and_space_but_not_star_or_hash() {
        for c in ['a', 'Z', '5', '.', ':', '-', '/', ' '] {
            assert!(Network::M17.admits_dial_char(c), "{c} must be admitted");
        }
        for c in ['+', '*', '#', 'é'] {
            assert!(!Network::M17.admits_dial_char(c), "{c} must be dropped");
        }
    }

    #[test]
    fn display_badge_placeholder_and_dialpad_match_the_mac() {
        assert_eq!(Network::Allstar.display_name(), "AllStar");
        assert_eq!(Network::Hamlink.display_name(), "Hamlink");
        assert_eq!(Network::M17.display_name(), "M17");
        assert_eq!(Network::Allstar.badge(), "ASL");
        assert_eq!(Network::Hamlink.badge(), "SVX");
        assert_eq!(Network::M17.badge(), "M17");
        assert_eq!(Network::Allstar.dial_placeholder(), "Node or IP address");
        assert_eq!(
            Network::Hamlink.dial_placeholder(),
            "Reflector host / talkgroup"
        );
        assert_eq!(
            Network::M17.dial_placeholder(),
            "Reflector host:port / module"
        );
        assert!(Network::Allstar.shows_dialpad());
        assert!(!Network::Hamlink.shows_dialpad());
        assert!(!Network::M17.shows_dialpad());
    }

    #[test]
    fn serde_round_trips_as_lowercase_strings() {
        // TOML has no bare top-level scalar, so round-trip through a tiny
        // wrapper struct — the point is the lowercase string representation.
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            network: Network,
        }
        let doc = toml::to_string(&Wrap {
            network: Network::Allstar,
        })
        .unwrap();
        assert_eq!(doc.trim(), "network = \"allstar\"");
        let doc = toml::to_string(&Wrap {
            network: Network::Hamlink,
        })
        .unwrap();
        assert_eq!(doc.trim(), "network = \"hamlink\"");
        let doc = toml::to_string(&Wrap {
            network: Network::M17,
        })
        .unwrap();
        assert_eq!(doc.trim(), "network = \"m17\"");
        let parsed: Wrap = toml::from_str("network = \"hamlink\"\n").unwrap();
        assert_eq!(parsed.network, Network::Hamlink);
        let parsed: Wrap = toml::from_str("network = \"m17\"\n").unwrap();
        assert_eq!(parsed.network, Network::M17);
    }
}
