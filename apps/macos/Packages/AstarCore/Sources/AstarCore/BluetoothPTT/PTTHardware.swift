// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// A config's PTT hardware, as the picker offers it: one device per config.
///
/// The on-screen button, Space and VOX are not choices here. They key
/// alongside whichever device this is. VOX is the VOX card's own switch.
public enum PTTHardwareChoice: Hashable, Identifiable, Sendable {
    /// On-screen button and Space only (the `headset` profile).
    case none
    /// The AllScan UCI150 serial preset.
    case uci150
    /// A USB radio interface with hand-tuned serial lines.
    case customSerial
    /// A Bluetooth LE PTT button from the engine's device table.
    case bluetooth(deviceID: String)

    /// Stable tag for the picker. The serial choices reuse their
    /// `HardwareProfileRegistry` ids, so the existing tags keep their meaning.
    /// A Bluetooth choice is namespaced so it can never collide with a
    /// profile id.
    public var id: String {
        switch self {
        case .none: return HardwareProfileRegistry.headsetID
        case .uci150: return HardwareProfileRegistry.uci150ID
        case .customSerial: return HardwareProfileRegistry.customID
        case .bluetooth(let deviceID): return "ble:\(deviceID)"
        }
    }

    /// The picker label. A Bluetooth choice shows the device table's display
    /// name. For a model this build doesn't know, it shows the stored id
    /// instead of an empty row.
    public var title: String {
        switch self {
        case .none: return "None"
        case .uci150: return "AllScan UCI150"
        case .customSerial: return "Custom serial"
        case .bluetooth(let deviceID):
            return BLEButtonDevices.device(id: deviceID)?.displayName ?? deviceID
        }
    }

    /// The picker's options, in order: None, AllScan UCI150 and Custom serial
    /// (each only if the registry has it), then one per device-table row.
    /// The three wired choices are fixed cases, filtered by the registry ids;
    /// a new serial preset needs a new case here. A new button model needs
    /// only a new device-table row.
    public static func all(
        registry: HardwareProfileRegistry = HardwareProfileRegistry(),
        devices: [BLEButtonDevice] = BLEButtonDevices.all
    ) -> [PTTHardwareChoice] {
        let profileIDs = Set(registry.builtins.map(\.id))
        let wired: [PTTHardwareChoice] = [.none, .uci150, .customSerial]
        return wired.filter { profileIDs.contains($0.id) }
            + devices.map { PTTHardwareChoice.bluetooth(deviceID: $0.id) }
    }
}

/// What applying a config does to PTT hardware, decided in one testable
/// place. `SetupController.applyHardware` switches on it.
public enum PTTHardwarePlan: Equatable, Sendable {
    /// Open serial with this spec.
    case serial(SerialLineSpec)
    /// Keep serial closed and attach the Bluetooth PTT controller for this spec.
    case bluetooth(BluetoothPTTSpec)
    /// Keep serial closed and attach nothing.
    case noHardware

    /// The spec comes first: a non-nil `bluetoothPTT` wins over any serial
    /// profile, so a hand-edited config holding both never opens serial.
    /// Without a spec this is exactly the old `applyHardware` rule.
    public static func plan(
        for setup: Setup, registry: HardwareProfileRegistry = HardwareProfileRegistry()
    ) -> PTTHardwarePlan {
        if let spec = setup.bluetoothPTT {
            return .bluetooth(spec)
        }
        let profile = registry.resolve(id: setup.hardwareProfileID)
        guard profile.usesSerial else { return .noHardware }
        return .serial(setup.serial ?? profile.serial ?? .uci150)
    }
}

extension Setup {
    /// The picker's current value. The spec comes first, as in
    /// `PTTHardwarePlan`. Otherwise the profile, resolved the way the
    /// registry resolves it, so an unknown id reads as the UCI150 it would
    /// open.
    public var pttHardware: PTTHardwareChoice {
        if let spec = bluetoothPTT {
            return .bluetooth(deviceID: spec.deviceID)
        }
        switch HardwareProfileRegistry().resolve(id: hardwareProfileID).id {
        case HardwareProfileRegistry.headsetID: return .none
        case HardwareProfileRegistry.customID: return .customSerial
        default: return .uci150
        }
    }

    /// This config with `choice` as its PTT hardware.
    ///
    /// - `.bluetooth`: `hardwareProfileID` becomes `headset`, so an older
    ///   astar that ignores the spec opens no serial port. The spec keeps its
    ///   `preferredName` when the device is the same one. `serial` is
    ///   untouched.
    /// - `.uci150` / `.customSerial`: that profile id, spec cleared, `serial`
    ///   untouched, so the old spec comes back intact.
    /// - `.none`: `headset`, spec cleared.
    public func withPTTHardware(_ choice: PTTHardwareChoice) -> Setup {
        var copy = self
        switch choice {
        case .none:
            copy.hardwareProfileID = HardwareProfileRegistry.headsetID
            copy.bluetoothPTT = nil
        case .uci150:
            copy.hardwareProfileID = HardwareProfileRegistry.uci150ID
            copy.bluetoothPTT = nil
        case .customSerial:
            copy.hardwareProfileID = HardwareProfileRegistry.customID
            copy.bluetoothPTT = nil
        case .bluetooth(let deviceID):
            copy.hardwareProfileID = HardwareProfileRegistry.headsetID
            if bluetoothPTT?.deviceID != deviceID {
                copy.bluetoothPTT = BluetoothPTTSpec(deviceID: deviceID)
            }
        }
        return copy
    }
}

/// Writes a PTT hardware choice the way Quick settings needs: straight into
/// the selected config, like a device change. It does not wait for "Save
/// changes to …", which only covers gains and processing.
public enum PTTHardwareEditor {
    /// Saves the choice into the selected config immediately and returns it;
    /// nil for System Default or no selection.
    ///
    /// System Default is refused by id even if a store holds a copy of it. It
    /// is the always-safe config and never gets PTT hardware.
    public static func setActive(
        _ choice: PTTHardwareChoice, selectedID: String?, store: SetupStore
    ) -> Setup? {
        guard let id = selectedID, id != SystemDefaultSetup.id,
            let current = store.all().first(where: { $0.id == id })
        else { return nil }
        let updated = current.withPTTHardware(choice)
        store.save(updated)
        return updated
    }
}
