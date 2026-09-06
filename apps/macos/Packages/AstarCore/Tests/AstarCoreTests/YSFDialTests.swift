// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The YSF address grammar: `host[:port]`, and deliberately no module.
final class YSFDialTests: XCTestCase {

    func testAHostAndPortParse() {
        let parsed = YSFDial.parse("ysf.example:42002")
        XCTAssertEqual(parsed?.host, "ysf.example")
        XCTAssertEqual(parsed?.port, 42002)
    }

    /// 42000 is the plurality, not the standard — 837 of the reflectors the
    /// directory carries, against a tail of others. Fine as a fallback for
    /// text an operator typed; never a substitute for the directory's own.
    func testAMissingPortTakesTheCommonOne() {
        XCTAssertEqual(YSFDial.parse("ysf.example")?.port, 42000)
        XCTAssertEqual(YSFDial.defaultPort, 42000)
    }

    func testWhitespaceAroundTheTargetIsIgnored() {
        XCTAssertEqual(YSFDial.parse("  ysf.example:42000  ")?.host, "ysf.example")
    }

    /// The rejection that matters. `XLX836 A` is a D-Star dial, and a parser
    /// that dropped the module would hand back `XLX836` as a YSF hostname and
    /// link the operator to whatever DNS made of it.
    func testAModuleSeparatorIsRefusedRatherThanIgnored() {
        XCTAssertNil(YSFDial.parse("XLX836 A"))
        XCTAssertNil(YSFDial.parse("ysf.example/A"))
        XCTAssertNil(YSFDial.parse("ysf.example:42000/A"))
    }

    func testMalformedTargetsAreRefused() {
        XCTAssertNil(YSFDial.parse(""), "empty")
        XCTAssertNil(YSFDial.parse("   "), "whitespace only")
        XCTAssertNil(YSFDial.parse(":42000"), "no host")
        XCTAssertNil(YSFDial.parse("ysf.example:0"), "port zero")
        XCTAssertNil(YSFDial.parse("ysf.example:99999"), "port out of range")
        XCTAssertNil(YSFDial.parse("ysf.example:abc"), "non-numeric port")
        XCTAssertNil(YSFDial.parse("a:1:2"), "two colons")
    }

    /// An IPv4 literal is a host like any other — plenty of reflectors are
    /// published as bare addresses.
    func testABareAddressParses() {
        let parsed = YSFDial.parse("45.56.69.219:42001")
        XCTAssertEqual(parsed?.host, "45.56.69.219")
        XCTAssertEqual(parsed?.port, 42001)
    }
}

/// `Network.ysf` as a picker segment: gated on hardware, and carrying its own
/// labels and dial grammar.
final class NetworkYSFTests: XCTestCase {

    func testTheSegmentAppearsOnlyWithAVocoder() {
        XCTAssertFalse(Network.available(m17: true, dstar: true).contains(.ysf))
        XCTAssertTrue(Network.available(m17: false, dstar: false, ysf: true).contains(.ysf))
    }

    /// A persisted `.ysf` selection on a machine with the dongle unplugged
    /// falls back rather than offering a network that cannot connect.
    func testAPersistedSelectionFallsBackWhenUnavailable() {
        XCTAssertEqual(Network.resolve("ysf", m17: true, dstar: true), .allstar)
        XCTAssertEqual(Network.resolve("ysf", m17: false, dstar: false, ysf: true), .ysf)
    }

    func testItCarriesItsOwnLabels() {
        XCTAssertEqual(Network.ysf.displayName, "Fusion")
        XCTAssertEqual(Network.ysf.badge, "YSF")
        XCTAssertFalse(Network.ysf.symbol.isEmpty)
        XCTAssertFalse(Network.ysf.showsDialpad, "the dialpad is an AllStar concern")
    }

    /// The dial field admits the reflector-address alphabet. The space is in
    /// the set because the field is shared with the networks whose grammar
    /// uses it — `YSFDial` is what refuses a module, not the keystroke filter.
    func testTheDialFieldAdmitsAddressCharacters() {
        for c in "ysf.example:42000" {
            XCTAssertTrue(Network.ysf.admitsDialCharacter(c), "rejected \(c)")
        }
        XCTAssertFalse(Network.ysf.admitsDialCharacter("é"))
    }

    /// YSF puts the operator's callsign in every frame's header, so a dial
    /// without one is refused before it reaches the engine.
    func testItRequiresACallsign() {
        XCTAssertTrue(CallSession.requiresCallsign(.ysf))
    }

    /// The directory's `ysf` rows now reach a network astar can actually
    /// dial, in both directions.
    func testItRoundTripsWithTheDirectorysNetwork() {
        XCTAssertEqual(Network.ysf.reflectorNetwork, .ysf)
        XCTAssertEqual(Network.matching(.ysf), .ysf)
    }
}
