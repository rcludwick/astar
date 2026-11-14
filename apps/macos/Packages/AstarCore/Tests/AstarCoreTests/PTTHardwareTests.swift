// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// A `SetupStore` held in memory that counts saves. A test can then prove that
/// the call under test wrote a choice, and that nothing else did.
private final class InMemorySetupStore: SetupStore {
    private(set) var setups: [Setup]
    private(set) var saveCount = 0
    private var selected: String?
    private var defaultID: String?

    init(_ setups: [Setup]) { self.setups = setups }

    func all() -> [Setup] { setups }

    func save(_ setup: Setup) {
        saveCount += 1
        if let i = setups.firstIndex(where: { $0.id == setup.id }) {
            setups[i] = setup
        } else {
            setups.append(setup)
        }
    }

    func remove(id: String) { setups.removeAll { $0.id == id } }

    func move(from: Int, to: Int) {
        guard setups.indices.contains(from), to >= 0, to <= setups.count else { return }
        let item = setups.remove(at: from)
        setups.insert(item, at: min(to, setups.count))
    }

    func loadSelectedID() -> String? { selected }
    func saveSelectedID(_ id: String?) { selected = id }
    func loadDefaultID() -> String? { defaultID }
    func saveDefaultID(_ id: String?) { defaultID = id }
}

/// One PTT device per config. A config with a Bluetooth spec never opens
/// serial, whatever its profile says. Choosing the button moves the profile to
/// `headset`, so an older astar that ignores the spec still opens no port.
/// Choosing serial again restores the old serial spec untouched.
final class PTTHardwareTests: XCTestCase {
    private let deviceID = "inrico-b02ptt"
    private let headset = HardwareProfileRegistry.headsetID
    private let uci150 = HardwareProfileRegistry.uci150ID
    private let customID = HardwareProfileRegistry.customID

    private var spec: BluetoothPTTSpec {
        BluetoothPTTSpec(deviceID: deviceID, preferredName: "B02PTT-6522")
    }

    /// A serial spec unlike `.uci150` in every field, so "kept intact" can't
    /// pass by coincidence.
    private let custom = SerialLineSpec(
        portPath: "/dev/cu.usbserial-test", autodetect: false,
        keyLineRaw: 1, keyActiveHigh: false, radioLineRaw: 1, radioActiveHigh: false,
        debounceMs: 40, rxModeRaw: 1, rxFloorDb: -50, rxHangMs: 300, transportRaw: 0)

    // MARK: - Choices

    func testChoicesTitlesAndIDs() {
        let all = PTTHardwareChoice.all()
        XCTAssertEqual(all, [.none, .uci150, .customSerial, .bluetooth(deviceID: "inrico-b02ptt")])
        XCTAssertEqual(
            all.map(\.title), ["None", "AllScan UCI150", "Custom serial", "Inrico B02PTT"])
        XCTAssertEqual(all.map(\.id), ["headset", "uci150", "custom", "ble:inrico-b02ptt"])
    }

    func testChoicesFollowTheRegistryAndTheDeviceTable() {
        let noCustom = HardwareProfileRegistry(
            builtins: HardwareProfileRegistry.defaultBuiltins.filter { $0.id != customID })
        XCTAssertEqual(PTTHardwareChoice.all(registry: noCustom, devices: []), [.none, .uci150])
        XCTAssertEqual(
            PTTHardwareChoice.all(devices: BLEButtonDevices.all).last,
            .bluetooth(deviceID: "inrico-b02ptt"))
    }

    func testUnknownDeviceTitleFallsBackToItsID() {
        XCTAssertEqual(PTTHardwareChoice.bluetooth(deviceID: "acme-x1").title, "acme-x1")
        XCTAssertEqual(PTTHardwareChoice.bluetooth(deviceID: "acme-x1").id, "ble:acme-x1")
    }

    // MARK: - Plan: the Bluetooth spec comes first

    /// The hand-edited case: a serial profile and a spec on one config. The
    /// spec wins, so serial stays closed.
    func testBluetoothConfigNeverOpensSerial() {
        let uciAndSpec = Setup(
            id: "both", name: "Both", hardwareProfileID: uci150, serial: .uci150,
            bluetoothPTT: spec)
        XCTAssertEqual(PTTHardwarePlan.plan(for: uciAndSpec), .bluetooth(spec))
        XCTAssertEqual(uciAndSpec.pttHardware, .bluetooth(deviceID: deviceID))

        let customAndSpec = Setup(
            id: "both2", name: "Both 2", hardwareProfileID: customID, serial: custom,
            bluetoothPTT: spec)
        XCTAssertEqual(PTTHardwarePlan.plan(for: customAndSpec), .bluetooth(spec))
    }

    /// Without a spec the plan is exactly what `SetupController.applyHardware`
    /// does today: `setup.serial ?? profile.serial ?? .uci150` for a serial
    /// profile, nothing for headset.
    func testPlanWithoutSpecIsTodaysBehaviour() {
        XCTAssertEqual(
            PTTHardwarePlan.plan(for: Setup(id: "u", name: "U", hardwareProfileID: uci150)),
            .serial(.uci150))
        XCTAssertEqual(
            PTTHardwarePlan.plan(
                for: Setup(id: "c", name: "C", hardwareProfileID: customID, serial: custom)),
            .serial(custom))
        XCTAssertEqual(
            PTTHardwarePlan.plan(for: Setup(id: "c2", name: "C2", hardwareProfileID: customID)),
            .serial(.uci150))
        XCTAssertEqual(
            PTTHardwarePlan.plan(
                for: Setup(id: "h", name: "H", hardwareProfileID: headset, serial: custom)),
            .noHardware)
        // An unknown id resolves to the UCI150 and opens serial. That is why
        // choosing the button never invents a new hardwareProfileID.
        XCTAssertEqual(
            PTTHardwarePlan.plan(for: Setup(id: "x", name: "X", hardwareProfileID: "b02ptt")),
            .serial(.uci150))
    }

    func testPTTHardwareReadsTheProfileWhenThereIsNoSpec() {
        XCTAssertEqual(Setup(id: "u", name: "U", hardwareProfileID: uci150).pttHardware, .uci150)
        XCTAssertEqual(
            Setup(id: "h", name: "H", hardwareProfileID: headset).pttHardware,
            PTTHardwareChoice.none)
        XCTAssertEqual(
            Setup(id: "c", name: "C", hardwareProfileID: customID).pttHardware, .customSerial)
        XCTAssertEqual(
            Setup(id: "x", name: "X", hardwareProfileID: "b02ptt").pttHardware, .uci150)
    }

    // MARK: - Changing the choice

    func testChoosingBluetoothSetsHeadsetProfile() {
        let desk = Setup(id: "desk", name: "Desk", hardwareProfileID: uci150, serial: custom)
        let chosen = desk.withPTTHardware(.bluetooth(deviceID: deviceID))

        var expected = desk
        expected.hardwareProfileID = headset
        expected.bluetoothPTT = BluetoothPTTSpec(deviceID: deviceID)
        XCTAssertEqual(chosen, expected, "only the profile and the spec change")
        XCTAssertEqual(chosen.serial, custom, "the serial spec is kept for later")
        XCTAssertEqual(chosen.pttHardware, .bluetooth(deviceID: deviceID))
        XCTAssertEqual(
            PTTHardwarePlan.plan(for: chosen), .bluetooth(BluetoothPTTSpec(deviceID: deviceID)))
    }

    func testChoosingTheSameBluetoothDeviceKeepsItsPreferredName() {
        let s = Setup(id: "h", name: "H", hardwareProfileID: headset, bluetoothPTT: spec)
        XCTAssertEqual(s.withPTTHardware(.bluetooth(deviceID: deviceID)).bluetoothPTT, spec)
        XCTAssertEqual(
            s.withPTTHardware(.bluetooth(deviceID: "acme-x1")).bluetoothPTT,
            BluetoothPTTSpec(deviceID: "acme-x1"))
    }

    func testChoosingSerialClearsBluetoothSpec() {
        let onButton = Setup(id: "desk", name: "Desk", hardwareProfileID: uci150, serial: custom)
            .withPTTHardware(.bluetooth(deviceID: deviceID))

        let toCustom = onButton.withPTTHardware(.customSerial)
        XCTAssertNil(toCustom.bluetoothPTT)
        XCTAssertEqual(toCustom.hardwareProfileID, customID)
        XCTAssertEqual(toCustom.serial, custom, "old serial intact")
        XCTAssertEqual(PTTHardwarePlan.plan(for: toCustom), .serial(custom))

        let toUCI = onButton.withPTTHardware(.uci150)
        XCTAssertNil(toUCI.bluetoothPTT)
        XCTAssertEqual(toUCI.hardwareProfileID, uci150)
        XCTAssertEqual(toUCI.serial, custom, "old serial intact")
        XCTAssertEqual(PTTHardwarePlan.plan(for: toUCI), .serial(custom))
    }

    func testChoosingNoneClearsSpecAndOpensNothing() {
        let onButton = Setup(
            id: "desk", name: "Desk", hardwareProfileID: headset, serial: custom,
            bluetoothPTT: spec)
        let none = onButton.withPTTHardware(.none)
        XCTAssertNil(none.bluetoothPTT)
        XCTAssertEqual(none.hardwareProfileID, headset)
        XCTAssertEqual(none.serial, custom)
        XCTAssertEqual(PTTHardwarePlan.plan(for: none), .noHardware)
        XCTAssertEqual(none.pttHardware, PTTHardwareChoice.none)
    }

    // MARK: - Quick settings saves immediately

    func testQuickSettingsPTTChoiceSavesImmediately() {
        let desk = Setup(id: "desk", name: "Desk", hardwareProfileID: uci150, serial: custom)
        let other = Setup(id: "other", name: "Other", hardwareProfileID: headset)
        let store = InMemorySetupStore([desk, other])

        let returned = PTTHardwareEditor.setActive(
            .bluetooth(deviceID: deviceID), selectedID: "desk", store: store)

        let expected = desk.withPTTHardware(.bluetooth(deviceID: deviceID))
        XCTAssertEqual(store.saveCount, 1, "one save, by setActive itself")
        XCTAssertEqual(store.all(), [expected, other])
        XCTAssertEqual(returned, expected)
    }

    func testUnknownSelectionSavesNothing() {
        let desk = Setup(id: "desk", name: "Desk", hardwareProfileID: uci150)
        let store = InMemorySetupStore([desk])
        XCTAssertNil(PTTHardwareEditor.setActive(.none, selectedID: "gone", store: store))
        XCTAssertEqual(store.saveCount, 0)
        XCTAssertEqual(store.all(), [desk])
    }

    /// System Default can't be edited, so it has no PTT choice to write, even
    /// when a hand-edited store carries a copy of it.
    func testSystemDefaultHasNoEditablePTTChoice() {
        let desk = Setup(id: "desk", name: "Desk", hardwareProfileID: uci150)
        let store = InMemorySetupStore([SystemDefaultSetup.setup, desk])

        XCTAssertNil(
            PTTHardwareEditor.setActive(
                .bluetooth(deviceID: deviceID), selectedID: SystemDefaultSetup.id, store: store))
        XCTAssertNil(PTTHardwareEditor.setActive(.uci150, selectedID: nil, store: store))
        XCTAssertEqual(store.saveCount, 0)
        XCTAssertEqual(store.all(), [SystemDefaultSetup.setup, desk])
        XCTAssertEqual(SystemDefaultSetup.setup.pttHardware, PTTHardwareChoice.none)
        XCTAssertEqual(PTTHardwarePlan.plan(for: SystemDefaultSetup.setup), .noHardware)
    }
}
