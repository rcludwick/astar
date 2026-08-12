// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class LevelHistoryTests: XCTestCase {
    func testAppendStoresSamplesInOrder() {
        var history = LevelHistory(capacity: 4)

        history.append(tx: -10, rx: -20)
        history.append(tx: -12, rx: -22)

        XCTAssertEqual(
            history.samples,
            [
                LevelSample(tx: -10, rx: -20),
                LevelSample(tx: -12, rx: -22),
            ])
    }

    func testCapacityCapsBufferDroppingOldest() {
        var history = LevelHistory(capacity: 3)

        history.append(tx: -1, rx: -1)
        history.append(tx: -2, rx: -2)
        history.append(tx: -3, rx: -3)
        history.append(tx: -4, rx: -4)

        XCTAssertEqual(history.count, 3)
        XCTAssertEqual(
            history.samples,
            [
                LevelSample(tx: -2, rx: -2),
                LevelSample(tx: -3, rx: -3),
                LevelSample(tx: -4, rx: -4),
            ], "the oldest sample should be dropped when over capacity")
    }

    func testStartsEmpty() {
        let history = LevelHistory(capacity: 8)
        XCTAssertTrue(history.samples.isEmpty)
        XCTAssertEqual(history.count, 0)
    }

    func testTxAndRxSeriesReadInOrder() {
        var history = LevelHistory(capacity: 4)
        history.append(tx: -5, rx: -50)
        history.append(tx: -6, rx: -40)
        history.append(tx: -7, rx: -30)

        XCTAssertEqual(history.txSeries, [-5, -6, -7])
        XCTAssertEqual(history.rxSeries, [-50, -40, -30])
    }

    func testCapacityIsAtLeastOne() {
        var history = LevelHistory(capacity: 0)
        history.append(tx: -1, rx: -2)
        XCTAssertEqual(history.count, 1)
        XCTAssertEqual(history.samples, [LevelSample(tx: -1, rx: -2)])
    }
}
