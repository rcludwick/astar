// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The app calls sleep, wake and quit on the Bluetooth PTT controller for
/// every user, including people who never chose the button. Those calls must
/// create no Bluetooth central, or macOS would ask everyone for Bluetooth
/// permission after a wake. Nobody who never chose the button may ever see a
/// Bluetooth permission prompt. They also must not leave the controller
/// stuck, so a later choice of the button still connects.
@MainActor
final class BluetoothPTTLifecycleHookTests: XCTestCase {
    private func makeController(
        factory: CountingCentralFactory, ticker: ManualTicker,
        store: UserDefaultsBluetoothPTTLocalStore
    ) -> BluetoothPTTController {
        BluetoothPTTController(
            centralFactory: { factory.make() }, store: store, ticker: ticker,
            clockMS: { 0 })
    }

    func testSleepWakeAndShutdownWithoutAttachCreateNoCentral() {
        let factory = CountingCentralFactory()
        let ticker = ManualTicker()
        let store = UserDefaultsBluetoothPTTLocalStore(bluetoothScratchDefaults(cleanup: self))
        let controller = makeController(factory: factory, ticker: ticker, store: store)

        controller.systemWillSleep()
        controller.systemDidWake()
        controller.systemWillSleep()
        controller.systemDidWake()
        controller.shutdown()

        XCTAssertEqual(factory.made.count, 0, "no hook may create a central unattached")
        XCTAssertEqual(ticker.startCount, 0)
        XCTAssertEqual(controller.status, .disabled)
    }

    /// A sleep and wake before the button is chosen must not leave the
    /// controller believing the Mac is still asleep. If it did, a later
    /// attach would never connect.
    func testSleepAndWakeBeforeAttachStillLetsAConfirmedUnitConnect() {
        let factory = CountingCentralFactory()
        let ticker = ManualTicker()
        let defaults = bluetoothScratchDefaults(cleanup: self)
        let store = UserDefaultsBluetoothPTTLocalStore(defaults)
        store.confirm(
            BLEConfirmation(
                platformID: BLETestFixtures.unitA,
                advertisedName: BLETestFixtures.advertisedName,
                confirmedAt: Date(timeIntervalSince1970: 1_790_000_000)),
            deviceID: BLETestFixtures.deviceID)
        let session = hermeticSession(FakeStation(), defaults: defaults)
        let controller = makeController(factory: factory, ticker: ticker, store: store)

        controller.systemWillSleep()
        controller.systemDidWake()
        XCTAssertEqual(factory.made.count, 0)

        controller.attach(session: session, spec: BLETestFixtures.spec)
        XCTAssertEqual(factory.made.count, 1)
        let central = factory.made[0]
        central.known = [
            BLEPeripheralRef(
                identifier: BLETestFixtures.unitA, name: BLETestFixtures.advertisedName)
        ]
        central.setAdapter(.on)

        XCTAssertEqual(central.connects.map(\.id), [BLETestFixtures.unitA])
        XCTAssertEqual(central.connects.map(\.epoch), [1])
    }
}
