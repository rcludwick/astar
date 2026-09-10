// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Security
import XCTest

@testable import AstarCore

/// A `KeychainBackend` over a dictionary, so `KeychainItem`'s logic — above all
/// the one-time move from the legacy service name — is proven without any
/// test reaching the login Keychain (that is what astar-kcprompt was about).
final class FakeKeychainBackend: KeychainBackend {
    struct Key: Hashable {
        let service: String
        let account: String
    }
    var items: [Key: Data] = [:]
    var writeFails = false
    var writes: [Key] = []
    var deletes: [Key] = []

    func read(service: String, account: String) -> Data? {
        items[Key(service: service, account: account)]
    }

    func write(service: String, account: String, data: Data) throws {
        let key = Key(service: service, account: account)
        writes.append(key)
        if writeFails { throw KeychainError(status: errSecNotAvailable) }
        items[key] = data
    }

    func delete(service: String, account: String) throws {
        let key = Key(service: service, account: account)
        deletes.append(key)
        items.removeValue(forKey: key)
    }
}

final class KeychainItemTests: XCTestCase {
    private let account = "allstar-portal"
    private let blob = Data("secret".utf8)

    private func item(_ backend: FakeKeychainBackend) -> KeychainItem {
        KeychainItem(
            service: KeychainService.current, account: account,
            legacyService: KeychainService.legacy, backend: backend)
    }

    func testTheServiceNameIsAstarsAndNotACallsign() {
        XCTAssertEqual(KeychainService.current, "com.astar.app")
        XCTAssertFalse(KeychainService.current.lowercased().contains("aj7hr"))
        XCTAssertEqual(KeychainService.legacy, "com.aj7hr.astar", "the name items were filed under before 0.1.14-beta")
    }

    func testAnItemUnderTheCurrentServiceIsReadWithoutTouchingTheLegacyOne() {
        let backend = FakeKeychainBackend()
        backend.items[.init(service: KeychainService.current, account: account)] = blob
        backend.items[.init(service: KeychainService.legacy, account: account)] = Data("stale".utf8)

        XCTAssertEqual(item(backend).read(), blob)
        XCTAssertTrue(backend.writes.isEmpty, "nothing to migrate when the current item exists")
        XCTAssertTrue(backend.deletes.isEmpty)
    }

    func testALegacyItemIsMovedToTheCurrentServiceOnFirstRead() {
        let backend = FakeKeychainBackend()
        backend.items[.init(service: KeychainService.legacy, account: account)] = blob

        XCTAssertEqual(item(backend).read(), blob, "the data comes back on the very read that moves it")
        XCTAssertEqual(backend.items[.init(service: KeychainService.current, account: account)], blob)
        XCTAssertNil(
            backend.items[.init(service: KeychainService.legacy, account: account)],
            "the old name is gone from Keychain Access, not left as a duplicate")

        // Second read: served from the current service, no further churn.
        backend.writes.removeAll()
        backend.deletes.removeAll()
        XCTAssertEqual(item(backend).read(), blob)
        XCTAssertTrue(backend.writes.isEmpty)
        XCTAssertTrue(backend.deletes.isEmpty)
    }

    func testAFailedMoveStillReturnsTheDataAndKeepsTheLegacyItem() {
        let backend = FakeKeychainBackend()
        backend.items[.init(service: KeychainService.legacy, account: account)] = blob
        backend.writeFails = true

        XCTAssertEqual(item(backend).read(), blob, "a Keychain that refuses the write must not lose the account")
        XCTAssertEqual(
            backend.items[.init(service: KeychainService.legacy, account: account)], blob,
            "the legacy item is only deleted once the copy is known to be written")
        XCTAssertTrue(backend.deletes.isEmpty)
    }

    func testNothingAnywhereReadsAsNil() {
        let backend = FakeKeychainBackend()
        XCTAssertNil(item(backend).read())
        XCTAssertTrue(backend.writes.isEmpty)
    }

    func testDeleteClearsBothNames() throws {
        let backend = FakeKeychainBackend()
        backend.items[.init(service: KeychainService.current, account: account)] = blob
        backend.items[.init(service: KeychainService.legacy, account: account)] = blob

        try item(backend).delete()
        XCTAssertTrue(backend.items.isEmpty, "clearing the account must not leave a copy under the old name")
    }

    func testWithoutALegacyServiceOnlyTheCurrentNameIsConsulted() {
        let backend = FakeKeychainBackend()
        backend.items[.init(service: KeychainService.legacy, account: account)] = blob
        let plain = KeychainItem(service: KeychainService.current, account: account, backend: backend)

        XCTAssertNil(plain.read())
    }

    func testTheTwoStoresShareTheServiceAndKeepSeparateAccounts() throws {
        let backend = FakeKeychainBackend()
        let creds = KeychainCredentialStore(
            item: KeychainItem(
                service: KeychainService.current, account: "allstar-portal",
                legacyService: KeychainService.legacy, backend: backend))
        let dmr = KeychainDmrPasswordStore(
            item: KeychainItem(
                service: KeychainService.current, account: "dmr-passwords",
                legacyService: KeychainService.legacy, backend: backend))

        try creds.save(Credentials(portalUser: "AJ7HR", portalPass: "pw"))
        try dmr.save("dmrpw", system: "brandmeister")

        XCTAssertEqual(
            Set(backend.items.keys.map(\.service)), [KeychainService.current])
        XCTAssertEqual(
            Set(backend.items.keys.map(\.account)), ["allstar-portal", "dmr-passwords"])
        XCTAssertEqual(creds.load()?.portalUser, "AJ7HR")
        XCTAssertEqual(dmr.password(system: "brandmeister"), "dmrpw")
    }
}
