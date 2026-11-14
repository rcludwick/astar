// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarStation
    import CoreBluetooth
    import Foundation

    /// The Bluetooth LE transport for the PTT button, and the ONLY code in astar
    /// that constructs a `CBCentralManager` (`ci/guard-ble-ptt.sh` enforces it).
    ///
    /// It is dumb on purpose. It turns CoreBluetooth callbacks into
    /// `BLECentralDelegate` calls and executes the controller's commands. Every
    /// decision about whether astar transmits is made elsewhere: the button
    /// machine and the engine's arbiter, in Rust. Keeping CoreBluetooth in the
    /// app target means `swift test` on AstarCore can never create a manager.
    ///
    /// What it does:
    /// - Creates the manager lazily, in `powerUp()`, never in `init`. So macOS
    ///   asks for Bluetooth permission only after the user has chosen the button
    ///   as a config's PTT hardware.
    /// - Passes `ShowPowerAlert: false`, so astar's own "Bluetooth is off" is the
    ///   only message.
    /// - Defers, never fails, until the adapter is on. CoreBluetooth reports
    ///   `poweredOn` some time after the manager exists, and can do nothing
    ///   before that. So a scan or connect asked for earlier is held and run
    ///   at `poweredOn`. Failing it instead would only invite a retry loop, and
    ///   the adapter callback already tells the controller the state.
    /// - Scans unfiltered. The vendor service is not in the button's advert, so a
    ///   service-filtered scan would find nothing. The controller post-filters by
    ///   name. Only advertisers whose name matches a known button are retained,
    ///   so a long scan among phones and beacons cannot grow memory.
    /// - Discovers only the device's service and that service's characteristics.
    ///   A discovery error is treated as a broken link (cancel, then reconnect),
    ///   never as "this device lacks the service": that verdict skips the unit
    ///   for the session, and one transient error must not do that to a real,
    ///   confirmed button.
    /// - Subscribes by enabling notifications. That descriptor write is the only
    ///   write astar ever makes to the device; there is no characteristic write
    ///   anywhere in the app (`ci/guard-ble-ptt.sh`). Liveness is `readRSSI`, a
    ///   controller-level query, not GATT traffic.
    /// - Echoes back, in every callback, the epoch the controller gave for that
    ///   connect attempt, so a stale callback from a dead link cannot key.
    @MainActor
    final class CoreBluetoothCentral: NSObject, BLECentralDriving, CBCentralManagerDelegate,
        CBPeripheralDelegate
    {
        weak var delegate: BLECentralDelegate?

        /// One peripheral this central holds: one advertising a button's name,
        /// one retrieved, or one the controller asked to connect.
        /// `pendingEpoch` is the connect attempt in flight. `liveEpoch` is the
        /// attempt that actually connected; peripheral callbacks report it.
        /// They are kept apart so that the disconnect of an old link, arriving
        /// after the controller has already started the next attempt, still
        /// reports the old epoch, which the machine then ignores.
        private struct Link {
            let peripheral: CBPeripheral
            var service: CBUUID?
            var characteristic: CBUUID?
            var pendingEpoch: UInt32?
            var liveEpoch: UInt32?
        }

        private var manager: CBCentralManager?
        /// Peripherals retrieved, connected, or advertising a button's name.
        private var links: [UUID: Link] = [:]
        /// A scan asked for before the adapter reported `poweredOn`.
        private var scanWanted = false
        /// Connects asked for before the adapter reported `poweredOn`: the
        /// newest epoch and service for each identifier.
        private var pendingConnects: [UUID: (epoch: UInt32, service: String)] = [:]

        /// Constructs nothing. See `powerUp()`.
        override init() {
            super.init()
        }

        // MARK: - BLECentralDriving

        func powerUp() {
            if let manager {
                // Already up: tell the controller where the adapter is now, on
                // the next turn of the run loop rather than re-entering it.
                if let state = Self.adapterState(manager.state) {
                    deliverLater { $0.centralAdapterChanged(state) }
                }
                return
            }
            manager = CBCentralManager(
                delegate: self, queue: .main,
                options: [CBCentralManagerOptionShowPowerAlertKey: false])
        }

        func retrievePeripherals(identifiers: [UUID], connectedWithService service: String)
            -> [BLEPeripheralRef]
        {
            // Before `poweredOn` nothing can be retrieved. The controller asks
            // again when it hears `.on`.
            guard let manager, manager.state == .poweredOn else { return [] }
            var found = manager.retrievePeripherals(withIdentifiers: identifiers)
            // A dual-mode device can stop advertising while it is LE-connected to
            // the system, so ask for already-connected peripherals with the
            // service too.
            if let serviceID = Self.cbuuid(service) {
                for peripheral in manager.retrieveConnectedPeripherals(withServices: [serviceID])
                where !found.contains(where: { $0.identifier == peripheral.identifier }) {
                    found.append(peripheral)
                }
            }
            for peripheral in found { remember(peripheral) }
            return found.map { BLEPeripheralRef(identifier: $0.identifier, name: $0.name) }
        }

        func startScan() {
            guard let manager, manager.state == .poweredOn else {
                // Not on yet: scan once it is (see `runDeferred`).
                scanWanted = true
                return
            }
            guard !manager.isScanning else { return }
            manager.scanForPeripherals(
                withServices: nil,
                options: [CBCentralManagerScanOptionAllowDuplicatesKey: false])
        }

        func stopScan() {
            scanWanted = false
            guard let manager, manager.isScanning else { return }
            manager.stopScan()
        }

        func connect(_ id: UUID, epoch: UInt32, service: String) {
            guard let manager, manager.state == .poweredOn else {
                // Not on yet. Hold the request, keeping only the newest epoch,
                // and send no failure: the adapter callback has already told
                // the controller, and a failure here would come straight back
                // as a retry.
                pendingConnects[id] = (epoch: epoch, service: service)
                return
            }
            guard let serviceID = Self.cbuuid(service),
                let peripheral = links[id]?.peripheral
                    ?? manager.retrievePeripherals(withIdentifiers: [id]).first
            else {
                // The adapter is on but CoreBluetooth has never seen this
                // identifier. That is a real failure, reported once.
                deliverLater { $0.centralConnectFailed(id, epoch: epoch) }
                return
            }
            var link = links[id] ?? Link(peripheral: peripheral)
            link.service = serviceID
            link.pendingEpoch = epoch
            links[id] = link
            peripheral.delegate = self
            // A pending connect has no timeout in CoreBluetooth. It completes
            // when the device comes back, which is the reconnect behaviour we want.
            manager.connect(peripheral, options: nil)
        }

        func subscribe(_ id: UUID, epoch: UInt32, service: String, characteristic: String) {
            guard var link = links[id], link.liveEpoch == epoch,
                let serviceID = Self.cbuuid(service),
                let characteristicID = Self.cbuuid(characteristic),
                let target = link.peripheral.services?
                    .first(where: { $0.uuid == serviceID })?
                    .characteristics?
                    .first(where: { $0.uuid == characteristicID })
            else {
                deliverLater { $0.centralUnsubscribed(id, epoch: epoch) }
                return
            }
            link.characteristic = characteristicID
            links[id] = link
            // The CCCD enable: the only write astar ever makes to the device.
            link.peripheral.setNotifyValue(true, for: target)
        }

        func readRSSI(_ id: UUID, epoch: UInt32) {
            guard let link = links[id], link.liveEpoch == epoch,
                link.peripheral.state == .connected
            else {
                deliverLater { $0.centralReadRSSI(id, epoch: epoch, rssi: nil) }
                return
            }
            link.peripheral.readRSSI()
        }

        func cancelConnection(_ id: UUID) {
            pendingConnects[id] = nil
            guard let manager, let link = links[id] else { return }
            manager.cancelPeripheralConnection(link.peripheral)
        }

        func shutdown() {
            pendingConnects.removeAll()
            scanWanted = false
            guard let manager else { return }
            if manager.isScanning { manager.stopScan() }
            for link in links.values where link.peripheral.state != .disconnected {
                manager.cancelPeripheralConnection(link.peripheral)
            }
            links.removeAll()
            manager.delegate = nil
            self.manager = nil
        }

        // MARK: - CBCentralManagerDelegate
        //
        // The manager delivers on the main queue (see `powerUp`), so each
        // callback asserts main-actor isolation and hops straight in. Values are
        // copied out of the CoreBluetooth objects first.

        nonisolated func centralManagerDidUpdateState(_ central: CBCentralManager) {
            let state = central.state
            MainActor.assumeIsolated { self.adapterDidChange(state) }
        }

        nonisolated func centralManager(
            _ central: CBCentralManager, didDiscover peripheral: CBPeripheral,
            advertisementData: [String: Any], rssi rssiNumber: NSNumber
        ) {
            let advertised = advertisementData[CBAdvertisementDataLocalNameKey] as? String
            let name = advertised ?? peripheral.name
            let rssi = Self.rssi(rssiNumber)
            MainActor.assumeIsolated { self.discovered(peripheral, name: name, rssi: rssi) }
        }

        nonisolated func centralManager(
            _ central: CBCentralManager, didConnect peripheral: CBPeripheral
        ) {
            let id = peripheral.identifier
            MainActor.assumeIsolated { self.didConnect(id) }
        }

        nonisolated func centralManager(
            _ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?
        ) {
            let id = peripheral.identifier
            MainActor.assumeIsolated { self.didFailToConnect(id) }
        }

        nonisolated func centralManager(
            _ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral,
            error: Error?
        ) {
            let id = peripheral.identifier
            MainActor.assumeIsolated { self.didDisconnect(id) }
        }

        // MARK: - CBPeripheralDelegate

        nonisolated func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
            let id = peripheral.identifier
            let failed = error != nil
            MainActor.assumeIsolated { self.servicesDiscovered(id, failed: failed) }
        }

        nonisolated func peripheral(
            _ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService,
            error: Error?
        ) {
            let id = peripheral.identifier
            let serviceUUID = service.uuid.uuidString
            let failed = error != nil
            let found: [BLEGattCharacteristic] = (service.characteristics ?? []).map {
                BLEGattCharacteristic(
                    service: serviceUUID, uuid: $0.uuid.uuidString,
                    properties: UInt8(truncatingIfNeeded: $0.properties.rawValue))
            }
            MainActor.assumeIsolated {
                self.characteristicsDiscovered(
                    id, serviceUUID: serviceUUID, failed: failed, found: found)
            }
        }

        nonisolated func peripheral(
            _ peripheral: CBPeripheral,
            didUpdateNotificationStateFor characteristic: CBCharacteristic,
            error: Error?
        ) {
            let id = peripheral.identifier
            let uuid = characteristic.uuid.uuidString
            let notifying = error == nil && characteristic.isNotifying
            MainActor.assumeIsolated {
                self.notificationStateChanged(id, uuid: uuid, notifying: notifying)
            }
        }

        nonisolated func peripheral(
            _ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic,
            error: Error?
        ) {
            let id = peripheral.identifier
            let uuid = characteristic.uuid.uuidString
            // A failed update is delivered as an empty payload. The machine
            // decodes that as Malformed, which releases and latches, so an
            // error can only ever end a transmission.
            let bytes: [UInt8] = error == nil ? [UInt8](characteristic.value ?? Data()) : []
            MainActor.assumeIsolated { self.valueArrived(id, uuid: uuid, bytes: bytes) }
        }

        /// The peripheral invalidated services while the link stays up (a GATT
        /// database change, or services re-resolved when HFP reconnects). If
        /// the button's service is among them, notifications have stopped
        /// with no `didUpdateNotificationStateFor`, so a held button would
        /// never send its `00`. Report it as the unsubscribe it is, in this
        /// same step: the controller releases (latching only if the button
        /// was down), cancels the link and reconnects. The later
        /// `didDisconnectPeripheral` carries the old epoch and is ignored.
        nonisolated func peripheral(
            _ peripheral: CBPeripheral, didModifyServices invalidatedServices: [CBService]
        ) {
            let id = peripheral.identifier
            let invalidated = invalidatedServices.map(\.uuid)
            MainActor.assumeIsolated { self.servicesInvalidated(id, invalidated: invalidated) }
        }

        nonisolated func peripheral(
            _ peripheral: CBPeripheral, didReadRSSI rssiNumber: NSNumber, error: Error?
        ) {
            let id = peripheral.identifier
            let rssi: Int16? = error == nil ? Self.rssi(rssiNumber) : nil
            MainActor.assumeIsolated { self.rssiArrived(id, rssi: rssi) }
        }

        // MARK: - Main-actor handlers

        private func adapterDidChange(_ state: CBManagerState) {
            switch state {
            case .poweredOn:
                runDeferred()
            case .unknown:
                // Not reported yet: keep what was deferred for `poweredOn`.
                break
            default:
                // Below poweredOn, CoreBluetooth invalidates every peripheral
                // and may send no disconnect callbacks. Forget them all, and
                // what was deferred too. The controller treats this change as
                // the release, and asks again once the adapter is back on.
                links.removeAll()
                pendingConnects.removeAll()
                scanWanted = false
            }
            guard let mapped = Self.adapterState(state) else { return }
            delegate?.centralAdapterChanged(mapped)
        }

        /// Run what was asked for before the adapter came on. This runs before
        /// the controller hears `.on`, so a connect the controller issues in
        /// answer, on a newer epoch, replaces a deferred one and is never
        /// replaced by it.
        private func runDeferred() {
            let connects = pendingConnects
            pendingConnects.removeAll()
            for (id, request) in connects {
                connect(id, epoch: request.epoch, service: request.service)
            }
            if scanWanted {
                scanWanted = false
                startScan()
            }
        }

        private func discovered(_ peripheral: CBPeripheral, name: String?, rssi: Int16?) {
            // Retain a handle only on what could be the button. Everything else
            // in range (phones and watches on rotating addresses, beacons) is
            // passed on for the controller to drop, but not kept. A confirmed
            // unit advertising under another name is still reachable, because
            // `connect` falls back to `retrievePeripherals(withIdentifiers:)`.
            if BLEButtonDevices.match(advertisedName: name) != nil { remember(peripheral) }
            delegate?.centralDiscovered(
                BLEPeripheralRef(identifier: peripheral.identifier, name: name), rssi: rssi)
        }

        private func didConnect(_ id: UUID) {
            guard var link = links[id], let epoch = link.pendingEpoch else { return }
            link.liveEpoch = epoch
            link.pendingEpoch = nil
            link.characteristic = nil
            links[id] = link
            delegate?.centralConnected(id, epoch: epoch)
            guard let service = link.service else { return }
            link.peripheral.discoverServices([service])
        }

        private func didFailToConnect(_ id: UUID) {
            guard var link = links[id], let epoch = link.pendingEpoch else { return }
            link.pendingEpoch = nil
            links[id] = link
            delegate?.centralConnectFailed(id, epoch: epoch)
        }

        private func didDisconnect(_ id: UUID) {
            guard var link = links[id] else { return }
            let epoch: UInt32
            if let live = link.liveEpoch {
                epoch = live
                link.liveEpoch = nil
            } else if let pending = link.pendingEpoch {
                epoch = pending
                link.pendingEpoch = nil
            } else {
                return
            }
            link.characteristic = nil
            links[id] = link
            delegate?.centralDisconnected(id, epoch: epoch)
        }

        private func servicesDiscovered(_ id: UUID, failed: Bool) {
            guard let link = links[id], let epoch = link.liveEpoch, let wanted = link.service
            else { return }
            guard !failed else {
                // A discovery error says nothing about whether the device has
                // the service. Treat it as a broken link: cancel, and the
                // disconnect callback releases (if pressed) and reconnects.
                dropBrokenLink(link)
                return
            }
            guard let service = link.peripheral.services?.first(where: { $0.uuid == wanted })
            else {
                // Discovery succeeded and the service is truly absent: the
                // controller rejects the lookalike.
                delegate?.centralDiscoveredGATT(id, epoch: epoch, characteristics: [])
                return
            }
            link.peripheral.discoverCharacteristics(nil, for: service)
        }

        private func characteristicsDiscovered(
            _ id: UUID, serviceUUID: String, failed: Bool, found: [BLEGattCharacteristic]
        ) {
            guard let link = links[id], let epoch = link.liveEpoch,
                link.service?.uuidString == serviceUUID
            else { return }
            guard !failed else {
                // As above: an error is a broken link, not a lookalike.
                dropBrokenLink(link)
                return
            }
            delegate?.centralDiscoveredGATT(id, epoch: epoch, characteristics: found)
        }

        private func servicesInvalidated(_ id: UUID, invalidated: [CBUUID]) {
            guard let link = links[id], let epoch = link.liveEpoch, let service = link.service,
                invalidated.contains(service)
            else { return }
            delegate?.centralUnsubscribed(id, epoch: epoch)
        }

        /// Cancel a link whose GATT discovery failed. CoreBluetooth answers
        /// with `didDisconnectPeripheral`, which reports the live epoch, so
        /// the controller releases if pressed and reconnects as for any lost
        /// link.
        private func dropBrokenLink(_ link: Link) {
            manager?.cancelPeripheralConnection(link.peripheral)
        }

        private func notificationStateChanged(_ id: UUID, uuid: String, notifying: Bool) {
            guard let link = links[id], let epoch = link.liveEpoch,
                link.characteristic?.uuidString == uuid
            else { return }
            if notifying {
                delegate?.centralSubscribed(id, epoch: epoch)
            } else {
                delegate?.centralUnsubscribed(id, epoch: epoch)
            }
        }

        private func valueArrived(_ id: UUID, uuid: String, bytes: [UInt8]) {
            guard let link = links[id], let epoch = link.liveEpoch,
                link.characteristic?.uuidString == uuid
            else { return }
            delegate?.centralNotified(id, epoch: epoch, bytes: bytes)
        }

        private func rssiArrived(_ id: UUID, rssi: Int16?) {
            guard let link = links[id], let epoch = link.liveEpoch else { return }
            delegate?.centralReadRSSI(id, epoch: epoch, rssi: rssi)
        }

        // MARK: - Helpers

        private func remember(_ peripheral: CBPeripheral) {
            if links[peripheral.identifier] == nil {
                links[peripheral.identifier] = Link(peripheral: peripheral)
            }
        }

        /// Answers that would otherwise come back synchronously, from inside
        /// the controller's own call, go out on the next main-actor turn
        /// instead, so the controller is never re-entered mid-update.
        private func deliverLater(_ body: @escaping @MainActor (BLECentralDelegate) -> Void) {
            Task { @MainActor [weak self] in
                guard let delegate = self?.delegate else { return }
                body(delegate)
            }
        }

        /// `CBUUID(string:)` raises on a malformed string. Going through `UUID`
        /// makes a bad string a nil instead of a crash. Every string here comes
        /// from the device table, so all of them are full 128-bit UUIDs.
        nonisolated private static func cbuuid(_ string: String) -> CBUUID? {
            UUID(uuidString: string).map { CBUUID(nsuuid: $0) }
        }

        /// CoreBluetooth reports 127 when no RSSI is available.
        nonisolated private static func rssi(_ value: NSNumber) -> Int16? {
            let raw = value.intValue
            return raw == 127 ? nil : Int16(clamping: raw)
        }

        /// `.unknown` is CoreBluetooth's "not reported yet", and the machine
        /// treats an unreported adapter the same way, so it maps to nothing. A
        /// state added in a future SDK maps to `.off`: it fails safe, releasing
        /// rather than keying.
        nonisolated private static func adapterState(_ state: CBManagerState) -> BLEAdapterState? {
            switch state {
            case .poweredOn: return .on
            case .poweredOff: return .off
            case .unauthorized: return .unauthorized
            case .resetting: return .resetting
            case .unsupported: return .unsupported
            case .unknown: return nil
            @unknown default: return .off
            }
        }
    }
#endif
