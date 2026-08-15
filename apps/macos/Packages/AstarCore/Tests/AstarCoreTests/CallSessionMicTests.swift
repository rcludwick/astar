// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class InMemoryMicProfileStore: MicProfileStore {
    private var map: [String: MicProfile] = [:]
    func all() -> [MicProfile] { Array(map.values) }
    func profile(id: String) -> MicProfile? { map[id] }
    func save(_ profile: MicProfile) { map[profile.id] = profile }
    func remove(id: String) { map.removeValue(forKey: id) }
}

final class CallSessionMicTests: XCTestCase {
    private func profile(_ id: String, json: String) -> MicProfile {
        MicProfile(id: id, name: "p", deviceName: "Jabra", characterizationJSON: json)
    }

    func testNullStationMicDefaults() throws {
        let s = NullStation()
        XCTAssertEqual(try s.micSpectrum(), [])
        XCTAssertEqual(try s.characterize(harmonicComb: false), "")
        XCTAssertNoThrow(try s.monitorStart(input: nil))
        XCTAssertNoThrow(try s.monitorStop())
        XCTAssertNoThrow(try s.setMicProfile(nil))
    }

    /// NullStation (no engine) degrades the in-call TX/RX FFT to empty bins, so the
    /// overlaid canvas draws nothing rather than crashing (astar-8b5b).
    func testNullStationTxRxSpectrumDefaults() throws {
        let s = NullStation()
        XCTAssertEqual(try s.txSpectrum(), [])
        XCTAssertEqual(try s.rxSpectrum(), [])
    }

    /// `txSpectrum()` / `rxSpectrum()` forward straight through to the station —
    /// the in-call overlaid FFT poll path (astar-8b5b), mirroring `micSpectrum()`.
    func testSessionTxRxSpectrumPassThrough() throws {
        let fake = FakeStation()
        fake.txSpectrumToReturn = [-80, -60, -40]
        fake.rxSpectrumToReturn = [-95, -55, -30]
        let session = CallSession(station: fake)

        XCTAssertEqual(try session.txSpectrum(), [-80, -60, -40])
        XCTAssertEqual(try session.rxSpectrum(), [-95, -55, -30])
    }

    func testSessionMonitorAndSpectrumPassThrough() throws {
        let fake = FakeStation()
        fake.spectrumToReturn = [-90, -70, -50]
        fake.characterizeJSON = "{\"floorDb\":-52}"
        let session = CallSession(station: fake)

        try session.monitorStart(input: "Jabra Link 390")
        XCTAssertEqual(fake.monitorStartCount, 1)
        XCTAssertEqual(try session.micSpectrum(), [-90, -70, -50])
        XCTAssertEqual(try session.characterize(harmonicComb: false), "{\"floorDb\":-52}")
        try session.monitorStop()
        XCTAssertEqual(fake.monitorStopCount, 1)
    }

    func testMonitorRetainOpensOnceAndReleasesOnLastHolder() throws {
        // Two front-ends (Mic Analyzer + VOX calibration) can both want the mic
        // lane open; it opens on the first retain and closes only on the last
        // release, so one closing doesn't pull the mic from the other.
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.monitorRetain(input: "Jabra")  // 0 → 1: open
        XCTAssertEqual(fake.monitorStartCount, 1)
        try session.monitorRetain(input: "Jabra")  // 1 → 2: already open
        XCTAssertEqual(fake.monitorStartCount, 1)

        try session.monitorRelease()  // 2 → 1: still held
        XCTAssertEqual(fake.monitorStopCount, 0)
        try session.monitorRelease()  // 1 → 0: close
        XCTAssertEqual(fake.monitorStopCount, 1)
    }

    func testMonitorReleaseIsClampedAtZero() throws {
        // Over-releasing must not throw the count negative (which would let the
        // next retain skip the real open).
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.monitorRelease()  // nothing held → no-op
        XCTAssertEqual(fake.monitorStopCount, 0)

        try session.monitorRetain(input: nil)
        XCTAssertEqual(fake.monitorStartCount, 1)
        try session.monitorRelease()
        XCTAssertEqual(fake.monitorStopCount, 1)
    }

    func testApplyMicProfileAppliesSelectedJSON() throws {
        let fake = FakeStation()
        let store = InMemoryMicProfileStore()
        store.save(profile("p1", json: "{\"f\":-52}"))
        let session = CallSession(station: fake, micProfileStore: store)

        session.applyMicProfile(id: "p1")

        XCTAssertEqual(fake.setMicProfileCalls, ["{\"f\":-52}"])
    }

    func testApplyMicProfileClearsForDefaultOrUnknown() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, micProfileStore: InMemoryMicProfileStore())

        session.applyMicProfile(id: nil)  // Default → unfiltered
        session.applyMicProfile(id: "missing")  // unknown id → unfiltered

        XCTAssertEqual(fake.setMicProfileCalls, [nil, nil])
    }

    func testSetSelectionPublishesPersistsAndApplies() throws {
        let fake = FakeStation()
        let store = InMemoryMicProfileStore()
        store.save(profile("p1", json: "{\"f\":-52}"))
        let audio = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: audio, micProfileStore: store)

        session.setMicProfileSelection(id: "p1")

        XCTAssertEqual(session.micProfileID, "p1", "publishes the selection")
        XCTAssertEqual(audio.settings.micProfileID, "p1", "persists it")
        XCTAssertEqual(fake.setMicProfileCalls, ["{\"f\":-52}"], "applies it")
    }

    func testDeleteSelectedProfileFallsBackToDefault() throws {
        let fake = FakeStation()
        let store = InMemoryMicProfileStore()
        store.save(profile("p1", json: "{\"f\":-52}"))
        let session = CallSession(
            station: fake, audioStore: MemoryAudioStore(), micProfileStore: store)
        session.setMicProfileSelection(id: "p1")

        session.deleteMicProfile(id: "p1")

        XCTAssertNil(store.profile(id: "p1"), "removed from the library")
        XCTAssertNil(session.micProfileID, "selection falls back to Default")
        XCTAssertEqual(fake.setMicProfileCalls.last, .some(nil), "and the engine is cleared")
    }

    func testPublishedLibraryReflectsSaveDeleteRename() throws {
        let store = InMemoryMicProfileStore()
        let session = CallSession(
            station: FakeStation(), audioStore: MemoryAudioStore(), micProfileStore: store)
        XCTAssertTrue(session.micProfiles.isEmpty)

        session.saveMicProfile(profile("p1", json: "{\"notches\":[]}"))
        XCTAssertEqual(session.micProfiles.map(\.id), ["p1"])

        session.renameMicProfile(id: "p1", to: "fake icom")
        XCTAssertEqual(session.micProfiles.first?.name, "fake icom")

        session.deleteMicProfile(id: "p1")
        XCTAssertTrue(session.micProfiles.isEmpty)
    }

    func testInitLoadsPublishedLibraryFromStore() throws {
        let store = InMemoryMicProfileStore()
        store.save(profile("p1", json: "{}"))
        let session = CallSession(station: FakeStation(), micProfileStore: store)
        XCTAssertEqual(session.micProfiles.map(\.id), ["p1"])
    }

    func testConnectAppliesSelectedProfile() throws {
        let fake = FakeStation()
        let store = InMemoryMicProfileStore()
        store.save(profile("p1", json: "{\"f\":-52}"))
        let audio = MemoryAudioStore()
        audio.settings = AudioSettings(micProfileID: "p1")
        let session = CallSession(
            station: fake, hasCredentials: true,
            audioStore: audio, micProfileStore: store)

        try session.connect(node: "55553")

        XCTAssertTrue(
            fake.setMicProfileCalls.contains("{\"f\":-52}"),
            "connect recalls the selected profile")
    }
}
