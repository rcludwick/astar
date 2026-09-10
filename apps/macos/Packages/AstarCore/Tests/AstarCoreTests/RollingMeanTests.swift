// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class RollingMeanTests: XCTestCase {
    func testEmptyIsNilThenFillsThenSlides() {
        var m = RollingMean(window: 3)
        XCTAssertNil(m.value)
        m.push(1)
        m.push(2)
        m.push(3)
        XCTAssertEqual(m.count, 3)
        XCTAssertEqual(m.value!, 2, accuracy: 1e-6)
        m.push(6)  // window slides: 2, 3, 6
        XCTAssertEqual(m.count, 3)
        XCTAssertEqual(m.value!, 11.0 / 3.0, accuracy: 1e-5)
        m.push(9)  // 3, 6, 9
        XCTAssertEqual(m.value!, 6, accuracy: 1e-5)
    }

    func testNonFiniteSamplesAreDroppedAndResetForgets() {
        var m = RollingMean(window: 4)
        m.push(-60)
        m.push(.nan)
        m.push(.infinity)
        XCTAssertEqual(m.count, 1)
        XCTAssertEqual(m.value!, -60, accuracy: 1e-6)
        m.reset()
        XCTAssertNil(m.value)
        XCTAssertEqual(m.count, 0)
    }

    func testAWindowOfZeroIsClampedToOne() {
        var m = RollingMean(window: 0)
        m.push(5)
        m.push(7)
        XCTAssertEqual(m.window, 1)
        XCTAssertEqual(m.value!, 7, accuracy: 1e-6)
    }
}
