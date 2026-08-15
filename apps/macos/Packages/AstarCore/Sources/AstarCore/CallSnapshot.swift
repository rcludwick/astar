// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation

/// A value snapshot of live call state, mirrored from the AstarStation binding.
///
/// Exists because `AstarStation.Snapshot` has no public initializer (the binding
/// fills it from C), so it can't be constructed in tests. `CallSnapshot` is the
/// testable, app-facing shape; the real `Station` maps into it.
public struct CallSnapshot: Equatable {
    public var status: IaxStatus
    public var ptt: Bool
    public var remotePTT: Bool
    public var txDB: Float
    public var rxDB: Float
    /// Continuous mic input level (dBFS), metered even while unkeyed — unlike
    /// `txDB`, which is post-gate (floored when not transmitting). This is the
    /// signal VOX keys from, so it can trigger from silence (iax-5c30).
    public var inputDB: Float
    public var rttMS: Int?
    /// Negotiated voice codec of the active call, or `nil` while idle or still
    /// negotiating (astar-eb6c). `.slin16` = wideband is live. Defaults `nil`
    /// so pre-existing fixtures (and `NullStation`) need no change.
    public var negotiatedFormat: VoiceFormat?
    /// Digits already sent of the active `sendDTMF(sequence:)` command
    /// (astar-7d21); `0` when no sequence is playing. Defaults 0 so
    /// pre-existing fixtures need no change.
    public var dtmfPlayed: Int
    /// Total digits of the active `sendDTMF(sequence:)` command; `0` when no
    /// sequence is playing.
    public var dtmfTotal: Int
    /// Whether the engine can dial M17 (iax-f2b8 Task 8) — codec2 resolved,
    /// M17 session support compiled in. Defaults `false` so pre-existing
    /// fixtures (and `NullStation`) need no change.
    public var m17Available: Bool
    /// Whether the live call is an M17 session (as opposed to IAX2). Defaults
    /// `false`; the two networks are mutually exclusive at the engine.
    public var m17Active: Bool

    public init(
        status: IaxStatus, ptt: Bool, remotePTT: Bool,
        txDB: Float, rxDB: Float, inputDB: Float = -60, rttMS: Int?,
        negotiatedFormat: VoiceFormat? = nil,
        dtmfPlayed: Int = 0, dtmfTotal: Int = 0,
        m17Available: Bool = false, m17Active: Bool = false
    ) {
        self.status = status
        self.ptt = ptt
        self.remotePTT = remotePTT
        self.txDB = txDB
        self.rxDB = rxDB
        self.inputDB = inputDB
        self.rttMS = rttMS
        self.negotiatedFormat = negotiatedFormat
        self.dtmfPlayed = dtmfPlayed
        self.dtmfTotal = dtmfTotal
        self.m17Available = m17Available
        self.m17Active = m17Active
    }

    /// The idle resting state: no call, meters at the floor.
    public static let idle = CallSnapshot(
        status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, inputDB: -60, rttMS: nil
    )
}

extension VoiceFormat {
    /// The status-card tag naming this codec (astar-ef35: every call shows its
    /// negotiated codec, not just wideband ones).
    public var badge: String {
        switch self {
        case .g711u: return "G.711 µ"
        case .g711a: return "G.711 A"
        case .slin: return "slin8"
        case .slin16: return "slin16"
        }
    }

    /// Whether this is the 16 kHz wideband codec — the tag's green tint
    /// (narrowband tags stay muted; wideband is worth celebrating).
    public var isWideband: Bool {
        self == .slin16
    }

    /// Nominal bitrate of this codec's audio, for the codec badge's tooltip
    /// (astar-bitrate: expose the bit rate alongside the negotiated codec).
    public var bitrateLabel: String {
        switch self {
        case .g711u, .g711a: return "64 kbit/s"
        case .slin: return "128 kbit/s"
        case .slin16: return "256 kbit/s"
        }
    }
}
