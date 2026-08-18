// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// When astar should put the user in Settings rather than the dial UI
/// (astar-4e8a).
final class FirstRunPresentationTests: XCTestCase {

    // MARK: - Which pane the window opens on

    /// No account configured: the dial field is disabled and nothing can be
    /// typed into it, so opening on the call UI shows a dead form. Open on the
    /// thing the user has to fill in.
    func testWindowOpensOnSettingsWithNoAccount() {
        XCTAssertTrue(FirstRunPresentation.opensOnSettings(hasCredentials: false))
    }

    /// Configured: never hijack the window. Opening on Settings every time would
    /// be a nag for anyone who is set up and just wants to dial.
    func testWindowOpensOnTheCallUIOnceConfigured() {
        XCTAssertFalse(FirstRunPresentation.opensOnSettings(hasCredentials: true))
    }

    // MARK: - Whether to raise the window at launch

    /// A genuine first launch with nothing configured: bring the window up on
    /// Settings. astar is a menu-bar app, so without this a new user sees only
    /// an asterisk and has to guess.
    func testFirstLaunchWithNoAccountRaisesTheWindow() {
        XCTAssertTrue(
            FirstRunPresentation.raisesWindowAtLaunch(
                hasCredentials: false, hasShownWelcome: false))
    }

    /// Only once. Someone who deliberately runs without an AllStarLink account —
    /// M17 needs no portal login — must not have the window thrown at them on
    /// every single launch.
    func testWindowIsNotRaisedAgainAfterTheFirstTime() {
        XCTAssertFalse(
            FirstRunPresentation.raisesWindowAtLaunch(
                hasCredentials: false, hasShownWelcome: true))
    }

    /// A configured user is never interrupted, welcome flag or not.
    func testConfiguredLaunchNeverRaisesTheWindow() {
        for seen in [true, false] {
            XCTAssertFalse(
                FirstRunPresentation.raisesWindowAtLaunch(
                    hasCredentials: true, hasShownWelcome: seen),
                "a configured account must launch straight to the menu bar")
        }
    }

    // MARK: - The welcome flag

    func testWelcomeFlagDefaultsToUnseenAndPersists() {
        let suite = "astar.tests.firstrun"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        defer { defaults.removePersistentDomain(forName: suite) }

        let pref = WelcomePreference(defaults)
        XCTAssertFalse(pref.hasShownWelcome)
        pref.markShown()
        XCTAssertTrue(pref.hasShownWelcome)
        XCTAssertTrue(WelcomePreference(defaults).hasShownWelcome, "must survive a fresh read")
    }
}
