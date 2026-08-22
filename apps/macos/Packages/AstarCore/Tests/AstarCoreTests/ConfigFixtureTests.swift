// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-b52e — a real `0.1.4beta` export, kept as a fixture so the **v1 format
/// stays readable forever**.
///
/// Every other config test builds its archive with the current code, so the two
/// sides move together and a format change can never fail them. This one is a
/// file written by a shipped build: if a later change stops it importing, that
/// is a compatibility break in a file users already have on disk.
///
/// Scrubbed before committing — callsign is `N0CALL`, two of the operator's
/// nodes are gone, and the USB adapter's hardware serial is genericised out of
/// the saved config's port path. The rest is verbatim.
final class ConfigFixtureTests: XCTestCase {

    private func fixture() throws -> Data {
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: "astar-config-v1", withExtension: "astarconfig",
                subdirectory: "Fixtures"),
            "fixture missing from the test bundle")
        return try Data(contentsOf: url)
    }

    func testAShippedV1ExportStillDecodes() throws {
        let archive = try ConfigArchive.decode(try fixture())
        XCTAssertEqual(archive.version, 1)
        XCTAssertEqual(archive.appVersion, "0.1.4beta")
        XCTAssertEqual(archive.presentSections, Set(ConfigSection.allCases))
    }

    func testItCarriesWhatItSaysOnTheTin() throws {
        let a = try ConfigArchive.decode(try fixture())
        XCTAssertEqual(a.rigs?.setups.map(\.name), ["UCI150", "Headphone Port"])
        XCTAssertEqual(a.directory?.count, 13)
        XCTAssertEqual(a.callsign, "N0CALL")
        XCTAssertEqual(a.settings?["audio.input"], .string("USB Audio Device"))
        XCTAssertEqual(a.interface?["ui.network"], .string("m17"))
        // Per-config serial survives the round trip — the richest nested type
        // in the format, and the one most likely to break silently.
        let uci = try XCTUnwrap(a.rigs?.setups.first { $0.name == "UCI150" })
        XCTAssertEqual(uci.serial?.debounceMs, 30)
        XCTAssertNotNil(uci.serial?.portPath)
        // Both networks are represented, so a decode that lost `network` fails.
        let nets = Set((a.directory ?? []).map(\.network))
        XCTAssertEqual(nets, [.allstar, .m17])
    }

    func testItImportsCleanlyIntoAFreshMac() throws {
        let name = "astar.test.fixture.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { UserDefaults.standard.removePersistentDomain(forName: name) }

        let session = CallSession(
            station: NullStation(),
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
        let setupStore = UserDefaultsSetupStore(defaults)

        let summary = ConfigTransfer.apply(
            try ConfigArchive.decode(try fixture()), session: session,
            setupStore: setupStore,
            profileStore: UserDefaultsMicProfileStore(defaults),
            defaults: defaults)

        XCTAssertEqual(summary.setupsAdded, 2)
        XCTAssertEqual(summary.directoryAdded, 13)
        XCTAssertEqual(session.directoryAll().count, 13)
        XCTAssertEqual(session.m17Callsign, "N0CALL")
        // The ★ must land: this is the value that was silently lost before
        // SetupController.reloadFromStore existed.
        XCTAssertNotNil(setupStore.loadDefaultID())
        XCTAssertTrue(setupStore.all().contains { $0.id == setupStore.loadDefaultID() })
    }

    func testTheFixtureCarriesNothingPersonal() throws {
        // Guards the scrub itself: this file ships in the repo, which is going
        // to be public.
        let text = String(decoding: try fixture(), as: UTF8.self)
        for forbidden in ["AJ7HR", "69586", "543260", "WA7ABU", "5B210098241"] {
            XCTAssertFalse(text.contains(forbidden), "fixture leaked \(forbidden)")
        }
    }
}
