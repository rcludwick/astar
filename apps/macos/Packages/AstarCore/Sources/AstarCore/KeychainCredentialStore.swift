// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation
import Security

/// The Keychain service both of astar's items are filed under. It is what
/// Keychain Access shows as the item's name, so it has to be astar's name and
/// nobody's callsign: the first release used `com.aj7hr.astar` (the bundle
/// id), and the first thing a user saw in Keychain Access was the author's
/// callsign (issue #1). The bundle id itself is unchanged — it is a signing
/// identity and a preferences domain, not something a user reads.
public enum KeychainService {
    public static let current = "com.astar.app"
    /// Where items written before `0.1.14-beta` live. Read once, moved, deleted.
    public static let legacy = "com.aj7hr.astar"
}

/// The raw `SecItem` calls behind [`KeychainItem`], as a protocol so the item
/// logic — and the one-time move from the legacy service — is testable without
/// a unit test ever touching the login Keychain.
protocol KeychainBackend {
    func read(service: String, account: String) -> Data?
    func write(service: String, account: String, data: Data) throws
    func delete(service: String, account: String) throws
}

struct SecItemBackend: KeychainBackend {
    private func baseQuery(service: String, account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    func read(service: String, account: String) -> Data? {
        var query = baseQuery(service: service, account: account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
            let data = item as? Data
        else { return nil }
        return data
    }

    func write(service: String, account: String, data: Data) throws {
        let base = baseQuery(service: service, account: account)
        SecItemDelete(base as CFDictionary)  // upsert: clear then add
        var add = base
        add[kSecValueData as String] = data
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else { throw KeychainError(status: status) }
    }

    func delete(service: String, account: String) throws {
        let status = SecItemDelete(baseQuery(service: service, account: account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError(status: status)
        }
    }
}

/// One `kSecClassGenericPassword` item, read and written as an opaque blob.
///
/// Extracted so the two things astar keeps in the Keychain — the AllStarLink
/// account and the DMR master passwords — are two SEPARATE items sharing one
/// piece of `SecItem` plumbing, rather than two halves of one payload. That
/// separation is the point: a DMR password saved by someone who has no
/// AllStarLink account must not bring an (empty) account into existence, and
/// clearing the account must not take somebody's DMR passwords with it.
///
/// **The legacy service.** When `legacyService` is set and nothing is filed
/// under `service`, a read falls back to the legacy item and, if it finds one,
/// moves it: written under `service`, deleted from the legacy one. The move is
/// best effort — the data is returned either way, so a Keychain that refuses
/// the write costs nothing but another attempt next launch — and it happens
/// on read rather than on save because a user who never re-saves must still
/// stop seeing the old name in Keychain Access.
struct KeychainItem {
    let service: String
    let account: String
    var legacyService: String?
    var backend: KeychainBackend = SecItemBackend()

    init(
        service: String, account: String, legacyService: String? = nil,
        backend: KeychainBackend = SecItemBackend()
    ) {
        self.service = service
        self.account = account
        self.legacyService = legacyService
        self.backend = backend
    }

    func read() -> Data? {
        if let data = backend.read(service: service, account: account) {
            return data
        }
        guard let legacy = legacyService, legacy != service,
            let data = backend.read(service: legacy, account: account)
        else { return nil }
        if (try? backend.write(service: service, account: account, data: data)) != nil {
            try? backend.delete(service: legacy, account: account)
        }
        return data
    }

    func write(_ data: Data) throws {
        try backend.write(service: service, account: account, data: data)
    }

    func delete() throws {
        try backend.delete(service: service, account: account)
        if let legacy = legacyService, legacy != service {
            try backend.delete(service: legacy, account: account)
        }
    }
}

/// `CredentialStore` backed by the Apple Keychain (a `kSecClassGenericPassword`
/// item). The whole `Credentials` value — including the password — lives only
/// here, encrypted at rest; nothing is written to config files or logs.
public final class KeychainCredentialStore: CredentialStore {
    private let item: KeychainItem

    public init(
        service: String = KeychainService.current, account: String = "allstar-portal",
        legacyService: String? = KeychainService.legacy
    ) {
        self.item = KeychainItem(service: service, account: account, legacyService: legacyService)
    }

    init(item: KeychainItem) {
        self.item = item
    }

    public func load() -> Credentials? {
        guard let data = item.read() else { return nil }
        return try? JSONDecoder().decode(Credentials.self, from: data)
    }

    public func save(_ credentials: Credentials) throws {
        try item.write(try JSONEncoder().encode(credentials))
    }

    public func clear() throws {
        try item.delete()
    }
}

/// A Keychain operation failure carrying the `OSStatus`.
public struct KeychainError: Error {
    public let status: OSStatus
}
