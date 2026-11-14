// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation
import XCTest

@testable import AstarCore

/// Fixed values every Bluetooth PTT test shares, so a test never spells a UUID
/// or a device id twice.
enum BLETestFixtures {
    static let deviceID = "inrico-b02ptt"
    static let spec = BluetoothPTTSpec(deviceID: deviceID)
    static let unitA = UUID(uuidString: "6A1B2C3D-0000-4000-8000-00000000000A")!
    static let unitB = UUID(uuidString: "6A1B2C3D-0000-4000-8000-00000000000B")!
    static let advertisedName = "B02PTT-6522"
    /// The table row, through the real FFI, so the fake never hard-codes a UUID.
    static let device: BLEButtonDevice = BLEButtonDevices.device(id: deviceID)!
    static let buttonCharacteristic = BLEGattCharacteristic(
        service: device.serviceUUID, uuid: device.characteristicUUID, properties: 0x10)
    static let leasePress = "pttLeasePress(bluetooth,1500ms)"
    static let release = "pttSource(bluetooth,false)"
    static let renew = "pttLeaseRenew(bluetooth)"
}

/// The keying-relevant part of `FakeStation.pttLog`: renewals and time-out
/// pushes removed, so a test can compare presses and releases with `==`.
func bluetoothKeyLog(_ station: FakeStation) -> [String] {
    station.pttLog.filter { !$0.hasPrefix("setTxTimeout") && $0 != BLETestFixtures.renew }
}

/// A fresh defaults suite that `test` removes in its teardown, so no Bluetooth
/// test leaves a domain behind or reads one an earlier run wrote.
func bluetoothScratchDefaults(cleanup test: XCTestCase) -> UserDefaults {
    let suite = "astar.tests.bleptt.\(UUID().uuidString)"
    let defaults = UserDefaults(suiteName: suite)!
    defaults.removePersistentDomain(forName: suite)
    test.addTeardownBlock { UserDefaults.standard.removePersistentDomain(forName: suite) }
    return defaults
}

/// A session whose every store points at `defaults`, so nothing a test does
/// (a callsign, the transmit time-out, a saved node) reaches the test host's
/// standard domain.
func hermeticSession(_ station: StationDriving, defaults: UserDefaults) -> CallSession {
    CallSession(
        station: station, audioStore: UserDefaultsAudioSettingsStore(defaults),
        directoryStore: UserDefaultsNodeDirectoryStore(defaults),
        micProfileStore: UserDefaultsMicProfileStore(defaults), userDefaults: defaults)
}

/// An answered call with a quiet mic: the only state in which a press may key.
func answeredSnapshot(voxDB: Float = -60) -> CallSnapshot {
    CallSnapshot(
        status: .answered, ptt: false, remotePTT: false,
        txDB: -60, rxDB: -60, inputDB: voxDB, rttMS: 20, voxDB: voxDB)
}

/// A scriptable transport. It records every command the controller issues, in
/// order, as `"verb"` or `"verb:epoch"`, so tests can assert the exact
/// sequence. It has no write verb to record because the protocol it conforms
/// to has no write method.
@MainActor
final class FakeBLECentral: BLECentralDriving {
    weak var delegate: BLECentralDelegate?
    private(set) var log: [String] = []
    private(set) var connects: [(id: UUID, epoch: UInt32)] = []
    /// Returned by `retrievePeripherals` when asked for by identifier.
    var known: [BLEPeripheralRef] = []
    /// Returned by `retrievePeripherals` whatever identifiers were asked for
    /// (CoreBluetooth's `retrieveConnectedPeripherals(withServices:)`).
    var connectedWithService: [BLEPeripheralRef] = []
    /// When set, every `readRSSI` is answered at once with this value.
    var autoReplyRSSI: Int16?
    /// When true, every `readRSSI` is answered at once as a failed read
    /// (`rssi: nil`), which the machine counts as a miss. Wins over
    /// `autoReplyRSSI`.
    var autoFailRSSI = false

    var verbs: [String] { log.map { String($0.split(separator: ":").first ?? "") } }

    func powerUp() { log.append("powerUp") }
    func retrievePeripherals(identifiers: [UUID], connectedWithService service: String)
        -> [BLEPeripheralRef]
    {
        log.append("retrieve")
        return known.filter { identifiers.contains($0.identifier) } + connectedWithService
    }
    func startScan() { log.append("startScan") }
    func stopScan() { log.append("stopScan") }
    func connect(_ id: UUID, epoch: UInt32, service: String) {
        log.append("connect:\(epoch)")
        connects.append((id, epoch))
    }
    func subscribe(_ id: UUID, epoch: UInt32, service: String, characteristic: String) {
        log.append("subscribe:\(epoch)")
    }
    func readRSSI(_ id: UUID, epoch: UInt32) {
        log.append("readRSSI:\(epoch)")
        if autoFailRSSI {
            delegate?.centralReadRSSI(id, epoch: epoch, rssi: nil)
        } else if let autoReplyRSSI {
            delegate?.centralReadRSSI(id, epoch: epoch, rssi: autoReplyRSSI)
        }
    }
    func cancelConnection(_ id: UUID) { log.append("cancel") }
    func shutdown() { log.append("shutdown") }

    // MARK: - Scripting the callbacks

    func setAdapter(_ state: BLEAdapterState) { delegate?.centralAdapterChanged(state) }

    func advertise(_ id: UUID, name: String?, rssi: Int16 = -55) {
        delegate?.centralDiscovered(BLEPeripheralRef(identifier: id, name: name), rssi: rssi)
    }

    /// Connect, report the characteristics, and confirm the subscription, but
    /// only if the controller actually asked to subscribe on this epoch.
    func linkUp(
        _ id: UUID, epoch: UInt32,
        characteristics: [BLEGattCharacteristic] = [BLETestFixtures.buttonCharacteristic]
    ) {
        delegate?.centralConnected(id, epoch: epoch)
        delegate?.centralDiscoveredGATT(id, epoch: epoch, characteristics: characteristics)
        if log.last == "subscribe:\(epoch)" { delegate?.centralSubscribed(id, epoch: epoch) }
    }

    func notify(_ id: UUID, epoch: UInt32, _ bytes: [UInt8]) {
        delegate?.centralNotified(id, epoch: epoch, bytes: bytes)
    }

    func drop(_ id: UUID, epoch: UInt32) { delegate?.centralDisconnected(id, epoch: epoch) }
}

/// Counts constructions. No Bluetooth manager may exist until a config that
/// uses the button is attached with a confirmed unit, or the user starts
/// setup, so someone who never uses the feature never sees a permission
/// prompt.
@MainActor
final class CountingCentralFactory {
    private(set) var made: [FakeBLECentral] = []
    func make() -> BLECentralDriving {
        let central = FakeBLECentral()
        made.append(central)
        return central
    }
}

/// A ticker the test fires by hand, so renewals and probes happen exactly
/// when the test says.
@MainActor
final class ManualTicker: BluetoothPTTTicker {
    private(set) var periodMS: UInt32?
    private(set) var startCount = 0
    private var tick: (@MainActor () -> Void)?
    var isRunning: Bool { tick != nil }

    func start(everyMS: UInt32, _ tick: @escaping @MainActor () -> Void) {
        periodMS = everyMS
        startCount += 1
        self.tick = tick
    }
    func stop() { tick = nil }
    func fire() { tick?() }
}

/// The controller's injected monotonic clock, in milliseconds.
final class TestClock {
    var ms: UInt64 = 0
}
