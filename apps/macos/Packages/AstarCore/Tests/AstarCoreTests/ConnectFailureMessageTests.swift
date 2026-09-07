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

    /// Any other `StationError` falls back to its `message` — the engine's
    /// detail when there is one, the generic family text otherwise. Never
    /// `description`, which prefixes an error code: a number is not a
    /// diagnosis and an operator should not be shown one.
    func testOtherStationErrorFallsBackToTheMessageWithoutTheCode() {
        let error = StationError(code: -4, text: "a call is already in progress")
        let message = connectFailureMessage(for: error, node: "61057")
        XCTAssertEqual(message, "a call is already in progress")
        XCTAssertFalse(message.contains("-4"), "an error code is not a diagnosis")
    }

    /// IAX_ERR_AUDIO (-7): one plain sentence, whatever the engine's detail
    /// said. Every cause the engine distinguishes — missing, busy, unplugged,
    /// a config the device refused — has the same two remedies, so naming
    /// which one it was gives the operator nothing more to act on.
    func testAudioFailureIsOnePlainSentence() {
        let expected = "Couldn’t open audio device, is it busy or unplugged?"
        // With a detail...
        XCTAssertEqual(
            connectFailureMessage(
                for: StationError(
                    code: -7, text: "audio error",
                    detail: "audio error: audio device: no device matched \"in:gone\" for Input"),
                node: "61057"),
            expected)
        // ...and without one.
        XCTAssertEqual(
            connectFailureMessage(
                for: StationError(code: -7, text: "audio error"), node: "61057"),
            expected)
    }

    /// The detail is still carried on the error for anyone debugging — it is
    /// the message that drops it, not the channel.
    func testTheEngineDetailSurvivesOnTheErrorEvenWhenUnused() {
        let error = StationError(
            code: -7, text: "audio error",
            detail: "audio error: device not found: KT USB Audio")
        XCTAssertTrue(error.detail.contains("KT USB Audio"))
        XCTAssertEqual(error.message, "audio error: device not found: KT USB Audio")
    }

    /// A D-Star failure now prefers the engine's real reason over the
    /// three-causes guess, because the last-error accessor exists.
    func testADStarFailureWithDetailNamesTheRealReason() {
        let error = StationError(
            code: -19, text: "dstar error",
            detail: "dstar error: ThumbDV at /dev/cu.usbserial-A1 is busy")
        let message = connectFailureMessage(for: error, node: "XLX836 A")
        XCTAssertEqual(
            message, "Couldn’t connect to XLX836 A: ThumbDV at /dev/cu.usbserial-A1 is busy.")
    }

    /// The NXDN refusals are `ConnectError`s, so they reach the popover
    /// through `localizedDescription`. Each has to name the remedy: an id
    /// that is missing, an id the wire cannot carry, and — the one worth
    /// spelling out — that it is not the DMR number.
    func testTheNXDNIdentityRefusalsNameTheRemedy() {
        let missing = CallSession.ConnectError.missingRadioID.localizedDescription
        XCTAssertTrue(missing.contains("NXDN ID"), missing)
        XCTAssertTrue(missing.contains("Settings"), missing)

        let range = CallSession.ConnectError.radioIDOutOfRange.localizedDescription
        XCTAssertTrue(range.contains("65519"), range)
        XCTAssertTrue(range.contains("DMR"), "the two numbers must not be confused: \(range)")

        let target = CallSession.ConnectError.badNXDNTarget.localizedDescription
        XCTAssertTrue(target.contains("talkgroup"), target)
    }

    /// A DMR failure gets the same treatment as D-Star's: `iax_error_text(-22)`
    /// is the static "dmr error", so the engine's own detail is what tells an
    /// operator whether the master refused the password or the ID.
    func testADMRFailureWithDetailNamesTheRealReason() {
        let error = StationError(
            code: -22, text: "dmr error",
            detail: "dmr error: master refused the login (auth)")
        XCTAssertEqual(
            connectFailureMessage(for: error, node: "tgif:tgif.network/31313/2"),
            "Couldn’t connect to tgif:tgif.network/31313/2: master refused the login (auth).")

        let bare = StationError(code: -22, text: "dmr error", detail: "")
        let message = connectFailureMessage(for: bare, node: "tgif")
        XCTAssertTrue(message.contains("DMR password"), message)
        XCTAssertTrue(message.contains("ThumbDV"), message)
    }

    /// DMR's refusals are its own cases, not NXDN's: the two radio IDs are
    /// different numbers with different registrations, and a message naming
    /// the wrong one sends the operator to the wrong page.
    func testTheDMRRefusalsNameTheirOwnRemedies() {
        let missing = CallSession.ConnectError.missingDMRRadioID.localizedDescription
        XCTAssertTrue(missing.contains("DMR radio ID"), missing)
        XCTAssertFalse(missing.contains("NXDN"), missing)

        let range = CallSession.ConnectError.dmrRadioIDOutOfRange.localizedDescription
        XCTAssertTrue(range.contains("radioid.net"), range)

        let password = CallSession.ConnectError.missingDMRPassword.localizedDescription
        XCTAssertTrue(password.contains("Settings"), password)
        XCTAssertTrue(password.contains("issues its own"), password)

        let consent = CallSession.ConnectError.brandmeisterNotConsented.localizedDescription
        XCTAssertTrue(consent.contains("BrandMeister enforces its own access rules"), consent)

        let target = CallSession.ConnectError.badDMRTarget.localizedDescription
        XCTAssertTrue(target.contains("talkgroup"), target)
        XCTAssertTrue(target.contains("timeslot"), target)
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

    /// The engine classifies D-Star failures precisely, but that text does not
    /// cross the C-ABI — `iax_error_text(-19)` is the static string "dstar
    /// error". Left alone the operator would read "astarstation error -19:
    /// dstar error", which says nothing about the one thing that is almost
    /// always wrong: the dongle.
    /// With NO detail — the guess is still better than the code.
    func testADStarFailureNamesTheDongleRatherThanTheErrorCode() {
        let message = connectFailureMessage(
            for: StationError(code: -19, text: "dstar error"), node: "XLX836 A")
        XCTAssertTrue(message.contains("ThumbDV"), message)
        XCTAssertTrue(message.contains("XLX836 A"), message)
        XCTAssertFalse(message.contains("-19"), "an error code is not a diagnosis")
    }
}
