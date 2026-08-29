// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The operator's DMR radio ID on `CallSession`: sanitised on the way in and
/// on the way out, persisted under its own key, and entirely independent of
/// the callsign — the two are different credentials and must never be one
/// field wearing two hats.
final class CallSessionRadioIDTests: XCTestCase {

    private func scratchDefaults() -> (UserDefaults, String) {
        let suite = "astar.tests.radioid.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return (defaults, suite)
    }

    func testStartsEmptyAndPersistsUnderItsOwnKey() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        let session = CallSession(station: FakeStation(), userDefaults: defaults)
        XCTAssertEqual(session.dmrRadioID, "")

        session.dmrRadioID = "3153591"
        XCTAssertEqual(defaults.string(forKey: "dmr.radioId"), "3153591")
    }

    func testSurvivesRelaunch() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        CallSession(station: FakeStation(), userDefaults: defaults).dmrRadioID = "3153591"
        let reopened = CallSession(station: FakeStation(), userDefaults: defaults)
        XCTAssertEqual(reopened.dmrRadioID, "3153591")
    }

    func testNonDigitsNeverReachTheStoredValue() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        let session = CallSession(station: FakeStation(), userDefaults: defaults)
        session.dmrRadioID = "DMR 315-3591"
        XCTAssertEqual(session.dmrRadioID, "3153591")
        XCTAssertEqual(defaults.string(forKey: "dmr.radioId"), "3153591")
    }

    /// A defaults domain edited by hand (or written by a build that validated
    /// differently) is not a trusted source.
    func testAStoredValueIsSanitisedOnLoad() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        defaults.set("31x53591!!!!!!!!!", forKey: "dmr.radioId")
        let session = CallSession(station: FakeStation(), userDefaults: defaults)
        XCTAssertEqual(session.dmrRadioID, "3153591")
    }

    /// The whole point of the field: DMR's identity is not the callsign, so
    /// writing one must not disturb the other.
    func testTheRadioIDAndTheCallsignAreIndependent() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }

        let session = CallSession(station: FakeStation(), userDefaults: defaults)
        session.operatorCallsign = "AJ7HR"
        session.dmrRadioID = "3153591"

        XCTAssertEqual(session.operatorCallsign, "AJ7HR")
        XCTAssertEqual(session.dmrRadioID, "3153591")

        session.dmrRadioID = ""
        XCTAssertEqual(session.operatorCallsign, "AJ7HR")
    }
}
