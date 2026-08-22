// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-b52e — export/import of the app's configuration.
///
/// The load-bearing rule: **nothing from the Keychain is ever written to an
/// archive.** The portal password, username and node stay where they are.
final class ConfigArchiveTests: XCTestCase {

    // MARK: - Section ownership

    func testEverySectionKeyBelongsToExactlyOneSection() {
        let defaults: [String: Any] = [
            "audio.inputGain": 0.4, "audio.setups": Data(), "audio.micProfiles": Data(),
            "audio.selectedSetup": "x", "audio.defaultSetup": "y",
            "serial.enabled": true, "m17.callsign": "AJ7HR",
            "m17.audio.txTrim": 1.0, "directory.nodes": Data(),
            "ui.showInDock": true,
        ]
        let settings = Set(ConfigArchive.settingsSlice(from: defaults).keys)
        let interface = Set(ConfigArchive.interfaceSlice(from: defaults).keys)
        XCTAssertTrue(settings.isDisjoint(with: interface))
        // Rig-, directory- and callsign-owned keys are carried by their own
        // typed sections and must not leak into the scalar slices.
        for owned in [
            "audio.setups", "audio.micProfiles", "audio.selectedSetup",
            "audio.defaultSetup", "directory.nodes", "m17.callsign",
        ] {
            XCTAssertFalse(settings.contains(owned), "\(owned) leaked into settings")
            XCTAssertFalse(interface.contains(owned), "\(owned) leaked into interface")
        }
    }

    func testSettingsSliceTakesAudioSerialAndM17Scalars() {
        let defaults: [String: Any] = [
            "audio.inputGain": 0.4, "serial.enabled": true, "m17.audio.txTrim": 1.5,
        ]
        XCTAssertEqual(
            Set(ConfigArchive.settingsSlice(from: defaults).keys),
            ["audio.inputGain", "serial.enabled", "m17.audio.txTrim"])
    }

    func testInterfaceSliceTakesOnlyUIKeys() {
        let defaults: [String: Any] = [
            "ui.showInDock": true, "ui.network": "allstar", "audio.inputGain": 0.4,
        ]
        XCTAssertEqual(
            Set(ConfigArchive.interfaceSlice(from: defaults).keys),
            ["ui.showInDock", "ui.network"])
    }

    func testForeignKeysAreIgnored() {
        // dictionaryRepresentation() also returns NSGlobalDomain noise.
        let defaults: [String: Any] = [
            "AppleLanguages": ["en"], "NSWindow Frame main": "0 0", "audio.inputGain": 0.4,
        ]
        XCTAssertEqual(Array(ConfigArchive.settingsSlice(from: defaults).keys), ["audio.inputGain"])
        XCTAssertTrue(ConfigArchive.interfaceSlice(from: defaults).isEmpty)
    }

    func testDeadKeysAreNotExported() {
        // audio.wideband is documented dead (astar-e542). Exporting it would
        // propagate cruft into every future import.
        let defaults: [String: Any] = ["audio.wideband": true, "audio.inputGain": 0.4]
        XCTAssertEqual(Array(ConfigArchive.settingsSlice(from: defaults).keys), ["audio.inputGain"])
    }

    func testUnrepresentableValuesAreDroppedNotCrashed() {
        let defaults: [String: Any] = ["audio.weird": Data([1, 2, 3]), "audio.inputGain": 0.4]
        XCTAssertEqual(Array(ConfigArchive.settingsSlice(from: defaults).keys), ["audio.inputGain"])
    }

    // MARK: - Scalar round trip

    func testScalarsSurviveAJSONRoundTrip() throws {
        let defaults: [String: Any] = [
            "audio.inputGain": Double(0.4022594),
            "audio.compression": true,
            "audio.voxHangtimeMS": Int(750),
            "audio.input": "USB Audio Device",
        ]
        let slice = ConfigArchive.settingsSlice(from: defaults)
        let data = try JSONEncoder().encode(slice)
        let back = try JSONDecoder().decode([String: SettingValue].self, from: data)
        XCTAssertEqual(back, slice)
        XCTAssertEqual(back["audio.compression"], .bool(true))
        XCTAssertEqual(back["audio.voxHangtimeMS"], .int(750))
        XCTAssertEqual(back["audio.input"], .string("USB Audio Device"))
    }

    func testBoolIsNotFlattenedToANumber() throws {
        // NSNumber bridging makes this easy to get wrong: a Bool that decodes
        // back as 1 would turn every toggle into a number on import.
        let slice = ConfigArchive.settingsSlice(from: ["audio.compression": true])
        let back = try JSONDecoder().decode(
            [String: SettingValue].self, from: try JSONEncoder().encode(slice))
        XCTAssertEqual(back["audio.compression"], .bool(true))
        XCTAssertNotEqual(back["audio.compression"], .int(1))
    }

    // MARK: - Building an archive

    private func sampleSources() -> ConfigArchive.Sources {
        ConfigArchive.Sources(
            defaults: [
                "audio.inputGain": 0.4, "serial.enabled": true, "ui.showInDock": true,
                "m17.callsign": "AJ7HR",
            ],
            setups: [Setup(id: "s1", name: "UCI150 desk", hardwareProfileID: "uci150")],
            micProfiles: [
                MicProfile(
                    id: "m1", name: "desk", deviceName: "USB Audio Device",
                    characterizationJSON: "{}")
            ],
            selectedSetupID: "s1", defaultSetupID: "s1",
            directory: [NodeEntry(id: "n1", label: "AJ7HR", node: "12345")],
            callsign: "AJ7HR")
    }

    func testAnEmptySelectionProducesAnArchiveWithNoSections() {
        let archive = ConfigArchive.make(sections: [], from: sampleSources())
        XCTAssertNil(archive.rigs)
        XCTAssertNil(archive.directory)
        XCTAssertNil(archive.settings)
        XCTAssertNil(archive.callsign)
        XCTAssertNil(archive.interface)
        XCTAssertEqual(archive.version, ConfigArchive.currentVersion)
    }

    func testRigsSectionCarriesSetupsAndProfiles() {
        let archive = ConfigArchive.make(sections: [.rigs], from: sampleSources())
        XCTAssertEqual(archive.rigs?.setups.map(\.id), ["s1"])
        XCTAssertEqual(archive.rigs?.micProfiles.map(\.id), ["m1"])
        XCTAssertEqual(archive.rigs?.defaultSetupID, "s1")
        XCTAssertNil(archive.directory, "rigs must not drag the directory along")
        XCTAssertNil(archive.settings)
    }

    func testChoosingOneSectionExcludesTheOthers() {
        let archive = ConfigArchive.make(sections: [.directory], from: sampleSources())
        XCTAssertEqual(archive.directory?.map(\.node), ["12345"])
        XCTAssertNil(archive.rigs)
        XCTAssertNil(archive.settings)
        XCTAssertNil(archive.interface)
        XCTAssertNil(archive.callsign)
    }

    func testCallsignIsItsOwnSectionAndNotPartOfSettings() {
        let withCallsign = ConfigArchive.make(sections: [.callsign], from: sampleSources())
        XCTAssertEqual(withCallsign.callsign, "AJ7HR")
        XCTAssertNil(withCallsign.settings)

        let withSettings = ConfigArchive.make(sections: [.settings], from: sampleSources())
        XCTAssertNil(withSettings.callsign)
        XCTAssertNil(withSettings.settings?["m17.callsign"])
    }

    func testEverySectionAtOnce() {
        let archive = ConfigArchive.make(
            sections: Set(ConfigSection.allCases), from: sampleSources())
        XCTAssertNotNil(archive.rigs)
        XCTAssertNotNil(archive.directory)
        XCTAssertNotNil(archive.settings)
        XCTAssertNotNil(archive.interface)
        XCTAssertNotNil(archive.callsign)
    }

    // MARK: - The secret-free contract

    func testNoArchiveEverCarriesCredentials() throws {
        let archive = ConfigArchive.make(
            sections: Set(ConfigSection.allCases), from: sampleSources())
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        let json = String(decoding: try encoder.encode(archive), as: UTF8.self).lowercased()
        for forbidden in [
            "portalpass", "portaluser", "portalnode", "password", "secret", "keychain",
        ] {
            XCTAssertFalse(json.contains(forbidden), "archive leaked \(forbidden):\n\(json)")
        }
    }

    func testCredentialKeysInDefaultsWouldStillNotBeExported() {
        // Defence in depth: credentials live in the Keychain, but if anything
        // ever wrote one into the defaults domain it must not ride along.
        let defaults: [String: Any] = [
            "audio.inputGain": 0.4,
            "audio.portalPass": "hunter2",
            "serial.portalPassword": "hunter2",
        ]
        let keys = Set(ConfigArchive.settingsSlice(from: defaults).keys)
        XCTAssertEqual(keys, ["audio.inputGain"])
    }

    // MARK: - Archive round trip

    func testArchiveSurvivesAFullJSONRoundTrip() throws {
        let archive = ConfigArchive.make(
            sections: Set(ConfigSection.allCases), from: sampleSources())
        let data = try ConfigArchive.encode(archive)
        let back = try ConfigArchive.decode(data)
        XCTAssertEqual(back.rigs?.setups.map(\.id), ["s1"])
        XCTAssertEqual(back.directory?.map(\.id), ["n1"])
        XCTAssertEqual(back.callsign, "AJ7HR")
        XCTAssertEqual(back.settings?["audio.inputGain"], .double(0.4))
        XCTAssertEqual(back.interface?["ui.showInDock"], .bool(true))
    }

    func testEncodedArchiveIsHumanReadable() throws {
        // An exported file is something a user may open, read and email. Setups
        // must not land as an opaque base64 blob the way UserDefaults holds them.
        let data = try ConfigArchive.encode(
            ConfigArchive.make(sections: [.rigs], from: sampleSources()))
        let json = String(decoding: data, as: UTF8.self)
        XCTAssertTrue(json.contains("UCI150 desk"), json)
        XCTAssertTrue(json.contains("\n"), "should be pretty-printed")
    }

    func testARejectedFileFailsCleanly() {
        XCTAssertThrowsError(try ConfigArchive.decode(Data("not json".utf8)))
    }

    func testAFutureVersionIsRefused() throws {
        var archive = ConfigArchive.make(sections: [.rigs], from: sampleSources())
        archive.version = ConfigArchive.currentVersion + 1
        let data = try JSONEncoder().encode(archive)
        XCTAssertThrowsError(try ConfigArchive.decode(data)) { error in
            XCTAssertTrue(
                "\(error)".lowercased().contains("newer"), "unhelpful error: \(error)")
        }
    }
}
