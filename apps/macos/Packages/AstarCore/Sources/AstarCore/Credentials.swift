// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// AllStar portal credentials for the authenticated WebTransceiver path.
///
/// `portalPass` is a secret: it is stored only in the Keychain, loaded into a
/// `StationConfig` solely at station construction (consumed by the binding,
/// never retained), and deliberately redacted from debug output. Honors the
/// secret-free contract (PTT/secret prime rules).
public struct Credentials: Equatable, Codable {
    public var portalUser: String
    public var portalPass: String
    public var portalNode: String

    public init(portalUser: String, portalPass: String, portalNode: String) {
        self.portalUser = portalUser
        self.portalPass = portalPass
        self.portalNode = portalNode
    }
}

extension Credentials: CustomDebugStringConvertible {
    public var debugDescription: String {
        "Credentials(user: \(portalUser), node: \(portalNode), pass: <redacted>)"
    }
}
