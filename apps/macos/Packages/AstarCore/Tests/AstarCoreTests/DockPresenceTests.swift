// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class DockPresenceTests: XCTestCase {
    private func freshDefaults() -> UserDefaults {
        // A throwaway suite so tests don't touch the real app defaults.
        UserDefaults(
            suiteName: "astar.tests.dock.\(ProcessInfo.processInfo.globallyUniqueString)")!
    }

    // The default-ON contract. A Dock icon is what a Mac user expects of an app
    // with a window; this is exactly the kind of default that regresses silently,
    // which is why it gets its own test (cf. testDefaultTransportIsUsb).
    func testDefaultsToShowingInDock() {
        XCTAssertTrue(DockPreference(freshDefaults()).load())
    }

    func testSavedFalseReadsBackFalse() {
        let defaults = freshDefaults()
        let pref = DockPreference(defaults)
        pref.save(false)
        XCTAssertFalse(pref.load())
    }

    func testSavedTrueReadsBackTrue() {
        let defaults = freshDefaults()
        let pref = DockPreference(defaults)
        pref.save(false)
        pref.save(true)
        XCTAssertTrue(pref.load())
    }

    // A second store over the same suite sees the write — the menu toggle and
    // the launch path each build their own DockPreference.
    func testPersistsAcrossStores() {
        let defaults = freshDefaults()
        DockPreference(defaults).save(false)
        XCTAssertFalse(DockPreference(defaults).load())
    }

    func testPresenceMapsBothDirections() {
        XCTAssertEqual(DockPresence(showInDock: true), .dock)
        XCTAssertEqual(DockPresence(showInDock: false), .menuBarOnly)
        XCTAssertTrue(DockPresence.dock.showsInDock)
        XCTAssertFalse(DockPresence.menuBarOnly.showsInDock)
    }

    // Clicking the Dock icon with nothing on screen must open the window —
    // doing nothing reads as a hung app.
    func testReopenShowsWindowWhenNoneVisible() {
        XCTAssertTrue(DockPresence.shouldShowWindowOnReopen(hasVisibleWindows: false))
    }

    // A window is already up: leave it alone. This must never toggle it closed.
    func testReopenLeavesVisibleWindowAlone() {
        XCTAssertFalse(DockPresence.shouldShowWindowOnReopen(hasVisibleWindows: true))
    }
}
