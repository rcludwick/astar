// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore
@testable import AstarStation

/// Exercises `connectFailureMessage(for:node:)` — the mapper that turns a
/// connect-time `Error` into the popover's user-facing failure text. The point
/// of the mapper (astar-0217): `StationError` does not conform to
/// `LocalizedError`, so `localizedDescription` is the useless Foundation
/// default, and the common "node is offline" failure (IAX_ERR_RESOLVE) needs
/// plain-language wording instead.
final class ConnectFailureMessageTests: XCTestCase {
    /// IAX_ERR_RESOLVE (-6): directory lookup found no records — the node has
    /// no live AllStarLink registration. The wording must name the node.
    func testResolveCodeMapsToOfflineWording() {
        let error = StationError(code: -6, text: "address resolution failed")
        let message = connectFailureMessage(for: error, node: "61057")
        XCTAssertEqual(message, "Node 61057 is not registered in Allstar.")
    }

    /// IAX_ERR_RESOLVE for an address-shaped dial (astar-427f): the user typed
    /// a host/host:port, so "not registered in Allstar" would be wrong — the
    /// lookup that failed was for their address, not a node registration.
    func testResolveCodeForAddressDialSaysCouldntReach() {
        let error = StationError(code: -6, text: "address resolution failed")
        XCTAssertEqual(
            connectFailureMessage(for: error, node: "127.0.0.1:4569"),
            "Couldn’t reach 127.0.0.1:4569.")
        XCTAssertEqual(
            connectFailureMessage(for: error, node: "my-node.example.com"),
            "Couldn’t reach my-node.example.com.")
    }

    /// IAX_ERR_PORTAL (-5): the portal token mint failed — point the user at
    /// their AllStarLink account, not at the node.
    func testPortalCodeMapsToAccountWording() {
        let error = StationError(code: -5, text: "portal request failed")
        let message = connectFailureMessage(for: error, node: "61057")
        XCTAssertTrue(
            message.contains("AllStarLink account"),
            "portal failures should point at the account: \(message)")
        XCTAssertTrue(
            message.contains("Settings"),
            "portal failures should point at Settings: \(message)")
    }

    /// Any other `StationError` falls back to its `description`, which carries
    /// the code and the library's generic text — never `localizedDescription`.
    func testOtherStationErrorFallsBackToDescription() {
        let error = StationError(code: -4, text: "a call is already in progress")
        XCTAssertEqual(
            connectFailureMessage(for: error, node: "61057"),
            "astarstation error -4: a call is already in progress")
    }

    /// Non-StationError errors keep the existing `localizedDescription`
    /// behavior — `ConnectError.needsAccount` already has good wording.
    func testNonStationErrorKeepsLocalizedDescription() {
        let error = CallSession.ConnectError.needsAccount
        XCTAssertEqual(
            connectFailureMessage(for: error, node: "61057"),
            error.localizedDescription)
    }

    /// Documents the "before" (astar-0217): `StationError` is not
    /// `LocalizedError`, so `localizedDescription` is the Foundation default —
    /// the useless text the popover used to show for an offline node.
    func testStationErrorLocalizedDescriptionIsTheUselessDefault() {
        let error = StationError(code: -6, text: "address resolution failed")
        let before = error.localizedDescription
        XCTAssertTrue(
            before.hasPrefix("The operation couldn’t be completed."),
            "expected the Foundation default, got: \(before)")
        XCTAssertFalse(before.contains("offline"), "before-text carries no useful hint")
    }
}
