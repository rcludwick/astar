// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The NXDN address grammar: `host[:port]`, with the talkgroup after it.
final class NXDNDialTests: XCTestCase {

    func testAHostAndPortParse() {
        let parsed = NXDNDial.parse("nxdn.example:41401")
        XCTAssertEqual(parsed?.host, "nxdn.example")
        XCTAssertEqual(parsed?.port, 41401)
        XCTAssertNil(parsed?.talkgroup, "none was typed, so none is invented")
    }

    func testAMissingPortTakesTheCommonOne() {
        XCTAssertEqual(NXDNDial.parse("nxdn.example")?.port, 41400)
        XCTAssertEqual(NXDNDial.defaultPort, 41400)
    }

    /// The talkgroup is part of the target, not a preference: an
    /// NXDNReflector relays exactly one, and drops every frame addressed
    /// anywhere else.
    func testATalkgroupIsReadFromEitherSeparator() {
        XCTAssertEqual(NXDNDial.parse("nxdn.example:41400/100")?.talkgroup, 100)
        XCTAssertEqual(NXDNDial.parse("nxdn.example 65000")?.talkgroup, 65000)
        XCTAssertEqual(NXDNDial.parse("nxdn.example/100")?.port, 41400)
        XCTAssertEqual(NXDNDial.parse("nxdn.example/100")?.host, "nxdn.example")
    }

    func testWhitespaceAroundTheTargetIsIgnored() {
        XCTAssertEqual(NXDNDial.parse("  nxdn.example:41400  ")?.host, "nxdn.example")
    }

    /// The rejection that matters. `XLX836 A` is a D-Star dial that happens to
    /// look like a hostname, and a parser that dropped the ` A` would link the
    /// operator to whatever DNS made of `XLX836`.
    func testAModuleSeparatorIsRefusedRatherThanIgnored() {
        XCTAssertNil(NXDNDial.parse("XLX836 A"))
        XCTAssertNil(NXDNDial.parse("nxdn.example A"))
        XCTAssertNil(NXDNDial.parse("nxdn.example/A"))
        XCTAssertNil(NXDNDial.parse("nxdn.example:41400/A"))
    }

    func testMalformedTargetsAreRefused() {
        XCTAssertNil(NXDNDial.parse(""), "empty")
        XCTAssertNil(NXDNDial.parse("   "), "whitespace only")
        XCTAssertNil(NXDNDial.parse(":41400"), "no host")
        XCTAssertNil(NXDNDial.parse("nxdn.example:0"), "port zero")
        XCTAssertNil(NXDNDial.parse("nxdn.example:99999"), "port out of range")
        XCTAssertNil(NXDNDial.parse("nxdn.example:abc"), "non-numeric port")
        XCTAssertNil(NXDNDial.parse("a:b:c"), "two colons")
        XCTAssertNil(NXDNDial.parse("nxdn.example/0"), "talkgroup zero addresses nobody")
        XCTAssertNil(NXDNDial.parse("nxdn.example/99999"), "talkgroup out of 16 bits")
        XCTAssertNil(NXDNDial.parse("nxdn.example/"), "a separator and nothing after it")
    }

    func testABareAddressParses() {
        let parsed = NXDNDial.parse("45.56.69.219:41401/100")
        XCTAssertEqual(parsed?.host, "45.56.69.219")
        XCTAssertEqual(parsed?.port, 41401)
        XCTAssertEqual(parsed?.talkgroup, 100)
    }
}

/// The operator's NXDN id — its own credential, and its own validator.
final class NxdnIDTests: XCTestCase {

    func testItKeepsOnlyDigits() {
        XCTAssertEqual(NxdnID.sanitized("  1 234 "), "1234")
        XCTAssertEqual(NxdnID.sanitized("12a34"), "1234")
        XCTAssertEqual(NxdnID.sanitized(""), "")
    }

    /// `0` addresses nobody and `65520` and up are reserved by the standard,
    /// so neither is an id a station may transmit as.
    func testItRefusesWhatTheWireCannotCarry() {
        XCTAssertEqual(NxdnID.value("1"), 1)
        XCTAssertEqual(NxdnID.value("65519"), 65519)
        XCTAssertNil(NxdnID.value("0"))
        XCTAssertNil(NxdnID.value("65520"))
        XCTAssertNil(NxdnID.value(""))
        XCTAssertNil(NxdnID.value("abc"))
    }

    /// A registered DMR ID is six or seven digits and does not fit in sixteen
    /// bits. This is the whole reason NXDN gets a field of its own — the
    /// alternative is truncating into somebody else's number.
    func testADMRSizedNumberIsNotAnNXDNID() {
        XCTAssertNil(NxdnID.value(RadioID.sanitized("3153591")))
    }
}

/// `Network.nxdn` as a picker segment: gated on hardware, receive only, and
/// carrying its own labels and dial grammar.
final class NetworkNXDNTests: XCTestCase {

    func testTheSegmentAppearsOnlyWithAVocoder() {
        XCTAssertFalse(Network.available(m17: false).contains(.nxdn))
        XCTAssertFalse(Network.available(m17: true, dstar: true, ysf: true).contains(.nxdn))
        XCTAssertTrue(Network.available(m17: false, nxdn: true).contains(.nxdn))
    }

    /// A persisted `.nxdn` selection on a machine with the dongle unplugged
    /// falls back rather than offering a network that cannot connect.
    func testAPersistedSelectionFallsBackWhenUnavailable() {
        XCTAssertEqual(Network.resolve("nxdn", m17: false), .allstar)
        XCTAssertEqual(Network.resolve("nxdn", m17: false, nxdn: true), .nxdn)
    }

    func testItCarriesItsOwnLabels() {
        XCTAssertEqual(Network.nxdn.displayName, "NXDN")
        XCTAssertEqual(Network.nxdn.badge, "NXDN")
        XCTAssertFalse(Network.nxdn.symbol.isEmpty)
        XCTAssertFalse(Network.nxdn.showsDialpad, "the dialpad is an AllStar concern")
        XCTAssertTrue(Network.nxdn.isDigitalVoice, "the last-heard line applies")
    }

    func testTheDialFieldAdmitsAddressCharacters() {
        XCTAssertTrue(Network.nxdn.admitsDialCharacter(":"))
        XCTAssertTrue(Network.nxdn.admitsDialCharacter("3"))
        XCTAssertFalse(Network.nxdn.admitsDialCharacter("#"))
        for c in "nxdn.example:41400/100" {
            XCTAssertTrue(Network.nxdn.admitsDialCharacter(c), "rejected \(c)")
        }
    }

    /// NXDN names the operator's callsign in the poll that registers the link,
    /// so a dial without one is refused before it reaches the engine.
    func testItRequiresACallsign() {
        XCTAssertTrue(CallSession.requiresCallsign(.nxdn))
    }

    /// The directory's `nxdn` rows now reach a network astar can dial, in both
    /// directions.
    func testItRoundTripsWithTheDirectorysNetwork() {
        XCTAssertEqual(Network.nxdn.reflectorNetwork, .nxdn)
        XCTAssertEqual(Network.matching(.nxdn), .nxdn)
    }

    /// `DirectoryEntry.key` is `network:id` precisely so an NXDN talkgroup and
    /// a P25 one that happen to share a number are two rows.
    func testNXDNAndP25TalkgroupOneHundredDoNotCollide() {
        let a = DirectoryEntry(
            network: .nxdn, id: "100", name: "NXDN 100",
            dial: .nxdn(host: "a.example", port: 41400))
        let b = DirectoryEntry(
            network: .p25, id: "100", name: "P25 100",
            dial: .p25(host: "b.example", port: 41000))
        XCTAssertNotEqual(a.key, b.key)
    }
}
