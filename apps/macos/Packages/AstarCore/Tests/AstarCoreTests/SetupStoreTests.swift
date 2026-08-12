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
