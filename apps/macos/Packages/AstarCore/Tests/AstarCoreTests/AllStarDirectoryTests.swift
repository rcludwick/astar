// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Loading and syncing `allstar.json`. The sync policy itself is shared with
/// the reflector directory and pinned by `ReflectorDirectorySyncTests`; these
/// tests pin that the AllStar side is wired to it and to its own files.
@MainActor
final class AllStarDirectoryTests: XCTestCase {

    private var clock = Date(timeIntervalSince1970: 1_780_000_000)

    private static func feedData(nodes: [String] = ["2002"], generated: String = "2026-11-12")
        -> Data
    {
        let rows = nodes.map { #"{"node": "\#($0)", "callsign": "N0CALL"}"# }
        return Data(
            """
            {"api_version": "v1", "generated": "\(generated)", "client_refresh_days": 7,
             "attribution": "AllStarLink node data courtesy of AllStarLink, Inc.",
             "nodes": [\(rows.joined(separator: ","))]}
            """.utf8)
    }

    private func makeDirectory(
        storage: ReflectorDirectoryStoring,
        fetcher: FakeReflectorFeedFetcher = FakeReflectorFeedFetcher()
    ) -> AllStarDirectory {
        AllStarDirectory(
            storage: storage, fetcher: fetcher,
            configuration: .init(
                feedURL: URL(string: "https://example.invalid/allstar.json")!,
                appVersion: "0.1.7beta"),
            now: { self.clock })
    }

    func testDefaultFeedURLIsHamcallDB() {
        XCTAssertEqual(
            AllStarDirectory.Configuration.defaultFeedURL.absoluteString,
            "https://rcludwick.github.io/hamcall-db/api/v1/allstar.json")
    }

    // MARK: - Loading

    func testCacheIsPreferredOverTheBundledSnapshot() {
        let storage = FakeReflectorDirectoryStorage(
            bundled: Self.feedData(nodes: ["1000"]), cache: Self.feedData(nodes: ["2002", "2003"]))
        let dir = makeDirectory(storage: storage)
        XCTAssertEqual(dir.origin, .cache)
        XCTAssertEqual(dir.index.count, 2)
        XCTAssertNotNil(dir.index.node("2002"))
    }

    func testCorruptCacheIsDeletedAndTheBundleServes() {
        let storage = FakeReflectorDirectoryStorage(
            bundled: Self.feedData(nodes: ["1000"]), cache: Data("<html>".utf8))
        let dir = makeDirectory(storage: storage)
        XCTAssertEqual(dir.origin, .bundled)
        XCTAssertNotNil(dir.index.node("1000"))
        XCTAssertEqual(storage.cacheRemovals, 1)
        XCTAssertNil(storage.cache)
    }

    func testNoCacheAndNoBundleIsEmptyNotBroken() {
        let dir = makeDirectory(storage: FakeReflectorDirectoryStorage())
        XCTAssertEqual(dir.origin, .none)
        XCTAssertEqual(dir.index.count, 0)
        XCTAssertEqual(dir.feed, .empty)
    }

    // MARK: - Sync

    func testAPayloadThatDoesNotDecodeLeavesCacheAndIndexAlone() async {
        let cached = Self.feedData(nodes: ["2002"])
        let storage = FakeReflectorDirectoryStorage(cache: cached)
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(data: Data("<html>error</html>".utf8), etag: nil, lastModified: nil)
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        do {
            _ = try await dir.sync()
            XCTFail("an undecodable payload must throw")
        } catch {}

        XCTAssertEqual(storage.cache, cached)
        XCTAssertEqual(storage.cacheWrites, 0)
        XCTAssertEqual(dir.index.count, 1)
        XCTAssertNotNil(dir.index.node("2002"))
        XCTAssertNotNil(dir.lastSyncError)
    }

    func testNotModifiedKeepsTheIndex() async throws {
        let storage = FakeReflectorDirectoryStorage(cache: Self.feedData(nodes: ["2002", "2003"]))
        let fetcher = FakeReflectorFeedFetcher(responses: [.notModified])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        let outcome = try await dir.sync()
        XCTAssertEqual(outcome, .notModified)
        XCTAssertEqual(dir.index.count, 2)
        XCTAssertEqual(storage.metadata.lastFetched, clock)
        XCTAssertNil(dir.lastSyncError)
    }

    func testAGoodPayloadReplacesTheIndexAndWritesAllstarJSON() async throws {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("astar-allstar-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let storage = FileReflectorDirectoryStorage(
            directory: root, bundledSnapshotURL: nil,
            feedFileName: "allstar.json", metadataFileName: "allstar-sync.json")
        let payload = Self.feedData(nodes: ["2002", "2003", "2004"], generated: "2026-11-13")
        let fetcher = FakeReflectorFeedFetcher(responses: [
            .payload(data: payload, etag: "\"a1\"", lastModified: nil)
        ])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)
        XCTAssertEqual(dir.origin, .none)

        let outcome = try await dir.sync()
        XCTAssertEqual(outcome, .updated(count: 3, generated: "2026-11-13"))
        XCTAssertEqual(dir.index.count, 3)
        XCTAssertEqual(dir.origin, .cache)
        XCTAssertEqual(
            fetcher.requests.first?.url.absoluteString,
            "https://example.invalid/allstar.json")

        let files = try FileManager.default.contentsOfDirectory(atPath: root.path).sorted()
        XCTAssertEqual(files, ["allstar-sync.json", "allstar.json"])
        XCTAssertEqual(try Data(contentsOf: root.appendingPathComponent("allstar.json")), payload)
        XCTAssertEqual(storage.syncMetadata().etag, "\"a1\"")
    }

    func testManualSyncIsDebouncedLikeTheReflectorDirectory() async throws {
        let storage = FakeReflectorDirectoryStorage()
        let fetcher = FakeReflectorFeedFetcher(responses: [.notModified])
        let dir = makeDirectory(storage: storage, fetcher: fetcher)

        _ = try await dir.sync()
        clock = clock.addingTimeInterval(60)
        let second = try await dir.sync()
        guard case .skipped(.debounced) = second else {
            return XCTFail("expected a debounced skip, got \(second)")
        }
        XCTAssertEqual(fetcher.requests.count, 1)
    }

    // MARK: - Storage

    /// The bundled snapshot is looked up by `feedFileName`, so the AllStar
    /// store reads `allstar.json` from the bundle, never `reflectors.json`.
    func testFileStorageFindsTheBundledSnapshotByFeedFileName() throws {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("astar-allstar-bundle-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let snapshot = Self.feedData(nodes: ["2002"])
        try snapshot.write(to: root.appendingPathComponent("allstar.json"))
        try Data("{}".utf8).write(to: root.appendingPathComponent("reflectors.json"))
        let bundle = try XCTUnwrap(Bundle(url: root))

        let store = FileReflectorDirectoryStorage(
            directory: root.appendingPathComponent("cache", isDirectory: true), bundle: bundle,
            feedFileName: "allstar.json", metadataFileName: "allstar-sync.json")
        XCTAssertEqual(store.bundledSnapshot(), snapshot)
        XCTAssertEqual(store.cacheURL.lastPathComponent, "allstar.json")
        XCTAssertEqual(store.metadataURL.lastPathComponent, "allstar-sync.json")
    }
}
