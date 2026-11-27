// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Table tests for `DialTarget.parse` — the smart dial-field classifier
/// (astar-427f). One field accepts a node number OR a direct address:
/// digits (with optional `*`/`#` command prefixes) dial as a node; anything
/// with a dot/colon/letters dials as host or host:port; garbage parses to
/// nil so the Connect button stays disabled.
final class DialTargetTests: XCTestCase {
    func testNodeNumbers() {
        XCTAssertEqual(DialTarget.parse("55553"), .node("55553"))
        XCTAssertEqual(DialTarget.parse("2"), .node("2"))
    }

    func testCommandPrefixesStayOnTheNodePath() {
        // `*`/`#` are AllStar command-dial characters, not address syntax —
        // "*3" + digits must keep dialing as a node string (astar-75fc keys).
        XCTAssertEqual(DialTarget.parse("*355553"), .node("*355553"))
        XCTAssertEqual(DialTarget.parse("#1"), .node("#1"))
        XCTAssertEqual(DialTarget.parse("*70"), .node("*70"))
    }

    func testBareIPv4IsAnAddress() {
        XCTAssertEqual(DialTarget.parse("127.0.0.1"), .address("127.0.0.1"))
    }

    func testHostPortIsAnAddress() {
        XCTAssertEqual(DialTarget.parse("127.0.0.1:4569"), .address("127.0.0.1:4569"))
        XCTAssertEqual(DialTarget.parse("10.0.0.5:14569"), .address("10.0.0.5:14569"))
    }

    func testHostnameIsAnAddress() {
        XCTAssertEqual(DialTarget.parse("localhost"), .address("localhost"))
        XCTAssertEqual(
            DialTarget.parse("node.example-net.com"), .address("node.example-net.com"))
    }

    func testHostnameWithPortIsAnAddress() {
        XCTAssertEqual(DialTarget.parse("localhost:4569"), .address("localhost:4569"))
    }

    func testDigitsWithPortIsAnAddress() {
        // A colon is address syntax even when the host part is all digits.
        XCTAssertEqual(DialTarget.parse("12345:4569"), .address("12345:4569"))
    }

    func testNodeAtAddressCarriesBothHalves() {
        // node@host[:port]: the node is the WT destination, the address is
        // where the call goes. A bare address cannot name a node, which is
        // why stock ASL3 nodes hang up on it after the greeting.
        XCTAssertEqual(
            DialTarget.parse("55553@127.0.0.1:4569"),
            .nodeAtAddress(node: "55553", address: "127.0.0.1:4569"))
        XCTAssertEqual(
            DialTarget.parse("55553@127.0.0.1"),
            .nodeAtAddress(node: "55553", address: "127.0.0.1"))
        XCTAssertEqual(
            DialTarget.parse("2@localhost:14569"),
            .nodeAtAddress(node: "2", address: "localhost:14569"))
        XCTAssertEqual(
            DialTarget.parse(" 55553@node.example-net.com "),
            .nodeAtAddress(node: "55553", address: "node.example-net.com"))
    }

    func testMalformedNodeAtAddressParsesToNil() {
        XCTAssertNil(DialTarget.parse("@127.0.0.1"), "node half required")
        XCTAssertNil(DialTarget.parse("55553@"), "address half required")
        XCTAssertNil(DialTarget.parse("55553@@127.0.0.1"), "exactly one @")
        XCTAssertNil(DialTarget.parse("1@2@127.0.0.1"), "exactly one @")
        XCTAssertNil(DialTarget.parse("*355553@127.0.0.1"), "a destination, not a command")
        XCTAssertNil(DialTarget.parse("node@127.0.0.1"), "node half is digits only")
        XCTAssertNil(DialTarget.parse("55553@12345"), "right half must be an address")
        XCTAssertNil(DialTarget.parse("55553@host:0"), "address half is validated")
        XCTAssertNil(DialTarget.parse("55553 @127.0.0.1"), "no inner whitespace")
        XCTAssertNil(DialTarget.parse("55553@ 127.0.0.1"), "no space after the @ either")
    }

    func testBareAddressGetsTheNodeAtAddressHint() {
        XCTAssertEqual(
            DialTarget.bareAddressHint(" 127.0.0.1:4569 "),
            "To reach an AllStarLink node, put its number first: node@127.0.0.1:4569.")
        XCTAssertEqual(
            DialTarget.bareAddressHint("localhost"),
            "To reach an AllStarLink node, put its number first: node@localhost.")
    }

    func testOnlyABareAddressGetsTheHint() {
        XCTAssertNil(DialTarget.bareAddressHint("55553"), "a node number")
        XCTAssertNil(DialTarget.bareAddressHint("*355553"), "a command dial")
        XCTAssertNil(DialTarget.bareAddressHint("55553@127.0.0.1:4569"), "already names a node")
        XCTAssertNil(DialTarget.bareAddressHint(""), "empty field")
        XCTAssertNil(DialTarget.bareAddressHint("host:0"), "malformed")
    }

    func testSpokenAndNodeHalf() {
        XCTAssertEqual(DialTarget.spoken("77777@127.0.0.1:4569"), "77777 at 127.0.0.1:4569")
        XCTAssertEqual(DialTarget.spoken(" 77777 "), "77777")
        XCTAssertEqual(DialTarget.spoken("localhost:4569"), "localhost:4569")
        XCTAssertEqual(DialTarget.nodeHalf("77777@localhost"), "77777")
        XCTAssertNil(DialTarget.nodeHalf("77777"))
        XCTAssertNil(DialTarget.nodeHalf("localhost"))
    }

    func testWhitespaceIsTrimmed() {
        XCTAssertEqual(DialTarget.parse("  55553  "), .node("55553"))
        XCTAssertEqual(DialTarget.parse(" example.com "), .address("example.com"))
    }

    func testEmptyAndGarbageParseToNil() {
        XCTAssertNil(DialTarget.parse(""))
        XCTAssertNil(DialTarget.parse("   "))
        XCTAssertNil(DialTarget.parse("*"), "command prefix with no digits is not a dial")
        XCTAssertNil(DialTarget.parse("#*#"))
        XCTAssertNil(DialTarget.parse("hello world"), "inner whitespace is never valid")
        XCTAssertNil(DialTarget.parse("."))
        XCTAssertNil(DialTarget.parse(".example.com"), "host can't start with a dot")
        XCTAssertNil(DialTarget.parse("example.com."), "host can't end with a dot")
        XCTAssertNil(DialTarget.parse("-host"), "host can't start with a hyphen")
        XCTAssertNil(DialTarget.parse("host-"), "host can't end with a hyphen")
        XCTAssertNil(DialTarget.parse("a..b"), "empty hostname label")
        XCTAssertNil(DialTarget.parse("host:"), "colon requires a port")
        XCTAssertNil(DialTarget.parse(":4569"), "port requires a host")
        XCTAssertNil(DialTarget.parse("host:abc"), "port must be numeric")
        XCTAssertNil(DialTarget.parse("host:0"), "port 0 is not dialable")
        XCTAssertNil(DialTarget.parse("host:99999"), "port must fit 1...65535")
        XCTAssertNil(DialTarget.parse("a:1:2"), "at most one colon (no IPv6 form yet)")
        XCTAssertNil(DialTarget.parse("*3.example.com"), "command chars never mix with hosts")
    }
}
