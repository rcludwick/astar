// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The sync policy shared by every hamcall-db feed astar caches: when a sync
/// may run, what it asks for, and what it writes.
///
/// The directories own the decoded data and its publishing; this owns the
/// clock-and-network half. It holds the sync metadata itself, so two
/// directories never share validators and the metadata is never passed
/// `inout` across an `await`.
///
/// A class rather than a struct for that reason: the metadata changes during
/// a sync, and the directory holding it must see the change.
@MainActor
final class FeedSync {
    typealias SyncOutcome = ReflectorDirectory.SyncOutcome
    typealias SyncTrigger = ReflectorDirectory.SyncTrigger

    private let storage: ReflectorDirectoryStoring
    private let fetcher: ReflectorFeedFetching
    private let feedURL: URL
    private let userAgent: String
    private let manualSyncInterval: TimeInterval
    private let now: () -> Date

    private(set) var metadata: ReflectorSyncMetadata

    init(
        storage: ReflectorDirectoryStoring,
        fetcher: ReflectorFeedFetching,
        feedURL: URL,
        userAgent: String,
        manualSyncInterval: TimeInterval,
        now: @escaping () -> Date
    ) {
        self.storage = storage
        self.fetcher = fetcher
        self.feedURL = feedURL
        self.userAgent = userAgent
        self.manualSyncInterval = manualSyncInterval
        self.now = now
        self.metadata = storage.syncMetadata()
    }

    /// `lastFetched` plus the feed's own refresh cadence; `nil` means never
    /// fetched, so a sync is due now.
    func nextAutomaticSync(refreshDays: Int) -> Date? {
        metadata.lastFetched.map {
            $0.addingTimeInterval(TimeInterval(refreshDays) * 86_400)
        }
    }

    /// Fetch the feed if policy allows, and hand a changed one to `adopt`.
    ///
    /// - Parameters:
    ///   - refreshDays: the loaded feed's `client_refresh_days`, read by the
    ///     caller at call time so an upstream change applies on the next sync.
    ///   - decode: parses a payload. Runs before anything is written, so a
    ///     200 carrying an error page never overwrites a cache that works.
    ///   - adopt: takes the parsed feed once it is cached, and returns the
    ///     `.updated` outcome describing it.
    /// - Throws: whatever the fetch, the decode or the cache write threw.
    ///   Recording the error is the caller's job.
    func run<Feed>(
        trigger: SyncTrigger,
        refreshDays: Int,
        decode: (Data) throws -> Feed,
        adopt: (Feed) -> SyncOutcome
    ) async throws -> SyncOutcome {
        let start = now()

        if trigger == .automatic, let due = nextAutomaticSync(refreshDays: refreshDays),
            start < due
        {
            return .skipped(.notDue(nextDue: due))
        }
        if let attempt = metadata.lastAttempt {
            let retry = attempt.addingTimeInterval(manualSyncInterval)
            if start < retry { return .skipped(.debounced(retryAfter: retry)) }
        }

        // Stamped before the request, so an endpoint that hangs or fails is
        // debounced exactly like one that succeeds.
        metadata.lastAttempt = start
        try? storage.writeSyncMetadata(metadata)

        let response = try await fetcher.fetch(
            ReflectorFeedRequest(
                url: feedURL,
                etag: metadata.etag,
                lastModified: metadata.lastModified,
                userAgent: userAgent))

        switch response {
        case .notModified:
            metadata.lastFetched = now()
            try? storage.writeSyncMetadata(metadata)
            return .notModified

        case .payload(let data, let etag, let lastModified):
            let parsed = try decode(data)
            try storage.writeCachedFeed(data)
            metadata.lastFetched = now()
            metadata.etag = etag
            metadata.lastModified = lastModified
            try? storage.writeSyncMetadata(metadata)
            return adopt(parsed)
        }
    }
}
