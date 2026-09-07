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
