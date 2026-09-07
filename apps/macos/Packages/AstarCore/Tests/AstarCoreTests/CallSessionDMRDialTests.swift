// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// `CallSession` linking DMR: four things reach the engine (the network, the
/// master, the talkgroup and the slot) plus two credentials (the radio ID and
/// the master password), and every one of them is refused before the dongle
/// is opened when it is missing.
///
/// Every dial here lands on `FakeStation`. Nothing opens a serial port,
/// nothing resolves a host, and nothing leaves the process — no test in this
/// file may ever reach a real master.
final class CallSessionDMRDialTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.dmrdial.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    /// Shapes from the published feed: the id is `<system>-server-<name>`,
    /// the dial carries the system verbatim, and 30 of 185 rows have none.
    private var dmrIndex: ReflectorIndex {
        ReflectorIndex(entries: [
            DirectoryEntry(
                network: .dmr, id: "freedmr-network-server-freedmr-eu", name: "FreeDMR EU",
                dial: .mmdvm(system: "freedmr-network", host: "eu.example", port: 62031)),
            DirectoryEntry(
                network: .dmr, id: "ipsc2-poland-server-ipsc2-poland", name: "IPSC2 Poland",
                dial: .mmdvm(system: "ipsc2-poland", host: "pl.example", port: 55555)),
            DirectoryEntry(
                network: .dmr, id: "adn-systems-chile-server-adn", name: "ADN Chile", dial: nil),
        ])
    }

    private func store(password: String? = "hunter2", system: String = "freedmr-network")
        -> InMemoryCredentialStore
    {
        let passwords = password.map { [system: $0] } ?? [:]
        return InMemoryCredentialStore(
            Credentials(
                portalUser: "AJ7HR", portalPass: "portal", portalNode: "12345",
                dmrPasswords: passwords))
    }

    private func session(
        available: Bool = true, radioID: String = "3153591",
        credentials: InMemoryCredentialStore? = nil
    ) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            dmrAvailable: available)
        let session = CallSession(
            station: fake, credentialStore: credentials ?? store(),
            userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.dmrRadioID = radioID
        session.reflectorIndex = dmrIndex
        session.poll()  // adopt `dmrAvailable` from the snapshot
        return (session, fake)
    }

    // MARK: - Dialling by directory row

    /// The system comes from the row's own `dial`, never from the row id and
    /// never from a guess: the login is per network, and a near-miss would put
    /// the operator on the wrong system under their own registration.
    func testAResolvedRowSuppliesTheSystemTheMasterAndThePort() throws {
        let (session, fake) = session()
        try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        XCTAssertEqual(fake.connectDMRCalls.count, 1)
        let call = try XCTUnwrap(fake.connectDMRCalls.first)
        XCTAssertEqual(call.system, "freedmr-network")
        XCTAssertEqual(call.host, "eu.example")
        XCTAssertEqual(call.port, 62031)
        XCTAssertEqual(call.radioID, 3_153_591)
        XCTAssertEqual(call.callsign, "AJ7HR")
        XCTAssertEqual(call.talkgroup, 91)
        XCTAssertEqual(call.timeslot, 2)
        XCTAssertEqual(call.password, "hunter2")
        XCTAssertEqual(session.activeCallNetwork, .dmr)
    }

    func testTheDirectorysPortIsUsedNotTheDefault() throws {
        let (session, fake) = session(credentials: store(system: "ipsc2-poland"))
        try session.connect(node: "ipsc2-poland-server-ipsc2-poland/260/1", network: .dmr)
        XCTAssertEqual(fake.connectDMRCalls.first?.port, 55555)
        XCTAssertEqual(fake.connectDMRCalls.first?.timeslot, 1)
    }

    /// A row is a master, not a room. Without a talkgroup there is nothing to
    /// join, and TS2 is a default only for the slot — never for the number.
    func testAResolvedRowWithNoTalkgroupIsRefused() {
        let (session, fake) = session()
        XCTAssertFalse(
            session.canDial("freedmr-network-server-freedmr-eu", network: .dmr),
            "Connect stays off")
        XCTAssertThrowsError(
            try session.connect(node: "freedmr-network-server-freedmr-eu", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .badDMRTarget)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)
    }

    func testAListedButUndialableRowIsRefusedByName() {
        let (session, fake) = session()
        XCTAssertThrowsError(
            try session.connect(node: "adn-systems-chile-server-adn/91/2", network: .dmr)
        ) { error in
            XCTAssertEqual(
                error as? CallSession.ConnectError,
                .reflectorNotDialable("adn-systems-chile-server-adn"))
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)
    }

    // MARK: - Dialling by address

    /// TGIF is not in the directory — no row's `system` contains `tgif` —
    /// so astar's first and recommended target is reachable only by typing an
    /// address. That is the reason the manual form exists.
    func testATypedAddressCarriesItsOwnSystem() throws {
        let (session, fake) = session(credentials: store(system: "tgif"))
        try session.connect(node: "tgif:tgif.network:62031/31313/2", network: .dmr)
        let call = try XCTUnwrap(fake.connectDMRCalls.first)
        XCTAssertEqual(call.system, "tgif")
        XCTAssertEqual(call.host, "tgif.network")
        XCTAssertEqual(call.talkgroup, 31313)
    }

    /// An address with no system names no network, so there is no password to
    /// look up and no login to attempt. Refused rather than guessed at.
    func testATypedAddressWithNoSystemIsRefused() {
        let (session, fake) = session()
        XCTAssertFalse(session.canDial("other.example/91/2", network: .dmr))
        XCTAssertThrowsError(try session.connect(node: "other.example/91/2", network: .dmr)) {
            error in
            XCTAssertEqual(error as? CallSession.ConnectError, .badDMRTarget)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)
    }

    func testATargetWithAModuleIsRefusedRatherThanTruncated() {
        let (session, fake) = session()
        XCTAssertThrowsError(try session.connect(node: "XLX836 A", network: .dmr)) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .badDMRTarget)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)
    }

    // MARK: - Refusals

    func testDiallingDMRWithNoRadioIDIsRefused() {
        let (session, fake) = session(radioID: "")
        XCTAssertThrowsError(
            try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingDMRRadioID)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty, "nothing reached the engine")
    }

    /// A 24-bit field holds a 7-digit registration, so this CONVERTS where
    /// NXDN refuses — and refuses only what is not a registration at all.
    func testASevenDigitRegistrationFitsAndAWiderOneDoesNot() throws {
        let (wide, wideFake) = session(radioID: "315359100")
        let (fits, fitsFake) = session(radioID: "3153591")
        try fits.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        XCTAssertEqual(fitsFake.connectDMRCalls.first?.radioID, 3_153_591)

        XCTAssertThrowsError(
            try wide.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .dmrRadioIDOutOfRange)
        }
        XCTAssertTrue(wideFake.connectDMRCalls.isEmpty)
    }

    func testDiallingDMRWithNoPasswordIsRefused() {
        // Every dialable row in the directory says `requires: ["dmr_id",
        // "password"]`. Refusing here, with a message that says where to get
        // one, beats an MSTNAK the operator cannot interpret.
        let (session, fake) = session(credentials: store(password: nil))
        XCTAssertThrowsError(
            try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingDMRPassword)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)
    }

    /// Each DMR network issues its own password, so one saved for FreeDMR is
    /// not a password for DMR+.
    func testAPasswordForAnotherNetworkIsNotThisNetworksPassword() {
        let (session, fake) = session(credentials: store(system: "freedmr-network"))
        XCTAssertThrowsError(
            try session.connect(node: "ipsc2-poland-server-ipsc2-poland/260/1", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingDMRPassword)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)
    }

    func testDiallingWithNoVocoderIsRefusedBeforeTheEngine() {
        let (session, fake) = session(available: false)
        XCTAssertThrowsError(
            try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .dmrUnavailable)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty, "the dongle is never opened")
    }

    func testARefusedDialWithNoCallsignNeverReachesTheDongle() {
        let (session, fake) = session()
        session.operatorCallsign = "  "
        XCTAssertThrowsError(
            try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .missingCallsign)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)
    }

    // MARK: - The consent gate

    func testBrandmeisterIsRefusedUntilTheOperatorHasTickedTheBox() throws {
        // docs/design/dmr-networks.md: off by default, never pre-ticked, and
        // never inferred from anything else.
        let (session, fake) = session(credentials: store(system: "brandmeister"))
        XCTAssertFalse(session.brandmeisterConsent, "the gate is shut by default")
        XCTAssertThrowsError(
            try session.connect(node: "brandmeister:bm.example/91/2", network: .dmr)
        ) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .brandmeisterNotConsented)
        }
        XCTAssertTrue(fake.connectDMRCalls.isEmpty)

        session.brandmeisterConsent = true
        XCTAssertNoThrow(try session.connect(node: "brandmeister:bm.example/91/2", network: .dmr))
        XCTAssertEqual(fake.connectDMRCalls.count, 1)
    }

    /// The gate is one explicit opt-in and it persists — an operator who read
    /// the terms should not be asked again at every launch, and one who never
    /// did must never find it already ticked.
    func testTheConsentIsPersistedAndDefaultsOff() {
        let defaults = scratchDefaults()
        let first = CallSession(station: FakeStation(), userDefaults: defaults)
        XCTAssertFalse(first.brandmeisterConsent)
        first.brandmeisterConsent = true
        XCTAssertEqual(defaults.object(forKey: "dmr.brandmeisterConsent") as? Bool, true)
        XCTAssertTrue(
            CallSession(station: FakeStation(), userDefaults: defaults).brandmeisterConsent)
    }

    // MARK: - The password is a credential

    func testTheDMRPasswordNeverReachesAPublishedProperty() {
        // CLAUDE.md: secrets are connect-time in-args only. CallSession is
        // observed by SwiftUI and its @Published values end up in view
        // diagnostics; a password among them would be one screenshot away
        // from a support thread.
        let mirror = Mirror(
            reflecting: CallSession(station: NullStation(), userDefaults: scratchDefaults()))
        for child in mirror.children {
            let label = (child.label ?? "").lowercased()
            XCTAssertFalse(label.contains("password"), "CallSession exposes \(label)")
            XCTAssertFalse(label.contains("passphrase"), "CallSession exposes \(label)")
        }
    }

    /// The Keychain is the only home. A password must not ride out in a
    /// `.astarconfig` an operator hands to a friend.
    func testARoundTrippedConfigArchiveCarriesNoDMRPassword() throws {
        let archive = ConfigArchive.make(
            sections: Set(ConfigSection.allCases),
            from: ConfigArchive.Sources(
                defaults: [
                    "dmr.brandmeisterConsent": true, "audio.inputGain": 1.0,
                    "dmr.password.tgif": "hunter2",
                ],
                setups: [], micProfiles: [], selectedSetupID: nil, defaultSetupID: nil,
                directory: [], callsign: "AJ7HR", radioID: "3153591"))
        let data = try JSONEncoder().encode(archive)
        let text = String(decoding: data, as: UTF8.self)
        XCTAssertFalse(text.contains("hunter2"), "a secret reached an exported config")
        XCTAssertFalse(text.lowercased().contains("password"))
    }

    // MARK: - Live state

    func testThePollAdoptsTheLinksOwnState() throws {
        let (session, fake) = session()
        try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -20, rttMS: nil,
            dmrAvailable: true, dmrActive: true)
        fake.dmrStateValue = DMRState(
            link: .linked, lastHeard: "3153591", lastHeardID: 3_153_591, talkgroup: 91,
            timeslot: 2, framesRX: 412, receiving: true, backend: .thumbdv)
        session.poll()

        XCTAssertEqual(session.dmrLink, .linked)
        XCTAssertEqual(session.dmrLastHeard, "3153591")
        XCTAssertTrue(session.dmrReceiving)
        // The one line every digital network shares.
        XCTAssertEqual(session.lastHeard, "3153591")
    }

    func testTheLastHeardFieldsClearWhenTheLinkGoesAway() throws {
        let (session, fake) = session()
        try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -20, rttMS: nil,
            dmrAvailable: true, dmrActive: true)
        fake.dmrStateValue = DMRState(link: .linked, lastHeard: "3153591", framesRX: 1)
        session.poll()
        XCTAssertEqual(session.dmrLastHeard, "3153591")

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            dmrAvailable: true, dmrActive: false)
        fake.dmrStateValue = nil
        session.poll()

        XCTAssertNil(session.dmrLastHeard)
        XCTAssertNil(session.dmrLink)
        XCTAssertFalse(session.dmrReceiving)
        XCTAssertNil(session.lastHeard)
    }

    // MARK: - Transmit

    func testDMRCannotTransmit() throws {
        // Receive-first gate: the PTT control and the key choke point are the
        // two places that must never disagree.
        let (session, fake) = session()
        try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)
        XCTAssertFalse(session.canTransmit)

        try session.setPTT(true)
        XCTAssertEqual(fake.pttCalls.last, false, "a key-down must never reach the station")
    }

    // MARK: - Teardown

    func testHangingUpDisconnectsTheLinkExplicitly() throws {
        let (session, fake) = session()
        try session.connect(node: "freedmr-network-server-freedmr-eu/91/2", network: .dmr)

        try session.disconnect()

        XCTAssertEqual(fake.dmrDisconnects, 1)
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.dmrLastHeard)
        XCTAssertTrue(session.canTransmit, "the gate lifts with the network")
    }
}
