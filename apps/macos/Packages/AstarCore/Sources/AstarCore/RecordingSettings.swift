// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// How and where the app records a call. Saved in `UserDefaults` under the
/// `recording.` keys, which no `.astarconfig` section carries: the folder is
/// a path on this Mac and means nothing on another one.
public struct RecordingSettings: Equatable {
    /// Stereo (receive left, transmit right), two mono files, or one mixed
    /// file.
    public var layout: RecordingLayout
    /// Start a recording by itself whenever a call is answered.
    public var alwaysRecord: Bool
    /// Also keep the codec frames (D-Star, M17, Fusion, NXDN, DMR) under the
    /// folder's `captured/`, for unamblify.
    public var saveCodecFrames: Bool
    /// Where the files go. Created on the first recording if missing.
    public var folder: URL
    /// Show the capture button. Nothing is captured until it's pressed. Off
    /// by default; kept on this Mac, never in an `.astarconfig`.
    public var debugMode: Bool = false
    /// Where the packet logs go.
    public var packetLogFolder: URL = RecordingSettings.defaultPacketLogFolder
    /// The most the packet logs may take up in total; the oldest go first.
    public var packetLogMaxBytes: Int64 = 1_000_000_000

    /// The sizes Settings offers, in bytes.
    public static let packetLogLimits: [Int64] = [
        250_000_000, 500_000_000, 1_000_000_000, 2_000_000_000, 5_000_000_000,
    ]

    /// `~/Music/astar Recordings/Packet Logs`.
    public static var defaultPacketLogFolder: URL {
        defaultFolder.appendingPathComponent("Packet Logs", isDirectory: true)
    }

    /// `~/Music/astar Recordings`.
    public static var defaultFolder: URL {
        URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
            .appendingPathComponent("Music", isDirectory: true)
            .appendingPathComponent("astar Recordings", isDirectory: true)
    }

    public init(
        layout: RecordingLayout = .stereo, alwaysRecord: Bool = false,
        saveCodecFrames: Bool = false, folder: URL = RecordingSettings.defaultFolder
    ) {
        self.layout = layout
        self.alwaysRecord = alwaysRecord
        self.saveCodecFrames = saveCodecFrames
        self.folder = folder
    }
}

public protocol RecordingSettingsStore {
    func load() -> RecordingSettings
    func save(_ settings: RecordingSettings)
}

/// `UserDefaults`-backed store. Anything absent, or a layout number this
/// build does not know, reads as the default.
public final class UserDefaultsRecordingSettingsStore: RecordingSettingsStore {
    private enum Key {
        static let layout = "recording.layout"
        static let always = "recording.always"
        static let frames = "recording.frames"
        static let folder = "recording.folder"
        static let debugMode = "recording.packetLogDebugMode"
        static let packetLogFolder = "recording.packetLogFolder"
        static let packetLogMaxBytes = "recording.packetLogMaxBytes"
    }

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func load() -> RecordingSettings {
        var settings = RecordingSettings()
        let storedLayout = Int32(clamping: defaults.integer(forKey: Key.layout))
        if defaults.object(forKey: Key.layout) != nil,
            let layout = RecordingLayout(rawValue: storedLayout)
        {
            settings.layout = layout
        }
        settings.alwaysRecord = defaults.bool(forKey: Key.always)
        settings.saveCodecFrames = defaults.bool(forKey: Key.frames)
        if let path = defaults.string(forKey: Key.folder), !path.isEmpty {
            settings.folder = URL(fileURLWithPath: path, isDirectory: true)
        }
        settings.debugMode = defaults.bool(forKey: Key.debugMode)
        if let path = defaults.string(forKey: Key.packetLogFolder), !path.isEmpty {
            settings.packetLogFolder = URL(fileURLWithPath: path, isDirectory: true)
        }
        // A size this build does not offer reads as the default.
        if let stored = defaults.object(forKey: Key.packetLogMaxBytes) as? NSNumber,
            RecordingSettings.packetLogLimits.contains(stored.int64Value)
        {
            settings.packetLogMaxBytes = stored.int64Value
        }
        return settings
    }

    public func save(_ settings: RecordingSettings) {
        defaults.set(Int(settings.layout.rawValue), forKey: Key.layout)
        defaults.set(settings.alwaysRecord, forKey: Key.always)
        defaults.set(settings.saveCodecFrames, forKey: Key.frames)
        defaults.set(settings.folder.path, forKey: Key.folder)
        defaults.set(settings.debugMode, forKey: Key.debugMode)
        defaults.set(settings.packetLogFolder.path, forKey: Key.packetLogFolder)
        defaults.set(settings.packetLogMaxBytes, forKey: Key.packetLogMaxBytes)
    }
}
