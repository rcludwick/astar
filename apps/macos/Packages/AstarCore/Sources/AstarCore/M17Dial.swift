// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Parses the M17 dial field's text into a reflector target (iax-f2b8 Task 8).
///
/// Grammar: `host[:port]/module` or `host[:port] module` — the module letter
/// trails `host[:port]` behind a `/` OR a ` ` (mirrors
/// `Network.m17.admitsDialCharacter`, the only network that admits both).
/// Port defaults to 17000 (the M17 reflector default) when omitted; the
/// module is a single ASCII letter, case-folded to uppercase — mirroring the
/// vendored `Station.connectM17`'s own module validation.
public enum M17Dial {
    /// Classify the M17 dial field's raw text. Whitespace is trimmed at the
    /// ends first. Returns `nil` for anything that doesn't fit the grammar:
    /// no `/`/` ` separator, an empty host, more than one `:` before the
    /// separator, an unparseable/zero/out-of-range port, or a module that
    /// isn't exactly one ASCII letter.
    public static func parse(_ raw: String) -> (host: String, port: UInt16, module: Character)? {
        let text = raw.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return nil }

        // `host[:port]` never itself contains `/` or ` ` (host has no internal
        // whitespace, port is digits only), so the FIRST occurrence of either
        // is unambiguously the module separator — whichever grammar form was
        // used.
        guard let sepIndex = text.firstIndex(where: { $0 == "/" || $0 == " " }) else {
            return nil
        }
        let hostPort = text[..<sepIndex]
        let modulePart = text[text.index(after: sepIndex)...]
            .trimmingCharacters(in: .whitespaces)

        guard modulePart.count == 1, let module = modulePart.first,
            module.isASCII, module.isLetter
        else { return nil }

        // At most one `:` — the part before it is the host, the part after
        // (if present) is the port.
        let hostPortParts = hostPort.split(separator: ":", omittingEmptySubsequences: false)
        guard hostPortParts.count <= 2 else { return nil }
        let host = String(hostPortParts[0])
        guard !host.isEmpty, !host.contains(where: \.isWhitespace) else { return nil }

        let port: UInt16
        if hostPortParts.count == 2 {
            guard let parsed = UInt16(hostPortParts[1]), parsed > 0 else { return nil }
            port = parsed
        } else {
            port = 17000
        }

        return (host: host, port: port, module: Character(module.uppercased()))
    }
}
