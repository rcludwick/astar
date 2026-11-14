// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Keying needs a confirm press bound to the peripheral's platform
/// identifier, never to its advertised name. These confirmations are per-Mac
/// and live under `blePTT.confirmed`, a prefix no export section takes.
final class BluetoothPTTLocalStoreTests: XCTestCase {
    private var suite = ""
    private var defaults: UserDefaults!
    private var store: UserDefaultsBluetoothPTTLocalStore!

    private let model = "inrico-b02ptt"
    private let unitA = UUID(uuidString: "1B3E5C2A-7D44-4F0B-9A61-2C8E0F5D7A13")!
    private let unitB = UUID(uuidString: "9C0D4E21-55AB-4C77-8E19-B04F3A6D2E58")!
    private let when = Date(timeIntervalSince1970: 1_791_000_000)

    override func setUp() {
        super.setUp()
        suite = "astar.tests.bleptt.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
        store = UserDefaultsBluetoothPTTLocalStore(defaults)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
        super.tearDown()
    }

    private func confirmation(_ id: UUID, _ name: String = "B02PTT-6522") -> BLEConfirmation {
        BLEConfirmation(platformID: id, advertisedName: name, confirmedAt: when)
    }

    func testKeyIsBlePTTConfirmed() {
        XCTAssertEqual(UserDefaultsBluetoothPTTLocalStore.key, "blePTT.confirmed")
    }

    func testNothingIsConfirmedByDefault() {
        XCTAssertEqual(store.confirmations(deviceID: model), [])
        XCTAssertFalse(store.isConfirmed(platformID: unitA, deviceID: model))
    }

    func testConfirmThenIsConfirmedByPlatformIdentifier() {
        store.confirm(confirmation(unitA), deviceID: model)
        XCTAssertTrue(store.isConfirmed(platformID: unitA, deviceID: model))
        XCTAssertEqual(store.confirmations(deviceID: model), [confirmation(unitA)])
        // Persisted under exactly the one key, as Data.
        XCTAssertNotNil(defaults.data(forKey: UserDefaultsBluetoothPTTLocalStore.key))
        XCTAssertEqual(
            UserDefaultsBluetoothPTTLocalStore(defaults).confirmations(deviceID: model),
            [confirmation(unitA)], "a second store on the same domain reads it back")
    }

    /// The binding is to the peripheral identifier, never the advertised
    /// name: another unit advertising the same name is a new, unconfirmed
    /// device.
    func testSameNameDifferentIdentifierIsUnconfirmed() {
        store.confirm(confirmation(unitA, "B02PTT-6522"), deviceID: model)
        XCTAssertFalse(store.isConfirmed(platformID: unitB, deviceID: model))
    }

    func testConfirmationIsScopedToItsModel() {
        store.confirm(confirmation(unitA), deviceID: model)
        XCTAssertFalse(store.isConfirmed(platformID: unitA, deviceID: "acme-x1"))
        XCTAssertEqual(store.confirmations(deviceID: "acme-x1"), [])
    }

    func testConfirmUpsertsByPlatformIdentifier() {
        store.confirm(confirmation(unitA, "B02PTT-6522"), deviceID: model)
        store.confirm(confirmation(unitB, "B02PTT-7001"), deviceID: model)
        let renamed = BLEConfirmation(
            platformID: unitA, advertisedName: "B02PTT-6522-2",
            confirmedAt: when.addingTimeInterval(60))
        store.confirm(renamed, deviceID: model)
        XCTAssertEqual(
            store.confirmations(deviceID: model), [renamed, confirmation(unitB, "B02PTT-7001")])
    }

    func testForgetRemovesOnlyThatUnit() {
        store.confirm(confirmation(unitA), deviceID: model)
        store.confirm(confirmation(unitB, "B02PTT-7001"), deviceID: model)
        store.forget(platformID: unitA, deviceID: model)
        XCTAssertFalse(store.isConfirmed(platformID: unitA, deviceID: model))
        XCTAssertTrue(store.isConfirmed(platformID: unitB, deviceID: model))
        XCTAssertEqual(store.confirmations(deviceID: model), [confirmation(unitB, "B02PTT-7001")])
    }

    func testForgettingTheLastUnitRemovesTheKey() {
        store.confirm(confirmation(unitA), deviceID: model)
        store.forget(platformID: unitA, deviceID: model)
        XCTAssertNil(defaults.data(forKey: UserDefaultsBluetoothPTTLocalStore.key))
        XCTAssertEqual(store.confirmations(deviceID: model), [])
    }

    func testForgetUnknownIsANoOp() {
        store.confirm(confirmation(unitA), deviceID: model)
        store.forget(platformID: unitB, deviceID: model)
        store.forget(platformID: unitA, deviceID: "acme-x1")
        XCTAssertEqual(store.confirmations(deviceID: model), [confirmation(unitA)])
    }

    /// A corrupt value reads as "nothing confirmed". It fails closed and never
    /// crashes.
    func testCorruptDataReadsAsNothingConfirmed() {
        defaults.set(Data("not json".utf8), forKey: UserDefaultsBluetoothPTTLocalStore.key)
        XCTAssertEqual(store.confirmations(deviceID: model), [])
        XCTAssertFalse(store.isConfirmed(platformID: unitA, deviceID: model))
    }
}
