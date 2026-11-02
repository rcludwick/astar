// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

// EchoLink favorites are ordinary entries in the shared node directory
// (`NodeEntry`, network `.echolink`), so the dial row's saved-nodes menu lists
// them beside every other network's. This file is the EchoLink side of that:
// how a callsign or node number becomes one entry, and the rows the station
// browser shows.
//
// An EchoLink favorite's target is the station's CALLSIGN as the station list
// gives it (`KX7ABC-L`, `*ECHOTEST*`) whenever the list knows the station: a
// callsign is what the operator recognises and what stays put, while a node
// number can change. A target saved before the list had the station may be a
// number; it still matches that station by number.

/// The key EchoLink targets match on: trimmed and upper-cased, because
/// callsigns match in any case.
func echolinkKey(_ target: String) -> String {
    target.trimmingCharacters(in: .whitespacesAndNewlines).uppercased()
}

/// Whether `station` is named by one of `keys` (each from `echolinkKey`), by
/// callsign or by node number.
func echolinkStationIsFavorite(_ station: EchoLinkStationEntry, keys: Set<String>) -> Bool {
    keys.contains(echolinkKey(station.call)) || keys.contains(String(station.node))
}

/// One row of the EchoLink station browser: a listed station, or a favorite
/// that is not in the current list (shown greyed as "Not online").
public struct EchoLinkListRow: Identifiable, Equatable, Sendable {
    /// The callsign as listed, or the favorite's saved target otherwise.
    public let callsign: String
    /// The listed station, or `nil` when the directory does not list it now.
    public let station: EchoLinkStationEntry?
    /// Whether the star is on.
    public let isFavorite: Bool

    /// Listed rows are identified by node number, as the list always was;
    /// unlisted favorites by their key. The prefixes keep the two from ever
    /// colliding.
    public var id: String {
        if let station { return "node-\(station.node)" }
        return "favorite-\(echolinkKey(callsign))"
    }

    /// The live node number, when listed.
    public var node: UInt32? { station?.node }

    /// Whether the directory lists the station right now, so it can be
    /// chosen.
    public var isListed: Bool { station != nil }
}

/// The rows the station browser shows for `filter` and `query`.
///
/// `favorites` are the EchoLink favorites' saved targets, from the shared
/// directory. For every filter but Favorites the rows are the listed stations
/// `filterEchoLinkStations` admits, each marked with whether it is a
/// favorite. For Favorites, every favorite is a row: the listed station with
/// its live free/busy status when the list has it, and an unlisted row
/// otherwise. The search applies to both, and the order is by callsign
/// either way.
public func echolinkListRows(
    stations: [EchoLinkStationEntry], favorites: [String],
    filter: EchoLinkFilter, query: String
) -> [EchoLinkListRow] {
    let keys = Set(favorites.map(echolinkKey).filter { !$0.isEmpty })
    let listed = filterEchoLinkStations(
        stations, filter: filter, query: query, favorites: favorites)
    var rows = listed.map {
        EchoLinkListRow(
            callsign: $0.call, station: $0,
            isFavorite: echolinkStationIsFavorite($0, keys: keys))
    }
    guard filter == .favorites else { return rows }

    var listedKeys = Set<String>()
    for station in stations {
        listedKeys.insert(echolinkKey(station.call))
        listedKeys.insert(String(station.node))
    }
    let needle = query.trimmingCharacters(in: .whitespacesAndNewlines)
    var seen = Set<String>()
    for favorite in favorites {
        let key = echolinkKey(favorite)
        guard !key.isEmpty, !listedKeys.contains(key), seen.insert(key).inserted else { continue }
        let matches = needle.isEmpty || favorite.range(of: needle, options: .caseInsensitive) != nil
        guard matches else { continue }
        rows.append(
            EchoLinkListRow(
                callsign: favorite.trimmingCharacters(in: .whitespacesAndNewlines),
                station: nil, isFavorite: true))
    }
    rows.sort { a, b in
        let order = a.callsign.caseInsensitiveCompare(b.callsign)
        if order != .orderedSame { return order == .orderedAscending }
        return (a.node ?? 0) < (b.node ?? 0)
    }
    return rows
}

extension CallSession {
    /// The target an EchoLink favorite or recent is saved under: the callsign
    /// exactly as the station list gives it, when the list has a station
    /// with this node number or this callsign (in any case); otherwise the
    /// text itself, trimmed.
    ///
    /// Reads the station list, so it runs on the main thread, as every
    /// directory write does.
    public func echolinkCanonicalTarget(_ raw: String) -> String {
        let target = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !target.isEmpty else { return target }
        let key = echolinkKey(target)
        if let listed = echolinkStations.first(where: {
            String($0.node) == target || echolinkKey($0.call) == key
        }) {
            return listed.call
        }
        return target
    }

    /// The saved targets of every EchoLink favorite in the shared directory,
    /// for the station browser's stars and its Favorites filter.
    public var echolinkFavoriteTargets: [String] {
        directoryFavorites().filter { $0.network == .echolink }.map(\.node)
    }

    /// Every target an EchoLink favorite for `target` could be saved under:
    /// the listed callsign and node number when the list has the station,
    /// and the text itself either way.
    func echolinkAliases(_ target: String) -> [String] {
        let trimmed = target.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return [] }
        let key = echolinkKey(trimmed)
        var aliases = [trimmed]
        if let listed = echolinkStations.first(where: {
            String($0.node) == trimmed || echolinkKey($0.call) == key
        }) {
            aliases.append(listed.call)
            aliases.append(String(listed.node))
        }
        return aliases
    }

    /// Whether the EchoLink station `target` (a callsign or node number) is a
    /// favorite — the same entry whichever of the two names it, and whichever
    /// of the two it was saved under.
    public func isEchoLinkFavorite(_ target: String) -> Bool {
        echolinkAliases(target).contains { isFavorite(node: $0, network: .echolink) }
    }

    /// The station on the EchoLink call in hand, as a favorite would save it,
    /// or `nil` with no EchoLink call: the listed callsign for the dialled
    /// target when the list has it, and failing that the far station's
    /// callsign as the call reports it, or the dialled text.
    public var echolinkCallStation: String? {
        guard activeCallNetwork == .echolink, let dialed = dialedNode else { return nil }
        let target = dialed.trimmingCharacters(in: .whitespacesAndNewlines)
        let canonical = echolinkCanonicalTarget(target)
        if canonical != target || UInt32(target) == nil {
            return canonical.isEmpty ? nil : canonical
        }
        if let peer = echolinkPeer?.trimmingCharacters(in: .whitespacesAndNewlines),
            !peer.isEmpty
        {
            return peer
        }
        return target
    }
}
