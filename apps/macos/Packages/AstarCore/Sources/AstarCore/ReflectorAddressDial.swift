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
