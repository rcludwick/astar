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

    /// The largest id the wire can carry: a `DMRD` frame's source address is
    /// **24 bits**, so 16,777,215 and no more.
    ///
    /// Its own constant, and deliberately not `DmrDial.maxTalkgroup` even
    /// though the number is the same: a source address and a talkgroup are two
    /// different fields that happen to be the same width today, and a shared
    /// constant would make one of them silently follow the other if either ever
    /// changed.
    ///
    /// Unlike NXDN's 16-bit refusal a registered id FITS: 7 digits is at most
    /// 9,999,999, and even the 9-digit multi-device convention (`id × 100 + nn`)
    /// fits for ids below 167,772. So the dial CONVERTS rather than refusing,
    /// and refuses only what is not a registration at all.
    public static let maximum: UInt32 = 0x00FF_FFFF

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

/// The operator's NXDN id — a *different* number from the DMR radio ID, and a
/// separate field for the same reason the DMR one is separate from a callsign.
///
/// NXDN addresses stations by a 16-bit number: `NXDNGateway/NXDNNetwork.cpp`
/// packs the source and destination as `unsigned short srcId, dstId`, and
/// `NXDNReflector`'s `Reflectors.h` holds its own id the same way. A
/// registered DMR ID is six or seven digits and does not fit in sixteen bits,
/// so it cannot stand in here — truncating one would put somebody else's
/// number on the air, which is why astar asks for this separately rather than
/// deriving it.
///
/// Range `1...65519`: `0` addresses nobody, and `65520` and up are reserved
/// by the standard for special destinations, so neither is an id a station
/// may transmit as.
public enum NxdnID {
    /// The smallest usable id. `0` is not an address.
    public static let minimum: UInt16 = 1
    /// The largest id a station may use — everything above is reserved.
    public static let maximum: UInt16 = 65519

    /// The longest string the field will hold. Deliberately LONGER than the
    /// five digits of `maximum`: a DMR ID pasted here has to survive intact
    /// so it can be refused, and truncating `3153591` to `31535` would turn
    /// somebody else's registration into a number astar would happily
    /// transmit as. Same cap as `RadioID.maxDigits`, so nothing a DMR field
    /// accepts is silently reshaped by this one.
    public static let maxDigits = RadioID.maxDigits

    /// What to store for what the user typed: digits only.
    ///
    /// Filtering rather than refusing, exactly as `RadioID.sanitized` does —
    /// pasting a number out of an email that wrapped it in spaces should just
    /// work, and a stray letter should never reach the stored value. The
    /// RANGE is not enforced here: see `value(_:)`, which refuses.
    public static func sanitized(_ text: String) -> String {
        String(text.filter(\.isASCIIDigit).prefix(maxDigits))
    }

    /// The id `text` names, or `nil` when it is empty, not a number, or
    /// outside `1...65519`.
    ///
    /// Unlike `RadioID.isPlausible` this is a REFUSAL, not a caption: astar
    /// can dial NXDN, and a number the wire cannot carry is not something to
    /// warn about and then transmit anyway.
    public static func value(_ text: String) -> UInt16? {
        let digits = sanitized(text)
        guard !digits.isEmpty, let value = UInt16(digits),
            (minimum...maximum).contains(value)
        else { return nil }
        return value
    }
}

extension Character {
    /// `isNumber` is true for Unicode digits astar has no use for — Arabic-Indic
    /// forms, superscripts, Roman numerals. A radio ID is ASCII `0`–`9`.
    fileprivate var isASCIIDigit: Bool { self >= "0" && self <= "9" }
}
