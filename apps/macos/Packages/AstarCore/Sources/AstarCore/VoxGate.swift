// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Tuning for the voice-activated PTT gate.
public struct VoxConfig: Equatable {
    /// Mic level (dBFS) at/above which the gate opens.
    public var thresholdDBFS: Float
    /// How long the level must stay below threshold, after speech, before the
    /// gate closes again — bridges the natural gaps between words AND short
    /// pauses mid-sentence so a brief silence doesn't drop PTT. Default 500 ms
    /// (a half-second tail); any sample back over threshold resets it.
    public var hangoverMS: Int
    /// How long the level must stay at/above threshold before the gate opens —
    /// rejects momentary clicks. 0 = open on the first sample over threshold.
    public var attackMS: Int

    public init(thresholdDBFS: Float = -40, hangoverMS: Int = 500, attackMS: Int = 0) {
        self.thresholdDBFS = thresholdDBFS
        self.hangoverMS = hangoverMS
        self.attackMS = attackMS
    }
}

/// A pure, deterministic voice-activity gate. Feed it the mic level once per
/// poll along with the current time (injected so tests need no real sleeps); it
/// returns whether PTT should be keyed.
///
/// Behaviour:
///  - Below threshold and closed → stays closed.
///  - At/above threshold for `attackMS` → opens (keyed).
///  - Once open, stays open until the level stays below threshold for the full
///    `hangoverMS`; any sample back over threshold resets the hangover.
public struct VoxGate {
    public var config: VoxConfig

    private var keyed = false
    /// When the level first rose at/above threshold while still closed (attack).
    private var aboveSince: Date?
    /// When the level first dropped below threshold while open (hangover).
    private var belowSince: Date?

    public init(config: VoxConfig = VoxConfig()) {
        self.config = config
    }

    /// Advance the gate by one sample. Returns the keyed state to apply.
    public mutating func update(level: Float, now: Date) -> Bool {
        let isAbove = level >= config.thresholdDBFS

        if keyed {
            if isAbove {
                belowSince = nil  // speech resumed — cancel hangover
            } else {
                let start = belowSince ?? now
                belowSince = start
                if elapsedMS(from: start, to: now) >= config.hangoverMS {
                    keyed = false  // hangover elapsed — close
                    belowSince = nil
                }
            }
        } else {
            if isAbove {
                let start = aboveSince ?? now
                aboveSince = start
                if elapsedMS(from: start, to: now) >= config.attackMS {
                    keyed = true  // attack satisfied — open
                    aboveSince = nil
                    belowSince = nil
                }
            } else {
                aboveSince = nil  // dipped back down — reset attack
            }
        }

        return keyed
    }

    /// Whole milliseconds between two instants, rounded to avoid binary-float
    /// boundary misses (e.g. 0.050s → 49.999…).
    private func elapsedMS(from start: Date, to now: Date) -> Int {
        Int((now.timeIntervalSince(start) * 1000).rounded())
    }
}
