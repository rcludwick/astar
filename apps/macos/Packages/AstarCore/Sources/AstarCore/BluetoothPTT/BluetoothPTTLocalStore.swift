// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// One unit this Mac has confirmed with a press.
///
/// Bound to the CoreBluetooth platform identifier, never to the name. LE
/// advertising is unauthenticated, and anyone can advertise "B02PTT-6522".
/// The identifier is per-Mac, which is why this record never leaves the Mac.
public struct BLEConfirmation: Codable, Equatable, Sendable {
    /// `CBPeripheral.identifier` on this Mac.
    public var platformID: UUID
    /// The name it advertised when confirmed. Shown in Settings only; never
    /// used for matching.
    public var advertisedName: String
    /// When the confirm press happened, for the Advanced rows.
    public var confirmedAt: Date

    public init(platformID: UUID, advertisedName: String, confirmedAt: Date) {
        self.platformID = platformID
        self.advertisedName = advertisedName
        self.confirmedAt = confirmedAt
    }
}

/// The per-Mac record of confirmed Bluetooth PTT units, each bound to its
/// peripheral identifier, never to its advertised name.
///
/// A protocol so the controller's tests can use an in-memory double. A class
/// bound, because the controller holds one store and every write must be
/// seen by later reads.
public protocol BluetoothPTTLocalStore: AnyObject {
    /// Every unit of `deviceID` confirmed on this Mac, in confirm order.
    func confirmations(deviceID: String) -> [BLEConfirmation]
    /// Whether this exact peripheral identifier was confirmed for `deviceID`.
    func isConfirmed(platformID: UUID, deviceID: String) -> Bool
    /// Record a confirm press. Upserts by `platformID`.
    func confirm(_ confirmation: BLEConfirmation, deviceID: String)
    /// "Forget this button": drop one unit's confirmation.
    func forget(platformID: UUID, deviceID: String)
}

/// `UserDefaults`-backed confirmations under `blePTT.confirmed`, as JSON
/// `[deviceID: [BLEConfirmation]]`.
///
/// The `blePTT.` prefix is deliberately outside every `ConfigArchive` section
/// prefix, so no export ever carries it. An imported config arrives
/// unconfirmed and touches no Bluetooth until someone confirms a unit here.
public final class UserDefaultsBluetoothPTTLocalStore: BluetoothPTTLocalStore {
    /// The one defaults key this store owns.
    public static let key = "blePTT.confirmed"

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func confirmations(deviceID: String) -> [BLEConfirmation] {
        load()[deviceID] ?? []
    }

    public func isConfirmed(platformID: UUID, deviceID: String) -> Bool {
        confirmations(deviceID: deviceID).contains { $0.platformID == platformID }
    }

    public func confirm(_ confirmation: BLEConfirmation, deviceID: String) {
        var all = load()
        var list = all[deviceID] ?? []
        if let i = list.firstIndex(where: { $0.platformID == confirmation.platformID }) {
            list[i] = confirmation
        } else {
            list.append(confirmation)
        }
        all[deviceID] = list
        persist(all)
    }

    public func forget(platformID: UUID, deviceID: String) {
        var all = load()
        guard var list = all[deviceID] else { return }
        list.removeAll { $0.platformID == platformID }
        all[deviceID] = list.isEmpty ? nil : list
        persist(all)
    }

    /// Unreadable data reads as nothing confirmed: it fails closed.
    private func load() -> [String: [BLEConfirmation]] {
        guard let data = defaults.data(forKey: Self.key),
            let decoded = try? JSONDecoder().decode([String: [BLEConfirmation]].self, from: data)
        else { return [:] }
        return decoded
    }

    private func persist(_ all: [String: [BLEConfirmation]]) {
        guard !all.isEmpty else {
            defaults.removeObject(forKey: Self.key)
            return
        }
        if let data = try? JSONEncoder().encode(all) {
            defaults.set(data, forKey: Self.key)
        }
    }
}
