// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The directory's DMR rows, arranged the way a picker has to show them.
///
/// 185 masters across 111 system slugs is a flat list nobody can use, and
/// nine families is the only grouping that means anything — it is the same
/// grouping `astar_dmr::DmrNetwork` makes, and it is where the one question
/// with teeth (`requiresConsent`) is asked. Everything a family does not
/// claim goes under one heading and is dialled exactly like the rest.
///
/// Pure, so the picker's shape is testable without a directory, a view or a
/// clock.
public enum DmrSystemCatalog {
    /// One dialable master.
    public struct System: Identifiable, Equatable, Sendable {
        /// The directory's `system` slug — what names the login, the password
        /// and the upstream talkgroup list.
        public let slug: String
        /// The row's display name ("FreeDMR EU").
        public let name: String
        /// The row's `DirectoryEntry.id` — what goes in the dial field, because
        /// it is what the directory resolves. Several masters share one slug
        /// (19 rows are `freedmr-network`), so the slug cannot identify one.
        public let entryID: String
        public let country: String?

        public var id: String { entryID }

        public init(slug: String, name: String, entryID: String, country: String? = nil) {
            self.slug = slug
            self.name = name
            self.entryID = entryID
            self.country = country
        }
    }

    /// One heading in the picker.
    public struct Group: Identifiable, Equatable, Sendable {
        /// The family, or `nil` for the independent networks astar does not
        /// name — which is most of them.
        public let family: DmrFamily?
        public let title: String
        public let systems: [System]

        public var id: String { family?.rawValue ?? "independent" }
    }

    /// The heading everything unrecognised sits under. Not "Other": these are
    /// independently run networks with their own operators and their own
    /// passwords, and most of the directory is here.
    public static let independentTitle = "Independent networks"

    /// Group the dialable DMR rows for a picker.
    ///
    /// A row with no `dial` is left out — 30 of the 185 have none, and a
    /// picker entry that cannot be dialled is not a choice. (They stay
    /// listed in the reflector directory pane, which is where "listed, not
    /// dialable" belongs.)
    ///
    /// **BrandMeister is absent unless `consented`.** That is the gate doing
    /// its work at the affordance rather than only at the dial: an operator
    /// who has not read and accepted BrandMeister's terms is not offered
    /// their network. Consent puts the group back, in its normal place.
    public static func grouped(_ entries: [DirectoryEntry], consented: Bool) -> [Group] {
        var byFamily: [DmrFamily?: [System]] = [:]
        for entry in entries where entry.network == .dmr {
            guard case .mmdvm(let slug, _, _) = entry.dial else { continue }
            let family = DmrDial.family(ofSystem: slug)
            if family?.requiresConsent == true, !consented { continue }
            byFamily[family, default: []].append(
                System(
                    slug: slug, name: entry.name, entryID: entry.id, country: entry.country))
        }
        // Families in their declared order, then everything else under one
        // heading — alphabetical inside each, because 19 FreeDMR masters in
        // feed order is a list nobody can scan.
        var groups: [Group] = DmrFamily.allCases.compactMap { family in
            guard let systems = byFamily[family], !systems.isEmpty else { return nil }
            return Group(family: family, title: family.displayName, systems: sorted(systems))
        }
        if let independent = byFamily[DmrFamily?.none], !independent.isEmpty {
            groups.append(
                Group(family: nil, title: independentTitle, systems: sorted(independent)))
        }
        return groups
    }

    /// The system slug for a dial field's address, when it names a directory
    /// row — what the password lookup and the consent check both key on.
    /// `nil` when the text names no row, which includes every typed address.
    public static func slug(forEntryID id: String, in entries: [DirectoryEntry]) -> String? {
        let key = id.trimmingCharacters(in: .whitespaces).lowercased()
        guard !key.isEmpty else { return nil }
        for entry in entries where entry.network == .dmr && entry.id.lowercased() == key {
            if case .mmdvm(let slug, _, _) = entry.dial { return slug }
        }
        return nil
    }

    /// A readable name for a system SLUG, for the places that pick a network
    /// rather than a master — the password field, chiefly.
    ///
    /// The directory names servers, not networks, so there is no published
    /// label for a slug: `freedmr-network` covers 19 masters and none of their
    /// names is the network's. This builds one from the slug, using the family
    /// name where a family is recognised (`ipsc2-poland` → "DMR+ Poland",
    /// which is what that network is actually called) and title-casing the
    /// segments otherwise. The raw slug stays on screen beside it, because the
    /// slug is what the network's own paperwork says.
    public static func label(forSlug slug: String) -> String {
        let trimmed = slug.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return "" }
        let segments = trimmed.split(whereSeparator: { $0 == "-" || $0 == "_" }).map(String.init)
        guard !segments.isEmpty else { return trimmed }
        if let family = DmrDial.family(ofSystem: trimmed) {
            // The family owns the first segment (or two, for `dmr-plus`), so
            // drop what its own name already says and keep the rest.
            let rest = segments.dropFirst().filter { $0.lowercased() != "network" }
            let tail = rest.map(titleCased).joined(separator: " ")
            return tail.isEmpty ? family.displayName : "\(family.displayName) \(tail)"
        }
        return segments.map(titleCased).joined(separator: " ")
    }

    /// Upper-cases the first letter and leaves the rest alone: `xlx696` is
    /// "Xlx696" and `dvsph` stays `Dvsph`, which is closer to right than
    /// `capitalized` (which would lower-case everything after the first
    /// letter and mangle the acronyms these slugs are full of).
    private static func titleCased(_ segment: String) -> String {
        guard let first = segment.first else { return segment }
        return first.uppercased() + segment.dropFirst()
    }

    private static func sorted(_ systems: [System]) -> [System] {
        systems.sorted {
            $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
    }
}

/// The written parts of a DMR dial string, so a picker, a talkgroup field and
/// a timeslot control can each edit their own third of it.
///
/// The dial FIELD stays the single source of truth for what will be dialled —
/// the controls do not hold a talkgroup or a slot of their own beside it,
/// because two places holding half a target each is how a UI comes to show one
/// thing and dial another. Same rule `ReflectorDialText.applying(module:to:)`
/// follows for D-Star's module picker.
public enum DmrDialText {
    /// Split `address[/talkgroup[/timeslot]]` into the three things a control
    /// each owns. Lenient: this is for editing, not for dialling, so it never
    /// refuses — `DmrDial.parse` is what decides whether the result is a
    /// target.
    public static func parts(_ raw: String) -> (address: String, talkgroup: String, timeslot: UInt8)
    {
        let text = raw.trimmingCharacters(in: .whitespaces)
        let pieces = text.split(separator: "/", omittingEmptySubsequences: false)
        let address = pieces.first.map(String.init) ?? ""
        let talkgroup =
            pieces.count > 1 ? String(pieces[1].filter { $0.isASCII && $0.isNumber }) : ""
        let slot = pieces.count > 2 ? UInt8(pieces[2]) : nil
        return (
            address: address, talkgroup: talkgroup,
            timeslot: (slot == 1 || slot == 2) ? slot! : DmrDial.defaultTimeslot
        )
    }

    /// Rewrite a dial string from its three parts.
    ///
    /// An empty talkgroup leaves the address alone rather than appending a
    /// separator to nothing: a master with no room named yet is an unfinished
    /// form, and Connect is already off for it. An empty address keeps the
    /// room, so a talkgroup typed before a network is chosen is not thrown
    /// away when the picker fills the rest in.
    public static func compose(address: String, talkgroup: String, timeslot: UInt8) -> String {
        let address = address.trimmingCharacters(in: .whitespaces)
        let talkgroup = String(talkgroup.filter { $0.isASCII && $0.isNumber })
        let slot = (timeslot == 1 || timeslot == 2) ? timeslot : DmrDial.defaultTimeslot
        guard !talkgroup.isEmpty else { return address }
        return "\(address)/\(talkgroup)/\(slot)"
    }
}
