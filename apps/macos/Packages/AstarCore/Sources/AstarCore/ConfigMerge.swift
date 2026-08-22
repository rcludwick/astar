// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Merging an imported archive into what is already on this Mac (astar-b52e).
///
/// **Import merges; it never deletes.** Someone importing a friend's rig onto a
/// working Mac must not lose their own configs, and someone re-importing their
/// own backup must not end up with everything twice. Every operation here is
/// therefore an upsert, and every one of them is idempotent.
public enum ConfigMerge {

    /// Outcome of merging one collection.
    public struct Result<T: Equatable>: Equatable {
        public var merged: [T]
        public var added: Int
        public var updated: Int
    }

    /// Upsert by `id`, preserving the existing order.
    ///
    /// Position is part of the contract: saved configs are user-ordered
    /// (`SetupStore.move`), so an import must not shuffle the list. Matches are
    /// replaced where they sit; newcomers append.
    public static func byID<T: Identifiable & Equatable>(incoming: [T], into existing: [T])
        -> Result<T> where T.ID: Hashable
    {
        var merged = existing
        var index = [T.ID: Int]()
        for (i, item) in merged.enumerated() { index[item.id] = i }

        var added = 0
        var updated = 0
        for item in incoming {
            if let i = index[item.id] {
                // An identical entry is not a change — saying "1 updated" for a
                // re-import of the same file would be a lie the user can see.
                if merged[i] != item {
                    merged[i] = item
                    updated += 1
                }
            } else {
                index[item.id] = merged.count
                merged.append(item)
                added += 1
            }
        }
        return Result(merged: merged, added: added, updated: updated)
    }

    /// Upsert the node directory, matching on **node number, not id**.
    ///
    /// `recordRecent` upserts by node and `directorySetNode` refuses two entries
    /// pointing at one node, so matching on id here would manufacture exactly
    /// the duplicate those rules exist to prevent.
    ///
    /// On a match the LOCAL id survives — favorites and recents elsewhere
    /// reference it, and rewriting it from the archive would orphan them — and
    /// a local `favorite` is never cleared by an import: losing a curated
    /// favorite silently is worse than keeping one the archive did not set.
    public static func directory(incoming: [NodeEntry], into existing: [NodeEntry])
        -> Result<NodeEntry>
    {
        var merged = existing
        var byNode = [String: Int]()
        for (i, entry) in merged.enumerated() { byNode[entry.node] = i }
        var usedIDs = Set(merged.map(\.id))

        var added = 0
        var updated = 0
        for var entry in incoming {
            if let i = byNode[entry.node] {
                let local = merged[i]
                entry.id = local.id
                entry.favorite = entry.favorite || local.favorite
                if merged[i] != entry {
                    merged[i] = entry
                    updated += 1
                }
            } else {
                // A colliding id on a DIFFERENT node would give two entries one
                // identity; rebase rather than refuse the entry.
                if usedIDs.contains(entry.id) { entry.id = UUID().uuidString }
                usedIDs.insert(entry.id)
                byNode[entry.node] = merged.count
                merged.append(entry)
                added += 1
            }
        }
        return Result(merged: merged, added: added, updated: updated)
    }

    /// What an import did, for the confirmation the user reads afterwards.
    public struct Summary: Equatable {
        public var setupsAdded = 0
        public var setupsUpdated = 0
        public var micProfilesAdded = 0
        public var micProfilesUpdated = 0
        public var directoryAdded = 0
        public var directoryUpdated = 0
        public var settingsApplied = 0
        public var interfaceApplied = 0
        public var callsignApplied = false

        public init(
            setupsAdded: Int = 0, setupsUpdated: Int = 0,
            micProfilesAdded: Int = 0, micProfilesUpdated: Int = 0,
            directoryAdded: Int = 0, directoryUpdated: Int = 0,
            settingsApplied: Int = 0, interfaceApplied: Int = 0,
            callsignApplied: Bool = false
        ) {
            self.setupsAdded = setupsAdded
            self.setupsUpdated = setupsUpdated
            self.micProfilesAdded = micProfilesAdded
            self.micProfilesUpdated = micProfilesUpdated
            self.directoryAdded = directoryAdded
            self.directoryUpdated = directoryUpdated
            self.settingsApplied = settingsApplied
            self.interfaceApplied = interfaceApplied
            self.callsignApplied = callsignApplied
        }

        public var isEmpty: Bool { self == Summary() }

        /// Human-readable, and deliberately silent about sections that did
        /// nothing — "0 configs, 0 profiles, 0 nodes" reads as a failure.
        public var description: String {
            var parts = [String]()
            func phrase(_ n: Int, _ singular: String, _ plural: String, _ verb: String) {
                guard n > 0 else { return }
                parts.append("\(n) \(n == 1 ? singular : plural) \(verb)")
            }
            phrase(setupsAdded, "config", "configs", "added")
            phrase(setupsUpdated, "config", "configs", "updated")
            phrase(micProfilesAdded, "mic profile", "mic profiles", "added")
            phrase(micProfilesUpdated, "mic profile", "mic profiles", "updated")
            phrase(directoryAdded, "node", "nodes", "added")
            phrase(directoryUpdated, "node", "nodes", "updated")
            phrase(settingsApplied, "setting", "settings", "applied")
            phrase(interfaceApplied, "window setting", "window settings", "applied")
            if callsignApplied { parts.append("callsign set") }
            guard !parts.isEmpty else { return "Nothing to change — already up to date." }
            return parts.joined(separator: ", ") + "."
        }
    }

    /// Count what applying `archive` would change, without applying it.
    public static func summarize(
        _ archive: ConfigArchive,
        existingSetups: [Setup],
        existingProfiles: [MicProfile],
        existingDirectory: [NodeEntry]
    ) -> Summary {
        var summary = Summary()
        if let rigs = archive.rigs {
            let s = byID(incoming: rigs.setups, into: existingSetups)
            summary.setupsAdded = s.added
            summary.setupsUpdated = s.updated
            let m = byID(incoming: rigs.micProfiles, into: existingProfiles)
            summary.micProfilesAdded = m.added
            summary.micProfilesUpdated = m.updated
        }
        if let incoming = archive.directory {
            let d = directory(incoming: incoming, into: existingDirectory)
            summary.directoryAdded = d.added
            summary.directoryUpdated = d.updated
        }
        summary.settingsApplied = archive.settings?.count ?? 0
        summary.interfaceApplied = archive.interface?.count ?? 0
        summary.callsignApplied = archive.callsign?.isEmpty == false
        return summary
    }
}
