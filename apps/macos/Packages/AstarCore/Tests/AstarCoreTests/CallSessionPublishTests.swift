// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Combine
import XCTest

@testable import AstarCore

/// astar-3e04: the popover re-rendered ~20×/s forever because `poll()`
/// reassigned every `@Published` property each tick — Combine publishes on
/// every write, changed or not — saturating the main thread until the app
/// beach-balled after hours. These tests pin the fix: an unchanged snapshot
/// publishes nothing on the session, high-churn meter fields live on the
/// separate `CallMeters` object so their ticks never invalidate the whole
/// popover, and level jitter below the display quantum is silent entirely.
final class CallSessionPublishTests: XCTestCase {
    private var fake: FakeStation!
    private var session: CallSession!
    private var cancellables: Set<AnyCancellable> = []

    override func setUp() {
        super.setUp()
        fake = FakeStation()
        session = CallSession(station: fake)
    }

    override func tearDown() {
        cancellables.removeAll()
        super.tearDown()
    }

    /// Count objectWillChange emissions from `publisher` across `body()`.
    private func emissions<P: Publisher>(
        of publisher: P, during body: () -> Void
    ) -> Int where P.Failure == Never {
        var fired = 0
        publisher.sink { _ in fired += 1 }.store(in: &cancellables)
        body()
        return fired
    }

    func testUnchangedSnapshotPublishesNothing() {
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -18, rxDB: -25, inputDB: -33, rttMS: 42, negotiatedFormat: .slin16
        )
        session.poll()  // settle the published state onto the snapshot

        let sessionFired = emissions(of: session.objectWillChange) { session.poll() }
        let metersFired = emissions(of: session.meters.objectWillChange) { session.poll() }

        XCTAssertEqual(sessionFired, 0, "an identical snapshot must not invalidate the session")
        XCTAssertEqual(metersFired, 0, "an identical snapshot must not invalidate the meters")
    }

    func testStatusEdgeStillPublishes() {
        session.poll()  // settle on idle
        fake.snapshotToReturn.status = .answered

        let fired = emissions(of: session.objectWillChange) { session.poll() }

        XCTAssertGreaterThan(fired, 0, "a real state edge must still publish")
        XCTAssertEqual(session.status, .answered)
    }

    func testMeterChurnStaysOffTheSession() {
        fake.snapshotToReturn.inputDB = -50
        fake.snapshotToReturn.rxDB = -40
        session.poll()  // settle
        // Big level swings on every meter, no lifecycle change.
        fake.snapshotToReturn.inputDB = -20
        fake.snapshotToReturn.rxDB = -12
        fake.snapshotToReturn.txDB = -15
        fake.snapshotToReturn.rttMS = 55

        let sessionFired = emissions(of: session.objectWillChange) { session.poll() }

        XCTAssertEqual(
            sessionFired, 0,
            "meter/RTT churn must invalidate only CallMeters, never the whole session")
        XCTAssertEqual(session.meters.inputDB, -20)
        XCTAssertEqual(session.meters.rxDB, -12)
        XCTAssertEqual(session.meters.rttMS, 55)
    }

    func testMeterJitterBelowQuantumIsSilent() {
        fake.snapshotToReturn.inputDB = -55.1
        fake.snapshotToReturn.rxDB = -47.6
        session.poll()  // settle

        // Same 0.5 dB bucket: -55.2 quantizes with -55.1, -47.7 with -47.6.
        fake.snapshotToReturn.inputDB = -55.2
        fake.snapshotToReturn.rxDB = -47.7
        let jitterFired = emissions(of: session.meters.objectWillChange) { session.poll() }
        XCTAssertEqual(jitterFired, 0, "noise-floor jitter below 0.5 dB must not publish")

        // A visible move must publish.
        fake.snapshotToReturn.inputDB = -30
        let realFired = emissions(of: session.meters.objectWillChange) { session.poll() }
        XCTAssertGreaterThan(realFired, 0)
    }

    func testRawLevelsKeepFullPrecisionWithoutPublishing() {
        // astar-3458: the level graph reads raw levels on its own timeline, so
        // CallMeters must retain the exact snapshot values in non-published
        // fields — full float precision, zero objectWillChange — while the
        // published fields stay quantized for the VU bars.
        fake.snapshotToReturn.txDB = -23.4
        fake.snapshotToReturn.rxDB = -31.8
        session.poll()  // settle

        // Drift within one 0.5 dB bucket: published fields stay silent, but the
        // raw fields must track the exact new values.
        fake.snapshotToReturn.txDB = -23.6
        fake.snapshotToReturn.rxDB = -31.9
        let fired = emissions(of: session.meters.objectWillChange) { session.poll() }

        XCTAssertEqual(fired, 0, "sub-quantum drift must not publish")
        XCTAssertEqual(session.meters.rawTxDB, -23.6)
        XCTAssertEqual(session.meters.rawRxDB, -31.9)
        XCTAssertEqual(session.meters.txDB, -23.5, "published field stays on the 0.5 dB grid")
    }

    func testVoxStillKeysFromRawInputLevel() {
        // Quantization is display-only: the VOX gate must keep seeing the raw
        // snapshot level, keying exactly at threshold as before.
        session.setVoxEnabled(true)
        session.setVoxThreshold(-40)
        fake.snapshotToReturn.inputDB = -39.9
        session.poll()
        XCTAssertEqual(fake.pttCalls.last, true, "VOX keys from the raw mic level")
    }
}
