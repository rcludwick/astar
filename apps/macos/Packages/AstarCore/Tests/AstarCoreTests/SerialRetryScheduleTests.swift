// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class SerialRetryScheduleTests: XCTestCase {
    func testBackoffDoublesFromTwoSecondsAndCapsAtThirty() {
        XCTAssertEqual(SerialRetrySchedule.delay(attempt: 0), 2)
        XCTAssertEqual(SerialRetrySchedule.delay(attempt: 1), 4)
        XCTAssertEqual(SerialRetrySchedule.delay(attempt: 2), 8)
        XCTAssertEqual(SerialRetrySchedule.delay(attempt: 3), 16)
        XCTAssertEqual(SerialRetrySchedule.delay(attempt: 4), 30)
        XCTAssertEqual(SerialRetrySchedule.delay(attempt: 5), 30)
        XCTAssertEqual(SerialRetrySchedule.delay(attempt: 100), 30, "capped forever, never stops")
    }
}
