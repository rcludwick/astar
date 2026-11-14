// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// b02ptt-watch.swift — a listen-only watch on an Inrico B02PTT's PTT button,
// for the hardware checks that tune the Bluetooth PTT button before it ships.
//
// It links nothing from astar and never constructs a Station, so it cannot
// key anything. It scans, connects, subscribes to the button characteristic
// (the CCCD enable is the only write it causes) and reads RSSI. It prints the
// adapter state, discovery, GATT acceptance, RSSI round trips and timestamped
// button edges, which answer:
//   1. does the device send its current state on subscribe, and how late;
//   2. how long a readRSSI round trip takes, and how often a 500 ms read goes
//      unanswered on a healthy link;
//   3. whether LE advertising stops while HFP is connected, and whether
//      retrieveConnectedPeripherals still finds the unit;
//   4. the press-to-notify timing;
//   5. what RSSI "across the room" and "next room" read.
//
// Run it by hand only, from a terminal, never from CI or an agent:
//   IAX_BLE_TESTS=1 swift Tools/b02ptt-watch.swift
// Stop it with Ctrl-C.

import CoreBluetooth
import Foundation

let buttonService = CBUUID(string: "89A8591D-BB19-485B-9F59-58492BC33E24")
let buttonCharacteristic = CBUUID(string: "894C8042-E841-461C-A5C9-5A73D25DB08E")
let namePrefix = "B02PTT"

guard ProcessInfo.processInfo.environment["IAX_BLE_TESTS"] == "1" else {
    FileHandle.standardError.write(
        Data("b02ptt-watch: hardware check; set IAX_BLE_TESTS=1 to run it.\n".utf8))
    exit(2)
}

let started = Date()

func log(_ line: String) {
    print(String(format: "%9.3f  ", Date().timeIntervalSince(started)) + line)
    fflush(stdout)
}

func milliseconds(since date: Date) -> Int {
    Int((Date().timeIntervalSince(date) * 1000).rounded())
}

final class Watch: NSObject, CBCentralManagerDelegate, CBPeripheralDelegate {
    private var central: CBCentralManager?
    private var peripheral: CBPeripheral?
    private var subscribedAt: Date?
    private var rssiSentAt: Date?
    private var rssiReads = 0
    private var rssiMisses = 0
    private var rssiTimer: Timer?

    func begin() {
        central = CBCentralManager(
            delegate: self, queue: .main,
            options: [CBCentralManagerOptionShowPowerAlertKey: false])
    }

    // MARK: Central

    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        log("adapter state \(central.state.rawValue) (5 is poweredOn)")
        guard central.state == .poweredOn else { return }
        let connected = central.retrieveConnectedPeripherals(withServices: [buttonService])
        log("already connected with the button service: \(connected.map { $0.name ?? "?" })")
        if let first = connected.first {
            connect(first)
            return
        }
        // Unfiltered: the button service is not in the advert.
        central.scanForPeripherals(withServices: nil, options: nil)
        log("scanning")
    }

    func centralManager(
        _ central: CBCentralManager, didDiscover peripheral: CBPeripheral,
        advertisementData: [String: Any], rssi RSSI: NSNumber
    ) {
        let advertised = advertisementData[CBAdvertisementDataLocalNameKey] as? String
        guard let name = peripheral.name ?? advertised, name.hasPrefix(namePrefix),
            self.peripheral == nil
        else { return }
        log("discovered \(name), advert RSSI \(RSSI) dBm, identifier \(peripheral.identifier)")
        central.stopScan()
        connect(peripheral)
    }

    private func connect(_ target: CBPeripheral) {
        peripheral = target
        target.delegate = self
        log("connecting to \(target.name ?? "?")")
        central?.connect(target, options: nil)
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        log("connected; discovering the button service")
        peripheral.discoverServices([buttonService])
    }

    func centralManager(
        _ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?
    ) {
        log("connect failed: \(error?.localizedDescription ?? "no error"); scanning again")
        self.peripheral = nil
        central.scanForPeripherals(withServices: nil, options: nil)
    }

    func centralManager(
        _ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral,
        error: Error?
    ) {
        log("disconnected: \(error?.localizedDescription ?? "no error"); reconnecting")
        stopProbing()
        subscribedAt = nil
        central.connect(peripheral, options: nil)
    }

    // MARK: Peripheral

    func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        let found = peripheral.services?.map(\.uuid.uuidString) ?? []
        guard let service = peripheral.services?.first(where: { $0.uuid == buttonService }) else {
            log("GATT: button service missing (error \(error?.localizedDescription ?? "none")); found \(found)")
            return
        }
        peripheral.discoverCharacteristics(nil, for: service)
    }

    func peripheral(
        _ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService,
        error: Error?
    ) {
        guard
            let characteristic = service.characteristics?.first(where: {
                $0.uuid == buttonCharacteristic
            }),
            characteristic.properties.contains(.notify)
        else {
            log("GATT: rejected, no button characteristic with notify")
            return
        }
        log("GATT: accepted, properties 0x\(String(characteristic.properties.rawValue, radix: 16))")
        peripheral.setNotifyValue(true, for: characteristic)
    }

    func peripheral(
        _ peripheral: CBPeripheral,
        didUpdateNotificationStateFor characteristic: CBCharacteristic, error: Error?
    ) {
        log("notifying \(characteristic.isNotifying), error \(error?.localizedDescription ?? "none")")
        guard characteristic.isNotifying else { return }
        subscribedAt = Date()
        startProbing()
    }

    func peripheral(
        _ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic,
        error: Error?
    ) {
        let bytes = [UInt8](characteristic.value ?? Data())
        let hex = bytes.map { String(format: "%02X", $0) }.joined(separator: " ")
        let since = subscribedAt.map { "\(milliseconds(since: $0)) ms after subscribe" }
        log("button [\(hex)] (\(since ?? "before subscribe"))")
    }

    // MARK: RSSI

    private func startProbing() {
        stopProbing()
        rssiTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            self?.probe()
        }
    }

    private func stopProbing() {
        rssiTimer?.invalidate()
        rssiTimer = nil
        rssiSentAt = nil
    }

    private func probe() {
        guard let peripheral else { return }
        if let sent = rssiSentAt {
            rssiMisses += 1
            log("RSSI read unanswered after \(milliseconds(since: sent)) ms (\(rssiMisses) of \(rssiReads))")
        }
        rssiSentAt = Date()
        rssiReads += 1
        peripheral.readRSSI()
    }

    func peripheral(_ peripheral: CBPeripheral, didReadRSSI RSSI: NSNumber, error: Error?) {
        let roundTrip = rssiSentAt.map { milliseconds(since: $0) }
        rssiSentAt = nil
        if let error {
            log("RSSI read failed after \(roundTrip ?? -1) ms: \(error.localizedDescription)")
            return
        }
        log("RSSI \(RSSI) dBm, round trip \(roundTrip ?? -1) ms")
    }
}

let watch = Watch()
watch.begin()
signal(SIGINT) { _ in exit(0) }
RunLoop.main.run()
