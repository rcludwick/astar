// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class HardwareProfileTests: XCTestCase {
    private func freshDefaults() -> UserDefaults {
        UserDefaults(suiteName: "astar.tests.hw.\(ProcessInfo.processInfo.globallyUniqueString)")!
    }

    // MARK: - Registry builtins

    func testBuiltinsPresentByID() {
        let reg = HardwareProfileRegistry()
        XCTAssertEqual(reg.resolve(id: "uci150").id, "uci150")
        XCTAssertEqual(reg.resolve(id: "headset").id, "headset")
        XCTAssertEqual(reg.resolve(id: "custom").id, "custom")
    }

    func testBuiltinsHaveExpectedShape() {
        let reg = HardwareProfileRegistry()

        let uci = reg.resolve(id: "uci150")
        XCTAssertEqual(uci.name, "AllScan UCI150")
        XCTAssertTrue(uci.usesSerial)
        XCTAssertNotNil(uci.serial)
        XCTAssertEqual(uci.defaultPTTSource, .serial)

        let headset = reg.resolve(id: "headset")
        XCTAssertEqual(headset.name, "USB Headset / Bluetooth")
        XCTAssertFalse(headset.usesSerial)
        XCTAssertNil(headset.serial)
        XCTAssertEqual(headset.defaultPTTSource, .button)

        let custom = reg.resolve(id: "custom")
        XCTAssertEqual(custom.name, "Custom")
        XCTAssertTrue(custom.usesSerial)
        XCTAssertNil(custom.serial)  // nil = keep current config
        XCTAssertEqual(custom.defaultPTTSource, .button)
    }

    // MARK: - UCI150 spec mirrors SerialConfig() defaults

    func testUCI150SpecFieldValues() {
        let reg = HardwareProfileRegistry()
        let spec = reg.resolve(id: "uci150").serial!
        XCTAssertNil(spec.portPath)  // autodetect
        XCTAssertEqual(spec.keyLineRaw, 0)  // CTS
        XCTAssertTrue(spec.keyActiveHigh)
        XCTAssertEqual(spec.radioLineRaw, 0)  // RTS
        XCTAssertTrue(spec.radioActiveHigh)
        XCTAssertEqual(spec.debounceMs, 30)
        XCTAssertEqual(spec.rxModeRaw, 0)  // RemotePTT
        XCTAssertEqual(spec.rxFloorDb, -45.0)
        XCTAssertEqual(spec.rxHangMs, 250)
        XCTAssertEqual(spec.transportRaw, 1)  // raw-USB (Transport.usb) — astar-f772
    }

    func testSpecTransportCodableBackCompatDefaultsNil() throws {
        // A spec persisted before `transportRaw` existed must still decode (and read
        // nil → tty), so existing saved configs aren't broken by the new field.
        let legacy = """
            {"keyLineRaw":0,"keyActiveHigh":true,"radioLineRaw":0,"radioActiveHigh":true,\
            "debounceMs":30,"rxModeRaw":0,"rxFloorDb":-45,"rxHangMs":250}
            """
        let spec = try JSONDecoder().decode(SerialLineSpec.self, from: Data(legacy.utf8))
        XCTAssertNil(spec.transportRaw)
        // And a USB spec round-trips through Codable.
        let usb = try JSONDecoder().decode(
            SerialLineSpec.self,
            from: JSONEncoder().encode(SerialLineSpec.uci150))
        XCTAssertEqual(usb.transportRaw, 1)
    }

    // MARK: - Serial port autodetect flag

    func testUCI150SpecAutodetectsByDefault() {
        let spec = HardwareProfileRegistry().resolve(id: "uci150").serial!
        // The preset leaves the port unset → autodetect (nil == autodetect).
        XCTAssertNil(spec.autodetect)
        XCTAssertTrue(spec.isAutodetect)
        XCTAssertNil(spec.portPath)
    }

    func testManualPortSpecIsNotAutodetect() {
        let spec = SerialLineSpec(
            portPath: "/dev/cu.usbserial-1420", autodetect: false,
            keyLineRaw: 0, keyActiveHigh: true,
            radioLineRaw: 0, radioActiveHigh: true,
            debounceMs: 30, rxModeRaw: 0,
            rxFloorDb: -45, rxHangMs: 250)
        XCTAssertFalse(spec.isAutodetect)
        XCTAssertEqual(spec.portPath, "/dev/cu.usbserial-1420")
    }

    func testSpecDecodesWithoutAutodetectKey() throws {
        // Setups stored before the flag existed must still decode (autodetect = nil).
        let json = """
            {"keyLineRaw":0,"keyActiveHigh":true,"radioLineRaw":0,"radioActiveHigh":true,
             "debounceMs":30,"rxModeRaw":0,"rxFloorDb":-45,"rxHangMs":250}
            """.data(using: .utf8)!
        let spec = try JSONDecoder().decode(SerialLineSpec.self, from: json)
        XCTAssertNil(spec.autodetect)
        XCTAssertTrue(spec.isAutodetect)
    }

    func testSpecAutodetectRoundTrips() throws {
        let spec = SerialLineSpec(
            portPath: "/dev/cu.usbmodem1", autodetect: false,
            keyLineRaw: 0, keyActiveHigh: true,
            radioLineRaw: 0, radioActiveHigh: true,
            debounceMs: 30, rxModeRaw: 0,
            rxFloorDb: -45, rxHangMs: 250)
        let data = try JSONEncoder().encode(spec)
        let back = try JSONDecoder().decode(SerialLineSpec.self, from: data)
        XCTAssertEqual(back, spec)
    }

    // MARK: - Unknown id falls back

    func testUnknownIDFallsBackToDefault() {
        let reg = HardwareProfileRegistry()
        XCTAssertEqual(reg.resolve(id: "nope").id, "uci150")
    }

    func testDefaultProfileIsUCI150() {
        let reg = HardwareProfileRegistry()
        XCTAssertEqual(reg.defaultProfile.id, "uci150")
    }

    func testDefaultProfileFallsBackToFirstBuiltinWhenNoUCI150() {
        // A registry built without the uci150 id can't resolve it, so
        // defaultProfile falls back to builtins[0] — the safety net for a
        // hypothetical catalog that drops the canonical default.
        let only = HardwareProfile(
            id: "headset", name: "USB Headset",
            usesSerial: false, serial: nil, defaultPTTSource: .button)
        let reg = HardwareProfileRegistry(builtins: [only])
        XCTAssertEqual(reg.defaultProfile.id, "headset", "no uci150 → first builtin")
        XCTAssertEqual(reg.resolve(id: "nope").id, "headset", "unknown id → that fallback")
    }

    // MARK: - Store

    func testStoreDefaultsToUCI150() {
        let store = UserDefaultsHardwareProfileStore(freshDefaults())
        XCTAssertEqual(store.loadSelectedID(), "uci150")
    }

    func testSelectionRoundTrips() {
        let store = UserDefaultsHardwareProfileStore(freshDefaults())
        store.saveSelectedID("headset")
        XCTAssertEqual(store.loadSelectedID(), "headset")
    }

    func testExtensibility() {
        // Adding a preset is a plain append; the registry must surface it by id.
        let extra = HardwareProfile(
            id: "cm108", name: "CM108 Adapter",
            usesSerial: false, serial: nil,
            defaultPTTSource: .button)
        let reg = HardwareProfileRegistry(
            builtins: HardwareProfileRegistry.defaultBuiltins + [extra])
        XCTAssertEqual(reg.resolve(id: "cm108").name, "CM108 Adapter")
    }
}
