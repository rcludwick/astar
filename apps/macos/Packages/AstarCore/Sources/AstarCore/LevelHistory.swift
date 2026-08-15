// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// One time-step of meter levels, in dBFS (−60…0).
public struct LevelSample: Equatable, Sendable {
    public var tx: Float
    public var rx: Float

    public init(tx: Float, rx: Float) {
        self.tx = tx
        self.rx = rx
    }
}

/// A fixed-capacity ring buffer of recent `(tx, rx)` level samples, oldest first.
///
/// Pure value type so it is trivially testable and SwiftUI-friendly (mutate a
/// `@State`/`@StateObject`-held copy, read `samples` to draw). When full, the
/// oldest sample is dropped on append, giving a scrolling time window.
public struct LevelHistory: Equatable, Sendable {
    public let capacity: Int
    private var storage: [LevelSample]

    /// Create a history holding at most `capacity` samples (clamped to ≥ 1).
    public init(capacity: Int) {
        self.capacity = max(1, capacity)
        self.storage = []
        self.storage.reserveCapacity(self.capacity)
    }

    /// Append a sample, dropping the oldest if at capacity.
    public mutating func append(tx: Float, rx: Float) {
        storage.append(LevelSample(tx: tx, rx: rx))
        if storage.count > capacity {
            storage.removeFirst(storage.count - capacity)
        }
    }

    /// The samples in chronological order (oldest first).
    public var samples: [LevelSample] { storage }

    /// Number of samples currently held.
    public var count: Int { storage.count }

    /// The TX levels in order.
    public var txSeries: [Float] { storage.map(\.tx) }

    /// The RX levels in order.
    public var rxSeries: [Float] { storage.map(\.rx) }
}
