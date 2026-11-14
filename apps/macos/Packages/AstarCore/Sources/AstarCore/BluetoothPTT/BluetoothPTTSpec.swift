// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Which Bluetooth LE PTT button a config keys from.
///
/// It lives on `Setup` as an optional field, so a config without one decodes
/// and behaves exactly as it did before. It names a model (a row of the
/// engine's device table, by id) and optionally a unit by advertised name. It
/// never names a CoreBluetooth peripheral identifier: those differ on every
/// Mac. An exported config therefore carries the model and the name, and each
/// Mac confirms its own unit with one press (`BluetoothPTTLocalStore`). A
/// config imported from elsewhere can never key a button nobody here has
/// confirmed.
public struct BluetoothPTTSpec: Codable, Equatable, Hashable, Sendable {
    /// `BLEButtonDevice.id` of the model, e.g. `"inrico-b02ptt"`.
    public var deviceID: String
    /// The advertised name of the unit this config prefers, e.g.
    /// `"B02PTT-6522"`. `nil` means any confirmed unit of that model, which
    /// lets one config follow a replacement button without editing.
    public var preferredName: String?

    public init(deviceID: String, preferredName: String? = nil) {
        self.deviceID = deviceID
        self.preferredName = preferredName
    }
}
