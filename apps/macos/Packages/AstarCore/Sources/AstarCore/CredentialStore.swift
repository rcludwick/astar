// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// Persistent store for AllStar portal `Credentials`. The real implementation is
/// `KeychainCredentialStore`; `InMemoryCredentialStore` backs tests and previews.
public protocol CredentialStore {
    /// The stored credentials, or `nil` if none are saved.
    func load() -> Credentials?
    /// Persist (replacing any existing) credentials.
    func save(_ credentials: Credentials) throws
    /// Remove any stored credentials.
    func clear() throws
}

/// Non-persistent store for tests and SwiftUI previews.
public final class InMemoryCredentialStore: CredentialStore {
    private var credentials: Credentials?

    public init(_ initial: Credentials? = nil) {
        self.credentials = initial
    }

    public func load() -> Credentials? { credentials }
    public func save(_ credentials: Credentials) throws { self.credentials = credentials }
    public func clear() throws { credentials = nil }
}
