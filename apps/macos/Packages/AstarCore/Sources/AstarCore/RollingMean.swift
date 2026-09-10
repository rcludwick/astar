// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// A running mean over the last `window` samples — the smoothing behind the mic
/// analyzer's threshold line. The live spectrum is peak-held and polled at
/// 20 Hz, so its scan-band median jitters from poll to poll; averaging a
/// second of it (20 samples) gives a line that holds still long enough to
/// read against the peaks. Non-finite samples are dropped rather than
/// poisoning the mean.
public struct RollingMean {
    public let window: Int
    private var samples: [Float] = []
    private var next = 0
    private var sum: Double = 0

    public init(window: Int) {
        self.window = max(1, window)
        samples.reserveCapacity(self.window)
    }

    public mutating func push(_ value: Float) {
        guard value.isFinite else { return }
        if samples.count < window {
            samples.append(value)
        } else {
            sum -= Double(samples[next])
            samples[next] = value
            next = (next + 1) % window
        }
        sum += Double(value)
    }

    /// The mean of the samples held so far, or `nil` before the first one.
    public var value: Float? {
        samples.isEmpty ? nil : Float(sum / Double(samples.count))
    }

    /// How many samples the mean currently covers (at most `window`).
    public var count: Int { samples.count }

    public mutating func reset() {
        samples.removeAll(keepingCapacity: true)
        next = 0
        sum = 0
    }
}
