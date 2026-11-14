// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// What the Bluetooth controller asks the session to do with the engine.
public enum BluetoothPTTIntent: Equatable, Sendable {
    /// A fresh, gated press: take the engine lease.
    case press
    /// Keep an existing lease alive. It never presses.
    case renew
    /// Let go. It is never refused.
    case release
}

/// How the session answered.
public enum BluetoothPTTOutcome: Equatable, Sendable {
    case accepted
    /// The client gate refused. The station was not called.
    case refused
    /// The engine refused or no lease is held. The machine must latch and wait
    /// for a release, matching what the arbiter now believes.
    case leaseLost
}

/// The controller talks to this, not to `CallSession` directly, so its tests
/// can drive a real session while the choke point stays the one door.
public protocol BluetoothPTTSink: AnyObject {
    func bluetoothPTT(_ intent: BluetoothPTTIntent) -> BluetoothPTTOutcome
    /// Listen-only, or a receive-only network: the chip says "won't transmit".
    var transmitBlocked: Bool { get }
}

extension CallSession: BluetoothPTTSink {
    /// The engine lease a Bluetooth press takes: 1.5 s, renewed every 250 ms,
    /// and enforced by the engine, so a client that stops renewing unkeys on
    /// its own. It comes from the Rust machine's config so the two can never
    /// disagree.
    public static var bluetoothLeaseTTL: Duration {
        .milliseconds(BLEButtonMachine.defaultConfig.leaseTTLMS)
    }

    /// The Bluetooth choke point. It is driven from the CoreBluetooth callback
    /// and the controller's own ticker, never from the poll.
    ///
    /// A press is refused here unless a call is answered, listen-only is off
    /// and the network can transmit. The station is not called at all, so the
    /// arbiter never records a hold for a press made while dialing, and a
    /// button held through the answer cannot key the moment the call connects.
    /// Renewal never presses: the engine extends only a lease it already
    /// holds. Release is unconditional, because letting go must always work.
    public func bluetoothPTT(_ intent: BluetoothPTTIntent) -> BluetoothPTTOutcome {
        switch intent {
        case .press:
            guard status == .answered, !txDisabled, canTransmit else { return .refused }
            do {
                try station.pttLeasePress(.bluetooth, ttl: Self.bluetoothLeaseTTL)
                return .accepted
            } catch {
                return .leaseLost
            }
        case .renew:
            do {
                try station.pttLeaseRenew(.bluetooth)
                return .accepted
            } catch {
                return .leaseLost
            }
        case .release:
            try? station.pttSource(.bluetooth, on: false)
            return .accepted
        }
    }
}
