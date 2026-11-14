// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The controller's own clock. It drives lease renewals and RSSI probes.
///
/// It is separate from `CallSession`'s 20 Hz poll because the app pauses that
/// poll on non-call panes. If renewals rode on the poll, opening Settings
/// mid-over would let the engine's lease lapse and cut the transmission.
@MainActor
public protocol BluetoothPTTTicker: AnyObject {
    /// Replaces any running schedule.
    func start(everyMS: UInt32, _ tick: @escaping @MainActor () -> Void)
    func stop()
}

/// The production ticker: a `DispatchSourceTimer` on the main queue, so every
/// tick runs on the main actor beside the CoreBluetooth callbacks.
@MainActor
public final class DispatchBluetoothPTTTicker: BluetoothPTTTicker {
    private var timer: DispatchSourceTimer?

    /// Nonisolated so it can be the controller's default argument: it only
    /// sets an empty timer slot, and every use after it is on the main actor.
    nonisolated public init() {}

    public func start(everyMS: UInt32, _ tick: @escaping @MainActor () -> Void) {
        stop()
        let period = DispatchTimeInterval.milliseconds(Int(max(everyMS, 1)))
        let source = DispatchSource.makeTimerSource(queue: .main)
        source.schedule(deadline: .now() + period, repeating: period, leeway: .milliseconds(5))
        source.setEventHandler { MainActor.assumeIsolated { tick() } }
        source.resume()
        timer = source
    }

    public func stop() {
        timer?.cancel()
        timer = nil
    }
}
