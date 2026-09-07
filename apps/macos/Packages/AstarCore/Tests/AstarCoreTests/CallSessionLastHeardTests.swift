// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// `CallSession.lastHeard` — the ONE property the popover's "Last heard" line
/// reads, whichever digital network is live.
///
/// The point of these tests is that the line is network-agnostic: each
/// network feeds the same property from its own engine state, and a network
/// that carries no talker identity (AllStar) feeds it nothing rather than
/// leaving somebody else's callsign on screen.
///
/// Every dial lands on `FakeStation`. Nothing opens a port, resolves a host,
/// or leaves the process.
final class CallSessionLastHeardTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.lastheard.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private var index: ReflectorIndex {
        ReflectorIndex(entries: [
            DirectoryEntry(
                network: .ysf, id: "US-KCWIDE", name: "US-KCWIDE",
                dial: .ysf(host: "ysf.example", port: 42000)),
            DirectoryEntry(
                network: .dstar, id: "XLX836", name: "XLX836",
                dial: .dextra(
                    host: "45.56.69.219", port: 30_001, callsign: "XRF836", modules: [])),
            DirectoryEntry(
                network: .m17, id: "M17-002", name: "M17-002",
                dial: .m17(
                    host: "89.240.4.99", port: 17_001, callsign: "M17-002", modules: [])),
        ])
    }

    private func session() -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            m17Available: true, dstarAvailable: true, ysfAvailable: true)
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.m17Callsign = "AJ7HR"
        session.reflectorIndex = index
        session.poll()  // adopt the capability flags from the snapshot
        return (session, fake)
    }

    private func live(m17: Bool = false, dstar: Bool = false, ysf: Bool = false) -> CallSnapshot {
        CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -20, rttMS: nil,
            m17Available: true, m17Active: m17,
            dstarAvailable: true, dstarActive: dstar,
            ysfAvailable: true, ysfActive: ysf)
    }

    // MARK: - One property, every network

    /// System Fusion's `last_heard` must reach `lastHeard`, not just
    /// `ysfLastHeard` — the field existed and nothing ever displayed it.
    func testAYSFLastHeardReachesTheSharedProperty() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        fake.snapshotToReturn = live(ysf: true)
        fake.ysfStateValue = YSFState(link: .linked, lastHeard: "K7ABC", framesRX: 12)
        session.poll()

        XCTAssertEqual(session.ysfLastHeard, "K7ABC", "the per-network property still works")
        XCTAssertEqual(session.lastHeard, "K7ABC", "and the shared one follows it")
    }

    func testAnM17TalkerReachesTheSharedProperty() throws {
        let (session, fake) = session()
        try session.connect(node: "M17-002 A", network: .m17)
        fake.snapshotToReturn = live(m17: true)
        fake.m17StateValue = M17State(link: .linked, receiving: true, talker: "N0CALL")
        session.poll()

        XCTAssertEqual(session.m17Talker, "N0CALL")
        XCTAssertEqual(session.m17Link, .linked)
        XCTAssertEqual(session.lastHeard, "N0CALL")
    }

    // D-Star is not exercised here: `DStarState`'s only initializer is the
    // JSON one, internal to `AstarStation`, so a fake cannot hand one back.
    // Its arm of `refreshLastHeard` is the same three lines as the two above,
    // and `CallSessionDStarDialTests` covers `dstarTalker` itself.

    /// The property follows the ACTIVE network, not whichever one happens to
    /// have a callsign cached. A YSF link that ends and an M17 session that
    /// starts must not leave the Fusion callsign on screen.
    func testTheSharedPropertyFollowsTheActiveNetwork() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        fake.snapshotToReturn = live(ysf: true)
        fake.ysfStateValue = YSFState(link: .linked, lastHeard: "K7ABC", framesRX: 1)
        session.poll()
        XCTAssertEqual(session.lastHeard, "K7ABC")

        try session.disconnect()
        try session.connect(node: "M17-002 A", network: .m17)
        fake.snapshotToReturn = live(m17: true)
        fake.ysfStateValue = nil
        fake.m17StateValue = M17State(link: .linked, talker: "N0CALL")
        session.poll()

        XCTAssertEqual(session.activeCallNetwork, .m17)
        XCTAssertEqual(session.lastHeard, "N0CALL", "the M17 talker, not the Fusion one")
    }

    /// AllStar carries no talker identity in its audio path — a node number
    /// is who you dialled, not who is speaking — so the line has nothing to
    /// say and must say nothing.
    func testAllStarHasNoLastHeard() throws {
        let fake = FakeStation()
        let session = CallSession(
            station: fake, hasCredentials: true, userDefaults: scratchDefaults())
        try session.connect(node: "55553", network: .allstar)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: true, txDB: -60, rxDB: -12, rttMS: 30)
        session.poll()

        XCTAssertEqual(session.activeCallNetwork, .allstar)
        XCTAssertNil(session.lastHeard)
    }

    // MARK: - Clearing

    /// A callsign left on screen after the session is gone is a claim about
    /// the present that is no longer true — on every network.
    func testTheSharedPropertyClearsWhenTheSessionEnds() throws {
        let (session, fake) = session()
        try session.connect(node: "M17-002 A", network: .m17)
        fake.snapshotToReturn = live(m17: true)
        fake.m17StateValue = M17State(link: .linked, talker: "N0CALL")
        session.poll()
        XCTAssertEqual(session.lastHeard, "N0CALL")

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            m17Available: true, m17Active: false)
        fake.m17StateValue = nil
        session.poll()

        XCTAssertNil(session.m17Talker, "the per-network field clears")
        XCTAssertNil(session.m17Link)
        XCTAssertNil(session.lastHeard, "and so does the shared one")
    }

    func testAnExplicitDisconnectClearsIt() throws {
        let (session, fake) = session()
        try session.connect(node: "US-KCWIDE", network: .ysf)
        fake.snapshotToReturn = live(ysf: true)
        fake.ysfStateValue = YSFState(link: .linked, lastHeard: "K7ABC", framesRX: 1)
        session.poll()
        XCTAssertEqual(session.lastHeard, "K7ABC")

        try session.disconnect()

        XCTAssertNil(session.lastHeard, "a user-initiated hangup clears it synchronously")
        XCTAssertNil(session.activeCallNetwork)
    }

    // MARK: - The gate the popover uses

    /// The popover shows the line when `activeCallNetwork.isDigitalVoice`, so
    /// this is the list of networks that get one. A network added later opts
    /// in here and inherits the line with no UI change.
    func testOnlyDigitalVoiceNetworksClaimALastHeardLine() {
        XCTAssertTrue(Network.m17.isDigitalVoice)
        XCTAssertTrue(Network.dstar.isDigitalVoice)
        XCTAssertTrue(Network.ysf.isDigitalVoice)
        XCTAssertFalse(Network.allstar.isDigitalVoice, "IAX2 carries no talker identity")
        XCTAssertFalse(Network.hamlink.isDigitalVoice, "no engine link at all yet")
    }
}
