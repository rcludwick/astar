// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-b52e — a real `0.1.4beta` export, kept as a fixture so the **v1 format
/// stays readable forever**.
///
/// Every other config test builds its archive with the current code, so the two
/// sides move together and a format change can never fail them. This one is a
/// file written by a shipped build: if a later change stops it importing, that
/// is a compatibility break in a file users already have on disk.
///
/// Scrubbed before committing — callsign is `N0CALL`, two of the operator's
/// nodes are gone, and the USB adapter's hardware serial is genericised out of
/// the saved config's port path. The rest is verbatim.
final class ConfigFixtureTests: XCTestCase {

    private func fixture(_ name: String = "astar-config-v1") throws -> Data {
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: name, withExtension: "astarconfig",
                subdirectory: "Fixtures"),
            "fixture \(name) missing from the test bundle")
        return try Data(contentsOf: url)
    }

    func testAShippedV1ExportStillDecodes() throws {
        let archive = try ConfigArchive.decode(try fixture())
        XCTAssertEqual(archive.version, 1)
        XCTAssertEqual(archive.appVersion, "0.1.4beta")
        XCTAssertEqual(archive.presentSections, Set(ConfigSection.allCases))
    }

    func testItCarriesWhatItSaysOnTheTin() throws {
        let a = try ConfigArchive.decode(try fixture())
        XCTAssertEqual(a.rigs?.setups.map(\.name), ["UCI150", "Headphone Port"])
        XCTAssertEqual(a.directory?.count, 13)
        XCTAssertEqual(a.callsign, "N0CALL")
        XCTAssertEqual(a.settings?["audio.input"], .string("USB Audio Device"))
        XCTAssertEqual(a.interface?["ui.network"], .string("m17"))
        // Per-config serial survives the round trip — the richest nested type
        // in the format, and the one most likely to break silently.
        let uci = try XCTUnwrap(a.rigs?.setups.first { $0.name == "UCI150" })
        XCTAssertEqual(uci.serial?.debounceMs, 30)
        XCTAssertNotNil(uci.serial?.portPath)
        // Both networks are represented, so a decode that lost `network` fails.
        let nets = Set((a.directory ?? []).map(\.network))
        XCTAssertEqual(nets, [.allstar, .m17])
    }

    func testItImportsCleanlyIntoAFreshMac() throws {
        let name = "astar.test.fixture.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let session = CallSession(
            station: NullStation(),
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
        let setupStore = UserDefaultsSetupStore(defaults)

        let summary = ConfigTransfer.apply(
            try ConfigArchive.decode(try fixture()), session: session,
            setupStore: setupStore,
            profileStore: UserDefaultsMicProfileStore(defaults),
            defaults: defaults)

        XCTAssertEqual(summary.setupsAdded, 2)
        XCTAssertEqual(summary.directoryAdded, 13)
        XCTAssertEqual(session.directoryAll().count, 13)
        XCTAssertEqual(session.m17Callsign, "N0CALL")
        // The ★ must land: this is the value that was silently lost before
        // SetupController.reloadFromStore existed.
        XCTAssertNotNil(setupStore.loadDefaultID())
        XCTAssertTrue(setupStore.all().contains { $0.id == setupStore.loadDefaultID() })
        // Written before Bluetooth PTT and the transmit time-out: no config
        // has a spec, and the time-out reads as the 2 min default.
        XCTAssertEqual(setupStore.all().map(\.bluetoothPTT), [nil, nil])
        XCTAssertEqual(UserDefaultsAudioSettingsStore(defaults).load().txTimeoutSeconds, 120)
        // Written when VOX judged the peak: both thresholds arrive translated
        // by the crest offset (-10 dB), the settings one under the new key.
        XCTAssertEqual(
            Set(setupStore.all().compactMap(\.voxThreshold).map { ($0 * 10).rounded() / 10 }),
            [-50, -34.6])
        XCTAssertEqual(defaults.object(forKey: "audio.voxThresholdRMSDBFS") as? Double, -50)
        XCTAssertNil(
            defaults.object(forKey: "audio.voxThresholdDBFS"), "the old key is not written")
        XCTAssertEqual(UserDefaultsAudioSettingsStore(defaults).load().voxThresholdDBFS, -50)
    }

    func testAnOldFileImportsItsThresholdTranslatedOverANewScaleOne() throws {
        // A Mac already on the new scale imports a 0.1.4beta file: the file's
        // -40 peak arrives as -50, the same as an in-place upgrade would give.
        let name = "astar.test.fixture.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }
        defaults.set(Float(-33), forKey: "audio.voxThresholdRMSDBFS")
        let session = CallSession(
            station: NullStation(),
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
        ConfigTransfer.apply(
            try ConfigArchive.decode(try fixture()).filtered(to: [.settings]), session: session,
            setupStore: UserDefaultsSetupStore(defaults),
            profileStore: UserDefaultsMicProfileStore(defaults),
            defaults: defaults)
        XCTAssertEqual(UserDefaultsAudioSettingsStore(defaults).load().voxThresholdDBFS, -50)
    }

    func testTheFixtureCarriesNothingPersonal() throws {
        // Guards the scrub itself: this file ships in the repo, which is going
        // to be public.
        for name in ["astar-config-v1", "astar-config-v1-bleptt"] {
            let text = String(decoding: try fixture(name), as: UTF8.self)
            for forbidden in ["AJ7HR", "69586", "543260", "WA7ABU", "5B210098241"] {
                XCTAssertFalse(text.contains(forbidden), "\(name) leaked \(forbidden)")
            }
        }
    }

    // MARK: - v1 still means today's hardware

    /// The shipped v1 file predates Bluetooth PTT. It keeps planning exactly
    /// what it did: serial for the UCI150 rig, nothing for the headphone rig.
    func testTheV1FixtureStillPlansTodaysHardware() throws {
        let a = try ConfigArchive.decode(try fixture())
        let setups = try XCTUnwrap(a.rigs?.setups)
        XCTAssertEqual(setups.map(\.bluetoothPTT), [nil, nil])
        let uci = try XCTUnwrap(setups.first { $0.name == "UCI150" })
        let phones = try XCTUnwrap(setups.first { $0.name == "Headphone Port" })
        XCTAssertEqual(PTTHardwarePlan.plan(for: uci), .serial(try XCTUnwrap(uci.serial)))
        XCTAssertEqual(PTTHardwarePlan.plan(for: phones), .noHardware)
        XCTAssertEqual(uci.pttHardware, .uci150)
        XCTAssertEqual(phones.pttHardware, PTTHardwareChoice.none)
        XCTAssertNil(a.settings?["audio.txTimeoutSeconds"])
    }

    // MARK: - The Bluetooth PTT fixture (still version 1)

    /// A hand-written v1 file with a Bluetooth spec and a 5 min transmit
    /// time-out. Both are additions, so the version stays 1, and both must
    /// survive a decode and re-encode unchanged.
    func testBlePTTFixtureRoundTrips() throws {
        let data = try fixture("astar-config-v1-bleptt")
        let a = try ConfigArchive.decode(data)
        let spec = BluetoothPTTSpec(deviceID: "inrico-b02ptt", preferredName: "B02PTT-6522")

        XCTAssertEqual(a.version, 1)
        XCTAssertEqual(ConfigVersion.current, 1, "an added field is not a new format")
        XCTAssertEqual(a.appVersion, "0.1.16-beta")
        XCTAssertEqual(a.presentSections, [.rigs, .settings])
        XCTAssertEqual(a.rigs?.setups.count, 1)
        let rig = try XCTUnwrap(a.rigs?.setups.first)
        XCTAssertEqual(rig.bluetoothPTT, spec)
        XCTAssertEqual(rig.hardwareProfileID, HardwareProfileRegistry.headsetID)
        XCTAssertEqual(rig.serial, .uci150, "the serial spec rides along untouched")
        XCTAssertEqual(PTTHardwarePlan.plan(for: rig), .bluetooth(spec))
        XCTAssertEqual(a.settings?["audio.txTimeoutSeconds"], .int(300))

        XCTAssertEqual(try ConfigArchive.decode(try ConfigArchive.encode(a)), a)
        XCTAssertFalse(String(decoding: data, as: UTF8.self).contains("blePTT"))
    }

    /// Importing it lands the spec and the time-out, and confirms nothing:
    /// the button stays unconfirmed until someone presses it on this Mac.
    func testBlePTTFixtureImportsUnconfirmed() throws {
        let name = "astar.test.fixture.bleptt.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        // The session's own audio store points at the test domain, so the
        // imported time-out never lands in the test host's standard domain.
        let session = CallSession(
            station: NullStation(),
            audioStore: UserDefaultsAudioSettingsStore(defaults),
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
        let setupStore = UserDefaultsSetupStore(defaults)

        ConfigTransfer.apply(
            try ConfigArchive.decode(try fixture("astar-config-v1-bleptt")), session: session,
            setupStore: setupStore,
            profileStore: UserDefaultsMicProfileStore(defaults),
            defaults: defaults)

        let spec = BluetoothPTTSpec(deviceID: "inrico-b02ptt", preferredName: "B02PTT-6522")
        XCTAssertEqual(setupStore.all().map(\.bluetoothPTT), [spec])
        XCTAssertEqual(UserDefaultsAudioSettingsStore(defaults).load().txTimeoutSeconds, 300)
        XCTAssertEqual(session.txTimeoutSeconds, 300, "the session took the imported time-out")
        XCTAssertEqual(
            UserDefaultsBluetoothPTTLocalStore(defaults).confirmations(deviceID: "inrico-b02ptt"),
            [])
        // These guard against an import path ever writing `blePTT.` keys:
        // confirmations are per-Mac and never travel in a file. The hostile
        // test below covers a file crafted to carry one anyway.
        let written = UserDefaults.standard.persistentDomain(forName: name) ?? [:]
        XCTAssertEqual(written.keys.filter { $0.hasPrefix("blePTT.") }, [])
    }

    /// What the store itself would write for one confirmed unit, as a
    /// string: the only shape a `.astarconfig` value can carry it in.
    private func confirmationPayload(_ platformID: UUID, deviceID: String) throws -> String {
        String(
            decoding: try JSONEncoder().encode([
                deviceID: [
                    BLEConfirmation(
                        platformID: platformID, advertisedName: "B02PTT-6522",
                        confirmedAt: Date(timeIntervalSinceReferenceDate: 0))
                ]
            ]),
            as: UTF8.self)
    }

    /// Imports arrive unconfirmed, even from a hand-edited file:
    /// `ConfigTransfer.apply` skips every `blePTT.` key in both the
    /// `settings` and `interface` sections, so a crafted archive writes
    /// nothing under the confirmation key at all, and the summary the user
    /// reads counts only what was written. The key rides in both sections,
    /// so each loop's filter is exercised on its own, and each section also
    /// carries one ordinary key, so a filter that dropped everything fails.
    func testHostileArchiveCannotConfirmAButton() throws {
        let name = "astar.test.fixture.hostile.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let deviceID = "inrico-b02ptt"
        let platformID = try XCTUnwrap(UUID(uuidString: "6A2F1C3E-5B7D-4E89-9A01-23456789ABCD"))
        let payload = try confirmationPayload(platformID, deviceID: deviceID)
        let hostile = ConfigArchive(
            version: 1, exportedAt: Date(), appVersion: nil,
            rigs: nil, directory: nil,
            settings: [
                UserDefaultsBluetoothPTTLocalStore.key: .string(payload),
                "audio.voxHangtimeMS": .int(700),
            ],
            callsign: nil,
            interface: [
                UserDefaultsBluetoothPTTLocalStore.key: .string(payload),
                "ui.hostileTestMarker": .bool(true),
            ])

        let summary = ConfigTransfer.apply(
            hostile,
            session: CallSession(
                station: NullStation(),
                audioStore: UserDefaultsAudioSettingsStore(defaults),
                directoryStore: UserDefaultsNodeDirectoryStore(defaults),
                userDefaults: defaults),
            setupStore: UserDefaultsSetupStore(defaults),
            profileStore: UserDefaultsMicProfileStore(defaults),
            defaults: defaults)

        XCTAssertNil(
            defaults.object(forKey: UserDefaultsBluetoothPTTLocalStore.key),
            "an import never writes a blePTT. key")
        let store = UserDefaultsBluetoothPTTLocalStore(defaults)
        XCTAssertEqual(store.confirmations(deviceID: deviceID), [])
        XCTAssertFalse(store.isConfirmed(platformID: platformID, deviceID: deviceID))

        // The ordinary keys still land, and the summary counts them alone.
        XCTAssertEqual(defaults.object(forKey: "audio.voxHangtimeMS") as? Int, 700)
        XCTAssertEqual(defaults.object(forKey: "ui.hostileTestMarker") as? Bool, true)
        XCTAssertEqual(summary.settingsApplied, 1, "summary counted a blePTT. setting")
        XCTAssertEqual(summary.interfaceApplied, 1, "summary counted a blePTT. window setting")
        XCTAssertEqual(summary, ConfigMerge.Summary(settingsApplied: 1, interfaceApplied: 1))
    }

    /// The second layer: even a string that somehow lands under the key
    /// (an older astar's import, a hand-edited plist) is not a
    /// confirmation, because the store reads only `Data`. If the store ever
    /// accepts a string, this goes red.
    func testAConfirmationStringInDefaultsIsNotAConfirmation() throws {
        let name = "astar.test.fixture.hostile-string.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let deviceID = "inrico-b02ptt"
        let platformID = try XCTUnwrap(UUID(uuidString: "6A2F1C3E-5B7D-4E89-9A01-23456789ABCD"))
        defaults.set(
            try confirmationPayload(platformID, deviceID: deviceID),
            forKey: UserDefaultsBluetoothPTTLocalStore.key)

        let store = UserDefaultsBluetoothPTTLocalStore(defaults)
        XCTAssertEqual(store.confirmations(deviceID: deviceID), [])
        XCTAssertFalse(store.isConfirmed(platformID: platformID, deviceID: deviceID))
    }
}
