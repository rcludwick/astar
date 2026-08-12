// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class MicProfileStoreTests: XCTestCase {
    private func store() -> UserDefaultsMicProfileStore {
        UserDefaultsMicProfileStore(
            UserDefaults(
                suiteName: "astar.tests.mic.\(ProcessInfo.processInfo.globallyUniqueString)")!)
    }

    private func profile(_ id: String, _ name: String) -> MicProfile {
        MicProfile(
            id: id, name: name, deviceName: "Icom",
            characterizationJSON: "{\"floorDb\":-50}")
    }

    func testNotchFrequenciesParsesEngineJSON() {
        let p = MicProfile(
            id: "x", name: "fake icom", deviceName: "Icom",
            characterizationJSON:
                "{\"notches\":[{\"freq_hz\":588.0,\"q\":20},{\"freq_hz\":1176.0,\"q\":20}],\"noise_floor_dbfs\":-52}"
        )
        XCTAssertEqual(p.notchFrequencies, [588, 1176])
    }

    func testNotchFrequenciesEmptyForNoNotches() {
        let p = MicProfile(
            id: "x", name: "clean", deviceName: "Mic",
            characterizationJSON: "{\"notches\":[],\"noise_floor_dbfs\":-60}")
        XCTAssertTrue(p.notchFrequencies.isEmpty)
        XCTAssertTrue(
            MicProfile(id: "y", name: "junk", deviceName: "M", characterizationJSON: "nonsense")
                .notchFrequencies.isEmpty)
    }

    func testNotchFrequenciesEmptyForNonObjectJSON() {
        // Valid JSON that isn't a `{…}` object (a bare array) parses but isn't a
        // dict, so the cast fails and we fall through to [].
        let p = MicProfile(
            id: "z", name: "array", deviceName: "M", characterizationJSON: "[1,2,3]")
        XCTAssertTrue(p.notchFrequencies.isEmpty)
    }

    func testNotchFrequenciesSkipsEntriesMissingFreq() {
        // Notch entries without a numeric `freq_hz` are dropped (compactMap).
        let p = MicProfile(
            id: "z", name: "partial", deviceName: "M",
            characterizationJSON:
                "{\"notches\":[{\"q\":20},{\"freq_hz\":1000.0,\"q\":20}]}")
        XCTAssertEqual(p.notchFrequencies, [1000])
    }

    func testNotchFrequenciesEmptyForEmptyString() {
        let p = MicProfile(id: "z", name: "empty", deviceName: "M", characterizationJSON: "")
        XCTAssertTrue(p.notchFrequencies.isEmpty)
    }

    func testMicProfileCodableRoundTrips() throws {
        let p = MicProfile(
            id: "p1", name: "Icom", deviceName: "UCI150",
            characterizationJSON: "{\"floorDb\":-52}",
            characterizedAt: Date(timeIntervalSince1970: 1_700_000_000))
        let data = try JSONEncoder().encode(p)
        let decoded = try JSONDecoder().decode(MicProfile.self, from: data)
        XCTAssertEqual(decoded, p)
    }

    func testMicProfileDecodesWithoutCharacterizedAt() throws {
        // Back-compat: the optional date may be absent in older saved JSON.
        let json = Data(
            #"{"id":"p1","name":"Icom","deviceName":"M","characterizationJSON":"{}"}"#.utf8)
        let decoded = try JSONDecoder().decode(MicProfile.self, from: json)
        XCTAssertNil(decoded.characterizedAt)
        XCTAssertEqual(decoded.id, "p1")
    }

    func testEmptyByDefault() {
        let s = store()
        XCTAssertTrue(s.all().isEmpty)
        XCTAssertNil(s.profile(id: "nope"))
    }

    func testSaveLookupRoundTrip() {
        let s = store()
        let p = profile("p1", "fake icom")
        s.save(p)
        XCTAssertEqual(s.profile(id: "p1"), p)
        XCTAssertEqual(s.all().count, 1)
    }

    func testUpsertByID() {
        let s = store()
        s.save(profile("p1", "fake icom"))
        s.save(profile("p1", "fake icom 2"))
        XCTAssertEqual(s.all().count, 1)
        XCTAssertEqual(s.profile(id: "p1")?.name, "fake icom 2")
    }

    func testMultipleProfiles() {
        let s = store()
        s.save(profile("p1", "fake icom"))
        s.save(profile("p2", "quiet room"))
        XCTAssertEqual(Set(s.all().map(\.id)), ["p1", "p2"])
    }

    func testRemove() {
        let s = store()
        s.save(profile("p1", "fake icom"))
        s.remove(id: "p1")
        XCTAssertNil(s.profile(id: "p1"))
        XCTAssertTrue(s.all().isEmpty)
    }

    func testIDDefaultsToUUID() {
        let a = MicProfile(name: "a", deviceName: "X", characterizationJSON: "{}")
        let b = MicProfile(name: "b", deviceName: "X", characterizationJSON: "{}")
        XCTAssertFalse(a.id.isEmpty)
        XCTAssertNotEqual(a.id, b.id)
    }
}
