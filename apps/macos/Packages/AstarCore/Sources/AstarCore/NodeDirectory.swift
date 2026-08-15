// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A saved node in the user's directory: a callsign or human label mapped to an
/// AllStar node number, e.g. "AJ7HR" → "77777". `favorite` entries are curated
/// by the user; entries with a `lastUsed` are auto-tracked recents (a node the
/// user has successfully connected to). The two overlap: favoriting a recent
/// keeps its `lastUsed`, and re-dialing a favorite updates its `lastUsed`.
///
/// Platform-neutral (no AppKit) so it's iOS-ready and testable.
public struct NodeEntry: Codable, Equatable, Identifiable {
    /// Stable id (the app generates a UUID string). Used for upsert/select.
    public var id: String
    /// User-facing label — a callsign or name, e.g. "AJ7HR".
    public var label: String
    /// AllStar node number as a string, e.g. "77777".
    public var node: String
    /// User-curated favorite (shown first, sorted by label).
    public var favorite: Bool
    /// Last successful connect to this node; `nil` for a never-dialed favorite.
    /// Optional so entries stored before recents existed still decode.
    public var lastUsed: Date?
    /// Optional free-form note. Optional for back-compat.
    public var note: String?
    /// Per-node talk-timer override: enable/disable the repeater-courtesy timer
    /// for this node. `nil` = inherit the global default (see `TalkTimer`).
    /// Optional with default-nil so entries stored before the timer existed
    /// still decode (Codable back-compat).
    public var talkTimerEnabled: Bool?
    /// Per-node talk-timer duration override, in seconds. `nil` = inherit the
    /// global default. Optional for the same back-compat reason.
    public var talkTimerSeconds: Int?
    /// The network this entry dials on (astar-9b3e). Unlike the optional
    /// fields above, this isn't itself optional — a saved node always has a
    /// definite network — so back-compat for entries persisted before this
    /// field existed is handled explicitly in `init(from:)` below
    /// (`decodeIfPresent ?? .allstar`) rather than by Optional-decode.
    public var network: Network

    public init(
        id: String = UUID().uuidString, label: String, node: String,
        favorite: Bool = false, lastUsed: Date? = nil, note: String? = nil,
        talkTimerEnabled: Bool? = nil, talkTimerSeconds: Int? = nil,
        network: Network = .allstar
    ) {
        self.id = id
        self.label = label
        self.node = node
        self.favorite = favorite
        self.lastUsed = lastUsed
        self.note = note
        self.talkTimerEnabled = talkTimerEnabled
        self.talkTimerSeconds = talkTimerSeconds
        self.network = network
    }

    /// Custom decode so `network` defaults to `.allstar` when absent (entries
    /// persisted before astar-9b3e). Every other field is decoded exactly as
    /// the compiler-synthesized initializer would (required, or `decodeIfPresent`
    /// for the pre-existing Optional fields) — nothing about their decoding
    /// changes.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        label = try c.decode(String.self, forKey: .label)
        node = try c.decode(String.self, forKey: .node)
        favorite = try c.decode(Bool.self, forKey: .favorite)
        lastUsed = try c.decodeIfPresent(Date.self, forKey: .lastUsed)
        note = try c.decodeIfPresent(String.self, forKey: .note)
        talkTimerEnabled = try c.decodeIfPresent(Bool.self, forKey: .talkTimerEnabled)
        talkTimerSeconds = try c.decodeIfPresent(Int.self, forKey: .talkTimerSeconds)
        // Decode as a raw String first rather than `Network` directly: an
        // unknown-but-present value (e.g. "m17", written by a future build
        // then read after a downgrade) must fall back to `.allstar` too, not
        // throw and take the whole entry (and thus the whole directory, via
        // `UserDefaultsNodeDirectoryStore.all()`'s `try?`) down with it.
        let rawNetwork = try c.decodeIfPresent(String.self, forKey: .network)
        network = rawNetwork.flatMap(Network.init(rawValue:)) ?? .allstar
    }
}

/// Persists the node directory and derives the favorites/recents lists.
public protocol NodeDirectoryStore {
    /// Every entry, in storage order.
    func all() -> [NodeEntry]
    /// Upsert by `id` (new ids append, existing ids replace in place).
    func upsert(_ entry: NodeEntry)
    /// Remove the entry with `id` (no-op if absent).
    func remove(id: String)
    /// Auto-track a successful connect: upsert by **node** (not id), so re-dialing
    /// the same node updates its `lastUsed` rather than duplicating. An existing
    /// favorite flag and curated label are preserved. `network` stamps which
    /// network the connect went out on (astar-c2e5) — a recent recorded while
    /// on M17 must not read back as AllStar.
    func recordRecent(node: String, label: String, network: Network)
}

extension NodeDirectoryStore {
    /// Convenience for callers with no network in scope — defaults to
    /// `.allstar`, the pre-M17 (astar-c2e5) behavior.
    public func recordRecent(node: String, label: String) {
        recordRecent(node: node, label: label, network: .allstar)
    }

    /// Favorited entries, sorted by label (case-insensitive).
    public var favorites: [NodeEntry] {
        all().filter { $0.favorite }
            .sorted { $0.label.localizedCaseInsensitiveCompare($1.label) == .orderedAscending }
    }

    /// Recently-used entries (those with a `lastUsed`), newest first, capped.
    public func recents(limit: Int = 10) -> [NodeEntry] {
        all().compactMap { e in e.lastUsed.map { (e, $0) } }
            .sorted { $0.1 > $1.1 }
            .prefix(limit)
            .map { $0.0 }
    }

    /// Recently-used entries, newest first, capped at 10.
    public var recents: [NodeEntry] { recents(limit: 10) }
}

/// `UserDefaults`-backed store: the list as a JSON array under `directory.nodes`.
/// Platform-neutral / iOS-ready. Mirrors `UserDefaultsSetupStore`'s approach.
public final class UserDefaultsNodeDirectoryStore: NodeDirectoryStore {
    private enum Key {
        static let nodes = "directory.nodes"
    }

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func all() -> [NodeEntry] {
        guard let data = defaults.data(forKey: Key.nodes),
            let entries = try? JSONDecoder().decode([NodeEntry].self, from: data)
        else {
            return []
        }
        return entries
    }

    public func upsert(_ entry: NodeEntry) {
        var list = all()
        if let idx = list.firstIndex(where: { $0.id == entry.id }) {
            list[idx] = entry
        } else {
            list.append(entry)
        }
        persist(list)
    }

    public func remove(id: String) {
        persist(all().filter { $0.id != id })
    }

    public func recordRecent(node: String, label: String, network: Network) {
        var list = all()
        if let idx = list.firstIndex(where: { $0.node == node }) {
            // Existing node: bump lastUsed + the network just dialed on,
            // preserve favorite + curated label.
            list[idx].lastUsed = Date()
            list[idx].network = network
        } else {
            list.append(NodeEntry(label: label, node: node, lastUsed: Date(), network: network))
        }
        persist(list)
    }

    private func persist(_ list: [NodeEntry]) {
        if let data = try? JSONEncoder().encode(list) {
            defaults.set(data, forKey: Key.nodes)
        }
    }
}
