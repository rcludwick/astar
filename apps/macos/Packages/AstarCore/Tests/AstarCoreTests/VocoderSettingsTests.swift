// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// A scripted stand-in for the engine's vocoder listing and enrolment.
///
/// **Nothing here touches a serial port.** The real enroller opens the port it
/// is handed, and on this desk that could be a radio's CAT/PTT cable — so the
/// view-model is only ever tested against this fake, and every serial is made
/// up.
final class FakeVocoderEnroller: VocoderEnrolling, @unchecked Sendable {
    private let lock = NSLock()
    private var _candidates: [VocoderCandidate] = []
    private var _outcomes: [String: VocoderEnrolment] = [:]
    private var _enrolCalls: [String] = []
    private var _listCalls = 0

    var candidatesToReturn: [VocoderCandidate] {
        get { lock.withLock { _candidates } }
        set { lock.withLock { _candidates = newValue } }
    }
    var enrolCalls: [String] { lock.withLock { _enrolCalls } }
    var listCalls: Int { lock.withLock { _listCalls } }

    func script(_ serial: String, _ outcome: VocoderEnrolment) {
        lock.withLock { _outcomes[serial] = outcome }
    }

    func candidates() -> [VocoderCandidate] {
        lock.withLock {
            _listCalls += 1
            return _candidates
        }
    }

    func enrol(serial: String) throws -> VocoderEnrolment {
        lock.withLock {
            _enrolCalls.append(serial)
            return _outcomes[serial]
                ?? VocoderEnrolment(
                    result: .notFound, serial: serial, port: nil, prodid: nil, model: nil,
                    message: "No vocoder-class USB device with serial \(serial) is attached.")
        }
    }
}

final class InMemoryVocoderTrustStore: VocoderTrustStore {
    var stored: [TrustedVocoder] = []
    private(set) var saves = 0
    func load() -> [TrustedVocoder] { stored }
    func save(_ vocoders: [TrustedVocoder]) {
        stored = vocoders
        saves += 1
    }
}

enum VocoderFixtures {
    static let stick = VocoderCandidate(serial: "TESTSER1", port: "/dev/cu.usbserial-TESTSER1")
    static let stick33 = VocoderCandidate(serial: "TESTSER3", port: "/dev/cu.usbserial-TESTSER3")
    static let catCable = VocoderCandidate(serial: "CATCABLE", port: "/dev/cu.usbserial-CATCABLE")

    static func trusted(_ c: VocoderCandidate) -> VocoderEnrolment {
        VocoderEnrolment(
            result: .trusted, serial: c.serial, port: c.port, prodid: "AMBE3000R",
            model: "DVstick 30 / ThumbDV-class",
            message: "Added: DVstick 30 / ThumbDV-class (AMBE3000R).")
    }

    static func unsupported(_ c: VocoderCandidate) -> VocoderEnrolment {
        VocoderEnrolment(
            result: .unsupported, serial: c.serial, port: c.port, prodid: "AMBE3003F",
            model: "DVstick 33 / AMBE-3003",
            message:
                "This is a DVstick 33 / AMBE-3003 (AMBE3003F). It is recognised but not "
                + "supported yet, so it was not added.")
    }

    static func noAnswer(_ c: VocoderCandidate) -> VocoderEnrolment {
        VocoderEnrolment(
            result: .noAnswer, serial: c.serial, port: c.port, prodid: nil, model: nil,
            message:
                "Nothing at \(c.port) answered as a vocoder. It may be a radio's CAT/PTT "
                + "cable; it was not added.")
    }
}

@MainActor
final class VocoderSettingsTests: XCTestCase {

    private var store: InMemoryVocoderTrustStore!
    private var enroller: FakeVocoderEnroller!
    private var pushed: [[String]] = []
    private var clock = Date(timeIntervalSince1970: 1_800_000_000)

    override func setUp() {
        super.setUp()
        store = InMemoryVocoderTrustStore()
        enroller = FakeVocoderEnroller()
        pushed = []
    }

    private func model() -> VocoderSettings {
        VocoderSettings(
            store: store, enroller: enroller, now: { [unowned self] in self.clock },
            onTrustChanged: { [unowned self] in self.pushed.append($0) })
    }

    // MARK: - Nothing opens on its own

    func testConstructingAndRefreshingNeverEnrols() {
        enroller.candidatesToReturn = [VocoderFixtures.stick, VocoderFixtures.catCable]
        let vm = model()
        vm.refresh()
        vm.refresh()
        XCTAssertEqual(enroller.enrolCalls, [], "listing must never open a port")
        XCTAssertEqual(vm.candidates.count, 2)
    }

    func testAFreshMacTrustsNothing() {
        let vm = model()
        XCTAssertTrue(vm.trusted.isEmpty)
        XCTAssertEqual(vm.serials, [])
    }

    // MARK: - Adding

    func testAnAMBE3000IsTrustedPersistedAndPushed() async {
        enroller.candidatesToReturn = [VocoderFixtures.stick]
        enroller.script("TESTSER1", VocoderFixtures.trusted(VocoderFixtures.stick))
        let vm = model()
        vm.refresh()

        let outcome = await vm.enrol(VocoderFixtures.stick)

        XCTAssertEqual(outcome?.result, .trusted)
        XCTAssertEqual(enroller.enrolCalls, ["TESTSER1"])
        XCTAssertEqual(vm.serials, ["TESTSER1"])
        XCTAssertEqual(store.stored.map(\.serial), ["TESTSER1"])
        XCTAssertEqual(store.stored.first?.prodid, "AMBE3000R")
        XCTAssertEqual(store.stored.first?.model, "DVstick 30 / ThumbDV-class")
        XCTAssertEqual(store.stored.first?.lastSeen, clock)
        XCTAssertEqual(pushed, [["TESTSER1"]])
    }

    func testADVstick33IsRecognisedButNotTrusted() async {
        enroller.candidatesToReturn = [VocoderFixtures.stick33]
        enroller.script("TESTSER3", VocoderFixtures.unsupported(VocoderFixtures.stick33))
        let vm = model()
        vm.refresh()

        let outcome = await vm.enrol(VocoderFixtures.stick33)

        XCTAssertEqual(outcome?.result, .unsupported)
        XCTAssertTrue(vm.trusted.isEmpty)
        XCTAssertTrue(store.stored.isEmpty)
        XCTAssertEqual(pushed, [])
        let summary = VocoderOutcomeSummary(outcome!)
        XCTAssertEqual(summary.title, "DVstick 33 isn’t supported yet")
        XCTAssertFalse(summary.isSuccess)
        XCTAssertTrue(summary.detail.contains("not supported yet"))
    }

    func testAnUnansweringCableIsRefused() async {
        enroller.candidatesToReturn = [VocoderFixtures.catCable]
        enroller.script("CATCABLE", VocoderFixtures.noAnswer(VocoderFixtures.catCable))
        let vm = model()
        vm.refresh()

        let outcome = await vm.enrol(VocoderFixtures.catCable)

        XCTAssertEqual(outcome?.result, .noAnswer)
        XCTAssertTrue(vm.trusted.isEmpty)
        XCTAssertEqual(pushed, [])
        XCTAssertFalse(VocoderOutcomeSummary(outcome!).isSuccess)
    }

    func testEnrollingAnAlreadyTrustedStickDoesNotDuplicateIt() async {
        enroller.candidatesToReturn = [VocoderFixtures.stick]
        enroller.script("TESTSER1", VocoderFixtures.trusted(VocoderFixtures.stick))
        let vm = model()
        _ = await vm.enrol(VocoderFixtures.stick)
        _ = await vm.enrol(VocoderFixtures.stick)
        XCTAssertEqual(vm.serials, ["TESTSER1"])
    }

    func testAddableCandidatesLeaveOutTrustedSticks() async {
        enroller.candidatesToReturn = [VocoderFixtures.stick, VocoderFixtures.catCable]
        enroller.script("TESTSER1", VocoderFixtures.trusted(VocoderFixtures.stick))
        let vm = model()
        vm.refresh()
        _ = await vm.enrol(VocoderFixtures.stick)
        XCTAssertEqual(vm.addableCandidates, [VocoderFixtures.catCable])
    }

    // MARK: - Removing

    func testRemovingUntrustsPersistsAndPushes() async {
        enroller.candidatesToReturn = [VocoderFixtures.stick]
        enroller.script("TESTSER1", VocoderFixtures.trusted(VocoderFixtures.stick))
        let vm = model()
        _ = await vm.enrol(VocoderFixtures.stick)

        vm.remove(serial: "TESTSER1")

        XCTAssertTrue(vm.trusted.isEmpty)
        XCTAssertTrue(store.stored.isEmpty)
        XCTAssertEqual(pushed, [["TESTSER1"], []])
    }

    // MARK: - Persistence

    func testTrustSurvivesARelaunch() async {
        enroller.candidatesToReturn = [VocoderFixtures.stick]
        enroller.script("TESTSER1", VocoderFixtures.trusted(VocoderFixtures.stick))
        _ = await model().enrol(VocoderFixtures.stick)

        let relaunched = model()
        XCTAssertEqual(relaunched.serials, ["TESTSER1"])
    }

    func testUserDefaultsStoreRoundTripsUnderTheDocumentedKey() throws {
        let suite = "astar.tests.vocoder.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let s = UserDefaultsVocoderTrustStore(defaults)
        XCTAssertEqual(UserDefaultsVocoderTrustStore.key, "vocoder.trusted")
        let v = TrustedVocoder(
            serial: "TESTSER1", prodid: "AMBE3000R", model: "DVstick 30 / ThumbDV-class",
            lastSeen: clock)
        s.save([v])
        XCTAssertNotNil(defaults.object(forKey: "vocoder.trusted"))
        XCTAssertEqual(UserDefaultsVocoderTrustStore(defaults).load(), [v])
    }

    /// The engine's FFI splits its list on commas and whitespace, so a stored
    /// "serial" carrying either would be read as TWO trusted serials — a
    /// hand-edited preferences domain must not be able to widen the list.
    func testAStoredSerialThatWouldSplitIsDropped() throws {
        let suite = "astar.tests.vocoder.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let s = UserDefaultsVocoderTrustStore(defaults)
        s.save([
            TrustedVocoder(serial: "TESTSER1,CATCABLE", prodid: nil, model: nil, lastSeen: nil),
            TrustedVocoder(serial: "TESTSER2 CATCABLE", prodid: nil, model: nil, lastSeen: nil),
            TrustedVocoder(serial: "  ", prodid: nil, model: nil, lastSeen: nil),
            TrustedVocoder(serial: "TESTSER1", prodid: nil, model: nil, lastSeen: nil),
            TrustedVocoder(serial: "TESTSER1", prodid: nil, model: nil, lastSeen: nil),
        ])
        XCTAssertEqual(s.load().map(\.serial), ["TESTSER1"])
    }

    func testACorruptStoredValueReadsAsNothingTrusted() throws {
        let suite = "astar.tests.vocoder.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set("TESTSER1,CATCABLE", forKey: UserDefaultsVocoderTrustStore.key)
        XCTAssertEqual(UserDefaultsVocoderTrustStore(defaults).load(), [])
    }

    // MARK: - Connected and last seen

    func testConnectedFollowsTheListingAndStampsLastSeen() async {
        enroller.candidatesToReturn = [VocoderFixtures.stick]
        enroller.script("TESTSER1", VocoderFixtures.trusted(VocoderFixtures.stick))
        let vm = model()
        vm.refresh()
        _ = await vm.enrol(VocoderFixtures.stick)
        let first = clock
        XCTAssertTrue(vm.isConnected(serial: "TESTSER1"))
        XCTAssertEqual(vm.port(serial: "TESTSER1"), "/dev/cu.usbserial-TESTSER1")

        // Unplugged: not connected, and last seen stays where it was.
        enroller.candidatesToReturn = []
        clock = clock.addingTimeInterval(3600)
        vm.refresh()
        XCTAssertFalse(vm.isConnected(serial: "TESTSER1"))
        XCTAssertNil(vm.port(serial: "TESTSER1"))
        XCTAssertEqual(vm.trusted.first?.lastSeen, first)

        // Plugged back in: seen now.
        enroller.candidatesToReturn = [VocoderFixtures.stick]
        vm.refresh()
        XCTAssertEqual(vm.trusted.first?.lastSeen, clock)
        XCTAssertEqual(store.stored.first?.lastSeen, clock)
    }

    /// The launch-time hint: an untrusted vocoder-class device is attached and
    /// nothing is trusted yet — exactly an upgrade from a build that scanned.
    func testTheSetupHintShowsOnlyWhileNothingIsTrusted() async {
        let vm = model()
        vm.refresh()
        XCTAssertFalse(vm.suggestsAdding, "nothing attached, nothing to suggest")

        enroller.candidatesToReturn = [VocoderFixtures.stick]
        vm.refresh()
        XCTAssertTrue(vm.suggestsAdding)

        enroller.script("TESTSER1", VocoderFixtures.trusted(VocoderFixtures.stick))
        _ = await vm.enrol(VocoderFixtures.stick)
        XCTAssertFalse(vm.suggestsAdding)
    }

    // MARK: - Outcome summaries

    func testOutcomeSummariesNameWhatHappened() {
        let ok = VocoderOutcomeSummary(VocoderFixtures.trusted(VocoderFixtures.stick))
        XCTAssertTrue(ok.isSuccess)
        XCTAssertEqual(ok.title, "Vocoder added")

        let cable = VocoderOutcomeSummary(VocoderFixtures.noAnswer(VocoderFixtures.catCable))
        XCTAssertEqual(cable.title, "Not a vocoder")
        XCTAssertTrue(cable.detail.contains("CAT/PTT"))
    }

    // MARK: - Connect-error affordance

    func testTheNoTrustedVocoderErrorOffersSettings() {
        XCTAssertTrue(
            VocoderSetupPrompt.offersSettings(
                for:
                    "Couldn’t connect to XLX836 A: No trusted vocoder — add one in Settings → Vocoders."
            ))
        XCTAssertTrue(
            VocoderSetupPrompt.offersSettings(
                for: CallSession.ConnectError.noTrustedVocoder(.dstar).errorDescription))
        XCTAssertFalse(
            VocoderSetupPrompt.offersSettings(for: "Node 61057 is not registered in Allstar."))
        XCTAssertFalse(VocoderSetupPrompt.offersSettings(for: nil))
    }
}
