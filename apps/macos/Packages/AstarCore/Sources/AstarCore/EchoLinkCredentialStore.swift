// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Where EchoLink passwords are kept, one per callsign.
///
/// A password is a secret, so it lives in the Keychain and nowhere else: not
/// in the defaults domain, not in `Credentials`, not in an `.astarconfig`
/// export and not in any published property. `CallSession` holds this STORE,
/// never a password; it reads one at the moment of a login and hands it
/// straight to the engine.
///
/// Its own Keychain item (`echolink-passwords`), beside the AllStarLink
/// account and the DMR passwords and never inside either, for the reason
/// `DmrPasswordStore` gives: two facts sharing one record would mean saving
/// one could create or delete the other.
///
/// Keyed by callsign, trimmed and upper-cased, because that is what EchoLink
/// issues a password against: `KX7ABC` and `KX7ABC-L` are separate
/// registrations with separate passwords.
public protocol EchoLinkCredentialStore {
    /// The password saved for `callsign`, or `nil` when none is — a
    /// confirmed absence. Blank counts as none. **Throws when the Keychain
    /// cannot be read**, which is not the same thing: the password may well
    /// be there.
    func password(callsign: String) throws -> String?
    /// The callsigns that have a password saved — keys only, never a
    /// password — so a form can say "saved" without reading one. Throws when
    /// the Keychain cannot be read.
    func savedCallsigns() throws -> Set<String>
    /// Save `password` for `callsign`. Throws, and writes nothing, when the
    /// existing passwords cannot be read, so a failed read never costs the
    /// other callsigns theirs.
    func save(_ password: String, callsign: String) throws
    func remove(callsign: String) throws
}

extension EchoLinkCredentialStore {
    /// The one spelling of a callsign both implementations key by.
    static func normalise(_ callsign: String) -> String {
        callsign.trimmingCharacters(in: .whitespacesAndNewlines).uppercased()
    }

    static func usable(_ password: String?) -> String? {
        guard let password, !password.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return nil }
        return password
    }
}

/// Why the Keychain store would not answer, beyond the Keychain's own errors.
public enum EchoLinkCredentialStoreError: Error, Equatable {
    /// The item is there but is not the JSON map this store writes. Nothing
    /// is written over it, so whatever it holds is not lost.
    case unreadableItem
}

/// `EchoLinkCredentialStore` backed by the Apple Keychain: one item holding a
/// JSON map of `CALLSIGN: password`.
public final class KeychainEchoLinkCredentialStore: EchoLinkCredentialStore {
    /// The Keychain account the map is filed under.
    public static let defaultAccount = "echolink-passwords"

    private let item: KeychainItem

    public init(
        service: String = KeychainService.current, account: String = defaultAccount,
        legacyService: String? = KeychainService.legacy
    ) {
        self.item = KeychainItem(service: service, account: account, legacyService: legacyService)
    }

    init(item: KeychainItem) {
        self.item = item
    }

    /// Every saved password. **Throws when the Keychain read fails, and when
    /// the item it reads does not decode**: neither is an empty map, and a
    /// save or remove that wrote one back would wipe the other callsigns'
    /// passwords. Only no item at all is an empty map.
    private func load() throws -> [String: String] {
        guard let data = try item.readChecked() else { return [:] }
        do {
            return try JSONDecoder().decode([String: String].self, from: data)
        } catch {
            // The decoder's own error can quote the item's bytes, which are
            // passwords: say only that it would not decode.
            throw EchoLinkCredentialStoreError.unreadableItem
        }
    }

    public func password(callsign: String) throws -> String? {
        Self.usable(try load()[Self.normalise(callsign)])
    }

    public func savedCallsigns() throws -> Set<String> {
        Set(try load().filter { Self.usable($0.value) != nil }.keys)
    }

    public func save(_ password: String, callsign: String) throws {
        var all = try load()
        all[Self.normalise(callsign)] = password
        try item.write(try JSONEncoder().encode(all))
    }

    public func remove(callsign: String) throws {
        var all = try load()
        guard all.removeValue(forKey: Self.normalise(callsign)) != nil else { return }
        // An empty map is an item worth removing outright rather than a
        // record of nothing.
        if all.isEmpty {
            try item.delete()
        } else {
            try item.write(try JSONEncoder().encode(all))
        }
    }
}

/// Non-persistent store for tests and SwiftUI previews.
public final class InMemoryEchoLinkCredentialStore: EchoLinkCredentialStore {
    private var passwords: [String: String]

    public init(_ initial: [String: String] = [:]) {
        self.passwords = initial.reduce(into: [:]) { $0[Self.normalise($1.key)] = $1.value }
    }

    public func password(callsign: String) throws -> String? {
        Self.usable(passwords[Self.normalise(callsign)])
    }

    public func savedCallsigns() throws -> Set<String> {
        Set(passwords.filter { Self.usable($0.value) != nil }.keys)
    }

    public func save(_ password: String, callsign: String) throws {
        passwords[Self.normalise(callsign)] = password
    }

    public func remove(callsign: String) throws {
        passwords.removeValue(forKey: Self.normalise(callsign))
    }
}
