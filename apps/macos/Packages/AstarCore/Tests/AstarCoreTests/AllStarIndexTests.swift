// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class AllStarIndexTests: XCTestCase {
    private func sampleIndex() throws -> AllStarIndex {
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: "allstar-sample", withExtension: "json", subdirectory: "Fixtures"))
        return AllStarIndex(try AllStarFeed.decode(try Data(contentsOf: url)).nodes)
    }

    func testLookupTrimsAndStripsLeadingZeros() throws {
        XCTAssertEqual(try sampleIndex().node(" 02002 ")?.callsign, "WB6NIL")
        XCTAssertNil(try sampleIndex().node("999999"))
    }

    func testExactNodeNumberFirst() throws {
        XCTAssertEqual(try sampleIndex().search("2002").nodes.first?.node, "2002")
    }

    func testCallsignSearchInNodeOrder() throws {
        let r = try sampleIndex().search("wb6nil")
        XCTAssertEqual(r.nodes.map(\.node), ["2000", "2001", "2002"])
    }

    func testDescriptionAndLocationSearch() throws {
        let idx = try sampleIndex()
        XCTAssertEqual(idx.search("parrot").nodes.map(\.node), ["2002"])
        XCTAssertEqual(idx.search("la mesa").nodes.map(\.node), ["2014", "2017", "2018", "2019"])
    }

    func testPrefixThenTextOrdering() {
        let idx = AllStarIndex([
            AllStarNode(node: "9", callsign: "K1ABC", description: "link 20", location: nil),
            AllStarNode(node: "200", callsign: "K2ABC"),
            AllStarNode(node: "20", callsign: "K3ABC"),
            AllStarNode(node: "2", callsign: "K4ABC"),
        ])
        XCTAssertEqual(idx.search("20").nodes.map(\.node), ["20", "200", "9"])
    }

    func testResultLimit() {
        let nodes = (2000...2999).map { AllStarNode(node: String($0), callsign: "N0CALL") }
        let r = AllStarIndex(nodes).search("2")
        XCTAssertEqual(r.nodes.count, 200)
        XCTAssertEqual(r.totalMatches, 1000)
        XCTAssertEqual(r.nodes.first?.node, "2000")
    }

    func testEmptyQueryListsFirstPage() {
        let nodes = (1...500).map { AllStarNode(node: String($0), callsign: "N0CALL") }
        let idx = AllStarIndex(nodes)
        let r = idx.search("  ")
        XCTAssertEqual(r.nodes.count, 200)
        XCTAssertEqual(r.totalMatches, idx.count)
        XCTAssertEqual(r.nodes.first?.node, "1")
    }
}
