// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// StationPTT.swift — the station's PTT sources, as the C-ABI numbers them.
//
// The engine keys the radio while ANY source holds (an OR). It latches a source
// after a forced stop until that source is released, and it enforces the
// Bluetooth lease and the one transmit time-out itself. These types only name
// the sources and read the masks back: no keying decision is made in Swift.

import CAstarStation

/// One way of keying the station. Mirrors the C-ABI's `IAX_PTT_*` values.
///
/// It is named `StationPTTSource`, not `PTTSource`, because AstarCore already
/// has a `PTTSource` (a hardware profile's default keying method), and the two
/// mean different things.
public enum StationPTTSource: UInt32, Sendable, CaseIterable, CustomStringConvertible {
    /// The on-screen PTT button and the Space bar, which share one source.
    case manual = 0
    /// Voice-operated keying, driven by the app's VOX gate.
    case vox = 1
    /// A USB radio interface's PTT line, read by the serial poll.
    case serial = 2
    /// A Bluetooth LE button. It keys only through a lease that the engine
    /// expires on its own, so a wedged app cannot hold the key.
    case bluetooth = 3
    /// `setPTT(_:)` callers that predate sources: the node daemon, the CLI,
    /// Python and the paused Iced client.
    case legacy = 4

    /// This source's bit in a `StationPTTSourceSet`.
    public var bit: StationPTTSourceSet { StationPTTSourceSet(rawValue: 1 << rawValue) }

    /// The lowercase name used in logs and test transcripts.
    public var description: String {
        switch self {
        case .manual: return "manual"
        case .vox: return "vox"
        case .serial: return "serial"
        case .bluetooth: return "bluetooth"
        case .legacy: return "legacy"
        }
    }

    /// The value the C-ABI expects. It is read from the header, not restated,
    /// so a renumbered `IAX_PTT_*` fails `testSourceRawValuesMatchTheHeader`
    /// instead of keying the wrong source.
    var abiValue: UInt32 {
        switch self {
        case .manual: return UInt32(IAX_PTT_MANUAL)
        case .vox: return UInt32(IAX_PTT_VOX)
        case .serial: return UInt32(IAX_PTT_SERIAL)
        case .bluetooth: return UInt32(IAX_PTT_BLUETOOTH)
        case .legacy: return UInt32(IAX_PTT_LEGACY)
        }
    }
}

/// A set of PTT sources, as the snapshot's `ptt_*` masks carry them
/// (bit = `1 << IAX_PTT_*`).
public struct StationPTTSourceSet: OptionSet, Sendable, Hashable {
    public let rawValue: UInt32
    public init(rawValue: UInt32) { self.rawValue = rawValue }
    public static let manual = StationPTTSourceSet(rawValue: 1 << 0)
    public static let vox = StationPTTSourceSet(rawValue: 1 << 1)
    public static let serial = StationPTTSourceSet(rawValue: 1 << 2)
    public static let bluetooth = StationPTTSourceSet(rawValue: 1 << 3)
    public static let legacy = StationPTTSourceSet(rawValue: 1 << 4)
}

/// Why the engine stopped transmitting on its own. `IAX_PTT_FORCED_NONE` (0)
/// has no case: the snapshot reports it as `nil`.
public enum StationPTTForcedReason: UInt32, Sendable {
    /// The Bluetooth button's lease ran out without a renewal (the app or the
    /// link went quiet).
    case leaseExpired = 1
    /// The radio was on air, from any mix of sources, for the whole transmit
    /// time-out.
    case txTimeout = 2
}

extension StationError {
    /// `IAX_ERR_NOT_LEASED`: a renew on a source that holds no lease. A renew
    /// never presses, so this is how a lost lease is noticed.
    public static let notLeasedCode: Int32 = -25
    /// `IAX_ERR_INVALID`: an argument outside its range. Examples are a
    /// time-out outside 60...600 s, a lease TTL outside 1...5000 ms, or a lease
    /// on a source other than Bluetooth.
    public static let invalidArgumentCode: Int32 = -26
    /// `IAX_ERR_PTT_LATCHED`: a press on a source that a forced stop latched.
    /// Release it first.
    public static let pttLatchedCode: Int32 = -27
}
