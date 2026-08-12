// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A sliding-window peak-hold: reports the maximum value seen within the last
/// `window` seconds. A fast-jittering signal (e.g. a level meter polled ~20 Hz)
/// then reads steadily — a transient peak stays visible for `window` before the
/// window slides past it. Cheap: a small ring sized to the poll rate × window.
public struct PeakHold {
    private var samples: [(t: Date, v: Float)] = []
    private let window: TimeInterval

    public init(window: TimeInterval) {
        self.window = window
    }

    /// Record `value` at `now`, drop samples older than `window`, and return the
    /// max over the trailing window (the value itself if it's the only sample).
    public mutating func push(_ value: Float, now: Date) -> Float {
        samples.append((now, value))
        let cutoff = now.addingTimeInterval(-window)
        samples.removeAll { $0.t < cutoff }
        return samples.map(\.v).max() ?? value
    }
}
