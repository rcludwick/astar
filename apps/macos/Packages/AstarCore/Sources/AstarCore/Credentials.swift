// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// AllStar portal credentials for the authenticated WebTransceiver path.
///
/// `portalPass` is a secret: it is stored only in the Keychain, loaded into a
/// `StationConfig` solely at station construction (consumed by the binding,
/// never retained), and deliberately redacted from debug output. Honors the
/// secret-free contract (PTT/secret prime rules).
///
/// There is no node number. The token comes from AllStarLink's documented API
/// (`POST /api/v2/auth-wt-legacy`, username + password), which needs none. A
/// Keychain blob written by an earlier build carries a `portalNode` key; the
/// synthesized decoder ignores keys it has no property for, so those blobs
/// keep loading — which is why this needed no config-version bump: nothing is
/// misread, a field is simply no longer looked at.
public struct Credentials: Equatable, Codable {
    public var portalUser: String
    public var portalPass: String

    public init(portalUser: String, portalPass: String) {
        self.portalUser = portalUser
        self.portalPass = portalPass
    }
}

extension Credentials: CustomDebugStringConvertible {
    public var debugDescription: String {
        "Credentials(user: \(portalUser), pass: <redacted>)"
    }
}
