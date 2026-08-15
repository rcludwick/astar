// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The compose-then-send dialpad rules (astar-7d21): what the connected-mode
/// command field may contain, when Send is allowed, and how a locked command
/// splits for the progress render.
final class DialpadComposerTests: XCTestCase {

    // MARK: - filtered(_:)

    func testFilteredUppercasesAndKeepsAllSixteenKeys() {
        XCTAssertEqual(
            DialpadComposer.filtered("0123456789*#abcd"),
            "0123456789*#ABCD",
            "lowercase a-d uppercase; every DTMF key survives")
    }

    func testFilteredStripsEverythingElse() {
        // A pasted command with spaces, dashes and junk: only DTMF keys remain.
        XCTAssertEqual(DialpadComposer.filtered("*3 5460-54x!"), "*3546054")
        // E is NOT a DTMF key (only A-D are) — everything here strips away.
        XCTAssertEqual(DialpadComposer.filtered("Ee \n+é/"), "")
        XCTAssertEqual(DialpadComposer.filtered(""), "")
    }

    // MARK: - canSend

    func testCanSendTruthTable() {
        XCTAssertTrue(DialpadComposer.canSend(command: "*3", answered: true, playing: false))
        XCTAssertFalse(
            DialpadComposer.canSend(command: "", answered: true, playing: false),
            "empty command has nothing to send")
        XCTAssertFalse(
            DialpadComposer.canSend(command: "*3", answered: false, playing: false),
            "no on-air tones before answer")
        XCTAssertFalse(
            DialpadComposer.canSend(command: "*3", answered: true, playing: true),
            "one sequence at a time")
    }

    // MARK: - progressSplit

    func testProgressSplitBounds() {
        let cmd = "*3546054"
        XCTAssertEqual(DialpadComposer.progressSplit(command: cmd, played: 0).played, "")
        XCTAssertEqual(DialpadComposer.progressSplit(command: cmd, played: 0).pending, cmd)

        let mid = DialpadComposer.progressSplit(command: cmd, played: 3)
        XCTAssertEqual(mid.played, "*35")
        XCTAssertEqual(mid.pending, "46054")

        let done = DialpadComposer.progressSplit(command: cmd, played: 8)
        XCTAssertEqual(done.played, cmd)
        XCTAssertEqual(done.pending, "")

        // Overshoot clamps rather than trapping (snapshot races are benign).
        let over = DialpadComposer.progressSplit(command: cmd, played: 99)
        XCTAssertEqual(over.played, cmd)
        XCTAssertEqual(over.pending, "")
    }
}
