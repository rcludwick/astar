// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

// astar-a9c3 F3: the accessibilityValue strings behind the gain/dB/ms
// sliders in QuickConfigView/SetupsView — unit-correct wording a blind
// operator can act on, independent of the sighted-only readout text.
final class AccessibilityValueFormatterTests: XCTestCase {
    func testPercentRoundsToNearestWholeNumber() {
        XCTAssertEqual(AccessibilityValueFormatter.percent(1.2), "120 percent")
        XCTAssertEqual(AccessibilityValueFormatter.percent(0.904), "90 percent")
        XCTAssertEqual(AccessibilityValueFormatter.percent(0), "0 percent")
    }

    func testDecibelsSpellsOutMinusRatherThanUsingTheGlyph() {
        XCTAssertEqual(AccessibilityValueFormatter.decibels(-40), "minus 40 decibels")
        XCTAssertEqual(AccessibilityValueFormatter.decibels(-0.6), "minus 1 decibel")
    }

    func testSingularUnitsAtOne() {
        XCTAssertEqual(AccessibilityValueFormatter.decibels(1), "1 decibel")
        XCTAssertEqual(AccessibilityValueFormatter.milliseconds(1), "1 millisecond")
    }

    func testDecibelsAtOrAboveZeroOmitsMinus() {
        XCTAssertEqual(AccessibilityValueFormatter.decibels(0), "0 decibels")
        XCTAssertEqual(AccessibilityValueFormatter.decibels(3.4), "3 decibels")
    }

    func testMillisecondsRoundsToNearestWholeNumber() {
        XCTAssertEqual(AccessibilityValueFormatter.milliseconds(500), "500 milliseconds")
        XCTAssertEqual(AccessibilityValueFormatter.milliseconds(149.6), "150 milliseconds")
    }
}
