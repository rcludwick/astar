// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Whether astar shows up in the Dock and Cmd-Tab, or stays a menu-bar-only
/// accessory (astar-7c31). Platform-neutral so it can be unit-tested and so
/// AstarCore keeps building for iOS; the macOS `AppDelegate` maps it to an
/// `NSApplication.ActivationPolicy`, which does not exist on iOS.
public enum DockPresence: Equatable {
    /// Dock icon + Cmd-Tab entry, alongside the menu-bar item (`.regular`).
    case dock
    /// Menu-bar item only — no Dock icon, no Cmd-Tab entry (`.accessory`).
    case menuBarOnly

    public init(showInDock: Bool) {
        self = showInDock ? .dock : .menuBarOnly
    }

    public var showsInDock: Bool { self == .dock }

    /// What to do when the user clicks the Dock icon. **Show, never toggle**: a
    /// second Dock click hiding the window is behaviour no Mac app has, and with
    /// no visible window a Dock click that does nothing reads as a hung app.
    public static func shouldShowWindowOnReopen(hasVisibleWindows: Bool) -> Bool {
        !hasVisibleWindows
    }
}

/// Persistence for the "Show in Dock" preference. Follows
/// `UserDefaultsAudioSettingsStore`'s injection style — but without a protocol:
/// one boolean with one consumer does not earn an abstraction.
public struct DockPreference {
    /// Global UserDefaults key, in the same `ui.` namespace as the spectrum
    /// decay preference.
    public static let key = "ui.showInDock"

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    /// Read the persisted value, defaulting to **on** when the key is absent
    /// (first launch → `bool(forKey:)` would return `false`, which is the wrong
    /// default here). A Dock icon is what a Mac user expects of an app with a
    /// window; anyone who wants menu-bar-only turns it off once and it sticks.
    public func load() -> Bool {
        defaults.object(forKey: Self.key) == nil ? true : defaults.bool(forKey: Self.key)
    }

    public func save(_ showInDock: Bool) {
        defaults.set(showInDock, forKey: Self.key)
    }
}
