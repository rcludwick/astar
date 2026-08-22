// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-b52e — the whole trip, against real stores: gather → encode → decode
/// → apply, from one `UserDefaults` suite into a second, empty one.
///
/// The unit tests cover the rules; this covers the wiring, which is the part
/// that can quietly write to the wrong store.
final class ConfigTransferTests: XCTestCase {

    private var suiteA: UserDefaults!
    private var suiteB: UserDefaults!
    private var nameA = ""
    private var nameB = ""

    override func setUp() {
        super.setUp()
        nameA = "astar.test.transfer.a.\(UUID().uuidString)"
        nameB = "astar.test.transfer.b.\(UUID().uuidString)"
        suiteA = UserDefaults(suiteName: nameA)
        suiteB = UserDefaults(suiteName: nameB)
    }

    override func tearDown() {
        UserDefaults.standard.removePersistentDomain(forName: nameA)
        UserDefaults.standard.removePersistentDomain(forName: nameB)
        super.tearDown()
    }

    /// A session wired to one suite, so nothing here touches the real app.
    private func session(_ defaults: UserDefaults) -> CallSession {
        CallSession(
            station: NullStation(),
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
    }

    private func populate(_ defaults: UserDefaults, session: CallSession) {
        let setups = UserDefaultsSetupStore(defaults)
        setups.save(
            Setup(
                id: "rig-1", name: "UCI150 desk", hardwareProfileID: "uci150",
                inputDevice: "USB Audio Device", outputDevice: "USB Audio Device",
                inputGain: 0.4, micProfileID: "mic-1"))
        setups.saveDefaultID("rig-1")
        UserDefaultsMicProfileStore(defaults).save(
            MicProfile(
                id: "mic-1", name: "desk", deviceName: "USB Audio Device",
                characterizationJSON: "{\"notches\":[]}"))
        session.directoryUpsert(
            NodeEntry(id: "n-1", label: "AR Newswire", node: "516228", favorite: true))
        defaults.set(0.4022594, forKey: "audio.inputGain")
        defaults.set(true, forKey: "audio.compression")
        defaults.set(750, forKey: "audio.voxHangtimeMS")
        defaults.set("/dev/cu.usbserial", forKey: "serial.portPath")
        defaults.set(true, forKey: "ui.showInDock")
        session.m17Callsign = "AJ7HR"
    }

    private func roundTrip(sections: Set<ConfigSection>) throws -> (
        source: CallSession, target: CallSession, summary: ConfigMerge.Summary
    ) {
        let a = session(suiteA)
        populate(suiteA, session: a)
        let archive = ConfigTransfer.archive(
            sections: sections, session: a,
            setupStore: UserDefaultsSetupStore(suiteA),
            profileStore: UserDefaultsMicProfileStore(suiteA),
            defaults: suiteA)
        let decoded = try ConfigArchive.decode(try ConfigArchive.encode(archive))
        let b = session(suiteB)
        let summary = ConfigTransfer.apply(
            decoded, session: b,
            setupStore: UserDefaultsSetupStore(suiteB),
            profileStore: UserDefaultsMicProfileStore(suiteB),
            defaults: suiteB)
        return (a, b, summary)
    }

    func testEverythingLandsOnTheOtherSide() throws {
        let r = try roundTrip(sections: Set(ConfigSection.allCases))

        let setups = UserDefaultsSetupStore(suiteB).all()
        XCTAssertEqual(setups.map(\.id), ["rig-1"])
        XCTAssertEqual(setups.first?.inputDevice, "USB Audio Device")
        XCTAssertEqual(setups.first?.micProfileID, "mic-1")
        XCTAssertEqual(UserDefaultsSetupStore(suiteB).loadDefaultID(), "rig-1")

        XCTAssertEqual(UserDefaultsMicProfileStore(suiteB).all().map(\.id), ["mic-1"])
        XCTAssertEqual(r.target.directoryAll().map(\.node), ["516228"])
        XCTAssertTrue(r.target.directoryAll().first?.favorite == true)

        XCTAssertEqual(suiteB.double(forKey: "audio.inputGain"), 0.4022594, accuracy: 1e-7)
        XCTAssertTrue(suiteB.bool(forKey: "audio.compression"))
        XCTAssertEqual(suiteB.integer(forKey: "audio.voxHangtimeMS"), 750)
        XCTAssertEqual(suiteB.string(forKey: "serial.portPath"), "/dev/cu.usbserial")
        XCTAssertTrue(suiteB.bool(forKey: "ui.showInDock"))
        XCTAssertEqual(r.target.m17Callsign, "AJ7HR")
    }

    func testUncheckedSectionsDoNotTravel() throws {
        let r = try roundTrip(sections: [.rigs])
        XCTAssertEqual(UserDefaultsSetupStore(suiteB).all().count, 1)
        XCTAssertTrue(r.target.directoryAll().isEmpty, "directory travelled unasked")
        XCTAssertEqual(r.target.m17Callsign, "", "callsign travelled unasked")
        XCTAssertNil(suiteB.object(forKey: "audio.inputGain"))
        XCTAssertNil(suiteB.object(forKey: "ui.showInDock"))
    }

    func testCallsignStaysBehindWhenUnchecked() throws {
        // The share case: everything useful, nothing identifying.
        let r = try roundTrip(sections: [.rigs, .settings, .interface])
        XCTAssertEqual(r.target.m17Callsign, "")
        XCTAssertEqual(UserDefaultsSetupStore(suiteB).all().count, 1)
    }

    func testImportingTwiceChangesNothingTheSecondTime() throws {
        let r = try roundTrip(sections: Set(ConfigSection.allCases))
        let archive = ConfigTransfer.archive(
            sections: Set(ConfigSection.allCases), session: r.source,
            setupStore: UserDefaultsSetupStore(suiteA),
            profileStore: UserDefaultsMicProfileStore(suiteA),
            defaults: suiteA)
        let second = ConfigTransfer.apply(
            archive, session: r.target,
            setupStore: UserDefaultsSetupStore(suiteB),
            profileStore: UserDefaultsMicProfileStore(suiteB),
            defaults: suiteB)
        XCTAssertEqual(second.setupsAdded, 0)
        XCTAssertEqual(second.setupsUpdated, 0)
        XCTAssertEqual(second.directoryAdded, 0)
        XCTAssertEqual(UserDefaultsSetupStore(suiteB).all().count, 1, "config duplicated")
        XCTAssertEqual(r.target.directoryAll().count, 1, "node duplicated")
    }

    func testAnImportDoesNotDeleteWhatIsAlreadyThere() throws {
        let b = session(suiteB)
        UserDefaultsSetupStore(suiteB).save(
            Setup(id: "mine", name: "My rig", hardwareProfileID: "headset"))
        b.directoryUpsert(NodeEntry(id: "mine-1", label: "Mine", node: "99999"))

        let a = session(suiteA)
        populate(suiteA, session: a)
        let archive = ConfigTransfer.archive(
            sections: Set(ConfigSection.allCases), session: a,
            setupStore: UserDefaultsSetupStore(suiteA),
            profileStore: UserDefaultsMicProfileStore(suiteA),
            defaults: suiteA)
        ConfigTransfer.apply(
            archive, session: b,
            setupStore: UserDefaultsSetupStore(suiteB),
            profileStore: UserDefaultsMicProfileStore(suiteB),
            defaults: suiteB)

        XCTAssertEqual(
            Set(UserDefaultsSetupStore(suiteB).all().map(\.id)), ["mine", "rig-1"],
            "an import removed a config that was already here")
        XCTAssertEqual(Set(b.directoryAll().map(\.node)), ["99999", "516228"])
    }

    func testTheExportedFileCarriesNoCredentials() throws {
        let a = session(suiteA)
        populate(suiteA, session: a)
        // Even with a secret-looking key planted in the domain.
        suiteA.set("hunter2", forKey: "audio.portalPass")
        let data = try ConfigArchive.encode(
            ConfigTransfer.archive(
                sections: Set(ConfigSection.allCases), session: a,
                setupStore: UserDefaultsSetupStore(suiteA),
                profileStore: UserDefaultsMicProfileStore(suiteA),
                defaults: suiteA))
        let json = String(decoding: data, as: UTF8.self)
        XCTAssertFalse(json.contains("hunter2"), json)
        XCTAssertFalse(json.lowercased().contains("portalpass"))
    }

    func testTheDefaultConfigIsNeverWrittenAsASavedConfig() throws {
        // "None (system default)" is synthesised, not stored. Importing it as a
        // real row would give the picker two of them.
        let r = try roundTrip(sections: [.rigs])
        XCTAssertFalse(
            UserDefaultsSetupStore(suiteB).all().contains { $0.id == SystemDefaultSetup.id })
    }
}
