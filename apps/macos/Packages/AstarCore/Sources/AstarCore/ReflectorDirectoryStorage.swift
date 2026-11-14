// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// What the last sync learned, kept beside the cached feed.
///
/// Separate from the feed itself so the cache file stays a byte-for-byte
/// plausible copy of `reflectors.json` — the same shape as the bundled
/// snapshot, readable by anything that reads the upstream file, and swappable
/// for it by hand when debugging.
public struct ReflectorSyncMetadata: Hashable, Sendable, Codable {
    /// When astar last got a definitive answer from the server — a 200 *or* a
    /// 304. A 304 is proof of freshness, so it moves this forward; treating it
    /// otherwise would re-fetch a byte-stable file every launch.
    public var lastFetched: Date?
    /// When astar last *tried*, success or not. Drives the manual debounce, so
    /// a failing endpoint cannot be hammered either.
    public var lastAttempt: Date?
    public var etag: String?
    public var lastModified: String?

    public init(
        lastFetched: Date? = nil, lastAttempt: Date? = nil, etag: String? = nil,
        lastModified: String? = nil
    ) {
        self.lastFetched = lastFetched
        self.lastAttempt = lastAttempt
        self.etag = etag
        self.lastModified = lastModified
    }

    private enum CodingKeys: String, CodingKey {
        case lastFetched = "last_fetched"
        case lastAttempt = "last_attempt"
        case etag
        case lastModified = "last_modified"
    }
}

/// The two-layer store the design calls for: a snapshot shipped inside the
/// app, and a cache that sync writes.
public protocol ReflectorDirectoryStoring: AnyObject {
    /// The bundled snapshot's bytes, or `nil` if the build ships without one.
    func bundledSnapshot() -> Data?
    /// The cached feed's bytes, or `nil` if nothing has been cached.
    func cachedFeed() -> Data?
    /// Replace the cached feed.
    func writeCachedFeed(_ data: Data) throws
    /// Discard an unreadable cache so the next launch does not re-parse it.
    func removeCachedFeed()

    func syncMetadata() -> ReflectorSyncMetadata
    func writeSyncMetadata(_ metadata: ReflectorSyncMetadata) throws
}

/// The real store, for any feed that follows the bundle-plus-cache layout.
///
/// Bundled snapshot: `astar.app/Contents/Resources/<feedFileName>`, so a
/// first launch with no network still has a directory.
/// Cache: `~/Library/Application Support/astar/<feedFileName>`, plus
/// `<metadataFileName>` for the validators. The defaults are the reflector
/// directory's (`reflectors.json`, `reflectors-sync.json`); the AllStar
/// directory passes `allstar.json` and `allstar-sync.json`.
public final class FileReflectorDirectoryStorage: ReflectorDirectoryStoring {
    private let directory: URL
    private let bundledURL: URL?

    /// File name for both the bundled snapshot and the cache — they hold the
    /// same format, so they share a name.
    public let feedFileName: String
    public let metadataFileName: String

    public var cacheURL: URL { directory.appendingPathComponent(feedFileName) }
    public var metadataURL: URL { directory.appendingPathComponent(metadataFileName) }

    /// - Parameters:
    ///   - directory: where the cache lives. Defaults to
    ///     `~/Library/Application Support/astar`.
    ///   - bundle: the bundle holding the shipped snapshot. `nil` disables the
    ///     bundled layer, which is what a build that does not ship one has.
    ///   - feedFileName: the cache's file name, and the bundled snapshot's.
    ///   - metadataFileName: the sync metadata's file name.
    public init(
        directory: URL? = nil,
        bundle: Bundle? = .main,
        feedFileName: String = "reflectors.json",
        metadataFileName: String = "reflectors-sync.json"
    ) {
        self.directory =
            directory
            ?? FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("astar", isDirectory: true)
        let resource = feedFileName as NSString
        self.bundledURL = bundle?.url(
            forResource: resource.deletingPathExtension,
            withExtension: resource.pathExtension.isEmpty ? nil : resource.pathExtension)
        self.feedFileName = feedFileName
        self.metadataFileName = metadataFileName
    }

    /// Explicit-path initializer, for tests and for a build that keeps its
    /// snapshot somewhere other than the bundle root.
    public init(
        directory: URL,
        bundledSnapshotURL: URL?,
        feedFileName: String = "reflectors.json",
        metadataFileName: String = "reflectors-sync.json"
    ) {
        self.directory = directory
        self.bundledURL = bundledSnapshotURL
        self.feedFileName = feedFileName
        self.metadataFileName = metadataFileName
    }

    public func bundledSnapshot() -> Data? {
        bundledURL.flatMap { try? Data(contentsOf: $0) }
    }

    public func cachedFeed() -> Data? {
        try? Data(contentsOf: cacheURL)
    }

    public func writeCachedFeed(_ data: Data) throws {
        try ensureDirectory()
        try data.write(to: cacheURL, options: .atomic)
    }

    public func removeCachedFeed() {
        try? FileManager.default.removeItem(at: cacheURL)
    }

    public func syncMetadata() -> ReflectorSyncMetadata {
        guard let data = try? Data(contentsOf: metadataURL),
            let decoded = try? JSONDecoder().decode(ReflectorSyncMetadata.self, from: data)
        else { return ReflectorSyncMetadata() }
        return decoded
    }

    public func writeSyncMetadata(_ metadata: ReflectorSyncMetadata) throws {
        try ensureDirectory()
        try JSONEncoder().encode(metadata).write(to: metadataURL, options: .atomic)
    }

    private func ensureDirectory() throws {
        try FileManager.default.createDirectory(
            at: directory, withIntermediateDirectories: true)
    }
}
