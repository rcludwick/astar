// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import XCTest

@testable import AstarCore

final class TalkTimerTests: XCTestCase {
    // MARK: - phase(elapsed:limit:amberWindow:)

    func testGreenWellBelowAmberWindow() {
        // 30 s into a 120 s limit, amber window 15 s → green (90 s of slack).
        XCTAssertEqual(TalkTimer.phase(elapsed: 30, limit: 120, amberWindow: 15), .green)
        XCTAssertEqual(TalkTimer.phase(elapsed: 0, limit: 120, amberWindow: 15), .green)
    }

    func testGreenRightUpToAmberBoundary() {
        // amberStart = 120 - 15 = 105. Just under is still green.
        XCTAssertEqual(TalkTimer.phase(elapsed: 104.9, limit: 120, amberWindow: 15), .green)
    }

    func testAmberWithinTheLastWindow() {
        // At and within the amber window (last 15 s before the limit) → amber.
        XCTAssertEqual(TalkTimer.phase(elapsed: 105, limit: 120, amberWindow: 15), .amber)
        XCTAssertEqual(TalkTimer.phase(elapsed: 110, limit: 120, amberWindow: 15), .amber)
        XCTAssertEqual(TalkTimer.phase(elapsed: 119.9, limit: 120, amberWindow: 15), .amber)
    }

    func testRedAtAndPastTheLimit() {
        XCTAssertEqual(TalkTimer.phase(elapsed: 120, limit: 120, amberWindow: 15), .red)
        XCTAssertEqual(TalkTimer.phase(elapsed: 200, limit: 120, amberWindow: 15), .red)
    }

    func testDefaultAmberWindowIsFifteen() {
        XCTAssertEqual(TalkTimer.amberWindowSeconds, 15)
        // Uses the default amber window when omitted.
        XCTAssertEqual(TalkTimer.phase(elapsed: 100, limit: 120), .green)
        XCTAssertEqual(TalkTimer.phase(elapsed: 110, limit: 120), .amber)
    }

    func testNonPositiveLimitIsRed() {
        XCTAssertEqual(TalkTimer.phase(elapsed: 0, limit: 0), .red)
        XCTAssertEqual(TalkTimer.phase(elapsed: 0, limit: -5), .red)
    }

    func testAmberWindowExceedingLimitCollapsesGreen() {
        // amberWindow >= limit → amber from the start (no green band), red at limit.
        XCTAssertEqual(TalkTimer.phase(elapsed: 0, limit: 10, amberWindow: 30), .amber)
        XCTAssertEqual(TalkTimer.phase(elapsed: 9, limit: 10, amberWindow: 30), .amber)
        XCTAssertEqual(TalkTimer.phase(elapsed: 10, limit: 10, amberWindow: 30), .red)
    }

    // MARK: - resolvedLimitSeconds(entry:defaultEnabled:defaultLimitSeconds:)

    func testNilEntryUsesGlobalDefault() {
        XCTAssertEqual(
            TalkTimer.resolvedLimitSeconds(
                entry: nil, defaultEnabled: true, defaultLimitSeconds: 120),
            120)
    }

    func testNilEntryDisabledDefaultIsNil() {
        XCTAssertNil(
            TalkTimer.resolvedLimitSeconds(
                entry: nil, defaultEnabled: false, defaultLimitSeconds: 120))
    }

    func testEntryWithNoOverrideInheritsDefault() {
        let e = NodeEntry(label: "A", node: "1")  // talkTimer fields nil
        XCTAssertEqual(
            TalkTimer.resolvedLimitSeconds(
                entry: e, defaultEnabled: true, defaultLimitSeconds: 120),
            120)
    }

    func testPerNodeDurationOverrideWins() {
        let e = NodeEntry(
            label: "A", node: "1", talkTimerEnabled: true, talkTimerSeconds: 180)
        XCTAssertEqual(
            TalkTimer.resolvedLimitSeconds(
                entry: e, defaultEnabled: true, defaultLimitSeconds: 120),
            180,
            "the per-node duration overrides the global default")
    }

    func testPerNodeDurationOverrideWinsEvenIfDefaultDisabled() {
        // Node explicitly enabled overrides a globally-disabled default.
        let e = NodeEntry(
            label: "A", node: "1", talkTimerEnabled: true, talkTimerSeconds: 90)
        XCTAssertEqual(
            TalkTimer.resolvedLimitSeconds(
                entry: e, defaultEnabled: false, defaultLimitSeconds: 120),
            90)
    }

    func testPerNodeDisabledReturnsNil() {
        let e = NodeEntry(label: "A", node: "1", talkTimerEnabled: false)
        XCTAssertNil(
            TalkTimer.resolvedLimitSeconds(
                entry: e, defaultEnabled: true, defaultLimitSeconds: 120),
            "disabled for this node → no dot (nil), even though the default is on")
    }

    func testEnabledOverrideWithoutDurationInheritsDefaultDuration() {
        // enabled==true but no seconds → use the global default duration.
        let e = NodeEntry(label: "A", node: "1", talkTimerEnabled: true, talkTimerSeconds: nil)
        XCTAssertEqual(
            TalkTimer.resolvedLimitSeconds(
                entry: e, defaultEnabled: false, defaultLimitSeconds: 120),
            120)
    }

    // MARK: - NodeEntry Codable back-compat

    func testLegacyEntryWithoutTalkTimerFieldsDecodes() {
        let json = #"{"id":"a","label":"AJ7HR","node":"77777","favorite":true}"#
        let e = try! JSONDecoder().decode(NodeEntry.self, from: Data(json.utf8))
        XCTAssertNil(e.talkTimerEnabled)
        XCTAssertNil(e.talkTimerSeconds)
    }

    func testTalkTimerFieldsRoundTrip() {
        let e = NodeEntry(
            label: "A", node: "1", talkTimerEnabled: true, talkTimerSeconds: 240)
        let data = try! JSONEncoder().encode(e)
        let decoded = try! JSONDecoder().decode(NodeEntry.self, from: data)
        XCTAssertEqual(decoded.talkTimerEnabled, true)
        XCTAssertEqual(decoded.talkTimerSeconds, 240)
    }
}
