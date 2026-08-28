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
    /// D-Star (DExtra) reflector dialing. The engine has had the whole path
    /// since iax-a9d4/iax-2f6b — `dstar_connect`/`dstar_disconnect`/
    /// `dstar_available`, RX and TX — and this case is what lets the app
    /// reach it.
    ///
    /// Availability is HARDWARE, not a build flag: D-Star voice is AMBE, and
    /// astar has no software vocoder. `dstarAvailable` is true only when the
    /// engine found a ThumbDV, so this segment appears when a dongle is
    /// attached and not otherwise. That is the honest gate — a picker entry
    /// that always failed to connect would be worse than no entry.
    case dstar

    /// The networks the engine can actually drive right now. AllStar is
    /// always available; `hamlink` stays unavailable until the engine gains
    /// reflector capability (iax-b3d7). `m17` and `dstar` are each available
    /// exactly when the caller's own capability flag (from `CallSession`,
    /// mirroring the engine's snapshot) says so — for `dstar` that flag
    /// means "a vocoder dongle is present", which is a fact about the desk,
    /// not about the build.
    public static func available(m17: Bool, dstar: Bool = false) -> [Network] {
        var networks: [Network] = [.allstar]
        if m17 { networks.append(.m17) }
        if dstar { networks.append(.dstar) }
        return networks
    }

    /// Map a persisted raw value to an AVAILABLE network. Unknown strings,
    /// nil, and known-but-unavailable networks all fall back to `.allstar`
    /// (always the default; nothing user-actionable in the mismatch). `m17`
    /// mirrors `available(m17:)`'s flag.
    public static func resolve(_ raw: String?, m17: Bool, dstar: Bool = false) -> Network {
        guard let raw, let network = Network(rawValue: raw),
            available(m17: m17, dstar: dstar).contains(network)
        else { return .allstar }
        return network
    }

    /// The picker segment / favorites tooltip title.
    public var displayName: String {
        switch self {
        case .allstar: return "AllStar"
        case .hamlink: return "Hamlink"
        case .m17: return "M17"
        case .dstar: return "D-Star"
        }
    }

    /// The short capsule tag (status card, favorites rows) — same visual
    /// family as the codec badge.
    public var badge: String {
        switch self {
        case .allstar: return "ASL"
        case .hamlink: return "SVX"
        case .m17: return "M17"
        case .dstar: return "DSTAR"
        }
    }

    /// SF Symbol for the picker segment.
    public var symbol: String {
        switch self {
        case .allstar: return "antenna.radiowaves.left.and.right"
        case .hamlink: return "dot.radiowaves.left.and.right"
        case .m17: return "waveform"
        case .dstar: return "waveform.circle"
        }
    }

    /// The dial field's placeholder for this network.
    public var dialPlaceholder: String {
        switch self {
        case .allstar: return "Node or IP address"
        case .hamlink: return "Reflector host / talkgroup"
        case .m17: return "Reflector host:port / module"
        // Names first, because names are what the directory holds: 944 D-Star
        // reflectors are reachable by `XLX836 A`, and the address form is the
        // fallback for the one nobody has listed.
        case .dstar: return "Reflector name or host / module"
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
        case .m17, .dstar:
            // `host[:port]/module` or `host[:port] module`
            // (`ReflectorAddressDial`) — unlike the node networks, the space
            // is part of the grammar (the alternate separator), not something
            // to drop. Both reflector networks share it because they share the
            // grammar; only the default port differs.
            return (c.isASCII && (c.isLetter || c.isNumber)) || ".:-/ ".contains(c)
        }
    }

    /// Whether the DTMF dialpad disclosure applies — an AllStar concern;
    /// reflector networks will bring their own sections later.
    public var showsDialpad: Bool { self == .allstar }
}
