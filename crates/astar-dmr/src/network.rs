// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Which DMR you mean.
//!
//! Every other network astar speaks is a thing you connect to. DMR is a family
//! of independently operated networks that happen to share a protocol — each
//! with its own operator, its own account, its own talkgroup numbering and its
//! own rules about who may connect. A talkgroup number on its own does not name
//! a target: TG 91 exists on more than one of these and means something
//! different on each. **The network is part of the address.**
//!
//! # The shape this settles
//!
//! `docs/design/dmr-networks.md` left it open whether each DMR network should
//! become its own `Network` case or whether the dial grammar should grow a
//! network selector. It is the selector: one DMR network *type*, carried
//! alongside the talkgroup, so the client picker shows one DMR segment rather
//! than eight. A case per operator would be honest and unusable.
//!
//! The classification is the other half of that. [`NetworkClass`] splits the
//! family in two — the independently run networks, which astar treats alike,
//! and BrandMeister, which is gated. That split is not editorial: it is the
//! only thing in this file that changes astar's behaviour.
//!
//! # What is deliberately not here
//!
//! **No master hostnames, ports or passwords.** A password is a per-network
//! secret and astar's rule is absolute — connect-time in-arg only, never on a
//! `Station`, never in a snapshot, event, error or log. Endpoints are directory
//! data with their own sourcing problem (`dmr-networks.md` §"Order of work",
//! item 3) and they move; a hostname compiled into the engine is a hostname
//! that goes stale in a release binary. This module names networks. It does not
//! know how to reach them.

use core::fmt;

/// How astar treats a DMR network, which is the only distinction here that has
/// teeth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkClass {
    /// Independently operated: its own registration, its own talkgroups, and
    /// no astar-side gate beyond the credentials the operator already holds.
    ///
    /// These are grouped together because astar has nothing to say that
    /// distinguishes them. They differ in size, geography and talkgroup
    /// numbering — all of which is the operator's business and the directory's
    /// job to carry, not a reason for the engine to treat them differently.
    Independent,
    /// BrandMeister, which is gated behind an explicit opt-in.
    ///
    /// Not because BrandMeister is bad. Because it is a private network whose
    /// operators set the terms, decide what counts as a violation, and have
    /// permanently blocked accounts — for conduct and for technical reasons.
    /// astar is a third-party client and cannot promise that connecting this
    /// way is within their rules. That is a real risk to the operator's *own*
    /// network access, created by using this software, so it is presented
    /// before it is taken rather than after. See `docs/design/dmr-networks.md`
    /// §"BrandMeister: a consent gate, and why".
    BrandMeister,
}

impl NetworkClass {
    /// The label this class carries in the UI.
    ///
    /// Fixed strings: they cross the C ABI and land in saved configuration, so
    /// they are part of the contract, not copy to be reworded in passing.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            NetworkClass::Independent => "Independent networks",
            NetworkClass::BrandMeister => "BrandMeister",
        }
    }

    /// Whether a network of this class may be dialled only after the operator
    /// has explicitly opted in.
    #[must_use]
    pub const fn requires_consent(self) -> bool {
        matches!(self, NetworkClass::BrandMeister)
    }
}

/// One DMR network.
///
/// The list is the one in `docs/design/dmr-networks.md`. It is not exhaustive
/// and never will be — networks appear and merge — so [`DmrNetwork::from_slug`]
/// answers `None` rather than guessing, and a directory row naming something
/// unknown stays listed and undialable instead of being quietly pointed
/// somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DmrNetwork {
    /// Small, permissive, straightforward registration — the first target,
    /// because proving the protocol against it cannot cost an operator their
    /// access somewhere that matters.
    Tgif,
    /// Community-run and open.
    FreeDmr,
    /// DMR+ / IPSC2 — a reflector-and-talkgroup hybrid.
    DmrPlus,
    SystemX,
    AmComm,
    VkDmr,
    FreeStar,
    Adn,
    /// The largest, and the gated one. See [`NetworkClass::BrandMeister`].
    BrandMeister,
}

/// Every network this build knows, in the order a picker should show them:
/// the independents first, alphabetical bar TGIF — which leads because it is
/// the one astar recommends starting on — and BrandMeister last, where the
/// gate is.
pub const ALL: &[DmrNetwork] = &[
    DmrNetwork::Tgif,
    DmrNetwork::Adn,
    DmrNetwork::AmComm,
    DmrNetwork::DmrPlus,
    DmrNetwork::FreeDmr,
    DmrNetwork::FreeStar,
    DmrNetwork::SystemX,
    DmrNetwork::VkDmr,
    DmrNetwork::BrandMeister,
];

impl DmrNetwork {
    /// The name the operator knows this network by.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            DmrNetwork::Tgif => "TGIF",
            DmrNetwork::FreeDmr => "FreeDMR",
            DmrNetwork::DmrPlus => "DMR+",
            DmrNetwork::SystemX => "SystemX",
            DmrNetwork::AmComm => "AmComm",
            DmrNetwork::VkDmr => "VKDMR",
            DmrNetwork::FreeStar => "FreeSTAR",
            DmrNetwork::Adn => "ADN",
            DmrNetwork::BrandMeister => "BrandMeister",
        }
    }

    /// The stable identifier used in dial grammar, directory rows and saved
    /// configuration.
    ///
    /// Lower-case ASCII, no spaces, and **fixed for the life of the field**:
    /// renaming one would strand every saved target that names it. The label
    /// above is what changes if a network rebrands; this does not.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            DmrNetwork::Tgif => "tgif",
            DmrNetwork::FreeDmr => "freedmr",
            DmrNetwork::DmrPlus => "dmrplus",
            DmrNetwork::SystemX => "systemx",
            DmrNetwork::AmComm => "amcomm",
            DmrNetwork::VkDmr => "vkdmr",
            DmrNetwork::FreeStar => "freestar",
            DmrNetwork::Adn => "adn",
            DmrNetwork::BrandMeister => "brandmeister",
        }
    }

    /// Parse a slug. Case-insensitive on the way in — a hand-edited config or
    /// a directory row is not required to match our capitalisation — but
    /// nothing else is forgiven, because a near-miss that resolves to the
    /// wrong network puts an operator on the wrong system under their own ID.
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<DmrNetwork> {
        let lowered = slug.trim().to_ascii_lowercase();
        ALL.iter().copied().find(|n| n.slug() == lowered)
    }

    /// How astar treats this network.
    #[must_use]
    pub const fn class(self) -> NetworkClass {
        match self {
            DmrNetwork::BrandMeister => NetworkClass::BrandMeister,
            _ => NetworkClass::Independent,
        }
    }

    /// Whether this network is one of the independently run ones astar groups
    /// together and treats alike.
    #[must_use]
    pub const fn is_independent(self) -> bool {
        matches!(self.class(), NetworkClass::Independent)
    }

    /// Whether dialling this network requires the operator to have opted in
    /// first. True for BrandMeister and nothing else.
    #[must_use]
    pub const fn requires_consent(self) -> bool {
        self.class().requires_consent()
    }
}

impl fmt::Display for DmrNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The networks astar will dial given the current consent setting.
///
/// One function rather than a filter written out at each call site, so the
/// gate cannot be forgotten in one of them. `brandmeister_consented` is the
/// operator's own answer — off by default, never pre-ticked, and never
/// inferred from anything else.
#[must_use]
pub fn dialable(brandmeister_consented: bool) -> Vec<DmrNetwork> {
    ALL.iter()
        .copied()
        .filter(|n| brandmeister_consented || !n.requires_consent())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_network_has_a_distinct_slug_and_label() {
        let mut slugs: Vec<&str> = ALL.iter().map(|n| n.slug()).collect();
        let count = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), count, "two networks share a slug");

        let mut labels: Vec<&str> = ALL.iter().map(|n| n.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count, "two networks share a label");
    }

    #[test]
    fn slugs_round_trip() {
        for &network in ALL {
            assert_eq!(DmrNetwork::from_slug(network.slug()), Some(network));
        }
    }

    #[test]
    fn slug_parsing_forgives_case_and_padding_and_nothing_else() {
        assert_eq!(DmrNetwork::from_slug("TGIF"), Some(DmrNetwork::Tgif));
        assert_eq!(
            DmrNetwork::from_slug("  FreeDMR "),
            Some(DmrNetwork::FreeDmr)
        );
        assert_eq!(DmrNetwork::from_slug("tg if"), None);
        assert_eq!(DmrNetwork::from_slug("brand-meister"), None);
        assert_eq!(DmrNetwork::from_slug(""), None);
    }

    #[test]
    fn brandmeister_is_the_only_gated_network() {
        let gated: Vec<DmrNetwork> = ALL
            .iter()
            .copied()
            .filter(|n| n.requires_consent())
            .collect();
        assert_eq!(gated, vec![DmrNetwork::BrandMeister]);

        for &network in ALL {
            assert_eq!(
                network.is_independent(),
                network != DmrNetwork::BrandMeister
            );
        }
    }

    #[test]
    fn the_gate_is_shut_by_default() {
        let without = dialable(false);
        assert!(!without.contains(&DmrNetwork::BrandMeister));
        assert!(without.contains(&DmrNetwork::Tgif));
        assert_eq!(without.len(), ALL.len() - 1);

        let with = dialable(true);
        assert!(with.contains(&DmrNetwork::BrandMeister));
        assert_eq!(with.len(), ALL.len());
    }

    #[test]
    fn tgif_leads_the_list_and_brandmeister_ends_it() {
        assert_eq!(ALL.first(), Some(&DmrNetwork::Tgif));
        assert_eq!(ALL.last(), Some(&DmrNetwork::BrandMeister));
    }

    #[test]
    fn the_class_labels_are_the_group_names() {
        assert_eq!(NetworkClass::Independent.label(), "Independent networks");
        assert_eq!(NetworkClass::BrandMeister.label(), "BrandMeister");
        assert!(!NetworkClass::Independent.requires_consent());
        assert!(NetworkClass::BrandMeister.requires_consent());
    }
}
