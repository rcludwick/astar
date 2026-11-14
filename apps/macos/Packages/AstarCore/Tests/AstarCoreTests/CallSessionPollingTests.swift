// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `isPolling` is how the main window avoids a redundant `start()`. Each
/// `start()` stops the timer, polls once and schedules a new one, so calling it
/// on every PTT edge would poll twice per edge and shift the 20 Hz phase. If
/// `isPolling` ever lied "true", the window would skip a start it needed and
/// freeze the call card. If it lied "false", the double poll would come back.
final class CallSessionPollingTests: XCTestCase {
    func testIsPollingFollowsStartAndStop() {
        let session = hermeticSession(
            FakeStation(), defaults: bluetoothScratchDefaults(cleanup: self))
        XCTAssertEqual(session.isPolling, false, "a new session is not polling")

        session.start()
        XCTAssertEqual(session.isPolling, true)

        session.stop()
        XCTAssertEqual(session.isPolling, false)

        session.start()
        session.start()
        XCTAssertEqual(session.isPolling, true, "a restart leaves it polling")
        session.stop()
        XCTAssertEqual(session.isPolling, false)
    }
}
