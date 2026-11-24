// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The P25 address grammar: `host[:port]`, with an optional talkgroup label
/// after it.
final class P25DialTests: XCTestCase {

    func testABareHostTakesPort41000() {
        let parsed = P25Dial.parse("p25.example")
        XCTAssertEqual(parsed?.host, "p25.example")
        XCTAssertEqual(parsed?.port, 41000)
        XCTAssertNil(parsed?.talkgroup, "none was typed, so none is invented")
        XCTAssertEqual(P25Dial.defaultPort, 41000)
    }

    func testAPortAndTalkgroupParse() {
        let parsed = P25Dial.parse("p25.example:41001/10200")
        XCTAssertEqual(parsed?.host, "p25.example")
        XCTAssertEqual(parsed?.port, 41001)
        XCTAssertEqual(parsed?.talkgroup, 10200)
    }

    func testASpaceSeparatesTheTalkgroupToo() {
        let parsed = P25Dial.parse("p25.example 10200")
        XCTAssertEqual(parsed?.host, "p25.example")
        XCTAssertEqual(parsed?.port, 41000)
        XCTAssertEqual(parsed?.talkgroup, 10200)
    }

    /// P25 talkgroups are 24-bit on the wire.
    func testATwentyFourBitTalkgroupIsTheLimit() {
        XCTAssertEqual(P25Dial.parse("p25.example/16777215")?.talkgroup, 16_777_215)
        XCTAssertNil(P25Dial.parse("p25.example/16777216"))
        XCTAssertNil(P25Dial.parse("p25.example/0"))
        XCTAssertNil(P25Dial.parse("p25.example/99999999999999999999"))
    }

    /// `XLX836 A` is a D-Star dial that happens to look like a hostname;
    /// dropping the ` A` would link the operator to whatever DNS made of it.
    func testATrailingWordIsARejection() {
        XCTAssertNil(P25Dial.parse("XLX836 A"))
        XCTAssertNil(P25Dial.parse("p25.example/"))
        XCTAssertNil(P25Dial.parse("p25.example/-1"))
    }

    func testMalformedAddressesAreRejected() {
        XCTAssertNil(P25Dial.parse(""))
        XCTAssertNil(P25Dial.parse("   "))
        XCTAssertNil(P25Dial.parse(":41000"))
        XCTAssertNil(P25Dial.parse("a:b:c"))
        XCTAssertNil(P25Dial.parse("p25.example:0"))
        XCTAssertNil(P25Dial.parse("p25.example:70000"))
    }

    func testTheDirectoryBridgesBothWays() {
        XCTAssertEqual(Network.p25.reflectorNetwork, .p25)
        XCTAssertEqual(Network.matching(.p25), .p25)
        XCTAssertNil(Network.matching(.urf), "URF is still listed, not dialled")
    }
}
