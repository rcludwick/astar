// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Combine
import XCTest

@testable import AstarCore

/// Every keying source is a named source on the engine's arbiter, which ORs
/// them. These pin which calls `CallSession` makes; the arbiter's own rules
/// (latching, the lease, the time-out clock) are pinned in Rust.
final class CallSessionPTTSourcesTests: XCTestCase {
    private struct SnapshotFailed: Error {}

    /// An answered call on a network that can transmit, quiet mic, no RX.
    private func answered(
        sources: StationPTTSourceSet = []
    ) -> (CallSession, FakeStation, MemoryAudioStore) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, inputDB: -60, rttMS: 20)
        fake.snapshotToReturn.pttSources = sources
        let store = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: store)
        // These pin which source calls are made, one poll per edge; the
        // VOX attack has its own tests.
        session.voxGate.config.attackMS = 0
        return (session, fake, store)
    }

    // MARK: - OR keying (the deliberate change)

    func testVoxCloseDoesNotUnkeyHeldSerial() {
        // Before the arbiter, VOX closing called setPTT(false) and cut off a
        // handset that was still held. Now VOX releases only its own source.
        let (session, fake, _) = answered()
        session.setVoxEnabled(true)
        session.setVoxHangtime(0)  // close on the first quiet sample
        var serialEdges: [Bool?] = [true]
        session.pttSourceTick = { _, _ in serialEdges.isEmpty ? nil : serialEdges.removeFirst() }

        fake.snapshotToReturn.voxDB = -10
        session.poll()  // VOX opens, the handset keys
        fake.snapshotToReturn.voxDB = -60
        session.poll()  // VOX closes; the handset is still held

        XCTAssertEqual(
            fake.keyingLog,
            ["pttSource(vox,true)", "pttSource(serial,true)", "pttSource(vox,false)"])
        XCTAssertEqual(fake.stopAllCount, 0, "a VOX close is never a stop")
    }

    func testVoxCloseDoesNotUnkeyHeldBluetooth() {
        // The Bluetooth button holds through the engine's lease, not through
        // CallSession, so all VOX may do when it closes is release `.vox`.
        let (session, fake, _) = answered(sources: [.bluetooth])
        session.setVoxEnabled(true)
        session.setVoxHangtime(0)  // close on the first quiet sample

        fake.snapshotToReturn.voxDB = -10
        session.poll()  // VOX opens while the button holds
        fake.snapshotToReturn.voxDB = -60
        session.poll()  // VOX closes; the button is still held

        XCTAssertEqual(fake.keyingLog, ["pttSource(vox,true)", "pttSource(vox,false)"])
        XCTAssertEqual(fake.stopAllCount, 0, "a VOX close never stops the button's over")
    }

    func testDisablingVoxReleasesVoxOnly() {
        let (session, fake, _) = answered()
        session.setVoxEnabled(true)
        var serialEdges: [Bool?] = [true]
        session.pttSourceTick = { _, _ in serialEdges.isEmpty ? nil : serialEdges.removeFirst() }
        fake.snapshotToReturn.voxDB = -10
        session.poll()

        session.setVoxEnabled(false)

        XCTAssertEqual(
            fake.keyingLog,
            ["pttSource(vox,true)", "pttSource(serial,true)", "pttSource(vox,false)"])
        XCTAssertEqual(fake.stopAllCount, 0)
    }

    // MARK: - The universal stop

    func testScreenPressDuringBluetoothOverStops() throws {
        let (session, fake, _) = answered(sources: [.bluetooth])
        // Not polled yet: the published set is still empty, so the rule has to
        // read a fresh snapshot rather than trust it.
        XCTAssertEqual(session.pttSources, [])
        XCTAssertFalse(session.manualPressStops)

        try session.setPTT(true)

        XCTAssertEqual(fake.keyingLog, ["pttStopAll"], "a press during another source's over stops")
        XCTAssertEqual(fake.stopAllCount, 1)

        session.poll()
        XCTAssertEqual(session.pttSources, [.bluetooth])
        XCTAssertTrue(session.manualPressStops)

        try session.setPTT(false)  // the same click's release
        XCTAssertEqual(fake.keyingLog, ["pttStopAll", "pttSource(manual,false)"])
    }

    func testManualPressStopsWithStalePublishedSources() throws {
        // A Space or on-screen press that lands inside the 50 ms poll gap
        // right after a B02PTT press: the engine already holds Bluetooth, the
        // published set does not know yet. The press must still STOP.
        let (session, fake, _) = answered(sources: [.bluetooth])
        XCTAssertEqual(session.pttSources, [], "not polled: the published set is stale")

        try session.setPTT(true)

        XCTAssertEqual(fake.stopAllCount, 1)
        XCTAssertEqual(fake.pttSourceCalls.count, 0, "the press never reaches .manual")
        XCTAssertEqual(fake.pttLog, ["pttStopAll"])
    }

    func testScreenPressStopsWhicheverOtherSourceHolds() throws {
        let holders: [StationPTTSourceSet] = [
            [.vox], [.serial], [.legacy], [.bluetooth], [.manual, .bluetooth], [.manual, .vox],
        ]
        for holder in holders {
            let (session, fake, _) = answered(sources: holder)
            try session.setPTT(true)
            XCTAssertEqual(fake.keyingLog, ["pttStopAll"], "holder \(holder.rawValue)")
        }
    }

    func testStopRuleFallsBackToPublishedSourcesWhenSnapshotFails() throws {
        let (session, fake, _) = answered(sources: [.serial])
        session.poll()
        XCTAssertEqual(session.pttSources, [.serial])
        fake.snapshotError = SnapshotFailed()

        try session.setPTT(true)

        XCTAssertEqual(fake.keyingLog, ["pttStopAll"])
    }

    func testSpacePressWithOnlyManualHeldKeys() throws {
        // Key repeat, or a second press while Space already holds: Manual alone
        // is not "another source", so it keys rather than stops.
        let (session, fake, _) = answered(sources: [.manual])
        session.poll()
        XCTAssertFalse(session.manualPressStops)

        try session.setPTT(true)

        XCTAssertEqual(fake.keyingLog, ["pttSource(manual,true)"])
        XCTAssertEqual(fake.stopAllCount, 0)
    }

    func testIdlePressKeysManual() throws {
        let (session, fake, _) = answered()
        try session.setPTT(true)
        try session.setPTT(false)
        XCTAssertEqual(fake.keyingLog, ["pttSource(manual,true)", "pttSource(manual,false)"])
    }

    // MARK: - Listen-only

    func testSetTxDisabledStopsAll() {
        let (session, fake, _) = answered()
        session.setVoxEnabled(true)
        fake.snapshotToReturn.voxDB = -10
        session.poll()  // VOX keys

        session.setTxDisabled(true)

        // The stop latches `.vox` and nothing releases it here: VOX re-arms
        // only when its own gate closes (see the next test).
        let afterStop = ["pttSource(vox,true)", "pttStopAll"]
        XCTAssertEqual(fake.keyingLog, afterStop)
        XCTAssertTrue(session.transmitBlocked)

        session.setTxDisabled(false)
        XCTAssertEqual(fake.keyingLog, afterStop, "turning listen-only off never re-keys")
        XCTAssertFalse(session.transmitBlocked)
    }

    func testVoxReArmsOnlyOnceItsGateClosesAfterListenOnly() {
        // Listen-only is a forced stop: the engine latched `.vox`, and VOX is
        // released (which re-arms it) only on its own gate's falling edge. A
        // mic that never went quiet must not re-key just because listen-only
        // went off.
        let (session, fake, _) = answered()
        session.setVoxEnabled(true)
        session.setVoxHangtime(0)  // close on the first quiet sample
        fake.snapshotToReturn.voxDB = -10
        session.poll()  // VOX keys
        session.setTxDisabled(true)
        session.setTxDisabled(false)
        let afterStop = ["pttSource(vox,true)", "pttStopAll"]

        session.poll()  // (1) still loud: no new edge, so no re-key
        XCTAssertEqual(
            fake.keyingLog, afterStop, "a still-loud mic must not re-key after listen-only")

        fake.snapshotToReturn.voxDB = -60
        session.poll()  // (2) quiet: the falling edge releases `.vox` and clears its latch
        XCTAssertEqual(fake.keyingLog, afterStop + ["pttSource(vox,false)"])

        fake.snapshotToReturn.voxDB = -10
        session.poll()  // (3) loud again: a fresh over keys normally
        XCTAssertEqual(
            fake.keyingLog, afterStop + ["pttSource(vox,false)", "pttSource(vox,true)"])
    }

    func testSetTxDisabledStopsAllEvenWithNothingKeyedHere() {
        // Bluetooth or Legacy may hold without CallSession having keyed
        // anything itself: listen-only must still stop the engine.
        let (session, fake, _) = answered(sources: [.bluetooth])
        session.setTxDisabled(true)
        XCTAssertEqual(fake.keyingLog, ["pttStopAll"])
    }

    func testStopAllTransmitCallsStopAll() {
        let (session, fake, _) = answered()
        session.stopAllTransmit()
        XCTAssertEqual(fake.keyingLog, ["pttStopAll"])
        XCTAssertEqual(fake.stopAllCount, 1)
    }

    // MARK: - Snapshot publishing

    func testForcedStopPublishedOncePerSeq() throws {
        let (session, fake, _) = answered()
        var published: [PTTForcedStop?] = []
        let subscription = session.$lastForcedStop.dropFirst().sink { published.append($0) }
        defer { subscription.cancel() }

        session.poll()  // seq 0: nothing has been forced
        XCTAssertNil(session.lastForcedStop)
        XCTAssertEqual(published.count, 0)

        fake.snapshotToReturn.pttForcedSeq = 1
        fake.snapshotToReturn.pttForcedSources = [.serial, .vox]
        fake.snapshotToReturn.pttForcedReason = .txTimeout
        fake.snapshotToReturn.pttTxTimeoutMS = 180_000
        session.poll()
        session.poll()  // same seq: must not publish again

        XCTAssertEqual(published.count, 1)
        let first = try XCTUnwrap(session.lastForcedStop)
        XCTAssertEqual(first.seq, 1)
        XCTAssertEqual(first.sources, [.serial, .vox])
        XCTAssertEqual(first.reason, .txTimeout)
        XCTAssertEqual(first.txTimeoutSeconds, 180)

        fake.snapshotToReturn.pttForcedSeq = 2
        fake.snapshotToReturn.pttForcedSources = [.bluetooth]
        fake.snapshotToReturn.pttForcedReason = .leaseExpired
        session.poll()

        XCTAssertEqual(published.count, 2)
        let second = try XCTUnwrap(session.lastForcedStop)
        XCTAssertEqual(second.seq, 2)
        XCTAssertEqual(second.sources, [.bluetooth])
        XCTAssertEqual(second.reason, .leaseExpired)
    }

    func testReconfigureForgetsTheOldStationsForcedSeq() {
        // A rebuilt station counts forced stops from zero again; its first one
        // must not be mistaken for the old station's last.
        let (session, fake, _) = answered()
        fake.snapshotToReturn.pttForcedSeq = 1
        fake.snapshotToReturn.pttForcedReason = .txTimeout
        session.poll()
        XCTAssertEqual(session.lastForcedStop?.seq, 1)

        let rebuilt = FakeStation()
        rebuilt.snapshotToReturn = fake.snapshotToReturn
        rebuilt.snapshotToReturn.pttForcedSources = [.manual]
        session.reconfigure(station: rebuilt, hasCredentials: false)
        session.poll()

        XCTAssertEqual(session.lastForcedStop?.sources, [.manual])
    }

    func testUnchangedPTTStatePublishesNothing() {
        let (session, fake, _) = answered(sources: [.serial])
        fake.snapshotToReturn.pttForcedSeq = 3
        fake.snapshotToReturn.pttForcedReason = .txTimeout
        session.poll()  // settle
        var fired = 0
        let subscription = session.objectWillChange.sink { _ in fired += 1 }
        defer { subscription.cancel() }

        session.poll()

        XCTAssertEqual(fired, 0, "a 20 Hz poll with nothing new must not re-render")
    }

    // MARK: - Transmit time-out

    func testTxTimeoutPushedAtLaunchAndOnChange() {
        let (session, fake, store) = answered()
        XCTAssertEqual(session.txTimeoutSeconds, 120)

        // Launch: `CallSession.live` calls applyAudioSettings with the store.
        session.applyAudioSettings(AudioSettings(txTimeoutSeconds: 300))
        XCTAssertEqual(session.txTimeoutSeconds, 300)
        XCTAssertEqual(fake.txTimeoutCalls, [.seconds(300)])

        session.setTxTimeoutSeconds(240)
        XCTAssertEqual(session.txTimeoutSeconds, 240)
        XCTAssertEqual(store.settings.txTimeoutSeconds, 240)
        XCTAssertEqual(fake.txTimeoutCalls, [.seconds(300), .seconds(240)])

        // A station rebuilt after a credential change starts at the engine's
        // 120 s default; the session must push the user's value onto it.
        let rebuilt = FakeStation()
        session.reconfigure(station: rebuilt, hasCredentials: false)
        XCTAssertEqual(rebuilt.txTimeoutCalls, [.seconds(240)])
    }

    func testReconfigurePushesTxTimeoutToNewStation() {
        // Saving AllStarLink credentials rebuilds the station; the new engine
        // starts at 120 s and must be given the operator's value at once.
        let (session, _, _) = answered()
        session.setTxTimeoutSeconds(300)
        let s2 = FakeStation()

        session.reconfigure(station: s2, hasCredentials: false)

        XCTAssertEqual(s2.txTimeoutCalls, [.seconds(300)])
        XCTAssertEqual(session.txTimeoutSeconds, 300)
    }

    func testSetTxTimeoutSecondsNormalises() {
        let (session, fake, store) = answered()
        session.setTxTimeoutSeconds(0)
        XCTAssertEqual(session.txTimeoutSeconds, 60, "0 is never Off")
        XCTAssertEqual(store.settings.txTimeoutSeconds, 60)
        session.setTxTimeoutSeconds(900)
        session.setTxTimeoutSeconds(90)
        XCTAssertEqual(fake.txTimeoutCalls, [.seconds(60), .seconds(600), .seconds(120)])
        XCTAssertEqual(session.txTimeoutSeconds, 120)
    }

    func testApplyAudioSettingsNormalisesAStoredZero() {
        let (session, fake, _) = answered()
        session.applyAudioSettings(AudioSettings(txTimeoutSeconds: 0))
        XCTAssertEqual(session.txTimeoutSeconds, 60)
        XCTAssertEqual(fake.txTimeoutCalls, [.seconds(60)])
    }

    func testVoxHangtimeUntouchedByTxTimeout() {
        let (session, fake, store) = answered()
        session.setTxTimeoutSeconds(600)
        XCTAssertEqual(session.voxHangtimeMS, 500)
        XCTAssertEqual(session.voxGate.config.hangoverMS, 500)
        XCTAssertEqual(store.settings.voxHangtimeMS, 500)

        session.setVoxHangtime(900)
        XCTAssertEqual(session.txTimeoutSeconds, 600)
        XCTAssertEqual(store.settings.txTimeoutSeconds, 600)
        XCTAssertEqual(
            fake.txTimeoutCalls, [.seconds(600)], "a hang-time change pushes no time-out")
    }
}
