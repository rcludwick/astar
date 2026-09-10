// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class MicAnalyzerSeedTests: XCTestCase {
    func testExplicitWinsThenStoredThenSystemDefault() {
        XCTAssertEqual(
            MicAnalyzerSeed.input(explicit: "USB Audio Device", stored: "KT USB Audio"),
            "USB Audio Device")
        XCTAssertEqual(
            MicAnalyzerSeed.input(explicit: nil, stored: "USB Audio Device"), "USB Audio Device")
        XCTAssertEqual(
            MicAnalyzerSeed.input(explicit: "", stored: "USB Audio Device"), "USB Audio Device")
        XCTAssertNil(MicAnalyzerSeed.input(explicit: nil, stored: nil))
        XCTAssertNil(MicAnalyzerSeed.input(explicit: nil, stored: ""))
    }
}
