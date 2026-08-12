// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class VoxBackgroundCheckTests: XCTestCase {
    func testNoiseFloorIsLoudestSample() {
        let c = VoxBackgroundCheck(samples: [-55, -48, -60, -52])
        XCTAssertEqual(c.noiseFloorDBFS, -48)
    }

    func testEmptySamplesFallBackToSilenceFloor() {
        let c = VoxBackgroundCheck(samples: [])
        XCTAssertEqual(c.noiseFloorDBFS, -120)
    }

    func testThresholdClearsFloorByMargin() {
        let c = VoxBackgroundCheck(samples: [-50])
        // -40 is 10 dB above the -50 floor → clears a 6 dB margin.
        XCTAssertTrue(c.clears(threshold: -40, margin: 6))
        // -47 is only 3 dB above → does not clear.
        XCTAssertFalse(c.clears(threshold: -47, margin: 6))
        // Exactly at margin clears.
        XCTAssertTrue(c.clears(threshold: -44, margin: 6))
    }

    func testSuggestedThresholdIsFloorPlusMarginCappedAtZero() {
        XCTAssertEqual(VoxBackgroundCheck(samples: [-50]).suggestedThreshold(margin: 6), -44)
        // A loud floor would push the suggestion past 0 dBFS — cap it.
        XCTAssertEqual(VoxBackgroundCheck(samples: [-3]).suggestedThreshold(margin: 6), 0)
    }
}
