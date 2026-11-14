// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// D-Star's vocoder choice (astar-dstar-perens): the dongle by default, the
/// software vocoder only for D-Star and only in a build that has it.
///
/// Nothing here opens a port or touches the network: the session runs on
/// `FakeStation`, and the view-model on in-memory stores.
@MainActor
final class DStarVocoderChoiceTests: XCTestCase {

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.dstarvocoder.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private func model(
        _ defaults: UserDefaults, compiled: Bool,
        pushed: @escaping (DStarVocoder) -> Void = { _ in }
    ) -> VocoderSettings {
        VocoderSettings(
            store: InMemoryVocoderTrustStore(), enroller: FakeVocoderEnroller(),
            dstarDefaults: defaults, softwareVocoderCompiled: compiled,
            onDStarVocoderChanged: pushed)
    }

    // MARK: - The preference

    func testAnUnstampedMacUsesTheDongle() {
        XCTAssertEqual(DStarVocoderPreference.load(scratchDefaults()), .dongle)
    }

    func testAnUnreadableValueReadsAsTheDongle() {
        let d = scratchDefaults()
        d.set("Software", forKey: DStarVocoderPreference.key)
        XCTAssertEqual(DStarVocoderPreference.load(d), .dongle)
        d.set(1, forKey: DStarVocoderPreference.key)
        XCTAssertEqual(DStarVocoderPreference.load(d), .dongle)
    }

    func testTheChoiceIsMachineLocal() {
        XCTAssertTrue(DStarVocoderPreference.key.hasPrefix("vocoder."))
        XCTAssertTrue(
            ConfigArchive.machineLocalPrefixes.contains {
                DStarVocoderPreference.key.hasPrefix($0)
            },
            "an exported config must never carry the vocoder choice")
    }

    // MARK: - Per mode: D-Star only

    func testOnlyDStarMayOfferTheSoftwareVocoder() {
        XCTAssertTrue(VocoderModeRow.dstar.allowsSoftwareVocoder)
        for row in [VocoderModeRow.ysf, .nxdn, .dmr] {
            XCTAssertFalse(row.allowsSoftwareVocoder, "\(row) is AMBE+2: hardware only")
        }
        XCTAssertTrue(VocoderModeRow.ambePlus2Reason.contains("8,359,197"))
        XCTAssertTrue(VocoderModeRow.ambePlus2Reason.contains("2028-05-20"))
    }

    // MARK: - The view-model

    func testTheViewModelDefaultsToTheDongle() {
        let vm = model(scratchDefaults(), compiled: true)
        XCTAssertEqual(vm.dstarVocoder, .dongle)
        XCTAssertFalse(vm.softwareChosenButMissing)
    }

    func testChoosingSoftwareStoresAndPushesIt() {
        let d = scratchDefaults()
        var pushed: [DStarVocoder] = []
        let vm = model(d, compiled: true) { pushed.append($0) }
        vm.setDStarVocoder(.software)
        XCTAssertEqual(vm.dstarVocoder, .software)
        XCTAssertEqual(DStarVocoderPreference.load(d), .software)
        XCTAssertEqual(pushed, [.software])
        vm.setDStarVocoder(.software)
        XCTAssertEqual(pushed, [.software], "no push for a choice that did not change")
        vm.setDStarVocoder(.dongle)
        XCTAssertEqual(pushed, [.software, .dongle])
    }

    func testSoftwareCannotBeChosenInABuildWithoutIt() {
        let d = scratchDefaults()
        var pushed: [DStarVocoder] = []
        let vm = model(d, compiled: false) { pushed.append($0) }
        vm.setDStarVocoder(.software)
        XCTAssertEqual(vm.dstarVocoder, .dongle)
        XCTAssertEqual(DStarVocoderPreference.load(d), .dongle)
        XCTAssertEqual(pushed, [])
    }

    func testAStoredSoftwareChoiceInABuildWithoutItIsFlaggedNotRewritten() {
        let d = scratchDefaults()
        DStarVocoderPreference.save(.software, d)
        let vm = model(d, compiled: false)
        XCTAssertEqual(vm.dstarVocoder, .software, "never quietly turned into the dongle")
        XCTAssertTrue(vm.softwareChosenButMissing)
        vm.setDStarVocoder(.dongle)
        XCTAssertFalse(vm.softwareChosenButMissing)
    }

    // MARK: - The session

    func testTheSessionHandsTheStoredChoiceToTheStation() {
        let d = scratchDefaults()
        DStarVocoderPreference.save(.software, d)
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: d)
        XCTAssertEqual(session.dstarVocoder, .software)
        XCTAssertEqual(fake.dstarVocoderCalls.last, .software)

        session.applyDStarVocoder(.dongle)
        XCTAssertEqual(session.dstarVocoder, .dongle)
        XCTAssertEqual(fake.dstarVocoderCalls.last, .dongle)

        let rebuilt = FakeStation()
        session.reconfigure(station: rebuilt, hasCredentials: false)
        XCTAssertEqual(rebuilt.dstarVocoderCalls, [.dongle], "a rebuilt station keeps the choice")
    }

    func testSoftwareChosenButUnavailableSaysSoNotAddAVocoder() {
        let fake = FakeStation()  // its snapshot reports D-Star unavailable
        let session = CallSession(station: fake, userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.applyDStarVocoder(.software)
        session.poll()
        XCTAssertFalse(session.dstarAvailable)
        XCTAssertThrowsError(try session.connect(node: "127.0.0.1:30001/A", network: .dstar)) {
            error in
            guard case CallSession.ConnectError.dstarSoftwareVocoderMissing = error else {
                return XCTFail("expected dstarSoftwareVocoderMissing, got \(error)")
            }
            XCTAssertTrue(
                (error as? LocalizedError)?.errorDescription?.contains("Settings → Vocoders")
                    ?? false)
        }
        XCTAssertTrue(fake.dstarConnects.isEmpty)
    }

    // MARK: - Making Software easy to find

    func testDStarsNoTrustedRefusalNamesTheSoftwareVocoderWhenBuiltIn() {
        let dstar = VocoderSetupPrompt.noTrustedVocoderMessage(for: .dstar, softwareCompiled: true)
        XCTAssertTrue(VocoderSetupPrompt.offersSettings(for: dstar), "still opens Settings")
        XCTAssertTrue(VocoderSetupPrompt.offersSoftwareSwitch(for: dstar))
        let without = VocoderSetupPrompt.noTrustedVocoderMessage(
            for: .dstar, softwareCompiled: false)
        XCTAssertFalse(VocoderSetupPrompt.offersSoftwareSwitch(for: without))
        XCTAssertTrue(VocoderSetupPrompt.offersSettings(for: without))
    }

    func testAmbePlus2RefusalsNeverOfferTheSoftwareVocoder() {
        for network in [Network.ysf, .nxdn, .dmr] {
            let m = VocoderSetupPrompt.noTrustedVocoderMessage(for: network, softwareCompiled: true)
            XCTAssertFalse(VocoderSetupPrompt.offersSoftwareSwitch(for: m), "\(network)")
            XCTAssertTrue(VocoderSetupPrompt.offersSettings(for: m))
        }
    }

    func testTheSwiftSentenceMatchesTheEngines() {
        // The engine's NO_TRUSTED_VOCODER_DSTAR, word for word.
        XCTAssertEqual(
            VocoderSetupPrompt.dstarNoTrustedVocoderMessage,
            "No trusted vocoder — add one in Settings → Vocoders, "
                + "or switch D-Star to the software vocoder there")
    }

    func testTheShippedBindingHasTheSoftwareVocoder() {
        // `just xcframework` builds it in unless ASTAR_DSTAR_SOFT_AMBE=0.
        if ProcessInfo.processInfo.environment["ASTAR_DSTAR_SOFT_AMBE"] != "0" {
            XCTAssertTrue(Station.dstarSoftwareVocoderCompiled)
        }
    }
}
