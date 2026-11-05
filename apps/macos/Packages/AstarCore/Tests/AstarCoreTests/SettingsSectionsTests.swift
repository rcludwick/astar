// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The Settings pane's collapsible sections: which start open, where their
/// state is remembered, that jumps open the right one, and that none of it
/// rides along in an exported config.
@MainActor
final class SettingsSectionsTests: XCTestCase {

    private var defaults: UserDefaults!
    private var suiteName = ""

    override func setUp() {
        super.setUp()
        suiteName = "astar.test.settingsSections.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suiteName)
    }

    override func tearDown() {
        UserDefaults.standard.removePersistentDomain(forName: suiteName)
        super.tearDown()
    }

    func testOnlyOperatorAndAccountStartOpen() {
        let disclosure = SettingsDisclosure(defaults: defaults)
        XCTAssertEqual(disclosure.expanded, [.operatorIdentity, .account])
        XCTAssertFalse(disclosure.allExpanded)
    }

    func testEverySectionHasItsOwnUISettingsKey() {
        let keys = SettingsSectionID.allCases.map(\.defaultsKey)
        XCTAssertEqual(Set(keys).count, keys.count, "two sections share a key")
        for key in keys {
            XCTAssertTrue(key.hasPrefix("ui.settings."), key)
            XCTAssertTrue(key.hasSuffix(".expanded"), key)
        }
        XCTAssertEqual(SettingsSectionID.vocoders.defaultsKey, "ui.settings.vocoders.expanded")
    }

    func testEverySectionHasATitleAndSymbol() {
        for id in SettingsSectionID.allCases {
            XCTAssertFalse(id.title.isEmpty, "\(id)")
            XCTAssertFalse(id.systemImage.isEmpty, "\(id)")
        }
    }

    func testStateIsRememberedAcrossInstances() {
        let first = SettingsDisclosure(defaults: defaults)
        first.setExpanded(.recording, true)
        first.setExpanded(.account, false)

        let second = SettingsDisclosure(defaults: defaults)
        XCTAssertTrue(second.isExpanded(.recording))
        XCTAssertFalse(second.isExpanded(.account), "a closed default-open section reopened")
        XCTAssertTrue(second.isExpanded(.operatorIdentity))
    }

    func testToggleFlipsOneSection() {
        let disclosure = SettingsDisclosure(defaults: defaults)
        disclosure.toggle(.dmr)
        XCTAssertTrue(disclosure.isExpanded(.dmr))
        XCTAssertFalse(disclosure.isExpanded(.nxdn))
        disclosure.toggle(.dmr)
        XCTAssertFalse(disclosure.isExpanded(.dmr))
    }

    func testOptionToggleMovesEverySectionToTheNewState() {
        let disclosure = SettingsDisclosure(defaults: defaults)
        // A closed section ⌥-clicked open opens everything…
        disclosure.toggle(.backup, all: true)
        XCTAssertTrue(disclosure.allExpanded)
        // …and an open one ⌥-clicked shut closes everything, even the
        // sections that start open.
        disclosure.toggle(.spectrum, all: true)
        XCTAssertTrue(disclosure.expanded.isEmpty)
        XCTAssertEqual(defaults.object(forKey: "ui.settings.operator.expanded") as? Bool, false)
    }

    func testSetAll() {
        let disclosure = SettingsDisclosure(defaults: defaults)
        disclosure.setAll(true)
        XCTAssertTrue(disclosure.allExpanded)
        XCTAssertEqual(
            SettingsDisclosure(defaults: defaults).expanded.count,
            SettingsSectionID.allCases.count)
        disclosure.setAll(false)
        XCTAssertTrue(disclosure.expanded.isEmpty)
    }

    func testEveryAnchorMapsToItsSection() {
        XCTAssertEqual(SettingsAnchor.vocoders.section, .vocoders)
        XCTAssertEqual(SettingsAnchor.echolink.section, .echolink)
    }

    func testRevealOpensTheAnchorsSectionOnce() {
        let disclosure = SettingsDisclosure(defaults: defaults)
        XCTAssertTrue(disclosure.reveal(.vocoders), "a folded section has to open")
        XCTAssertTrue(disclosure.isExpanded(.vocoders))
        XCTAssertFalse(disclosure.reveal(.vocoders), "an open section needs no opening")
        XCTAssertTrue(disclosure.reveal(.echolink))
        XCTAssertTrue(disclosure.isExpanded(.echolink))
    }

    func testSectionStateIsNeverExported() {
        var prefs: [String: Any] = ["ui.showInDock": true]
        for id in SettingsSectionID.allCases { prefs[id.defaultsKey] = true }
        let slice = ConfigArchive.interfaceSlice(from: prefs)
        XCTAssertEqual(Set(slice.keys), ["ui.showInDock"])
    }

    func testSectionStateIsNeverImported() {
        for id in SettingsSectionID.allCases {
            XCTAssertFalse(ConfigArchive.isImportable(key: id.defaultsKey), id.defaultsKey)
        }
        XCTAssertTrue(ConfigArchive.isImportable(key: "ui.showInDock"))
        XCTAssertTrue(ConfigArchive.isImportable(key: "ui.quickSettingsExpanded"))
    }
}
