// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import Foundation
    import AstarSerial

    /// Persisted serial-PTT preferences: the enabled flag plus the full
    /// `SerialConfig` (port, key/radio lines + polarity, debounce, RX mode/floor/hang).
    /// Stored in `UserDefaults` (mirrors `UserDefaultsAudioSettingsStore` in
    /// AstarCore). macOS-only, since `SerialConfig` is.
    struct SerialSettings {
        var enabled: Bool
        var config: SerialConfig

        init(enabled: Bool = false, config: SerialConfig = SerialConfig()) {
            self.enabled = enabled
            self.config = config
        }
    }

    protocol SerialSettingsStore {
        func load() -> SerialSettings
        func save(_ settings: SerialSettings)
    }

    /// `UserDefaults`-backed store. Absent keys fall back to `SerialConfig`'s
    /// defaults (UCI150 profile: CTS-in / RTS-out, both active-high, 30 ms debounce,
    /// RemotePTT RX mode).
    final class UserDefaultsSerialSettingsStore: SerialSettingsStore {
        private enum Key {
            static let enabled = "serial.enabled"
            static let portPath = "serial.portPath"
            static let keyLine = "serial.keyLine"
            static let keyActiveHigh = "serial.keyActiveHigh"
            static let radioLine = "serial.radioLine"
            static let radioActiveHigh = "serial.radioActiveHigh"
            static let debounceMs = "serial.debounceMs"
            static let rxMode = "serial.rxMode"
            static let rxFloorDb = "serial.rxFloorDb"
            static let rxHangMs = "serial.rxHangMs"
            static let transport = "serial.transport"
        }

        private let defaults: UserDefaults

        init(_ defaults: UserDefaults = .standard) {
            self.defaults = defaults
        }

        func load() -> SerialSettings {
            var c = SerialConfig()  // start from defaults, override what's stored
            // An empty string persists "autodetect" (nil); a non-empty one is explicit.
            if let s = defaults.string(forKey: Key.portPath), !s.isEmpty { c.portPath = s }
            if defaults.object(forKey: Key.keyLine) != nil,
                let k = KeyLine(rawValue: UInt32(defaults.integer(forKey: Key.keyLine)))
            {
                c.keyLine = k
            }
            if defaults.object(forKey: Key.keyActiveHigh) != nil {
                c.keyActiveHigh = defaults.bool(forKey: Key.keyActiveHigh)
            }
            if defaults.object(forKey: Key.radioLine) != nil,
                let r = RadioLine(rawValue: UInt32(defaults.integer(forKey: Key.radioLine)))
            {
                c.radioLine = r
            }
            if defaults.object(forKey: Key.radioActiveHigh) != nil {
                c.radioActiveHigh = defaults.bool(forKey: Key.radioActiveHigh)
            }
            if defaults.object(forKey: Key.debounceMs) != nil {
                c.debounceMs = UInt32(max(0, defaults.integer(forKey: Key.debounceMs)))
            }
            if defaults.object(forKey: Key.rxMode) != nil,
                let m = RxKeyMode(rawValue: UInt32(defaults.integer(forKey: Key.rxMode)))
            {
                c.rxMode = m
            }
            if defaults.object(forKey: Key.rxFloorDb) != nil {
                c.rxFloorDb = defaults.float(forKey: Key.rxFloorDb)
            }
            if defaults.object(forKey: Key.rxHangMs) != nil {
                c.rxHangMs = UInt32(max(0, defaults.integer(forKey: Key.rxHangMs)))
            }
            // Absent (configs saved before the transport field) → keep the
            // .usb default. A config that names a transport keeps it, so an
            // operator who deliberately chose .tty is not migrated off it.
            if defaults.object(forKey: Key.transport) != nil,
                let t = Transport(rawValue: UInt32(defaults.integer(forKey: Key.transport)))
            {
                c.transport = t
            }
            return SerialSettings(enabled: defaults.bool(forKey: Key.enabled), config: c)
        }

        func save(_ settings: SerialSettings) {
            let c = settings.config
            defaults.set(settings.enabled, forKey: Key.enabled)
            defaults.set(c.portPath ?? "", forKey: Key.portPath)
            defaults.set(Int(c.keyLine.rawValue), forKey: Key.keyLine)
            defaults.set(c.keyActiveHigh, forKey: Key.keyActiveHigh)
            defaults.set(Int(c.radioLine.rawValue), forKey: Key.radioLine)
            defaults.set(c.radioActiveHigh, forKey: Key.radioActiveHigh)
            defaults.set(Int(c.debounceMs), forKey: Key.debounceMs)
            defaults.set(Int(c.rxMode.rawValue), forKey: Key.rxMode)
            defaults.set(c.rxFloorDb, forKey: Key.rxFloorDb)
            defaults.set(Int(c.rxHangMs), forKey: Key.rxHangMs)
            defaults.set(Int(c.transport.rawValue), forKey: Key.transport)
        }
    }
#endif
