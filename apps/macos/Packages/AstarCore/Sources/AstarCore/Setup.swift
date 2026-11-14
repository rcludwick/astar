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
    /// VOX trigger level (dBFS, on the 50 ms mean-power scale — see
    /// `VoxThreshold`) for this rig; `nil` = don't override. Encoded as
    /// `voxThresholdRMS`; a config saved before that key existed carries a
    /// peak-scale `voxThreshold`, which decodes translated by
    /// `VoxThreshold.fromPeak`.
    public var voxThreshold: Float?
    /// Full-duplex audio for this rig; `nil` = don't override (defaults half).
    public var fullDuplex: Bool?
    /// Per-config serial line settings (for a serial radio like the UCI150).
    /// `nil` = use the `hardwareProfileID` preset / no serial. Kept even while
    /// `bluetoothPTT` is set, so switching back to serial restores it intact.
    public var serial: SerialLineSpec?
    /// Selected mic-profile id for this config. `nil` = the built-in Default
    /// profile (unfiltered). Switching to this config applies this profile.
    public var micProfileID: String?
    /// The Bluetooth LE PTT button this config keys from, or `nil` for none.
    ///
    /// When set, the config's PTT hardware is that button and serial stays
    /// closed, whatever `hardwareProfileID` says (one PTT device per config).
    /// Optional and decoded leniently, so configs and exports written before it
    /// existed read as `nil` and keep today's behaviour. That is why adding it
    /// does not move `ConfigVersion`. See `PTTHardwarePlan`.
    public var bluetoothPTT: BluetoothPTTSpec?

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
        micProfileID: String? = nil,
        bluetoothPTT: BluetoothPTTSpec? = nil
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
        self.bluetoothPTT = bluetoothPTT
    }

    // MARK: Coding

    /// Every key is the property's own name except the VOX threshold, which
    /// moved to `voxThresholdRMS` when its scale changed. `legacyVoxThreshold`
    /// is the old peak-scale key, decoded only.
    private enum CodingKeys: String, CodingKey {
        case id, name, hardwareProfileID, inputDevice, outputDevice
        case inputGain, outputGain, compression, compressionLevel, txTrim
        case noiseReduction, voxEnabled, fullDuplex, serial, micProfileID
        case bluetoothPTT
        case voxThreshold = "voxThresholdRMS"
        case legacyVoxThreshold = "voxThreshold"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        hardwareProfileID = try c.decode(String.self, forKey: .hardwareProfileID)
        inputDevice = try c.decodeIfPresent(String.self, forKey: .inputDevice)
        outputDevice = try c.decodeIfPresent(String.self, forKey: .outputDevice)
        inputGain = try c.decodeIfPresent(Float.self, forKey: .inputGain)
        outputGain = try c.decodeIfPresent(Float.self, forKey: .outputGain)
        compression = try c.decodeIfPresent(Bool.self, forKey: .compression)
        compressionLevel = try c.decodeIfPresent(Float.self, forKey: .compressionLevel)
        txTrim = try c.decodeIfPresent(Float.self, forKey: .txTrim)
        noiseReduction = try c.decodeIfPresent(Bool.self, forKey: .noiseReduction)
        voxEnabled = try c.decodeIfPresent(Bool.self, forKey: .voxEnabled)
        if let rms = try c.decodeIfPresent(Float.self, forKey: .voxThreshold) {
            voxThreshold = VoxThreshold.clamped(rms)
        } else {
            voxThreshold = try c.decodeIfPresent(Float.self, forKey: .legacyVoxThreshold)
                .map(VoxThreshold.fromPeak)
        }
        fullDuplex = try c.decodeIfPresent(Bool.self, forKey: .fullDuplex)
        serial = try c.decodeIfPresent(SerialLineSpec.self, forKey: .serial)
        micProfileID = try c.decodeIfPresent(String.self, forKey: .micProfileID)
        bluetoothPTT = try c.decodeIfPresent(BluetoothPTTSpec.self, forKey: .bluetoothPTT)
    }

    /// Never writes the old key: an older astar reading this config sees no
    /// threshold ("don't override") rather than a power-scale number it would
    /// take for a peak.
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(name, forKey: .name)
        try c.encode(hardwareProfileID, forKey: .hardwareProfileID)
        try c.encodeIfPresent(inputDevice, forKey: .inputDevice)
        try c.encodeIfPresent(outputDevice, forKey: .outputDevice)
        try c.encodeIfPresent(inputGain, forKey: .inputGain)
        try c.encodeIfPresent(outputGain, forKey: .outputGain)
        try c.encodeIfPresent(compression, forKey: .compression)
        try c.encodeIfPresent(compressionLevel, forKey: .compressionLevel)
        try c.encodeIfPresent(txTrim, forKey: .txTrim)
        try c.encodeIfPresent(noiseReduction, forKey: .noiseReduction)
        try c.encodeIfPresent(voxEnabled, forKey: .voxEnabled)
        try c.encodeIfPresent(voxThreshold, forKey: .voxThreshold)
        try c.encodeIfPresent(fullDuplex, forKey: .fullDuplex)
        try c.encodeIfPresent(serial, forKey: .serial)
        try c.encodeIfPresent(micProfileID, forKey: .micProfileID)
        try c.encodeIfPresent(bluetoothPTT, forKey: .bluetoothPTT)
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
