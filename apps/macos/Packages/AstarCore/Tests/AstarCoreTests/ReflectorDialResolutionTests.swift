// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Dialling a reflector by name (astar-refl-ship): the grammar, the
/// directory-then-address ordering, and the resolved-but-incomplete state.
///
/// Nothing here touches a network or a filesystem — the index is a value built
/// from rows in memory, which is the whole reason it exists as a separate type
/// from the directory that loads them.
final class ReflectorDialResolutionTests: XCTestCase {

    // MARK: - Fixtures

    private func dstar(
        id: String = "XLX836", aliases: [String] = ["XRF836"], dial: ReflectorDial? = nil
    ) -> DirectoryEntry {
        DirectoryEntry(
            network: .dstar, id: id, name: id, aliases: aliases,
            dial: dial
                ?? .dextra(host: "45.56.69.219", port: 30001, callsign: "XRF836", modules: []))
    }

    private var m17Entry: DirectoryEntry {
        DirectoryEntry(
            network: .m17, id: "M17-002", name: "M17-002",
            dial: .m17(host: "89.240.4.99", port: 17000, callsign: "M17-002", modules: ["A"]))
    }

    private var ysfEntry: DirectoryEntry {
        DirectoryEntry(
            network: .ysf, id: "00006", name: "HELLAS Zone A",
            dial: .ysf(host: "hellaszone.com", port: 42000))
    }

    private var index: ReflectorIndex {
        ReflectorIndex(entries: [dstar(), m17Entry, ysfEntry])
    }

    // MARK: - Resolving a name

    func testBareNameResolvesButNeedsAModule() {
        guard case .needsModule(let entry) = index.resolveDial("XLX836", network: .dstar) else {
            return XCTFail("a bare D-Star name must resolve as incomplete, not as a target")
        }
        XCTAssertEqual(entry.id, "XLX836")
    }

    func testNameWithSpaceSeparatedModuleIsComplete() throws {
        let target = try XCTUnwrap(index.resolveDial("XLX836 A", network: .dstar).target)
        XCTAssertEqual(target.host, "45.56.69.219")
        XCTAssertEqual(target.port, 30001)
        XCTAssertEqual(target.callsign, "XRF836")
        XCTAssertEqual(target.module, "A")
        XCTAssertEqual(target.entry.id, "XLX836")
    }

    func testNameWithSlashSeparatedModuleIsComplete() throws {
        let target = try XCTUnwrap(index.resolveDial("XLX836/B", network: .dstar).target)
        XCTAssertEqual(target.module, "B")
        XCTAssertEqual(target.host, "45.56.69.219")
    }

    func testAliasResolvesToTheSameEntry() throws {
        let target = try XCTUnwrap(index.resolveDial("XRF836 A", network: .dstar).target)
        XCTAssertEqual(
            target.entry.id, "XLX836", "XRF836 is the same box under its older name")
    }

    func testMatchingAndModuleAreCaseInsensitive() throws {
        let target = try XCTUnwrap(index.resolveDial("  xlx836/b  ", network: .dstar).target)
        XCTAssertEqual(target.entry.id, "XLX836")
        XCTAssertEqual(target.module, "B", "a typed module is case-folded, never rejected")
    }

    func testNetworkIsPartOfTheMatch() {
        // Same id filed under two networks — an NXDN 100 and a P25 100 both
        // exist upstream. Asking on the wrong network must miss, not guess.
        let both = ReflectorIndex(entries: [
            DirectoryEntry(
                network: .nxdn, id: "100", name: "NXDN 100",
                dial: .nxdn(host: "nxdn.invalid", port: 41400)),
            DirectoryEntry(
                network: .p25, id: "100", name: "P25 100",
                dial: .p25(host: "p25.invalid", port: 41000)),
        ])
        XCTAssertEqual(both.resolveDial("100", network: .p25).target?.host, "p25.invalid")
        XCTAssertEqual(both.resolveDial("100", network: .nxdn).target?.host, "nxdn.invalid")
        XCTAssertEqual(both.resolveDial("100", network: .m17), .notInDirectory)
    }

    // MARK: - Directory first, address second

    /// The ordering test. `XLX836` is a perfectly well-formed hostname, and
    /// the address parser says so — which is exactly why the directory has to
    /// be asked first. If this inverted, typing a reflector name would send a
    /// DNS query for "xlx836" instead of dialling the reflector.
    func testAReflectorNameIsNotTreatedAsAHostname() {
        XCTAssertEqual(
            DialTarget.parse("XLX836"), .address("XLX836"),
            "precondition: the address grammar accepts a reflector name as a host")

        guard case .needsModule = index.resolveDial("XLX836", network: .dstar) else {
            return XCTFail("the directory must claim the name before the address parser sees it")
        }
        guard case .ready = index.resolveDial("XLX836 A", network: .dstar) else {
            return XCTFail("a named reflector with a module is a directory hit, not an address")
        }
    }

    func testAnAddressFallsThroughToTheAddressParser() {
        XCTAssertEqual(
            index.resolveDial("45.56.69.219:30001/A", network: .dstar), .notInDirectory,
            "an address names nothing in the directory, so the caller parses it as one")
        XCTAssertEqual(index.resolveDial("m17.example.net:17001/a", network: .m17), .notInDirectory)
    }

    // MARK: - No directory at all

    /// A build with no bundled snapshot and no sync yet. Every name misses,
    /// which is precisely the address-only behaviour that existed before the
    /// directory did — resolution degrades, it does not fail.
    func testAnEmptyIndexResolvesNothingAndRefusesNothing() {
        let empty = ReflectorIndex.empty
        XCTAssertTrue(empty.isEmpty)
        for text in ["XLX836", "XLX836 A", "XRF836/B", "45.56.69.219:30001/A", ""] {
            XCTAssertEqual(
                empty.resolveDial(text, network: .dstar), .notInDirectory,
                "\(text.debugDescription) with no directory must fall through, not error")
        }
    }

    // MARK: - Listed but not dialable

    func testAnEntryWithNoDialIsNeverATarget() {
        let listed = ReflectorIndex(entries: [
            DirectoryEntry(network: .dstar, id: "XLX999", name: "XLX999", dial: nil)
        ])
        guard case .notDialable(let entry) = listed.resolveDial("XLX999 A", network: .dstar) else {
            return XCTFail("a row with no dial object must be refused, not dialled")
        }
        XCTAssertEqual(entry.id, "XLX999")
        XCTAssertNil(listed.resolveDial("XLX999 A", network: .dstar).target)
    }

    func testAnUnsupportedKindIsNeverATarget() {
        let listed = ReflectorIndex(entries: [
            DirectoryEntry(
                network: .other("dmr"), id: "4400", name: "DMR 4400",
                dial: .unsupported(kind: "dmr"))
        ])
        guard case .notDialable = listed.resolveDial("4400", network: .other("dmr")) else {
            return XCTFail("a kind this build cannot drive must be refused, not dialled")
        }
    }

    func testAPortlessURFRowIsNeverATarget() {
        // URF publishes a host and no port for every row. `isDialable` is
        // already false there; resolution must honour it rather than reach
        // for `endpoint` and find nothing.
        let listed = ReflectorIndex(entries: [
            DirectoryEntry(
                network: .urf, id: "URF001", name: "URF001",
                dial: .urf(host: "urf001.invalid", port: nil, modules: ["A"]))
        ])
        guard case .notDialable = listed.resolveDial("URF001 A", network: .urf) else {
            return XCTFail("no port means nothing to connect to; a guess would be worse")
        }
    }

    // MARK: - The module

    func testAModulelessNetworkIsCompleteWithoutOne() throws {
        let target = try XCTUnwrap(index.resolveDial("00006", network: .ysf).target)
        XCTAssertNil(target.module, "YSF has no module; absent is complete, not missing")
        XCTAssertEqual(target.port, 42000)
    }

    func testM17NeedsAModuleTheSameWayDStarDoes() throws {
        guard case .needsModule = index.resolveDial("M17-002", network: .m17) else {
            return XCTFail("M17 reflectors are name + module too — one path serves both")
        }
        let target = try XCTUnwrap(index.resolveDial("M17-002 A", network: .m17).target)
        XCTAssertEqual(target.host, "89.240.4.99")
        XCTAssertEqual(target.module, "A")
    }

    /// A published `modules` array must not become a module. It says what the
    /// publisher listed, and for every D-Star row on earth it is empty —
    /// reading emptiness as "no module needed" is how a client dials into a
    /// room it never chose.
    func testAPublishedModuleListNeverStandsInForAChoice() {
        guard case .needsModule = index.resolveDial("M17-002", network: .m17) else {
            return XCTFail("the entry lists module A; that is information, not a decision")
        }
    }

    func testHalfTypedModuleStaysIncompleteRatherThanBecomingAMiss() {
        guard case .needsModule(let entry) = index.resolveDial("XLX836 AB", network: .dstar)
        else {
            return XCTFail("the reflector still resolved; only the module is unusable")
        }
        XCTAssertEqual(entry.id, "XLX836")
        XCTAssertEqual(index.resolveDial("XLX836 1", network: .dstar), .needsModule(dstar()))
    }

    // MARK: - Grammar

    func testSplitTakesTheFirstSeparator() throws {
        let bare = try XCTUnwrap(ReflectorDialText.split("XLX836"))
        XCTAssertEqual(bare.name, "XLX836")
        XCTAssertNil(bare.module)

        let spaced = try XCTUnwrap(ReflectorDialText.split(" XLX836  a "))
        XCTAssertEqual(spaced.name, "XLX836")
        XCTAssertEqual(spaced.module, "a")

        let slashed = try XCTUnwrap(ReflectorDialText.split("XLX836/b"))
        XCTAssertEqual(slashed.name, "XLX836")
        XCTAssertEqual(slashed.module, "b")

        XCTAssertNil(ReflectorDialText.split(""))
        XCTAssertNil(ReflectorDialText.split("   "))
        XCTAssertNil(ReflectorDialText.split("/A"), "a bare separator names nothing")
    }

    func testModuleAcceptsOneASCIILetterAndNothingElse() {
        XCTAssertEqual(ReflectorDialText.module("a"), "A")
        XCTAssertEqual(ReflectorDialText.module("Z"), "Z")
        XCTAssertNil(ReflectorDialText.module("AB"))
        XCTAssertNil(ReflectorDialText.module("1"))
        XCTAssertNil(ReflectorDialText.module(""))
        XCTAssertNil(ReflectorDialText.module("Å"))
    }

    func testWhichDialsAddressAModule() {
        XCTAssertTrue(
            ReflectorDial.dextra(host: "h", port: 1, callsign: "c", modules: []).addressesModule)
        XCTAssertTrue(
            ReflectorDial.m17(host: "h", port: 1, callsign: "c", modules: []).addressesModule)
        XCTAssertTrue(ReflectorDial.urf(host: "h", port: 1, modules: []).addressesModule)
        XCTAssertFalse(ReflectorDial.ysf(host: "h", port: 1).addressesModule)
        XCTAssertFalse(ReflectorDial.nxdn(host: "h", port: 1).addressesModule)
        XCTAssertFalse(ReflectorDial.p25(host: "h", port: 1).addressesModule)
        XCTAssertFalse(ReflectorDial.unsupported(kind: "dmr").addressesModule)
    }

    // MARK: - The resolved-target line

    func testTheStatusLineShowsAMissingModuleAsMissing() {
        XCTAssertEqual(
            index.resolveDial("XLX836", network: .dstar).statusLine,
            "XLX836 · module — · 45.56.69.219:30001")
        XCTAssertEqual(
            index.resolveDial("XLX836/b", network: .dstar).statusLine,
            "XLX836 · module B · 45.56.69.219:30001")
        XCTAssertEqual(
            index.resolveDial("00006", network: .ysf).statusLine,
            "00006 · hellaszone.com:42000", "no module means no module column")
        XCTAssertNil(
            index.resolveDial("45.56.69.219:30001/A", network: .dstar).statusLine,
            "an address is not a resolved reflector and gets no line")
    }

    // MARK: - Against real published rows

    /// The same journey over the real feed slice, so the grammar is pinned
    /// against what hamcall-db actually publishes rather than only against
    /// hand-built rows.
    func testResolvesXLX836FromTheRealFeedSlice() throws {
        let feed = try ReflectorFeed.decode(ReflectorFixtures.sliceData())
        let index = ReflectorIndex(entries: feed.entries)

        guard case .needsModule = index.resolveDial("xlx836", network: .dstar) else {
            return XCTFail("XLX836 resolves; its module does not come from the feed")
        }
        let target = try XCTUnwrap(index.resolveDial("XRF836/A", network: .dstar).target)
        XCTAssertEqual(target.host, "45.56.69.219")
        XCTAssertEqual(target.port, 30001)
        XCTAssertEqual(target.callsign, "XRF836")
        XCTAssertEqual(target.module, "A")
    }
}
