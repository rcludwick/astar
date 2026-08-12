// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import AstarStation
import XCTest

@testable import AstarCore

/// Pure planner tests (astar-b167): every event class the audit's F6 finding
/// calls for, plus the "primed" (no announcement on initial state adoption)
/// guardrail and the PTT bounce-coalescing rule. No Combine/AppKit here —
/// the planner only ever sees plain values + injected `Date`s.
final class AccessibilityAnnouncementPlannerTests: XCTestCase {
    // MARK: - Status transitions

    func testFirstStatusObservationIsPrimedNotAnnounced() {
        let planner = AccessibilityAnnouncementPlanner()
        // The very first call ever, even though it "looks like" a connect,
        // must be suppressed — it's the initial state adoption, not a
        // real transition (Combine replays the current value to a fresh
        // subscriber).
        XCTAssertEqual(
            planner.statusChanged(to: .answered, connectedTarget: "node 77777"), [])
    }

    func testConnectAnnouncesWithTheDialedTargetLabel() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.statusChanged(to: .idle, connectedTarget: nil)  // prime
        XCTAssertEqual(
            planner.statusChanged(to: .answered, connectedTarget: "node 77777"),
            [Announcement(text: "Connected to node 77777", priority: .medium)])
    }

    func testCallEndedAnnouncedLeavingAnswered() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.statusChanged(to: .answered, connectedTarget: "node 77777")  // prime
        XCTAssertEqual(
            planner.statusChanged(to: .hangup, connectedTarget: nil),
            [Announcement(text: "Call ended", priority: .medium)])
    }

    func testCallEndedAnnouncedEvenWhenHangupIsSkippedStraightToIdle() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.statusChanged(to: .answered, connectedTarget: "node 77777")  // prime
        XCTAssertEqual(
            planner.statusChanged(to: .idle, connectedTarget: nil),
            [Announcement(text: "Call ended", priority: .medium)])
    }

    func testCallEndedNotDoubleAnnouncedAcrossHangupThenIdle() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.statusChanged(to: .answered, connectedTarget: "node 77777")  // prime
        _ = planner.statusChanged(to: .hangup, connectedTarget: nil)  // "Call ended" here
        XCTAssertEqual(planner.statusChanged(to: .idle, connectedTarget: nil), [])
    }

    func testDialingNeverAnsweredThenHangupIsSilentOnTheStatusChannel() {
        // A dial that never answers reports via `lastDialFailure`, not this
        // channel — the status planner must stay quiet so it isn't announced
        // twice (once here, once by the dial-failure channel).
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.statusChanged(to: .idle, connectedTarget: nil)  // prime
        XCTAssertEqual(planner.statusChanged(to: .dialing, connectedTarget: nil), [])
        XCTAssertEqual(planner.statusChanged(to: .hangup, connectedTarget: nil), [])
    }

    func testRepeatedIdenticalStatusIsNotReannounced() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.statusChanged(to: .answered, connectedTarget: "node 77777")  // prime
        XCTAssertEqual(planner.statusChanged(to: .answered, connectedTarget: "node 77777"), [])
    }

    // MARK: - Dial failure

    func testFirstDialFailureObservationIsPrimedNotAnnounced() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.dialFailureChanged(to: "Node 77777 didn’t answer."), [])
    }

    func testDialFailureAnnouncesVerbatimOnNonNilTransition() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.dialFailureChanged(to: nil)  // prime
        XCTAssertEqual(
            planner.dialFailureChanged(to: "Node 77777 didn’t answer."),
            [Announcement(text: "Node 77777 didn’t answer.", priority: .medium)])
    }

    func testDialFailureClearingToNilAnnouncesNothing() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.dialFailureChanged(to: "Node 77777 didn’t answer.")  // prime
        XCTAssertEqual(planner.dialFailureChanged(to: nil), [])
    }

    func testRepeatedIdenticalDialFailureIsNotReannounced() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.dialFailureChanged(to: nil)  // prime
        XCTAssertEqual(
            planner.dialFailureChanged(to: "Node 77777 didn’t answer."),
            [Announcement(text: "Node 77777 didn’t answer.", priority: .medium)])
        // Same string again, no intervening nil — must be a no-op, exactly
        // like `testRepeatedIdenticalStatusIsNotReannounced`.
        XCTAssertEqual(planner.dialFailureChanged(to: "Node 77777 didn’t answer."), [])
    }

    // MARK: - PTT edges (+ bounce coalescing)

    func testFirstPTTObservationIsPrimedNotAnnounced() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.pttChanged(to: true, now: Date(timeIntervalSince1970: 0)), [])
    }

    func testKeyEdgeAnnouncesTransmitting() {
        let planner = AccessibilityAnnouncementPlanner()
        let t0 = Date(timeIntervalSince1970: 0)
        _ = planner.pttChanged(to: false, now: t0)  // prime
        XCTAssertEqual(
            planner.pttChanged(to: true, now: t0.addingTimeInterval(10)),
            [Announcement(text: "Transmitting", priority: .high)])
    }

    func testUnkeyEdgeAnnouncesTransmissionEnded() {
        let planner = AccessibilityAnnouncementPlanner()
        let t0 = Date(timeIntervalSince1970: 0)
        _ = planner.pttChanged(to: false, now: t0)  // prime
        _ = planner.pttChanged(to: true, now: t0.addingTimeInterval(10))
        XCTAssertEqual(
            planner.pttChanged(to: false, now: t0.addingTimeInterval(12)),
            [Announcement(text: "Transmission ended", priority: .high)])
    }

    func testRapidBouncePairWithin300msIsCoalescedAway() {
        let planner = AccessibilityAnnouncementPlanner()
        let t0 = Date(timeIntervalSince1970: 0)
        _ = planner.pttChanged(to: false, now: t0)  // prime
        XCTAssertEqual(
            planner.pttChanged(to: true, now: t0.addingTimeInterval(10)),
            [Announcement(text: "Transmitting", priority: .high)])
        // Bounce back to false 100 ms later — inside the 300 ms window.
        XCTAssertEqual(planner.pttChanged(to: false, now: t0.addingTimeInterval(10.1)), [])
        // And back to true again another 100 ms later — still bouncing.
        XCTAssertEqual(planner.pttChanged(to: true, now: t0.addingTimeInterval(10.2)), [])
    }

    func testEdgeAfterTheBounceWindowAnnouncesNormallyAgain() {
        let planner = AccessibilityAnnouncementPlanner()
        let t0 = Date(timeIntervalSince1970: 0)
        _ = planner.pttChanged(to: false, now: t0)  // prime
        _ = planner.pttChanged(to: true, now: t0.addingTimeInterval(10))
        // Well clear of the 300 ms bounce window.
        XCTAssertEqual(
            planner.pttChanged(to: false, now: t0.addingTimeInterval(11)),
            [Announcement(text: "Transmission ended", priority: .high)])
    }

    // MARK: - DTMF sequence complete (falling edge to 0)

    func testFirstDTMFObservationIsPrimedNotAnnounced() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.dtmfTotalChanged(to: 0), [])
    }

    func testDTMFFallingEdgeToZeroAnnouncesCommandSent() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.dtmfTotalChanged(to: 0)  // prime
        _ = planner.dtmfTotalChanged(to: 4)  // sequence starts
        XCTAssertEqual(
            planner.dtmfTotalChanged(to: 0),
            [Announcement(text: "Command sent", priority: .medium)])
    }

    func testDTMFRisingOrMidSequenceChangesAreSilent() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.dtmfTotalChanged(to: 0)  // prime
        XCTAssertEqual(planner.dtmfTotalChanged(to: 4), [])
        XCTAssertEqual(planner.dtmfTotalChanged(to: 6), [])
    }

    func testDTMFStartingAtZeroThenStayingZeroIsSilent() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.dtmfTotalChanged(to: 0)  // prime
        XCTAssertEqual(planner.dtmfTotalChanged(to: 0), [])
    }

    // MARK: - TX enable flips

    func testFirstTxDisabledObservationIsPrimedNotAnnounced() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.txDisabledChanged(to: true), [])
    }

    func testTxDisabledFlipAnnouncesListeningOnly() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.txDisabledChanged(to: false)  // prime
        XCTAssertEqual(
            planner.txDisabledChanged(to: true),
            [Announcement(text: "TX disabled, listening only", priority: .high)])
    }

    func testTxEnabledFlipAnnouncesTxEnabled() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.txDisabledChanged(to: true)  // prime
        XCTAssertEqual(
            planner.txDisabledChanged(to: false),
            [Announcement(text: "TX enabled", priority: .high)])
    }

    // MARK: - Talk-timer phase crossings

    func testFirstTalkTimerObservationIsPrimedNotAnnounced() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.talkTimerPhaseChanged(to: .red), [])
    }

    func testEnteringGreenIsNeverAnnounced() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.talkTimerPhaseChanged(to: nil)  // prime (unkeyed)
        XCTAssertEqual(planner.talkTimerPhaseChanged(to: .green), [])
    }

    func testCrossingIntoAmberAnnouncesTheSharedWording() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.talkTimerPhaseChanged(to: nil)  // prime
        _ = planner.talkTimerPhaseChanged(to: .green)
        XCTAssertEqual(
            planner.talkTimerPhaseChanged(to: .amber),
            [Announcement(text: TalkTimer.help(for: .amber), priority: .high)])
    }

    func testCrossingIntoRedAnnouncesTheSharedWording() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.talkTimerPhaseChanged(to: nil)  // prime
        _ = planner.talkTimerPhaseChanged(to: .green)
        _ = planner.talkTimerPhaseChanged(to: .amber)
        XCTAssertEqual(
            planner.talkTimerPhaseChanged(to: .red),
            [Announcement(text: TalkTimer.help(for: .red), priority: .high)])
    }

    func testUnkeyResetToNilIsSilent() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.talkTimerPhaseChanged(to: nil)  // prime
        _ = planner.talkTimerPhaseChanged(to: .green)
        _ = planner.talkTimerPhaseChanged(to: .amber)
        _ = planner.talkTimerPhaseChanged(to: .red)
        XCTAssertEqual(planner.talkTimerPhaseChanged(to: nil), [])
    }

    func testSamePhaseRepeatedIsNotReannounced() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.talkTimerPhaseChanged(to: nil)  // prime
        _ = planner.talkTimerPhaseChanged(to: .green)
        _ = planner.talkTimerPhaseChanged(to: .amber)
        XCTAssertEqual(planner.talkTimerPhaseChanged(to: .amber), [])
    }

    func testFreshKeyDownAfterUnkeyRampsThroughPhasesAgain() {
        // A second, later transmission must re-announce amber/red — the
        // "already announced" state must reset on the nil (unkey) edge.
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.talkTimerPhaseChanged(to: nil)  // prime
        _ = planner.talkTimerPhaseChanged(to: .green)
        _ = planner.talkTimerPhaseChanged(to: .amber)
        _ = planner.talkTimerPhaseChanged(to: .red)
        _ = planner.talkTimerPhaseChanged(to: nil)  // unkey
        _ = planner.talkTimerPhaseChanged(to: .green)  // re-key
        XCTAssertEqual(
            planner.talkTimerPhaseChanged(to: .amber),
            [Announcement(text: TalkTimer.help(for: .amber), priority: .high)])
    }
}
