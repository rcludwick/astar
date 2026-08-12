// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Table tests for `M17Dial.parse` — the M17 dial-field grammar (astar-c2e5/
/// iax-f2b8 Task 8): `host[:port]/module` or `host[:port] module`.
final class M17DialTests: XCTestCase {
    private typealias Parsed = (host: String, port: UInt16, module: Character)

    private func assertParses(
        _ raw: String, host: String, port: UInt16, module: Character,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        guard let result = M17Dial.parse(raw) else {
            return XCTFail("expected a parse for \(raw.debugDescription)", file: file, line: line)
        }
        XCTAssertEqual(result.host, host, "host", file: file, line: line)
        XCTAssertEqual(result.port, port, "port", file: file, line: line)
        XCTAssertEqual(result.module, module, "module", file: file, line: line)
    }

    private func assertNil(
        _ raw: String, _ message: String = "", file: StaticString = #filePath, line: UInt = #line
    ) {
        let reason = message.isEmpty ? "" : " (\(message))"
        XCTAssertNil(
            M17Dial.parse(raw), "expected nil for \(raw.debugDescription)\(reason)",
            file: file, line: line)
    }

    // MARK: - Slash grammar

    func testSlashSeparatorWithDefaultPort() {
        assertParses("m17.example.net/A", host: "m17.example.net", port: 17000, module: "A")
    }

    func testSlashSeparatorWithExplicitPort() {
        assertParses(
            "m17.example.net:17001/A", host: "m17.example.net", port: 17001, module: "A")
    }

    // MARK: - Space grammar

    func testSpaceSeparatorWithDefaultPort() {
        assertParses("m17.example.net A", host: "m17.example.net", port: 17000, module: "A")
    }

    func testSpaceSeparatorWithExplicitPort() {
        assertParses(
            "m17.example.net:17001 A", host: "m17.example.net", port: 17001, module: "A")
    }

    func testExtraSpaceBeforeModuleIsTolerated() {
        assertParses("m17.example.net  A", host: "m17.example.net", port: 17000, module: "A")
    }

    // MARK: - Module case-folding

    func testModuleIsUppercased() {
        assertParses("m17.example.net/a", host: "m17.example.net", port: 17000, module: "A")
        assertParses("m17.example.net z", host: "m17.example.net", port: 17000, module: "Z")
    }

    // MARK: - Whitespace trimming

    func testSurroundingWhitespaceIsTrimmed() {
        assertParses("  m17.example.net/A  ", host: "m17.example.net", port: 17000, module: "A")
    }

    // MARK: - Rejections

    func testEmptyAndWhitespaceOnlyRejected() {
        assertNil("")
        assertNil("   ")
    }

    func testNoSeparatorRejected() {
        assertNil("m17.example.net", "no module at all")
        assertNil("m17.example.net:17001", "port but no module")
    }

    func testEmptyModuleRejected() {
        assertNil("m17.example.net/")
        assertNil("m17.example.net/ ")
    }

    func testMultiCharacterModuleRejected() {
        assertNil("m17.example.net/AB")
        assertNil("m17.example.net AB")
    }

    func testNonLetterModuleRejected() {
        assertNil("m17.example.net/1")
        assertNil("m17.example.net/#")
    }

    func testEmptyHostRejected() {
        assertNil("/A")
        assertNil(":17000/A")
        assertNil(" /A")
    }

    func testHostWithInternalSpaceRejected() {
        // The first space is consumed as the module separator, leaving a
        // multi-character "module" — which is itself invalid.
        assertNil("my host/A")
    }

    func testMoreThanOneColonRejected() {
        assertNil("host:1:2/A")
    }

    func testNonNumericPortRejected() {
        assertNil("host:abc/A")
    }

    func testZeroPortRejected() {
        assertNil("host:0/A")
    }

    func testOutOfRangePortRejected() {
        assertNil("host:99999/A")
    }

    func testEmptyPortAfterColonRejected() {
        assertNil("host:/A")
    }
}
