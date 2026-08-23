// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-6b83: editing a saved favorite's node number / address in place.
/// Renaming was already possible; the dial target itself was read-only, so a
/// node that changed number (or a repeater that moved address) could only be
/// deleted and re-added, losing its label and talk-timer override.
final class DirectoryEditNodeTests: XCTestCase {
    private let suite = "astar.tests.directory.editnode"
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

    private func session() -> CallSession {
        CallSession(station: NullStation(), directoryStore: store)
    }

    private func seed(id: String = "a", label: String = "WA7ABU", node: String = "543260") {
        store.upsert(
            NodeEntry(
                id: id, label: label, node: node, favorite: true,
                talkTimerEnabled: true, talkTimerSeconds: 300))
    }

    // MARK: - The happy path

    func testEditsNodeNumberInPlace() {
        seed()
        let s = session()
        XCTAssertTrue(s.directorySetNode(id: "a", to: "543261"))
        let entry = store.all().first { $0.id == "a" }
        XCTAssertEqual(entry?.node, "543261")
    }

    func testPreservesLabelFavoriteAndTalkTimer() {
        seed()
        let s = session()
        XCTAssertTrue(s.directorySetNode(id: "a", to: "543261"))
        let entry = store.all().first { $0.id == "a" }
        // The whole point: editing the number must not cost the curation.
        XCTAssertEqual(entry?.label, "WA7ABU")
        XCTAssertEqual(entry?.favorite, true)
        XCTAssertEqual(entry?.talkTimerEnabled, true)
        XCTAssertEqual(entry?.talkTimerSeconds, 300)
        XCTAssertEqual(entry?.id, "a")
    }

    func testAcceptsADirectAddress() {
        seed()
        let s = session()
        XCTAssertTrue(s.directorySetNode(id: "a", to: "node.example.org:4569"))
        XCTAssertEqual(store.all().first { $0.id == "a" }?.node, "node.example.org:4569")
    }

    func testTrimsSurroundingWhitespace() {
        seed()
        let s = session()
        XCTAssertTrue(s.directorySetNode(id: "a", to: "  543261  "))
        XCTAssertEqual(store.all().first { $0.id == "a" }?.node, "543261")
    }

    func testCommandDialCharactersAreAllowed() {
        seed()
        let s = session()
        // "*3" prefixes are legitimate AllStar command dials.
        XCTAssertTrue(s.directorySetNode(id: "a", to: "*3543261"))
        XCTAssertEqual(store.all().first { $0.id == "a" }?.node, "*3543261")
    }

    // MARK: - Rejections leave the entry untouched

    func testRejectsEmpty() {
        seed()
        let s = session()
        XCTAssertFalse(s.directorySetNode(id: "a", to: "   "))
        XCTAssertEqual(store.all().first { $0.id == "a" }?.node, "543260")
    }

    func testRejectsMalformedTarget() {
        seed()
        let s = session()
        // Same validator the dial field uses, so what you can save is exactly
        // what you could have dialed.
        for bad in ["*", "host..name", "host:0", "host:99999", "a:b:c"] {
            XCTAssertFalse(s.directorySetNode(id: "a", to: bad), "should reject \(bad)")
        }
        XCTAssertEqual(store.all().first { $0.id == "a" }?.node, "543260")
    }

    func testRejectsUnknownID() {
        seed()
        let s = session()
        XCTAssertFalse(s.directorySetNode(id: "nope", to: "543261"))
        XCTAssertEqual(store.all().count, 1)
    }

    func testRejectsCollisionWithAnotherEntry() {
        seed()
        store.upsert(NodeEntry(id: "b", label: "Other", node: "543261", favorite: true))
        let s = session()
        // Two directory rows pointing at one node would make `recordRecent`
        // (which upserts BY NODE) and `directoryEntry(forNode:)` ambiguous.
        XCTAssertFalse(s.directorySetNode(id: "a", to: "543261"))
        XCTAssertEqual(store.all().first { $0.id == "a" }?.node, "543260")
        XCTAssertEqual(store.all().count, 2)
    }

    func testNoOpWhenUnchangedIsStillSuccess() {
        seed()
        let s = session()
        // Committing an unedited field must not read as a collision with itself.
        XCTAssertTrue(s.directorySetNode(id: "a", to: "543260"))
        XCTAssertEqual(store.all().first { $0.id == "a" }?.node, "543260")
    }

    // MARK: - The name resolver follows the edit

    func testDisplayNameFollowsTheNewNumber() {
        seed()
        let s = session()
        XCTAssertTrue(s.directorySetNode(id: "a", to: "543261"))
        XCTAssertEqual(s.name(forNode: "543261"), "WA7ABU")
        XCTAssertNil(s.name(forNode: "543260"))
    }
}
