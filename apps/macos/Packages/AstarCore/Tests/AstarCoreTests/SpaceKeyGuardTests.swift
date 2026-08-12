// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if canImport(AppKit)
    import AppKit
    import XCTest

    @testable import AstarCore

    // astar-e814: regression coverage for the spacebar PTT monitor's
    // typing-guard. A focused NSTextView (the field editor behind any
    // SwiftUI TextField — DTMF compose, M17 callsign, favorite-label editor,
    // credentials form) must suppress PTT-keying so Space types a literal
    // space instead of transmitting.
    final class SpaceKeyGuardTests: XCTestCase {
        func testTypingIntoATextViewSuppressesPTT() {
            XCTAssertTrue(SpaceKeyGuard.spaceIsTyping(firstResponder: NSTextView()))
        }

        func testNoFirstResponderDoesNotSuppressPTT() {
            XCTAssertFalse(SpaceKeyGuard.spaceIsTyping(firstResponder: nil))
        }

        func testNonTextResponderDoesNotSuppressPTT() {
            XCTAssertFalse(SpaceKeyGuard.spaceIsTyping(firstResponder: NSView()))
        }
    }
#endif
