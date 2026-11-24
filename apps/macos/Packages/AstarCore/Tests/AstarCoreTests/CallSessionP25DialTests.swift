// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// `CallSession` linking P25: a directory row becomes a host and a port, the
/// talkgroup stays a label on this side, and the receive-only shape of the
/// network is held to at the key choke point rather than only in the UI.
///
/// Every dial here lands on `FakeStation`. Nothing resolves a host and
/// nothing leaves the process.
final class CallSessionP25DialTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.p25dial.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    /// Shapes from the published feed: the id is usually the talkgroup, as a
    /// string — and sometimes a name, as for a parrot.
    private var p25Entries: [DirectoryEntry] {
        [
            DirectoryEntry(
                network: .p25, id: "10200", name: "P25 10200",
                dial: .p25(host: "p25.example", port: 41000)),
            DirectoryEntry(
                network: .p25, id: "PARROT", name: "P25 Parrot",
                dial: .p25(host: "parrot.example", port: 41001)),
            DirectoryEntry(network: .p25, id: "31", name: "P25 31", dial: nil),
        ]
    }

    private var p25Index: ReflectorIndex { ReflectorIndex(entries: p25Entries) }

    private func snapshot(
        status: IaxStatus = .idle, available: Bool = true, active: Bool = false,
        rxDB: Float = -60
    ) -> CallSnapshot {
        CallSnapshot(
            status: status, ptt: false, remotePTT: false, txDB: -60, rxDB: rxDB, rttMS: nil,
            p25Available: available, p25Active: active)
    }

    private func session(available: Bool = true) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = snapshot(available: available)
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.reflectorIndex = p25Index
        session.poll()  // adopt `p25Available` from the snapshot
        return (session, fake)
    }

    // MARK: - Dialling

    func testAResolvedRowDialsItsAddressAndShowsItsTalkgroup() throws {
        let (session, fake) = session()
        XCTAssertTrue(session.canDial("10200", network: .p25))
        try session.connect(node: "10200", network: .p25)
        XCTAssertEqual(fake.connectP25Calls.count, 1)
        XCTAssertEqual(fake.connectP25Calls.first?.host, "p25.example:41000")
        XCTAssertEqual(fake.connectP25Calls.first?.callsign, "AJ7HR")
        XCTAssertEqual(session.p25Talkgroup, 10200, "kept for display only")
        XCTAssertEqual(session.activeCallNetwork, .p25)
    }

    /// Review Focus 5: the talkgroup is only a label, so a row whose id is a
    /// name still dials.
    func testANonNumericP25RowDialsWithNoTalkgroupLabel() throws {
        let (session, fake) = session()
        XCTAssertTrue(session.canDial("PARROT", network: .p25))
        try session.connect(node: "PARROT", network: .p25)
        XCTAssertEqual(fake.connectP25Calls.first?.host, "parrot.example:41001")
        XCTAssertNil(session.p25Talkgroup)
    }

    func testATypedAddressWithNoTalkgroupIsDialable() throws {
        let (session, fake) = session()
        XCTAssertTrue(session.canDial("other.example", network: .p25))
        try session.connect(node: "other.example", network: .p25)
        XCTAssertEqual(fake.connectP25Calls.first?.host, "other.example:41000")
        XCTAssertNil(session.p25Talkgroup)

        try session.disconnect()
        try session.connect(node: "other.example:41002/777", network: .p25)
        XCTAssertEqual(fake.connectP25Calls.last?.host, "other.example:41002")
        XCTAssertEqual(session.p25Talkgroup, 777)
    }

    func testABadAddressIsRefusedWithItsMessage() {
        let (session, fake) = session()
        XCTAssertFalse(session.canDial("XLX836 A", network: .p25))
        XCTAssertThrowsError(try session.connect(node: "XLX836 A", network: .p25)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .badP25Target)
            XCTAssertEqual(
                connectFailureMessage(for: error, node: "XLX836 A"),
                "Enter a P25 talkgroup from the directory, or an address as host:port "
                    + "(for example p25.example:41000).")
        }
        XCTAssertTrue(fake.connectP25Calls.isEmpty)
    }

    func testAListedButUndialableRowIsRefusedByName() {
        let (session, fake) = session()
        XCTAssertThrowsError(try session.connect(node: "31", network: .p25)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .reflectorNotDialable("31"))
        }
        XCTAssertTrue(fake.connectP25Calls.isEmpty)
    }

    // MARK: - Refusals

    func testDiallingWithoutP25InTheBuildIsRefusedBeforeTheEngine() {
        let (session, fake) = session(available: false)
        XCTAssertFalse(session.canDial("10200", network: .p25), "Connect stays off")
        XCTAssertThrowsError(try session.connect(node: "10200", network: .p25)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .p25Unavailable)
            XCTAssertEqual(
                connectFailureMessage(for: error, node: "10200"),
                "This build of astar doesn't include P25.")
        }
        XCTAssertTrue(fake.connectP25Calls.isEmpty)
    }

    /// Listed, not dialable, with the honest reason — in a build without P25.
    func testDirectoryRowsAreRefusedWithTheReasonWithoutP25() throws {
        let entry = try XCTUnwrap(p25Entries.first)
        let (without, _) = session(available: false)
        XCTAssertEqual(
            without.directoryRefusal(for: entry), "This build of astar doesn't include P25.")
        let (with, _) = session()
        XCTAssertNil(with.directoryRefusal(for: entry))
    }

    func testARefusedDialWithNoCallsignNeverReachesTheEngine() {
        let (session, fake) = session()
        session.operatorCallsign = "  "
        XCTAssertThrowsError(try session.connect(node: "10200", network: .p25)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingCallsign)
        }
        XCTAssertTrue(fake.connectP25Calls.isEmpty)
        XCTAssertTrue(CallSession.requiresCallsign(.p25))
    }

    func testTheEnginesP25ErrorDetailIsShownVerbatim() {
        let (session, fake) = session()
        fake.p25ConnectError = StationError(
            code: -28, text: "p25 error",
            detail: "p25 error: callsign longer than 10 characters")
        XCTAssertThrowsError(try session.connect(node: "10200", network: .p25)) { error in
            let message = connectFailureMessage(for: error, node: "10200")
            XCTAssertTrue(message.contains("callsign longer than 10 characters"), message)
            XCTAssertFalse(message.contains("-28"), "an error code is not a diagnosis")
        }
        XCTAssertNil(session.dialedNode, "a failed dial leaves nothing dialled")
        XCTAssertNil(session.activeCallNetwork)
    }

    // MARK: - Live state

    func testThePollAdoptsTheLinksOwnState() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true, rxDB: -20)
        fake.p25StateValue = P25State(
            link: .linked, reflector: "p25.example:41000", lastHeard: "1234567",
            lastHeardID: 1_234_567, talkgroup: 10200, framesRx: 90, receiving: true)
        session.poll()

        XCTAssertEqual(session.p25Link, .linked)
        XCTAssertEqual(session.p25LastHeard, "1234567")
        XCTAssertTrue(session.p25Receiving)
        XCTAssertFalse(session.p25Encrypted)
        XCTAssertEqual(session.lastHeard, "1234567")
        XCTAssertNil(session.lastHeardNote)
    }

    func testAnEncryptedOverSetsTheLastHeardNote() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(
            link: .linked, lastHeard: "1234567", lastHeardID: 1_234_567, receiving: true,
            encrypted: true)
        session.poll()
        XCTAssertTrue(session.p25Encrypted)
        XCTAssertEqual(session.lastHeard, "1234567")
        XCTAssertEqual(session.lastHeardNote, "Encrypted")

        // The next over is in the clear.
        fake.p25StateValue = P25State(
            link: .linked, lastHeard: "7654321", lastHeardID: 7_654_321, receiving: true)
        session.poll()
        XCTAssertEqual(session.lastHeard, "7654321")
        XCTAssertNil(session.lastHeardNote)
    }

    /// The engine clears `encrypted` when the over ends; the note stays on
    /// the talker line until the next over starts, so a silent line still
    /// says why nothing was heard.
    func testTheEncryptedNoteOutlivesTheOverUntilTheNextOneStarts() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(
            link: .linked, lastHeard: "1234567", receiving: true, encrypted: true)
        session.poll()
        XCTAssertEqual(session.lastHeardNote, "Encrypted")

        // The over ends: the engine drops both flags.
        fake.p25StateValue = P25State(link: .linked, lastHeard: "1234567")
        session.poll()
        XCTAssertFalse(session.p25Encrypted, "the engine's flag is the over in progress")
        XCTAssertEqual(session.lastHeardNote, "Encrypted", "kept until the next over")

        // The next over starts, in the clear.
        fake.p25StateValue = P25State(link: .linked, lastHeard: "1234567", receiving: true)
        session.poll()
        XCTAssertNil(session.lastHeardNote)
    }

    /// An encrypted over is keyed but muted. Nothing plays, so nothing may
    /// look as if it does: the RX meter and the menu-bar star read
    /// `receiving`.
    func testAnEncryptedOverIsNotReceiving() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: true, txDB: -60, rxDB: -60, rttMS: nil,
            p25Available: true, p25Active: true)
        fake.p25StateValue = P25State(
            link: .linked, lastHeard: "1234567", receiving: true, encrypted: true)
        session.poll()
        XCTAssertTrue(session.remotePTT, "someone is keyed")
        XCTAssertFalse(session.receiving, "but nothing is playing")

        fake.p25StateValue = P25State(link: .linked, lastHeard: "7654321", receiving: true)
        session.poll()
        XCTAssertTrue(session.receiving, "a clear over lights up as usual")
    }

    /// Final review finding 8: the PTT source hook (the serial interface)
    /// gets the same encrypted-aware flag as `receiving`. A muted encrypted
    /// over must not look keyed to it, or a relay mode could key the local
    /// radio with dead air.
    func testThePTTSourceTickDoesNotSeeAnEncryptedOverAsKeyed() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        var seen: [Bool] = []
        session.pttSourceTick = { remote, _ in
            seen.append(remote)
            return nil
        }
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: true, txDB: -60, rxDB: -60, rttMS: nil,
            p25Available: true, p25Active: true)
        fake.p25StateValue = P25State(
            link: .linked, lastHeard: "1234567", receiving: true, encrypted: true)
        session.poll()
        XCTAssertEqual(seen, [false], "an encrypted over is not keyed to the PTT source")
        XCTAssertFalse(session.receiving)
        XCTAssertFalse(
            fake.keyingLog.contains { $0.hasPrefix("pttSource(serial,true)") },
            "nothing keyed: \(fake.keyingLog)")

        // The control: a clear over reaches the hook as keyed.
        fake.p25StateValue = P25State(link: .linked, lastHeard: "7654321", receiving: true)
        session.poll()
        XCTAssertEqual(seen, [false, true], "a clear over still reads as keyed")
    }

    func testTheLastHeardFieldsClearWhenTheLinkGoesAway() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(
            link: .linked, lastHeard: "1234567", lastHeardID: 1_234_567, encrypted: true)
        session.poll()
        XCTAssertEqual(session.p25LastHeard, "1234567")

        fake.snapshotToReturn = snapshot(status: .answered, active: false)
        fake.p25StateValue = nil
        session.poll()

        XCTAssertNil(session.p25LastHeard)
        XCTAssertNil(session.p25Link)
        XCTAssertFalse(session.p25Receiving)
        XCTAssertFalse(session.p25Encrypted)
        XCTAssertNil(session.lastHeard)
        XCTAssertNil(session.lastHeardNote)
    }

    /// Task 7 review finding #1: the heard list reads `p25Active`.
    func testTheHeardListFollowsAP25Link() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(link: .linked, lastHeard: "1234567")
        fake.heardValue = [
            HeardEntry(callsign: "1234567", network: "p25", ageMs: 1_500),
            HeardEntry(callsign: "AJ7HR", network: "m17", ageMs: 2_000),
        ]
        session.poll()
        XCTAssertEqual(session.heardHistory.map(\.callsign), ["1234567"])
    }

    // MARK: - A lost link

    /// A link that timed out or hit a receive error says so, is torn down,
    /// and leaves its target to dial again — never a "Dialing" that hangs.
    func testAFailedLinkIsReportedLostAndReleased() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(link: .linked, lastHeard: "1234567")
        session.poll()

        fake.snapshotToReturn = snapshot(status: .hangup, active: true)
        fake.p25StateValue = P25State(link: .failed, lastHeard: "1234567")
        session.poll()

        XCTAssertEqual(session.p25LostTarget, "10200")
        XCTAssertEqual(session.lastDialFailure, CallSession.p25LinkLostMessage)
        XCTAssertEqual(fake.p25Disconnects, 1, "the dead link is released")
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.lastHeard)

        // The next dial — on any network — clears it.
        fake.snapshotToReturn = snapshot()
        try session.connect(node: "10200", network: .p25)
        XCTAssertNil(session.p25LostTarget)
        XCTAssertNil(session.lastDialFailure)
    }

    /// A link that never came up and then failed is lost too, not "didn't
    /// answer".
    func testALinkThatFailsWhileLinkingIsLostNotUnanswered() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .dialing, active: true)
        fake.p25StateValue = P25State(link: .linking)
        session.poll()

        fake.snapshotToReturn = snapshot(status: .hangup, active: true)
        fake.p25StateValue = P25State(link: .failed)
        session.poll()

        XCTAssertEqual(session.p25LostTarget, "10200")
        XCTAssertEqual(session.lastDialFailure, CallSession.p25LinkLostMessage)
    }

    func testAnOrdinaryHangupIsNotALostLink() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(link: .linked)
        session.poll()
        try session.disconnect()
        fake.snapshotToReturn = snapshot(status: .hangup)
        fake.p25StateValue = nil
        session.poll()
        XCTAssertNil(session.p25LostTarget)
        XCTAssertNil(session.lastDialFailure)
    }

    /// The edge itself, reached without `disconnect()`: a link that goes away
    /// while still `.linked` is a call that ended, not a lost link.
    func testAHangupWhileLinkedIsNotALostLink() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(link: .linked, lastHeard: "1234567")
        session.poll()

        fake.snapshotToReturn = snapshot(status: .hangup, active: true)
        session.poll()

        XCTAssertNil(session.activeCallNetwork, "the edge ran")
        XCTAssertNil(session.p25LostTarget)
        XCTAssertNil(session.lastDialFailure)
        XCTAssertEqual(fake.p25Disconnects, 0, "only a failed link is released here")
    }

    /// Moving to another network dismisses a lost link: the title, the line
    /// and the Reconnect row all read `p25LostTarget`.
    func testDismissingALostLinkClearsItAndItsLine() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        fake.snapshotToReturn = snapshot(status: .answered, active: true)
        fake.p25StateValue = P25State(link: .linked)
        session.poll()
        fake.snapshotToReturn = snapshot(status: .hangup, active: true)
        fake.p25StateValue = P25State(link: .failed)
        session.poll()
        XCTAssertEqual(session.p25LostTarget, "10200")

        session.dismissP25LinkLost()
        XCTAssertNil(session.p25LostTarget)
        XCTAssertNil(session.lastDialFailure)
    }

    // MARK: - Transmit

    func testP25CannotTransmit() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)
        XCTAssertFalse(session.canTransmit)

        try session.setPTT(true)
        XCTAssertTrue(session.transmitBlocked)
        XCTAssertEqual(
            fake.keyingLog, ["pttSource(manual,false)"], "a key-down must never reach the station")
    }

    // MARK: - Teardown

    func testHangingUpDisconnectsTheLinkExplicitly() throws {
        let (session, fake) = session()
        try session.connect(node: "10200", network: .p25)

        try session.disconnect()

        XCTAssertEqual(fake.p25Disconnects, 1)
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.p25LastHeard)
        XCTAssertNil(session.p25Talkgroup)
        XCTAssertTrue(session.canTransmit, "the gate lifts with the network")
    }
}
