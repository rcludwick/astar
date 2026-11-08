// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

final class RecordingSettingsTests: XCTestCase {
    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.recording.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private var expectedFolder: URL {
        URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
            .appendingPathComponent("Music", isDirectory: true)
            .appendingPathComponent("astar Recordings", isDirectory: true)
    }

    func testDefaults() {
        let stored = UserDefaultsRecordingSettingsStore(scratchDefaults()).load()
        for settings in [RecordingSettings(), stored] {
            XCTAssertEqual(settings.layout, .stereo)
            XCTAssertFalse(settings.alwaysRecord)
            XCTAssertFalse(settings.saveCodecFrames)
            XCTAssertEqual(settings.folder.standardizedFileURL, expectedFolder.standardizedFileURL)
            XCTAssertEqual(settings.folder.path, NSHomeDirectory() + "/Music/astar Recordings")
        }
    }

    func testRoundTrip() {
        let defaults = scratchDefaults()
        let store = UserDefaultsRecordingSettingsStore(defaults)
        let chosen = RecordingSettings(
            layout: .twoMono, alwaysRecord: true, saveCodecFrames: true,
            folder: URL(fileURLWithPath: "/tmp/somewhere else", isDirectory: true))
        store.save(chosen)

        let loaded = UserDefaultsRecordingSettingsStore(defaults).load()
        XCTAssertEqual(loaded, chosen)
        XCTAssertEqual(defaults.integer(forKey: "recording.layout"), 1)
        XCTAssertTrue(defaults.bool(forKey: "recording.always"))
        XCTAssertTrue(defaults.bool(forKey: "recording.frames"))
        XCTAssertEqual(defaults.string(forKey: "recording.folder"), "/tmp/somewhere else")
    }

    func testAnUnknownLayoutReadsAsStereo() {
        let defaults = scratchDefaults()
        defaults.set(9, forKey: "recording.layout")
        XCTAssertEqual(UserDefaultsRecordingSettingsStore(defaults).load().layout, .stereo)
    }

    /// The recording settings are this Mac's, not a station's: a folder path
    /// means nothing on another machine. So no `.astarconfig` section carries
    /// them, and adding them needed no `ConfigVersion` bump.
    func testRecordingKeysStayOutOfConfigExports() {
        let defaults: [String: Any] = [
            "recording.layout": 2, "recording.always": true, "recording.frames": true,
            "recording.folder": "/Users/someone/Music/astar Recordings",
        ]
        XCTAssertTrue(ConfigArchive.settingsSlice(from: defaults).isEmpty)
        XCTAssertTrue(ConfigArchive.interfaceSlice(from: defaults).isEmpty)
        XCTAssertEqual(ConfigVersion.current, 1)
    }

    func testPacketLogDefaultsToOff() {
        let stored = UserDefaultsRecordingSettingsStore(scratchDefaults()).load()
        for settings in [RecordingSettings(), stored] {
            XCTAssertFalse(settings.debugMode)
            XCTAssertEqual(settings.packetLogFolder, RecordingSettings.defaultPacketLogFolder)
            XCTAssertEqual(
                RecordingSettings.defaultPacketLogFolder.path,
                NSHomeDirectory() + "/Music/astar Recordings/Packet Logs")
            XCTAssertEqual(settings.packetLogMaxBytes, 1_000_000_000)
        }
        XCTAssertTrue(
            RecordingSettings.packetLogLimits.contains(RecordingSettings().packetLogMaxBytes))
    }

    func testPacketLogSettingsRoundTrip() {
        let defaults = scratchDefaults()
        let store = UserDefaultsRecordingSettingsStore(defaults)
        var chosen = RecordingSettings()
        chosen.debugMode = true
        chosen.packetLogFolder = URL(fileURLWithPath: "/tmp/pkts", isDirectory: true)
        chosen.packetLogMaxBytes = 5_000_000_000
        store.save(chosen)
        XCTAssertEqual(defaults.bool(forKey: "recording.packetLogDebugMode"), true)
        XCTAssertEqual(defaults.string(forKey: "recording.packetLogFolder"), "/tmp/pkts")
        XCTAssertEqual(
            defaults.object(forKey: "recording.packetLogMaxBytes") as? Int64, 5_000_000_000)
        let loaded = UserDefaultsRecordingSettingsStore(defaults).load()
        XCTAssertEqual(loaded, chosen)
    }

    func testAnUnknownLimitFallsBackToTheDefault() {
        let defaults = scratchDefaults()
        defaults.set(Int64(123_456), forKey: "recording.packetLogMaxBytes")
        XCTAssertEqual(
            UserDefaultsRecordingSettingsStore(defaults).load().packetLogMaxBytes, 1_000_000_000)
    }
}
