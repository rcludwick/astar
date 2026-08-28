// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// One reflector as hamcall-db publishes it: an envelope of things to show a
/// human, plus an optional `dial` saying how to reach it.
///
/// `dial == nil` means listed but not dialable, and that is a legitimate state
/// rather than a defect — the feed is not astar-specific, and a reflector
/// astar cannot call is still information. Nothing here invents a default for
/// a missing field; an absent port stays absent, an absent module stays absent.
public struct DirectoryEntry: Hashable, Sendable, Codable {
    /// Which network the publisher filed this row under. Unknown networks
    /// survive as `ReflectorNetwork.other`.
    public let network: ReflectorNetwork
    /// The publisher's identifier — "XLX836", "M17-002", or for the
    /// DVRef-sourced networks a bare number like "00006". Unique only within
    /// a network; see `key`.
    public let id: String
    /// Display name. Often equal to `id` for D-Star, a human phrase elsewhere.
    public let name: String
    /// Other identifiers that mean the same box — "XRF836" for XLX836.
    /// Resolution matches these, because they are genuinely the same machine.
    public let aliases: [String]
    /// Free text from the publisher. May contain HTML: several upstream rows
    /// carry markup verbatim, so a view must not assume plain text.
    public let description: String?
    public let country: String?
    public let sponsor: String?
    /// The reflector's web dashboard, as published. Kept as a string rather
    /// than a `URL` so a malformed one costs a dead link, not a dropped row.
    public let dashboard: String?
    /// Which upstream registry this row came from ("xlx", "dvref").
    public let source: String?
    /// How to connect, or `nil` for listed-but-not-dialable.
    public let dial: ReflectorDial?

    public init(
        network: ReflectorNetwork, id: String, name: String, aliases: [String] = [],
        description: String? = nil, country: String? = nil, sponsor: String? = nil,
        dashboard: String? = nil, source: String? = nil, dial: ReflectorDial? = nil
    ) {
        self.network = network
        self.id = id
        self.name = name
        self.aliases = aliases
        self.description = description
        self.country = country
        self.sponsor = sponsor
        self.dashboard = dashboard
        self.source = source
        self.dial = dial
    }

    /// A directory-wide unique identity, for list diffing.
    ///
    /// Deliberately not an `Identifiable` conformance on `id`: `id` is the
    /// publisher's field name and it collides across networks (NXDN "100" and
    /// P25 "100" both exist). A SwiftUI `List` keyed on a colliding id drops
    /// rows silently, which is exactly the failure this whole type is built to
    /// avoid.
    public var key: String { "\(network.rawValue):\(id)" }

    /// Whether astar could place this call if the user asked. False when the
    /// row has no dial at all, and false when its `kind` is one this build
    /// cannot drive.
    public var isDialable: Bool { dial?.isDialable ?? false }

    private enum CodingKeys: String, CodingKey {
        case network, id, name, aliases, description, country, sponsor, dashboard, source, dial
    }

    /// Lenient by design. Only `network` and `id` are structurally required —
    /// everything else is optional in the wire format or tolerable as absent,
    /// and a `dial` that will not decode leaves the row listed rather than
    /// taking it out of the picker.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        network = try c.decode(ReflectorNetwork.self, forKey: .network)
        id = try c.decode(String.self, forKey: .id)
        name = ((try? c.decodeIfPresent(String.self, forKey: .name)) ?? nil) ?? id
        aliases = ((try? c.decodeIfPresent([String].self, forKey: .aliases)) ?? nil) ?? []
        description = (try? c.decodeIfPresent(String.self, forKey: .description)) ?? nil
        country = (try? c.decodeIfPresent(String.self, forKey: .country)) ?? nil
        sponsor = (try? c.decodeIfPresent(String.self, forKey: .sponsor)) ?? nil
        dashboard = (try? c.decodeIfPresent(String.self, forKey: .dashboard)) ?? nil
        source = (try? c.decodeIfPresent(String.self, forKey: .source)) ?? nil
        dial = (try? c.decodeIfPresent(ReflectorDial.self, forKey: .dial)) ?? nil
    }
}

extension DirectoryEntry {
    /// `description` with markup taken out, for a list row.
    ///
    /// Several upstream rows carry HTML verbatim — `<br>`, anchors, the
    /// occasional `<font>` — because the registries that produced them feed
    /// web dashboards. SwiftUI's `Text` renders that as literal angle
    /// brackets, so the sponsor of one reflector reads as source code in the
    /// picker. Stripping is the honest option: astar is not going to render
    /// someone else's markup, and it is not going to show the tags either.
    ///
    /// `nil` when there was nothing, and also when the field held *only*
    /// markup — an empty caption line is worse than no caption line.
    public var plainDescription: String? { DirectoryEntry.stripMarkup(description) }

    /// Sponsor, same treatment — it comes from the same fields upstream and
    /// carries the same markup.
    public var plainSponsor: String? { DirectoryEntry.stripMarkup(sponsor) }

    /// Tags out, the handful of entities upstream actually emits decoded,
    /// runs of whitespace collapsed.
    ///
    /// Deliberately not `NSAttributedString(html:)`: that spins up WebKit, has
    /// to run on the main thread, and would be doing it once per visible row
    /// while someone types in a search field.
    static func stripMarkup(_ raw: String?) -> String? {
        guard let raw, !raw.isEmpty else { return nil }
        var text = ""
        // What has been swallowed since the last `<`. A stray angle bracket in
        // prose ("temp < 5C") is not a tag, and treating it as one would eat
        // the rest of the description — so an unterminated tag is put back.
        var pending: String?
        for character in raw {
            switch character {
            case "<":
                if let pending { text.append(pending) }
                pending = "<"
            case ">" where pending != nil:
                // A tag is an element boundary: `<br>` and `</b><b>` separate
                // words, so the tag leaves a space behind rather than joining
                // what stood on either side of it. Runs collapse below.
                pending = nil
                text.append(" ")
            default:
                if pending != nil { pending?.append(character) } else { text.append(character) }
            }
        }
        if let pending { text.append(pending) }
        // `&amp;` decodes LAST, or `&amp;lt;` would decode twice and come out
        // as a literal `<` the source never wrote.
        for (entity, replacement) in [
            ("&nbsp;", " "), ("&lt;", "<"), ("&gt;", ">"),
            ("&quot;", "\""), ("&#39;", "'"), ("&apos;", "'"), ("&amp;", "&"),
        ] {
            text = text.replacingOccurrences(of: entity, with: replacement)
        }
        let collapsed = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return collapsed.isEmpty ? nil : collapsed
    }
}
