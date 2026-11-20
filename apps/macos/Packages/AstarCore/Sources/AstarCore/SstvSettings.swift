// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Whether the app decodes SSTV pictures out of what it hears, and where it
/// saves them. Saved in `UserDefaults` under the `sstv.` keys, which are
/// machine-local (`ConfigArchive.machineLocalPrefixes`): never exported in
/// an `.astarconfig`, and refused on import, because the folder is a path on
/// this Mac and means nothing on another one.
public struct SstvSettings: Equatable {
    /// Decode pictures. On by default: decoding only reads audio already
    /// heard, and never sends anything.
    public var enabled: Bool
    /// Where the PNGs and their `pictures.jsonl` index go. Created by the
    /// engine if missing.
    public var folder: URL

    /// `~/Pictures/astar SSTV`.
    public static var defaultFolder: URL {
        URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
            .appendingPathComponent("Pictures", isDirectory: true)
            .appendingPathComponent("astar SSTV", isDirectory: true)
    }

    public init(enabled: Bool = true, folder: URL = SstvSettings.defaultFolder) {
        self.enabled = enabled
        self.folder = folder
    }
}

public protocol SstvSettingsStore {
    func load() -> SstvSettings
    func save(_ settings: SstvSettings)
}

/// `UserDefaults`-backed store. Anything absent reads as the default, so a
/// Mac that never saw this setting decodes pictures into `~/Pictures`.
public final class UserDefaultsSstvSettingsStore: SstvSettingsStore {
    private enum Key {
        static let enabled = "sstv.enabled"
        static let folder = "sstv.folder"
    }

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func load() -> SstvSettings {
        var settings = SstvSettings()
        if let enabled = defaults.object(forKey: Key.enabled) as? Bool {
            settings.enabled = enabled
        }
        if let path = defaults.string(forKey: Key.folder), !path.isEmpty {
            settings.folder = URL(fileURLWithPath: path, isDirectory: true)
        }
        return settings
    }

    public func save(_ settings: SstvSettings) {
        defaults.set(settings.enabled, forKey: Key.enabled)
        defaults.set(settings.folder.path, forKey: Key.folder)
    }
}
