// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// When astar should put the user in Settings instead of the dial UI
/// (astar-4e8a).
///
/// Without an AllStarLink account the dial field is disabled outright
/// (`needsCredentials` in the popover), so opening on the call UI presents a
/// form that cannot be typed into and a Connect button that cannot be pressed.
/// A beta tester hit exactly that and reported it as "I can't enter an IP
/// address or node" — the app was working as designed and telling him nothing.
public enum FirstRunPresentation {

    /// Whether opening the main window should land on Settings.
    ///
    /// Keyed on having an account at all, not on whether it is *correct*: astar
    /// cannot know a password is wrong without a network round-trip to the
    /// portal, so the launch path can only act on "nothing configured". A
    /// password that is present but rejected is surfaced by the token test's red
    /// highlight instead (`CredentialsValidation`).
    public static func opensOnSettings(hasCredentials: Bool) -> Bool {
        !hasCredentials
    }

    /// Whether launching should raise the window at all.
    ///
    /// astar is a menu-bar app: on a fresh install a new user sees an asterisk
    /// and nothing else, with no hint that an account is needed before anything
    /// works. So the first launch with no account brings the window up.
    ///
    /// Exactly once, which is what `hasShownWelcome` is for. M17 needs no portal
    /// login, so someone can legitimately run astar forever without AllStarLink
    /// credentials — throwing the window at them on every launch would be a nag,
    /// and a nag is how a good first-run gesture turns into an annoyance people
    /// look for a setting to disable.
    public static func raisesWindowAtLaunch(hasCredentials: Bool, hasShownWelcome: Bool) -> Bool {
        !hasCredentials && !hasShownWelcome
    }
}

/// Remembers whether the first-run window has been shown, so it happens once.
public struct WelcomePreference {
    public static let key = "ui.hasShownWelcome"

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public var hasShownWelcome: Bool { defaults.bool(forKey: Self.key) }

    public func markShown() { defaults.set(true, forKey: Self.key) }
}
