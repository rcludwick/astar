// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import AstarStation
import XCTest

@testable import AstarCore

/// CallSession-side talk-timer behavior: keyed-start tracking across the poll
/// loop (reset every unkey) and the per-node phase resolution the dot reads.
final class CallSessionTalkTimerTests: XCTestCase {
    private func keyedSnapshot(_ ptt: Bool) -> CallSnapshot {
        CallSnapshot(
            status: .answered, ptt: ptt, remotePTT: false,
            txDB: ptt ? -12 : -60, rxDB: -50, rttMS: 30)
    }

    func testKeyedSinceSetOnKeyAndClearedOnUnkey() {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        // Unkeyed: no start time.
        fake.snapshotToReturn = keyedSnapshot(false)
        session.poll()
        XCTAssertNil(session.keyedSince)

        // Key → start time stamped.
        fake.snapshotToReturn = keyedSnapshot(true)
        session.poll()
        let first = session.keyedSince
        XCTAssertNotNil(first)

        // Still keyed → start time is held (not re-stamped each tick).
        session.poll()
        XCTAssertEqual(session.keyedSince, first, "the keyed-start time must persist across ticks")

        // Unkey → cleared (resets the timer).
        fake.snapshotToReturn = keyedSnapshot(false)
        session.poll()
        XCTAssertNil(session.keyedSince)
    }

    func testPhaseNilWhenNotKeyed() {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        session.setDialedNode("77777")
        fake.snapshotToReturn = keyedSnapshot(false)
        session.poll()
        XCTAssertNil(
            session.talkTimerPhase(defaultEnabled: true, defaultLimitSeconds: 120),
            "no dot while unkeyed")
    }

    func testPhaseNilWhenNoDialedNode() {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        fake.snapshotToReturn = keyedSnapshot(true)
        session.poll()
        XCTAssertNil(session.talkTimerPhase(defaultEnabled: true, defaultLimitSeconds: 120))
    }

    func testPhaseUsesGlobalDefaultForUnconfiguredNode() {
        let fake = FakeStation()
        let directory = UserDefaultsNodeDirectoryStore(scratchDefaults())
        let session = CallSession(station: fake, directoryStore: directory)
        session.setDialedNode("77777")
        fake.snapshotToReturn = keyedSnapshot(true)
        session.poll()
        guard let since = session.keyedSince else { return XCTFail("expected keyedSince") }

        // 1 s in, 120 s default → green.
        XCTAssertEqual(
            session.talkTimerPhase(
                defaultEnabled: true, defaultLimitSeconds: 120, now: since.addingTimeInterval(1)),
            .green)
        // 110 s in → amber (within 15 s of 120).
        XCTAssertEqual(
            session.talkTimerPhase(
                defaultEnabled: true, defaultLimitSeconds: 120, now: since.addingTimeInterval(110)),
            .amber)
        // 130 s in → red.
        XCTAssertEqual(
            session.talkTimerPhase(
                defaultEnabled: true, defaultLimitSeconds: 120, now: since.addingTimeInterval(130)),
            .red)
    }

    func testPhaseUsesPerNodeOverride() {
        let fake = FakeStation()
        let directory = UserDefaultsNodeDirectoryStore(scratchDefaults())
        let session = CallSession(station: fake, directoryStore: directory)
        session.addFavorite(node: "77777", label: "Repeater")
        let id = directory.all().first { $0.node == "77777" }!.id
        // Custom 60 s limit for this node.
        session.directorySetTalkTimer(id: id, enabled: true, seconds: 60)

        session.setDialedNode("77777")
        fake.snapshotToReturn = keyedSnapshot(true)
        session.poll()
        guard let since = session.keyedSince else { return XCTFail("expected keyedSince") }

        // 30 s into a 60 s limit → green; 50 s → amber; 65 s → red.
        XCTAssertEqual(
            session.talkTimerPhase(
                defaultEnabled: true, defaultLimitSeconds: 120, now: since.addingTimeInterval(30)),
            .green)
        XCTAssertEqual(
            session.talkTimerPhase(
                defaultEnabled: true, defaultLimitSeconds: 120, now: since.addingTimeInterval(50)),
            .amber)
        XCTAssertEqual(
            session.talkTimerPhase(
                defaultEnabled: true, defaultLimitSeconds: 120, now: since.addingTimeInterval(65)),
            .red)
    }

    func testPhaseNilWhenDisabledForNode() {
        let fake = FakeStation()
        let directory = UserDefaultsNodeDirectoryStore(scratchDefaults())
        let session = CallSession(station: fake, directoryStore: directory)
        session.addFavorite(node: "77777", label: "Repeater")
        let id = directory.all().first { $0.node == "77777" }!.id
        session.directorySetTalkTimer(id: id, enabled: false, seconds: nil)

        session.setDialedNode("77777")
        fake.snapshotToReturn = keyedSnapshot(true)
        session.poll()
        XCTAssertNil(
            session.talkTimerPhase(defaultEnabled: true, defaultLimitSeconds: 120),
            "disabled for this node → no dot even while keyed")
    }

    /// A fresh, isolated UserDefaults suite for directory storage.
    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.talktimer.\(UUID().uuidString)"
        let d = UserDefaults(suiteName: suite)!
        d.removePersistentDomain(forName: suite)
        return d
    }
}
