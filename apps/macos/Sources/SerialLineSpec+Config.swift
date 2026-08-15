// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarSerial

    /// Maps the platform-neutral `SerialLineSpec` (AstarCore) to/from AstarSerial's
    /// macOS-only `SerialConfig`. AstarCore must never import AstarSerial, so the raw
    /// `keyLineRaw`/`radioLineRaw`/`rxModeRaw` values are reconstructed into the
    /// AstarSerial enums here, in the app layer, using the same raw-value initializers
    /// the persistence store already uses.
    extension SerialConfig {
        /// Build a `SerialConfig` from a neutral spec. Any enum raw value that
        /// somehow doesn't map keeps `SerialConfig()`'s (UCI150) default — so the
        /// result is always a valid, working config.
        init(spec: SerialLineSpec) {
            var c = SerialConfig()
            // Autodetect → leave portPath nil so SerialClient discovers the WCH port;
            // manual → pin the chosen port.
            c.portPath = spec.isAutodetect ? nil : spec.portPath
            if let k = KeyLine(rawValue: spec.keyLineRaw) { c.keyLine = k }
            c.keyActiveHigh = spec.keyActiveHigh
            if let r = RadioLine(rawValue: spec.radioLineRaw) { c.radioLine = r }
            c.radioActiveHigh = spec.radioActiveHigh
            c.debounceMs = spec.debounceMs
            if let m = RxKeyMode(rawValue: spec.rxModeRaw) { c.rxMode = m }
            c.rxFloorDb = spec.rxFloorDb
            c.rxHangMs = spec.rxHangMs
            // nil/unmapped transport keeps the default (.usb), so a spec that
            // predates the field lands on the driver-free path. A spec that
            // names .tty still gets it.
            if let t = spec.transportRaw, let tr = Transport(rawValue: t) { c.transport = tr }
            self = c
        }

        /// The neutral spec mirror of this config.
        var spec: SerialLineSpec {
            SerialLineSpec(
                portPath: portPath,
                autodetect: portPath == nil,
                keyLineRaw: keyLine.rawValue,
                keyActiveHigh: keyActiveHigh,
                radioLineRaw: radioLine.rawValue,
                radioActiveHigh: radioActiveHigh,
                debounceMs: debounceMs,
                rxModeRaw: rxMode.rawValue,
                rxFloorDb: rxFloorDb,
                rxHangMs: rxHangMs,
                transportRaw: transport.rawValue
            )
        }
    }
#endif
