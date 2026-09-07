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
/// and never will be — networks appear and merge — so both parsers answer
/// `None` rather than guessing. `None` is not a refusal: a target naming a
/// network this build cannot name is dialed as an independent one, because
/// the family is read for the consent gate and nothing else. What guessing
/// would cost is worse than not knowing — it would put an operator on the
/// wrong system under their own registered ID.
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

/// The names each family is spelled with in a **directory row's** `system`
/// field, for [`DmrNetwork::from_system_slug`].
///
/// `ipsc2` and `ipsc3` are DMR+: those slugs name the server software the
/// network runs (`ipsc2-poland`), not the network, and DMR+ is what it is.
///
/// The order is the order the twin Swift table
/// (`DmrDial.familyNames`) carries, and first match wins in both — so a slug
/// that could satisfy two entries resolves the same way on both sides of the
/// ABI. Add a name to one and add it to the other.
const SYSTEM_NAMES: &[(DmrNetwork, &[&str])] = &[
    (DmrNetwork::BrandMeister, &["brandmeister"]),
    (DmrNetwork::FreeDmr, &["freedmr"]),
    (DmrNetwork::FreeStar, &["freestar"]),
    (
        DmrNetwork::DmrPlus,
        &["dmrplus", "dmr-plus", "ipsc2", "ipsc3"],
    ),
    (DmrNetwork::SystemX, &["systemx", "system-x"]),
    (DmrNetwork::AmComm, &["amcomm"]),
    (DmrNetwork::VkDmr, &["vkdmr", "vk-dmr"]),
    (DmrNetwork::Tgif, &["tgif"]),
    (DmrNetwork::Adn, &["adn"]),
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

    /// The stable identifier used in dial grammar and saved configuration.
    ///
    /// **Not what a directory row's `system` field holds.** `DVRef` enumerates
    /// *servers* — `freedmr-network`, `ipsc2-poland`, `xlx696` — and this
    /// enumerates *families*; checked on 2026-09-07, not one of the 111
    /// distinct `system` values in the feed's 185 DMR rows equals a slug here.
    /// [`DmrNetwork::from_system_slug`] is the bridge between the two, and
    /// `docs/design/dmr-networks.md` §"The directory's system slug is not the
    /// engine's family slug" is the reasoning.
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

    /// Resolve a **directory** `system` value — `DVRef`'s, not [`Self::slug`] —
    /// to the family that operates it, or `None` for one this build does not
    /// recognise.
    ///
    /// # Two vocabularies meet here
    ///
    /// A directory row names a *server*: one operator instance, of which a
    /// family has many (`freedmr-network` and `freedmr-reunion` are both
    /// FreeDMR; 19 rows in the 2026-09-07 feed are the former). [`Self::slug`]
    /// names a *family*, nine of them. Neither can be derived from the other
    /// by renaming, so both are kept and this is the documented bridge. It is
    /// what makes a directory row dialable at all: `Station::dmr_connect`
    /// resolves `system` through [`Self::from_slug`] first and this second,
    /// and a `system` that answers `None` here is still dialed — the family
    /// is consulted only for the consent gate, and a network astar does not
    /// recognise is not BrandMeister.
    ///
    /// # The twin that must stay in step
    ///
    /// `DmrDial.family(ofSystem:)` in
    /// `apps/macos/Packages/AstarCore/Sources/AstarCore/ReflectorAddressDial.swift`
    /// is this function in Swift, over the same table. The app groups its
    /// picker with that one and the engine gates with this one, so a name
    /// added to either belongs in both — they would otherwise disagree about
    /// which rows are BrandMeister.
    ///
    /// # The rule
    ///
    /// The slug either **is** a family's name or is that name followed by
    /// `-` or `_`, because the directory spells its systems
    /// `<family>-<place>` (`adn-systems-espana`, `hb_it_trani_conference`).
    /// Never a bare prefix test: `adn` would then claim `adnetwork`, and a
    /// wrong family is a network filed under somebody else's terms.
    #[must_use]
    pub fn from_system_slug(system: &str) -> Option<DmrNetwork> {
        let slug = system.trim().to_ascii_lowercase();
        if slug.is_empty() {
            return None;
        }
        SYSTEM_NAMES
            .iter()
            .find(|(_, names)| {
                names.iter().any(|name| {
                    slug.strip_prefix(name).is_some_and(|rest| {
                        rest.is_empty() || rest.starts_with('-') || rest.starts_with('_')
                    })
                })
            })
            .map(|(network, _)| *network)
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

    /// The table the app's `DmrDial.family(ofSystem:)` is checked against,
    /// with the same rows: real `system` values from the 2026-09-07 `DVRef`
    /// feed, plus the two cases the rule exists for.
    #[test]
    fn a_directory_system_resolves_to_its_family() {
        for (system, want) in [
            ("tgif", Some(DmrNetwork::Tgif)),
            ("freedmr-network", Some(DmrNetwork::FreeDmr)),
            ("ipsc2-poland", Some(DmrNetwork::DmrPlus)),
            ("dmrplus-ipsc2-uk", Some(DmrNetwork::DmrPlus)),
            ("system-x-uk", Some(DmrNetwork::SystemX)),
            ("brandmeister-3102", Some(DmrNetwork::BrandMeister)),
            ("hb_it_trani_conference", None),
            // A server row for a network this build has no name for. It is
            // NOT a refusal: `None` means "independent, unrecognised".
            ("xlx696", None),
            // The reason the rule is not `starts_with`: `adn` must not claim
            // a network whose name merely begins with those three letters.
            ("adnetwork", None),
            ("", None),
        ] {
            assert_eq!(
                DmrNetwork::from_system_slug(system),
                want,
                "system {system:?}"
            );
        }
    }

    /// Case and padding are forgiven on the way in — a hand-edited config or
    /// a directory row is not required to match our capitalisation — and the
    /// separator is either of the two the feed uses.
    #[test]
    fn a_directory_system_forgives_case_and_padding_and_takes_either_separator() {
        assert_eq!(
            DmrNetwork::from_system_slug("  FreeDMR-Network "),
            Some(DmrNetwork::FreeDmr)
        );
        assert_eq!(
            DmrNetwork::from_system_slug("adn_systems_espana"),
            Some(DmrNetwork::Adn)
        );
    }

    /// Every family's own slug is also a system name, so a target saved with
    /// the engine's spelling resolves through either door.
    #[test]
    fn every_slug_is_also_a_system_name() {
        for &network in ALL {
            assert_eq!(
                DmrNetwork::from_system_slug(network.slug()),
                Some(network),
                "{}",
                network.slug()
            );
        }
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
