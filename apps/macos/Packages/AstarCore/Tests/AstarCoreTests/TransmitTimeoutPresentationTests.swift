// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The Settings → Transmit slider's wording and arithmetic. The time-out can
/// never be turned off, so the one guard here is that no slider position, and
/// no value VoiceOver's adjust action can reach, turns into 0 seconds.
final class TransmitTimeoutPresentationTests: XCTestCase {
    func testSectionLabelAndCaptionAreExact() {
        XCTAssertEqual(TransmitTimeoutPresentation.sectionTitle, "Transmit")
        XCTAssertEqual(TransmitTimeoutPresentation.label, "Transmit time-out")
        XCTAssertEqual(
            TransmitTimeoutPresentation.caption,
            "Stops transmitting after this long on air, whatever is keying: on-screen button, Space, serial, Bluetooth or VOX. Release, or stop talking, to transmit again."
        )
    }

    func testSliderRunsFromOneToTenMinutesWithNoOffPosition() {
        XCTAssertEqual(TransmitTimeoutPresentation.sliderRange, 1.0...10.0)
    }

    func testSliderPositionsBecomeWholeMinutesOfSeconds() {
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 1), 60)
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 2), 120)
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 10), 600)
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 2.4), 120)
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 2.6), 180)
    }

    func testSliderCanNeverProduceZero() {
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 0), 60)
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 0.4), 60)
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: -3), 60)
    }

    func testSliderClampsAboveTenMinutes() {
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: 11), 600)
    }

    func testNonFiniteSliderValueFallsBackToTheDefault() {
        XCTAssertEqual(TransmitTimeoutPresentation.seconds(fromMinutes: .nan), 120)
    }

    func testStoredSecondsBecomeSliderMinutes() {
        XCTAssertEqual(TransmitTimeoutPresentation.minutes(fromSeconds: 60), 1)
        XCTAssertEqual(TransmitTimeoutPresentation.minutes(fromSeconds: 120), 2)
        XCTAssertEqual(TransmitTimeoutPresentation.minutes(fromSeconds: 600), 10)
        XCTAssertEqual(TransmitTimeoutPresentation.minutes(fromSeconds: 0), 1)
    }

    func testValueTextIsAbbreviatedMinutes() {
        XCTAssertEqual(TransmitTimeoutPresentation.valueText(seconds: 60), "1 min")
        XCTAssertEqual(TransmitTimeoutPresentation.valueText(seconds: 120), "2 min")
        XCTAssertEqual(TransmitTimeoutPresentation.valueText(seconds: 600), "10 min")
    }

    func testAccessibilityValueSpellsMinutesOut() {
        XCTAssertEqual(TransmitTimeoutPresentation.accessibilityValue(seconds: 60), "1 minute")
        XCTAssertEqual(TransmitTimeoutPresentation.accessibilityValue(seconds: 120), "2 minutes")
        XCTAssertEqual(TransmitTimeoutPresentation.accessibilityValue(seconds: 600), "10 minutes")
    }
}
