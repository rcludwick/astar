// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// One AllStarLink node as hamcall-db publishes it in `allstar.json`.
public struct AllStarNode: Codable, Hashable, Sendable {
    /// The node number, a string of digits ("2002").
    public let node: String
    public let callsign: String
    public let description: String?
    public let location: String?

    public init(node: String, callsign: String, description: String? = nil, location: String? = nil)
    {
        self.node = node
        self.callsign = callsign
        self.description = description
        self.location = location
    }
}

/// A decoded `allstar.json`: the nodes plus the envelope fields the UI and
/// the sync policy need. `attribution` is shown wherever the data is.
public struct AllStarFeed: Hashable, Sendable {
    public let generated: String
    public let clientRefreshDays: Int
    public let attribution: String
    public let nodes: [AllStarNode]

    public init(
        generated: String = "", clientRefreshDays: Int = 7, attribution: String = "",
        nodes: [AllStarNode] = []
    ) {
        self.generated = generated
        self.clientRefreshDays = clientRefreshDays
        self.attribution = attribution
        self.nodes = nodes
    }

    public static let empty = AllStarFeed()

    public enum DecodeError: Error, Equatable {
        case unsupportedAPIVersion(String?)
    }

    private struct Wire: Decodable {
        let apiVersion: String?
        let generated: String?
        let clientRefreshDays: Int?
        let attribution: String?
        let nodes: [AllStarNode]
    }

    /// Decode a feed. Throws when `api_version` is not "v1" or `nodes` is
    /// missing: a payload that is not a directory must not read as an empty one.
    public static func decode(_ data: Data) throws -> AllStarFeed {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let wire = try decoder.decode(Wire.self, from: data)
        guard wire.apiVersion == "v1" else {
            throw DecodeError.unsupportedAPIVersion(wire.apiVersion)
        }
        let days = wire.clientRefreshDays.flatMap { $0 > 0 ? $0 : nil } ?? 7
        return AllStarFeed(
            generated: wire.generated ?? "", clientRefreshDays: days,
            attribution: wire.attribution ?? "", nodes: wire.nodes)
    }
}
