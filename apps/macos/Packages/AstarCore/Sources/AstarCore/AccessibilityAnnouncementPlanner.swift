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

    // MARK: - Forced stops (high) and Bluetooth PTT link loss: wording

    /// An engine forced stop: the transmit time-out fired, or the Bluetooth
    /// lease expired because the app stopped renewing it. The wording is the
    /// banner's, built by the same function
    /// (`BluetoothPTTPresentation.forcedStopBanner`), so VoiceOver and the
    /// orange banner can never say two different things about one stop.
    /// `.high`: the operator was on air and now is not.
    public func pttForcedStop(reason: StationPTTForcedReason, txTimeoutSeconds: Int)
        -> [Announcement]
    {
        // Only `reason` and `txTimeoutSeconds` reach the wording; the other
        // fields are bookkeeping the banner's 8 s window uses.
        let stop = PTTForcedStop(
            seq: 0, sources: [], reason: reason, txTimeoutSeconds: txTimeoutSeconds,
            at: Date(timeIntervalSince1970: 0))
        return [
            Announcement(text: BluetoothPTTPresentation.forcedStopBanner(stop), priority: .high)
        ]
    }

    /// The Bluetooth PTT button's link went down. `whileKeyed` true means it was
    /// holding the key, so the engine unkeyed and the button must be released
    /// before it keys again: that is the banner's text, at `.high`. False means
    /// nothing was on air, so it is a `.medium` notice that queues behind
    /// whatever VoiceOver is saying.
    ///
    /// The device name comes from the device table, not from a literal here,
    /// so AstarCore never carries a device fact of its own. The announcer's
    /// path goes through `bluetoothForcedReleaseChanged`, which passes the
    /// attached device's `namePrefix` to the overload below.
    public func blePttLinkLost(whileKeyed: Bool) -> [Announcement] {
        blePttLinkLost(
            whileKeyed: whileKeyed,
            shortName: BLEButtonDevices.all.first?.namePrefix ?? "Bluetooth PTT")
    }

    /// The worker behind `blePttLinkLost(whileKeyed:)`, with the device's short
    /// name passed in. Internal: the forced-release channel calls it with the
    /// attached device's name, and the tests call it with a name that is not
    /// in the table so a hard-coded name turns them red.
    func blePttLinkLost(whileKeyed: Bool, shortName: String) -> [Announcement] {
        guard whileKeyed else {
            return [Announcement(text: "Bluetooth PTT button disconnected", priority: .medium)]
        }
        guard let text = BluetoothPTTPresentation.releaseBanner(.linkLost, shortName: shortName)
        else { return [] }
        return [Announcement(text: text, priority: .high)]
    }

    /// Any other forced Bluetooth release: adapter off, the Mac sleeping, or a
    /// value the button should never send. Returns nothing for `.button` and
    /// `.disabled` (the user let go, or switched configs: not forced) and for
    /// `.linkLost`, which `blePttLinkLost(whileKeyed:)` reports, so one loss is
    /// announced once.
    public func bluetoothRelease(reason: BLEReleaseReason, shortName: String) -> [Announcement] {
        guard reason != .linkLost,
            let text = BluetoothPTTPresentation.releaseBanner(reason, shortName: shortName)
        else { return [] }
        return [Announcement(text: text, priority: .high)]
    }

    // MARK: - Forced-stop channel (high)

    private var primedForcedStop = false
    private var lastForcedStop: PTTForcedStop?

    /// Feed every `CallSession.lastForcedStop` delivery. Speaks once per
    /// forced stop: the value Combine replays on subscribe is swallowed (the
    /// "primed" rule), a re-publish of the same stop is silent, and the value
    /// clearing to nil says nothing. A stop is compared whole, not by seq: a
    /// station rebuilt after a credential change counts from 1 again, and its
    /// first stop (same seq, new `at`) is news.
    public func forcedStopChanged(to stop: PTTForcedStop?) -> [Announcement] {
        guard primedForcedStop else {
            primedForcedStop = true
            lastForcedStop = stop
            return []
        }
        guard let stop, stop != lastForcedStop else { return [] }
        lastForcedStop = stop
        return pttForcedStop(reason: stop.reason, txTimeoutSeconds: stop.txTimeoutSeconds)
    }

    // MARK: - Bluetooth forced-release channel (high)

    private var primedBluetoothRelease = false
    private var lastBluetoothReleaseSeq: Int?

    /// Feed every `BluetoothPTTController.lastForcedStop` delivery, with the
    /// attached device's short name (nil when no Bluetooth device is attached).
    /// Same primed and seq rules as `forcedStopChanged(to:)`. A `.linkLost`
    /// release means the button was keyed when the link went, so it gets the
    /// keyed link-loss text here and the status channel stays quiet for it.
    /// With no device the release is consumed silently: after a switch to a
    /// serial config an old release must not speak.
    public func bluetoothForcedReleaseChanged(
        to release: BluetoothForcedRelease?, shortName: String?
    ) -> [Announcement] {
        guard primedBluetoothRelease else {
            primedBluetoothRelease = true
            lastBluetoothReleaseSeq = release?.seq
            return []
        }
        guard let release, release.seq != lastBluetoothReleaseSeq else { return [] }
        lastBluetoothReleaseSeq = release.seq
        guard let shortName else { return [] }
        if release.reason == .linkLost {
            return blePttLinkLost(whileKeyed: true, shortName: shortName)
        }
        return bluetoothRelease(reason: release.reason, shortName: shortName)
    }

    // MARK: - Bluetooth link-status channel (medium)

    /// `.disabled` until the first delivery. The value Combine replays on
    /// subscribe can therefore never be a drop: a drop needs a linked
    /// previous status, and `.disabled` is not one.
    private var lastBluetoothStatus: BLELinkStatus = .disabled
    /// The keyed release (`.linkLost` or `.sleep`) the status channel last
    /// attributed a drop to, so one release explains one drop and a later
    /// drop still speaks.
    private var forcedReleaseSeqSeenByStatus: Int?
    /// The release reasons that mean the button was keyed when the link
    /// went, and that the forced-release channel already announces: a lost
    /// link, and the Mac going to sleep (sleep cancels the link too).
    private static let keyedLossReasons: Set<BLEReleaseReason> = [.linkLost, .sleep]

    /// How recent a `.linkLost` release must be to explain a drop. It bounds
    /// the RSSI path (two missed probes, then the link is cancelled) with
    /// room to spare, and stops a stale release from swallowing a later,
    /// unrelated disconnect.
    static let keyedLossWindow: TimeInterval = 10

    /// Statuses in which the link is up, so a move to `.reconnecting` is a
    /// real drop and not a failed first connect.
    private static let linkedStatuses: Set<BLELinkStatus> = [.ready, .releaseToArm, .needsConfirm]

    /// Feed every `BluetoothPTTController.status` delivery, with the
    /// controller's current `lastForcedStop` and the time. Speaks only on a
    /// linked → `.reconnecting` edge. If a fresh `.linkLost` or `.sleep`
    /// release the status channel has not used yet explains the drop, the button was keyed
    /// and the forced-release channel owns it, so this returns nothing.
    /// Otherwise nothing was on air and it is the `.medium` notice. One loss,
    /// one announcement, whichever channel is delivered first.
    public func bluetoothStatusChanged(
        to newStatus: BLELinkStatus, lastForcedStop: BluetoothForcedRelease?, now: Date
    ) -> [Announcement] {
        defer { lastBluetoothStatus = newStatus }
        guard newStatus == .reconnecting, Self.linkedStatuses.contains(lastBluetoothStatus)
        else { return [] }
        if let stop = lastForcedStop, Self.keyedLossReasons.contains(stop.reason),
            stop.seq != forcedReleaseSeqSeenByStatus,
            now.timeIntervalSince(stop.at) < Self.keyedLossWindow
        {
            forcedReleaseSeqSeenByStatus = stop.seq
            return []
        }
        return blePttLinkLost(whileKeyed: false)
    }
}
