// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// A peripheral as the transport saw it: CoreBluetooth's per-Mac identifier
/// and whatever name it advertised. They travel together because a
/// confirmation binds to the identifier, never the name, so a neighbour's
/// button that advertises the same name can never key.
public struct BLEPeripheralRef: Hashable, Sendable {
    public let identifier: UUID
    public let name: String?

    public init(identifier: UUID, name: String?) {
        self.identifier = identifier
        self.name = name
    }
}

/// One discovered characteristic, in the strings CoreBluetooth prints. The
/// transport does not know the device table. It hands these to the controller,
/// which asks `BLEButtonDevices.acceptsGATT`, so the acceptance rule lives in
/// Rust and in one place.
public struct BLEGattCharacteristic: Hashable, Sendable {
    public let service: String
    public let uuid: String
    public let properties: UInt8

    public init(service: String, uuid: String, properties: UInt8) {
        self.service = service
        self.uuid = uuid
        self.properties = properties
    }
}

/// Callbacks from the transport, delivered on the main actor. Each one is
/// tagged with the epoch of the connect attempt it belongs to, so a late
/// callback from a dead link can be recognised and dropped instead of keying.
@MainActor
public protocol BLECentralDelegate: AnyObject {
    func centralAdapterChanged(_ state: BLEAdapterState)
    func centralDiscovered(_ peripheral: BLEPeripheralRef, rssi: Int16?)
    func centralConnected(_ id: UUID, epoch: UInt32)
    func centralConnectFailed(_ id: UUID, epoch: UInt32)
    /// `[]` means the button's service is missing on this peripheral.
    func centralDiscoveredGATT(_ id: UUID, epoch: UInt32, characteristics: [BLEGattCharacteristic])
    func centralSubscribed(_ id: UUID, epoch: UInt32)
    func centralUnsubscribed(_ id: UUID, epoch: UInt32)
    func centralNotified(_ id: UUID, epoch: UInt32, bytes: [UInt8])
    /// `nil` means the RSSI read failed. The machine counts that as a missed probe.
    func centralReadRSSI(_ id: UUID, epoch: UInt32, rssi: Int16?)
    func centralDisconnected(_ id: UUID, epoch: UInt32)
}

/// The transport seam between the controller and CoreBluetooth.
///
/// The app target's `CoreBluetoothCentral` is the only real conformer, and the
/// only code that constructs a `CBCentralManager`. Keeping the protocol here,
/// in AstarCore, lets `swift test` drive the controller through a fake without
/// ever touching Bluetooth.
///
/// There is deliberately no write method, so nothing above this seam can send
/// the device a command. The only bytes astar sends the device are the CCCD
/// enable inside `subscribe`, and RSSI reads are a controller-level query, not
/// GATT traffic.
@MainActor
public protocol BLECentralDriving: AnyObject {
    var delegate: BLECentralDelegate? { get set }
    /// The first call constructs the `CBCentralManager` (main queue, power
    /// alert off), which is when macOS asks for Bluetooth permission.
    func powerUp()
    /// Known peripherals by identifier, plus any already connected that host
    /// `service`. A dual-mode device can stop advertising while it is connected.
    func retrievePeripherals(identifiers: [UUID], connectedWithService service: String)
        -> [BLEPeripheralRef]
    /// Unfiltered, because the button's vendor service is not in its advert.
    /// The controller post-filters by name.
    func startScan()
    func stopScan()
    /// Connect, then discover that service's characteristics.
    func connect(_ id: UUID, epoch: UInt32, service: String)
    /// The CCCD enable: the only write astar ever makes.
    func subscribe(_ id: UUID, epoch: UInt32, service: String, characteristic: String)
    func readRSSI(_ id: UUID, epoch: UInt32)
    func cancelConnection(_ id: UUID)
    func shutdown()
}
