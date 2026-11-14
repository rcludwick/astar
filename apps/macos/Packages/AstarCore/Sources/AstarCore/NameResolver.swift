// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A source that can map an AllStar node number to a human-readable display name
/// (a callsign or label). The directory (saved favorites/recents) is one source;
/// the AllStar directory from hamcall-db is another — each conforms to this
/// protocol and is listed in a `NameResolver`'s sources.
///
/// Platform-neutral (no AppKit) so it's iOS-ready and testable.
public protocol NameSource {
    /// The saved/known display name for `node`, or `nil` if this source doesn't
    /// know one.
    func name(forNode node: String) -> String?
}

/// Resolves a node number to a display name by consulting an **ordered** list of
/// sources, first hit wins. The saved directory comes first (a user's curated
/// label always beats a downloaded callsign), then the AllStar directory.
///
/// `name(forNode:)` returns `nil` when no source knows the node, so callers fall
/// back to showing the bare number. `displayName(forNode:)` does that fallback
/// for you.
public final class NameResolver {
    private let sources: [NameSource]

    /// - Parameter sources: consulted in order; the first non-`nil` result wins.
    ///   Put the most-authoritative source (the saved directory) first.
    public init(sources: [NameSource]) {
        self.sources = sources
    }

    /// The resolved display name for `node`, or `nil` if no source knows it.
    public func name(forNode node: String) -> String? {
        let trimmed = node.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return nil }
        for source in sources {
            if let name = source.name(forNode: trimmed)?
                .trimmingCharacters(in: .whitespaces), !name.isEmpty
            {
                return name
            }
        }
        return nil
    }

    /// The resolved display name, falling back to the bare node number when no
    /// source knows it. Convenient for UI that always needs *something* to show.
    public func displayName(forNode node: String) -> String {
        name(forNode: node) ?? node
    }
}

/// Adapts a `NodeDirectoryStore` into a `NameSource`: returns the saved entry's
/// label for a node, preferring a favorite over a plain recent when both exist
/// for the same number. Returns `nil` when the label is empty or just the number
/// itself (an unnamed recent), so resolution falls through to the next source
/// rather than echoing the bare number as a "name".
///
/// Scoped to one network (AllStar unless told otherwise): the directory holds
/// every network's entries, and an EchoLink favorite for node 1234 must not
/// name AllStar node 1234.
public struct DirectoryNameSource: NameSource {
    private let store: NodeDirectoryStore
    private let network: Network

    public init(_ store: NodeDirectoryStore, network: Network = .allstar) {
        self.store = store
        self.network = network
    }

    public func name(forNode node: String) -> String? {
        let matches = store.all().filter { $0.matches(target: node, network: network) }
        // Prefer a favorite (curated) over a bare recent.
        let entry = matches.first(where: { $0.favorite }) ?? matches.first
        guard let label = entry?.label.trimmingCharacters(in: .whitespaces),
            !label.isEmpty, label != node
        else {
            return nil
        }
        return label
    }
}

/// A lock-protected holder for the current `AllStarIndex`, so a `NameSource`
/// (which is a plain value, not main-actor bound) can read whatever the app has
/// most recently loaded or synced. The session owns one and the app swaps the
/// index in when the AllStar directory publishes a new one.
public final class AllStarIndexBox: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: AllStarIndex

    public init(_ index: AllStarIndex = .empty) { stored = index }

    public var index: AllStarIndex {
        get {
            lock.lock()
            defer { lock.unlock() }
            return stored
        }
        set {
            lock.lock()
            defer { lock.unlock() }
            stored = newValue
        }
    }
}

/// Names an AllStar node from the public node directory: the callsign, with the
/// location after a middle dot when the directory has one ("WB6NIL · AWS
/// US-EAST-1"). Sits after `DirectoryNameSource`, so a saved label wins.
public struct AllStarNameSource: NameSource {
    private let box: AllStarIndexBox

    public init(_ box: AllStarIndexBox) { self.box = box }

    public func name(forNode node: String) -> String? {
        guard let entry = box.index.node(node) else { return nil }
        if let location = entry.location { return "\(entry.callsign) \u{00B7} \(location)" }
        return entry.callsign
    }
}
