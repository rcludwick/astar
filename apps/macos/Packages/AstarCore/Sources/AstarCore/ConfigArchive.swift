// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// One selectable part of an exported configuration (astar-b52e).
///
/// Sections partition the settings — no key belongs to two — so the export
/// sheet's checkboxes compose without overlap and an import can apply exactly
/// what the user ticked.
public enum ConfigSection: String, CaseIterable, Codable, Sendable {
    /// Saved configs and the mic profiles they reference.
    case rigs
    /// Favorites and recents.
    case directory
    /// Audio, M17 audio overrides, and serial/PTT settings.
    case settings
    /// The operator's callsign and radio IDs — separated out because they
    /// identify the operator, so a shareable export can leave them behind.
    ///
    /// The raw value stays `callsign` even though the section now carries
    /// three fields: it is written into every exported file, and renaming it would
    /// make v1 archives unreadable to buy nothing. See `ConfigVersion` — a
    /// bump marks translation, not change.
    case callsign
    /// Window and panel state.
    case interface

    /// Checkbox label.
    public var title: String {
        switch self {
        case .rigs: return "Saved configs"
        case .directory: return "Node directory"
        case .settings: return "Audio and serial settings"
        case .callsign: return "Callsign and radio IDs"
        case .interface: return "Window and panel state"
        }
    }

    /// One line of "what am I actually ticking".
    public var detail: String {
        switch self {
        case .rigs: return "Your rigs and their mic profiles"
        case .directory: return "Favorites and recents"
        case .settings: return "Devices, gains, VOX, compression, serial PTT"
        case .callsign: return "Identifies you — leave off to share this file"
        case .interface: return "Which panels are open, Dock icon, network"
        }
    }
}

/// A scalar preference value, encoded as its natural JSON type.
///
/// A raw `UserDefaults` copy would round-trip through plist and land base64'd
/// and unreadable; an exported config is a file people open and read, so the
/// four types the app actually stores get named cases instead.
public enum SettingValue: Codable, Equatable, Sendable {
    case bool(Bool)
    case int(Int)
    case double(Double)
    case string(String)

    /// Wraps a `UserDefaults` value, or `nil` if it is not a scalar astar stores.
    ///
    /// `Bool` must be tested before the numeric cases: `UserDefaults` bridges
    /// booleans to `NSNumber`, so an `as? Int` would match first and silently
    /// turn every toggle into `1`.
    public init?(defaultsValue: Any) {
        if let number = defaultsValue as? NSNumber {
            if CFGetTypeID(number) == CFBooleanGetTypeID() {
                self = .bool(number.boolValue)
            } else if CFNumberIsFloatType(number) {
                self = .double(number.doubleValue)
            } else {
                self = .int(number.intValue)
            }
            return
        }
        if let string = defaultsValue as? String {
            self = .string(string)
            return
        }
        return nil
    }

    /// The value to hand back to `UserDefaults.set(_:forKey:)`.
    public var defaultsValue: Any {
        switch self {
        case .bool(let v): return v
        case .int(let v): return v
        case .double(let v): return v
        case .string(let v): return v
        }
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        // Bool first, for the same reason as above.
        if let v = try? c.decode(Bool.self) {
            self = .bool(v)
        } else if let v = try? c.decode(
            Int.self)
        {
            self = .int(v)
        } else if let v = try? c.decode(Double.self) {
            self = .double(v)
        } else {
            self = .string(try c.decode(String.self))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .bool(let v): try c.encode(v)
        case .int(let v): try c.encode(v)
        case .double(let v): try c.encode(v)
        case .string(let v): try c.encode(v)
        }
    }
}

/// A portable snapshot of the app's configuration.
///
/// **Nothing from the Keychain is ever written here.** The portal password,
/// username and node stay in the Keychain; an import leaves existing
/// credentials alone and the user re-enters the account on the new Mac. That
/// keeps an exported file safe to hand to someone regardless of which sections
/// were ticked (bar `callsign`, which is why it is its own checkbox).
public struct ConfigArchive: Codable, Equatable {
    /// The config version this build writes. See `ConfigVersion` for the rule
    /// that governs when it moves — it marks translation, not change.
    public static let currentVersion = ConfigVersion.current

    public var version: Int
    public var exportedAt: Date
    public var appVersion: String?

    public var rigs: Rigs?
    public var directory: [NodeEntry]?
    public var settings: [String: SettingValue]?
    public var callsign: String?
    /// The operator's DMR radio ID. Travels with the callsign because it is
    /// the same kind of fact — who you are, not what you own — and so is
    /// withheld by the same checkbox.
    public var radioID: String?
    /// The operator's NXDN id. A separate number from `radioID`, because NXDN
    /// ids are 16-bit and a registered DMR ID does not fit in one — see
    /// `NxdnID`. Travels with the callsign for the same reason `radioID`
    /// does, and is withheld by the same checkbox.
    ///
    /// Adding it does NOT move `ConfigVersion`: a v1 reader ignores a key it
    /// does not know, and this reader treats an absent one as unset.
    public var nxdnRadioID: String?
    public var interface: [String: SettingValue]?

    /// Saved configs travel with their mic profiles: a `Setup` references a
    /// profile by id, so importing one without the other yields a config that
    /// points at nothing.
    public struct Rigs: Codable, Equatable {
        public var setups: [Setup]
        public var micProfiles: [MicProfile]
        public var defaultSetupID: String?
        public var selectedSetupID: String?

        public init(
            setups: [Setup], micProfiles: [MicProfile],
            defaultSetupID: String? = nil, selectedSetupID: String? = nil
        ) {
            self.setups = setups
            self.micProfiles = micProfiles
            self.defaultSetupID = defaultSetupID
            self.selectedSetupID = selectedSetupID
        }
    }

    /// Everything an export reads, gathered so the builder stays pure and the
    /// tests need no `UserDefaults`.
    public struct Sources {
        public var defaults: [String: Any]
        public var setups: [Setup]
        public var micProfiles: [MicProfile]
        public var selectedSetupID: String?
        public var defaultSetupID: String?
        public var directory: [NodeEntry]
        public var callsign: String?
        public var radioID: String?
        public var nxdnRadioID: String?

        public init(
            defaults: [String: Any], setups: [Setup], micProfiles: [MicProfile],
            selectedSetupID: String?, defaultSetupID: String?,
            directory: [NodeEntry], callsign: String?, radioID: String? = nil,
            nxdnRadioID: String? = nil
        ) {
            self.defaults = defaults
            self.setups = setups
            self.micProfiles = micProfiles
            self.selectedSetupID = selectedSetupID
            self.defaultSetupID = defaultSetupID
            self.directory = directory
            self.callsign = callsign
            self.radioID = radioID
            self.nxdnRadioID = nxdnRadioID
        }
    }

    // MARK: - Which keys belong where

    /// Keys carried by a typed section, so they must never also appear in a
    /// scalar slice.
    private static let claimedElsewhere: Set<String> = [
        "audio.setups", "audio.micProfiles", "audio.selectedSetup", "audio.defaultSetup",
        "directory.nodes", "m17.callsign", "dmr.radioId", "nxdn.radioId",
    ]

    /// `audio.wideband` is a documented dead key (astar-e542). Exporting cruft
    /// would propagate it into every future import.
    private static let dead: Set<String> = ["audio.wideband"]

    /// Prefix-driven rather than an enumerated list, so a preference added
    /// later is picked up without anyone remembering to update this file.
    private static let settingsPrefixes = ["audio.", "m17.", "serial."]
    private static let interfacePrefixes = ["ui."]

    /// Anything whose key hints at a secret is refused even though credentials
    /// live in the Keychain and cannot reach the defaults domain today. Cheap
    /// insurance against a future key that does.
    private static let secretHints = ["pass", "secret", "token", "credential"]

    private static func slice(from defaults: [String: Any], prefixes: [String])
        -> [String: SettingValue]
    {
        var out = [String: SettingValue]()
        for (key, raw) in defaults {
            guard prefixes.contains(where: { key.hasPrefix($0) }) else { continue }
            guard !claimedElsewhere.contains(key), !dead.contains(key) else { continue }
            let lowered = key.lowercased()
            guard !secretHints.contains(where: { lowered.contains($0) }) else { continue }
            guard let value = SettingValue(defaultsValue: raw) else { continue }
            out[key] = value
        }
        return out
    }

    /// Audio, M17 audio overrides and serial settings.
    public static func settingsSlice(from defaults: [String: Any]) -> [String: SettingValue] {
        slice(from: defaults, prefixes: settingsPrefixes)
    }

    /// Window and panel state.
    public static func interfaceSlice(from defaults: [String: Any]) -> [String: SettingValue] {
        slice(from: defaults, prefixes: interfacePrefixes)
    }

    // MARK: - Building

    /// Build an archive holding exactly the chosen sections.
    public static func make(
        sections: Set<ConfigSection>, from sources: Sources,
        exportedAt: Date = Date(), appVersion: String? = nil
    ) -> ConfigArchive {
        ConfigArchive(
            version: currentVersion,
            exportedAt: exportedAt,
            appVersion: appVersion,
            rigs: sections.contains(.rigs)
                ? Rigs(
                    setups: sources.setups, micProfiles: sources.micProfiles,
                    defaultSetupID: sources.defaultSetupID,
                    selectedSetupID: sources.selectedSetupID)
                : nil,
            directory: sections.contains(.directory) ? sources.directory : nil,
            settings: sections.contains(.settings) ? settingsSlice(from: sources.defaults) : nil,
            callsign: sections.contains(.callsign) ? sources.callsign : nil,
            radioID: sections.contains(.callsign) ? sources.radioID : nil,
            nxdnRadioID: sections.contains(.callsign) ? sources.nxdnRadioID : nil,
            interface: sections.contains(.interface)
                ? interfaceSlice(from: sources.defaults) : nil)
    }

    /// A copy carrying only `sections` — the import chooser's filter.
    ///
    /// Intersects rather than sets: ticking a section a file does not contain
    /// cannot conjure an empty one. That matters because an empty section is
    /// not the same as an absent one downstream — `settings: [:]` would report
    /// "0 settings applied" where `nil` correctly reports nothing at all.
    public func filtered(to sections: Set<ConfigSection>) -> ConfigArchive {
        ConfigArchive(
            version: version,
            exportedAt: exportedAt,
            appVersion: appVersion,
            rigs: sections.contains(.rigs) ? rigs : nil,
            directory: sections.contains(.directory) ? directory : nil,
            settings: sections.contains(.settings) ? settings : nil,
            callsign: sections.contains(.callsign) ? callsign : nil,
            radioID: sections.contains(.callsign) ? radioID : nil,
            nxdnRadioID: sections.contains(.callsign) ? nxdnRadioID : nil,
            interface: sections.contains(.interface) ? interface : nil)
    }

    /// Sections this archive actually carries.
    public var presentSections: Set<ConfigSection> {
        var out = Set<ConfigSection>()
        if rigs != nil { out.insert(.rigs) }
        if directory != nil { out.insert(.directory) }
        if settings != nil { out.insert(.settings) }
        // Any one field alone is enough: an operator with a radio ID and no
        // callsign (or the reverse) still exported the identity section.
        if callsign != nil || radioID != nil || nxdnRadioID != nil { out.insert(.callsign) }
        if interface != nil { out.insert(.interface) }
        return out
    }

    // MARK: - File format

    public enum ArchiveError: LocalizedError, Equatable {
        case notAnAstarConfig
        case newerFormat(found: Int, supported: Int)

        public var errorDescription: String? {
            switch self {
            case .notAnAstarConfig:
                return "That file isn’t an astar configuration."
            case .newerFormat(let found, let supported):
                return
                    "That configuration was written by a newer version of astar "
                    + "(format \(found); this build reads \(supported)). Update astar and try again."
            }
        }
    }

    public static func encode(_ archive: ConfigArchive) throws -> Data {
        let encoder = JSONEncoder()
        // Readable and stable: an exported file is meant to be opened, diffed
        // and mailed, and sorted keys keep two exports of the same config
        // byte-identical.
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        encoder.dateEncodingStrategy = .iso8601
        return try encoder.encode(archive)
    }

    public static func decode(_ data: Data) throws -> ConfigArchive {
        // Read the version BEFORE the full decode. A newer archive may carry
        // fields and encodings this build cannot parse, and failing that decode
        // would report "not an astar configuration" for a file that plainly is
        // one — sending the user looking for the wrong problem.
        if let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let version = object["version"] as? Int,
            version > currentVersion
        {
            throw ArchiveError.newerFormat(found: version, supported: currentVersion)
        }

        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        do {
            return try decoder.decode(ConfigArchive.self, from: data)
        } catch {
            throw ArchiveError.notAnAstarConfig
        }
    }
}
