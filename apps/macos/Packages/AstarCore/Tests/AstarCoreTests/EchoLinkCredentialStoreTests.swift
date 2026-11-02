// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `KeychainEchoLinkCredentialStore` over `FakeKeychainBackend`, so no test
/// reaches the login Keychain.
final class EchoLinkCredentialStoreTests: XCTestCase {

    private func item(_ backend: FakeKeychainBackend, account: String) -> KeychainItem {
        KeychainItem(
            service: KeychainService.current, account: account,
            legacyService: KeychainService.legacy, backend: backend)
    }

    func testPasswordRoundTripsPerCallsign() throws {
        let backend = FakeKeychainBackend()
        let store = KeychainEchoLinkCredentialStore(
            item: item(backend, account: "echolink-passwords"))

        XCTAssertNil(try store.password(callsign: "KX7ABC"))
        try store.save("first-pw", callsign: "KX7ABC")
        try store.save("second-pw", callsign: "W1AW")

        XCTAssertEqual(try store.password(callsign: "KX7ABC"), "first-pw")
        XCTAssertEqual(
            try store.password(callsign: " kx7abc "), "first-pw",
            "a callsign is one callsign however it was typed")
        XCTAssertEqual(try store.password(callsign: "W1AW"), "second-pw")

        try store.remove(callsign: "KX7ABC")
        XCTAssertNil(try store.password(callsign: "KX7ABC"))
        XCTAssertEqual(try store.password(callsign: "W1AW"), "second-pw", "only that callsign goes")

        try store.remove(callsign: "W1AW")
        XCTAssertTrue(backend.items.isEmpty, "an empty map is removed outright")

        try store.save("   ", callsign: "KX7ABC")
        XCTAssertNil(try store.password(callsign: "KX7ABC"), "a blank password counts as none")
    }

    func testEchoLinkItemIsSeparateFromDmrAndAccount() throws {
        let backend = FakeKeychainBackend()
        let creds = KeychainCredentialStore(item: item(backend, account: "allstar-portal"))
        let dmr = KeychainDmrPasswordStore(item: item(backend, account: "dmr-passwords"))
        let echolink = KeychainEchoLinkCredentialStore(
            item: item(backend, account: "echolink-passwords"))

        try creds.save(Credentials(portalUser: "AJ7HR", portalPass: "portal-pw"))
        try dmr.save("dmr-pw", system: "tgif")
        let before = backend.items

        try echolink.save("echo-pw", callsign: "KX7ABC")

        let allstarKey = FakeKeychainBackend.Key(
            service: KeychainService.current, account: "allstar-portal")
        let dmrKey = FakeKeychainBackend.Key(
            service: KeychainService.current, account: "dmr-passwords")
        XCTAssertEqual(backend.items[allstarKey], before[allstarKey], "AllStar item untouched")
        XCTAssertEqual(backend.items[dmrKey], before[dmrKey], "DMR item untouched")
        XCTAssertEqual(
            Set(backend.items.keys.map(\.account)),
            ["allstar-portal", "dmr-passwords", "echolink-passwords"])

        try echolink.remove(callsign: "KX7ABC")
        XCTAssertEqual(creds.load()?.portalPass, "portal-pw")
        XCTAssertEqual(dmr.password(system: "tgif"), "dmr-pw")
    }

    func testTheDefaultAccountIsItsOwn() {
        XCTAssertEqual(KeychainEchoLinkCredentialStore.defaultAccount, "echolink-passwords")
    }

    /// A Keychain read that fails is not an empty Keychain: writing the
    /// map back after one would wipe every other callsign's password.
    func testAFailedReadRefusesToWrite() throws {
        let backend = FakeKeychainBackend()
        let store = KeychainEchoLinkCredentialStore(
            item: item(backend, account: "echolink-passwords"))
        try store.save("first-pw", callsign: "KX7ABC")
        try store.save("second-pw", callsign: "W1AW")
        let before = backend.items
        let writesBefore = backend.writes.count

        backend.readFails = true
        XCTAssertThrowsError(try store.save("third-pw", callsign: "N0CALL"))
        XCTAssertThrowsError(try store.remove(callsign: "KX7ABC"))
        XCTAssertThrowsError(try store.savedCallsigns())
        XCTAssertThrowsError(
            try store.password(callsign: "KX7ABC"), "unreadable is not the same as none")
        XCTAssertEqual(backend.writes.count, writesBefore, "nothing written")
        XCTAssertEqual(backend.items, before, "nothing lost")

        backend.readFails = false
        XCTAssertEqual(try store.password(callsign: "W1AW"), "second-pw")
    }

    /// An item that is there but does not decode is not an empty map either:
    /// a save that wrote over it would wipe whatever it holds.
    func testAnItemThatDoesNotDecodeRefusesToWrite() throws {
        let backend = FakeKeychainBackend()
        let store = KeychainEchoLinkCredentialStore(
            item: item(backend, account: "echolink-passwords"))
        try store.save("first-pw", callsign: "KX7ABC")
        let key = FakeKeychainBackend.Key(
            service: KeychainService.current, account: "echolink-passwords")
        backend.items[key] = Data("not json".utf8)
        let before = backend.items
        let writesBefore = backend.writes.count

        XCTAssertThrowsError(try store.save("second-pw", callsign: "W1AW")) {
            XCTAssertEqual($0 as? EchoLinkCredentialStoreError, .unreadableItem)
        }
        XCTAssertThrowsError(try store.remove(callsign: "KX7ABC"))
        XCTAssertThrowsError(try store.savedCallsigns())
        XCTAssertThrowsError(try store.password(callsign: "KX7ABC"))
        XCTAssertEqual(backend.writes.count, writesBefore, "nothing written")
        XCTAssertEqual(backend.items, before, "nothing lost")
    }

    /// No item at all is an empty Keychain, which a save may start.
    func testNoItemIsAnEmptyMap() throws {
        let backend = FakeKeychainBackend()
        let store = KeychainEchoLinkCredentialStore(
            item: item(backend, account: "echolink-passwords"))
        XCTAssertEqual(try store.savedCallsigns(), [])
        try store.save("pw", callsign: "kx7abc")
        try store.save("  ", callsign: "W1AW")
        XCTAssertEqual(try store.savedCallsigns(), ["KX7ABC"], "keys only, blank ones left out")
    }
}
