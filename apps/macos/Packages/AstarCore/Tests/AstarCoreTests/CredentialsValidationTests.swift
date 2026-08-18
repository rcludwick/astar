// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Red-highlight rules for the AllStarLink account fields (astar-4e8a).
final class CredentialsValidationTests: XCTestCase {

    // MARK: - The trap: a saved account shows an empty password field on purpose

    /// astar never pre-fills the password — it lives in the Keychain and the
    /// field reads "re-enter to change". Flagging that blank field red would tell
    /// someone with perfectly good saved credentials that they are broken.
    func testEmptyPasswordIsNotAnErrorWhenCredentialsAreSaved() {
        XCTAssertEqual(
            CredentialsValidation.password(
                text: "", hasSavedCredentials: true, portalRejected: false),
            .neutral)
    }

    /// Same field, same emptiness — but with nothing saved it really is missing.
    func testEmptyPasswordIsFlaggedWhenNothingIsSaved() {
        let status = CredentialsValidation.password(
            text: "", hasSavedCredentials: false, portalRejected: false)
        XCTAssertTrue(status.isInvalid)
        XCTAssertEqual(status.message, CredentialsValidation.missingPassword)
    }

    /// Whitespace is not a password.
    func testWhitespaceOnlyPasswordIsFlagged() {
        XCTAssertTrue(
            CredentialsValidation.password(
                text: "   ", hasSavedCredentials: false, portalRejected: false
            ).isInvalid)
    }

    func testTypedPasswordIsAccepted() {
        XCTAssertEqual(
            CredentialsValidation.password(
                text: "hunter2", hasSavedCredentials: false, portalRejected: false),
            .neutral)
    }

    // MARK: - Rejected by the portal

    /// The only real validity signal astar has is the token mint. A rejection
    /// outranks everything — including a saved account, which is exactly the case
    /// where the user cannot see the password to check it.
    func testPortalRejectionFlagsTheField() {
        let status = CredentialsValidation.password(
            text: "", hasSavedCredentials: true, portalRejected: true)
        XCTAssertTrue(status.isInvalid)
        XCTAssertEqual(status.message, CredentialsValidation.portalRejected)
    }

    func testPortalRejectionOutranksATypedPassword() {
        XCTAssertTrue(
            CredentialsValidation.password(
                text: "hunter2", hasSavedCredentials: true, portalRejected: true
            ).isInvalid)
    }

    /// A missing password and a stale rejection at once: say it is missing. The
    /// user cannot act on "the portal rejected it" while the box is empty.
    func testMissingBeatsRejectedWhenNothingIsSaved() {
        XCTAssertEqual(
            CredentialsValidation.password(
                text: "", hasSavedCredentials: false, portalRejected: true
            ).message,
            CredentialsValidation.missingPassword)
    }

    // MARK: - Messages

    /// Every invalid state must carry text: a red box with no reason is a puzzle.
    func testEveryInvalidStateExplainsItself() {
        for status in [
            CredentialsValidation.password(
                text: "", hasSavedCredentials: false, portalRejected: false),
            CredentialsValidation.password(
                text: "x", hasSavedCredentials: true, portalRejected: true),
        ] {
            XCTAssertFalse(status.message?.isEmpty ?? true)
        }
    }

    /// The password is the portal account password, not the node's IAX secret —
    /// the single most common mix-up. The missing-password copy has to say so.
    func testMissingMessageNamesTheRightPassword() {
        XCTAssertTrue(CredentialsValidation.missingPassword.contains("allstarlink.org"))
    }
}
