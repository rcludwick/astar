// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Search, resolution, and the two storage layers.
@MainActor
final class ReflectorDirectoryTests: XCTestCase {

    private func directory(
        cache: Data? = nil, bundled: Data? = nil
    ) throws -> (ReflectorDirectory, FakeReflectorDirectoryStorage) {
        let fallback = try ReflectorFixtures.sliceData()
        let storage = FakeReflectorDirectoryStorage(bundled: bundled ?? fallback, cache: cache)
        return (
            ReflectorDirectory(storage: storage, fetcher: FakeReflectorFeedFetcher()), storage
        )
    }

    // MARK: - Search

    /// The three ways a human arrives at the same reflector: the start of its
    /// name, the number they remember, and the alias it used to be called.
    func testSearchFindsOneEntryByPrefixNumberAndAlias() throws {
        let (dir, _) = try directory()
        for query in ["XLX8", "836", "XRF836", "xlx836"] {
            XCTAssertEqual(
                dir.search(query, network: .dstar).map(\.id), ["XLX836"],
                "\(query) should find XLX836")
        }
    }

    func testSearchMatchesSponsorCountryAndDescription() throws {
        let (dir, _) = try directory()
        XCTAssertEqual(dir.search("cumbriacq").map(\.id), ["M17-002"], "sponsor")
        XCTAssertEqual(dir.search("Brazil").map(\.id), ["XLX000"], "country")
        XCTAssertEqual(dir.search("AMRASE").map(\.id), ["XLX000"], "description")
    }

    func testSearchIsCaseInsensitive() throws {
        let (dir, _) = try directory()
        XCTAssertEqual(dir.search("CUMBRIACQ").map(\.id), dir.search("cumbriacq").map(\.id))
    }

    /// The network argument narrows; `nil` is the whole directory.
    func testNetworkFilterNarrowsSearch() throws {
        let (dir, _) = try directory()
        XCTAssertTrue(dir.search("XLX836", network: .m17).isEmpty)
        XCTAssertEqual(dir.search("XLX836", network: nil).map(\.id), ["XLX836"])
    }

    /// An empty query is the browse case, not a degenerate one.
    func testEmptyQueryBrowsesTheNetwork() throws {
        let (dir, _) = try directory()
        XCTAssertEqual(dir.search("").count, dir.entries.count)
        XCTAssertEqual(dir.search("   ", network: .urf).count, dir.search("", network: .urf).count)
    }

    /// Un-dialable rows stay in the results. "astar can see this but cannot
    /// call it" is information; hiding it makes the app look wrong rather than
    /// honest, and the UI is what renders the row disabled.
    func testUndialableEntriesAreStillFindable() throws {
        let feed = Data(
            """
            {"reflectors": [
              {"network": "dmr", "id": "TG91", "name": "Worldwide DMR",
               "dial": {"kind": "dmr", "host": "10.0.0.9", "port": 62031}},
              {"network": "ysf", "id": "00099", "name": "Listed only YSF"}
            ]}
            """.utf8)
        let (dir, _) = try directory(cache: feed)
        XCTAssertEqual(dir.search("worldwide").map(\.id), ["TG91"])
        XCTAssertEqual(dir.search("listed").map(\.id), ["00099"])
        XCTAssertTrue(dir.entries.allSatisfy { !$0.isDialable })
    }

    // MARK: - Resolve

    func testResolveMatchesIdAndAliasCaseInsensitively() throws {
        let (dir, _) = try directory()
        for name in ["XLX836", "xlx836", " XRF836 ", "xrf836"] {
            XCTAssertEqual(
                dir.resolve(name, network: .dstar)?.id, "XLX836", "\(name) should resolve")
        }
    }

    /// Exact match only — resolution is not search. `XLX8` is a search term,
    /// not a reflector, and silently resolving it to XLX836 would dial
    /// somewhere the operator did not name.
    func testResolveIsExactNotSubstring() throws {
        let (dir, _) = try directory()
        XCTAssertNil(dir.resolve("XLX8", network: .dstar))
        XCTAssertNil(dir.resolve("836", network: .dstar))
        XCTAssertNil(dir.resolve("", network: .dstar))
    }

    /// An id can repeat across networks, so the network is part of the match
    /// rather than a filter applied after it.
    func testResolveIsScopedToItsNetwork() throws {
        let (dir, _) = try directory()
        XCTAssertNil(dir.resolve("XLX836", network: .m17))
        XCTAssertEqual(dir.resolve("100", network: .nxdn)?.network, .nxdn)
        XCTAssertEqual(dir.resolve("100", network: .p25), nil, "no P25 row with that id here")
    }

    /// Resolution stops at the entry. It does not choose a D-Star module,
    /// because on D-Star the module *is* the room: a guess does not fail
    /// visibly, it succeeds and puts an operator in the wrong conversation
    /// under their own callsign. There is no data behind such a guess —
    /// `modules` is empty for every D-Star row — so the caller picks, and the
    /// UI keeps Connect disabled until it has.
    func testResolveDoesNotInventAModule() throws {
        let (dir, _) = try directory()
        let entry = try XCTUnwrap(dir.resolve("XLX836", network: .dstar))
        XCTAssertEqual(entry.dial?.modules, [])
        XCTAssertEqual(entry.dial?.endpoint?.host, "45.56.69.219")
    }

    /// Only M17 has an app-network counterpart today; the bridge says so
    /// rather than pretending AllStar is a reflector network.
    func testAppNetworkBridgesOnlyWhereAReflectorNetworkExists() {
        XCTAssertEqual(Network.m17.reflectorNetwork, .m17)
        XCTAssertNil(Network.allstar.reflectorNetwork)
        XCTAssertNil(Network.hamlink.reflectorNetwork)
    }

    // MARK: - Storage layers

    func testCacheIsPreferredOverTheBundledSnapshot() throws {
        let (dir, _) = try directory(cache: ReflectorFixtures.feedData(ids: ["XLX777"]))
        XCTAssertEqual(dir.origin, .cache)
        XCTAssertEqual(dir.entries.map(\.id), ["XLX777"])
    }

    func testBundledSnapshotServesWhenThereIsNoCache() throws {
        let (dir, _) = try directory(cache: nil)
        XCTAssertEqual(dir.origin, .bundled)
        XCTAssertFalse(dir.entries.isEmpty)
    }

    /// A corrupt cache falls back to the bundled copy and is deleted.
    ///
    /// The fallback stops the picker going empty, and an empty picker is
    /// indistinguishable from a broken feature — only one of which a user can
    /// do anything about. Deleting the bad file matters as much: keeping it
    /// would mean re-parsing garbage on every launch, forever.
    func testCorruptCacheFallsBackToBundledAndIsDiscarded() throws {
        for corrupt in [Data("not json at all".utf8), Data(#"{"schema_version": 1}"#.utf8), Data()]
        {
            let (dir, storage) = try directory(cache: corrupt)
            XCTAssertEqual(dir.origin, .bundled)
            XCTAssertFalse(dir.entries.isEmpty, "the directory must not go empty")
            XCTAssertEqual(storage.cacheRemovals, 1)
            XCTAssertNil(storage.cache)
        }
    }

    /// A cache that parses but holds nothing is treated as damage too: a
    /// zero-row directory is the same user-visible failure as an unparseable
    /// one, and the bundled copy is strictly better than it.
    func testAnEmptyCacheFallsBackToBundled() throws {
        let (dir, _) = try directory(cache: Data(#"{"reflectors": []}"#.utf8))
        XCTAssertEqual(dir.origin, .bundled)
        XCTAssertFalse(dir.entries.isEmpty)
    }

    /// With neither layer present the directory is empty but alive — search
    /// answers, resolution answers, nothing traps.
    func testNoCacheAndNoBundledSnapshotIsEmptyNotBroken() {
        let dir = ReflectorDirectory(
            storage: FakeReflectorDirectoryStorage(), fetcher: FakeReflectorFeedFetcher())
        XCTAssertEqual(dir.origin, .none)
        XCTAssertTrue(dir.entries.isEmpty)
        XCTAssertTrue(dir.search("XLX836").isEmpty)
        XCTAssertNil(dir.resolve("XLX836", network: .dstar))
    }

    /// CC BY makes attribution a condition of use, so it has to reach the UI
    /// from the data rather than from a string in a repository file.
    func testAttributionIsAvailableToTheUI() throws {
        let (dir, _) = try directory()
        XCTAssertTrue(try XCTUnwrap(dir.attribution).contains("DVRef"))
    }

    func testPerNetworkCountsComeFromTheRowsActuallyHeld() throws {
        let (dir, _) = try directory()
        XCTAssertEqual(dir.feed.counts[.dstar], 2)
        XCTAssertEqual(dir.feed.counts[.urf], 2)
        XCTAssertEqual(dir.feed.counts.values.reduce(0, +), dir.entries.count)
    }
}

/// The real, on-disk store.
final class FileReflectorDirectoryStorageTests: XCTestCase {
    private var root: URL!

    override func setUpWithError() throws {
        root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("astar-reflectors-\(UUID().uuidString)", isDirectory: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    func testCacheAndMetadataRoundTripThroughTheFilesystem() throws {
        let store = FileReflectorDirectoryStorage(directory: root, bundledSnapshotURL: nil)
        XCTAssertNil(store.cachedFeed())

        let payload = ReflectorFixtures.feedData(ids: ["XLX404"])
        try store.writeCachedFeed(payload)
        XCTAssertEqual(store.cachedFeed(), payload)

        let stamped = Date(timeIntervalSince1970: 1_780_000_000)
        try store.writeSyncMetadata(
            ReflectorSyncMetadata(
                lastFetched: stamped, lastAttempt: stamped, etag: "\"abc\"",
                lastModified: "Tue, 26 Aug 2026 00:00:00 GMT"))
        let read = store.syncMetadata()
        XCTAssertEqual(read.etag, "\"abc\"")
        XCTAssertEqual(read.lastModified, "Tue, 26 Aug 2026 00:00:00 GMT")
        XCTAssertEqual(read.lastFetched?.timeIntervalSince1970, stamped.timeIntervalSince1970)

        store.removeCachedFeed()
        XCTAssertNil(store.cachedFeed())
    }

    /// The cache file holds the upstream format verbatim, so it is the same
    /// shape as the bundled snapshot — swappable by hand when debugging, and
    /// readable by anything that reads `reflectors.json`.
    func testTheCacheFileIsThePublishedFormat() throws {
        let store = FileReflectorDirectoryStorage(directory: root, bundledSnapshotURL: nil)
        try store.writeCachedFeed(try ReflectorFixtures.sliceData())
        let reread = try ReflectorFeed.decode(try Data(contentsOf: store.cacheURL))
        XCTAssertEqual(reread.license, "CC BY 4.0")
        XCTAssertEqual(store.cacheURL.lastPathComponent, "reflectors.json")
    }

    func testMissingMetadataReadsAsNeverSynced() {
        let store = FileReflectorDirectoryStorage(directory: root, bundledSnapshotURL: nil)
        XCTAssertEqual(store.syncMetadata(), ReflectorSyncMetadata())
    }

    func testUnreadableMetadataReadsAsNeverSyncedRatherThanTrapping() throws {
        let store = FileReflectorDirectoryStorage(directory: root, bundledSnapshotURL: nil)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try Data("{{{".utf8).write(to: store.metadataURL)
        XCTAssertEqual(store.syncMetadata(), ReflectorSyncMetadata())
    }
}
