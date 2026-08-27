// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A conditional GET of the reflector feed, expressed as a protocol so the
/// directory's sync policy is testable without a network. Nothing in
/// `AstarCoreTests` ever reaches a real host.
public protocol ReflectorFeedFetching: Sendable {
    func fetch(_ request: ReflectorFeedRequest) async throws -> ReflectorFeedResponse
}

/// What to ask for, including the validators from the last successful fetch.
public struct ReflectorFeedRequest: Hashable, Sendable {
    public let url: URL
    /// `ETag` from the previous response, echoed as `If-None-Match`.
    public let etag: String?
    /// `Last-Modified` from the previous response, echoed as
    /// `If-Modified-Since`.
    public let lastModified: String?
    /// Identifies astar and its version. hamcall-db's own upstreams ask for
    /// this so a misbehaving client can be contacted instead of blocked;
    /// astar extends hamcall-db the same courtesy.
    public let userAgent: String

    public init(url: URL, etag: String? = nil, lastModified: String? = nil, userAgent: String) {
        self.url = url
        self.etag = etag
        self.lastModified = lastModified
        self.userAgent = userAgent
    }
}

/// The two outcomes worth distinguishing. A 304 is the expected common case:
/// the upstream build is byte-stable, so most weeks there is genuinely
/// nothing to send.
public enum ReflectorFeedResponse: Hashable, Sendable {
    case notModified
    case payload(data: Data, etag: String?, lastModified: String?)
}

/// Things the fetch can fail with that are worth naming.
public enum ReflectorFeedError: Error, Equatable {
    /// A status code that is neither 200 nor 304.
    case unexpectedStatus(Int)
    /// The response was not HTTP at all.
    case notHTTP
}

/// The production fetcher.
///
/// Conditional GET with `If-None-Match`/`If-Modified-Since`, and a reload
/// policy that ignores the URL cache — the validators live in astar's own
/// sync metadata, and a second, invisible cache in front of them would make
/// "when did we last actually check" unanswerable.
public struct URLSessionReflectorFeedFetcher: ReflectorFeedFetching {
    private let session: URLSession

    public init(session: URLSession = .shared) {
        self.session = session
    }

    public func fetch(_ request: ReflectorFeedRequest) async throws -> ReflectorFeedResponse {
        var req = URLRequest(url: request.url)
        req.cachePolicy = .reloadIgnoringLocalCacheData
        req.setValue(request.userAgent, forHTTPHeaderField: "User-Agent")
        req.setValue(request.etag, forHTTPHeaderField: "If-None-Match")
        req.setValue(request.lastModified, forHTTPHeaderField: "If-Modified-Since")

        let (data, response) = try await session.data(for: req)
        guard let http = response as? HTTPURLResponse else { throw ReflectorFeedError.notHTTP }
        switch http.statusCode {
        case 304:
            return .notModified
        case 200:
            return .payload(
                data: data,
                etag: http.value(forHTTPHeaderField: "ETag"),
                lastModified: http.value(forHTTPHeaderField: "Last-Modified"))
        default:
            throw ReflectorFeedError.unexpectedStatus(http.statusCode)
        }
    }
}
