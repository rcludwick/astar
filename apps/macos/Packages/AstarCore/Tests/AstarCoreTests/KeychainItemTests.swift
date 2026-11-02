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
    /// When set, `readChecked` throws: a Keychain that would not say.
    var readFails = false
    var writes: [Key] = []
    var deletes: [Key] = []

    func read(service: String, account: String) -> Data? {
        items[Key(service: service, account: account)]
    }

    func readChecked(service: String, account: String) throws -> Data? {
        if readFails { throw KeychainError(status: errSecInteractionNotAllowed) }
        return items[Key(service: service, account: account)]
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
        XCTAssertEqual(
            KeychainService.legacy, "com.aj7hr.astar",
            "the name items were filed under before 0.1.14-beta")
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

        XCTAssertEqual(
            item(backend).read(), blob, "the data comes back on the very read that moves it")
        XCTAssertEqual(
            backend.items[.init(service: KeychainService.current, account: account)], blob)
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

        XCTAssertEqual(
            item(backend).read(), blob,
            "a Keychain that refuses the write must not lose the account")
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
        XCTAssertTrue(
            backend.items.isEmpty, "clearing the account must not leave a copy under the old name")
    }

    func testWithoutALegacyServiceOnlyTheCurrentNameIsConsulted() {
        let backend = FakeKeychainBackend()
        backend.items[.init(service: KeychainService.legacy, account: account)] = blob
        let plain = KeychainItem(
            service: KeychainService.current, account: account, backend: backend)

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

    // MARK: - SecItemBackend.write

    /// The `SecItem` calls one `write` makes, recorded instead of reaching
    /// the login Keychain.
    private final class SecItemCalls {
        var updates: [(query: [String: Any], attributes: [String: Any])] = []
        var adds: [[String: Any]] = []
        var updateStatus: OSStatus = errSecSuccess
        var addStatus: OSStatus = errSecSuccess

        var backend: SecItemBackend {
            SecItemBackend(
                update: { query, attributes in
                    self.updates.append(
                        (query as! [String: Any], attributes as! [String: Any]))
                    return self.updateStatus
                },
                add: { attributes in
                    self.adds.append(attributes as! [String: Any])
                    return self.addStatus
                })
        }
    }

    /// An existing item is updated in place — never deleted first, so a
    /// write that fails cannot lose it — and matched by the same three
    /// attributes it was always written with.
    func testWriteUpdatesAnExistingItemInPlace() throws {
        let calls = SecItemCalls()
        try calls.backend.write(service: "com.astar.app", account: "echolink-passwords", data: blob)
        XCTAssertEqual(calls.updates.count, 1)
        XCTAssertTrue(calls.adds.isEmpty, "nothing added when the update found the item")
        let query = try XCTUnwrap(calls.updates.first?.query)
        XCTAssertEqual(query.count, 3, "class, service and account, as before")
        XCTAssertEqual(query[kSecClass as String] as? String, kSecClassGenericPassword as String)
        XCTAssertEqual(query[kSecAttrService as String] as? String, "com.astar.app")
        XCTAssertEqual(query[kSecAttrAccount as String] as? String, "echolink-passwords")
        let attributes = try XCTUnwrap(calls.updates.first?.attributes)
        XCTAssertEqual(attributes.count, 1)
        XCTAssertEqual(attributes[kSecValueData as String] as? Data, blob)
    }

    func testWriteAddsAnItemThatIsNotThereYet() throws {
        let calls = SecItemCalls()
        calls.updateStatus = errSecItemNotFound
        try calls.backend.write(service: "com.astar.app", account: "allstar-portal", data: blob)
        XCTAssertEqual(calls.adds.count, 1)
        let added = try XCTUnwrap(calls.adds.first)
        XCTAssertEqual(added.count, 4, "the three matching attributes plus the data")
        XCTAssertEqual(added[kSecAttrAccount as String] as? String, "allstar-portal")
        XCTAssertEqual(added[kSecValueData as String] as? Data, blob)
    }

    func testAFailedUpdateThrowsAndAddsNothing() {
        let calls = SecItemCalls()
        calls.updateStatus = errSecInteractionNotAllowed
        XCTAssertThrowsError(
            try calls.backend.write(service: "com.astar.app", account: "dmr-passwords", data: blob))
        XCTAssertTrue(calls.adds.isEmpty)

        let adding = SecItemCalls()
        adding.updateStatus = errSecItemNotFound
        adding.addStatus = errSecDuplicateItem
        XCTAssertThrowsError(
            try adding.backend.write(service: "com.astar.app", account: "dmr-passwords", data: blob)
        )
    }
}
