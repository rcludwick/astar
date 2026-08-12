// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class PeakHoldTests: XCTestCase {
    func testReportsMaxOverTrailingWindow() {
        var p = PeakHold(window: 0.25)
        let t0 = Date()
        XCTAssertEqual(p.push(-40, now: t0), -40)  // first sample = itself
        XCTAssertEqual(p.push(-10, now: t0.addingTimeInterval(0.1)), -10)  // jumps to a new peak
        // 0.2s after the -10 peak (still inside the 0.25 window): held.
        XCTAssertEqual(p.push(-50, now: t0.addingTimeInterval(0.3)), -10)
        // 0.3s after the -10 peak: window has slid past it → reports the recent level.
        XCTAssertEqual(p.push(-50, now: t0.addingTimeInterval(0.4)), -50)
    }
}
