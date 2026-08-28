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
        ReflectorAddressDial.parse(raw, defaultPort: defaultPort)
    }

    /// The M17 reflector default, applied when the text omits a port.
    public static let defaultPort: UInt16 = 17000
}
