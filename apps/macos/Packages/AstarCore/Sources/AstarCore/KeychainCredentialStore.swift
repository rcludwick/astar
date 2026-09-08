// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation
import Security

/// One `kSecClassGenericPassword` item, read and written as an opaque blob.
///
/// Extracted so the two things astar keeps in the Keychain — the AllStarLink
/// account and the DMR master passwords — are two SEPARATE items sharing one
/// piece of `SecItem` plumbing, rather than two halves of one payload. That
/// separation is the point: a DMR password saved by someone who has no
/// AllStarLink account must not bring an (empty) account into existence, and
/// clearing the account must not take somebody's DMR passwords with it.
struct KeychainItem {
    let service: String
    let account: String

    private var baseQuery: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    func read() -> Data? {
        var query = baseQuery
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
            let data = item as? Data
        else { return nil }
        return data
    }

    func write(_ data: Data) throws {
        SecItemDelete(baseQuery as CFDictionary)  // upsert: clear then add
        var add = baseQuery
        add[kSecValueData as String] = data
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else { throw KeychainError(status: status) }
    }

    func delete() throws {
        let status = SecItemDelete(baseQuery as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError(status: status)
        }
    }
}

/// `CredentialStore` backed by the Apple Keychain (a `kSecClassGenericPassword`
/// item). The whole `Credentials` value — including the password — lives only
/// here, encrypted at rest; nothing is written to config files or logs.
public final class KeychainCredentialStore: CredentialStore {
    private let item: KeychainItem

    public init(service: String = "com.aj7hr.astar", account: String = "allstar-portal") {
        self.item = KeychainItem(service: service, account: account)
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
