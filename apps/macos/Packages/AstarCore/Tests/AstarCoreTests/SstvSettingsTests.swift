// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class SstvSettingsTests: XCTestCase {
    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.sstv.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    func testDefaultsAreOnAndPictures() {
        let stored = UserDefaultsSstvSettingsStore(scratchDefaults()).load()
        for settings in [SstvSettings(), stored] {
            XCTAssertTrue(settings.enabled, "decoding is on by default")
            XCTAssertEqual(settings.folder.path, NSHomeDirectory() + "/Pictures/astar SSTV")
        }
    }

    func testFolderRoundTrips() {
        let defaults = scratchDefaults()
        let chosen = SstvSettings(
            enabled: false, folder: URL(fileURLWithPath: "/tmp/sstv pics", isDirectory: true))
        UserDefaultsSstvSettingsStore(defaults).save(chosen)

        XCTAssertEqual(UserDefaultsSstvSettingsStore(defaults).load(), chosen)
        XCTAssertEqual(defaults.object(forKey: "sstv.enabled") as? Bool, false)
        XCTAssertEqual(defaults.string(forKey: "sstv.folder"), "/tmp/sstv pics")
    }
}
