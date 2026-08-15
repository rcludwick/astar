// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Combine
import Foundation

/// High-churn live telemetry — the VU levels and round-trip time — split off
/// `CallSession` so their ~20 Hz poll updates re-render only the small views
/// that display them (astar-3e04). When these lived as `@Published` fields on
/// the session, every tick invalidated every view observing it: the whole
/// popover re-laid-out 20×/s for hours, saturating the main thread and
/// eventually beach-balling.
///
/// Levels are quantized to 0.5 dB before publishing, so mic/rx noise-floor
/// jitter below a visible step publishes nothing at all. Consumers that need
/// the raw level (the VOX gate, the rx-activity gate) read the snapshot
/// directly inside `poll()` and never see the quantization.
public final class CallMeters: ObservableObject {
    /// Instantaneous TX level (dBFS), post-gate: floored while unkeyed.
    @Published public private(set) var txDB: Float = -60
    /// Instantaneous RX level (dBFS).
    @Published public private(set) var rxDB: Float = -60
    /// ~250 ms peak-held TX/RX levels for the VU meters (astar-f78a).
    @Published public private(set) var txDBHeld: Float = -60
    @Published public private(set) var rxDBHeld: Float = -60
    /// Continuous mic input level (dBFS), metered even while unkeyed — drives
    /// the VOX calibration meter (the gate itself keys from the raw snapshot).
    @Published public private(set) var inputDB: Float = -60
    /// Round-trip time to the peer, when the call has measured one.
    @Published public private(set) var rttMS: Int?

    /// Raw, unquantized instantaneous levels for consumers that redraw on their
    /// own clock — the level graph's `TimelineView` reads these each tick
    /// (astar-3458: the 0.5 dB grid turned its trace into staircases).
    /// Deliberately NOT `@Published`: they change on every poll and would
    /// reintroduce exactly the churn this class exists to stop.
    public private(set) var rawTxDB: Float = -60
    public private(set) var rawRxDB: Float = -60

    /// Snap a dB level to the 0.5 dB display grid. Anything finer is invisible
    /// on a meter but would churn SwiftUI on every poll tick.
    static func quantize(_ db: Float) -> Float { (db * 2).rounded() / 2 }

    /// Advance from one poll tick, publishing only fields that visibly moved.
    func update(
        txDB: Float, rxDB: Float, inputDB: Float, txDBHeld: Float, rxDBHeld: Float, rttMS: Int?
    ) {
        rawTxDB = txDB
        rawRxDB = rxDB
        let tx = Self.quantize(txDB)
        let rx = Self.quantize(rxDB)
        let input = Self.quantize(inputDB)
        let txHeld = Self.quantize(txDBHeld)
        let rxHeld = Self.quantize(rxDBHeld)
        if self.txDB != tx { self.txDB = tx }
        if self.rxDB != rx { self.rxDB = rx }
        if self.inputDB != input { self.inputDB = input }
        if self.txDBHeld != txHeld { self.txDBHeld = txHeld }
        if self.rxDBHeld != rxHeld { self.rxDBHeld = rxHeld }
        if self.rttMS != rttMS { self.rttMS = rttMS }
    }
}
