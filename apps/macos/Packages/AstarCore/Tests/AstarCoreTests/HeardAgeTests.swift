// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `HeardAge.label` — the age column of the heard list, in the words a person
/// would use out loud. Whole units only, and never "0 s".
final class HeardAgeTests: XCTestCase {

    func testAgesReadAsAPersonWouldSayThem() {
        XCTAssertEqual(HeardAge.label(ms: 0), "now")
        XCTAssertEqual(HeardAge.label(ms: 1_999), "now")
        XCTAssertEqual(HeardAge.label(ms: 2_000), "2 s")
        XCTAssertEqual(HeardAge.label(ms: 59_999), "59 s")
        XCTAssertEqual(HeardAge.label(ms: 60_000), "1 min")
        XCTAssertEqual(HeardAge.label(ms: 3_599_000), "59 min")
        XCTAssertEqual(HeardAge.label(ms: 3_600_000), "1 h")
        XCTAssertEqual(HeardAge.label(ms: 90_000_000), "25 h")
    }
}
