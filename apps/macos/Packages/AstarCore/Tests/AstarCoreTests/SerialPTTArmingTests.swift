// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class SerialPTTArmingTests: XCTestCase {
    /// Opened at t = 100 s with the UCI150's 30 ms debounce, so the settle
    /// window ends at 102.03 s.
    private func gate(debounceMs: UInt32 = 30) -> SerialPTTArming {
        SerialPTTArming(openedAt: 100, debounceMs: debounceMs)
    }

    func testSettleWindowIsTheDebouncePlusTheMargin() {
        XCTAssertEqual(SerialPTTArming.settleMargin, 2)
        XCTAssertEqual(gate().settlesAt, 102.03, accuracy: 1e-9)
        XCTAssertEqual(gate(debounceMs: 500).settlesAt, 102.5, accuracy: 1e-9)
    }

    func testALineHeldAtOpenNeverKeysUntilItIsReleased() {
        var g = gate()
        // The worker reports the held line once its debounce has run.
        XCTAssertNil(g.filter(edge: nil, now: 100.0))
        XCTAssertNil(g.filter(edge: true, now: 100.06), "a line held at open must not key")
        XCTAssertFalse(g.isArmed)
        // Still held long after the window: still nothing, and still not armed.
        XCTAssertNil(g.filter(edge: nil, now: 103))
        XCTAssertNil(g.filter(edge: nil, now: 500))
        XCTAssertFalse(g.isArmed, "a held line must not arm by waiting out the window")
        // The release goes through (it is harmless) and arms the gate.
        XCTAssertEqual(g.filter(edge: false, now: 501), false)
        XCTAssertTrue(g.isArmed)
        // A fresh press now keys, and its release unkeys.
        XCTAssertEqual(g.filter(edge: true, now: 502), true)
        XCTAssertEqual(g.filter(edge: false, now: 503), false)
    }

    func testAKeyDownLateInTheWindowIsStillTreatedAsHeld() {
        var g = gate()
        XCTAssertNil(g.filter(edge: nil, now: 100.5))
        XCTAssertNil(g.filter(edge: nil, now: 102.0))
        XCTAssertNil(
            g.filter(edge: true, now: 102.02), "inside the window, a key-down is held-at-open")
        XCTAssertFalse(g.isArmed)
    }

    func testAKeyDownOnTheFirstTickPastTheWindowIsHeldWhenNoQuietTickCameFirst() {
        // A stalled poll: the first tick after open lands well past the
        // window, carrying the queued held-at-open edge. Only a quiet tick
        // past the window arms, so this one must not key.
        var g = gate()
        XCTAssertNil(g.filter(edge: true, now: 110))
        XCTAssertFalse(g.isArmed)
    }

    func testAnIdleLineArmsOnceTheWindowPassesQuietly() {
        var g = gate()
        XCTAssertNil(g.filter(edge: nil, now: 100.0))
        XCTAssertNil(g.filter(edge: nil, now: 102.029))
        XCTAssertFalse(g.isArmed, "not armed before the window has passed")
        XCTAssertNil(g.filter(edge: nil, now: 102.031))
        XCTAssertTrue(g.isArmed, "a quiet tick past the end of the window arms")
        XCTAssertEqual(g.filter(edge: true, now: 102.1), true, "the first press keys normally")
        XCTAssertEqual(g.filter(edge: false, now: 103), false)
        XCTAssertEqual(g.filter(edge: true, now: 104), true)
    }

    func testTheWindowFollowsTheConfiguredDebounce() {
        // With a 500 ms debounce the worker may report a held line as late as
        // ~100.5 s, so a quiet tick at 102.1 s must not arm yet.
        var g = gate(debounceMs: 500)
        XCTAssertNil(g.filter(edge: nil, now: 102.1))
        XCTAssertFalse(g.isArmed)
        XCTAssertNil(g.filter(edge: true, now: 102.2), "still inside the 500 ms debounce window")
        XCTAssertFalse(g.isArmed)
    }

    func testAReleaseInsideTheWindowArmsAtOnce() {
        var g = gate()
        XCTAssertEqual(g.filter(edge: false, now: 100.1), false)
        XCTAssertTrue(g.isArmed)
        XCTAssertEqual(
            g.filter(edge: true, now: 100.2), true, "seen released, so the next press keys")
    }

    func testRepeatedKeyDownsWhileHeldAreAllDropped() {
        var g = gate()
        XCTAssertNil(g.filter(edge: true, now: 100.1))
        XCTAssertNil(g.filter(edge: true, now: 100.2))
        XCTAssertNil(g.filter(edge: true, now: 105))
        XCTAssertFalse(g.isArmed)
    }

    func testAnArmedGatePassesEveryEdgeThrough() {
        var g = gate()
        XCTAssertNil(g.filter(edge: nil, now: 103))
        XCTAssertTrue(g.isArmed)
        XCTAssertNil(g.filter(edge: nil, now: 104))
        XCTAssertEqual(g.filter(edge: true, now: 105), true)
        XCTAssertEqual(g.filter(edge: true, now: 105.1), true)
        XCTAssertEqual(g.filter(edge: false, now: 106), false)
        XCTAssertNil(g.filter(edge: nil, now: 107))
    }
}
