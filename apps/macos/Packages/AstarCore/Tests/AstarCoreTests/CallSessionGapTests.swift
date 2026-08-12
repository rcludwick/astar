// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// Fills the remaining uncovered branches in `CallSession`: the thread-hopping
/// publishers (`setConnecting`, `setDialedNode`), the mic-profile pass-throughs,
/// the directory recents accessor, and the running-timer reconfigure path.
final class CallSessionGapTests: XCTestCase {
    func testSetConnectingOnMainThreadSetsSynchronously() {
        let session = CallSession(station: FakeStation())
        XCTAssertFalse(session.isConnecting)

        session.setConnecting(true)
        XCTAssertTrue(session.isConnecting, "on the main thread the flag flips synchronously")

        session.setConnecting(false)
        XCTAssertFalse(session.isConnecting)
    }

    func testSetConnectingOffMainThreadHopsToMain() {
        let session = CallSession(station: FakeStation())
        let done = expectation(description: "isConnecting set on main")

        DispatchQueue.global().async {
            session.setConnecting(true)  // exercises the non-main-thread branch
            DispatchQueue.main.async {
                XCTAssertTrue(session.isConnecting)
                done.fulfill()
            }
        }

        wait(for: [done], timeout: 2)
    }

    func testSetDialedNodeOnMainThreadSetsSynchronously() {
        let session = CallSession(station: FakeStation())

        session.setDialedNode("77777")
        XCTAssertEqual(session.dialedNode, "77777")

        session.setDialedNode(nil)
        XCTAssertNil(session.dialedNode)
    }

    func testSetDialedNodeOffMainThreadHopsToMain() {
        let session = CallSession(station: FakeStation())
        let done = expectation(description: "dialedNode set on main")

        DispatchQueue.global().async {
            session.setDialedNode("55553")  // non-main-thread branch
            DispatchQueue.main.async {
                XCTAssertEqual(session.dialedNode, "55553")
                done.fulfill()
            }
        }

        wait(for: [done], timeout: 2)
    }

    func testSetMicProfilePassesRawJSONToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.setMicProfile(#"{"notches":[]}"#)
        try session.setMicProfile(nil)

        XCTAssertEqual(fake.setMicProfileCalls, [#"{"notches":[]}"#, nil])
    }

    func testMicProfileLookupReturnsStoredProfile() {
        let store = InMemoryMicProfileStore()
        let profile = MicProfile(
            id: "p1", name: "Icom", deviceName: "UCI150", characterizationJSON: "{}")
        store.save(profile)
        let session = CallSession(station: FakeStation(), micProfileStore: store)

        XCTAssertEqual(session.micProfile(id: "p1"), profile)
    }

    func testMicProfileLookupNilForNilOrUnknownID() {
        let session = CallSession(
            station: FakeStation(), micProfileStore: InMemoryMicProfileStore())
        XCTAssertNil(session.micProfile(id: nil), "nil id resolves to no profile")
        XCTAssertNil(session.micProfile(id: "missing"), "unknown id resolves to no profile")
    }

    func testDirectoryRecentsReflectsStore() throws {
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(
            station: FakeStation(), hasCredentials: true, directoryStore: directory)
        try session.connect(node: "77777")
        session.station = {
            let fake = FakeStation()
            fake.snapshotToReturn = CallSnapshot(
                status: .answered, ptt: false, remotePTT: false,
                txDB: -40, rxDB: -50, rttMS: 30)
            return fake
        }()
        session.poll()

        XCTAssertEqual(
            session.directoryRecents().map(\.node), ["77777"],
            "an answered call surfaces as a recent")
    }

    func testReconfigureWhileRunningKeepsPolling() {
        let first = FakeStation()
        first.snapshotToReturn = CallSnapshot(
            status: .dialing, ptt: false, remotePTT: false, txDB: -50, rxDB: -55, rttMS: nil)
        let session = CallSession(station: first)
        session.start()
        defer { session.stop() }
        XCTAssertEqual(session.status, .dialing, "start() polls once immediately")

        let second = FakeStation()
        second.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -10, rxDB: -10, rttMS: 5)
        session.reconfigure(station: second, hasCredentials: true)

        // reconfigure restarted the loop on the new station, which polls once.
        XCTAssertEqual(session.status, .answered, "reconfigure re-polls the new station")
        XCTAssertTrue(session.hasCredentials)
    }
}
