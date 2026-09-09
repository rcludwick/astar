// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class CredentialStoreTests: XCTestCase {
    func testInMemoryStoreRoundTrips() throws {
        let store = InMemoryCredentialStore()
        XCTAssertNil(store.load())

        let creds = Credentials(portalUser: "rob", portalPass: "s3cret")
        try store.save(creds)
        XCTAssertEqual(store.load(), creds)

        try store.clear()
        XCTAssertNil(store.load())
    }

    func testCredentialsDebugRedactsPassword() {
        let creds = Credentials(portalUser: "rob", portalPass: "topsecret")
        // The secret-free contract: the password must never leak via logging.
        XCTAssertFalse(
            String(reflecting: creds).contains("topsecret"),
            "password must not appear in debug/reflected output"
        )
    }

    func testCredentialsCodableRoundTrips() throws {
        let creds = Credentials(portalUser: "rob", portalPass: "s3cret")
        let data = try JSONEncoder().encode(creds)
        let decoded = try JSONDecoder().decode(Credentials.self, from: data)
        XCTAssertEqual(decoded, creds)
    }

    func testInMemoryStoreSeedsWithInitialCredentials() {
        // The preview/test convenience: a store can be pre-seeded.
        let seed = Credentials(portalUser: "rob", portalPass: "p")
        let store = InMemoryCredentialStore(seed)
        XCTAssertEqual(store.load(), seed)
    }

    func testInMemoryStoreSaveReplacesExisting() throws {
        let store = InMemoryCredentialStore(
            Credentials(portalUser: "old", portalPass: "x"))
        let fresh = Credentials(portalUser: "new", portalPass: "y")
        try store.save(fresh)
        XCTAssertEqual(store.load(), fresh, "save replaces rather than appends")
    }

    // MARK: - DMR master passwords

    /// Its own store, and its own Keychain item. The invariant that matters:
    /// nothing here can produce a `Credentials`, so a DMR password can never
    /// bring an empty AllStarLink account into existence.
    func testTheDMRStoreRoundTripsPerSystemAndKnowsWhichAreSet() throws {
        let store = InMemoryDmrPasswordStore()
        XCTAssertNil(store.password(system: "tgif"))
        XCTAssertTrue(store.systems().isEmpty)

        try store.save("hunter2", system: "tgif")
        try store.save("other", system: "freedmr-network")
        XCTAssertEqual(store.password(system: "tgif"), "hunter2")
        XCTAssertEqual(store.systems(), ["tgif", "freedmr-network"])

        // Each network issues its own: one is never the other.
        XCTAssertNotEqual(store.password(system: "tgif"), store.password(system: "freedmr-network"))

        try store.remove(system: "tgif")
        XCTAssertNil(store.password(system: "tgif"))
        XCTAssertEqual(store.password(system: "freedmr-network"), "other")
    }

    /// A slug typed with different case or stray spaces must find the password
    /// a picker saved, or the dial refuses with one sitting right there.
    func testTheDMRStoreNormalisesTheSystemSlug() throws {
        let store = InMemoryDmrPasswordStore(["TGIF": "hunter2"])
        XCTAssertEqual(store.password(system: " tgif "), "hunter2")
    }

    /// A blank password is not a password: an empty login is a refusal the
    /// operator cannot interpret, so it reads as absent and the dial says so.
    func testABlankDMRPasswordCountsAsNone() throws {
        let store = InMemoryDmrPasswordStore(["tgif": "   "])
        XCTAssertNil(store.password(system: "tgif"))
        XCTAssertTrue(store.systems().isEmpty)
    }

    /// A Keychain blob written before 2026-09-09 carries `portalNode`; the
    /// decoder must ignore it rather than refuse the account.
    func testAnOldBlobWithANodeStillDecodes() throws {
        let json = #"{"portalUser":"rob","portalPass":"s3cret","portalNode":"77777"}"#
        let creds = try JSONDecoder().decode(Credentials.self, from: Data(json.utf8))
        XCTAssertEqual(creds, Credentials(portalUser: "rob", portalPass: "s3cret"))
    }
}
