// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A saved microphone characterization in the user's library. Created by
/// characterizing a mic in the Analyzer; selected per-config (each `Setup` and
/// the live state store a `micProfileID`). The built-in "Default" profile —
/// unfiltered — is represented by a `nil` selection, not a stored entry.
///
/// Holds the engine's opaque characterization JSON (noise floor + harmonic notch
/// comb from `Station.characterize`). Gain / NR / compression are NOT here — those
/// stay per-config (`Setup`) and live (`AudioSettings`).
public struct MicProfile: Codable, Equatable, Identifiable {
    /// Stable id (a UUID string); the selection key stored in configs.
    public var id: String
    /// User-facing label, e.g. "fake icom".
    public var name: String
    /// The input device this profile was characterized on (metadata / display).
    public var deviceName: String
    /// Opaque engine JSON — saved + fed back to `setMicProfile` verbatim.
    public var characterizationJSON: String
    /// When it was characterized (for a "characterized 2d ago" readout).
    public var characterizedAt: Date?

    public init(
        id: String = UUID().uuidString,
        name: String,
        deviceName: String,
        characterizationJSON: String,
        characterizedAt: Date? = nil
    ) {
        self.id = id
        self.name = name
        self.deviceName = deviceName
        self.characterizationJSON = characterizationJSON
        self.characterizedAt = characterizedAt
    }
}

extension MicProfile {
    /// Notch frequencies (Hz) this profile filters, parsed from the opaque engine
    /// JSON (`{ "notches": [{ "freq_hz", "q" }, … ] }`) for display. Empty when
    /// uncharacterized or unparseable.
    public var notchFrequencies: [Double] {
        guard let data = characterizationJSON.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let notches = obj["notches"] as? [[String: Any]]
        else { return [] }
        return notches.compactMap { ($0["freq_hz"] as? NSNumber)?.doubleValue }
    }
}

/// Persistence for the mic-profile library, keyed by profile id.
public protocol MicProfileStore {
    /// Every saved profile, in no particular order.
    func all() -> [MicProfile]
    /// The profile with `id`, or `nil`.
    func profile(id: String) -> MicProfile?
    /// Upsert by `id`.
    func save(_ profile: MicProfile)
    /// Remove the profile with `id` (no-op if absent).
    func remove(id: String)
}

/// `UserDefaults`-backed store. Persists all profiles as a single JSON dictionary
/// (`[id: MicProfile]`) under one key — the set is tiny.
public final class UserDefaultsMicProfileStore: MicProfileStore {
    private let defaults: UserDefaults
    private let key = "audio.micProfiles"

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func all() -> [MicProfile] {
        Array(profiles().values)
    }

    public func profile(id: String) -> MicProfile? {
        profiles()[id]
    }

    public func save(_ profile: MicProfile) {
        var map = profiles()
        map[profile.id] = profile
        write(map)
    }

    public func remove(id: String) {
        var map = profiles()
        map.removeValue(forKey: id)
        write(map)
    }

    // MARK: - JSON map helpers

    private func profiles() -> [String: MicProfile] {
        guard let data = defaults.data(forKey: key),
            let map = try? JSONDecoder().decode([String: MicProfile].self, from: data)
        else { return [:] }
        return map
    }

    private func write(_ map: [String: MicProfile]) {
        guard let data = try? JSONEncoder().encode(map) else { return }
        defaults.set(data, forKey: key)
    }
}
