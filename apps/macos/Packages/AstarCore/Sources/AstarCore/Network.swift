// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// The networks astar can dial on (astar-9b3e). One connection at a time —
/// this is "where the next dial goes", not a multi-link manager. AllStar is
/// the founding network; `hamlink` (SvxReflector, iax-b3d7) exists as a case
/// now so favorites/persistence are future-proof, but stays unavailable
/// until the engine gains reflector capability. Later families (M17, DMR,
/// D-Star) follow the same pattern: new case + engine capability.
public enum Network: String, CaseIterable, Codable, Sendable {
    case allstar
    case hamlink
    /// M17 reflector dialing (iax-f2b8 Task 8) — the vendored Station gained
    /// `connectM17`/`m17Disconnect`; this case lights it up in `available(m17:)`
    /// once `CallSession.m17Available` (the snapshot's engine-capability flag)
    /// says the running build can actually place the call.
    case m17

    /// The networks the engine can actually drive right now. AllStar is
    /// always available; `hamlink` stays unavailable until the engine gains
    /// reflector capability (iax-b3d7) — `m17` is available exactly when the
    /// caller's own `m17Available` flag (from `CallSession`, mirroring the
    /// engine's snapshot) says so.
    public static func available(m17: Bool) -> [Network] {
        var networks: [Network] = [.allstar]
        if m17 { networks.append(.m17) }
        return networks
    }

    /// Map a persisted raw value to an AVAILABLE network. Unknown strings,
    /// nil, and known-but-unavailable networks all fall back to `.allstar`
    /// (always the default; nothing user-actionable in the mismatch). `m17`
    /// mirrors `available(m17:)`'s flag.
    public static func resolve(_ raw: String?, m17: Bool) -> Network {
        guard let raw, let network = Network(rawValue: raw),
            available(m17: m17).contains(network)
        else { return .allstar }
        return network
    }

    /// The picker segment / favorites tooltip title.
    public var displayName: String {
        switch self {
        case .allstar: return "AllStar"
        case .hamlink: return "Hamlink"
        case .m17: return "M17"
        }
    }

    /// The short capsule tag (status card, favorites rows) — same visual
    /// family as the codec badge.
    public var badge: String {
        switch self {
        case .allstar: return "ASL"
        case .hamlink: return "SVX"
        case .m17: return "M17"
        }
    }

    /// SF Symbol for the picker segment.
    public var symbol: String {
        switch self {
        case .allstar: return "antenna.radiowaves.left.and.right"
        case .hamlink: return "dot.radiowaves.left.and.right"
        case .m17: return "waveform"
        }
    }

    /// The dial field's placeholder for this network.
    public var dialPlaceholder: String {
        switch self {
        case .allstar: return "Node or IP address"
        case .hamlink: return "Reflector host / talkgroup"
        case .m17: return "Reflector host:port / module"
        }
    }

    /// Whether the dial field admits `c` — the per-network input filter.
    /// AllStar keeps the smart-field rules verbatim (astar-427f): ASCII node
    /// digits, `* #` command dials, and hostname/IP characters — the
    /// `isASCII` gate matters (drops accented letters etc.), so keep it on
    /// the whole clause, not just the punctuation set.
    public func admitsDialCharacter(_ c: Character) -> Bool {
        switch self {
        case .allstar:
            return c.isASCII && (c.isLetter || c.isNumber || ".:-*#".contains(c))
        case .hamlink:
            return (c.isASCII && (c.isLetter || c.isNumber)) || ".:-/#".contains(c)
        case .m17:
            // `host[:port]/module` or `host[:port] module` (M17Dial.parse) —
            // unlike every other network, the space is part of the grammar
            // (the alternate separator), not something to drop.
            return (c.isASCII && (c.isLetter || c.isNumber)) || ".:-/ ".contains(c)
        }
    }

    /// Whether the DTMF dialpad disclosure applies — an AllStar concern;
    /// reflector networks will bring their own sections later.
    public var showsDialpad: Bool { self == .allstar }
}
