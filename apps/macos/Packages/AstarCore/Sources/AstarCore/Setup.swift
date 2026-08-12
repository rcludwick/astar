// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A named rig you switch to in one action: a hardware profile (serial preset /
/// none) bundled with the input + output devices to use. Lets the user flip
/// their whole setup at once — "UCI150 desk" ↔ "Jabra mobile" — instead of
/// reconfiguring serial and re-picking two devices by hand.
///
/// The per-mic `MicProfile` is **not** copied in here; it follows `inputDevice`
/// by name, so switching the Setup also restores that mic's gain/processing.
public struct Setup: Codable, Equatable, Identifiable {
    /// Stable id (the app generates a UUID string). Used for upsert/select.
    public var id: String
    /// User-facing label, e.g. "UCI150 desk", "Jabra mobile".
    public var name: String
    /// `HardwareProfileRegistry` id: `uci150` / `headset` / `custom`.
    public var hardwareProfileID: String
    /// Capture device name; `nil` = system default.
    public var inputDevice: String?
    /// Playback device name; `nil` = system default.
    public var outputDevice: String?
    /// Tuned mic (input) gain for this rig; `nil` = don't override on apply.
    /// Optional so setups stored before gains existed still decode.
    public var inputGain: Float?
    /// Tuned speaker (output) volume for this rig; `nil` = don't override.
    public var outputGain: Float?
    /// Mic voice compression for this rig; `nil` = don't override.
    public var compression: Bool?
    /// Mic compression strength (0…1) for this rig; `nil` = don't override.
    public var compressionLevel: Float?
    /// TX trim (post-compression output gain, UI "TX Volume") for this rig;
    /// `nil` = don't override. Inherently per-mic, so it belongs to the rig.
    public var txTrim: Float?
    /// Mic noise reduction for this rig; `nil` = don't override.
    public var noiseReduction: Bool?
    /// Voice-activated PTT for this rig; `nil` = don't override.
    public var voxEnabled: Bool?
    /// VOX trigger level (dBFS) for this rig; `nil` = don't override.
    public var voxThreshold: Float?
    /// Full-duplex audio for this rig; `nil` = don't override (defaults half).
    public var fullDuplex: Bool?
    /// Per-config serial line settings (for a serial radio like the UCI150).
    /// `nil` = use the `hardwareProfileID` preset / no serial.
    public var serial: SerialLineSpec?
    /// Selected mic-profile id for this config. `nil` = the built-in Default
    /// profile (unfiltered). Switching to this config applies this profile.
    public var micProfileID: String?

    // Deliberately NO per-Setup codec-policy override: wideband is always on
    // (astar-e542) — there is no knob anywhere, per-Setup or app-global. The
    // codec policy is unconditionally prefer_slin16; nodes without
    // allow=slin16 answer µ-law in IAX2 negotiation, so the node decides the
    // narrowband fallback, not a preference.

    public init(
        id: String, name: String, hardwareProfileID: String,
        inputDevice: String? = nil, outputDevice: String? = nil,
        inputGain: Float? = nil, outputGain: Float? = nil,
        compression: Bool? = nil, compressionLevel: Float? = nil,
        txTrim: Float? = nil,
        noiseReduction: Bool? = nil,
        voxEnabled: Bool? = nil, voxThreshold: Float? = nil,
        fullDuplex: Bool? = nil, serial: SerialLineSpec? = nil,
        micProfileID: String? = nil
    ) {
        self.id = id
        self.name = name
        self.hardwareProfileID = hardwareProfileID
        self.inputDevice = inputDevice
        self.outputDevice = outputDevice
        self.inputGain = inputGain
        self.outputGain = outputGain
        self.compression = compression
        self.compressionLevel = compressionLevel
        self.txTrim = txTrim
        self.noiseReduction = noiseReduction
        self.voxEnabled = voxEnabled
        self.voxThreshold = voxThreshold
        self.fullDuplex = fullDuplex
        self.serial = serial
        self.micProfileID = micProfileID
    }

    /// The named devices this Setup needs that aren't currently enumerated — so
    /// the UI can refuse to apply (or warn) when the USB gadget is unplugged.
    /// `nil` devices (system default) are always present. A device named on both
    /// directions (combined gadget) is reported once.
    public func missingDevices(inputs: [String], outputs: [String]) -> [String] {
        var missing: [String] = []
        if let inputDevice, !inputs.contains(inputDevice) {
            missing.append(inputDevice)
        }
        if let outputDevice, !outputs.contains(outputDevice), !missing.contains(outputDevice) {
            missing.append(outputDevice)
        }
        return missing
    }
}

/// Persists the ordered list of `Setup`s plus which one is selected.
public protocol SetupStore {
    /// Every setup, in user order.
    func all() -> [Setup]
    /// Upsert by `id`, preserving list position (new ids append).
    func save(_ setup: Setup)
    /// Remove the setup with `id` (no-op if absent).
    func remove(id: String)
    /// Reorder: move the setup at `from` to index `to`.
    func move(from: Int, to: Int)
    /// The selected setup id, or `nil` if none chosen.
    func loadSelectedID() -> String?
    func saveSelectedID(_ id: String?)
    /// The launch-default setup id, applied on startup; `nil` = none.
    func loadDefaultID() -> String?
    func saveDefaultID(_ id: String?)
}

/// `UserDefaults`-backed store: the list as a JSON array under `audio.setups`,
/// the selected id under `audio.selectedSetup`. Platform-neutral / iOS-ready.
public final class UserDefaultsSetupStore: SetupStore {
    private enum Key {
        static let setups = "audio.setups"
        static let selected = "audio.selectedSetup"
        static let defaultID = "audio.defaultSetup"
    }

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func all() -> [Setup] {
        guard let data = defaults.data(forKey: Key.setups),
            let setups = try? JSONDecoder().decode([Setup].self, from: data)
        else {
            return []
        }
        return setups
    }

    public func save(_ setup: Setup) {
        var list = all()
        if let idx = list.firstIndex(where: { $0.id == setup.id }) {
            list[idx] = setup
        } else {
            list.append(setup)
        }
        persist(list)
    }

    public func remove(id: String) {
        persist(all().filter { $0.id != id })
    }

    public func move(from: Int, to: Int) {
        var list = all()
        guard list.indices.contains(from), to >= 0, to <= list.count else { return }
        let item = list.remove(at: from)
        list.insert(item, at: min(to, list.count))
        persist(list)
    }

    public func loadSelectedID() -> String? {
        defaults.string(forKey: Key.selected)
    }

    public func saveSelectedID(_ id: String?) {
        defaults.set(id, forKey: Key.selected)
    }

    public func loadDefaultID() -> String? {
        defaults.string(forKey: Key.defaultID)
    }

    public func saveDefaultID(_ id: String?) {
        defaults.set(id, forKey: Key.defaultID)
    }

    private func persist(_ list: [Setup]) {
        if let data = try? JSONEncoder().encode(list) {
            defaults.set(data, forKey: Key.setups)
        }
    }
}
