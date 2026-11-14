// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Combine
import Foundation

/// The cached AllStarLink node directory: a snapshot shipped in the app, a
/// cache that sync writes, and the search index over whichever is loaded.
///
/// The same two-layer store and the same sync policy as `ReflectorDirectory`
/// (see `FeedSync`), pointed at `allstar.json`. For the app, storage is
/// `FileReflectorDirectoryStorage(feedFileName: "allstar.json",
/// metadataFileName: "allstar-sync.json")`, so the two directories never share
/// a cache file or a set of validators.
@MainActor
public final class AllStarDirectory: ObservableObject {

    /// Endpoints and identity. Split out so a test never has to name a host.
    public struct Configuration: Sendable {
        /// hamcall-db's AllStar endpoint: every node, no token, no account.
        public static let defaultFeedURL = URL(
            string: "https://rcludwick.github.io/hamcall-db/api/v1/allstar.json")!

        public var feedURL: URL
        /// astar's version, for the `User-Agent`.
        public var appVersion: String
        /// Floor on manual syncs, as for the reflector directory.
        public var manualSyncInterval: TimeInterval

        public init(
            feedURL: URL = Configuration.defaultFeedURL,
            appVersion: String = ReflectorDirectory.Configuration.bundleShortVersion(),
            manualSyncInterval: TimeInterval = 3600
        ) {
            self.feedURL = feedURL
            self.appVersion = appVersion
            self.manualSyncInterval = manualSyncInterval
        }

        /// The same `User-Agent` the reflector sync sends.
        public var userAgent: String {
            ReflectorDirectory.Configuration(appVersion: appVersion).userAgent
        }
    }

    public typealias SyncOutcome = ReflectorDirectory.SyncOutcome
    public typealias SyncTrigger = ReflectorDirectory.SyncTrigger

    /// The loaded feed — nodes plus the attribution the UI has to show.
    @Published public private(set) var feed: AllStarFeed
    /// The search index over `feed`, rebuilt on every load and every sync
    /// that changes the data.
    @Published public private(set) var index: AllStarIndex
    /// Where the loaded feed came from.
    @Published public private(set) var origin: ReflectorDirectory.Origin
    /// Why the last sync attempt failed, or `nil` if it did not. Published
    /// for the same reason as the reflector directory's: the launch sync has
    /// no caller to throw at.
    @Published public private(set) var lastSyncError: String?

    private let storage: ReflectorDirectoryStoring
    private let feedSync: FeedSync

    public init(
        storage: ReflectorDirectoryStoring,
        fetcher: ReflectorFeedFetching = URLSessionReflectorFeedFetcher(),
        configuration: Configuration = Configuration(),
        now: @escaping () -> Date = Date.init
    ) {
        self.storage = storage
        self.feedSync = FeedSync(
            storage: storage, fetcher: fetcher, feedURL: configuration.feedURL,
            userAgent: configuration.userAgent,
            manualSyncInterval: configuration.manualSyncInterval, now: now)
        self.feed = .empty
        self.index = .empty
        self.origin = .none
        load()
    }

    /// Cache first, bundled snapshot second. A cache that will not parse is
    /// deleted so the next launch does not re-parse it, and the bundle serves.
    private func load() {
        if let data = storage.cachedFeed() {
            if let parsed = try? AllStarFeed.decode(data), !parsed.nodes.isEmpty {
                adopt(parsed, origin: .cache)
                return
            }
            storage.removeCachedFeed()
        }
        if let data = storage.bundledSnapshot(), let parsed = try? AllStarFeed.decode(data) {
            adopt(parsed, origin: .bundled)
            return
        }
        adopt(.empty, origin: .none)
    }

    private func adopt(_ feed: AllStarFeed, origin: ReflectorDirectory.Origin) {
        self.feed = feed
        self.origin = origin
        index = AllStarIndex(feed.nodes)
    }

    // MARK: - Sync

    /// When astar last got a definitive answer — a 200 or a 304.
    public var lastFetched: Date? { feedSync.metadata.lastFetched }

    /// When the next automatic sync becomes permissible, from the loaded
    /// feed's own `client_refresh_days`.
    public var nextAutomaticSync: Date? {
        feedSync.nextAutomaticSync(refreshDays: feed.clientRefreshDays)
    }

    /// Fetch the feed if policy allows, and adopt it if it changed. A failure
    /// is recorded in `lastSyncError`, rethrown, and leaves the loaded feed
    /// alone.
    @discardableResult
    public func sync(trigger: SyncTrigger = .manual) async throws -> SyncOutcome {
        do {
            let outcome = try await feedSync.run(
                trigger: trigger, refreshDays: feed.clientRefreshDays,
                decode: AllStarFeed.decode,
                adopt: { parsed in
                    self.adopt(parsed, origin: .cache)
                    return .updated(count: parsed.nodes.count, generated: parsed.generated)
                })
            if case .skipped = outcome { return outcome }
            lastSyncError = nil
            return outcome
        } catch {
            lastSyncError = error.localizedDescription
            throw error
        }
    }
}
