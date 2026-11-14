// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

public struct AllStarSearchResults: Sendable {
    public let nodes: [AllStarNode]
    /// How many nodes matched before the result limit cut the list.
    public let totalMatches: Int
}

/// A searchable, node-number-ordered view of the AllStar directory. The
/// lowercased haystacks are built once in `init`, not per keystroke.
public struct AllStarIndex: Sendable {
    public static let resultLimit = 200
    public static let empty = AllStarIndex([])

    private struct Row: Sendable {
        let node: AllStarNode
        let haystacks: [String]
    }

    private let rows: [Row]
    private let byNumber: [String: Int]

    public var count: Int { rows.count }

    public init(_ nodes: [AllStarNode]) {
        func key(_ n: AllStarNode) -> (Int, String) { (Int(n.node) ?? Int.max, n.node) }
        let sorted = nodes.sorted { key($0) < key($1) }
        rows = sorted.map { n in
            Row(
                node: n,
                haystacks: [n.callsign, n.description ?? "", n.location ?? ""].map {
                    $0.lowercased()
                })
        }
        var map: [String: Int] = [:]
        for (i, r) in rows.enumerated() { map[Self.normalized(r.node.node)] = i }
        byNumber = map
    }

    private static func normalized(_ number: String) -> String {
        let t = number.trimmingCharacters(in: .whitespacesAndNewlines)
        let stripped = t.drop { $0 == "0" }
        return stripped.isEmpty && !t.isEmpty ? "0" : String(stripped)
    }

    /// The node with this number; whitespace and leading zeros are ignored.
    public func node(_ number: String) -> AllStarNode? {
        byNumber[Self.normalized(number)].map { rows[$0].node }
    }

    /// Exact node number first, then node numbers starting with the query,
    /// then callsign/description/location containing it; each group in node
    /// number order. An empty query lists from the start.
    public func search(_ query: String) -> AllStarSearchResults {
        let needle = query.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if needle.isEmpty {
            return AllStarSearchResults(
                nodes: rows.prefix(Self.resultLimit).map(\.node), totalMatches: rows.count)
        }
        let exact = node(needle).map { $0.node }
        var exactHits: [AllStarNode] = []
        var prefixHits: [AllStarNode] = []
        var textHits: [AllStarNode] = []
        for row in rows {
            if row.node.node == exact, exact != nil {
                exactHits.append(row.node)
            } else if row.node.node.hasPrefix(needle) {
                prefixHits.append(row.node)
            } else if row.haystacks.contains(where: { $0.contains(needle) }) {
                textHits.append(row.node)
            }
        }
        let all = exactHits + prefixHits + textHits
        return AllStarSearchResults(
            nodes: Array(all.prefix(Self.resultLimit)), totalMatches: all.count)
    }
}
