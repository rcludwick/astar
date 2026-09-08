// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The address half of a reflector dial: `host[:port]/module`, or
/// `host[:port] module` with a space instead of the slash.
///
/// Shared by M17 and D-Star because it genuinely is one grammar — the
/// networks differ in protocol and in default port, not in how an operator
/// types a reflector's address. `M17Dial` forwards here; D-Star reaches it
/// through `DStarDial`.
///
/// This is the **second** thing asked about a dial string, never the first.
/// A reflector name is a well-formed hostname, so a parser that ran ahead of
/// the directory would happily accept `XLX836` and dial whatever DNS said it
/// was. `ReflectorIndex.resolveDial` gets first refusal; `notInDirectory` is
/// the only way in here.
public enum ReflectorAddressDial {
    /// Classify `host[:port]` plus a module letter. `nil` for anything that
    /// does not fit: no separator, an empty host, more than one `:`, an
    /// unparseable/zero port, or a module that is not exactly one ASCII
    /// letter.
    public static func parse(_ raw: String, defaultPort: UInt16) -> (
        host: String, port: UInt16, module: Character
    )? {
        let text = raw.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return nil }

        // `host[:port]` never itself contains `/` or ` ` (a host has no
        // internal whitespace, a port is digits only), so the FIRST occurrence
        // of either is unambiguously the module separator — whichever form was
        // typed.
        guard let sepIndex = text.firstIndex(where: { $0 == "/" || $0 == " " }) else {
            return nil
        }
        let hostPort = text[..<sepIndex]
        let modulePart = text[text.index(after: sepIndex)...]
            .trimmingCharacters(in: .whitespaces)

        guard modulePart.count == 1, let module = modulePart.first,
            module.isASCII, module.isLetter
        else { return nil }

        // At most one `:` — before it the host, after it the port.
        let hostPortParts = hostPort.split(separator: ":", omittingEmptySubsequences: false)
        guard hostPortParts.count <= 2 else { return nil }
        let host = String(hostPortParts[0])
        guard !host.isEmpty, !host.contains(where: \.isWhitespace) else { return nil }

        let port: UInt16
        if hostPortParts.count == 2 {
            guard let parsed = UInt16(hostPortParts[1]), parsed > 0 else { return nil }
            port = parsed
        } else {
            port = defaultPort
        }

        return (host: host, port: port, module: Character(module.uppercased()))
    }
}

/// System Fusion's address grammar: `host[:port]`, and no module.
public enum YSFDial {
    /// The port the plurality of YSFReflectors listen on — 837 of the ~1100
    /// the directory carries, against a long tail of 42001/42002/42003 and
    /// one-offs. So it is a sensible default and a poor assumption: the
    /// directory row always carries the real one, and this default only ever
    /// applies to an address an operator typed without a port.
    public static let defaultPort: UInt16 = 42000

    /// Classify `host[:port]`. No module: a plain YSFReflector is one room,
    /// and DG-ID rooms are not addressed this way.
    ///
    /// A module separator is a REJECTION, not something to ignore. `XLX836 A`
    /// is a D-Star dial that happens to look like a hostname, and silently
    /// dropping the ` A` would connect the operator to a YSF reflector named
    /// `XLX836` — or to whatever DNS decided that was.
    public static func parse(_ raw: String) -> (host: String, port: UInt16)? {
        let text = raw.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return nil }
        guard !text.contains("/"), !text.contains(" ") else { return nil }

        let parts = text.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count <= 2 else { return nil }
        let host = String(parts[0])
        guard !host.isEmpty, !host.contains(where: \.isWhitespace) else { return nil }

        if parts.count == 2 {
            guard let port = UInt16(parts[1]), port > 0 else { return nil }
            return (host: host, port: port)
        }
        return (host: host, port: defaultPort)
    }
}

/// NXDN's address grammar: `host[:port]`, optionally followed by the
/// talkgroup — `host[:port]/TG` or `host[:port] TG`.
///
/// The talkgroup is part of the target, not a preference. An NXDNReflector
/// relays exactly one talkgroup: its poll registers a client only when the
/// poll names the reflector's own id, and it drops every data frame whose
/// `dstId` is something else. So a bare address is a target with the room
/// missing, exactly as `XLX836` is for D-Star — see `CallSession.nxdnTarget`,
/// which refuses one rather than guessing a number.
///
/// The directory is the normal way in and carries the talkgroup already: the
/// feed's 297 NXDN rows are `dial: {kind: "nxdn", host, port}` with the row's
/// **id** as the talkgroup ("100"), no callsigns and no modules. That is why
/// `ReflectorDial.addressesModule` is false here and the module picker
/// correctly never appears.
public enum NXDNDial {
    /// The port the overwhelming majority of NXDNReflectors listen on. A
    /// sensible fallback for an address someone typed without one, and never
    /// a substitute for the directory row's own.
    public static let defaultPort: UInt16 = 41400

    /// Classify `host[:port]`, with an optional talkgroup after a `/` or a
    /// space. `nil` for anything that does not fit.
    ///
    /// A separator followed by something that is not a talkgroup number is a
    /// REJECTION, not something to ignore. `XLX836 A` is a D-Star dial that
    /// happens to look like a hostname, and silently dropping the ` A` would
    /// link the operator to whatever DNS made of `XLX836`.
    public static func parse(_ raw: String) -> (host: String, port: UInt16, talkgroup: UInt16?)? {
        let text = raw.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return nil }

        // `host[:port]` contains neither a `/` nor a space (a host has no
        // internal whitespace, a port is digits only), so the FIRST of either
        // is unambiguously where the talkgroup starts — whichever was typed.
        var addressPart = Substring(text)
        var talkgroup: UInt16?
        if let separator = text.firstIndex(where: { $0 == "/" || $0 == " " }) {
            addressPart = text[..<separator]
            let rest = text[text.index(after: separator)...]
                .trimmingCharacters(in: .whitespaces)
            guard !rest.isEmpty, rest.allSatisfy({ $0.isASCII && $0.isNumber }),
                let parsed = UInt16(rest), parsed > 0
            else { return nil }
            talkgroup = parsed
        }

        let parts = addressPart.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count <= 2 else { return nil }
        let host = String(parts[0])
        guard !host.isEmpty, !host.contains(where: \.isWhitespace) else { return nil }

        if parts.count == 2 {
            guard let port = UInt16(parts[1]), port > 0 else { return nil }
            return (host: host, port: port, talkgroup: talkgroup)
        }
        return (host: host, port: defaultPort, talkgroup: talkgroup)
    }
}

/// D-Star's address grammar: the shared one, on the DExtra port.
///
/// The design doc used to say there is no `DStarDial` type, and while D-Star
/// was reachable only through the directory that was true. `Network.dstar`
/// changes it: a picker segment means an operator can type a bare address into
/// the field, and refusing to parse one would be a worse answer than parsing
/// it.
public enum DStarDial {
    /// DExtra's port, the same constant the directory publishes on every
    /// D-Star row.
    public static let defaultPort: UInt16 = 30001

    public static func parse(_ raw: String) -> (host: String, port: UInt16, module: Character)? {
        ReflectorAddressDial.parse(raw, defaultPort: defaultPort)
    }
}

/// The DMR network families astar names — `astar_dmr::DmrNetwork`'s cases,
/// mirrored on this side of the FFI for grouping and for the consent gate.
///
/// Nine families against the directory's 111 `system` slugs: this is a
/// vocabulary for *organising* a picker and for asking one question
/// (`requiresConsent`), never a list of what may be dialled. A system that
/// matches nothing here is an independent network astar does not recognise,
/// and it is dialled exactly like the ones it does.
public enum DmrFamily: String, CaseIterable, Sendable {
    case tgif, freedmr, dmrplus, systemx, amcomm, vkdmr, freestar, adn, brandmeister

    /// The label the picker groups under.
    public var displayName: String {
        switch self {
        case .tgif: return "TGIF"
        case .freedmr: return "FreeDMR"
        case .dmrplus: return "DMR+"
        case .systemx: return "SystemX"
        case .amcomm: return "AmComm"
        case .vkdmr: return "VKDMR"
        case .freestar: return "FreeSTAR"
        case .adn: return "ADN"
        case .brandmeister: return "BrandMeister"
        }
    }

    /// Whether the operator must opt in before this family is offered at all.
    ///
    /// True for BrandMeister and nothing else. BrandMeister is a private
    /// network whose operators set their own terms and have permanently
    /// blocked accounts; astar is a third-party client and cannot tell an
    /// operator whether connecting this way is within those rules. So it is
    /// hidden until the operator ticks a box that says so in plain words —
    /// `docs/design/dmr-networks.md`, "What the gate looks like", and
    /// `dmr-brandmeister-position.md` for what BrandMeister's own material
    /// did and did not say on 2026-09-07.
    ///
    /// The independent networks ask nothing of the sort: they publish their
    /// masters, issue their own passwords, and expect clients.
    public var requiresConsent: Bool { self == .brandmeister }
}

/// A DMR target: which network, which master, which talkgroup, which slot.
/// Four things, because a DMR talkgroup number names nothing on its own.
///
/// TG 91 exists on FreeDMR, on DMR+, on BrandMeister and on TGIF, and it is a
/// different room on each — so "91" is not a target, it is a quarter of one.
/// The timeslot is the fourth quarter and is not cosmetic either: a master
/// relays a talkgroup on the slot it was subscribed on.
///
/// Nothing here holds the password. Every dialable row in the directory
/// publishes `requires: ["dmr_id", "password"]`; the ID is the operator's
/// registration and the password is a per-network credential that lives in
/// the Keychain and reaches the engine as a connect-time in-arg.
public struct DmrDial: Equatable, Sendable {
    /// The port 75 of the directory's 185 rows publish — the plurality, ahead
    /// of 55555 (24) and 62030 (23). A sensible default for an address typed
    /// without one, and never a substitute for the row's own.
    public static let defaultPort: UInt16 = 62031
    /// TS2, the hotspot convention: a hotspot's own traffic rides slot 2 and
    /// that is the slot a softclient is standing in for.
    public static let defaultTimeslot: UInt8 = 2
    /// The widest talkgroup the wire carries — `DMRD` addresses are 24-bit.
    public static let maxTalkgroup: UInt32 = 0x00FF_FFFF

    /// The directory's slug for the network this master belongs to.
    public let system: String
    public let host: String
    public let port: UInt16
    public let talkgroup: UInt32
    /// `1` or `2`. Nothing else is a slot.
    public let timeslot: UInt8

    public init(system: String, host: String, port: UInt16, talkgroup: UInt32, timeslot: UInt8) {
        self.system = system
        self.host = host
        self.port = port
        self.talkgroup = talkgroup
        self.timeslot = timeslot
    }

    /// Classify a typed DMR target.
    ///
    ///     tgif.network:62031/31313/2      host, port, talkgroup, slot
    ///     tgif.network/31313              port 62031, slot 2
    ///     tgif:tgif.network:62031/31313/2 the system typed in as well
    ///
    /// `system` supplies the network when the text does not name one — it is
    /// what the picker has selected. The text WINS when it names one, because
    /// what was typed is what was meant.
    ///
    /// The two-part `a:b` address is decided on whether `b` is digits: a port
    /// is a number and a hostname is not, so `host:port` and `system:host`
    /// are told apart without guessing at either.
    ///
    /// Returns nil rather than guessing any of the four. A bare address is a
    /// target with the room missing; a bare talkgroup is a room with no
    /// master; a system that is nowhere in the string and nowhere in the
    /// argument is a login astar cannot even attempt. A space anywhere is a
    /// rejection too — `XLX836 A` is a D-Star dial, and quietly dropping the
    /// ` A` would put an operator on whatever DNS made of `XLX836`.
    public static func parse(_ raw: String, system: String?) -> DmrDial? {
        let text = raw.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty, !text.contains(where: \.isWhitespace) else { return nil }

        let slashParts = text.split(separator: "/", omittingEmptySubsequences: false)
        // `address/talkgroup` or `address/talkgroup/timeslot`. Fewer is not a
        // target; more is not this grammar.
        guard slashParts.count == 2 || slashParts.count == 3 else { return nil }

        guard let talkgroup = number(slashParts[1], max: maxTalkgroup), talkgroup > 0 else {
            return nil
        }
        let timeslot: UInt8
        if slashParts.count == 3 {
            guard let slot = number(slashParts[2], max: 2), slot == 1 || slot == 2 else {
                return nil
            }
            timeslot = UInt8(slot)
        } else {
            timeslot = defaultTimeslot
        }

        let colonParts = slashParts[0].split(separator: ":", omittingEmptySubsequences: false)
        let namedSystem: String?
        let hostPart: Substring
        let portPart: Substring?
        switch colonParts.count {
        case 1:
            namedSystem = nil
            hostPart = colonParts[0]
            portPart = nil
        case 2:
            if colonParts[1].allSatisfy({ $0.isASCII && $0.isNumber }) {
                namedSystem = nil
                hostPart = colonParts[0]
                portPart = colonParts[1]
            } else {
                namedSystem = String(colonParts[0])
                hostPart = colonParts[1]
                portPart = nil
            }
        case 3:
            namedSystem = String(colonParts[0])
            hostPart = colonParts[1]
            portPart = colonParts[2]
        default:
            return nil
        }

        let host = String(hostPart)
        guard !host.isEmpty else { return nil }

        let port: UInt16
        if let portPart {
            guard let parsed = number(portPart, max: UInt32(UInt16.max)), parsed > 0 else {
                return nil
            }
            port = UInt16(parsed)
        } else {
            port = defaultPort
        }

        let resolvedSystem = (namedSystem ?? system)?
            .trimmingCharacters(in: .whitespaces)
        guard let resolvedSystem, !resolvedSystem.isEmpty else { return nil }

        return DmrDial(
            system: resolvedSystem, host: host, port: port, talkgroup: talkgroup,
            timeslot: timeslot)
    }

    /// The family `astar_dmr::DmrNetwork` would call `system`, for grouping
    /// and for the consent gate. `nil` for a system this build does not know.
    ///
    /// **The directory's `system` is not the engine's slug, and cannot be.**
    /// Checked against the live feed on 2026-09-07: 185 DMR rows carry 111
    /// distinct `system` values — `freedmr-network`, `dmrplus-ipsc2-uk`,
    /// `ipsc2-poland`, `hb_it_trani_conference`, `xlx696` — and not one of
    /// them equals a `DmrNetwork` slug. DVRef enumerates *servers*, one row
    /// per operator instance; `DmrNetwork` enumerates *families*. Both are
    /// kept, and this table is the documented bridge between them.
    ///
    /// The rules below are prefixes because the directory's slugs are
    /// `<family>-<place>` by convention (`freedmr-reunion`,
    /// `adn-systems-espana`), with DMR+'s IPSC2/IPSC3 server software
    /// standing in for its name (`ipsc2-poland` is DMR+).
    ///
    /// **`nil` means "independent, unrecognised", never "refuse".** Most rows
    /// answer `nil` — 111 systems against nine families — and every one of
    /// them is listed and dialable. The only thing a family is consulted for
    /// is `requiresConsent`, and a network astar does not recognise is not
    /// BrandMeister.
    public static func family(ofSystem system: String) -> DmrFamily? {
        let slug = system.trimmingCharacters(in: .whitespaces).lowercased()
        guard !slug.isEmpty else { return nil }
        // The slug either IS the family's name or is that name followed by a
        // separator. Never a bare `hasPrefix`: `adn` would then claim
        // `adnetwork`, and a wrong family on a picker is a network filed under
        // somebody else's terms.
        for (family, names) in familyNames
        where names.contains(where: {
            slug == $0 || slug.hasPrefix($0 + "-") || slug.hasPrefix($0 + "_")
        }) {
            return family
        }
        return nil
    }

    /// The names each family is spelled with in the directory. `ipsc2` and
    /// `ipsc3` are DMR+: the slug names the server software the network runs
    /// (`ipsc2-poland`), not the network, and DMR+ is what it is.
    private static let familyNames: [(DmrFamily, [String])] = [
        (.brandmeister, ["brandmeister"]),
        (.freedmr, ["freedmr"]),
        (.freestar, ["freestar"]),
        (.dmrplus, ["dmrplus", "dmr-plus", "ipsc2", "ipsc3"]),
        (.systemx, ["systemx", "system-x"]),
        (.amcomm, ["amcomm"]),
        (.vkdmr, ["vkdmr", "vk-dmr"]),
        (.tgif, ["tgif"]),
        (.adn, ["adn"]),
    ]

    /// Digits-only, in range. `Substring` in, so the parser above never
    /// allocates a String for something it is about to reject.
    private static func number(_ text: Substring, max: UInt32) -> UInt32? {
        guard !text.isEmpty, text.allSatisfy({ $0.isASCII && $0.isNumber }),
            let value = UInt32(text), value <= max
        else { return nil }
        return value
    }
}
