// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Combine
import Foundation

/// The cached reflector directory: search it, resolve a name against it, and
/// refresh it on the cadence the data itself publishes.
///
/// It does not connect and it does not pick a module: resolution hands back an
/// entry, or a resolution that says a module is still missing, and the caller
/// decides what to do with it. The dial grammar itself lives in
/// `ReflectorDialText`, and the lookup in the immutable `index` — this type
/// owns the file, the clock and the publishing, and nothing else.
@MainActor
public final class ReflectorDirectory: ObservableObject {

    /// Endpoints and identity. Split out so a test never has to name a host.
    public struct Configuration: Sendable {
        /// hamcall-db's primary endpoint: every reflector, every network,
        /// no token, no account.
        public static let defaultFeedURL = URL(
            string: "https://rcludwick.github.io/hamcall-db/api/v1/reflectors.json")!

        public var feedURL: URL
        /// astar's version, for the `User-Agent`. AstarCore has no version
        /// constant of its own — the app owns `CFBundleShortVersionString` —
        /// so it is passed in rather than guessed at.
        public var appVersion: String
        /// Floor on manual syncs. The button is always *permitted*; this stops
        /// an impatient user turning a volunteer-run CDN into a target.
        public var manualSyncInterval: TimeInterval

        public init(
            feedURL: URL = Configuration.defaultFeedURL,
            appVersion: String = Configuration.bundleShortVersion(),
            manualSyncInterval: TimeInterval = 3600
        ) {
            self.feedURL = feedURL
            self.appVersion = appVersion
            self.manualSyncInterval = manualSyncInterval
        }

        /// `User-Agent: astar/0.1.7beta (+https://github.com/rcludwick/astar)`
        public var userAgent: String {
            "astar/\(appVersion) (+\(AboutLinks.repository.absoluteString))"
        }

        public static func bundleShortVersion(_ bundle: Bundle = .main) -> String {
            bundle.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
                ?? "unknown"
        }
    }

    /// Why a sync did nothing.
    public enum SkipReason: Hashable, Sendable {
        /// The cache is still inside `client_refresh_days`. Automatic syncs
        /// only; the manual button never sees this.
        case notDue(nextDue: Date)
        /// A sync ran less than `manualSyncInterval` ago.
        case debounced(retryAfter: Date)
    }

    /// What `sync()` did.
    public enum SyncOutcome: Hashable, Sendable {
        /// New data arrived and replaced the cache.
        case updated(count: Int, generated: String?)
        /// The server confirmed the cache is current (304). Freshness moves
        /// forward; the data does not change.
        case notModified
        case skipped(SkipReason)
    }

    /// What triggered a sync. The two obey different rules, so the caller has
    /// to say which it is.
    public enum SyncTrigger: Hashable, Sendable {
        /// Launch, or any other unattended refresh. Never runs more often
        /// than the feed's own `client_refresh_days`.
        case automatic
        /// The Sync Now button. Always permitted, hourly-debounced.
        case manual
    }

    /// The loaded feed — rows plus the licence text the UI has to show.
    @Published public private(set) var feed: ReflectorFeed
    /// Where the loaded feed came from, so Settings can say "bundled, never
    /// synced" rather than showing a blank date.
    @Published public private(set) var origin: Origin
    /// The name lookup over the loaded feed, republished on every load and
    /// every sync that changes the data.
    ///
    /// Handed to the dial path rather than the directory itself: this is an
    /// immutable `Sendable` value, and dialling happens off the main thread
    /// (see `ReflectorIndex`).
    @Published public private(set) var index: ReflectorIndex
    /// Why the last sync attempt failed, or `nil` if it did not.
    ///
    /// Published because the launch-time automatic sync has no caller to
    /// throw at: it is started and forgotten, on purpose, so a slow or dead
    /// endpoint never delays the UI. A failure with nowhere to surface is a
    /// failure nobody can debug — this is where Settings finds it, and it is
    /// the reason the automatic sync waited for the Settings section to exist.
    @Published public private(set) var lastSyncError: String?

    public enum Origin: Hashable, Sendable {
        /// Nothing loaded — no cache, no bundled snapshot.
        case none
        /// The snapshot shipped inside the app.
        case bundled
        /// The cache written by a sync.
        case cache
    }

    private let storage: ReflectorDirectoryStoring
    private let fetcher: ReflectorFeedFetching
    private let configuration: Configuration
    private let now: () -> Date

    private var metadata: ReflectorSyncMetadata

    /// Lowercased search haystacks, one per entry, built once per load. 3,000
    /// rows × six fields is not worth re-lowercasing on every keystroke.
    private var haystacks: [String] = []

    public init(
        storage: ReflectorDirectoryStoring,
        fetcher: ReflectorFeedFetching = URLSessionReflectorFeedFetcher(),
        configuration: Configuration = Configuration(),
        now: @escaping () -> Date = Date.init
    ) {
        self.storage = storage
        self.fetcher = fetcher
        self.configuration = configuration
        self.now = now
        self.metadata = storage.syncMetadata()
        self.feed = .empty
        self.origin = .none
        self.index = .empty
        load()
    }

    // MARK: - Loading

    /// Cache first, bundled snapshot second.
    ///
    /// A cache that will not parse is deleted and the bundled copy takes over.
    /// The alternative — an empty directory — is indistinguishable from a
    /// broken feature, and only one of those is something a user can recover
    /// from. Deleting the bad file matters as much as the fallback: keeping it
    /// would mean re-parsing garbage on every launch forever.
    private func load() {
        if let data = storage.cachedFeed() {
            if let parsed = try? ReflectorFeed.decode(data), !parsed.entries.isEmpty {
                adopt(parsed, origin: .cache)
                return
            }
            storage.removeCachedFeed()
        }
        if let data = storage.bundledSnapshot(), let parsed = try? ReflectorFeed.decode(data) {
            adopt(parsed, origin: .bundled)
            return
        }
        adopt(.empty, origin: .none)
    }

    private func adopt(_ feed: ReflectorFeed, origin: Origin) {
        self.feed = feed
        self.origin = origin
        haystacks = feed.entries.map(Self.haystack(for:))
        index = ReflectorIndex(entries: feed.entries)
    }

    private static func haystack(for entry: DirectoryEntry) -> String {
        var parts = [entry.id, entry.name]
        parts.append(contentsOf: entry.aliases)
        if let description = entry.description { parts.append(description) }
        if let country = entry.country { parts.append(country) }
        if let sponsor = entry.sponsor { parts.append(sponsor) }
        return parts.joined(separator: "\u{1F}").lowercased()
    }

    // MARK: - Reading

    /// Every entry, in publisher order.
    public var entries: [DirectoryEntry] { feed.entries }

    /// The attribution CC BY requires be shown wherever this data appears.
    public var attribution: String? { feed.attribution }

    /// Case-insensitive substring search over `id`, `name`, `aliases`,
    /// `description`, `country` and `sponsor`.
    ///
    /// Local, always. The whole set is a few hundred KB on disk; asking a
    /// server at dial time would trade a keystroke's latency for a network
    /// round-trip and a dependency on being online.
    ///
    /// An empty query returns everything matching `network` — the browse case,
    /// not a degenerate one.
    /// Entries that cannot be dialled are **included**. "astar can see this
    /// but cannot call it" is information; hiding it makes the app look wrong
    /// instead of honest.
    public func search(_ query: String, network: ReflectorNetwork? = nil) -> [DirectoryEntry] {
        let needle = query.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        var results: [DirectoryEntry] = []
        for (index, entry) in feed.entries.enumerated() {
            if let network, entry.network != network { continue }
            if needle.isEmpty || haystacks[index].contains(needle) { results.append(entry) }
        }
        return results
    }

    /// Exact, case-insensitive match on `id` or any alias, within one network.
    ///
    /// Aliases are matched because they are the same box under another name —
    /// `XRF836` and `XLX836` are one reflector, and an operator who knows it
    /// by the older name should not be told it does not exist.
    ///
    /// **Returns the entry and stops there.** It does not choose a D-Star
    /// module. On D-Star the module *is* the room, so a guessed one does not
    /// fail visibly — it succeeds, and puts an operator into a conversation
    /// they did not mean to join, keyed up under their own callsign. There is
    /// no data behind such a guess: `modules` is empty for every D-Star row
    /// because the registry does not publish it. Choosing the module is the
    /// caller's problem, and the UI's job is to require one before Connect.
    public func resolve(_ name: String, network: ReflectorNetwork) -> DirectoryEntry? {
        index.entry(named: name, network: network)
    }

    /// Interpret dial-field text — `XLX836`, `XLX836 A`, `XLX836/B` — against
    /// the loaded feed, ahead of any address parsing. Delegates to `index`,
    /// which is also what the dial path itself holds; this overload exists so
    /// a caller already on the main actor need not reach for the index by
    /// hand. See `ReflectorIndex.resolveDial`.
    public func resolveDial(_ raw: String, network: ReflectorNetwork) -> ReflectorDialResolution {
        index.resolveDial(raw, network: network)
    }

    // MARK: - Sync

    /// When astar last got a definitive answer — a 200 or a 304.
    public var lastFetched: Date? { metadata.lastFetched }

    /// When the next automatic sync becomes permissible: `lastFetched` plus
    /// the feed's own `client_refresh_days`. `nil` means never fetched, so a
    /// sync is due now.
    ///
    /// This is where the published cadence reaches the decision — read off the
    /// loaded feed on every call, never captured into a constant, so a change
    /// upstream takes effect the sync after it lands.
    public var nextAutomaticSync: Date? {
        metadata.lastFetched.map {
            $0.addingTimeInterval(TimeInterval(feed.clientRefreshDays) * 86_400)
        }
    }

    /// Fetch the feed if policy allows, and adopt it if it changed.
    ///
    /// A failure throws and leaves the loaded feed alone: the last good copy
    /// is better than no copy, and this is Settings' quiet status line, never
    /// a modal.
    @discardableResult
    public func sync(trigger: SyncTrigger = .manual) async throws -> SyncOutcome {
        let start = now()

        if trigger == .automatic, let due = nextAutomaticSync, start < due {
            return .skipped(.notDue(nextDue: due))
        }
        if let attempt = metadata.lastAttempt {
            let retry = attempt.addingTimeInterval(configuration.manualSyncInterval)
            if start < retry { return .skipped(.debounced(retryAfter: retry)) }
        }

        // Stamped before the request, so an endpoint that hangs or fails is
        // debounced exactly like one that succeeds.
        metadata.lastAttempt = start
        try? storage.writeSyncMetadata(metadata)

        do {
            let response = try await fetcher.fetch(
                ReflectorFeedRequest(
                    url: configuration.feedURL,
                    etag: metadata.etag,
                    lastModified: metadata.lastModified,
                    userAgent: configuration.userAgent))

            switch response {
            case .notModified:
                metadata.lastFetched = now()
                try? storage.writeSyncMetadata(metadata)
                lastSyncError = nil
                return .notModified

            case .payload(let data, let etag, let lastModified):
                // Parse before writing. A 200 carrying an error page must not
                // overwrite a directory that works.
                let parsed = try ReflectorFeed.decode(data)
                try storage.writeCachedFeed(data)
                metadata.lastFetched = now()
                metadata.etag = etag
                metadata.lastModified = lastModified
                try? storage.writeSyncMetadata(metadata)
                lastSyncError = nil
                adopt(parsed, origin: .cache)
                return .updated(count: parsed.entries.count, generated: parsed.generated)
            }
        } catch {
            // Recorded and rethrown, not swallowed: a caller that awaited this
            // still gets its error, and the fire-and-forget launch sync — which
            // has no caller — still leaves a trace in Settings.
            lastSyncError = error.localizedDescription
            throw error
        }
    }
}
