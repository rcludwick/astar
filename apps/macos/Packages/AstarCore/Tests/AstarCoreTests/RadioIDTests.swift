// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class RadioIDTests: XCTestCase {
    func testKeepsDigitsAndDropsEverythingElse() {
        XCTAssertEqual(RadioID.sanitized("3153591"), "3153591")
        XCTAssertEqual(RadioID.sanitized(" 315 3591 "), "3153591")
        XCTAssertEqual(RadioID.sanitized("DMR 3153591"), "3153591")
        XCTAssertEqual(RadioID.sanitized("31-53-591"), "3153591")
    }

    func testRefusesNonASCIIDigits() {
        // `Character.isNumber` would accept all of these; a radio ID is 0–9.
        XCTAssertEqual(RadioID.sanitized("٣١٥"), "")
        XCTAssertEqual(RadioID.sanitized("Ⅶ"), "")
        XCTAssertEqual(RadioID.sanitized("³"), "")
    }

    func testTruncatesPastTheLongestRealID() {
        // A 7-digit ID plus the two-digit multi-device suffix is the longest
        // thing anyone actually has; anything past that is a paste accident.
        XCTAssertEqual(RadioID.sanitized("315359102"), "315359102")
        XCTAssertEqual(RadioID.sanitized("3153591029999"), "315359102")
        XCTAssertEqual(RadioID.sanitized("3153591029999").count, RadioID.maxDigits)
    }

    func testPlausibilityIsALengthHintNotAValidator() {
        XCTAssertFalse(RadioID.isPlausible(""))
        XCTAssertFalse(RadioID.isPlausible("315"))
        XCTAssertTrue(RadioID.isPlausible("310000"))  // 6 digits — a repeater ID
        XCTAssertTrue(RadioID.isPlausible("3153591"))
        XCTAssertTrue(RadioID.isPlausible("315359102"))
        // Letters do not make a short number long.
        XCTAssertFalse(RadioID.isPlausible("AJ7HR"))
    }
}
