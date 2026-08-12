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
