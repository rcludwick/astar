// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Where the call's PTT (push-to-talk) comes from for a given hardware profile.
/// Platform-neutral; the macOS app maps `.serial` onto the AstarSerial bridge.
public enum PTTSource: String, Equatable, CaseIterable {
    case serial
    case button
    case vox
}

/// A platform-neutral snapshot of serial-line settings. Mirrors AstarSerial's
/// `SerialConfig` but uses raw values so AstarCore (multiplatform) never imports
/// the macOS-only `AstarSerial`. The macOS app maps this to/from `SerialConfig`.
///
/// Raw values match the AstarSerial enums:
///   keyLineRaw:  CTS=0, DCD=1, DSR=2, RI=3   (`KeyLine`)
///   radioLineRaw: RTS=0, DTR=1               (`RadioLine`)
///   rxModeRaw:   RemotePTT=0, RxActivity=1   (`RxKeyMode`)
public struct SerialLineSpec: Equatable, Codable {
    public var portPath: String?  // the manually-chosen port (used when !isAutodetect)
    /// Whether to autodetect the serial port at open time vs. use `portPath`.
    /// `nil` = autodetect (the legacy default, so specs stored before this flag
    /// existed still decode and keep autodetecting).
    public var autodetect: Bool?
    /// Resolved autodetect decision: `nil` flag means autodetect.
    public var isAutodetect: Bool { autodetect ?? true }
    public var keyLineRaw: UInt32
    public var keyActiveHigh: Bool
    public var radioLineRaw: UInt32
    public var radioActiveHigh: Bool
    public var debounceMs: UInt32
    public var rxModeRaw: UInt32
    public var rxFloorDb: Float
    public var rxHangMs: UInt32
    /// Transport reaching the modem-control lines: `nil`/0 = tty (legacy, needs
    /// the CH34x dext), 1 = raw-USB (`Transport.usb`, driverless / sandbox-MAS).
    /// Optional so specs persisted before this field still decode (→ tty).
    public var transportRaw: UInt32?

    public init(
        portPath: String? = nil,
        autodetect: Bool? = nil,
        keyLineRaw: UInt32,
        keyActiveHigh: Bool,
        radioLineRaw: UInt32,
        radioActiveHigh: Bool,
        debounceMs: UInt32,
        rxModeRaw: UInt32,
        rxFloorDb: Float,
        rxHangMs: UInt32,
        transportRaw: UInt32? = nil
    ) {
        self.portPath = portPath
        self.autodetect = autodetect
        self.keyLineRaw = keyLineRaw
        self.keyActiveHigh = keyActiveHigh
        self.radioLineRaw = radioLineRaw
        self.radioActiveHigh = radioActiveHigh
        self.debounceMs = debounceMs
        self.rxModeRaw = rxModeRaw
        self.rxFloorDb = rxFloorDb
        self.rxHangMs = rxHangMs
        self.transportRaw = transportRaw
    }

    /// The AllScan UCI150 spec. CTS key active-high, RTS radio active-high, 30 ms
    /// debounce, RemotePTT rx mode, -45 dB floor, 250 ms hang — and the **raw-USB**
    /// transport (astar-f772): driverless (no CH34x dext) and the only transport
    /// that works under the macOS sandbox / Mac App Store. The Rust Uci150Usb
    /// backend owns the CH343 0x95 decode (CTS=bit3, active-low) and hands astar a
    /// clean active-high "keyed".
    public static let uci150 = SerialLineSpec(
        portPath: nil,
        keyLineRaw: 0,  // CTS
        keyActiveHigh: true,
        radioLineRaw: 0,  // RTS
        radioActiveHigh: true,
        debounceMs: 30,
        rxModeRaw: 0,  // RemotePTT
        rxFloorDb: -45.0,
        rxHangMs: 250,
        transportRaw: 1  // raw-USB (Transport.usb)
    )
}

/// A named hardware preset: pick your device instead of hand-configuring serial
/// and audio. Platform-neutral — `serial` is a `SerialLineSpec`, mapped to
/// AstarSerial only in the macOS app layer.
public struct HardwareProfile: Equatable, Identifiable {
    public let id: String
    public let name: String
    /// Whether this device keys via the serial interface (vs. on-screen button).
    public let usesSerial: Bool
    /// The serial preset to apply. `nil` while `usesSerial` is true means
    /// "keep the current config" (the Custom profile).
    public let serial: SerialLineSpec?
    public let defaultPTTSource: PTTSource

    public init(
        id: String, name: String, usesSerial: Bool,
        serial: SerialLineSpec?, defaultPTTSource: PTTSource
    ) {
        self.id = id
        self.name = name
        self.usesSerial = usesSerial
        self.serial = serial
        self.defaultPTTSource = defaultPTTSource
    }
}

/// The catalog of built-in hardware presets, with id lookup. Adding a new preset
/// (another AllScan model, a CM108-class adapter) is a one-line append to
/// `builtins`.
public struct HardwareProfileRegistry {
    public static let uci150ID = "uci150"
    public static let headsetID = "headset"
    public static let customID = "custom"

    public let builtins: [HardwareProfile]

    public init(builtins: [HardwareProfile] = HardwareProfileRegistry.defaultBuiltins) {
        self.builtins = builtins
    }

    public static let defaultBuiltins: [HardwareProfile] = [
        HardwareProfile(
            id: uci150ID,
            name: "AllScan UCI150",
            usesSerial: true,
            serial: .uci150,
            defaultPTTSource: .serial
        ),
        HardwareProfile(
            id: headsetID,
            name: "USB Headset / Bluetooth",
            usesSerial: false,
            serial: nil,
            defaultPTTSource: .button
        ),
        HardwareProfile(
            id: customID,
            name: "Custom",
            usesSerial: true,
            serial: nil,  // nil = keep current config
            defaultPTTSource: .button
        ),
    ]

    /// The fallback profile for unknown ids — the safe, working default (UCI150).
    public var defaultProfile: HardwareProfile {
        profile(id: HardwareProfileRegistry.uci150ID) ?? builtins[0]
    }

    /// Look up a profile by id. Unknown id → `defaultProfile` (UCI150) unless the
    /// caller uses `profile(id:)` directly.
    public func resolve(id: String) -> HardwareProfile {
        profile(id: id) ?? defaultProfile
    }

    private func profile(id: String) -> HardwareProfile? {
        builtins.first { $0.id == id }
    }
}

/// Persists the *selected* hardware-profile id (the preset choice). The actual
/// serial config + enabled flag remain the source of truth elsewhere; this only
/// records which preset the user picked. Default selection = uci150 (reproduces
/// today's behavior on first launch).
public protocol HardwareProfileStore {
    func loadSelectedID() -> String
    func saveSelectedID(_ id: String)
}

public final class UserDefaultsHardwareProfileStore: HardwareProfileStore {
    private enum Key {
        static let selected = "hardware.selectedProfileID"
    }

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func loadSelectedID() -> String {
        defaults.string(forKey: Key.selected) ?? HardwareProfileRegistry.uci150ID
    }

    public func saveSelectedID(_ id: String) {
        defaults.set(id, forKey: Key.selected)
    }
}
