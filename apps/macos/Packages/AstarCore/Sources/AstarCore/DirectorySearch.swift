// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

// The directory search pane lists two kinds of thing: reflectors from the
// published reflector feed, and — while EchoLink is logged in — the stations
// the EchoLink directory lists. This file is the search across both.
//
// EchoLink is deliberately NOT a `ReflectorNetwork` case. That type records
// what the reflector publisher said a row is, and the publisher has no
// EchoLink rows; EchoLink's list comes from a login, lasts only as long as
// the login does, and holds stations rather than reflectors. So the pane's
// filter has its own small type that names either.

/// One choice in the directory search's network filter.
public enum DirectorySearchNetwork: Hashable, Sendable {
    /// One network from the reflector feed.
    case reflector(ReflectorNetwork)
    /// The EchoLink directory's stations. Offered only while logged in.
    case echolink
    /// The AllStarLink node directory. Offered once a node list is loaded.
    case allstar

    /// The filter menu's label, and the label on each result row.
    public var displayName: String {
        switch self {
        case .reflector(let network): return network.displayName
        case .echolink: return Network.echolink.displayName
        case .allstar: return Network.allstar.displayName
        }
    }

    /// A stable identity for `ForEach`.
    public var id: String {
        switch self {
        case .reflector(let network): return "reflector:\(network.rawValue)"
        case .echolink: return "echolink"
        case .allstar: return "allstar"
        }
    }

    /// The app network that dials a result from this filter, or `nil` when
    /// astar has no network for it (P25, URF, a network this build has never
    /// heard of).
    public var appNetwork: Network? {
        switch self {
        case .reflector(let network): return Network.matching(network)
        case .echolink: return .echolink
        case .allstar: return .allstar
        }
    }

    /// The filter the pane opens on when the dial is set to `network`: that
    /// network's reflectors, AllStar's nodes on AllStar, or `nil`
    /// (everything) when it has none. On EchoLink the dial row's magnifying
    /// glass opens EchoLink's own station browser, so there is no EchoLink
    /// case here.
    public static func preferred(for network: Network) -> DirectorySearchNetwork? {
        if network == .allstar { return .allstar }
        return network.reflectorNetwork.map { .reflector($0) }
    }

    /// The filter choices: the networks the reflector feed actually contains,
    /// in `ReflectorNetwork.known` order with unknown ones after, then
    /// EchoLink while logged in, then AllStar once its node list is loaded.
    ///
    /// Built from the data so the filter never offers a network with nothing
    /// behind it, and never hides one the feed added after this build shipped.
    public static func available(
        reflectors: [DirectoryEntry], echolinkLoggedIn: Bool, allstarAvailable: Bool = false
    ) -> [DirectorySearchNetwork] {
        let present = Set(reflectors.map(\.network))
        let known = ReflectorNetwork.known.filter(present.contains)
        let unknown = present.subtracting(known).sorted { $0.rawValue < $1.rawValue }
        var networks = (known + unknown).map { DirectorySearchNetwork.reflector($0) }
        if echolinkLoggedIn { networks.append(.echolink) }
        if allstarAvailable { networks.append(.allstar) }
        return networks
    }

    /// The filter to show for the operator's chosen `filter`: the choice
    /// itself while `available` offers it, and `nil` ("All networks") while
    /// it does not — EchoLink during a log-out, say. The choice is not
    /// forgotten, so it shows again once it is offered again.
    public static func keeping(
        _ filter: DirectorySearchNetwork?, available: [DirectorySearchNetwork]
    ) -> DirectorySearchNetwork? {
        guard let filter, available.contains(filter) else { return nil }
        return filter
    }
}

/// One row of the directory search: a reflector, or an EchoLink station.
public enum DirectorySearchResult: Identifiable, Equatable, Sendable {
    case reflector(DirectoryEntry)
    case echolink(EchoLinkStationEntry)
    case allstar(AllStarNode)

    /// Unique across both kinds: reflectors by their feed key, stations by
    /// node number.
    public var id: String {
        switch self {
        case .reflector(let entry): return entry.key
        case .echolink(let station): return "echolink:\(station.node)"
        case .allstar(let node): return "allstar:\(node.node)"
        }
    }

    /// The network the row belongs to, as the filter names it.
    public var network: DirectorySearchNetwork {
        switch self {
        case .reflector(let entry): return .reflector(entry.network)
        case .echolink: return .echolink
        case .allstar: return .allstar
        }
    }
}

/// The text that puts `station` in the dial field: its callsign, the thing a
/// favorite is saved under and the operator recognises, or its node number
/// when the callsign holds a character the dial field cannot.
public func echolinkDialText(for station: EchoLinkStationEntry) -> String {
    let call = station.call
    return !call.isEmpty && call.allSatisfy(Network.echolink.admitsDialCharacter)
        ? call : String(station.node)
}

/// The EchoLink station list made ready to search: sorted by callsign once,
/// with each station's callsign, location and node number already in the
/// form a query is compared against.
///
/// Build it when the station list changes, not per keystroke. The directory
/// lists thousands of stations, and sorting and lower-casing them all on
/// every key is the slow part of a search; with this built, a keystroke only
/// filters.
public struct EchoLinkSearchIndex: Sendable {
    fileprivate struct Row: Sendable {
        let station: EchoLinkStationEntry
        let call: String
        let location: String
        let node: String
    }

    fileprivate let rows: [Row]

    /// The number of stations indexed.
    public var count: Int { rows.count }

    public init(_ stations: [EchoLinkStationEntry]) {
        rows =
            stations
            .sorted { a, b in
                let order = a.call.caseInsensitiveCompare(b.call)
                return order == .orderedSame ? a.node < b.node : order == .orderedAscending
            }
            .map {
                Row(
                    station: $0, call: $0.call.lowercased(),
                    location: $0.description.lowercased(), node: String($0.node))
            }
    }

    /// The stations matching `query`, in callsign order: a station matches
    /// when its callsign or its location text contains the query in any
    /// case, or when its node number starts with it. An empty query matches
    /// every station.
    public func stations(matching query: String) -> [EchoLinkStationEntry] {
        let needle = query.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if needle.isEmpty { return rows.map(\.station) }
        return rows.compactMap { row in
            row.call.contains(needle) || row.node.hasPrefix(needle)
                || row.location.contains(needle)
                ? row.station : nil
        }
    }
}

extension ReflectorDirectory {
    /// Search the reflector feed, the EchoLink station list while logged in,
    /// and the AllStar node list once loaded.
    ///
    /// `echolink` is `nil` while EchoLink is not logged in, and `allstar` is
    /// `nil` while no node list is loaded; then those rows never appear
    /// whatever the filter says. Reflectors come first in publisher order
    /// (see `search(_:network:)`), then EchoLink stations by callsign, then
    /// AllStar nodes (at most `AllStarIndex.resultLimit`). An empty query
    /// lists everything the filter admits, except that "all networks" leaves
    /// AllStar out until there is something to search for: tens of thousands
    /// of nodes would bury the reflectors.
    public func search(
        _ query: String, filter: DirectorySearchNetwork?, echolink: EchoLinkSearchIndex?,
        allstar: AllStarIndex? = nil
    ) -> [DirectorySearchResult] {
        searchCountingAllStar(query, filter: filter, echolink: echolink, allstar: allstar).results
    }

    /// `search(_:filter:echolink:allstar:)`, plus how many AllStar nodes the
    /// query matched before the `AllStarIndex.resultLimit` cap (0 when no
    /// AllStar rows were searched), from the same single AllStar search.
    public func searchCountingAllStar(
        _ query: String, filter: DirectorySearchNetwork?, echolink: EchoLinkSearchIndex?,
        allstar: AllStarIndex? = nil
    ) -> (results: [DirectorySearchResult], allstarTotalMatches: Int) {
        var allstarTotal = 0
        var results: [DirectorySearchResult] = []
        switch filter {
        case .reflector(let network):
            results = search(query, network: network).map { .reflector($0) }
        case nil:
            results = search(query, network: nil).map { .reflector($0) }
        case .echolink, .allstar:
            break
        }
        if filter == nil || filter == .echolink, let echolink {
            results.append(contentsOf: echolink.stations(matching: query).map { .echolink($0) })
        }
        let hasQuery = !query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        if filter == .allstar || (filter == nil && hasQuery), let allstar {
            let found = allstar.search(query)
            allstarTotal = found.totalMatches
            results.append(contentsOf: found.nodes.map { .allstar($0) })
        }
        return (results, allstarTotal)
    }
}
