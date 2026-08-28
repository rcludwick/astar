// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `CallSession` dialling D-Star: a directory name becomes a host, port,
/// module AND the destination reflector's callsign; an incomplete one becomes
/// nothing at all.
///
/// Every dial here lands on `FakeStation`. Nothing opens a serial port,
/// nothing resolves a host, and nothing leaves the process — which is the only
/// way a D-Star suite can exist at all, since the real path opens a vocoder
/// dongle and keys a radio protocol.
final class CallSessionDStarDialTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.dstardial.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    /// Two real shapes from the published feed: XLX836, which the registry
    /// lists at a bare IP under the DExtra callsign `XRF836`, and a row with
    /// no dial at all.
    private var dstarIndex: ReflectorIndex {
        ReflectorIndex(entries: [
            DirectoryEntry(
                network: .dstar, id: "XLX836", name: "XLX836", aliases: ["XRF836"],
                dial: .dextra(
                    host: "45.56.69.219", port: 30001, callsign: "XRF836", modules: [])),
            DirectoryEntry(network: .dstar, id: "XLX000", name: "XLX000", dial: nil),
        ])
    }

    private func session() -> (CallSession, FakeStation) {
        let fake = FakeStation()
        // A dongle is attached, as far as the session can tell — the capability
        // flag is what the picker and the dial both gate on.
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            dstarAvailable: true)
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.reflectorIndex = dstarIndex
        session.poll()  // adopt `dstarAvailable` from the snapshot
        return (session, fake)
    }

    // MARK: - Dialling by name

    /// The whole point of the directory on D-Star: `XLX836 A` is a name, and
    /// the reflector it names lives at an IP address that no amount of parsing
    /// the text could have produced.
    func testAResolvedNameDialsItsPublishedAddress() throws {
        let (session, fake) = session()

        try session.connect(node: "XLX836 A", network: .dstar)

        let call = try XCTUnwrap(fake.dstarConnects.first)
        XCTAssertEqual(call.host, "45.56.69.219", "the host came from the directory, not the text")
        XCTAssertEqual(call.port, 30001)
        XCTAssertEqual(call.module, "A")
        XCTAssertEqual(call.callsign, "AJ7HR", "the operator's callsign, not the room's")
        XCTAssertEqual(session.activeCallNetwork, .dstar)
        XCTAssertEqual(session.dialedNode, "XLX836 A", "the label stays what was typed")
    }

    /// The field that goes out on the air. D-Star puts the destination in the
    /// transmitted header's RPT1/RPT2, and the engine derives it from the
    /// hostname when told nothing — which is impossible here, because this
    /// reflector's published address is a bare IP. Passing the feed's own
    /// callsign is what keeps that header from going out blank.
    func testTheReflectorCallsignComesFromTheFeedNotTheHostname() throws {
        let (session, fake) = session()

        try session.connect(node: "XLX836 A", network: .dstar)

        XCTAssertEqual(fake.dstarConnects.first?.reflectorCallsign, "XRF836")
    }

    /// `XRF836` and `XLX836` are one machine under two names, and an operator
    /// who knows it by the DExtra name should not be told it does not exist.
    func testResolvesTheDExtraAliasAndFoldsTheModule() throws {
        let (session, fake) = session()

        try session.connect(node: "xrf836/b", network: .dstar)

        XCTAssertEqual(fake.dstarConnects.first?.host, "45.56.69.219")
        XCTAssertEqual(fake.dstarConnects.first?.module, "B")
    }

    // MARK: - Resolved but incomplete

    /// On D-Star the module IS the room. A bare name must not dial, because
    /// the failure would not be visible — it would succeed, somewhere.
    func testABareNameRefusesToDialAndNamesWhatItUnderstood() {
        let (session, fake) = session()

        XCTAssertFalse(session.canDial("XLX836", network: .dstar))
        XCTAssertThrowsError(try session.connect(node: "XLX836", network: .dstar)) { error in
            guard case CallSession.ConnectError.needsModule(let name) = error else {
                return XCTFail("expected needsModule, got \(error)")
            }
            XCTAssertEqual(name, "XLX836")
        }
        XCTAssertTrue(fake.dstarConnects.isEmpty, "nothing may reach the dongle")
        XCTAssertNil(session.activeCallNetwork)
    }

    func testAListedButUndialableRowIsRefusedByName() {
        let (session, fake) = session()

        XCTAssertThrowsError(try session.connect(node: "XLX000 A", network: .dstar)) { error in
            guard case CallSession.ConnectError.reflectorNotDialable(let name) = error else {
                return XCTFail("expected reflectorNotDialable, got \(error)")
            }
            XCTAssertEqual(name, "XLX000")
        }
        XCTAssertTrue(fake.dstarConnects.isEmpty)
    }

    // MARK: - The address fallback

    /// A name the directory has never heard of falls through to the address
    /// grammar — and only there. The reflector callsign goes as `nil`, letting
    /// the engine derive RPT1/RPT2 from the hostname: astar knows nothing else
    /// about this target, and a guessed callsign would be transmitted.
    func testAnUnknownNameFallsThroughToTheAddressGrammar() throws {
        let (session, fake) = session()

        try session.connect(node: "xrf757.example.org/C", network: .dstar)

        let call = try XCTUnwrap(fake.dstarConnects.first)
        XCTAssertEqual(call.host, "xrf757.example.org")
        XCTAssertEqual(call.port, 30001, "DExtra's port, supplied because the text omitted one")
        XCTAssertEqual(call.module, "C")
        XCTAssertNil(call.reflectorCallsign, "nothing to derive from but the hostname")
    }

    func testAnExplicitPortIsHonoured() throws {
        let (session, fake) = session()

        try session.connect(node: "10.0.0.5:30051 D", network: .dstar)

        XCTAssertEqual(fake.dstarConnects.first?.port, 30051)
    }

    func testTextThatIsNeitherANameNorAnAddressIsRefused() {
        let (session, fake) = session()

        XCTAssertFalse(session.canDial("nonsense", network: .dstar))
        XCTAssertThrowsError(try session.connect(node: "nonsense", network: .dstar)) { error in
            guard case CallSession.ConnectError.badDStarTarget = error else {
                return XCTFail("expected badDStarTarget, got \(error)")
            }
        }
        XCTAssertTrue(fake.dstarConnects.isEmpty)
    }

    // MARK: - What D-Star requires before it dials

    /// D-Star puts the operator's callsign in every header. It is the same
    /// callsign M17 sends, which is why there is one field for it.
    func testARefusedDialWithNoCallsignNeverReachesTheDongle() {
        let (session, fake) = session()
        session.operatorCallsign = ""

        XCTAssertThrowsError(try session.connect(node: "XLX836 A", network: .dstar)) { error in
            guard case CallSession.ConnectError.missingCallsign = error else {
                return XCTFail("expected missingCallsign, got \(error)")
            }
        }
        XCTAssertTrue(fake.dstarConnects.isEmpty)
    }

    func testTheCallsignIsSharedWithM17AndNotADuplicateField() {
        let (session, _) = session()
        session.m17Callsign = "W1AW"
        XCTAssertEqual(session.operatorCallsign, "W1AW")
        session.operatorCallsign = "K7ABC"
        XCTAssertEqual(session.m17Callsign, "K7ABC")
    }

    /// No dongle, no D-Star — astar has no software AMBE decoder, and saying
    /// so before the engine does is what lets the message mention a dongle.
    func testDiallingWithNoVocoderIsRefusedBeforeTheEngine() {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.reflectorIndex = dstarIndex
        session.poll()
        XCTAssertFalse(session.dstarAvailable)

        XCTAssertThrowsError(try session.connect(node: "XLX836 A", network: .dstar)) { error in
            guard case CallSession.ConnectError.dstarUnavailable = error else {
                return XCTFail("expected dstarUnavailable, got \(error)")
            }
        }
        XCTAssertTrue(fake.dstarConnects.isEmpty)
    }

    // MARK: - Teardown

    func testHangingUpDisconnectsTheDStarSessionExplicitly() throws {
        let (session, fake) = session()
        try session.connect(node: "XLX836 A", network: .dstar)

        try session.disconnect()

        XCTAssertEqual(fake.dstarDisconnects, 1)
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.dstarTalker)
    }
}
