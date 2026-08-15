// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// What the smart dial field's text means (astar-427f): a node number to
/// resolve through the registrar, or a direct address to dial as-is. The one
/// field replaced the Advanced-options manual-address disclosure — `parse`
/// classifies the raw text and `nil` keeps the Connect button disabled.
public enum DialTarget: Equatable {
    /// An AllStar node string — digits plus optional `*`/`#` command-dial
    /// characters (e.g. "55553", "*355553"). Dialed via registrar lookup.
    case node(String)
    /// A direct dial address — "host" or "host:port" (IP or hostname). The
    /// port defaults to 4569 downstream when omitted.
    case address(String)

    /// Classify the dial field's raw text. Whitespace is trimmed first; the
    /// associated value is always the trimmed string. Returns nil for empty
    /// or malformed input (bare `*`, empty port, invalid host chars, …).
    public static func parse(_ raw: String) -> DialTarget? {
        let text = raw.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return nil }

        // Node path: only digits and the `*`/`#` command-dial characters, with
        // at least one digit ("*" alone is a prefix, not a dial).
        if text.allSatisfy({ $0.isASCII && ($0.isNumber || $0 == "*" || $0 == "#") }) {
            return text.contains(where: \.isNumber) ? .node(text) : nil
        }

        // Address path: "host" or "host:port". A single colon splits the port;
        // more than one is malformed (no bracketed-IPv6 form yet).
        let parts = text.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count <= 2 else { return nil }
        if parts.count == 2 {
            guard isValidHost(parts[0]), isValidPort(parts[1]) else { return nil }
        } else {
            guard isValidHost(parts[0]) else { return nil }
        }
        return .address(text)
    }

    /// Hostname/IPv4 shape: ASCII alphanumerics, dots, and hyphens; starts and
    /// ends alphanumeric; no empty labels (".." → nil). Deliberately loose —
    /// the resolver downstream is the real authority.
    private static func isValidHost(_ host: Substring) -> Bool {
        guard let first = host.first, let last = host.last else { return false }
        guard first.isASCIIAlphanumeric, last.isASCIIAlphanumeric else { return false }
        guard host.allSatisfy({ $0.isASCIIAlphanumeric || $0 == "." || $0 == "-" }) else {
            return false
        }
        return !host.contains("..")
    }

    /// Port shape: all digits, in 1...65535.
    private static func isValidPort(_ port: Substring) -> Bool {
        guard !port.isEmpty, port.allSatisfy({ $0.isASCII && $0.isNumber }) else { return false }
        guard let value = Int(port) else { return false }
        return (1...65535).contains(value)
    }
}

extension Character {
    fileprivate var isASCIIAlphanumeric: Bool {
        isASCII && (isLetter || isNumber)
    }
}
