// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// Decoding hamcall-db's reflector feed.
///
/// Half of these are degradation tests, and that is the point: the feed is not
/// astar's to control. Networks and dial kinds get added to it, rows arrive
/// malformed, and none of that may cost the rows around them. A directory that
/// throws on an unfamiliar row is a directory that goes empty the week
/// hamcall-db adds DMR — and an empty picker is indistinguishable from a
/// broken feature.
final class ReflectorFeedDecodingTests: XCTestCase {

    /// A real slice of `api/v1/reflectors.json`, one row per network, kept
    /// verbatim (envelope included) so the shipped contract has a fixture.
    private func slice() throws -> ReflectorFeed {
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: "reflectors-slice", withExtension: "json", subdirectory: "Fixtures"),
            "fixture missing from the test bundle")
        return try ReflectorFeed.decode(try Data(contentsOf: url))
    }

    private func feed(_ json: String) throws -> ReflectorFeed {
        try ReflectorFeed.decode(Data(json.utf8))
    }

    // MARK: - The published contract

    /// The envelope is decoded, not skipped past. `client_refresh_days` drives
    /// sync policy and the attribution is a CC BY condition — a decoder that
    /// kept only the rows would turn a licence obligation into a breach.
    func testEnvelopeMetadataSurvivesDecoding() throws {
        let feed = try slice()
        XCTAssertEqual(feed.schemaVersion, 1)
        XCTAssertEqual(feed.apiVersion, "v1")
        XCTAssertEqual(feed.clientRefreshDays, 7)
        XCTAssertEqual(feed.generated, "2026-08-26")
        XCTAssertEqual(feed.declaredCount, feed.entries.count)
        XCTAssertEqual(feed.license, "CC BY 4.0")
        XCTAssertEqual(feed.licenseURL, "https://creativecommons.org/licenses/by/4.0/")
        XCTAssertTrue(try XCTUnwrap(feed.attribution).contains("DVRef"))
        XCTAssertTrue(try XCTUnwrap(feed.attribution).contains("LX1IQ"))
        XCTAssertNotNil(feed.modifications)
    }

    func testDStarRowDecodesAsDextra() throws {
        let entry = try XCTUnwrap(try slice().entries.first { $0.id == "XLX836" })
        XCTAssertEqual(entry.network, .dstar)
        XCTAssertEqual(entry.aliases, ["XRF836"])
        XCTAssertEqual(entry.country, "USA")
        XCTAssertEqual(
            entry.dial,
            .dextra(host: "45.56.69.219", port: 30001, callsign: "XRF836", modules: []))
        XCTAssertTrue(entry.isDialable)
    }

    /// Every D-Star row publishes an empty `modules`: the XLX registry does not
    /// say which are active. Pinned because it is the fact the "no default
    /// module" rule rests on — there is nothing to default *from*.
    func testDStarRowsPublishNoModules() throws {
        let dstar = try slice().entries.filter { $0.network == .dstar }
        XCTAssertFalse(dstar.isEmpty)
        for entry in dstar { XCTAssertEqual(entry.dial?.modules, []) }
    }

    func testM17RowKeepsCallsignAndModules() throws {
        let entry = try XCTUnwrap(try slice().entries.first { $0.network == .m17 })
        XCTAssertEqual(entry.dial?.callsign, "M17-002")
        XCTAssertEqual(entry.dial?.modules, ["A"])
        XCTAssertEqual(entry.dial?.endpoint?.port, 17000)
    }

    /// All 89 URF rows genuinely publish no port. The kind is decoded anyway,
    /// with the port absent — inventing one would aim a connection at
    /// something that is not the reflector.
    func testURFRowDecodesWithoutAPort() throws {
        let urf = try slice().entries.filter { $0.network == .urf }
        XCTAssertFalse(urf.isEmpty)
        for entry in urf {
            guard case .urf(let host, let port, _) = try XCTUnwrap(entry.dial) else {
                return XCTFail("expected .urf, got \(String(describing: entry.dial))")
            }
            XCTAssertFalse(host.isEmpty)
            XCTAssertNil(port)
            XCTAssertFalse(entry.isDialable, "no port means nothing to connect to")
        }
    }

    /// Every network the feed covers today decodes to a named case, so none of
    /// them reach the `other` fallback by accident.
    func testEveryShippedNetworkHasANamedCase() throws {
        for entry in try slice().entries {
            if case .other(let raw) = entry.network {
                XCTFail("\(raw) should be a named ReflectorNetwork case")
            }
        }
    }

    // MARK: - Degradation: the extension mechanism

    /// The headline forward-compatibility test. A network and a dial kind that
    /// do not exist in this build must decode, stay listed, and report
    /// themselves as un-dialable — never throw, never vanish. This is what
    /// lets hamcall-db add DMR without astar shipping first.
    func testAFutureNetworkAndKindListWithoutBreakingTheBuild() throws {
        let feed = try feed(
            """
            {"schema_version": 1, "client_refresh_days": 7, "reflectors": [
              {"network": "dmr", "id": "TG91", "name": "Worldwide",
               "dial": {"kind": "dmr", "host": "dmr.example.org", "port": 62031,
                        "requires": ["dmr_id", "password"]}},
              {"network": "dstar", "id": "XLX999", "name": "XLX999",
               "dial": {"kind": "dextra", "host": "10.0.0.1", "port": 30001,
                        "callsign": "XRF999"}}
            ]}
            """)

        XCTAssertEqual(feed.entries.count, 2, "an unknown row must not cost the known one")
        let dmr = try XCTUnwrap(feed.entries.first)
        XCTAssertEqual(dmr.network, .other("dmr"))
        XCTAssertEqual(dmr.network.rawValue, "dmr", "the publisher's string is kept verbatim")
        XCTAssertEqual(dmr.network.displayName, "DMR")
        XCTAssertEqual(dmr.dial, .unsupported(kind: "dmr"))
        XCTAssertEqual(dmr.dial?.kind, "dmr", "the UI can still name what it cannot dial")
        XCTAssertFalse(dmr.isDialable)
        XCTAssertTrue(feed.entries[1].isDialable)
    }

    /// A known network carrying a kind from the future degrades on the dial
    /// alone — the row keeps its network, its name, and its place in the list.
    func testUnknownKindOnAKnownNetworkKeepsTheRow() throws {
        let entry = try XCTUnwrap(
            try feed(
                """
                {"reflectors": [{"network": "m17", "id": "M17-XYZ", "name": "Future",
                  "dial": {"kind": "m17-over-quic", "host": "h.example", "port": 1}}]}
                """
            ).entries.first)
        XCTAssertEqual(entry.network, .m17)
        XCTAssertEqual(entry.dial, .unsupported(kind: "m17-over-quic"))
    }

    /// `dial` absent means listed but not dialable. No port is invented, no
    /// host is guessed — the row is simply not callable.
    func testMissingDialMeansListedNotDialable() throws {
        let entry = try XCTUnwrap(
            try feed(
                """
                {"reflectors": [{"network": "ysf", "id": "00099", "name": "Registered only"}]}
                """
            ).entries.first)
        XCTAssertNil(entry.dial)
        XCTAssertFalse(entry.isDialable)
    }

    /// Port is optional for `urf` and required everywhere else. A portless
    /// `dextra` is damaged data, so its dial degrades — but the row stays,
    /// because the reflector is real and the user searching for it deserves to
    /// be told astar cannot call it rather than that it does not exist.
    func testMissingPortOnANonURFKindDegradesButKeepsTheRow() throws {
        let feed = try feed(
            """
            {"reflectors": [
              {"network": "dstar", "id": "XLX111", "name": "XLX111",
               "dial": {"kind": "dextra", "host": "10.0.0.2", "callsign": "XRF111"}},
              {"network": "ysf", "id": "00042", "name": "No port",
               "dial": {"kind": "ysf", "host": "10.0.0.3"}}
            ]}
            """)
        XCTAssertEqual(feed.entries.count, 2)
        XCTAssertEqual(feed.entries[0].dial, .unsupported(kind: "dextra"))
        XCTAssertEqual(feed.entries[1].dial, .unsupported(kind: "ysf"))
        XCTAssertNil(feed.entries[0].dial?.endpoint)
    }

    /// A dextra row with no callsign cannot be dialled — D-Star addresses the
    /// far end by callsign, not by host.
    func testMissingCallsignDegradesADStarDial() throws {
        let entry = try XCTUnwrap(
            try feed(
                """
                {"reflectors": [{"network": "dstar", "id": "XLX222", "name": "XLX222",
                  "dial": {"kind": "dextra", "host": "10.0.0.4", "port": 30001}}]}
                """
            ).entries.first)
        XCTAssertEqual(entry.dial, .unsupported(kind: "dextra"))
    }

    /// Wrong types read as absent rather than throwing, so one sloppy field
    /// does not take a whole row out of the directory.
    func testWronglyTypedFieldsReadAsAbsent() throws {
        let entry = try XCTUnwrap(
            try feed(
                """
                {"reflectors": [{"network": "ysf", "id": "00007", "name": "Odd",
                  "aliases": "not-an-array", "country": 12,
                  "dial": {"kind": "ysf", "host": "10.0.0.5", "port": "42000"}}]}
                """
            ).entries.first)
        XCTAssertEqual(entry.aliases, [])
        XCTAssertNil(entry.country)
        XCTAssertEqual(entry.dial, .unsupported(kind: "ysf"), "a string port is not a port")
    }

    /// A row with no identity is the one thing worth dropping — there is
    /// nothing left to list. The rows either side of it survive.
    func testARowWithNoIdIsSkippedAndTheRestSurvive() throws {
        let feed = try feed(
            """
            {"count": 3, "reflectors": [
              {"network": "m17", "id": "M17-AAA", "name": "A"},
              {"network": "m17", "name": "no id at all"},
              {"network": "m17", "id": "M17-BBB", "name": "B"}
            ]}
            """)
        XCTAssertEqual(feed.entries.map(\.id), ["M17-AAA", "M17-BBB"])
        XCTAssertEqual(feed.declaredCount, 3, "the publisher's count is kept, so the gap shows")
    }

    /// The rows array is the one required field. A payload without it is not a
    /// directory, and reading it as "zero reflectors" is exactly the silent
    /// emptiness the cache fallback exists to prevent.
    func testAPayloadWithNoRowsArrayThrows() {
        XCTAssertThrowsError(try feed(#"{"schema_version": 1}"#))
    }

    // MARK: - The refresh cadence is data

    func testPublishedRefreshCadenceWins() throws {
        XCTAssertEqual(
            try feed(#"{"client_refresh_days": 21, "reflectors": []}"#).clientRefreshDays, 21)
    }

    /// The constant is an absent-field fallback, never a substitute for a
    /// published value. A nonsense cadence is treated as absent rather than
    /// obeyed — obeying `0` would mean polling on every launch.
    func testAbsentOrNonsenseCadenceFallsBackToSeven() throws {
        XCTAssertEqual(try feed(#"{"reflectors": []}"#).clientRefreshDays, 7)
        XCTAssertEqual(
            try feed(#"{"client_refresh_days": 0, "reflectors": []}"#).clientRefreshDays, 7)
        XCTAssertEqual(
            try feed(#"{"client_refresh_days": -3, "reflectors": []}"#).clientRefreshDays, 7)
        XCTAssertEqual(ReflectorFeed.fallbackRefreshDays, 7)
    }

    // MARK: - Round-tripping

    /// A decoded feed re-encodes into something this decoder reads back the
    /// same way, which is what lets the cache file share a format with the
    /// bundled snapshot and the upstream file.
    func testTheSliceRoundTripsThroughEncoding() throws {
        let original = try slice()
        let reread = try ReflectorFeed.decode(try JSONEncoder().encode(original))
        XCTAssertEqual(reread, original)
    }

    func testEveryNetworkCaseRoundTripsItsRawValue() {
        for network in ReflectorNetwork.known + [.other("dmr")] {
            XCTAssertEqual(ReflectorNetwork(rawValue: network.rawValue), network)
        }
    }

    /// `id` repeats across networks, so list identity cannot be `id` alone —
    /// a SwiftUI list keyed on a colliding id drops rows silently.
    func testKeyDisambiguatesTheSameIdOnDifferentNetworks() {
        let nxdn = DirectoryEntry(network: .nxdn, id: "100", name: "NXDN 100")
        let p25 = DirectoryEntry(network: .p25, id: "100", name: "P25 100")
        XCTAssertNotEqual(nxdn.key, p25.key)
    }
}
