// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class AllStarFeedTests: XCTestCase {
    private func sample() throws -> AllStarFeed {
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: "allstar-sample", withExtension: "json", subdirectory: "Fixtures"))
        return try AllStarFeed.decode(try Data(contentsOf: url))
    }

    func testDecodesEnvelopeAndNodes() throws {
        let feed = try sample()
        XCTAssertEqual(feed.nodes.count, 20)
        XCTAssertEqual(feed.clientRefreshDays, 7)
        XCTAssertFalse(feed.generated.isEmpty)
        XCTAssertTrue(feed.attribution.contains("AllStarLink"))
        let n = try XCTUnwrap(feed.nodes.first { $0.node == "2002" })
        XCTAssertEqual(n.callsign, "WB6NIL")
        XCTAssertEqual(n.description, "AllStarLink Parrot")
        XCTAssertEqual(n.location, "AWS US-EAST-1")
    }

    func testNullsDecodeToNil() throws {
        let n = try XCTUnwrap(try sample().nodes.first { $0.node == "27110" })
        XCTAssertNil(n.description)
        XCTAssertNil(n.location)
    }

    func testRejectsOtherAPIVersion() {
        let json = #"{"api_version":"v2","nodes":[]}"#
        XCTAssertThrowsError(try AllStarFeed.decode(Data(json.utf8)))
    }

    func testRejectsMissingNodes() {
        let json = #"{"api_version":"v1"}"#
        XCTAssertThrowsError(try AllStarFeed.decode(Data(json.utf8)))
    }
}
