// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The operator's numeric radio ID — the second half of astar's identity model.
///
/// A callsign is what M17, D-Star and YSF put on the air. DMR does not: it
/// addresses radios by a number registered at radioid.net against a verified
/// licence. `docs/design/dmr-networks.md`, `nxdn-network.md` and
/// `p25-network.md` all raise the same question and all three say to settle it
/// once — this is that answer. A radio ID is a **separate credential** with its
/// own registration story, so it gets its own field rather than being bolted
/// onto the callsign, which means something else.
///
/// The rules are deliberately loose. Nothing dials DMR yet, so this validates
/// only enough to keep a typo out of the field: digits, and not more of them
/// than any real ID has.
public enum RadioID {
    /// The longest ID astar will hold. A registered ID is 6 digits (repeaters,
    /// older allocations) or 7 (individuals — US IDs start with `3`), and the
    /// widely used multi-device convention appends a two-digit suffix as
    /// `id × 100 + nn`, which takes a 7-digit ID to 9.
    public static let maxDigits = 9

    /// The shortest string worth calling an ID. Below this it is a half-typed
    /// number, not a registration.
    public static let minPlausibleDigits = 6

    /// What to actually store for what the user typed: digits only, truncated
    /// to `maxDigits`.
    ///
    /// Filtering rather than refusing is what makes the field feel right to
    /// type in — pasting `3153591` out of an email that wrapped it in spaces
    /// should just work, and a stray letter should never be able to reach the
    /// stored value.
    public static func sanitized(_ text: String) -> String {
        String(text.filter(\.isASCIIDigit).prefix(maxDigits))
    }

    /// Whether `text` is long enough to be a real registration.
    ///
    /// Advisory only: it drives a caption, never a refusal. An ID astar cannot
    /// yet use is not worth blocking anyone over, and the registry — not this
    /// function — is the authority on which numbers exist.
    public static func isPlausible(_ text: String) -> Bool {
        let digits = sanitized(text)
        return digits.count >= minPlausibleDigits
    }
}

extension Character {
    /// `isNumber` is true for Unicode digits astar has no use for — Arabic-Indic
    /// forms, superscripts, Roman numerals. A radio ID is ASCII `0`–`9`.
    fileprivate var isASCIIDigit: Bool { self >= "0" && self <= "9" }
}
