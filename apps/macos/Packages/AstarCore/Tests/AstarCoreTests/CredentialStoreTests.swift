// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class CredentialStoreTests: XCTestCase {
    func testInMemoryStoreRoundTrips() throws {
        let store = InMemoryCredentialStore()
        XCTAssertNil(store.load())

        let creds = Credentials(portalUser: "rob", portalPass: "s3cret", portalNode: "77777")
        try store.save(creds)
        XCTAssertEqual(store.load(), creds)

        try store.clear()
        XCTAssertNil(store.load())
    }

    func testCredentialsDebugRedactsPassword() {
        let creds = Credentials(portalUser: "rob", portalPass: "topsecret", portalNode: "77777")
        // The secret-free contract: the password must never leak via logging.
        XCTAssertFalse(
            String(reflecting: creds).contains("topsecret"),
            "password must not appear in debug/reflected output"
        )
    }

    func testCredentialsCodableRoundTrips() throws {
        let creds = Credentials(portalUser: "rob", portalPass: "s3cret", portalNode: "77777")
        let data = try JSONEncoder().encode(creds)
        let decoded = try JSONDecoder().decode(Credentials.self, from: data)
        XCTAssertEqual(decoded, creds)
    }

    func testInMemoryStoreSeedsWithInitialCredentials() {
        // The preview/test convenience: a store can be pre-seeded.
        let seed = Credentials(portalUser: "rob", portalPass: "p", portalNode: "1")
        let store = InMemoryCredentialStore(seed)
        XCTAssertEqual(store.load(), seed)
    }

    func testInMemoryStoreSaveReplacesExisting() throws {
        let store = InMemoryCredentialStore(
            Credentials(portalUser: "old", portalPass: "x", portalNode: "1"))
        let fresh = Credentials(portalUser: "new", portalPass: "y", portalNode: "2")
        try store.save(fresh)
        XCTAssertEqual(store.load(), fresh, "save replaces rather than appends")
    }
}
