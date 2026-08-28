// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Sync policy, driven entirely off a fake fetcher and an injected clock.
///
/// No test here opens a socket. The cadence being *data* is the thing most of
/// these pin: `client_refresh_days` is read off the loaded feed at decision
/// time, so a change upstream takes effect the sync after it lands, without an
/// astar release.
@MainActor
final class ReflectorDirectorySyncTests: XCTestCase {

    private var clock = Date(timeIntervalSince1970: 1_780_000_000)
    private func advance(days: Double = 0, hours: Double = 0) {
        clock = clock.addingTimeInterval(days * 86_400 + hours * 3_600)
    }

    private func makeDirectory(
        storage: FakeReflectorDirectoryStorage,
        fetcher: FakeReflectorFeedFetcher
    ) -> ReflectorDirectory {
        ReflectorDirectory(
            storage: storage, fetcher: fetcher,
            configuration: .init(
                feedURL: URL(string: "https://example.invalid/reflectors.json")!,
                appVersion: "0.1.7beta"),
            now: { self.clock })
    }

    /// `XCTAssertEqual`'s autoclosure cannot await, so outcomes go through a
    /// plain call instead.
    private func expect(
        _ actual: ReflectorDirectory.SyncOutcome, _ expected: ReflectorDirectory.SyncOutcome,
        _ message: String = "", file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertEqual(actual, expected, message, file: file, line: line)
    }

    // MARK: - The conditional GET

    /// The request carries the validators from the last success and a
    /// `User-Agent` that names astar. hamcall-db's own upstreams ask for that
    /// so a misbehaving client can be contacted rather than blocked; astar
    /// extends hamcall-db the same courtesy.
    func testFirstSyncSendsNoValidatorsButAlwaysIdentifiesItself() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(data: ReflectorFixtures.feedData(), etag: "\"v1\"", lastModified: "Mon")
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        let outcome = try await dir.sync()
        XCTAssertEqual(outcome, .updated(count: 1, generated: "2026-08-26"))

        let request = try XCTUnwrap(fetcher.requests.first)
        XCTAssertNil(request.etag)
        XCTAssertNil(request.lastModified)
        XCTAssertEqual(request.userAgent, "astar/0.1.7beta (+https://github.com/rcludwick/astar)")
        XCTAssertEqual(request.url.absoluteString, "https://example.invalid/reflectors.json")
    }

    func testTheSecondSyncEchoesTheStoredValidators() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(data: ReflectorFixtures.feedData(), etag: "\"v1\"", lastModified: "Mon"),
            .notModified,
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        _ = try await dir.sync()
        advance(hours: 2)
        expect(try await dir.sync(), .notModified)

        let second = try XCTUnwrap(fetcher.requests.last)
        XCTAssertEqual(second.etag, "\"v1\"")
        XCTAssertEqual(second.lastModified, "Mon")
    }

    /// A 304 is the expected common case — the upstream build is byte-stable,
    /// so most weeks there is genuinely nothing to send. It proves freshness,
    /// so it moves the clock forward without touching the data.
    func testNotModifiedRefreshesFreshnessAndLeavesTheDataAlone() async throws {
        let storage = FakeReflectorDirectoryStorage(
            cache: ReflectorFixtures.feedData(ids: ["XLX111"]))
        let dir = makeDirectory(storage: storage, fetcher: FakeReflectorFeedFetcher())

        advance(days: 30)
        expect(try await dir.sync(trigger: .automatic), .notModified)
        XCTAssertEqual(dir.entries.map(\.id), ["XLX111"])
        XCTAssertEqual(dir.lastFetched, clock)
        XCTAssertEqual(storage.cacheWrites, 0, "a 304 rewrites nothing")
    }

    // MARK: - The cadence is data, not a constant

    /// Automatic sync waits exactly `client_refresh_days` — read off the feed
    /// that is loaded, not off a compiled-in number.
    func testAutomaticSyncWaitsForThePublishedCadence() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(
                data: ReflectorFixtures.feedData(clientRefreshDays: 7), etag: nil,
                lastModified: nil),
            .notModified,
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)
        _ = try await dir.sync()
        let fetchedAt = clock

        advance(days: 6)
        expect(
            try await dir.sync(trigger: .automatic),
            .skipped(.notDue(nextDue: fetchedAt.addingTimeInterval(7 * 86_400))))
        XCTAssertEqual(fetcher.requests.count, 1, "no request goes out at all")

        advance(days: 1)
        expect(try await dir.sync(trigger: .automatic), .notModified)
    }

    /// The publisher changing the cadence changes the client's behaviour with
    /// no astar release. Here the feed says 30 days, so day 8 — due under the
    /// old 7-day number — is not due any more.
    func testANewPublishedCadenceTakesEffectImmediately() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(
                data: ReflectorFixtures.feedData(clientRefreshDays: 30), etag: nil,
                lastModified: nil),
            .notModified,
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)
        _ = try await dir.sync()
        let fetchedAt = clock

        XCTAssertEqual(dir.feed.clientRefreshDays, 30)
        XCTAssertEqual(dir.nextAutomaticSync, fetchedAt.addingTimeInterval(30 * 86_400))

        advance(days: 8)
        guard case .skipped(.notDue) = try await dir.sync(trigger: .automatic) else {
            return XCTFail("8 days in must not be due when the feed says 30")
        }
        advance(days: 23)
        expect(try await dir.sync(trigger: .automatic), .notModified)
    }

    /// A directory that has never fetched is due now — the cadence gates
    /// re-checks, not the first check.
    func testANeverSyncedDirectoryIsDueImmediately() async throws {
        let dir = makeDirectory(
            storage: FakeReflectorDirectoryStorage(), fetcher: FakeReflectorFeedFetcher())
        XCTAssertNil(dir.nextAutomaticSync)
        expect(try await dir.sync(trigger: .automatic), .notModified)
    }

    // MARK: - The manual button

    /// Always permitted, but floored at an hour so an impatient user cannot
    /// turn a volunteer-run CDN into a target.
    func testManualSyncIsDebouncedToHourly() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [.notModified])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        expect(try await dir.sync(trigger: .manual), .notModified)
        let attemptedAt = clock

        advance(hours: 0.5)
        expect(
            try await dir.sync(trigger: .manual),
            .skipped(.debounced(retryAfter: attemptedAt.addingTimeInterval(3600))))
        XCTAssertEqual(fetcher.requests.count, 1)

        advance(hours: 0.6)
        expect(try await dir.sync(trigger: .manual), .notModified)
        XCTAssertEqual(fetcher.requests.count, 2)
    }

    /// Manual overrides the weekly cadence — that is what the button is for,
    /// and it is precisely why the automatic schedule never has to be loosened.
    func testManualSyncIgnoresTheWeeklyCadence() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [.notModified])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        _ = try await dir.sync(trigger: .manual)
        advance(hours: 2)
        expect(try await dir.sync(trigger: .manual), .notModified, "well inside 7 days")
        XCTAssertEqual(fetcher.requests.count, 2)
    }

    // MARK: - Failure keeps the last good copy

    /// A failed sync throws and changes nothing. Reflector addresses move on a
    /// scale of weeks; a stale directory is worth far more than none, and this
    /// is Settings' quiet status line, never a modal.
    func testAFailedFetchLeavesTheLoadedDirectoryIntact() async throws {
        let storage = FakeReflectorDirectoryStorage(
            cache: ReflectorFixtures.feedData(ids: ["XLX222"]))
        let fetcher = FakeReflectorFeedFetcher()
        fetcher.error = ReflectorFeedError.unexpectedStatus(503)
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        advance(days: 30)
        do {
            _ = try await dir.sync()
            XCTFail("expected the fetch failure to propagate")
        } catch {
            XCTAssertEqual(error as? ReflectorFeedError, .unexpectedStatus(503))
        }
        XCTAssertEqual(dir.entries.map(\.id), ["XLX222"])
        XCTAssertNil(dir.lastFetched, "a failure is not proof of freshness")
    }

    /// A failing endpoint is debounced exactly like a succeeding one: the
    /// attempt is stamped before the request goes out, so an outage cannot be
    /// retried in a tight loop.
    func testAFailedAttemptStillDebouncesTheNextOne() async throws {
        let fetcher = FakeReflectorFeedFetcher(responses: [.notModified])
        fetcher.error = ReflectorFeedError.notHTTP
        let dir = makeDirectory(storage: FakeReflectorDirectoryStorage(), fetcher: fetcher)

        _ = try? await dir.sync()
        advance(hours: 0.25)
        guard case .skipped(.debounced) = try await dir.sync() else {
            return XCTFail("a failed attempt must still count as an attempt")
        }
        XCTAssertEqual(fetcher.requests.count, 1)
    }

    /// A 200 carrying an error page must not overwrite a directory that works.
    /// The payload is parsed before anything is written.
    func testAnUnparseableResponseNeitherReplacesNorWritesTheCache() async throws {
        let storage = FakeReflectorDirectoryStorage(
            cache: ReflectorFixtures.feedData(ids: ["XLX333"]))
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(data: Data("<html>502 Bad Gateway</html>".utf8), etag: nil, lastModified: nil)
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        advance(days: 30)
        _ = try? await dir.sync()
        XCTAssertEqual(dir.entries.map(\.id), ["XLX333"])
        XCTAssertEqual(storage.cacheWrites, 0)
        XCTAssertEqual(storage.cache, ReflectorFixtures.feedData(ids: ["XLX333"]))
    }

    // MARK: - A successful update

    func testASuccessfulSyncReplacesTheCacheAndTheLoadedFeed() async throws {
        let storage = FakeReflectorDirectoryStorage(
            bundled: ReflectorFixtures.feedData(ids: ["XLX000"]))
        let payload = ReflectorFixtures.feedData(
            generated: "2026-09-02", ids: ["XLX100", "XLX200"])
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(data: payload, etag: "\"w/2\"", lastModified: "Wed")
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)
        XCTAssertEqual(dir.origin, .bundled)

        expect(
            try await dir.sync(), .updated(count: 2, generated: "2026-09-02"))
        XCTAssertEqual(dir.origin, .cache)
        XCTAssertEqual(dir.entries.map(\.id), ["XLX100", "XLX200"])
        XCTAssertEqual(storage.cache, payload)
        XCTAssertEqual(storage.metadata.etag, "\"w/2\"")
        XCTAssertEqual(storage.metadata.lastFetched, clock)
        XCTAssertEqual(dir.resolve("XLX100", network: .dstar)?.id, "XLX100")
    }

    /// Sync metadata outlives the process, so a relaunch does not re-fetch a
    /// directory that is still fresh.
    func testFreshnessSurvivesARelaunch() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(data: ReflectorFixtures.feedData(), etag: "\"v1\"", lastModified: nil)
        ])
        _ = try await makeDirectory(storage: storage, fetcher: fetcher).sync()

        advance(days: 1)
        let relaunched = makeDirectory(storage: storage, fetcher: fetcher)
        XCTAssertEqual(relaunched.origin, .cache)
        guard case .skipped(.notDue) = try await relaunched.sync(trigger: .automatic) else {
            return XCTFail("a fresh cache must survive a relaunch")
        }
        XCTAssertEqual(fetcher.requests.count, 1)
    }

    /// The default `sync()` is the manual one — the design's signature, kept
    /// callable exactly as written.
    func testSyncDefaultsToTheManualTrigger() async throws {
        let storage = FakeReflectorDirectoryStorage(cache: ReflectorFixtures.feedData())
        storage.metadata = ReflectorSyncMetadata(lastFetched: clock, lastAttempt: nil)
        let fetcher = FakeReflectorFeedFetcher(responses: [.notModified])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        // Well inside the cadence, so an automatic sync would skip.
        guard case .skipped(.notDue) = try await dir.sync(trigger: .automatic) else {
            return XCTFail("automatic should be gated here")
        }
        expect(try await dir.sync(), .notModified, "the bare call is the manual one")
    }
}
