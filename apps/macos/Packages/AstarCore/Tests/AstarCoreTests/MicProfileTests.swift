// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The read-only views `MicProfile` offers over the engine's opaque
/// characterization JSON. The JSON itself is stored and replayed verbatim; these
/// are display-only readings of it, so an unparseable or older profile must
/// degrade quietly rather than throw.
final class MicProfileTests: XCTestCase {
    private func profile(_ json: String) -> MicProfile {
        MicProfile(id: "p1", name: "fake icom", deviceName: "Jabra", characterizationJSON: json)
    }

    /// An empty notch list is the pass-through profile: the characterizer found
    /// nothing standing far enough above the floor, so the mic is left untouched.
    func testEmptyNotchListIsPassThrough() {
        XCTAssertTrue(profile("{\"notches\":[]}").isPassThrough)
        XCTAssertEqual(profile("{\"notches\":[]}").notchFrequencies, [])
    }

    func testOneNotchIsNotPassThrough() {
        let p = profile("{\"notches\":[{\"freq_hz\":120.0,\"q\":30.0}]}")
        XCTAssertFalse(p.isPassThrough)
        XCTAssertEqual(p.notchFrequencies, [120.0])
    }

    /// Nothing to read is still "filters nothing" — an uncharacterized or
    /// unparseable profile reads as pass-through rather than crashing a view.
    func testUnparseableProfileIsPassThrough() {
        XCTAssertTrue(profile("").isPassThrough)
        XCTAssertTrue(profile("not json").isPassThrough)
    }

    func testPeakMarginParsesFromTheEngineJSON() {
        XCTAssertEqual(
            profile("{\"notches\":[],\"peak_margin_db\":18.5}").peakMarginDb, 18.5)
        XCTAssertEqual(
            profile("{\"notches\":[],\"peak_margin_db\":12}").peakMarginDb, 12)
    }

    /// A profile characterized before the margin existed (or one that will not
    /// parse) has no margin to show — `nil`, not a made-up default.
    func testPeakMarginIsNilWhenAbsentOrUnparseable() {
        XCTAssertNil(profile("{\"notches\":[]}").peakMarginDb)
        XCTAssertNil(profile("not json").peakMarginDb)
        XCTAssertNil(profile("").peakMarginDb)
    }

    func testAbsoluteThresholdParsesFromTheEngineJSON() {
        XCTAssertEqual(
            profile("{\"notches\":[],\"threshold_dbfs\":-60.5}").thresholdDbfs, -60.5)
        XCTAssertEqual(
            profile("{\"notches\":[],\"threshold_dbfs\":-60}").thresholdDbfs, -60)
    }

    /// A profile the relative margin decided (the engine writes `null` there),
    /// one characterized before the threshold existed, or one that will not
    /// parse — all have no absolute threshold to show.
    func testAbsoluteThresholdIsNilWhenAbsentNullOrUnparseable() {
        XCTAssertNil(profile("{\"notches\":[],\"threshold_dbfs\":null}").thresholdDbfs)
        XCTAssertNil(profile("{\"notches\":[]}").thresholdDbfs)
        XCTAssertNil(profile("not json").thresholdDbfs)
        XCTAssertNil(profile("").thresholdDbfs)
    }
}
