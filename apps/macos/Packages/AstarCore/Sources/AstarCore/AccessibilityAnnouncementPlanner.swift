// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import AstarStation
import Foundation

/// One VoiceOver announcement the poster should post, plus how urgently.
/// Pure data — no AppKit dependency, so it's constructible/comparable in
/// tests without a screen reader running.
public struct Announcement: Equatable, Sendable {
    /// Mirrors `NSAccessibilityPriorityLevel`: `.high` interrupts (on-air
    /// state changes — PTT, talk timer, TX enable); `.medium` queues behind
    /// whatever's already speaking (everything else).
    public enum Priority: Sendable {
        case high
        case medium
    }

    public let text: String
    public let priority: Priority

    public init(text: String, priority: Priority) {
        self.text = text
        self.priority = priority
    }
}

/// Pure, testable edge-detector for astar's accessibility announcements
/// (astar-b167, audit finding F6 + the announcement halves of F12/F21/F22/F24).
///
/// The AppKit-side poster owns one long-lived instance and feeds it each
/// `CallSession` published property's new value from its own Combine sink —
/// this type does the rest: remembers the previous value, decides whether the
/// change is a real transition worth announcing (vs. noise/bounce/the initial
/// value Combine replays to a fresh subscriber), and produces the exact
/// wording. ALL wording lives here so the poster stays a dumb relay.
///
/// Every `...Changed` method independently tracks "have I seen a first value
/// yet" for its own channel and swallows that first call — the "primed"
/// guardrail: without it, the poster's very first subscribe (which Combine
/// delivers the CURRENT value for) would announce e.g. "Call ended" at
/// launch, before anything real happened.
public final class AccessibilityAnnouncementPlanner {
    public init() {}

    // MARK: - Status transitions (medium)

    private var primedStatus = false
    private var lastStatus: IaxStatus = .idle

    /// `connectedTarget` is whatever `CallSession.connectedTargetLabel(for:)`
    /// returns for the current `dialedNode` — the same string the status
    /// card shows, so wording never drifts from the visual UI.
    public func statusChanged(to newStatus: IaxStatus, connectedTarget: String?) -> [Announcement] {
        defer { lastStatus = newStatus }
        guard primedStatus else {
            primedStatus = true
            return []
        }
        guard newStatus != lastStatus else { return [] }
        if newStatus == .answered {
            let target = connectedTarget ?? "node"
            return [Announcement(text: "Connected to \(target)", priority: .medium)]
        }
        if lastStatus == .answered {
            // Leaving `.answered` — whether the engine reports the
            // intermediate `.hangup` or jumps straight to `.idle` — is one
            // "the call ended" event. Only the FIRST step away from
            // `.answered` fires this (guarded by `lastStatus == .answered`
            // above), so a later `.hangup` → `.idle` tick can't double-fire.
            return [Announcement(text: "Call ended", priority: .medium)]
        }
        return []
    }

    // MARK: - Dial failure (medium)

    private var primedDialFailure = false
    private var lastDialFailure: String?

    /// Watches `CallSession.lastDialFailure` — announces its text verbatim on
    /// every non-nil transition. Guards on `newValue != lastDialFailure` like
    /// every sibling channel, so a repeated call with the identical string
    /// (no intervening nil) is a no-op rather than a re-announcement.
    public func dialFailureChanged(to newValue: String?) -> [Announcement] {
        defer { lastDialFailure = newValue }
        guard primedDialFailure else {
            primedDialFailure = true
            return []
        }
        guard newValue != lastDialFailure else { return [] }
        guard let newValue else { return [] }
        return [Announcement(text: newValue, priority: .medium)]
    }

    // MARK: - PTT edges (high), with bounce coalescing

    /// Edges within this window of the previous edge are treated as contact
    /// bounce (relay chatter / a serial glitch) and swallowed rather than
    /// announced — the audit calls for "coalesce", not "report every wiggle".
    public static let pttBounceWindow: TimeInterval = 0.3

    private var primedPTT = false
    private var lastPTT = false
    private var lastPTTEdgeTime: Date?

    /// Watches `CallSession.$ptt` (covers hold-to-talk, VOX, and serial
    /// keying alike — it's the single fold-in point for all of them).
    public func pttChanged(to newValue: Bool, now: Date) -> [Announcement] {
        defer {
            lastPTT = newValue
            lastPTTEdgeTime = now
        }
        guard primedPTT else {
            primedPTT = true
            return []
        }
        guard newValue != lastPTT else { return [] }
        if let lastEdge = lastPTTEdgeTime, now.timeIntervalSince(lastEdge) < Self.pttBounceWindow {
            return []
        }
        return [
            Announcement(
                text: newValue ? "Transmitting" : "Transmission ended", priority: .high)
        ]
    }

    // MARK: - DTMF sequence complete (medium) — falling edge to 0

    private var primedDTMFTotal = false
    private var lastDTMFTotal = 0

    /// Watches `CallSession.$dtmfTotal` — announces only the falling edge
    /// back to 0 (a sequence finishing), never the rise/mid-sequence changes.
    public func dtmfTotalChanged(to newValue: Int) -> [Announcement] {
        defer { lastDTMFTotal = newValue }
        guard primedDTMFTotal else {
            primedDTMFTotal = true
            return []
        }
        guard newValue != lastDTMFTotal else { return [] }
        guard newValue == 0, lastDTMFTotal > 0 else { return [] }
        return [Announcement(text: "Command sent", priority: .medium)]
    }

    // MARK: - TX enable flips (high — an operating-state change)

    private var primedTxDisabled = false
    private var lastTxDisabled = false

    public func txDisabledChanged(to newValue: Bool) -> [Announcement] {
        defer { lastTxDisabled = newValue }
        guard primedTxDisabled else {
            primedTxDisabled = true
            return []
        }
        guard newValue != lastTxDisabled else { return [] }
        return [
            Announcement(
                text: newValue ? "TX disabled, listening only" : "TX enabled", priority: .high)
        ]
    }

    // MARK: - Talk-timer phase crossings (high)

    private var primedTalkTimer = false
    private var lastTalkTimerPhase: TalkTimer.Phase?

    /// Watches the phase the poster's 1 s-while-keyed timer computes from
    /// `CallSession.talkTimerPhase(...)`. Only entering `.amber`/`.red`
    /// announces — never `.green` (that's every key-down's starting phase;
    /// announcing it would fire on every single transmission) and never the
    /// `nil` an unkey resets to. Wording comes from `TalkTimer.help(for:)` —
    /// the SAME string the dot's tooltip/VO value uses.
    public func talkTimerPhaseChanged(to newPhase: TalkTimer.Phase?) -> [Announcement] {
        defer { lastTalkTimerPhase = newPhase }
        guard primedTalkTimer else {
            primedTalkTimer = true
            return []
        }
        guard newPhase != lastTalkTimerPhase else { return [] }
        guard let newPhase, newPhase != .green else { return [] }
        return [Announcement(text: TalkTimer.help(for: newPhase), priority: .high)]
    }
}
