// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class NameResolverTests: XCTestCase {
    private let suite = "astar.tests.nameresolver"
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

    // MARK: - DirectoryNameSource

    func testDirectorySourceReturnsSavedLabel() {
        store.upsert(NodeEntry(label: "AJ7HR", node: "77777", favorite: true))
        let source = DirectoryNameSource(store)
        XCTAssertEqual(source.name(forNode: "77777"), "AJ7HR")
    }

    func testDirectorySourceUnknownNodeIsNil() {
        let source = DirectoryNameSource(store)
        XCTAssertNil(source.name(forNode: "00000"))
    }

    func testDirectorySourceIgnoresLabelEqualToNumber() {
        // An unnamed recent stores label == node; that's not a real name.
        store.recordRecent(node: "12345", label: "12345")
        let source = DirectoryNameSource(store)
        XCTAssertNil(source.name(forNode: "12345"))
    }

    func testDirectorySourcePrefersFavoriteOverRecent() {
        // Two entries for the same number: the favorite's label should win.
        store.upsert(NodeEntry(id: "r", label: "recent", node: "55"))
        store.upsert(NodeEntry(id: "f", label: "Repeater", node: "55", favorite: true))
        let source = DirectoryNameSource(store)
        XCTAssertEqual(source.name(forNode: "55"), "Repeater")
    }

    // MARK: - NameResolver

    func testResolverSavedNameWins() {
        store.upsert(NodeEntry(label: "W1AW", node: "2000", favorite: true))
        let resolver = NameResolver(sources: [DirectoryNameSource(store)])
        XCTAssertEqual(resolver.name(forNode: "2000"), "W1AW")
    }

    func testResolverUnknownReturnsNil() {
        let resolver = NameResolver(sources: [DirectoryNameSource(store)])
        XCTAssertNil(resolver.name(forNode: "9999"))
    }

    func testResolverDisplayNameFallsBackToNumber() {
        let resolver = NameResolver(sources: [DirectoryNameSource(store)])
        XCTAssertEqual(resolver.displayName(forNode: "9999"), "9999")
    }

    func testResolverDisplayNameUsesSavedName() {
        store.upsert(NodeEntry(label: "AJ7HR", node: "77777", favorite: true))
        let resolver = NameResolver(sources: [DirectoryNameSource(store)])
        XCTAssertEqual(resolver.displayName(forNode: "77777"), "AJ7HR")
    }

    func testResolverEmptyNodeIsNil() {
        let resolver = NameResolver(sources: [DirectoryNameSource(store)])
        XCTAssertNil(resolver.name(forNode: ""))
        XCTAssertNil(resolver.name(forNode: "  "))
    }

    func testResolverTrimsNodeBeforeLookup() {
        store.upsert(NodeEntry(label: "AJ7HR", node: "77777", favorite: true))
        let resolver = NameResolver(sources: [DirectoryNameSource(store)])
        XCTAssertEqual(resolver.name(forNode: " 77777 "), "AJ7HR")
    }

    // MARK: - Ordered sources / seam for astar-6c65

    func testResolverFirstSourceWins() {
        // Simulate the seam: a second (online-style) source is consulted only when
        // the directory has no name. The directory (first) wins when both know it.
        let online = StubNameSource(["77777": "REMOTE-CALLSIGN", "111": "K1ABC"])
        store.upsert(NodeEntry(label: "MyLabel", node: "77777", favorite: true))
        let resolver = NameResolver(sources: [DirectoryNameSource(store), online])
        // Directory wins for a node both know.
        XCTAssertEqual(resolver.name(forNode: "77777"), "MyLabel")
        // Falls through to the online source for a node only it knows.
        XCTAssertEqual(resolver.name(forNode: "111"), "K1ABC")
        // Still nil when neither knows it.
        XCTAssertNil(resolver.name(forNode: "222"))
    }

    private struct StubNameSource: NameSource {
        let table: [String: String]
        init(_ table: [String: String]) { self.table = table }
        func name(forNode node: String) -> String? { table[node] }
    }
}
