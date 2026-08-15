// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import XCTest
@testable import AstarSerial

final class SerialClientTests: XCTestCase {
    /// The default must stay `.usb`: the raw-USB backend needs no CH34x dext and
    /// never opens a tty. Opening a USB radio interface's tty asserts RTS, which
    /// is the radio-key line, so a fresh install must not land on that path by
    /// omission (iax-c7e1).
    func testDefaultTransportIsUsb() {
        XCTAssertEqual(SerialConfig().transport, .usb)
    }

    func testOpenBogusPathThrows() {
        var cfg = SerialConfig()
        // Explicitly the tty backend: `portPath` is what this exercises, and the
        // default transport ignores it.
        cfg.transport = .tty
        cfg.portPath = "/dev/iax-nonexistent-serial"
        XCTAssertThrowsError(try SerialClient(cfg))
    }

    func testAutodetectIsNilOrAPath() {
        // No device on CI → nil; with a UCI150 → a non-empty path. Never crashes.
        if let p = SerialClient.autodetect() { XCTAssertFalse(p.isEmpty) }
    }

    func testErrorTextSecretFree() {
        for code: Int32 in [0, -1, -2, -3, -4, -5, -6] {
            let t = SerialError.from(code).text.lowercased()
            for bad in ["secret", "password", "token"] {
                XCTAssertFalse(t.contains(bad))
            }
        }
    }
}
