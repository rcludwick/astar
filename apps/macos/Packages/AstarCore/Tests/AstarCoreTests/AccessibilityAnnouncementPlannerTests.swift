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

    // MARK: - Engine forced stops (transmit time-out, lease expiry)

    func testTxTimeoutAnnouncesMinutesInWords() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.pttForcedStop(reason: .txTimeout, txTimeoutSeconds: 120),
            [
                Announcement(
                    text: "Transmit stopped after 2 minutes. Release to talk again.",
                    priority: .high)
            ])
    }

    func testTxTimeoutAtOneMinuteIsSingular() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.pttForcedStop(reason: .txTimeout, txTimeoutSeconds: 60),
            [
                Announcement(
                    text: "Transmit stopped after 1 minute. Release to talk again.",
                    priority: .high)
            ])
    }

    func testTxTimeoutAtTenMinutes() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.pttForcedStop(reason: .txTimeout, txTimeoutSeconds: 600),
            [
                Announcement(
                    text: "Transmit stopped after 10 minutes. Release to talk again.",
                    priority: .high)
            ])
    }

    func testLeaseExpiryAnnouncesThatAstarStoppedResponding() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.pttForcedStop(reason: .leaseExpired, txTimeoutSeconds: 120),
            [
                Announcement(
                    text:
                        "Transmit stopped: astar stopped responding. Release the button, then press again.",
                    priority: .high)
            ])
    }

    func testForcedStopAnnouncementIsTheBannerWordForWord() {
        // VoiceOver and the orange banner must never say two different
        // things about one stop: both read BluetoothPTTPresentation.
        let planner = AccessibilityAnnouncementPlanner()
        let stop = PTTForcedStop(
            seq: 7, sources: [.bluetooth, .vox], reason: .txTimeout, txTimeoutSeconds: 300,
            at: Date(timeIntervalSince1970: 1_000))
        XCTAssertEqual(
            planner.pttForcedStop(reason: stop.reason, txTimeoutSeconds: stop.txTimeoutSeconds),
            [Announcement(text: BluetoothPTTPresentation.forcedStopBanner(stop), priority: .high)])
    }

    // MARK: - Bluetooth PTT link loss and releases

    func testLinkLostWhileKeyedAnnouncesTheBannerAtHighPriority() {
        // The contract signature: the name comes from the device table,
        // whose only entry is the B02PTT.
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.blePttLinkLost(whileKeyed: true),
            [
                Announcement(
                    text:
                        "Transmit stopped: B02PTT disconnected. Release the button, then press again.",
                    priority: .high)
            ])
    }

    func testLinkLostWhileKeyedUsesTheGivenShortName() {
        // The internal overload, with a name that is not in the device
        // table, so ignoring the parameter (or hard-coding "B02PTT") turns
        // this red.
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.blePttLinkLost(whileKeyed: true, shortName: "X9PTT"),
            [
                Announcement(
                    text:
                        "Transmit stopped: X9PTT disconnected. Release the button, then press again.",
                    priority: .high)
            ])
    }

    func testLinkLostWhileIdleIsAQuietMediumNotice() {
        // Nothing was on air, so this must not interrupt like a forced stop.
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.blePttLinkLost(whileKeyed: false),
            [Announcement(text: "Bluetooth PTT button disconnected", priority: .medium)])
    }

    func testBluetoothReleaseAdapterOff() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.bluetoothRelease(reason: .adapterOff, shortName: "B02PTT"),
            [
                Announcement(
                    text:
                        "Transmit stopped: Bluetooth turned off. Release the button, then press again.",
                    priority: .high)
            ])
    }

    func testBluetoothReleaseMalformed() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.bluetoothRelease(reason: .malformed, shortName: "B02PTT"),
            [
                Announcement(
                    text:
                        "Transmit stopped: B02PTT sent a value astar doesn't recognise. Release the button, then press again.",
                    priority: .high)
            ])
    }

    func testBluetoothReleaseSleep() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.bluetoothRelease(reason: .sleep, shortName: "B02PTT"),
            [
                Announcement(
                    text:
                        "Transmit stopped: the Mac went to sleep. Release the button, then press again.",
                    priority: .high)
            ])
    }

    func testBluetoothReleaseLinkLostIsLeftToTheLinkLostChannel() {
        // A keyed link loss is reported through blePttLinkLost; answering
        // here too would say it twice.
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.bluetoothRelease(reason: .linkLost, shortName: "B02PTT"), [])
    }

    func testBluetoothReleaseByTheButtonOrADisableIsSilent() {
        // Letting go of the button and switching configs are the user's own
        // actions, not forced stops.
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.bluetoothRelease(reason: .button, shortName: "B02PTT"), [])
        XCTAssertEqual(planner.bluetoothRelease(reason: .disabled, shortName: "B02PTT"), [])
    }

    // MARK: - Forced-stop channel (CallSession.lastForcedStop)

    private func engineStop(
        seq: UInt32, reason: StationPTTForcedReason = .txTimeout,
        at: Date = Date(timeIntervalSince1970: 1_000)
    ) -> PTTForcedStop {
        PTTForcedStop(
            seq: seq, sources: [.bluetooth], reason: reason, txTimeoutSeconds: 120, at: at)
    }

    func testForcedStopReplayedOnSubscribeIsSilent() {
        // Combine hands a fresh subscriber the current value; a stop from
        // before the announcer existed is not news.
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(planner.forcedStopChanged(to: engineStop(seq: 4)), [])
    }

    func testNewForcedStopIsAnnouncedOnce() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.forcedStopChanged(to: nil)  // primed
        XCTAssertEqual(
            planner.forcedStopChanged(to: engineStop(seq: 1)),
            [
                Announcement(
                    text: "Transmit stopped after 2 minutes. Release to talk again.",
                    priority: .high)
            ])
        XCTAssertEqual(planner.forcedStopChanged(to: engineStop(seq: 1)), [])
    }

    func testReplayedForcedStopStaysSilentButTheNextOneSpeaks() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.forcedStopChanged(to: engineStop(seq: 4))  // replay
        XCTAssertEqual(planner.forcedStopChanged(to: engineStop(seq: 4)), [])
        XCTAssertEqual(
            planner.forcedStopChanged(to: engineStop(seq: 5, reason: .leaseExpired)),
            [
                Announcement(
                    text:
                        "Transmit stopped: astar stopped responding. Release the button, then press again.",
                    priority: .high)
            ])
    }

    /// Saving credentials rebuilds the station, and a rebuilt engine counts
    /// forced stops from 1 again. Its first stop has the old seq but is a new
    /// stop (a new `at`), so it must still speak.
    func testSameSeqFromARebuiltStationStillSpeaks() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.forcedStopChanged(to: nil)  // primed
        _ = planner.forcedStopChanged(
            to: engineStop(seq: 1, at: Date(timeIntervalSince1970: 1_000)))
        XCTAssertEqual(
            planner.forcedStopChanged(
                to: engineStop(seq: 1, at: Date(timeIntervalSince1970: 2_000))),
            [
                Announcement(
                    text: "Transmit stopped after 2 minutes. Release to talk again.",
                    priority: .high)
            ])
    }

    func testForcedStopClearingIsSilent() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.forcedStopChanged(to: nil)
        _ = planner.forcedStopChanged(to: engineStop(seq: 1))
        XCTAssertEqual(planner.forcedStopChanged(to: nil), [])
    }

    // MARK: - Bluetooth forced-release channel (BluetoothPTTController.lastForcedStop)

    private func bleRelease(
        seq: Int, reason: BLEReleaseReason, at: Date = Date(timeIntervalSince1970: 10_000)
    ) -> BluetoothForcedRelease {
        BluetoothForcedRelease(seq: seq, reason: reason, at: at)
    }

    func testBluetoothReleaseReplayedOnSubscribeIsSilent() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.bluetoothForcedReleaseChanged(
                to: bleRelease(seq: 1, reason: .linkLost), shortName: "B02PTT"),
            [])
    }

    func testKeyedLinkLossIsAnnouncedOnceWithTheDeviceName() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothForcedReleaseChanged(to: nil, shortName: "X9PTT")
        XCTAssertEqual(
            planner.bluetoothForcedReleaseChanged(
                to: bleRelease(seq: 1, reason: .linkLost), shortName: "X9PTT"),
            [
                Announcement(
                    text:
                        "Transmit stopped: X9PTT disconnected. Release the button, then press again.",
                    priority: .high)
            ])
        XCTAssertEqual(
            planner.bluetoothForcedReleaseChanged(
                to: bleRelease(seq: 1, reason: .linkLost), shortName: "X9PTT"),
            [])
    }

    func testAdapterOffReleaseIsAnnouncedOnTheChannel() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothForcedReleaseChanged(to: nil, shortName: "B02PTT")
        XCTAssertEqual(
            planner.bluetoothForcedReleaseChanged(
                to: bleRelease(seq: 2, reason: .adapterOff), shortName: "B02PTT"),
            [
                Announcement(
                    text:
                        "Transmit stopped: Bluetooth turned off. Release the button, then press again.",
                    priority: .high)
            ])
    }

    func testButtonReleaseIsSilentOnTheChannel() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothForcedReleaseChanged(to: nil, shortName: "B02PTT")
        XCTAssertEqual(
            planner.bluetoothForcedReleaseChanged(
                to: bleRelease(seq: 3, reason: .button), shortName: "B02PTT"),
            [])
    }

    func testReleaseWithNoDeviceAttachedIsSilentAndConsumed() {
        // After a switch to a serial config there is no device to name. The
        // release is still consumed, so it cannot speak later.
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothForcedReleaseChanged(to: nil, shortName: nil)
        XCTAssertEqual(
            planner.bluetoothForcedReleaseChanged(
                to: bleRelease(seq: 1, reason: .linkLost), shortName: nil),
            [])
        XCTAssertEqual(
            planner.bluetoothForcedReleaseChanged(
                to: bleRelease(seq: 1, reason: .linkLost), shortName: "B02PTT"),
            [])
    }

    // MARK: - Bluetooth link-status channel (BluetoothPTTController.status)

    private let statusNow = Date(timeIntervalSince1970: 10_000)

    private let idleDrop = [
        Announcement(text: "Bluetooth PTT button disconnected", priority: .medium)
    ]

    func testBluetoothStatusReplayedOnSubscribeIsSilent() {
        let planner = AccessibilityAnnouncementPlanner()
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .reconnecting, lastForcedStop: nil, now: statusNow),
            [])
    }

    func testIdleDropFromReadyIsAQuietNotice() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .reconnecting, lastForcedStop: nil, now: statusNow),
            idleDrop)
    }

    func testKeyedDropIsLeftToTheForcedReleaseChannel() {
        // A fresh .linkLost release means the button was keyed; the
        // forced-release channel says so at .high, so this one stays quiet.
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .reconnecting,
                lastForcedStop: bleRelease(
                    seq: 1, reason: .linkLost, at: statusNow.addingTimeInterval(-1)),
                now: statusNow),
            [])
    }

    func testStaleLinkLostReleaseDoesNotSwallowALaterDrop() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .reconnecting,
                lastForcedStop: bleRelease(
                    seq: 1, reason: .linkLost, at: statusNow.addingTimeInterval(-11)),
                now: statusNow),
            idleDrop)
    }

    func testOneLinkLostReleaseExplainsOnlyOneDrop() {
        let planner = AccessibilityAnnouncementPlanner()
        let release = bleRelease(seq: 1, reason: .linkLost, at: statusNow.addingTimeInterval(-1))
        _ = planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .reconnecting, lastForcedStop: release, now: statusNow),
            [])
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .ready, lastForcedStop: release, now: statusNow), [])
        // Second drop 2 s later, still inside the window, with no new release:
        // nothing was keyed this time.
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .reconnecting, lastForcedStop: release,
                now: statusNow.addingTimeInterval(2)),
            idleDrop)
    }

    func testFailedFirstConnectIsNotADrop() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .connecting, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .reconnecting, lastForcedStop: nil, now: statusNow),
            [])
    }

    func testDropFromReleaseToArmOrNeedsConfirmIsADrop() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .releaseToArm, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .reconnecting, lastForcedStop: nil, now: statusNow),
            idleDrop)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .needsConfirm, lastForcedStop: nil, now: statusNow),
            [])
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .reconnecting, lastForcedStop: nil, now: statusNow),
            idleDrop)
    }

    func testANonLinkLostReleaseDoesNotExplainADrop() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .reconnecting,
                lastForcedStop: bleRelease(
                    seq: 2, reason: .button, at: statusNow.addingTimeInterval(-1)),
                now: statusNow),
            idleDrop)
    }

    /// The Mac sleeping while the button is held: the forced-release channel
    /// says "the Mac went to sleep" at .high, so the drop that follows (sleep
    /// cancels the link) must not add a second announcement.
    func testKeyedSleepDropIsLeftToTheForcedReleaseChannel() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .reconnecting,
                lastForcedStop: bleRelease(
                    seq: 1, reason: .sleep, at: statusNow.addingTimeInterval(-1)),
                now: statusNow),
            [])
    }

    func testOtherStatusChangesAreSilent() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .searching, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .connecting, lastForcedStop: nil, now: statusNow), [])
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow), [])
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .bluetoothOff, lastForcedStop: nil, now: statusNow), [])
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .disabled, lastForcedStop: nil, now: statusNow), [])
    }

    /// The conditional latch (decided 2026-10-09, Rob): an idle drop relinks
    /// to `.ready`, not `.releaseToArm`. The relink itself says nothing, and
    /// because `.ready` is a linked status the next idle drop is news again.
    func testIdleRelinkToReadyIsSilentAndTheNextDropSpeaks() {
        let planner = AccessibilityAnnouncementPlanner()
        _ = planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .reconnecting, lastForcedStop: nil, now: statusNow),
            idleDrop)
        XCTAssertEqual(
            planner.bluetoothStatusChanged(
                to: .connecting, lastForcedStop: nil, now: statusNow), [])
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .ready, lastForcedStop: nil, now: statusNow), [])
        XCTAssertEqual(
            planner.bluetoothStatusChanged(to: .reconnecting, lastForcedStop: nil, now: statusNow),
            idleDrop)
    }
}
