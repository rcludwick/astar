// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class PacketCaptureCopyTests: XCTestCase {
    func testTitleAndLabel() {
        XCTAssertEqual(PacketCaptureCopy.title, "Capturing")
        XCTAssertEqual(PacketCaptureCopy.accessibilityLabel, "Packet capture")
    }

    func testAccessibilityValue() {
        XCTAssertEqual(PacketCaptureCopy.accessibilityValue(capturing: true), "on")
        XCTAssertEqual(PacketCaptureCopy.accessibilityValue(capturing: false), "off")
    }

    func testHelp() {
        XCTAssertEqual(PacketCaptureCopy.help(capturing: true), "Stop capturing packets (⇧⌘R)")
        XCTAssertEqual(PacketCaptureCopy.help(capturing: false), "Capture packets (⇧⌘R)")
    }
}
