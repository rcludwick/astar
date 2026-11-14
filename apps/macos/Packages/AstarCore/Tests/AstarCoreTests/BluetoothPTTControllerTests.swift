// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// One controller wired to a real `CallSession` over a `FakeStation`, a fake
/// transport, a hand-fired ticker and a hand-set clock. Nothing here touches
/// Bluetooth, the network or a serial port.
@MainActor
private final class Rig {
    let station = FakeStation()
    let session: CallSession
    let ticker = ManualTicker()
    let clock = TestClock()
    let factory = CountingCentralFactory()
    let defaults: UserDefaults
    let store: UserDefaultsBluetoothPTTLocalStore
    let controller: BluetoothPTTController
    var logged: [String] = []

    /// `test` owns the scratch defaults suite and removes it in its teardown.
    init(_ test: XCTestCase, confirmed: [UUID] = [], answered: Bool = true) {
        defaults = bluetoothScratchDefaults(cleanup: test)
        store = UserDefaultsBluetoothPTTLocalStore(defaults)
        for id in confirmed {
            store.confirm(
                BLEConfirmation(
                    platformID: id, advertisedName: BLETestFixtures.advertisedName,
                    confirmedAt: Date(timeIntervalSince1970: 1_790_000_000)),
                deviceID: BLETestFixtures.deviceID)
        }
        station.snapshotToReturn =
            answered
            ? answeredSnapshot()
            : CallSnapshot(
                status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil)
        session = hermeticSession(station, defaults: defaults)
        session.poll()
        let factory = self.factory
        let clock = self.clock
        controller = BluetoothPTTController(
            centralFactory: { factory.make() }, store: store, ticker: ticker,
            clockMS: { clock.ms })
        controller.diagnosticLog = { [weak self] in self?.logged.append($0) }
    }

    var central: FakeBLECentral { factory.made[factory.made.count - 1] }
    var keyLog: [String] { bluetoothKeyLog(station) }

    /// Confirmed unit A, adapter on, linked on epoch 1, subscribed at t=0,
    /// then pressed at t=1000 ms, well past the 300 ms settle window.
    func attachAndPress() {
        controller.attach(session: session, spec: BLETestFixtures.spec)
        central.known = [
            BLEPeripheralRef(
                identifier: BLETestFixtures.unitA, name: BLETestFixtures.advertisedName)
        ]
        central.setAdapter(.on)
        central.linkUp(BLETestFixtures.unitA, epoch: 1)
        clock.ms += 1_000
        central.notify(BLETestFixtures.unitA, epoch: 1, [0x01])
    }

    func tick(_ count: Int = 1) {
        for _ in 0..<count {
            clock.ms += 250
            ticker.fire()
        }
    }
}

@MainActor
final class BluetoothPTTControllerTests: XCTestCase {
    private struct Boom: Error {}
    private let unitA = BLETestFixtures.unitA
    private let unitB = BLETestFixtures.unitB
    private let press = BLETestFixtures.leasePress
    private let release = BLETestFixtures.release

    // MARK: - No Bluetooth manager without the feature

    func testNoCentralWithoutBluetoothConfig() {
        let rig = Rig(self)
        XCTAssertEqual(
            rig.factory.made.count, 0, "constructing the controller touches no Bluetooth")

        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)

        XCTAssertEqual(rig.factory.made.count, 0, "an unconfirmed config never creates a manager")
        XCTAssertTrue(rig.controller.needsSetup)
        XCTAssertEqual(rig.controller.status, .disabled)
        XCTAssertFalse(rig.ticker.isRunning)

        rig.controller.detach()
        XCTAssertEqual(rig.factory.made.count, 0)

        // The positive control: only the user's own setup action creates it.
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.controller.beginSetup()
        XCTAssertEqual(rig.factory.made.count, 1)
        XCTAssertEqual(rig.central.log, ["powerUp"])
    }

    func testAttachWithAConfirmedUnitCreatesOneCentralAndTicksAt250ms() {
        let rig = Rig(self, confirmed: [unitA])

        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)

        XCTAssertEqual(rig.factory.made.count, 1)
        XCTAssertFalse(rig.controller.needsSetup)
        XCTAssertEqual(rig.ticker.periodMS, 250)
        XCTAssertTrue(rig.ticker.isRunning)
        XCTAssertEqual(rig.central.log, ["powerUp"], "nothing more until the adapter reports on")
    }

    // MARK: - Never someone else's button: the identifier decides, not the name

    func testImportedSpecArrivesUnconfirmed() throws {
        let defaults = bluetoothScratchDefaults(cleanup: self)
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: "astar-config-v1-bleptt", withExtension: "astarconfig",
                subdirectory: "Fixtures"))
        // Every store on the scratch suite: the import sets a 300 s transmit
        // time-out, which must never land in the test host's standard domain.
        let session = CallSession(
            station: NullStation(), audioStore: UserDefaultsAudioSettingsStore(defaults),
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            micProfileStore: UserDefaultsMicProfileStore(defaults), userDefaults: defaults)
        let setupStore = UserDefaultsSetupStore(defaults)
        _ = ConfigTransfer.apply(
            try ConfigArchive.decode(try Data(contentsOf: url)), session: session,
            setupStore: setupStore, profileStore: UserDefaultsMicProfileStore(defaults),
            defaults: defaults)
        let spec = try XCTUnwrap(setupStore.all().compactMap(\.bluetoothPTT).first)
        XCTAssertEqual(
            spec, BluetoothPTTSpec(deviceID: "inrico-b02ptt", preferredName: "B02PTT-6522"))

        let factory = CountingCentralFactory()
        let controller = BluetoothPTTController(
            centralFactory: { factory.make() },
            store: UserDefaultsBluetoothPTTLocalStore(defaults), ticker: ManualTicker(),
            clockMS: { 0 })
        controller.attach(session: session, spec: spec)

        XCTAssertEqual(factory.made.count, 0, "an imported config touches no Bluetooth")
        XCTAssertTrue(controller.needsSetup)
        XCTAssertEqual(controller.confirmedButtons, [])
        XCTAssertNil(defaults.object(forKey: UserDefaultsBluetoothPTTLocalStore.key))
    }

    func testSameNameDifferentIdentifierIsUnconfirmed() {
        let rig = Rig(self, confirmed: [unitA])
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        // B is LE-connected to this Mac already, under A's name.
        rig.central.connectedWithService = [
            BLEPeripheralRef(identifier: unitB, name: BLETestFixtures.advertisedName)
        ]

        rig.central.setAdapter(.on)
        rig.central.advertise(unitB, name: BLETestFixtures.advertisedName)

        XCTAssertEqual(rig.central.connects.count, 0, "a lookalike name never earns a connection")
        XCTAssertEqual(
            rig.controller.discovered.map(\.identifier), [unitB], "it is listed, nothing more")

        rig.central.advertise(unitA, name: BLETestFixtures.advertisedName)

        XCTAssertEqual(rig.central.connects.map { $0.id }, [unitA])
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1])
    }

    func testSetupConnectsToTheOnlyUnitAndConfirmsOnItsRawPressOnly() {
        let rig = Rig(self)
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.controller.beginSetup()
        XCTAssertTrue(rig.controller.awaitingConfirmation)
        rig.central.setAdapter(.on)
        XCTAssertTrue(rig.controller.isSearching)
        XCTAssertEqual(rig.central.log, ["powerUp", "retrieve", "startScan"])

        rig.central.advertise(unitA, name: BLETestFixtures.advertisedName)
        XCTAssertEqual(rig.central.connects.count, 0, "alone for under 2 s: not linked yet")
        rig.tick(8)  // 2 000 ms alone in range
        XCTAssertEqual(
            rig.central.connects.map { $0.id }, [unitA], "exactly one found: pre-highlighted")
        rig.central.linkUp(unitA, epoch: 1)
        XCTAssertEqual(rig.controller.status, .needsConfirm)
        XCTAssertFalse(rig.store.isConfirmed(platformID: unitA, deviceID: BLETestFixtures.deviceID))

        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 1, [0x01])

        XCTAssertEqual(rig.keyLog, [], "the confirming press never keys")
        XCTAssertTrue(rig.store.isConfirmed(platformID: unitA, deviceID: BLETestFixtures.deviceID))
        XCTAssertEqual(
            rig.store.confirmations(deviceID: BLETestFixtures.deviceID).map(\.advertisedName),
            ["B02PTT-6522"])
        XCTAssertTrue(rig.controller.justConfirmed)
        XCTAssertFalse(rig.controller.needsSetup)
        XCTAssertFalse(rig.controller.awaitingConfirmation)

        rig.central.notify(unitA, epoch: 1, [0x00])
        rig.central.notify(unitA, epoch: 1, [0x01])

        XCTAssertEqual(rig.keyLog, [release, press])
    }

    /// A config that names its unit must confirm exactly that unit, or the
    /// confirmation it writes is one the next attach cannot find and setup
    /// repeats forever.
    func testSetupWithPreferredNameConfirmsAUnitThatReattaches() {
        let rig = Rig(self)
        let spec = BluetoothPTTSpec(
            deviceID: BLETestFixtures.deviceID, preferredName: BLETestFixtures.advertisedName)
        rig.controller.attach(session: rig.session, spec: spec)
        XCTAssertTrue(rig.controller.needsSetup)
        rig.controller.beginSetup()
        rig.central.setAdapter(.on)

        rig.central.advertise(unitB, name: "B02PTT-7731")
        XCTAssertEqual(
            rig.controller.discovered.map(\.identifier), [], "another serial is not offered")
        XCTAssertEqual(rig.central.connects.count, 0)

        rig.central.advertise(unitA, name: BLETestFixtures.advertisedName)
        XCTAssertEqual(rig.controller.discovered.map(\.identifier), [unitA])
        rig.tick(8)  // the only offered unit, alone for 2 000 ms
        XCTAssertEqual(rig.central.connects.map { $0.id }, [unitA])
        rig.central.linkUp(unitA, epoch: 1)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 1, [0x01])
        rig.central.notify(unitA, epoch: 1, [0x00])
        XCTAssertEqual(
            rig.store.confirmations(deviceID: BLETestFixtures.deviceID).map(\.platformID), [unitA])

        rig.controller.detach()
        rig.central.known = [
            BLEPeripheralRef(identifier: unitA, name: BLETestFixtures.advertisedName)
        ]
        rig.controller.attach(session: rig.session, spec: spec)

        XCTAssertFalse(rig.controller.needsSetup, "the unit confirmed in setup is found again")
        XCTAssertEqual(rig.factory.made.count, 1)
        XCTAssertEqual(rig.central.connects.map { $0.id }, [unitA, unitA])
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])
        XCTAssertEqual(rig.keyLog, [release], "the confirming press never keyed")
    }

    /// A raw press inside the settle window (a "current state" notify on
    /// CCCD enable, or a stray 01) must not confirm a unit during setup: only
    /// a press after the window does.
    func testSettleWindowPressDoesNotConfirm() {
        let rig = Rig(self)
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.controller.beginSetup()
        rig.central.setAdapter(.on)
        rig.central.advertise(unitA, name: BLETestFixtures.advertisedName)
        rig.tick(8)
        rig.central.linkUp(unitA, epoch: 1)  // subscribed at 2 000 ms

        rig.clock.ms += 100
        rig.central.notify(unitA, epoch: 1, [0x01])

        XCTAssertFalse(rig.store.isConfirmed(platformID: unitA, deviceID: BLETestFixtures.deviceID))
        XCTAssertFalse(rig.controller.justConfirmed)
        XCTAssertTrue(rig.controller.awaitingConfirmation)

        rig.central.notify(unitA, epoch: 1, [0x00])
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 1, [0x01])

        XCTAssertTrue(rig.store.isConfirmed(platformID: unitA, deviceID: BLETestFixtures.deviceID))
        XCTAssertTrue(rig.controller.justConfirmed)
        XCTAssertEqual(rig.keyLog, [release], "neither press keyed; the 00 is a release")
    }

    /// Two B02PTT units in range: outside setup only the identifier confirmed
    /// on this Mac is ever connected, and during setup neither is linked or
    /// confirmed by itself.
    func testTwoUnitsInRangeNeitherAutoConnects() {
        let rig = Rig(self, confirmed: [unitA])
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.central.setAdapter(.on)
        rig.central.advertise(unitB, name: "B02PTT-1111")
        XCTAssertEqual(rig.central.connects.map { $0.id }, [], "B is not this Mac's button")

        let fresh = Rig(self)
        fresh.controller.attach(session: fresh.session, spec: BLETestFixtures.spec)
        fresh.controller.beginSetup()
        fresh.central.setAdapter(.on)
        fresh.central.advertise(unitA, name: BLETestFixtures.advertisedName)
        fresh.central.advertise(unitB, name: "B02PTT-1111")
        fresh.tick(12)  // 3 000 ms: past the sole-unit wait, but there are two

        XCTAssertEqual(fresh.controller.discovered.count, 2)
        XCTAssertEqual(fresh.central.connects.count, 0, "with two in range, Rob chooses")
        XCTAssertFalse(fresh.central.verbs.contains("connect"))
        XCTAssertEqual(fresh.store.confirmations(deviceID: BLETestFixtures.deviceID), [])
        XCTAssertTrue(fresh.station.leasePressCalls.isEmpty)
    }

    /// A second unit that appears after setup linked the first by itself
    /// drops that link, so its press can confirm nothing until Rob selects.
    func testASecondUnitDuringSetupDropsTheAutoLink() {
        let rig = Rig(self)
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.controller.beginSetup()
        rig.central.setAdapter(.on)
        rig.central.advertise(unitA, name: BLETestFixtures.advertisedName)
        rig.tick(8)
        rig.central.linkUp(unitA, epoch: 1)
        XCTAssertEqual(rig.central.connects.map { $0.id }, [unitA])

        rig.central.advertise(unitB, name: "B02PTT-1111")

        XCTAssertEqual(rig.central.log.last, "cancel", "the automatic link is dropped")
        XCTAssertNil(rig.controller.connected)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 1, [0x01])
        XCTAssertEqual(rig.store.confirmations(deviceID: BLETestFixtures.deviceID), [])

        rig.controller.select(
            BLEDiscoveredButton(
                identifier: unitA, advertisedName: BLETestFixtures.advertisedName,
                deviceID: BLETestFixtures.deviceID))
        XCTAssertEqual(rig.central.connects.map { $0.id }, [unitA, unitA])
        rig.central.linkUp(unitA, epoch: 2)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 2, [0x01])
        XCTAssertTrue(rig.store.isConfirmed(platformID: unitA, deviceID: BLETestFixtures.deviceID))
        XCTAssertEqual(rig.keyLog, [], "the confirming press never keys")
    }

    /// Outside setup `select` links only a unit confirmed on this Mac: a
    /// lookalike under Rob's name, picked from the list, is never linked.
    func testSelectOutsideSetupNeverLinksAnUnconfirmedUnit() {
        let rig = Rig(self, confirmed: [unitA])
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.central.setAdapter(.on)
        rig.central.advertise(unitB, name: BLETestFixtures.advertisedName)

        rig.controller.select(
            BLEDiscoveredButton(
                identifier: unitB, advertisedName: BLETestFixtures.advertisedName,
                deviceID: BLETestFixtures.deviceID))

        XCTAssertEqual(rig.central.connects.map { $0.id }, [])
        rig.clock.ms += 1_000
        rig.central.notify(unitB, epoch: 1, [0x01])
        XCTAssertEqual(rig.keyLog, [])

        // Positive control: during setup an explicit select does link.
        let setup = Rig(self)
        setup.controller.attach(session: setup.session, spec: BLETestFixtures.spec)
        setup.controller.beginSetup()
        setup.central.setAdapter(.on)
        setup.central.advertise(unitA, name: BLETestFixtures.advertisedName)
        setup.central.advertise(unitB, name: BLETestFixtures.advertisedName)
        setup.controller.select(
            BLEDiscoveredButton(
                identifier: unitB, advertisedName: BLETestFixtures.advertisedName,
                deviceID: BLETestFixtures.deviceID))
        XCTAssertEqual(setup.central.connects.map { $0.id }, [unitB])
    }

    func testLookalikeWithoutTheServiceIsSkippedForTheSession() {
        let rig = Rig(self)
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.controller.beginSetup()
        rig.central.setAdapter(.on)
        rig.central.advertise(unitB, name: "B02PTT-9999")
        rig.tick(8)  // alone in range for 2 000 ms
        XCTAssertEqual(rig.central.connects.map { $0.id }, [unitB])

        rig.central.linkUp(unitB, epoch: 1, characteristics: [])

        XCTAssertEqual(rig.controller.status, .notSupportedDevice)
        XCTAssertEqual(rig.controller.rejectedName, "B02PTT-9999")
        XCTAssertEqual(rig.controller.discovered.map(\.identifier), [])
        XCTAssertEqual(rig.central.log.filter { $0 == "cancel" }.count, 1)
        XCTAssertFalse(rig.central.log.contains("subscribe:1"), "never subscribed to a lookalike")

        rig.central.advertise(unitB, name: "B02PTT-9999")

        XCTAssertEqual(rig.central.connects.count, 1, "skipped for the rest of the session")
    }

    func testSearchEndsAfterThirtySeconds() {
        let rig = Rig(self)
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.controller.beginSetup()
        rig.central.setAdapter(.on)
        XCTAssertTrue(rig.controller.isSearching)

        rig.clock.ms = 29_999
        rig.ticker.fire()
        XCTAssertTrue(rig.controller.isSearching)

        rig.clock.ms = 30_000
        rig.ticker.fire()
        XCTAssertFalse(rig.controller.isSearching)
        XCTAssertEqual(rig.central.log.last, "stopScan")
    }

    // MARK: - Keying through the session

    func testPressReachesTheStationAsALease() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        XCTAssertEqual(rig.keyLog, [press])
        XCTAssertTrue(rig.controller.isLinked)
        XCTAssertEqual(rig.central.log, ["powerUp", "retrieve", "connect:1", "subscribe:1"])
        // Outside setup a press is only a press: it never re-confirms the unit.
        XCTAssertFalse(rig.controller.justConfirmed)
        XCTAssertEqual(
            rig.store.confirmations(deviceID: BLETestFixtures.deviceID),
            [
                BLEConfirmation(
                    platformID: unitA, advertisedName: BLETestFixtures.advertisedName,
                    confirmedAt: Date(timeIntervalSince1970: 1_790_000_000))
            ])
    }

    func testRenewFailureLatchesMachine() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()
        rig.station.leaseRenewError = Boom()

        rig.tick()

        XCTAssertEqual(rig.controller.status, .releaseToArm)
        let renewsAfterFailure = rig.station.leaseRenewCalls.count
        rig.station.leaseRenewError = nil
        rig.tick()
        XCTAssertEqual(
            rig.station.leaseRenewCalls.count, renewsAfterFailure,
            "a latched machine stops renewing")

        rig.central.notify(unitA, epoch: 1, [0x01])
        XCTAssertEqual(rig.keyLog, [press], "no re-key without a release")
        rig.central.notify(unitA, epoch: 1, [0x00])
        rig.central.notify(unitA, epoch: 1, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release, press])
    }

    func testRefusedPressLatchesMachine() {
        let rig = Rig(self, confirmed: [unitA], answered: false)
        rig.attachAndPress()

        XCTAssertEqual(rig.keyLog, [], "refused at the client gate")
        XCTAssertEqual(rig.controller.status, .releaseToArm)
        rig.tick()
        XCTAssertEqual(rig.station.leaseRenewCalls, [], "nothing to renew")

        rig.station.snapshotToReturn = answeredSnapshot()
        rig.session.poll()
        rig.central.notify(unitA, epoch: 1, [0x01])
        XCTAssertEqual(rig.keyLog, [], "held through the answer does not key")

        rig.central.notify(unitA, epoch: 1, [0x00])
        rig.central.notify(unitA, epoch: 1, [0x01])
        XCTAssertEqual(rig.keyLog, [release, press])
    }

    func testVoxCloseDoesNotUnkeyHeldBluetooth() {
        let rig = Rig(self, confirmed: [unitA])
        rig.session.setVoxHangtime(0)
        rig.session.voxGate.config.attackMS = 0  // open on the first loud poll
        rig.session.setVoxEnabled(true)
        rig.station.snapshotToReturn = answeredSnapshot(voxDB: -10)
        rig.session.poll()  // VOX opens

        rig.attachAndPress()
        rig.station.snapshotToReturn = answeredSnapshot(voxDB: -60)
        rig.session.poll()  // VOX closes with the button still held

        XCTAssertEqual(rig.keyLog, ["pttSource(vox,true)", press, "pttSource(vox,false)"])
        XCTAssertEqual(rig.station.stopAllCount, 0, "VOX closing releases only VOX")
    }

    func testRenewAndTickFireWithPollingStopped() {
        let rig = Rig(self, confirmed: [unitA])
        rig.session.start()
        rig.session.stop()  // a non-call pane: the 20 Hz poll is paused
        rig.attachAndPress()
        rig.central.autoReplyRSSI = -50

        rig.tick(8)

        XCTAssertEqual(
            rig.station.leaseRenewCalls,
            Array(repeating: StationPTTSource.bluetooth, count: 8))
        XCTAssertEqual(rig.keyLog, [press], "a healthy link is never cut")
        // Pressed at 1000 ms; keyed probes every 500 ms from there: 1500,
        // 2000, 2500 and 3000 ms within ticks at 1250…3000 ms.
        XCTAssertEqual(
            rig.central.verbs.filter { $0 == "readRSSI" }.count, 4,
            "keyed probes run on the same ticks")
        XCTAssertEqual(rig.controller.lastRSSI, -50)
        XCTAssertEqual(rig.controller.signal, .strong, "-50 dBm is at or above -65")
    }

    func testTwoMissedKeyedProbesReleaseDropAndReconnect() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()
        rig.central.autoReplyRSSI = nil

        // 1250: nothing due. 1500: probe. 2000: that one is unanswered (miss
        // one), probe again. 2500: second miss, release and drop.
        rig.tick(6)

        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertEqual(rig.controller.lastForcedStop?.reason, .linkLost)
        XCTAssertEqual(
            rig.central.log,
            [
                "powerUp", "retrieve", "connect:1", "subscribe:1", "readRSSI:1", "readRSSI:1",
                "cancel", "connect:2",
            ])
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])
        XCTAssertEqual(rig.central.connects.map { $0.id }, [unitA, unitA])
        XCTAssertEqual(rig.controller.status, .reconnecting)
        XCTAssertEqual(
            rig.station.leaseRenewCalls.count, 5, "renewed while keyed, not after the drop")
    }

    /// A failed RSSI read is a miss like a missing reply: two failures in a
    /// row release at the second one, not a tick later.
    func testTwoFailedKeyedReadsReleaseAtTheSecondFailure() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()  // pressed at 1 000 ms
        rig.central.autoFailRSSI = true

        rig.tick(2)  // 1 500 ms: the first probe fails at once (one miss)
        XCTAssertEqual(rig.keyLog, [press], "one failed read keeps the key")

        rig.tick(2)  // 2 000 ms: the second probe fails: release in this step
        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertEqual(rig.controller.lastForcedStop?.reason, .linkLost)
        XCTAssertEqual(
            Array(rig.central.log.suffix(4)), ["readRSSI:1", "readRSSI:1", "cancel", "connect:2"])
    }

    // MARK: - Every way the button goes quiet ends unkeyed

    func testLinkLossMidPressReleasesAndReconnectsOnANewEpoch() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        rig.central.drop(unitA, epoch: 1)

        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertEqual(rig.controller.lastForcedStop?.reason, .linkLost)
        XCTAssertEqual(rig.controller.status, .reconnecting)
        XCTAssertFalse(rig.controller.isLinked)
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])

        rig.central.notify(unitA, epoch: 1, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release], "a notify from the dead link is dropped")

        // Lost while held: the relink is latched, and only a release, then a
        // fresh press, keys again.
        rig.central.linkUp(unitA, epoch: 2)
        XCTAssertEqual(rig.controller.status, .releaseToArm)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 2, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release], "a relink never keys a held button")
        rig.central.notify(unitA, epoch: 2, [0x00])
        rig.central.notify(unitA, epoch: 2, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release, release, press])
    }

    /// Conditional latch: a link lost with the button up relinks Ready, and
    /// the first press past the settle window keys. The relink itself sends
    /// nothing to the station.
    func testIdleLinkLossRelinksReadyAndTheFirstPressKeys() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()
        rig.central.notify(unitA, epoch: 1, [0x00])
        XCTAssertEqual(rig.keyLog, [press, release])

        rig.central.drop(unitA, epoch: 1)

        XCTAssertEqual(rig.keyLog, [press, release], "an idle loss sends nothing")
        XCTAssertNil(rig.controller.lastForcedStop)
        XCTAssertEqual(rig.controller.status, .reconnecting)
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])

        rig.central.linkUp(unitA, epoch: 2)
        XCTAssertEqual(rig.controller.status, .ready)
        XCTAssertEqual(rig.keyLog, [press, release], "a relink never keys by itself")

        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 2, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release, press], "the first press keys")
    }

    /// The same rule across an idle Mac sleep and an idle Bluetooth off and
    /// on: each relink is Ready and its first press keys, with no release
    /// needed first.
    func testIdleSleepAndAdapterCycleRelinkReadyAndTheFirstPressKeys() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()
        rig.central.notify(unitA, epoch: 1, [0x00])

        rig.controller.systemWillSleep()
        XCTAssertEqual(rig.keyLog, [press, release], "an idle sleep sends nothing")
        XCTAssertNil(rig.controller.lastForcedStop)
        XCTAssertEqual(rig.controller.status, .reconnecting)
        rig.controller.systemDidWake()
        rig.central.linkUp(unitA, epoch: 2)
        XCTAssertEqual(rig.controller.status, .ready)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 2, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release, press], "the first press after wake keys")
        rig.central.notify(unitA, epoch: 2, [0x00])

        rig.central.setAdapter(.off)
        XCTAssertEqual(rig.controller.status, .bluetoothOff)
        XCTAssertNil(rig.controller.lastForcedStop, "Bluetooth off while idle is not news")
        rig.central.setAdapter(.on)
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2, 3])
        rig.central.linkUp(unitA, epoch: 3)
        XCTAssertEqual(rig.controller.status, .ready)
        XCTAssertEqual(rig.keyLog, [press, release, press, release], "coming back never keys")
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 3, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release, press, release, press])
    }

    func testAdapterOffMidPressReleases() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        rig.central.setAdapter(.off)

        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertEqual(rig.controller.lastForcedStop?.reason, .adapterOff)
        XCTAssertEqual(rig.controller.status, .bluetoothOff)

        rig.central.setAdapter(.on)
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])
        XCTAssertEqual(rig.keyLog, [press, release], "coming back never keys by itself")
    }

    func testConfigSwitchDetachesAndReleases() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        rig.controller.detach()

        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertNil(rig.controller.lastForcedStop, "a deliberate switch is not a forced stop")
        XCTAssertFalse(rig.ticker.isRunning)
        XCTAssertEqual(rig.controller.status, .disabled)
        XCTAssertFalse(rig.controller.isLinked)
        XCTAssertNil(rig.controller.spec)
        XCTAssertEqual(rig.central.log.last, "cancel")

        rig.central.notify(unitA, epoch: 1, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release], "nothing reaches the station after detach")
    }

    /// B02PTT config, then another config, then the B02PTT config again in
    /// one run. The second attach makes a fresh machine whose adapter starts
    /// unknown, and CoreBluetooth does not report an unchanged adapter again,
    /// so the controller must replay the state it already knows.
    func testReattachAfterConfigSwitchStillKeys() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()
        rig.central.notify(unitA, epoch: 1, [0x00])
        // Released before the switch, so Disable emits no second release.
        rig.controller.detach()
        rig.central.known = [
            BLEPeripheralRef(identifier: unitA, name: BLETestFixtures.advertisedName)
        ]

        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.central.linkUp(unitA, epoch: 2)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 2, [0x01])

        XCTAssertEqual(rig.factory.made.count, 1, "the re-attach reuses the one central")
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])
        XCTAssertEqual(rig.controller.status, .ready)
        XCTAssertEqual(rig.keyLog, [press, release, press], "the re-attached button keys")
    }

    func testSleepReleases() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        rig.controller.systemWillSleep()

        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertEqual(rig.controller.lastForcedStop?.reason, .sleep)
        XCTAssertEqual(rig.controller.status, .reconnecting)
        XCTAssertEqual(rig.central.log.last, "cancel")

        rig.controller.systemDidWake()
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])
        rig.central.linkUp(unitA, epoch: 2)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 2, [0x01])  // still held across the sleep

        XCTAssertEqual(rig.keyLog, [press, release], "never comes back keyed")
        XCTAssertEqual(rig.controller.status, .releaseToArm)

        rig.central.notify(unitA, epoch: 2, [0x00])
        rig.central.notify(unitA, epoch: 2, [0x01])
        XCTAssertEqual(rig.keyLog, [press, release, release, press])
    }

    func testTerminateReleases() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        rig.controller.shutdown()

        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertNil(rig.controller.lastForcedStop)
        XCTAssertFalse(rig.ticker.isRunning)
        XCTAssertEqual(rig.central.log.last, "shutdown")
    }

    func testForgetClearsTheConfirmationAndReleases() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        rig.controller.forgetConfirmedButton()

        XCTAssertEqual(rig.keyLog, [press, release])
        XCTAssertFalse(rig.store.isConfirmed(platformID: unitA, deviceID: BLETestFixtures.deviceID))
        XCTAssertTrue(rig.controller.needsSetup)
        XCTAssertFalse(rig.ticker.isRunning)
    }

    func testIdleLinkLossAndIdleMalformedAreNotForcedStops() {
        let rig = Rig(self, confirmed: [unitA])
        rig.controller.attach(session: rig.session, spec: BLETestFixtures.spec)
        rig.central.known = [
            BLEPeripheralRef(identifier: unitA, name: BLETestFixtures.advertisedName)
        ]
        rig.central.setAdapter(.on)
        rig.central.linkUp(unitA, epoch: 1)
        rig.clock.ms += 1_000

        // The machine releases on a malformed value even when nothing is
        // keyed; the session sees that release, but it is not news.
        rig.central.notify(unitA, epoch: 1, [0x02])
        XCTAssertEqual(rig.keyLog, [release])
        XCTAssertNil(rig.controller.lastForcedStop, "an idle malformed value is not a forced stop")

        rig.central.drop(unitA, epoch: 1)
        XCTAssertEqual(rig.keyLog, [release], "an idle link loss releases nothing")
        XCTAssertNil(rig.controller.lastForcedStop, "an idle link loss is not a forced stop")
        XCTAssertEqual(rig.central.connects.map { $0.epoch }, [1, 2])
    }

    func testMalformedPayloadHexIsLoggedOncePerConnection() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()

        rig.central.notify(unitA, epoch: 1, [0x02])
        rig.central.notify(unitA, epoch: 1, [0x02, 0x03])

        XCTAssertEqual(rig.logged, ["B02PTT-6522 sent an unrecognised value: 02 (connection 1)"])
        XCTAssertEqual(rig.controller.lastForcedStop?.reason, .malformed)
        // The machine releases on every malformed value, and both releases
        // reach the station.
        XCTAssertEqual(rig.keyLog, [press, release, release])

        rig.central.drop(unitA, epoch: 1)
        rig.central.linkUp(unitA, epoch: 2)
        rig.clock.ms += 1_000
        rig.central.notify(unitA, epoch: 2, [0x07])

        XCTAssertEqual(
            rig.logged,
            [
                "B02PTT-6522 sent an unrecognised value: 02 (connection 1)",
                "B02PTT-6522 sent an unrecognised value: 07 (connection 2)",
            ])
    }

    // MARK: - The transport sees exactly the commands the machine asked for

    /// A scripted run through press, probe, link loss, sleep, wake, detach and
    /// quit, compared command for command. An extra subscribe, a stray scan or
    /// a missing cancel fails it.
    ///
    /// The no-write guarantee itself does not rest on this test: it rests on
    /// `BLECentralDriving` having no write method, and on
    /// `ci/guard-ble-ptt.sh` failing the build if anything under `apps/macos`
    /// calls the CoreBluetooth characteristic-write API (run in Step 21).
    func testCentralCommandSequenceIsExact() {
        let rig = Rig(self, confirmed: [unitA])
        rig.attachAndPress()
        rig.central.autoReplyRSSI = -50
        rig.tick(2)  // 1250: nothing due; 1500: the first keyed probe
        rig.central.drop(unitA, epoch: 1)
        rig.central.linkUp(unitA, epoch: 2)
        rig.controller.systemWillSleep()
        rig.controller.systemDidWake()
        rig.central.linkUp(unitA, epoch: 3)
        rig.controller.detach()
        rig.controller.shutdown()

        XCTAssertEqual(rig.factory.made.count, 1)
        XCTAssertEqual(
            rig.central.log,
            [
                "powerUp", "retrieve", "connect:1", "subscribe:1", "readRSSI:1",
                "connect:2", "subscribe:2", "cancel",
                "retrieve", "connect:3", "subscribe:3", "cancel",
                "shutdown",
            ])
        XCTAssertEqual(rig.keyLog, [press, release])
    }

    // MARK: - The production ticker

    func testDispatchTickerFiresOnTheMainQueueAndStops() {
        let ticker = DispatchBluetoothPTTTicker()
        let fired = expectation(description: "three ticks")
        fired.expectedFulfillmentCount = 3
        var count = 0
        ticker.start(everyMS: 10) {
            count += 1
            XCTAssertTrue(Thread.isMainThread)
            if count <= 3 { fired.fulfill() }
        }
        wait(for: [fired], timeout: 2)
        ticker.stop()
        let atStop = count
        let quiet = expectation(description: "no tick after stop")
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { quiet.fulfill() }
        wait(for: [quiet], timeout: 1)
        XCTAssertEqual(count, atStop)
    }
}
