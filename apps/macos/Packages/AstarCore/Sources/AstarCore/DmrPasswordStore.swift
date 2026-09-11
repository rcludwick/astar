// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Persistent store for DMR master passwords, one per network.
///
/// **Its own store, and its own Keychain item, deliberately.** These are the
/// same *kind* of thing as the AllStarLink account password — a secret, so the
/// Keychain and nowhere else — but they are not part of that account, and
/// folding them into `Credentials` made two facts share one record with
/// consequences in both directions: saving a TGIF password would have brought
/// an empty AllStarLink account into existence (`hasCredentials` true, the
/// "add your account" prompt gone, AllStar Connect enabled against nothing),
/// and clearing the account would have deleted every DMR password with it.
/// Two items, two lifetimes.
///
/// Keyed by the DIRECTORY's `system` slug — `tgif`, `freedmr-network`,
/// `ipsc2-poland`, `brandmeister` — because that is the granularity a password
/// is issued at (each DMR network issues its own) and the string a dial
/// already carries.
///
/// The values are read at the moment of a dial and handed to the engine by
/// value. Nothing that holds one of these is `@Published`, exported, or logged.
public protocol DmrPasswordStore {
    /// The master password for `system`, or `nil` when none is saved. Blank
    /// counts as none: a login with an empty password is a refusal the operator
    /// cannot interpret.
    func password(system: String) -> String?
    /// Which systems have a password saved. For the UI's "saved / not set"
    /// state — it must never have to read a secret back to answer that.
    func systems() -> Set<String>
    func save(_ password: String, system: String) throws
    func remove(system: String) throws
}

extension DmrPasswordStore {
    /// The one normalisation both implementations share, so a password saved
    /// from a picker is found by a dial that spelled the slug differently.
    static func normalise(_ system: String) -> String {
        system.trimmingCharacters(in: .whitespaces).lowercased()
    }

    static func usable(_ password: String?) -> String? {
        guard let trimmed = password?.trimmingCharacters(in: .whitespacesAndNewlines),
            !trimmed.isEmpty
        else { return nil }
        return trimmed
    }
}

/// `DmrPasswordStore` backed by the Apple Keychain, in an item of its own
/// (`account: "dmr-passwords"`) beside — never inside — the AllStarLink one.
public final class KeychainDmrPasswordStore: DmrPasswordStore {
    private let item: KeychainItem

    public init(
        service: String = KeychainService.current, account: String = "dmr-passwords",
        legacyService: String? = KeychainService.legacy
    ) {
        self.item = KeychainItem(service: service, account: account, legacyService: legacyService)
    }

    init(item: KeychainItem) {
        self.item = item
    }

    private func load() -> [String: String] {
        guard let data = item.read(),
            let decoded = try? JSONDecoder().decode([String: String].self, from: data)
        else { return [:] }
        return decoded
    }

    public func password(system: String) -> String? {
        Self.usable(load()[Self.normalise(system)])
    }

    public func systems() -> Set<String> {
        Set(load().filter { Self.usable($0.value) != nil }.keys)
    }

    public func save(_ password: String, system: String) throws {
        var all = load()
        all[Self.normalise(system)] = password
        try item.write(try JSONEncoder().encode(all))
    }

    public func remove(system: String) throws {
        var all = load()
        all.removeValue(forKey: Self.normalise(system))
        // An empty map is an item worth removing outright rather than a record
        // of nothing.
        if all.isEmpty {
            try item.delete()
        } else {
            try item.write(try JSONEncoder().encode(all))
        }
    }
}

/// Non-persistent store for tests and SwiftUI previews.
public final class InMemoryDmrPasswordStore: DmrPasswordStore {
    private var passwords: [String: String]

    public init(_ initial: [String: String] = [:]) {
        self.passwords = initial.reduce(into: [:]) { $0[Self.normalise($1.key)] = $1.value }
    }

    public func password(system: String) -> String? {
        Self.usable(passwords[Self.normalise(system)])
    }

    public func systems() -> Set<String> {
        Set(passwords.filter { Self.usable($0.value) != nil }.keys)
    }

    public func save(_ password: String, system: String) throws {
        passwords[Self.normalise(system)] = password
    }

    public func remove(system: String) throws {
        passwords.removeValue(forKey: Self.normalise(system))
    }
}
