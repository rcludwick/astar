// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class StatusIconStateTests: XCTestCase {
    func testIdleWhenNeither() {
        XCTAssertEqual(StatusIconState(transmitting: false, receiving: false), .idle)
    }

    func testTransmitting() {
        XCTAssertEqual(StatusIconState(transmitting: true, receiving: false), .transmitting)
    }

    func testReceiving() {
        XCTAssertEqual(StatusIconState(transmitting: false, receiving: true), .receiving)
    }

    func testTransmitWinsWhenBoth() {
        // If we're keyed while audio is also coming in, show TX — it's what *we're* doing.
        XCTAssertEqual(StatusIconState(transmitting: true, receiving: true), .transmitting)
    }

    func testConnectedWhenIdleOnCall() {
        XCTAssertEqual(
            StatusIconState(transmitting: false, receiving: false, connected: true), .connected)
    }

    func testTransmitAndReceiveWinOverConnected() {
        XCTAssertEqual(
            StatusIconState(transmitting: true, receiving: false, connected: true), .transmitting)
        XCTAssertEqual(
            StatusIconState(transmitting: false, receiving: true, connected: true), .receiving)
    }

    func testIdleWhenNotConnected() {
        XCTAssertEqual(
            StatusIconState(transmitting: false, receiving: false, connected: false), .idle)
    }
}
