// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

@testable import AstarCore

/// A scriptable stand-in for the reflector feed endpoint.
///
/// The seam exists so the directory's sync policy is provable without a
/// network. Nothing in this suite resolves a host, and nothing in it may:
/// astar's tests do not reach anything but the loopback, and this reaches
/// nothing at all.
final class FakeReflectorFeedFetcher: @unchecked Sendable, ReflectorFeedFetching {
    /// Queued responses, consumed in order; the last one repeats.
    var responses: [ReflectorFeedResponse] = []
    /// When set, the next fetch throws it instead of answering.
    var error: Error?
    private(set) var requests: [ReflectorFeedRequest] = []

    init(responses: [ReflectorFeedResponse] = []) {
        self.responses = responses
    }

    func fetch(_ request: ReflectorFeedRequest) async throws -> ReflectorFeedResponse {
        requests.append(request)
        if let error {
            self.error = nil
            throw error
        }
        guard !responses.isEmpty else { return .notModified }
        return responses.count == 1 ? responses[0] : responses.removeFirst()
    }
}

/// In-memory two-layer store, so cache/bundle precedence and the corrupt-cache
/// fallback are testable without touching the filesystem.
final class FakeReflectorDirectoryStorage: ReflectorDirectoryStoring {
    var bundled: Data?
    var cache: Data?
    var metadata = ReflectorSyncMetadata()
    private(set) var cacheRemovals = 0
    private(set) var cacheWrites = 0
    /// When set, `writeCachedFeed` throws — a full disk must not be silent.
    var writeError: Error?

    init(bundled: Data? = nil, cache: Data? = nil) {
        self.bundled = bundled
        self.cache = cache
    }

    func bundledSnapshot() -> Data? { bundled }
    func cachedFeed() -> Data? { cache }

    func writeCachedFeed(_ data: Data) throws {
        if let writeError { throw writeError }
        cacheWrites += 1
        cache = data
    }

    func removeCachedFeed() {
        cacheRemovals += 1
        cache = nil
    }

    func syncMetadata() -> ReflectorSyncMetadata { metadata }
    func writeSyncMetadata(_ metadata: ReflectorSyncMetadata) throws { self.metadata = metadata }
}

enum ReflectorFixtures {
    /// The real slice of `api/v1/reflectors.json` kept in `Fixtures/`.
    static func sliceData() throws -> Data {
        guard
            let url = Bundle.module.url(
                forResource: "reflectors-slice", withExtension: "json", subdirectory: "Fixtures")
        else {
            struct MissingFixture: Error {}
            throw MissingFixture()
        }
        return try Data(contentsOf: url)
    }

    /// A minimal hand-built feed, for tests that care about policy rather than
    /// about real reflectors.
    static func feedData(
        clientRefreshDays: Int = 7, generated: String = "2026-08-26", ids: [String] = ["XLX001"]
    ) -> Data {
        let rows = ids.map {
            """
            {"network": "dstar", "id": "\($0)", "name": "\($0)",
             "dial": {"kind": "dextra", "host": "127.0.0.1", "port": 30001,
                      "callsign": "XRF\($0.suffix(3))"}}
            """
        }
        return Data(
            """
            {"schema_version": 1, "generated": "\(generated)",
             "client_refresh_days": \(clientRefreshDays),
             "attribution": "Reflector data provided by DVRef — https://dvref.com/",
             "license": "CC BY 4.0",
             "reflectors": [\(rows.joined(separator: ","))]}
            """.utf8)
    }
}
