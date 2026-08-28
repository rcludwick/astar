// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A decoded `reflectors.json` — the rows plus the envelope around them.
///
/// The envelope is not decoration. `clientRefreshDays` is the sync cadence,
/// published as data so it can change without an astar release; `license` and
/// `attribution` are a CC BY condition the UI has to display, and a credit
/// that survives only in a repository file is one refactor away from being
/// lost. Decoding the rows and discarding the envelope would quietly turn a
/// licence obligation into a licence breach.
public struct ReflectorFeed: Hashable, Sendable, Codable {
    /// The cadence to fall back on when the feed does not state one. Only a
    /// fallback for a missing field — never a substitute for the published
    /// value, which always wins when present.
    public static let fallbackRefreshDays = 7

    /// Feed schema generation. Bumped upstream when the row shape changes
    /// incompatibly; recorded so a future build can branch on it.
    public let schemaVersion: Int
    /// The API generation the endpoint belongs to ("v1").
    public let apiVersion: String?
    /// Publication date as published, `YYYY-MM-DD`. Kept as the publisher's
    /// string: it is a date with no time and no zone, and turning it into a
    /// `Date` would invent both. Sync freshness is measured from when *we*
    /// fetched, which is a real instant.
    public let generated: String?
    /// How often a client should re-check, in days. Data, not a constant.
    public let clientRefreshDays: Int
    /// Row count as the publisher counted it. May exceed `entries.count` if a
    /// row was too malformed to keep — compare the two to notice.
    public let declaredCount: Int?
    public let license: String?
    public let licenseURL: String?
    /// The attribution text CC BY requires be shown wherever the data is.
    public let attribution: String?
    /// What hamcall-db changed relative to its upstreams — also a CC BY
    /// requirement for a derived work.
    public let modifications: String?
    public let entries: [DirectoryEntry]

    public init(
        schemaVersion: Int = 1, apiVersion: String? = nil, generated: String? = nil,
        clientRefreshDays: Int = ReflectorFeed.fallbackRefreshDays, declaredCount: Int? = nil,
        license: String? = nil, licenseURL: String? = nil, attribution: String? = nil,
        modifications: String? = nil, entries: [DirectoryEntry] = []
    ) {
        self.schemaVersion = schemaVersion
        self.apiVersion = apiVersion
        self.generated = generated
        self.clientRefreshDays = clientRefreshDays
        self.declaredCount = declaredCount
        self.license = license
        self.licenseURL = licenseURL
        self.attribution = attribution
        self.modifications = modifications
        self.entries = entries
    }

    /// An empty feed — what a directory holds before anything has loaded.
    public static let empty = ReflectorFeed()

    /// Row counts per network, for the settings summary line
    /// ("953 D-Star · 104 M17 · 1,432 YSF"). Computed from the rows actually
    /// held rather than read from the envelope's own `networks` map, so the
    /// number shown always matches the number searchable.
    public var counts: [ReflectorNetwork: Int] {
        entries.reduce(into: [:]) { $0[$1.network, default: 0] += 1 }
    }

    private enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case apiVersion = "api_version"
        case generated
        case clientRefreshDays = "client_refresh_days"
        case count
        case license
        case licenseURL = "license_url"
        case attribution
        case modifications
        case reflectors
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        schemaVersion = ((try? c.decodeIfPresent(Int.self, forKey: .schemaVersion)) ?? nil) ?? 1
        apiVersion = (try? c.decodeIfPresent(String.self, forKey: .apiVersion)) ?? nil
        generated = (try? c.decodeIfPresent(String.self, forKey: .generated)) ?? nil
        // A published cadence always wins; the constant is the absent-field
        // fallback only. A nonsense value (0, negative) is treated as absent
        // rather than obeyed — obeying it would mean polling on every launch.
        let published = ((try? c.decodeIfPresent(Int.self, forKey: .clientRefreshDays)) ?? nil)
            .flatMap { $0 > 0 ? $0 : nil }
        clientRefreshDays = published ?? ReflectorFeed.fallbackRefreshDays
        declaredCount = (try? c.decodeIfPresent(Int.self, forKey: .count)) ?? nil
        license = (try? c.decodeIfPresent(String.self, forKey: .license)) ?? nil
        licenseURL = (try? c.decodeIfPresent(String.self, forKey: .licenseURL)) ?? nil
        attribution = (try? c.decodeIfPresent(String.self, forKey: .attribution)) ?? nil
        modifications = (try? c.decodeIfPresent(String.self, forKey: .modifications)) ?? nil
        // `reflectors` itself is required — a payload with no rows array is
        // not a directory, and silently reading it as empty is precisely the
        // "broken looks like empty" failure the cache fallback exists to stop.
        entries = try c.decode([LenientRow].self, forKey: .reflectors).compactMap(\.entry)
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(schemaVersion, forKey: .schemaVersion)
        try c.encodeIfPresent(apiVersion, forKey: .apiVersion)
        try c.encodeIfPresent(generated, forKey: .generated)
        try c.encode(clientRefreshDays, forKey: .clientRefreshDays)
        try c.encode(declaredCount ?? entries.count, forKey: .count)
        try c.encodeIfPresent(license, forKey: .license)
        try c.encodeIfPresent(licenseURL, forKey: .licenseURL)
        try c.encodeIfPresent(attribution, forKey: .attribution)
        try c.encodeIfPresent(modifications, forKey: .modifications)
        try c.encode(entries, forKey: .reflectors)
    }

    /// One row, decoded so that failure is a value rather than a throw.
    ///
    /// One row missing its `id` must not cost the other 3,184. This is the
    /// only place where dropping a row is right — by here the row has no
    /// identity, so there is nothing left to list.
    private struct LenientRow: Decodable {
        let entry: DirectoryEntry?

        init(from decoder: Decoder) throws {
            entry = try? DirectoryEntry(from: decoder)
        }
    }
}

extension ReflectorFeed {
    /// Decode a feed from raw bytes.
    public static func decode(_ data: Data) throws -> ReflectorFeed {
        try JSONDecoder().decode(ReflectorFeed.self, from: data)
    }
}
