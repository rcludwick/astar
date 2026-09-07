// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// DMR's target grammar, the directory-slug → family bridge, and the
/// talkgroup list — the three pieces that had to exist before `CallSession`
/// could dial a talkgroup on a master.
///
/// DMR is the first astar network where the address is not a host and the
/// credential is not a callsign, so most of what is pinned here is a refusal:
/// four things or nothing, no invented slot, no guessed network.
final class DmrDialTests: XCTestCase {

    // MARK: - The four-part target

    func testDMRDialTakesFourThingsBecauseATalkgroupNamesNothingAlone() {
        // docs/design/dmr-networks.md: "TG 91 exists on several of these
        // networks and is a different room on each." The dial is the network,
        // the master, the talkgroup and the slot -- never fewer.
        let full = DmrDial.parse("tgif.network:62031/31313/2", system: "tgif")
        XCTAssertEqual(full?.host, "tgif.network")
        XCTAssertEqual(full?.port, 62031)
        XCTAssertEqual(full?.talkgroup, 31313)
        XCTAssertEqual(full?.timeslot, 2)
        XCTAssertEqual(full?.system, "tgif")

        let defaulted = DmrDial.parse("tgif.network/31313", system: "tgif")
        XCTAssertEqual(defaulted?.port, 62031, "62031 is the directory's most common port")
        XCTAssertEqual(defaulted?.timeslot, 2, "TS2 is the hotspot convention")

        XCTAssertNil(DmrDial.parse("", system: "tgif"))
        XCTAssertNil(DmrDial.parse("tgif.network", system: "tgif"), "no talkgroup is not a target")
        XCTAssertNil(DmrDial.parse("tgif.network/31313/3", system: "tgif"), "there is no slot 3")
        XCTAssertNil(DmrDial.parse("tgif.network/31313/0", system: "tgif"))
        XCTAssertNil(DmrDial.parse("tgif.network:0/31313/2", system: "tgif"))
        XCTAssertNil(DmrDial.parse("tgif.network/notanumber/2", system: "tgif"))
        XCTAssertNil(
            DmrDial.parse("tgif.network/31313/2", system: nil), "the network is part of the address"
        )
    }

    /// The system may ride in the address itself, which is the only way to
    /// reach a network the directory does not list — TGIF, astar's first and
    /// recommended target, is exactly that case.
    func testATypedTargetCanCarryItsOwnSystem() {
        let dial = DmrDial.parse("tgif:tgif.network:62031/31313/2", system: nil)
        XCTAssertEqual(dial?.system, "tgif")
        XCTAssertEqual(dial?.host, "tgif.network")
        XCTAssertEqual(dial?.port, 62031)
        XCTAssertEqual(dial?.talkgroup, 31313)
        XCTAssertEqual(dial?.timeslot, 2)

        // A port is digits and a hostname is not, so the two-part form is
        // decidable without guessing.
        let noPort = DmrDial.parse("tgif:tgif.network/31313/1", system: nil)
        XCTAssertEqual(noPort?.system, "tgif")
        XCTAssertEqual(noPort?.host, "tgif.network")
        XCTAssertEqual(noPort?.port, DmrDial.defaultPort)
        XCTAssertEqual(noPort?.timeslot, 1)

        // The address wins over the argument: what was typed is what was meant.
        XCTAssertEqual(
            DmrDial.parse("brandmeister:bm.example/91/2", system: "tgif")?.system, "brandmeister")
        // Four colon-separated parts name nothing.
        XCTAssertNil(DmrDial.parse("a:b:c:d/91/2", system: nil))
    }

    func testDMRAddressesNoModuleAndNoModulePickerAppears() {
        // A DMR target is a talkgroup on a timeslot. There is no module, and
        // a module separator in a DMR dial is a REJECTION rather than
        // something to ignore -- adding-a-network.md §5.6.
        XCTAssertFalse(ReflectorDial.mmdvm(system: "s", host: "h", port: 62031).addressesModule)
        XCTAssertNil(DmrDial.parse("tgif.network 31313 A", system: "tgif"))
    }

    // MARK: - The directory slug → family bridge

    func testTheDirectorySystemSlugIsNotTheEngineFamilySlug() {
        // Checked against the live feed on 2026-09-07: 111 distinct `system`
        // values across 185 rows, and not one of them equals a DmrNetwork
        // slug. The two vocabularies are different by construction, and
        // `family(ofSystem:)` is the documented bridge -- not a coincidence
        // to be relied on.
        XCTAssertEqual(DmrDial.family(ofSystem: "freedmr-network"), .freedmr)
        XCTAssertEqual(DmrDial.family(ofSystem: "freedmr-reunion"), .freedmr)
        XCTAssertEqual(DmrDial.family(ofSystem: "dmrplus-ipsc2-uk"), .dmrplus)
        XCTAssertEqual(DmrDial.family(ofSystem: "ipsc2-poland"), .dmrplus)
        XCTAssertEqual(DmrDial.family(ofSystem: "systemx"), .systemx)
        XCTAssertEqual(DmrDial.family(ofSystem: "amcomm"), .amcomm)
        XCTAssertEqual(DmrDial.family(ofSystem: "adn-systems-espana"), .adn)
        XCTAssertEqual(DmrDial.family(ofSystem: "tgif"), .tgif)
        // 111 systems and nine families: most rows belong to no family this
        // build names, and they must still be listed and dialable.
        XCTAssertNil(DmrDial.family(ofSystem: "hb_it_trani_conference"))
        XCTAssertNil(DmrDial.family(ofSystem: "xlx696"))
        XCTAssertNil(DmrDial.family(ofSystem: ""))
    }

    func testAConsentedBrandmeisterStillNeverAppearsWhenTheGateIsPermanentlyClosed() {
        // If Task 2 ruled outcome (a) -- BrandMeister asks software clients
        // not to use the homebrew login -- this assertion is INVERTED and the
        // consent toggle is removed entirely. Read
        // docs/design/dmr-brandmeister-position.md before touching it.
        XCTAssertTrue(DmrFamily.brandmeister.requiresConsent)
        XCTAssertFalse(DmrFamily.tgif.requiresConsent)
        XCTAssertFalse(DmrFamily.freedmr.requiresConsent)
        XCTAssertEqual(DmrFamily.allCases.count, 9)
        for family in DmrFamily.allCases {
            XCTAssertFalse(family.displayName.isEmpty, "\(family) has no label")
        }
    }

    // MARK: - The directory row

    func testAnMMDVMDialDecodesFromTheDirectoryRow() throws {
        // The live shape, copied from the feed on 2026-09-07.
        let json =
            #"{"kind":"mmdvm","system":"freedmr-network","host":"m.example","port":62031,"requires":["dmr_id","password"],"talkgroups_url":"https://dvref.com/x"}"#
        let dial = try JSONDecoder().decode(ReflectorDial.self, from: Data(json.utf8))
        XCTAssertEqual(dial, .mmdvm(system: "freedmr-network", host: "m.example", port: 62031))
        XCTAssertEqual(dial.kind, "mmdvm")
        XCTAssertEqual(dial.endpoint?.port, 62031)
    }

    func testAnMMDVMRowWithNoDialIsListedAndNotDialable() {
        // 30 of the 185 rows have no `dial` at all. Dropping them would make
        // a directory that lists 185 networks look like one that lists 155.
        let json = #"{"kind":"mmdvm","system":"adn-systems-chile"}"#
        let dial = try? JSONDecoder().decode(ReflectorDial.self, from: Data(json.utf8))
        XCTAssertEqual(dial, .unsupported(kind: "mmdvm"))
        XCTAssertFalse(dial?.isDialable ?? true)
    }

    /// A cache is written back out and read in again; the system has to
    /// survive that trip or the row loses the one field that names its master.
    func testAnMMDVMDialRoundTripsThroughTheCache() throws {
        let dial = ReflectorDial.mmdvm(system: "freedmr-network", host: "m.example", port: 62031)
        let data = try JSONEncoder().encode(dial)
        XCTAssertEqual(try JSONDecoder().decode(ReflectorDial.self, from: data), dial)
    }

    func testDMRAndNXDNTalkgroupOneHundredDoNotCollide() {
        // DirectoryEntry.key is `network:id` precisely so these are two rows.
        let a = DirectoryEntry(
            network: .dmr, id: "100", name: "DMR 100",
            dial: .mmdvm(system: "systemx", host: "a.example", port: 62031))
        let b = DirectoryEntry(
            network: .nxdn, id: "100", name: "NXDN 100",
            dial: .nxdn(host: "b.example", port: 41400))
        XCTAssertNotEqual(a.key, b.key)
    }

    func testADirectoryDescriptionIsTextNotMarkup() {
        // Live rows carry raw HTML in `description`, in several languages.
        // It is publisher free text: shown as text, never rendered, never
        // trusted.
        let entry = DirectoryEntry(
            network: .dmr, id: "x", name: "n",
            description: "<p><strong>TG 8</strong>&nbsp;is bridged</p>",
            dial: .mmdvm(system: "s", host: "h", port: 62031))
        XCTAssertEqual(entry.plainDescription, "TG 8 is bridged")
    }

    /// `other("dmr")` and `.dmr` must be the same wire string, or every
    /// already-cached feed changes meaning the day the case is added.
    func testTheDMRDirectoryNetworkRoundTrips() {
        XCTAssertEqual(ReflectorNetwork.dmr.rawValue, "dmr")
        XCTAssertEqual(ReflectorNetwork(rawValue: "dmr"), .dmr)
        XCTAssertEqual(ReflectorNetwork(rawValue: "DMR"), .dmr)
        XCTAssertTrue(ReflectorNetwork.known.contains(.dmr))
        XCTAssertEqual(Network.dmr.reflectorNetwork, .dmr)
        XCTAssertEqual(Network.matching(.dmr), .dmr)
    }

    // MARK: - Talkgroups

    func testTalkgroupsDecodeAndAnAbsentListIsEmptyRatherThanAnError() {
        // The list is a separate hamcall-db task and does not exist yet.
        // Typing the number is how every DMR codeplug works, so its absence
        // is a smaller product, not a broken one.
        let json =
            #"{"talkgroups":[{"tg":31313,"name":"TGIF Nationwide"},{"tg":91,"name":"Worldwide"}]}"#
        let list = DmrTalkgroups.decode(Data(json.utf8))
        XCTAssertEqual(list.count, 2)
        XCTAssertEqual(list.first?.tg, 31313)
        XCTAssertEqual(DmrTalkgroups.decode(Data("{}".utf8)), [])
        XCTAssertEqual(DmrTalkgroups.decode(Data("not json".utf8)), [])
        XCTAssertEqual(DmrTalkgroups.decode(Data()), [])
    }

    /// A bare array is the other shape a publisher plausibly ships, and one
    /// bad row must not take the good ones with it.
    func testABareArrayDecodesAndBadRowsAreDroppedNotFatal() {
        let bare = #"[{"tg":91,"name":"Worldwide"},{"name":"no number"},{"tg":0,"name":"zero"}]"#
        XCTAssertEqual(
            DmrTalkgroups.decode(Data(bare.utf8)), [DmrTalkgroup(tg: 91, name: "Worldwide")])
    }
}
