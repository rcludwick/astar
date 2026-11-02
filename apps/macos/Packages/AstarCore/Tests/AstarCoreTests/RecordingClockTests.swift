// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class RecordingClockTests: XCTestCase {
    func testText() {
        XCTAssertEqual(RecordingClock.text(seconds: 0), "00:00")
        XCTAssertEqual(RecordingClock.text(seconds: 42), "00:42")
        XCTAssertEqual(RecordingClock.text(seconds: 3599), "59:59")
        XCTAssertEqual(RecordingClock.text(seconds: 3600), "1:00:00")
        XCTAssertEqual(RecordingClock.text(seconds: 3723), "1:02:03")
        XCTAssertEqual(RecordingClock.text(seconds: -5), "00:00", "never a negative clock")
    }

    func testAccessibilityLabel() {
        XCTAssertEqual(RecordingClock.accessibilityLabel(recording: false, seconds: 0), "Record")
        XCTAssertEqual(
            RecordingClock.accessibilityLabel(recording: false, seconds: 42), "Record",
            "idle says what the button does, whatever the last clock read")
        XCTAssertEqual(
            RecordingClock.accessibilityLabel(recording: true, seconds: 42),
            "Recording, 42 seconds")
        XCTAssertEqual(
            RecordingClock.accessibilityLabel(recording: true, seconds: 1), "Recording, 1 second")
        XCTAssertEqual(
            RecordingClock.accessibilityLabel(recording: true, seconds: 0), "Recording, 0 seconds")
        // Past a minute VoiceOver hears minutes, not "125 seconds".
        XCTAssertEqual(
            RecordingClock.accessibilityLabel(recording: true, seconds: 125),
            "Recording, 2 minutes 5 seconds")
        XCTAssertEqual(
            RecordingClock.accessibilityLabel(recording: true, seconds: 60), "Recording, 1 minute")
        XCTAssertEqual(
            RecordingClock.accessibilityLabel(recording: true, seconds: 3723),
            "Recording, 1 hour 2 minutes 3 seconds")
    }
}
