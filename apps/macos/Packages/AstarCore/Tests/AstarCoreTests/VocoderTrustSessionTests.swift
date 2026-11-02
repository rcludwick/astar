// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore
@testable import AstarStation

/// Vocoder trust as the session and the config transfer see it.
///
/// Every station here is a `FakeStation` and every serial is made up: nothing
/// opens a port. The trust list is per-machine hardware, like an audio
/// device's identity, so it must never ride along in an exported config, and
/// an imported one must never be able to add to it.
final class VocoderTrustSessionTests: XCTestCase {

    private func scratchDefaults() -> (UserDefaults, String) {
        let suite = "astar.tests.vocodertrust.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return (defaults, suite)
    }

    private let dstarIndex = ReflectorIndex(entries: [
        DirectoryEntry(
            network: .dstar, id: "XLX836", name: "XLX836", aliases: ["XRF836"],
            dial: .dextra(host: "45.56.69.219", port: 30001, callsign: "XRF836", modules: []))
    ])

    // MARK: - Station config

    func testTheStationConfigCarriesTheTrustList() {
        let config = CallSession.stationConfig(
            credentials: nil, audio: AudioSettings(), vocoderSerials: ["TESTSER1", "TESTSER2"])
        XCTAssertEqual(config.vocoderSerials, ["TESTSER1", "TESTSER2"])
    }

    func testTheStationConfigTrustsNothingByDefault() {
        let config = CallSession.stationConfig(credentials: nil, audio: AudioSettings())
        XCTAssertEqual(config.vocoderSerials, [])
    }

    // MARK: - Session

    func testTheSessionReadsTheStoredTrustList() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        UserDefaultsVocoderTrustStore(defaults).save([
            TrustedVocoder(serial: "TESTSER1", prodid: "AMBE3000R", model: nil, lastSeen: nil)
        ])
        let session = CallSession(station: FakeStation(), userDefaults: defaults)
        XCTAssertEqual(session.vocoderSerials, ["TESTSER1"])
    }

    func testApplyingTrustPushesItToTheStation() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: defaults)
        session.applyVocoderTrust(["TESTSER1"])
        XCTAssertEqual(session.vocoderSerials, ["TESTSER1"])
        XCTAssertEqual(fake.vocoderTrustCalls, [["TESTSER1"]])
    }

    func testARebuiltStationGetsTheTrustListToo() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        let session = CallSession(station: FakeStation(), userDefaults: defaults)
        session.applyVocoderTrust(["TESTSER1"])
        let rebuilt = FakeStation()
        session.reconfigure(station: rebuilt, hasCredentials: false)
        XCTAssertEqual(rebuilt.vocoderTrustCalls, [["TESTSER1"]])
    }

    /// With nothing trusted the refusal names the fix — Settings → Vocoders —
    /// rather than telling the operator to plug in a stick that may already
    /// be plugged in.
    func testDiallingWithNothingTrustedSaysWhereToAddOne() {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: defaults)
        session.operatorCallsign = "AJ7HR"
        session.reflectorIndex = dstarIndex
        session.poll()

        XCTAssertThrowsError(try session.connect(node: "XLX836 A", network: .dstar)) { error in
            guard case CallSession.ConnectError.noTrustedVocoder(.dstar) = error else {
                return XCTFail("expected noTrustedVocoder, got \(error)")
            }
            XCTAssertTrue(
                VocoderSetupPrompt.offersSettings(for: error.localizedDescription),
                error.localizedDescription)
        }
        XCTAssertTrue(fake.dstarConnects.isEmpty)
    }

    // MARK: - Engine refusal text

    func testTheEnginesRefusalIsShownPlainlyWithTheSettingsPrompt() {
        let error = StationError(
            code: -19, text: "dstar error",
            detail: "dstar error: No trusted vocoder — add one in Settings → Vocoders")
        let message = connectFailureMessage(for: error, node: "XLX836 A")
        XCTAssertTrue(VocoderSetupPrompt.offersSettings(for: message), message)
        // YSF and NXDN have no dedicated mapping; the trust refusal still reads plainly.
        for code: Int32 in [-20, -21, -22] {
            let e = StationError(
                code: code, text: "x error",
                detail: "x error: No trusted vocoder — add one in Settings → Vocoders")
            let m = connectFailureMessage(for: e, node: "TG 91")
            XCTAssertTrue(VocoderSetupPrompt.offersSettings(for: m), m)
        }
    }

    // MARK: - Config export / import

    func testExportNeverCarriesTheTrustList() throws {
        let defaults: [String: Any] = [
            "vocoder.trusted": Data("[{\"serial\":\"TESTSER1\"}]".utf8),
            // Even a scalar under the vocoder prefix stays home.
            "vocoder.lastPort": "/dev/cu.usbserial-TESTSER1",
            "audio.inputGain": 0.4,
        ]
        let archive = ConfigArchive.make(
            sections: Set(ConfigSection.allCases),
            from: ConfigArchive.Sources(
                defaults: defaults, setups: [], micProfiles: [], selectedSetupID: nil,
                defaultSetupID: nil, directory: [], callsign: nil))
        let json = String(decoding: try ConfigArchive.encode(archive), as: UTF8.self)
        XCTAssertFalse(json.contains("vocoder"), json)
        XCTAssertFalse(json.contains("TESTSER1"), json)
        XCTAssertNotNil(archive.settings?["audio.inputGain"])
    }

    func testTheRealExportPathLeavesTheTrustListBehind() throws {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        UserDefaultsVocoderTrustStore(defaults).save([
            TrustedVocoder(serial: "TESTSER1", prodid: "AMBE3000R", model: nil, lastSeen: nil)
        ])
        let session = CallSession(
            station: FakeStation(), directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
        let archive = ConfigTransfer.archive(
            sections: Set(ConfigSection.allCases), session: session,
            setupStore: UserDefaultsSetupStore(defaults),
            profileStore: UserDefaultsMicProfileStore(defaults), defaults: defaults)
        let json = String(decoding: try ConfigArchive.encode(archive), as: UTF8.self)
        XCTAssertFalse(json.contains("TESTSER1"))
        XCTAssertFalse(json.contains("vocoder."))
    }

    /// A hand-edited file cannot widen the list: `vocoder.*` keys in any
    /// scalar slice are dropped on the way in.
    func testImportCannotPlantATrustList() throws {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        let session = CallSession(
            station: FakeStation(), directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
        var archive = ConfigArchive.make(
            sections: [.settings, .interface],
            from: ConfigArchive.Sources(
                defaults: ["audio.inputGain": 0.4], setups: [], micProfiles: [],
                selectedSetupID: nil, defaultSetupID: nil, directory: [], callsign: nil))
        archive.settings?["vocoder.trusted"] = .string("CATCABLE")
        archive.interface?["vocoder.trusted"] = .string("CATCABLE")

        ConfigTransfer.apply(
            archive, session: session, setupStore: UserDefaultsSetupStore(defaults),
            profileStore: UserDefaultsMicProfileStore(defaults), defaults: defaults)

        XCTAssertNil(defaults.object(forKey: "vocoder.trusted"))
        XCTAssertEqual(UserDefaultsVocoderTrustStore(defaults).load(), [])
        XCTAssertEqual(defaults.double(forKey: "audio.inputGain"), 0.4)
    }

    func testImportingTheV1FixtureLeavesTheTrustListAlone() throws {
        let (defaults, suite) = scratchDefaults()
        defer { defaults.removePersistentDomain(forName: suite) }
        let mine = [
            TrustedVocoder(serial: "TESTSER1", prodid: "AMBE3000R", model: nil, lastSeen: nil)
        ]
        UserDefaultsVocoderTrustStore(defaults).save(mine)
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: "astar-config-v1", withExtension: "astarconfig",
                subdirectory: "Fixtures"))
        let session = CallSession(
            station: FakeStation(), directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)

        let summary = ConfigTransfer.apply(
            try ConfigArchive.decode(try Data(contentsOf: url)), session: session,
            setupStore: UserDefaultsSetupStore(defaults),
            profileStore: UserDefaultsMicProfileStore(defaults), defaults: defaults)

        XCTAssertEqual(summary.setupsAdded, 2)
        XCTAssertEqual(UserDefaultsVocoderTrustStore(defaults).load(), mine)
        XCTAssertEqual(ConfigVersion.current, 1, "an added field does not bump the version")
    }
}
