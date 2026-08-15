// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import CAstarSerial
import Foundation

public enum KeyLine: UInt32 { case cts = 0, dcd = 1, dsr = 2, ri = 3 }
public enum RadioLine: UInt32 { case rts = 0, dtr = 1 }
public enum RxKeyMode: UInt32 { case remotePTT = 0, rxActivity = 1 }

/// Transport reaching the modem-control lines. `.usb` is the raw-USB backend
/// (no tty, no CH34x dext): the sandbox/MAS- and iOS-eligible path.
///
/// The raw values mirror `IaxSerialTransport` in the C ABI and must not be
/// renumbered — `.tty` is the C enum's zero value.
public enum Transport: UInt32 { case tty = 0, usb = 1 }

public struct SerialConfig {
    public var portPath: String?          // nil = autodetect; ignored when .usb
    /// Defaults to `.usb`: a fresh install must never need the CH34x dext, and
    /// must never open a tty it was not explicitly pointed at — opening a USB
    /// radio interface's tty asserts RTS, which is the radio-key line. An
    /// operator who wants the tty path selects it deliberately.
    public var transport: Transport = .usb
    public var keyLine: KeyLine = .cts
    public var keyActiveHigh: Bool = true
    public var radioLine: RadioLine = .rts
    public var radioActiveHigh: Bool = true
    public var debounceMs: UInt32 = 30
    public var rxMode: RxKeyMode = .remotePTT
    public var rxFloorDb: Float = -45.0
    public var rxHangMs: UInt32 = 250
    public init() {}
}

public struct SerialError: Error, CustomStringConvertible {
    public let code: Int32
    public let text: String
    public var description: String { "SerialError(\(code)): \(text)" }
    static func from(_ code: Int32) -> SerialError {
        SerialError(code: code, text: String(cString: iax_serial_error_text(code)))
    }
}

/// A serial radio-interface client (PTT facet). Drive it from a ~20 ms loop:
/// read your AstarStation snapshot, call `pttTick`, forward `ptt` to `setPTT`.
public final class SerialClient {
    private let handle: OpaquePointer

    /// Autodetected port path (first WCH USB device), or nil.
    public static func autodetect() -> String? {
        var buf = [CChar](repeating: 0, count: 256)
        return buf.withUnsafeMutableBufferPointer { p -> String? in
            iax_serial_autodetect(p.baseAddress, UInt(p.count)) == 0
                ? String(cString: p.baseAddress!) : nil
        }
    }

    public init(_ config: SerialConfig) throws {
        // Keep the path C-string alive across iax_serial_open.
        let pathC = config.portPath.map { strdup($0) }
        defer { if let pathC { free(pathC) } }
        var c = IaxSerialConfig(
            port_path: pathC.flatMap { UnsafePointer($0) },
            transport: IaxSerialTransport(rawValue: config.transport.rawValue),
            key_line: IaxKeyLine(rawValue: config.keyLine.rawValue),
            key_active_high: config.keyActiveHigh,
            radio_line: IaxRadioLine(rawValue: config.radioLine.rawValue),
            radio_active_high: config.radioActiveHigh,
            cts_debounce_ms: config.debounceMs,
            rx_mode: IaxRxKeyMode(rawValue: config.rxMode.rawValue),
            rx_floor_db: config.rxFloorDb,
            rx_hang_ms: config.rxHangMs)
        guard let h = iax_serial_open(&c) else {
            throw SerialError.from(-3)
        }
        handle = h
    }

    /// One keying tick. Returns whether the call PTT should change and its new
    /// value. `remoteKeyed`/`rxDb` come from your AstarStation snapshot.
    public func pttTick(remoteKeyed: Bool, rxDb: Float) throws -> (changed: Bool, ptt: Bool) {
        var setPtt = false
        var changed = false
        let rc = iax_serial_ptt_tick(handle, remoteKeyed, rxDb, &setPtt, &changed)
        if rc < 0 { throw SerialError.from(rc) }
        return (changed, setPtt)
    }

    deinit { iax_serial_close(handle) }
}
