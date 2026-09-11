// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The bounds an RX jitter-buffer depth window may take (iax-rxjb).
///
/// The engine clamps and repairs whatever it is handed — both bounds to
/// `0...500` ms, and a `max` below `min` is raised to meet it — so a setting
/// is never refused. The UI applies the same rules *before* it stores or
/// pushes anything, so the sliders can't show a window the engine would not
/// honour, and the persisted value is the one that is actually running.
public enum RxJitterBounds {
    /// The window's hard limits, in ms. Mirrors the engine's clamp.
    public static let range = 0...500
    /// What one press of a stepper moves. Milliseconds finer than this are
    /// below the 20 ms frame the buffer schedules in.
    public static let step = 10

    /// Clamp one bound into `range`.
    public static func clamped(_ ms: Int) -> Int {
        min(max(ms, range.lowerBound), range.upperBound)
    }

    /// Repair a whole window: both bounds clamped, and the ceiling raised to
    /// the floor if it fell below it. `movingMin` says which bound the
    /// operator just dragged, so the OTHER one gives way — pushing `min` past
    /// `max` lifts `max`, and pulling `max` under `min` drops `min`. Without
    /// that the control would fight the drag.
    public static func repaired(min minMS: Int, max maxMS: Int, movingMin: Bool)
        -> (min: Int, max: Int)
    {
        let lo = clamped(minMS)
        let hi = clamped(maxMS)
        guard lo > hi else { return (lo, hi) }
        return movingMin ? (lo, lo) : (hi, hi)
    }
}

/// The receive path's health, as the call-quality line reads it (iax-rxjb).
///
/// A value type so `CallMeters` can publish it on change rather than on every
/// 20 Hz poll tick: the numbers move slowly, the poll does not.
public struct RxQuality: Equatable {
    /// Whether the RX jitter buffer is actually running — reported by the
    /// engine, so the line says what is happening rather than what was last
    /// asked for.
    public var jitterBufferEnabled: Bool
    /// Measured network jitter, ms.
    public var jitterMS: UInt32
    /// How much received audio the buffer is holding back, ms — the latency
    /// being paid for the smoothing.
    public var bufferDepthMS: UInt32
    /// Frames the buffer expected and did not play. Read it as "frames that
    /// did not reach the speaker", not as a packet-loss count.
    public var framesLost: UInt64
    /// Frames that arrived after their play time and were thrown away.
    public var framesLate: UInt64
    /// Device callbacks that got no audio at all while somebody was still
    /// talking. This is the alarm, not a statistic.
    public var underruns: UInt64

    public init(
        jitterBufferEnabled: Bool = true, jitterMS: UInt32 = 0, bufferDepthMS: UInt32 = 0,
        framesLost: UInt64 = 0, framesLate: UInt64 = 0, underruns: UInt64 = 0
    ) {
        self.jitterBufferEnabled = jitterBufferEnabled
        self.jitterMS = jitterMS
        self.bufferDepthMS = bufferDepthMS
        self.framesLost = framesLost
        self.framesLate = framesLate
        self.underruns = underruns
    }

    /// No call: the buffer's declared state, every counter at rest.
    public static let idle = RxQuality()
}

/// The one place that turns ``RxQuality`` into the popover's call-quality
/// line — views format through this and nowhere else, so the Mac and the Iced
/// client can be held to the same wording.
///
/// AllStarLink only. The jitter buffer schedules against a sender's wire
/// clock, and only the IAX2 path carries one: on M17, D-Star, YSF, NXDN and
/// DMR the decoded frames go straight to the bus, so every number here would
/// be a flat zero and the line would be a lie of omission.
public enum CallQualityLine {
    /// The line as it is drawn: `jitter 12 ms · buffer 60 ms · lost 0 · late 0`,
    /// with ` · underruns N` appended only when there are any — an underrun
    /// is the thing that went wrong, and a permanent "underruns 0" is one
    /// more number to read past before you find it.
    ///
    /// `nil` when the call is not on AllStarLink (including no call at all).
    public static func text(network: Network?, quality: RxQuality) -> String? {
        guard network == .allstar else { return nil }
        guard quality.jitterBufferEnabled else { return "jitter buffer off" }
        var line =
            "jitter \(quality.jitterMS) ms · buffer \(quality.bufferDepthMS) ms"
            + " · lost \(quality.framesLost) · late \(quality.framesLate)"
        if quality.underruns > 0 { line += " · underruns \(quality.underruns)" }
        return line
    }

    /// The same facts with the words spelled out, for VoiceOver — "12 ms" read
    /// as "12 ms" is a unit abbreviation a screen reader has to guess at, and
    /// the interpuncts are read as nothing at all.
    public static func spoken(network: Network?, quality: RxQuality) -> String? {
        guard network == .allstar else { return nil }
        guard quality.jitterBufferEnabled else { return "Jitter buffer off" }
        var line =
            "Network jitter \(quality.jitterMS) milliseconds,"
            + " buffer depth \(quality.bufferDepthMS) milliseconds,"
            + " \(quality.framesLost) frames lost, \(quality.framesLate) frames late"
        if quality.underruns > 0 { line += ", \(quality.underruns) underruns" }
        return line
    }
}
