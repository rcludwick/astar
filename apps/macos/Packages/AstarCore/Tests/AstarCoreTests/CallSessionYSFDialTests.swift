// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// `CallSession` linking System Fusion: a directory name becomes a host and
/// the port that reflector actually listens on, and the receive-only shape of
/// the network is held to.
///
/// Every dial here lands on `FakeStation`. Nothing opens a serial port,
/// nothing resolves a host, and nothing leaves the process — the only way a
/// suite for a hardware-gated network can exist at all.
final class CallSessionYSFDialTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.ysfdial.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    /// Shapes from the published feed: a reflector on the common port, one on
    /// the long tail, and a row with no dial at all.
    private var ysfIndex: ReflectorIndex {
        ReflectorIndex(entries: [
            DirectoryEntry(
                network: .ysf, id: "US-KCWIDE", name: "US-KCWIDE",
                dial: .ysf(host: "ysf.kcwide.example", port: 42000)),
            DirectoryEntry(
                network: .ysf, id: "US-ODDPORT", name: "US-ODDPORT",
                dial: .ysf(host: "odd.example", port: 42003)),
            DirectoryEntry(network: .ysf, id: "US-NODIAL", name: "US-NODIAL", dial: nil),
        ])
    }

    private func session(available: Bool = true) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            ysfAvailable: available)
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.reflectorIndex = ysfIndex
        session.poll()  // adopt `ysfAvailable` from the snapshot
        return (session, fake)
    }

    // MARK: - Dialling by name

    func testAResolvedNameDialsItsPublishedAddress() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        XCTAssertEqual(fake.ysfConnects.count, 1)
        XCTAssertEqual(fake.ysfConnects.first?.host, "ysf.kcwide.example:42000")
        XCTAssertEqual(fake.ysfConnects.first?.callsign, "AJ7HR")
        XCTAssertEqual(session.activeCallNetwork, .ysf)
    }

    /// The port is the field YSF gets wrong if it is assumed. 42000 is the
    /// plurality, not the standard, and the directory is the only thing that
    /// knows which reflector is on the tail.
    func testTheDirectorysPortIsUsedNotTheDefault() throws {
        let (session, fake) = session()
        try session.connect(node: "US-ODDPORT", network: .ysf)
        XCTAssertEqual(fake.ysfConnects.first?.host, "odd.example:42003")
    }

    func testAListedButUndialableRowIsRefusedByName() {
        let (session, fake) = session()
        XCTAssertThrowsError(try session.connect(node: "US-NODIAL", network: .ysf)) { error in
            XCTAssertEqual(
                error as? CallSession.ConnectError, .reflectorNotDialable("US-NODIAL"))
        }
        XCTAssertTrue(fake.ysfConnects.isEmpty)
    }

    // MARK: - Dialling by address

    func testAnUnknownNameFallsThroughToTheAddressGrammar() throws {
        let (session, fake) = session()
        try session.connect(node: "ysf.example:42002", network: .ysf)
        XCTAssertEqual(fake.ysfConnects.first?.host, "ysf.example:42002")
    }

    func testATypedAddressWithNoPortTakesTheCommonOne() throws {
        let (session, fake) = session()
        try session.connect(node: "ysf.example", network: .ysf)
        XCTAssertEqual(fake.ysfConnects.first?.host, "ysf.example:42000")
    }

    /// `XLX836 A` is a D-Star dial. Parsing it as a YSF host by ignoring the
    /// module would link the operator to whatever DNS said `XLX836` was.
    func testATargetWithAModuleIsRefusedRatherThanTruncated() {
        let (session, fake) = session()
        XCTAssertThrowsError(try session.connect(node: "XLX836 A", network: .ysf)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .badYSFTarget)
        }
        XCTAssertTrue(fake.ysfConnects.isEmpty)
    }

    // MARK: - Refusals

    func testDiallingWithNoVocoderIsRefusedBeforeTheEngine() {
        let (session, fake) = session(available: false)
        XCTAssertThrowsError(try session.connect(node: "US-KCWIDE", network: .ysf)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .ysfUnavailable)
        }
        XCTAssertTrue(fake.ysfConnects.isEmpty, "the dongle is never opened")
    }

    func testARefusedDialWithNoCallsignNeverReachesTheDongle() {
        let (session, fake) = session()
        session.operatorCallsign = "  "
        XCTAssertThrowsError(try session.connect(node: "US-KCWIDE", network: .ysf)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingCallsign)
        }
        XCTAssertTrue(fake.ysfConnects.isEmpty)
    }

    // MARK: - Live state

    /// The fields a UI draws, and the one it must not skip. A reflector
    /// sending VW leaves `link` at `linked` and `framesRX` climbing while
    /// producing no sound at all; `unsupportedMode` is the only thing that
    /// explains it.
    func testThePollAdoptsTheLinksOwnState() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -20, rttMS: nil,
            ysfAvailable: true, ysfActive: true)
        fake.ysfStateValue = YSFState(
            link: .linked, lastHeard: "AJ7HR", framesRX: 412, receiving: true,
            unsupportedMode: .voiceFullRate, backend: .thumbdv)
        session.poll()

        XCTAssertEqual(session.ysfLink, .linked)
        XCTAssertEqual(session.ysfLastHeard, "AJ7HR")
        XCTAssertTrue(session.ysfReceiving)
        XCTAssertEqual(session.ysfUnsupportedMode, .voiceFullRate)
    }

    /// A callsign left on screen after the link is gone is a claim about the
    /// present that is no longer true.
    func testTheLastHeardFieldsClearWhenTheLinkGoesAway() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -20, rttMS: nil,
            ysfAvailable: true, ysfActive: true)
        fake.ysfStateValue = YSFState(link: .linked, lastHeard: "AJ7HR", framesRX: 1)
        session.poll()
        XCTAssertEqual(session.ysfLastHeard, "AJ7HR")

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            ysfAvailable: true, ysfActive: false)
        fake.ysfStateValue = nil
        session.poll()

        XCTAssertNil(session.ysfLastHeard)
        XCTAssertNil(session.ysfLink)
        XCTAssertNil(session.ysfUnsupportedMode)
        XCTAssertFalse(session.ysfReceiving)
    }

    // MARK: - Transmit

    /// Fusion transmits now. This test asserted the opposite until the
    /// vendored deframer stopped rejecting half-rate encode replies — kept
    /// rather than deleted, because it is the assertion that has to flip when
    /// a network gains or loses a transmit path.
    func testFusionCanTransmitAndAKeyDownReachesTheStation() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        XCTAssertTrue(session.canTransmit, "Fusion is transceive")

        try session.setPTT(true)
        XCTAssertEqual(fake.pttCalls.last, true, "a key-down must reach the station")
        try session.setPTT(false)
        XCTAssertEqual(fake.pttCalls.last, false)
        try session.disconnect()
    }

    /// Listen-only still wins over a key-down, on Fusion as on every other
    /// network — the setting is the operator's and outranks the request.
    func testListenOnlyStillRefusesAKeyDownOnFusion() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        session.setTxDisabled(true)
        try session.setPTT(true)
        XCTAssertEqual(
            fake.pttCalls.last, false,
            "listen-only must force unkeyed whatever the network")
        try session.disconnect()
    }

    // MARK: - Teardown

    func testHangingUpDisconnectsTheLinkExplicitly() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)

        try session.disconnect()

        XCTAssertEqual(fake.ysfDisconnects, 1)
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.ysfLastHeard)
    }
}
