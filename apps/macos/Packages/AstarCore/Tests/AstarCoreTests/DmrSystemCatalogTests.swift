// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The shape of the DMR picker, and the three-part dial string the picker,
/// the talkgroup field and the timeslot control each edit a third of.
final class DmrSystemCatalogTests: XCTestCase {

    private func row(_ id: String, _ name: String, system: String?) -> DirectoryEntry {
        DirectoryEntry(
            network: .dmr, id: id, name: name,
            dial: system.map { .mmdvm(system: $0, host: "\($0).example", port: 62031) })
    }

    private var entries: [DirectoryEntry] {
        [
            row("freedmr-network-server-eu", "FreeDMR EU", system: "freedmr-network"),
            row("freedmr-network-server-cymru", "FreeDMR Cymru", system: "freedmr-network"),
            row("ipsc2-poland-server-pl", "IPSC2 Poland", system: "ipsc2-poland"),
            row("xlx696-server-xlx696", "XLX696", system: "xlx696"),
            row("hb_it_bat-server-bat", "HB IT BAT", system: "hb_it_bat"),
            row("brandmeister-server-3102", "BM 3102", system: "brandmeister"),
            row("adn-systems-chile-server-adn", "ADN Chile", system: nil),
            DirectoryEntry(
                network: .nxdn, id: "100", name: "NXDN 100",
                dial: .nxdn(host: "n.example", port: 41400)),
        ]
    }

    /// Nine families over 111 slugs: what a family does not claim goes under
    /// one heading, and that heading holds most of the directory.
    func testRowsGroupByFamilyWithEverythingElseUnderOneHeading() {
        let groups = DmrSystemCatalog.grouped(entries, consented: false)
        XCTAssertEqual(groups.map(\.title), ["FreeDMR", "DMR+", DmrSystemCatalog.independentTitle])
        XCTAssertEqual(
            groups.first?.systems.map(\.name), ["FreeDMR Cymru", "FreeDMR EU"],
            "alphabetical inside a family, not feed order")
        XCTAssertEqual(
            groups.last?.systems.map(\.name), ["HB IT BAT", "XLX696"],
            "an unrecognised system is listed and dialable, never dropped")
    }

    /// A row with no dial is not a choice; a row on another network is not a
    /// DMR choice.
    func testUndialableAndForeignRowsAreNotOffered() {
        let names = DmrSystemCatalog.grouped(entries, consented: true).flatMap { $0.systems }
            .map(\.name)
        XCTAssertFalse(names.contains("ADN Chile"))
        XCTAssertFalse(names.contains("NXDN 100"))
    }

    /// The gate at the affordance, not only at the dial: an operator who has
    /// not accepted BrandMeister's terms is not offered their network.
    func testBrandmeisterIsAbsentUntilConsented() {
        XCTAssertFalse(
            DmrSystemCatalog.grouped(entries, consented: false).contains {
                $0.family == .brandmeister
            })
        let consented = DmrSystemCatalog.grouped(entries, consented: true)
        XCTAssertEqual(
            consented.first { $0.family == .brandmeister }?.systems.map(\.name), ["BM 3102"])
        XCTAssertEqual(
            consented.map(\.title).last, DmrSystemCatalog.independentTitle,
            "consent puts BrandMeister in its normal place, not at the end")
    }

    /// The slug is what the password and the consent check key on, and it
    /// comes off the row's dial — never off its id, which merely starts with
    /// the same letters.
    func testTheSlugComesFromTheDialNotTheID() {
        XCTAssertEqual(
            DmrSystemCatalog.slug(forEntryID: "ipsc2-poland-server-pl", in: entries),
            "ipsc2-poland")
        XCTAssertNil(DmrSystemCatalog.slug(forEntryID: "adn-systems-chile-server-adn", in: entries))
        XCTAssertNil(DmrSystemCatalog.slug(forEntryID: "tgif.network", in: entries))
        XCTAssertNil(DmrSystemCatalog.slug(forEntryID: "", in: entries))
    }

    // MARK: - The three-part dial string

    func testTheDialStringSplitsIntoTheThreeThingsAControlEachOwns() {
        let full = DmrDialText.parts("freedmr-network-server-eu/91/1")
        XCTAssertEqual(full.address, "freedmr-network-server-eu")
        XCTAssertEqual(full.talkgroup, "91")
        XCTAssertEqual(full.timeslot, 1)

        // Unfinished input never throws away what IS there.
        XCTAssertEqual(DmrDialText.parts("host.example").talkgroup, "")
        XCTAssertEqual(DmrDialText.parts("host.example/91").timeslot, 2, "TS2 is the default")
        XCTAssertEqual(DmrDialText.parts("host.example/91/9").timeslot, 2, "there is no slot 9")
        XCTAssertEqual(DmrDialText.parts("").address, "")
    }

    func testComposingKeepsWhateverHalfOfTheTargetExistsSoFar() {
        XCTAssertEqual(
            DmrDialText.compose(address: "host.example", talkgroup: "91", timeslot: 1),
            "host.example/91/1")
        XCTAssertEqual(
            DmrDialText.compose(address: "host.example", talkgroup: "", timeslot: 2),
            "host.example", "a master with no room yet is unfinished, not malformed")
        XCTAssertEqual(
            DmrDialText.compose(address: "", talkgroup: "91", timeslot: 2), "/91/2",
            "a talkgroup typed before a network is chosen is not thrown away")
        XCTAssertEqual(
            DmrDialText.compose(address: "h", talkgroup: "9 1a", timeslot: 7), "h/91/2",
            "digits only, and no slot but 1 or 2")
    }

    /// The round trip the UI actually performs on every keystroke.
    func testEditingOnePartLeavesTheOthersAlone() {
        let original = "freedmr-network-server-eu/91/2"
        let parts = DmrDialText.parts(original)
        XCTAssertEqual(
            DmrDialText.compose(
                address: parts.address, talkgroup: "3100", timeslot: parts.timeslot),
            "freedmr-network-server-eu/3100/2")
        XCTAssertEqual(
            DmrDialText.compose(address: parts.address, talkgroup: parts.talkgroup, timeslot: 1),
            "freedmr-network-server-eu/91/1")
    }

    // MARK: - Naming a network rather than a master

    /// The password field picks a NETWORK, and the directory names servers —
    /// so a slug needs a readable label built from what is known, with the
    /// slug itself kept beside it because that is what the network's own
    /// paperwork says.
    func testASlugGetsAReadableLabelUsingTheFamilyNameWhereThereIsOne() {
        XCTAssertEqual(DmrSystemCatalog.label(forSlug: "freedmr-network"), "FreeDMR")
        XCTAssertEqual(DmrSystemCatalog.label(forSlug: "freedmr-reunion"), "FreeDMR Reunion")
        XCTAssertEqual(DmrSystemCatalog.label(forSlug: "ipsc2-poland"), "DMR+ Poland")
        XCTAssertEqual(DmrSystemCatalog.label(forSlug: "tgif"), "TGIF")
        // No family: title-cased segments, and the acronyms these slugs are
        // full of are left alone rather than lower-cased by `capitalized`.
        XCTAssertEqual(
            DmrSystemCatalog.label(forSlug: "hb_it_trani_conference"), "Hb It Trani Conference")
        XCTAssertEqual(DmrSystemCatalog.label(forSlug: "xlx696"), "Xlx696")
        XCTAssertEqual(DmrSystemCatalog.label(forSlug: ""), "")
    }
}
