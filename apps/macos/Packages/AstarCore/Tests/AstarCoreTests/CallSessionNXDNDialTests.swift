// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// `CallSession` linking NXDN: a directory row becomes a host, a port and the
/// talkgroup its id names, and the receive-only shape of the network is held
/// to at the key choke point rather than only in the UI.
///
/// Every dial here lands on `FakeStation`. Nothing opens a serial port,
/// nothing resolves a host, and nothing leaves the process — the only way a
/// suite for a hardware-gated network can exist at all.
final class CallSessionNXDNDialTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.nxdndial.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    /// Shapes from the published feed: the id is the TALKGROUP, as a string,
    /// and there are no callsigns and no modules anywhere in it.
    private var nxdnIndex: ReflectorIndex {
        ReflectorIndex(entries: [
            DirectoryEntry(
                network: .nxdn, id: "100", name: "NXDN 100",
                dial: .nxdn(host: "nxdn.example", port: 41400)),
            DirectoryEntry(
                network: .nxdn, id: "65000", name: "NXDN 65000",
                dial: .nxdn(host: "odd.example", port: 41401)),
            DirectoryEntry(network: .nxdn, id: "31", name: "NXDN 31", dial: nil),
        ])
    }

    private func session(available: Bool = true, radioID: String = "1234") -> (
        CallSession, FakeStation
    ) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            nxdnAvailable: available)
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.nxdnRadioID = radioID
        session.reflectorIndex = nxdnIndex
        session.poll()  // adopt `nxdnAvailable` from the snapshot
        return (session, fake)
    }

    // MARK: - Dialling by talkgroup

    /// The talkgroup comes from the row's id. Getting it from anywhere else —
    /// or defaulting it — is a link that comes up and stays silent, because
    /// the reflector drops every frame addressed elsewhere.
    func testAResolvedRowDialsItsAddressAndItsIDAsTheTalkgroup() throws {
        let (session, fake) = session()
        try session.connect(node: "100", network: .nxdn)
        XCTAssertEqual(fake.connectNXDNCalls.count, 1)
        XCTAssertEqual(fake.connectNXDNCalls.first?.host, "nxdn.example:41400")
        XCTAssertEqual(fake.connectNXDNCalls.first?.callsign, "AJ7HR")
        XCTAssertEqual(fake.connectNXDNCalls.first?.talkgroup, 100)
        XCTAssertEqual(fake.connectNXDNCalls.first?.radioID, 1234)
        XCTAssertEqual(session.activeCallNetwork, .nxdn)
    }

    func testTheDirectorysPortIsUsedNotTheDefault() throws {
        let (session, fake) = session()
        try session.connect(node: "65000", network: .nxdn)
        XCTAssertEqual(fake.connectNXDNCalls.first?.host, "odd.example:41401")
        XCTAssertEqual(fake.connectNXDNCalls.first?.talkgroup, 65000)
    }

    func testAListedButUndialableRowIsRefusedByName() {
        let (session, fake) = session()
        XCTAssertThrowsError(try session.connect(node: "31", network: .nxdn)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .reflectorNotDialable("31"))
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty)
    }

    // MARK: - Dialling by address

    func testAnUnknownNameFallsThroughToTheAddressGrammar() throws {
        let (session, fake) = session()
        try session.connect(node: "other.example:41402/222", network: .nxdn)
        XCTAssertEqual(fake.connectNXDNCalls.first?.host, "other.example:41402")
        XCTAssertEqual(fake.connectNXDNCalls.first?.talkgroup, 222)
    }

    func testATypedAddressWithNoPortTakesTheCommonOne() throws {
        let (session, fake) = session()
        try session.connect(node: "other.example/222", network: .nxdn)
        XCTAssertEqual(fake.connectNXDNCalls.first?.host, "other.example:41400")
    }

    /// A bare address is a target with the room missing. Guessing a talkgroup
    /// would produce a link that never carries audio and never says why.
    func testATypedAddressWithNoTalkgroupIsRefused() {
        let (session, fake) = session()
        XCTAssertFalse(session.canDial("other.example", network: .nxdn), "Connect stays off")
        XCTAssertThrowsError(try session.connect(node: "other.example", network: .nxdn)) {
            error in
            XCTAssertEqual(error as? CallSession.ConnectError, .badNXDNTarget)
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty)
    }

    func testATargetWithAModuleIsRefusedRatherThanTruncated() {
        let (session, fake) = session()
        XCTAssertThrowsError(try session.connect(node: "XLX836 A", network: .nxdn)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .badNXDNTarget)
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty)
    }

    // MARK: - Refusals

    func testDiallingWithNoVocoderIsRefusedBeforeTheEngine() {
        let (session, fake) = session(available: false)
        XCTAssertThrowsError(try session.connect(node: "100", network: .nxdn)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .nxdnUnavailable)
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty, "the dongle is never opened")
    }

    func testARefusedDialWithNoCallsignNeverReachesTheDongle() {
        let (session, fake) = session()
        session.operatorCallsign = "  "
        XCTAssertThrowsError(try session.connect(node: "100", network: .nxdn)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingCallsign)
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty)
    }

    func testDiallingNXDNWithNoRadioIDIsRefused() {
        let (session, fake) = session(radioID: "")
        XCTAssertThrowsError(try session.connect(node: "100", network: .nxdn)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingRadioID)
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty, "nothing reached the engine")
    }

    /// NXDN source ids are 16-bit (`NXDNGateway/NXDNNetwork.cpp`: `writeData`
    /// takes `unsigned short srcId`). A 7-digit DMR ID does not fit, and
    /// truncating it would put somebody else's number on the air — so the
    /// field refuses it on the way in and the dial refuses what is left.
    func testADMRSizedRadioIDIsRefusedRatherThanTruncated() {
        let (session, fake) = session(radioID: "")
        session.nxdnRadioID = "3153591"
        XCTAssertThrowsError(try session.connect(node: "100", network: .nxdn)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .radioIDOutOfRange)
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty)
    }

    /// The DMR field is a different credential and must not stand in for this
    /// one, however plausible it looks.
    func testTheDMRRadioIDIsNotUsedForNXDN() {
        let (session, fake) = session(radioID: "")
        session.dmrRadioID = "3153591"
        XCTAssertThrowsError(try session.connect(node: "100", network: .nxdn)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingRadioID)
        }
        XCTAssertTrue(fake.connectNXDNCalls.isEmpty)
    }

    // MARK: - Live state

    func testThePollAdoptsTheLinksOwnState() throws {
        let (session, fake) = session()
        try session.connect(node: "100", network: .nxdn)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -20, rttMS: nil,
            nxdnAvailable: true, nxdnActive: true)
        fake.nxdnStateValue = NXDNState(
            link: .linked, lastHeard: "4242", lastHeardID: 4242, framesRX: 412,
            receiving: true, backend: .thumbdv)
        session.poll()

        XCTAssertEqual(session.nxdnLink, .linked)
        XCTAssertEqual(session.nxdnLastHeard, "4242")
        XCTAssertTrue(session.nxdnReceiving)
        // The one line every digital network shares.
        XCTAssertEqual(session.lastHeard, "4242")
    }

    func testTheLastHeardFieldsClearWhenTheLinkGoesAway() throws {
        let (session, fake) = session()
        try session.connect(node: "100", network: .nxdn)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -20, rttMS: nil,
            nxdnAvailable: true, nxdnActive: true)
        fake.nxdnStateValue = NXDNState(
            link: .linked, lastHeard: "4242", lastHeardID: 4242, framesRX: 1)
        session.poll()
        XCTAssertEqual(session.nxdnLastHeard, "4242")

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            nxdnAvailable: true, nxdnActive: false)
        fake.nxdnStateValue = nil
        session.poll()

        XCTAssertNil(session.nxdnLastHeard)
        XCTAssertNil(session.nxdnLink)
        XCTAssertFalse(session.nxdnReceiving)
        XCTAssertNil(session.lastHeard)
    }

    // MARK: - Transmit

    /// The receive-first gate. The PTT control and the key choke point are the
    /// two places that must never disagree, so this asserts the second one —
    /// a key request must not reach the station even if something offered a
    /// button.
    func testNXDNCannotTransmit() throws {
        let (session, fake) = session()
        try session.connect(node: "100", network: .nxdn)
        XCTAssertFalse(session.canTransmit)

        try session.setPTT(true)
        XCTAssertEqual(fake.pttCalls.last, false, "a key-down must never reach the station")
    }

    // MARK: - Teardown

    func testHangingUpDisconnectsTheLinkExplicitly() throws {
        let (session, fake) = session()
        try session.connect(node: "100", network: .nxdn)

        try session.disconnect()

        XCTAssertEqual(fake.nxdnDisconnects, 1)
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.nxdnLastHeard)
        XCTAssertTrue(session.canTransmit, "the gate lifts with the network")
    }

    // MARK: - Persistence

    func testTheNXDNIDIsSanitisedAndPersisted() {
        let defaults = scratchDefaults()
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: defaults)
        session.nxdnRadioID = " 12a34 "
        XCTAssertEqual(session.nxdnRadioID, "1234")
        XCTAssertEqual(defaults.string(forKey: "nxdn.radioId"), "1234")

        let reloaded = CallSession(station: FakeStation(), userDefaults: defaults)
        XCTAssertEqual(reloaded.nxdnRadioID, "1234")
    }
}
