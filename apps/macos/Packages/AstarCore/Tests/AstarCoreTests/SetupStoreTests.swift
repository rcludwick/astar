// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class SetupStoreTests: XCTestCase {
    private let suite = "astar.tests.setups"
    private var defaults: UserDefaults!
    private var store: UserDefaultsSetupStore!

    override func setUp() {
        super.setUp()
        defaults = UserDefaults(suiteName: suite)
        defaults.removePersistentDomain(forName: suite)
        store = UserDefaultsSetupStore(defaults)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
        super.tearDown()
    }

    private func setup(
        _ id: String, _ name: String,
        hw: String = HardwareProfileRegistry.headsetID,
        input: String? = nil, output: String? = nil
    ) -> Setup {
        Setup(id: id, name: name, hardwareProfileID: hw, inputDevice: input, outputDevice: output)
    }

    // MARK: - Store round-trips

    func testEmptyByDefault() {
        XCTAssertTrue(store.all().isEmpty)
        XCTAssertNil(store.loadSelectedID())
    }

    func testSaveThenAllRoundTrips() {
        let s = setup(
            "a", "Jabra mobile", hw: HardwareProfileRegistry.headsetID,
            input: "Jabra Link 390", output: "Jabra Link 390")
        store.save(s)
        XCTAssertEqual(store.all(), [s])
    }

    func testSavePreservesInsertionOrder() {
        store.save(setup("a", "First"))
        store.save(setup("b", "Second"))
        store.save(setup("c", "Third"))
        XCTAssertEqual(store.all().map(\.id), ["a", "b", "c"])
    }

    func testSaveUpsertsByIDPreservingPosition() {
        store.save(setup("a", "First"))
        store.save(setup("b", "Second"))
        store.save(setup("a", "First renamed"))

        XCTAssertEqual(store.all().map(\.id), ["a", "b"], "upsert must not reorder or duplicate")
        XCTAssertEqual(store.all().first?.name, "First renamed")
    }

    func testRemoveDeletesByID() {
        store.save(setup("a", "First"))
        store.save(setup("b", "Second"))
        store.remove(id: "a")
        XCTAssertEqual(store.all().map(\.id), ["b"])
    }

    func testMoveReorders() {
        store.save(setup("a", "First"))
        store.save(setup("b", "Second"))
        store.save(setup("c", "Third"))
        store.move(from: 2, to: 0)
        XCTAssertEqual(store.all().map(\.id), ["c", "a", "b"])
    }

    func testMoveWithOutOfRangeIndexIsIgnored() {
        // An out-of-bounds `from` (or `to`) must be a no-op, never crash.
        store.save(setup("a", "First"))
        store.save(setup("b", "Second"))
        store.move(from: 9, to: 0)  // from out of range
        store.move(from: 0, to: 9)  // to out of range
        store.move(from: -1, to: 0)  // negative from
        XCTAssertEqual(store.all().map(\.id), ["a", "b"], "invalid moves leave order untouched")
    }

    func testMoveToEndClampsInsertion() {
        store.save(setup("a", "First"))
        store.save(setup("b", "Second"))
        store.save(setup("c", "Third"))
        store.move(from: 0, to: 3)  // to == count: insert at the clamped end
        XCTAssertEqual(store.all().map(\.id), ["b", "c", "a"])
    }

    func testDefaultIDRoundTrips() {
        XCTAssertNil(store.loadDefaultID())
        store.saveDefaultID("b")
        XCTAssertEqual(store.loadDefaultID(), "b")
        store.saveDefaultID(nil)
        XCTAssertNil(store.loadDefaultID())
    }

    func testAnOldPeakScaleVoxThresholdDecodesTranslated() throws {
        let json = #"{"id":"a","name":"A","hardwareProfileID":"none","voxThreshold":-40}"#
        let setup = try JSONDecoder().decode(Setup.self, from: Data(json.utf8))
        XCTAssertEqual(setup.voxThreshold, -50)
    }

    func testANewScaleVoxThresholdIsUsedAsIsAndWinsOverTheOldKey() throws {
        let json =
            #"{"id":"a","name":"A","hardwareProfileID":"none","voxThreshold":-40,"voxThresholdRMS":-44}"#
        let setup = try JSONDecoder().decode(Setup.self, from: Data(json.utf8))
        XCTAssertEqual(setup.voxThreshold, -44)
    }

    func testTheVoxThresholdIsWrittenOnlyUnderTheNewKey() throws {
        let setup = Setup(id: "a", name: "A", hardwareProfileID: "none", voxThreshold: -47)
        let data = try JSONEncoder().encode(setup)
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(obj["voxThresholdRMS"] as? Double, -47)
        XCTAssertNil(obj["voxThreshold"], "an older astar must not read it as a peak")
        XCTAssertEqual(try JSONDecoder().decode(Setup.self, from: data), setup)
    }

    func testFullDuplexAndSerialRoundTrip() {
        var s = setup("a", "Desk")
        s.fullDuplex = true
        s.serial = .uci150
        store.save(s)
        let loaded = store.all().first
        XCTAssertEqual(loaded?.fullDuplex, true)
        XCTAssertEqual(loaded?.serial, .uci150)
    }

    func testSelectedIDRoundTrips() {
        store.saveSelectedID("b")
        XCTAssertEqual(store.loadSelectedID(), "b")
        store.saveSelectedID(nil)
        XCTAssertNil(store.loadSelectedID())
    }

    // MARK: - Per-setup gains (mic + volume)

    func testSetupGainsRoundTrip() {
        var s = setup("a", "Tuned")
        s.inputGain = 1.3
        s.outputGain = 0.7
        store.save(s)
        let loaded = store.all().first
        XCTAssertEqual(loaded?.inputGain, 1.3)
        XCTAssertEqual(loaded?.outputGain, 0.7)
    }

    func testGainsDefaultToNil() {
        // A setup with no tuned gains leaves them unset (apply won't override).
        XCTAssertNil(setup("a", "x").inputGain)
        XCTAssertNil(setup("a", "x").outputGain)
    }

    func testTxTrimRoundTrips() {
        // TX trim (post-compression output gain) is per-mic/per-rig, so a
        // setup can carry its own override.
        var s = setup("a", "Tuned")
        s.txTrim = 0.4
        store.save(s)
        XCTAssertEqual(store.all().first?.txTrim, 0.4)
    }

    func testTxTrimDefaultsToNil() {
        // No stored trim = don't override on apply.
        XCTAssertNil(setup("a", "x").txTrim)
    }

    func testProcessingFlagsRoundTrip() {
        var s = setup("a", "x")
        s.compression = true
        s.noiseReduction = true
        s.voxEnabled = true
        store.save(s)
        let loaded = store.all().first
        XCTAssertEqual(loaded?.compression, true)
        XCTAssertEqual(loaded?.noiseReduction, true)
        XCTAssertEqual(loaded?.voxEnabled, true)
    }

    func testLegacySetupWithoutOptionalsDecodesToNil() {
        // Setups persisted before gains/processing existed must still decode
        // (Optional keys are absent in the stored JSON).
        let json = #"[{"id":"a","name":"Old","hardwareProfileID":"headset"}]"#
        defaults.set(Data(json.utf8), forKey: "audio.setups")
        let loaded = store.all()
        XCTAssertEqual(loaded.count, 1)
        XCTAssertNil(loaded.first?.inputGain)
        XCTAssertNil(loaded.first?.outputGain)
        XCTAssertNil(loaded.first?.compression)
        XCTAssertNil(loaded.first?.txTrim)
        XCTAssertNil(loaded.first?.noiseReduction)
        XCTAssertNil(loaded.first?.voxEnabled)
        XCTAssertNil(loaded.first?.fullDuplex)
        XCTAssertNil(loaded.first?.serial)
        XCTAssertNil(loaded.first?.bluetoothPTT)
    }

    // MARK: - Bluetooth PTT spec (additive; ConfigVersion stays 1)

    /// A config written before `bluetoothPTT` existed (here: a real UCI150
    /// shape, serial and all) decodes with the field nil, so it keeps
    /// today's serial behaviour.
    func testSetupDecodesWithoutBluetoothField() {
        let json = #"""
            [{"id":"a","name":"UCI150","hardwareProfileID":"uci150",
              "serial":{"keyLineRaw":0,"keyActiveHigh":true,"radioLineRaw":0,
                        "radioActiveHigh":true,"debounceMs":30,"rxModeRaw":0,
                        "rxFloorDb":-45,"rxHangMs":250,"transportRaw":1}}]
            """#
        defaults.set(Data(json.utf8), forKey: "audio.setups")
        let loaded = store.all()
        XCTAssertEqual(loaded.count, 1)
        XCTAssertNil(loaded.first?.bluetoothPTT)
        XCTAssertEqual(loaded.first?.serial, .uci150)
    }

    func testSetupRoundTripsBluetoothField() throws {
        let spec = BluetoothPTTSpec(deviceID: "inrico-b02ptt", preferredName: "B02PTT-6522")
        let s = Setup(
            id: "a", name: "Handheld", hardwareProfileID: HardwareProfileRegistry.headsetID,
            serial: .uci150, bluetoothPTT: spec)
        store.save(s)
        XCTAssertEqual(store.all(), [s])
        XCTAssertEqual(store.all().first?.bluetoothPTT, spec)

        // The JSON key is `bluetoothPTT` and holds exactly the two fields.
        let data = try XCTUnwrap(defaults.data(forKey: "audio.setups"))
        let array = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [[String: Any]])
        let stored = try XCTUnwrap(array.first?["bluetoothPTT"] as? [String: String])
        XCTAssertEqual(stored, ["deviceID": "inrico-b02ptt", "preferredName": "B02PTT-6522"])
    }

    /// A config without a spec writes no `bluetoothPTT` key at all, so an
    /// export from a Mac that never chose a Bluetooth button is unchanged.
    func testSetupWithoutSpecEncodesNoBluetoothKey() throws {
        store.save(setup("a", "Desk", hw: HardwareProfileRegistry.uci150ID))
        let data = try XCTUnwrap(defaults.data(forKey: "audio.setups"))
        let array = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [[String: Any]])
        XCTAssertEqual(array.count, 1)
        XCTAssertNil(array.first?["bluetoothPTT"])
    }

    /// An astar from before `bluetoothPTT` decodes a config that chose the
    /// button. It ignores the unknown key. Because choosing the button moved
    /// the profile to `headset`, it resolves to a profile that opens no serial
    /// port, rather than to the UCI150 and RTS.
    func testOlderReaderResolvesHeadset() throws {
        let desk = Setup(
            id: "a", name: "Desk", hardwareProfileID: HardwareProfileRegistry.uci150ID,
            serial: .uci150)
        let chosen = desk.withPTTHardware(.bluetooth(deviceID: "inrico-b02ptt"))
        let data = try JSONEncoder().encode([chosen])

        let old = try JSONDecoder().decode([PreBluetoothSetup].self, from: data)
        XCTAssertEqual(old.count, 1)
        XCTAssertEqual(old.first?.hardwareProfileID, HardwareProfileRegistry.headsetID)
        let oldID = try XCTUnwrap(old.first).hardwareProfileID
        let profile = HardwareProfileRegistry().resolve(id: oldID)
        XCTAssertEqual(profile.usesSerial, false)
        XCTAssertEqual(old.first?.serial, .uci150, "the serial spec rides along untouched")
    }

    // MARK: - Device-presence validation

    func testMissingDevicesWhenInputAbsent() {
        let s = setup("a", "UCI150 desk", input: "AllScan UCI150", output: "AllScan UCI150")
        let missing = s.missingDevices(inputs: ["Built-in Mic"], outputs: ["Built-in Output"])
        XCTAssertEqual(missing, ["AllScan UCI150"], "input + output share a name → reported once")
    }

    func testNoMissingDevicesWhenAllPresent() {
        let s = setup("a", "UCI150 desk", input: "AllScan UCI150", output: "AllScan UCI150")
        let missing = s.missingDevices(inputs: ["AllScan UCI150"], outputs: ["AllScan UCI150"])
        XCTAssertTrue(missing.isEmpty)
    }

    func testSystemDefaultDevicesAreAlwaysPresent() {
        // nil device = system default; never "missing".
        let s = setup("a", "Default", input: nil, output: nil)
        XCTAssertTrue(s.missingDevices(inputs: [], outputs: []).isEmpty)
    }

    func testReportsBothMissingWhenNamesDiffer() {
        let s = setup("a", "Split", input: "Mic A", output: "Speaker B")
        let missing = s.missingDevices(inputs: ["Mic A"], outputs: ["Speaker C"])
        XCTAssertEqual(missing, ["Speaker B"])
    }
}

/// `Setup` as an astar from before `bluetoothPTT` decodes it: the same fields
/// that affect hardware, and no `bluetoothPTT`. Synthesized `Decodable` drops
/// unknown keys, as the shipped builds do.
private struct PreBluetoothSetup: Decodable {
    var id: String
    var name: String
    var hardwareProfileID: String
    var serial: SerialLineSpec?
}
