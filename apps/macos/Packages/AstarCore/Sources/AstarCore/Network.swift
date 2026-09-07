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
    /// System Fusion (YSF) reflector dialing.
    ///
    /// Availability is HARDWARE, exactly as for ``dstar`` and from the same
    /// dongle: YSF voice is AMBE+2 and astar has no software vocoder, so this
    /// segment appears when a ThumbDV is attached and not otherwise.
    ///
    /// RECEIVE ONLY today. The engine decodes DN — the mode Yaesu radios
    /// actually transmit — and has no transmit path at all, so this network
    /// offers no PTT. That is deliberate: a transmit affordance that put
    /// nothing on the air would be a worse lie than an absent one.
    case ysf
    /// NXDN reflector linking (iax-b9c2).
    ///
    /// Availability is HARDWARE, exactly as for ``dstar`` and ``ysf`` and
    /// from the same dongle: NXDN voice is AMBE+2 and astar has no software
    /// vocoder, so this segment appears when a ThumbDV is attached and not
    /// otherwise.
    ///
    /// RECEIVE ONLY today. The engine decodes an NXDN reflector's audio and
    /// has no transmit path at all, so this network offers no PTT — see
    /// `CallSession.canTransmit`. That is deliberate: a transmit affordance
    /// that put nothing on the air would be a worse lie than an absent one.
    case nxdn

    /// The networks the engine can actually drive right now. AllStar is
    /// always available; `hamlink` stays unavailable until the engine gains
    /// reflector capability (iax-b3d7). `m17` and `dstar` are each available
    /// exactly when the caller's own capability flag (from `CallSession`,
    /// mirroring the engine's snapshot) says so — for `dstar` that flag
    /// means "a vocoder dongle is present", which is a fact about the desk,
    /// not about the build.
    public static func available(
        m17: Bool, dstar: Bool = false, ysf: Bool = false, nxdn: Bool = false
    ) -> [Network] {
        var networks: [Network] = [.allstar]
        if m17 { networks.append(.m17) }
        if dstar { networks.append(.dstar) }
        if ysf { networks.append(.ysf) }
        if nxdn { networks.append(.nxdn) }
        return networks
    }

    /// Map a persisted raw value to an AVAILABLE network. Unknown strings,
    /// nil, and known-but-unavailable networks all fall back to `.allstar`
    /// (always the default; nothing user-actionable in the mismatch). `m17`
    /// mirrors `available(m17:)`'s flag.
    public static func resolve(
        _ raw: String?, m17: Bool, dstar: Bool = false, ysf: Bool = false, nxdn: Bool = false
    ) -> Network {
        guard let raw, let network = Network(rawValue: raw),
            available(m17: m17, dstar: dstar, ysf: ysf, nxdn: nxdn).contains(network)
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
        case .ysf: return "Fusion"
        case .nxdn: return "NXDN"
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
        case .ysf: return "YSF"
        case .nxdn: return "NXDN"
        }
    }

    /// SF Symbol for the picker segment.
    public var symbol: String {
        switch self {
        case .allstar: return "antenna.radiowaves.left.and.right"
        case .hamlink: return "dot.radiowaves.left.and.right"
        case .m17: return "waveform"
        case .dstar: return "waveform.circle"
        case .ysf: return "waveform.badge.plus"
        case .nxdn: return "waveform.badge.magnifyingglass"
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
        // No module: a plain YSFReflector is one room. The port is part of
        // the address rather than a default, because YSF has no conventional
        // one and every directory row carries its own.
        case .ysf: return "Reflector name or host:port"
        // Talkgroups, not reflector names: every NXDN row the directory
        // carries is identified by its talkgroup number, and a typed address
        // has to name one too — the reflector at a given host serves exactly
        // one talkgroup and relays nothing else.
        case .nxdn: return "Talkgroup, or host:port/talkgroup"
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
        case .m17, .dstar, .ysf, .nxdn:
            // `host[:port]/module` or `host[:port] module`
            // (`ReflectorAddressDial`) — unlike the node networks, the space
            // is part of the grammar (the alternate separator), not something
            // to drop. Both reflector networks share it because they share the
            // grammar; only the default port differs.
            return (c.isASCII && (c.isLetter || c.isNumber)) || ".:-/ ".contains(c)
        }
    }

    /// Whether this network carries digital voice with a talker identity on
    /// the wire — the networks whose engine state can name whoever last keyed
    /// up, and so the ones the popover's "Last heard" line applies to.
    ///
    /// AllStar is `false` because IAX2 carries no callsign in the audio path:
    /// a node number is who you dialled, not who is speaking. Hamlink is
    /// `false` for now because the engine has no link at all. Every new
    /// digital network opts in HERE — NXDN, DMR — and inherits the last-heard
    /// line without the popover changing. NXDN's identity is a NUMBER rather
    /// than a callsign — an `NXDND` frame carries `srcId` and no callsign at
    /// all — and that is still who most recently keyed up, so it belongs on
    /// the same line.
    public var isDigitalVoice: Bool {
        switch self {
        case .allstar, .hamlink: return false
        case .m17, .dstar, .ysf, .nxdn: return true
        }
    }

    /// Whether the DTMF dialpad disclosure applies — an AllStar concern;
    /// reflector networks will bring their own sections later.
    public var showsDialpad: Bool { self == .allstar }
}
