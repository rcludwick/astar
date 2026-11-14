// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// `CallSession.bluetoothPTT(_:)`: the one door a Bluetooth press passes
/// through. A refused press must never reach the station, so the arbiter
/// never even sees a hold. A release must always reach it.
final class BluetoothPTTChokePointTests: XCTestCase {
    private struct Boom: Error {}

    private func session(status: IaxStatus = .answered) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: status, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, inputDB: -60, rttMS: 20)
        let session = hermeticSession(fake, defaults: bluetoothScratchDefaults(cleanup: self))
        session.poll()
        return (session, fake)
    }

    /// The NXDN recipe from CallSessionNXDNDialTests, answered: a live call on
    /// a network astar can hear but not key into.
    private func nxdnAnsweredSession() throws -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            nxdnAvailable: true)
        // Scratch defaults: the callsign and radio id below persist on set.
        let session = hermeticSession(fake, defaults: bluetoothScratchDefaults(cleanup: self))
        session.operatorCallsign = "AJ7HR"
        session.nxdnRadioID = "1234"
        session.reflectorIndex = ReflectorIndex(entries: [
            DirectoryEntry(
                network: .nxdn, id: "100", name: "NXDN 100",
                dial: .nxdn(host: "nxdn.example", port: 41400))
        ])
        session.poll()
        try session.connect(node: "100", network: .nxdn)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            nxdnAvailable: true, nxdnActive: true)
        session.poll()
        return (session, fake)
    }

    func testBluetoothPressWhenAnsweredTakesALease() {
        let (session, fake) = session()

        XCTAssertEqual(session.bluetoothPTT(.press), .accepted)

        XCTAssertEqual(fake.leasePressCalls.map { $0.source }, [.bluetooth])
        XCTAssertEqual(fake.leasePressCalls.map { $0.ttl }, [.milliseconds(1500)])
        XCTAssertEqual(bluetoothKeyLog(fake), [BLETestFixtures.leasePress])
    }

    func testBluetoothPressRefusedWhenNotAnswered() {
        for status in [IaxStatus.idle, .dialing, .hangup] {
            let (session, fake) = session(status: status)

            XCTAssertEqual(session.bluetoothPTT(.press), .refused, "\(status)")

            XCTAssertEqual(
                fake.leasePressCalls.count, 0, "a refused press never reaches the station")
        }
    }

    func testBluetoothPressRefusedWhenListenOnly() {
        let (session, fake) = session()
        session.setTxDisabled(true)

        XCTAssertEqual(session.bluetoothPTT(.press), .refused)

        XCTAssertEqual(fake.leasePressCalls.count, 0)
    }

    func testBluetoothPressRefusedOnNxdn() throws {
        let (session, fake) = try nxdnAnsweredSession()
        XCTAssertEqual(session.status, .answered)
        XCTAssertFalse(session.canTransmit)

        XCTAssertEqual(session.bluetoothPTT(.press), .refused)

        XCTAssertEqual(fake.leasePressCalls.count, 0)
    }

    func testReleaseAlwaysReachesStation() throws {
        let idle = session(status: .idle)
        let listenOnly = session()
        listenOnly.0.setTxDisabled(true)
        let nxdn = try nxdnAnsweredSession()
        let answered = session()

        for (name, pair) in [
            ("idle", idle), ("listen-only", listenOnly), ("nxdn", nxdn), ("answered", answered),
        ] {
            XCTAssertEqual(pair.0.bluetoothPTT(.release), .accepted, name)
            XCTAssertEqual(pair.1.pttLog.last, BLETestFixtures.release, name)
            XCTAssertEqual(pair.1.leasePressCalls.count, 0, name)
        }
    }

    func testLeasePressFailureIsLeaseLost() {
        let (session, fake) = session()
        fake.leasePressError = Boom()

        XCTAssertEqual(session.bluetoothPTT(.press), .leaseLost)
    }

    func testRenewNeverPressesAndReportsFailureAsLeaseLost() {
        let (session, fake) = session()

        XCTAssertEqual(session.bluetoothPTT(.renew), .accepted)
        XCTAssertEqual(fake.leaseRenewCalls, [.bluetooth])
        XCTAssertEqual(fake.leasePressCalls.count, 0, "renew never presses")

        fake.leaseRenewError = Boom()
        XCTAssertEqual(session.bluetoothPTT(.renew), .leaseLost)
        XCTAssertEqual(fake.leasePressCalls.count, 0)
    }

    func testBluetoothLeaseTTLIsTheMachineDefault() {
        XCTAssertEqual(CallSession.bluetoothLeaseTTL, .milliseconds(1500))
        XCTAssertEqual(
            CallSession.bluetoothLeaseTTL,
            .milliseconds(BLEButtonMachine.defaultConfig.leaseTTLMS))
    }

    func testSessionIsTheSinkAndReportsTransmitBlocked() {
        let (session, _) = session()
        let sink: BluetoothPTTSink = session
        XCTAssertFalse(sink.transmitBlocked)

        session.setTxDisabled(true)

        XCTAssertTrue(sink.transmitBlocked)
    }
}
