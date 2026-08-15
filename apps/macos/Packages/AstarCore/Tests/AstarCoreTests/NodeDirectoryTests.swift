// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class NodeDirectoryTests: XCTestCase {
    private let suite = "astar.tests.directory"
    private var defaults: UserDefaults!
    private var store: UserDefaultsNodeDirectoryStore!

    override func setUp() {
        super.setUp()
        defaults = UserDefaults(suiteName: suite)
        defaults.removePersistentDomain(forName: suite)
        store = UserDefaultsNodeDirectoryStore(defaults)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
        super.tearDown()
    }

    private func entry(
        _ id: String, label: String, node: String,
        favorite: Bool = false, lastUsed: Date? = nil
    ) -> NodeEntry {
        NodeEntry(id: id, label: label, node: node, favorite: favorite, lastUsed: lastUsed)
    }

    // MARK: - Store round-trips

    func testEmptyByDefault() {
        XCTAssertTrue(store.all().isEmpty)
        XCTAssertTrue(store.favorites.isEmpty)
        XCTAssertTrue(store.recents.isEmpty)
    }

    func testUpsertThenAllRoundTrips() {
        let e = entry("a", label: "AJ7HR", node: "77777", favorite: true)
        store.upsert(e)
        XCTAssertEqual(store.all(), [e])
    }

    func testUpsertByIDReplacesNotDuplicates() {
        store.upsert(entry("a", label: "AJ7HR", node: "77777"))
        store.upsert(entry("a", label: "Renamed", node: "77777"))
        XCTAssertEqual(store.all().count, 1)
        XCTAssertEqual(store.all().first?.label, "Renamed")
    }

    func testRemoveDeletesByID() {
        store.upsert(entry("a", label: "A", node: "1"))
        store.upsert(entry("b", label: "B", node: "2"))
        store.remove(id: "a")
        XCTAssertEqual(store.all().map(\.id), ["b"])
    }

    // MARK: - Derived lists

    func testFavoritesSortedByLabel() {
        store.upsert(entry("a", label: "Zulu", node: "1", favorite: true))
        store.upsert(entry("b", label: "Alpha", node: "2", favorite: true))
        store.upsert(entry("c", label: "Mike", node: "3", favorite: false))
        XCTAssertEqual(
            store.favorites.map(\.label), ["Alpha", "Zulu"],
            "favorites are favorite==true, sorted by label")
    }

    func testRecentsSortedByLastUsedDescending() {
        let old = Date(timeIntervalSince1970: 1000)
        let mid = Date(timeIntervalSince1970: 2000)
        let new = Date(timeIntervalSince1970: 3000)
        store.upsert(entry("a", label: "Old", node: "1", lastUsed: old))
        store.upsert(entry("b", label: "New", node: "2", lastUsed: new))
        store.upsert(entry("c", label: "Mid", node: "3", lastUsed: mid))
        store.upsert(entry("d", label: "Never", node: "4", lastUsed: nil))
        XCTAssertEqual(
            store.recents.map(\.label), ["New", "Mid", "Old"],
            "recents have lastUsed, newest first; nil excluded")
    }

    func testRecentsCappedAtTen() {
        for i in 0..<15 {
            store.upsert(
                entry(
                    "id\(i)", label: "L\(i)", node: "\(i)",
                    lastUsed: Date(timeIntervalSince1970: Double(i))))
        }
        XCTAssertEqual(store.recents.count, 10, "recents capped at 10")
        // Newest should be the highest-indexed (largest timestamp).
        XCTAssertEqual(store.recents.first?.node, "14")
    }

    // MARK: - recordRecent

    func testRecordRecentUpsertsByNode() {
        store.recordRecent(node: "77777", label: "AJ7HR")
        store.recordRecent(node: "77777", label: "AJ7HR")
        XCTAssertEqual(
            store.all().filter { $0.node == "77777" }.count, 1,
            "re-dialing the same node updates rather than duplicates")
    }

    func testRecordRecentUpdatesLastUsed() {
        store.recordRecent(node: "77777", label: "AJ7HR")
        let first = store.all().first?.lastUsed
        XCTAssertNotNil(first)
        // Re-record; lastUsed should move forward (or stay), but still be set.
        store.recordRecent(node: "77777", label: "AJ7HR")
        XCTAssertNotNil(store.all().first?.lastUsed)
    }

    func testRecordRecentPreservesFavoriteAndLabel() {
        // A favorited entry keeps its favorite flag and curated label when the
        // node is re-dialed (recents update lastUsed only).
        store.upsert(entry("fav", label: "My Repeater", node: "77777", favorite: true))
        store.recordRecent(node: "77777", label: "77777")
        let e = store.all().first { $0.node == "77777" }
        XCTAssertEqual(e?.favorite, true, "recording a recent must not clear favorite")
        XCTAssertEqual(e?.label, "My Repeater", "an existing curated label is preserved")
        XCTAssertNotNil(e?.lastUsed, "lastUsed is updated")
    }

    func testRecordRecentDefaultsToAllStarNetwork() {
        // The 2-arg convenience (astar-c2e5) is for callers with no network
        // in scope — pre-M17 behavior, unchanged.
        store.recordRecent(node: "77777", label: "AJ7HR")
        XCTAssertEqual(store.all().first { $0.node == "77777" }?.network, .allstar)
    }

    func testRecordRecentStampsTheProvidedNetwork() {
        // astar-c2e5: a recent recorded while on M17 must read back as M17,
        // not silently default to AllStar.
        store.recordRecent(node: "m17.example.net/A", label: "m17.example.net/A", network: .m17)
        let e = store.all().first { $0.node == "m17.example.net/A" }
        XCTAssertEqual(e?.network, .m17)
    }

    func testRecordRecentUpdatesNetworkOnRedial() {
        // Re-recording an existing node with a DIFFERENT network updates it —
        // "the current network", not whatever it was dialed on originally.
        store.recordRecent(node: "77777", label: "AJ7HR", network: .allstar)
        store.recordRecent(node: "77777", label: "AJ7HR", network: .m17)
        XCTAssertEqual(store.all().first { $0.node == "77777" }?.network, .m17)
    }

    func testRecordRecentNewNodeUsesProvidedLabel() {
        store.recordRecent(node: "12345", label: "W1AW")
        let e = store.all().first { $0.node == "12345" }
        XCTAssertEqual(e?.label, "W1AW")
        XCTAssertEqual(e?.favorite, false)
    }

    // MARK: - Codable back-compat

    func testLegacyEntryWithoutOptionalsDecodes() {
        // An entry persisted before note/lastUsed existed must still decode.
        let json = #"[{"id":"a","label":"AJ7HR","node":"77777","favorite":true}]"#
        defaults.set(Data(json.utf8), forKey: "directory.nodes")
        let loaded = store.all()
        XCTAssertEqual(loaded.count, 1)
        XCTAssertEqual(loaded.first?.label, "AJ7HR")
        XCTAssertNil(loaded.first?.lastUsed)
        XCTAssertNil(loaded.first?.note)
    }

    func testNodeEntryWithoutNetworkDecodesAsAllStar() throws {
        // JSON persisted before astar-9b3e has no "network" key.
        let json = #"{"id":"X","label":"AJ7HR","node":"77777","favorite":true}"#
        let entry = try JSONDecoder().decode(NodeEntry.self, from: Data(json.utf8))
        XCTAssertEqual(entry.network, .allstar)
    }

    func testNodeEntryWithUnknownNetworkDecodesAsAllStarNotThrow() throws {
        // A future build wrote "dmr" (astar-c2e5: "m17" graduated to a real
        // case, so it no longer exercises this path); this build doesn't
        // know that case yet (an app downgrade). Must fall back to .allstar
        // rather than throw — a throw here propagates up through
        // `UserDefaultsNodeDirectoryStore.all()`'s `try?` into an empty
        // array, permanently wiping every favorite/recent on the next upsert.
        let json = #"""
            {"id":"X","label":"AJ7HR","node":"77777","favorite":true,"network":"dmr"}
            """#
        let entry = try JSONDecoder().decode(NodeEntry.self, from: Data(json.utf8))
        XCTAssertEqual(entry.network, .allstar)
        XCTAssertEqual(entry.id, "X")
        XCTAssertEqual(entry.label, "AJ7HR")
        XCTAssertEqual(entry.node, "77777")
        XCTAssertEqual(entry.favorite, true)
    }

    func testNodeEntryNetworkRoundTrips() throws {
        var entry = NodeEntry(label: "Refl", node: "refl.example.org")  // match the real init
        entry.network = .hamlink
        let data = try JSONEncoder().encode(entry)
        let back = try JSONDecoder().decode(NodeEntry.self, from: data)
        XCTAssertEqual(back.network, .hamlink)
    }
}
