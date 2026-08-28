// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `CallSession` acting on a directory-resolved target (astar-refl-ship):
/// a name reaches the station as a host, port and module; an incomplete or
/// un-dialable one reaches nothing at all.
///
/// Every dial here lands on `FakeStation`. Nothing resolves a host and nothing
/// leaves the process.
final class CallSessionReflectorDialTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.refldial.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private var m17Index: ReflectorIndex {
        ReflectorIndex(entries: [
            DirectoryEntry(
                network: .m17, id: "M17-002", name: "M17-002", aliases: ["CUMBRIA"],
                dial: .m17(
                    host: "89.240.4.99", port: 17001, callsign: "M17-002", modules: ["A"])),
            DirectoryEntry(
                network: .m17, id: "M17-DEAD", name: "M17-DEAD", dial: nil),
        ])
    }

    private func session(index: ReflectorIndex) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.m17Callsign = "AJ7HR"
        session.reflectorIndex = index
        return (session, fake)
    }

    // MARK: - Dialling by name

    func testConnectsAResolvedNameByItsPublishedAddress() throws {
        let (session, fake) = session(index: m17Index)

        try session.connect(node: "M17-002 A", network: .m17)

        let call = try XCTUnwrap(fake.m17Connects.first)
        XCTAssertEqual(call.host, "89.240.4.99", "the host came from the directory, not the text")
        XCTAssertEqual(call.port, 17001)
        XCTAssertEqual(call.module, "A")
        XCTAssertEqual(call.callsign, "AJ7HR", "still the operator's callsign, not the room's")
        XCTAssertEqual(session.activeCallNetwork, .m17)
        XCTAssertEqual(session.dialedNode, "M17-002 A", "the label stays what was typed")
    }

    func testResolvesAnAliasAndFoldsTheModule() throws {
        let (session, fake) = session(index: m17Index)

        try session.connect(node: "cumbria/a", network: .m17)

        XCTAssertEqual(fake.m17Connects.first?.host, "89.240.4.99")
        XCTAssertEqual(fake.m17Connects.first?.module, "A")
    }

    // MARK: - Resolved but incomplete

    func testABareNameRefusesToDialAndNamesWhatItUnderstood() {
        let (session, fake) = session(index: m17Index)

        XCTAssertThrowsError(try session.connect(node: "M17-002", network: .m17)) {
            XCTAssertEqual($0 as? CallSession.ConnectError, .needsModule("M17-002"))
            XCTAssertEqual(
                ($0 as? LocalizedError)?.errorDescription,
                "M17-002 needs a module — try M17-002 A.")
        }
        XCTAssertTrue(fake.m17Connects.isEmpty, "no dial may reach the station")
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.dialedNode)
    }

    func testAnIncompleteNameIsNotDialableButAnAddressStillIs() {
        let (session, _) = session(index: m17Index)

        XCTAssertFalse(session.canDial("M17-002", network: .m17), "resolved, module missing")
        XCTAssertTrue(session.canDial("M17-002 A", network: .m17))
        XCTAssertTrue(session.canDial("M17-002/b", network: .m17))
        XCTAssertFalse(session.canDial("M17-002 AB", network: .m17), "AB is not a module")
        XCTAssertTrue(
            session.canDial("m17.example.net:17000/A", network: .m17),
            "an address never in the directory keeps working exactly as before")
        XCTAssertFalse(session.canDial("nonsense", network: .m17))
    }

    func testAListedButUndialableEntryIsRefused() {
        let (session, fake) = session(index: m17Index)

        XCTAssertFalse(session.canDial("M17-DEAD A", network: .m17))
        XCTAssertThrowsError(try session.connect(node: "M17-DEAD A", network: .m17)) {
            XCTAssertEqual($0 as? CallSession.ConnectError, .reflectorNotDialable("M17-DEAD"))
        }
        XCTAssertTrue(fake.m17Connects.isEmpty)
    }

    // MARK: - No directory loaded

    /// The degradation case: a first launch with no bundled snapshot and no
    /// sync. Address dialling is untouched, and a name is simply not a target
    /// — the same answer the app gave before the directory existed.
    func testWithNoDirectoryEverythingFallsThroughToTheAddressGrammar() throws {
        let (session, fake) = session(index: .empty)

        XCTAssertEqual(session.resolveReflector("M17-002 A", network: .m17), .notInDirectory)

        try session.connect(node: "m17.example.net:17000/A", network: .m17)
        XCTAssertEqual(fake.m17Connects.first?.host, "m17.example.net")

        // And `M17-002 A` reverts to what the M17 grammar always made of it:
        // a host named "M17-002". That dial will fail in DNS, which is the
        // pre-directory behaviour exactly — degrading means giving the old
        // answer, not a new error.
        try session.connect(node: "M17-002 A", network: .m17)
        XCTAssertEqual(fake.m17Connects.last?.host, "M17-002")

        XCTAssertThrowsError(try session.connect(node: "M17-002", network: .m17)) {
            XCTAssertEqual(
                $0 as? CallSession.ConnectError, .badM17Target,
                "with no directory a name is just unparseable text, as it always was")
        }
    }

    func testASessionThatWasNeverGivenADirectoryStartsEmpty() {
        let session = CallSession(station: FakeStation(), userDefaults: scratchDefaults())
        XCTAssertTrue(session.reflectorIndex.isEmpty)
        XCTAssertEqual(session.resolveReflector("XLX836", network: .dstar), .notInDirectory)
    }

    // MARK: - AllStar is untouched

    func testAllStarDiallingIgnoresTheDirectoryEntirely() {
        let (session, _) = session(index: m17Index)
        XCTAssertTrue(session.canDial("55553", network: .allstar))
        XCTAssertTrue(session.canDial("node.example.net:4569", network: .allstar))
        XCTAssertFalse(session.canDial("", network: .allstar))
        // AllStar is not a reflector network; the directory has no opinion and
        // is never consulted (`Network.allstar.reflectorNetwork` is nil).
        XCTAssertNil(Network.allstar.reflectorNetwork)
    }
}
