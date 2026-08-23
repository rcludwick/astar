// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-b52e — the config version, and the rule that governs when it moves.
///
/// It marks **translation**, not change. Adding a field or a whole section
/// leaves it alone, because an old reader ignores what it does not know and a
/// new reader tolerates what is absent. It goes up only when existing data
/// would be misread without being rewritten.
final class ConfigVersionTests: XCTestCase {

    private func suite() -> (UserDefaults, String) {
        let name = "astar.test.version.\(UUID().uuidString)"
        return (UserDefaults(suiteName: name)!, name)
    }

    func testTheCurrentVersionIsOne() {
        XCTAssertEqual(ConfigVersion.current, 1)
    }

    func testTheArchiveStampsTheSameVersionAsThePreferences() {
        // One version, two homes. If these ever disagree, a file and the Mac
        // that wrote it would claim different schemas.
        XCTAssertEqual(ConfigArchive.currentVersion, ConfigVersion.current)
    }

    func testStampingRecordsTheVersion() {
        let (d, name) = suite()
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }
        XCTAssertNil(d.object(forKey: ConfigVersion.defaultsKey))
        ConfigVersion.stamp(d)
        XCTAssertEqual(d.integer(forKey: ConfigVersion.defaultsKey), ConfigVersion.current)
    }

    func testStampingIsIdempotent() {
        let (d, name) = suite()
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }
        ConfigVersion.stamp(d)
        ConfigVersion.stamp(d)
        XCTAssertEqual(d.integer(forKey: ConfigVersion.defaultsKey), ConfigVersion.current)
    }

    func testUnstampedPreferencesReadAsVersionOne() {
        // Everything written before the version existed IS version 1 — that is
        // what version 1 describes. Reporting 0 would invent a migration.
        let (d, name) = suite()
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }
        XCTAssertEqual(ConfigVersion.installed(in: d), 1)
    }

    func testAStampedVersionIsReadBack() {
        let (d, name) = suite()
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }
        d.set(7, forKey: ConfigVersion.defaultsKey)
        XCTAssertEqual(ConfigVersion.installed(in: d), 7)
    }

    func testTheVersionKeyIsNotSweptIntoTheSettingsSection() {
        // It belongs to the envelope, which already carries it. Exporting it as
        // a setting would let an import overwrite the reader's own version.
        let slice = ConfigArchive.settingsSlice(from: [ConfigVersion.defaultsKey: 1])
        XCTAssertTrue(slice.isEmpty, "version leaked into settings: \(slice)")
        let ui = ConfigArchive.interfaceSlice(from: [ConfigVersion.defaultsKey: 1])
        XCTAssertTrue(ui.isEmpty)
    }

    // MARK: - What the version is FOR

    func testAFileFromAnOlderVersionIsStillAccepted() throws {
        // The whole point: version 1 files must keep importing after version 2
        // exists. Refusing them would make the version a wall, not a hinge.
        var archive = ConfigArchive(
            version: 1, exportedAt: Date(), appVersion: "0.1.4beta",
            rigs: nil, directory: [], settings: nil, callsign: nil, interface: nil)
        archive.version = 1
        let data = try ConfigArchive.encode(archive)
        XCTAssertNoThrow(try ConfigArchive.decode(data))
    }

    func testAFileFromANewerVersionIsRefusedWithAUsefulReason() throws {
        var archive = ConfigArchive(
            version: 1, exportedAt: Date(), appVersion: nil,
            rigs: nil, directory: [], settings: nil, callsign: nil, interface: nil)
        archive.version = ConfigVersion.current + 1
        let data = try JSONEncoder().encode(archive)
        XCTAssertThrowsError(try ConfigArchive.decode(data)) { error in
            let text = "\(error.localizedDescription)".lowercased()
            XCTAssertTrue(text.contains("newer"), "unhelpful: \(text)")
        }
    }
}
