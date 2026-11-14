// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// The directory search across the reflector feed and, while logged in, the
/// EchoLink station list. Offline: a fixture feed and hand-made stations.
@MainActor
final class DirectorySearchTests: XCTestCase {

    private func directory() throws -> ReflectorDirectory {
        let storage = FakeReflectorDirectoryStorage(
            bundled: try ReflectorFixtures.sliceData(), cache: nil)
        return ReflectorDirectory(storage: storage, fetcher: FakeReflectorFeedFetcher())
    }

    private let stations = [
        EchoLinkStationEntry(
            call: "W1ABC-R", description: "Boston, MA", node: 123_456, kind: .repeater,
            status: .online),
        EchoLinkStationEntry(
            call: "*ECHOTEST*", description: "Test server", node: 9999, kind: .conference,
            status: .busy),
        EchoLinkStationEntry(
            call: "K7XYZ", description: "Phoenix", node: 777, kind: .user, status: .online),
    ]

    private var index: EchoLinkSearchIndex { EchoLinkSearchIndex(stations) }

    private func echolinkCalls(_ results: [DirectorySearchResult]) -> [String] {
        results.compactMap {
            if case .echolink(let station) = $0 { return station.call }
            return nil
        }
    }

    private func reflectorCount(_ results: [DirectorySearchResult]) -> Int {
        results.filter { if case .reflector = $0 { return true } else { return false } }.count
    }

    // MARK: - Logged in and out

    func testAllNetworksIncludesEchoLinkWhileLoggedIn() throws {
        let dir = try directory()
        let results = dir.search("", filter: nil, echolink: index)
        XCTAssertEqual(reflectorCount(results), dir.entries.count)
        XCTAssertEqual(echolinkCalls(results), ["*ECHOTEST*", "K7XYZ", "W1ABC-R"])
        // Reflectors first, EchoLink after.
        if case .echolink = results.first { XCTFail("reflectors should lead") }
    }

    func testLoggedOutExcludesEchoLink() throws {
        let dir = try directory()
        XCTAssertTrue(echolinkCalls(dir.search("", filter: nil, echolink: nil)).isEmpty)
        XCTAssertTrue(dir.search("", filter: .echolink, echolink: nil).isEmpty)
        XCTAssertFalse(
            DirectorySearchNetwork.available(reflectors: dir.entries, echolinkLoggedIn: false)
                .contains(.echolink))
    }

    func testFilterOffersEchoLinkLastWhileLoggedIn() throws {
        let dir = try directory()
        let available = DirectorySearchNetwork.available(
            reflectors: dir.entries, echolinkLoggedIn: true)
        XCTAssertEqual(available.last, .echolink)
        XCTAssertEqual(
            Array(available.dropLast()),
            DirectorySearchNetwork.available(reflectors: dir.entries, echolinkLoggedIn: false))
    }

    func testEchoLinkFilterReturnsOnlyEchoLink() throws {
        let dir = try directory()
        let results = dir.search("", filter: .echolink, echolink: index)
        XCTAssertEqual(results.count, stations.count)
        XCTAssertTrue(results.allSatisfy { $0.network == .echolink })
    }

    func testReflectorFilterExcludesEchoLink() throws {
        let dir = try directory()
        let results = dir.search("", filter: .reflector(.dstar), echolink: index)
        XCTAssertTrue(echolinkCalls(results).isEmpty)
        XCTAssertEqual(results.count, dir.search("", network: .dstar).count)
    }

    // MARK: - Query matching

    func testQueryMatchesCallsignInAnyCase() throws {
        let dir = try directory()
        XCTAssertEqual(
            echolinkCalls(dir.search("w1abc", filter: .echolink, echolink: index)),
            ["W1ABC-R"])
        XCTAssertEqual(
            echolinkCalls(dir.search("echo", filter: nil, echolink: index)),
            ["*ECHOTEST*"])
    }

    func testQueryMatchesNodeNumberPrefix() throws {
        let dir = try directory()
        XCTAssertEqual(
            echolinkCalls(dir.search("1234", filter: .echolink, echolink: index)),
            ["W1ABC-R"])
        // A prefix, not a substring: "456" is inside 123456 but does not start it.
        XCTAssertTrue(
            echolinkCalls(dir.search("456", filter: .echolink, echolink: index))
                .isEmpty)
    }

    func testQueryMatchesLocation() throws {
        let dir = try directory()
        XCTAssertEqual(
            echolinkCalls(dir.search("phoenix", filter: .echolink, echolink: index)),
            ["K7XYZ"])
    }

    /// One query under All networks finds a reflector and an EchoLink
    /// station together, the reflector first.
    func testAllNetworksQueryHitsReflectorAndEchoLinkStation() throws {
        let dir = try directory()
        let brazil = EchoLinkStationEntry(
            call: "PY2XYZ-L", description: "Sao Paulo, Brazil", node: 31_337, kind: .link,
            status: .online)
        let results = dir.search(
            "brazil", filter: nil, echolink: EchoLinkSearchIndex(stations + [brazil]))
        XCTAssertEqual(results.map(\.id), ["dstar:XLX000", "echolink:31337"])
    }

    // MARK: - Filter and dial

    func testFilterFallsBackToAllWhenEchoLinkGoes() throws {
        let dir = try directory()
        let loggedOut = DirectorySearchNetwork.available(
            reflectors: dir.entries, echolinkLoggedIn: false)
        XCTAssertNil(DirectorySearchNetwork.keeping(.echolink, available: loggedOut))
        // The choice is kept, so the same choice shows EchoLink again once
        // the login is back.
        let loggedIn = DirectorySearchNetwork.available(
            reflectors: dir.entries, echolinkLoggedIn: true)
        XCTAssertEqual(DirectorySearchNetwork.keeping(.echolink, available: loggedIn), .echolink)
        XCTAssertEqual(
            DirectorySearchNetwork.keeping(.reflector(.dstar), available: loggedOut),
            .reflector(.dstar))
    }

    func testPreferredFilterFollowsTheDialNetwork() {
        XCTAssertEqual(DirectorySearchNetwork.preferred(for: .m17), .reflector(.m17))
        XCTAssertEqual(DirectorySearchNetwork.preferred(for: .allstar), .allstar)
    }

    func testEchoLinkResultDialsOnEchoLinkByCallsign() {
        XCTAssertEqual(DirectorySearchNetwork.echolink.appNetwork, .echolink)
        XCTAssertEqual(DirectorySearchNetwork.echolink.displayName, "EchoLink")
        XCTAssertEqual(echolinkDialText(for: stations[0]), "W1ABC-R")
        XCTAssertEqual(echolinkDialText(for: stations[1]), "*ECHOTEST*")
        let odd = EchoLinkStationEntry(
            call: "W1 ABC", description: "", node: 42, kind: .user, status: .online)
        XCTAssertEqual(echolinkDialText(for: odd), "42", "a callsign the field can't hold")
    }

    func testResultIDsAreDistinctAcrossKinds() throws {
        let dir = try directory()
        let results = dir.search("", filter: nil, echolink: index)
        XCTAssertEqual(Set(results.map(\.id)).count, results.count)
    }

    // MARK: - AllStar

    private let allstarIndex = AllStarIndex([
        AllStarNode(node: "2002", callsign: "W1AW", description: "Hub", location: "Newington, CT"),
        AllStarNode(node: "1999", callsign: "WB6NIL", description: "Parrot", location: "Texas"),
    ])

    private func allstarNodes(_ results: [DirectorySearchResult]) -> [String] {
        results.compactMap {
            if case .allstar(let node) = $0 { return node.node }
            return nil
        }
    }

    func testSearchCountingAllStarReportsUncappedTotal() throws {
        let dir = try directory()
        let counted = dir.searchCountingAllStar(
            "", filter: .allstar, echolink: nil, allstar: allstarIndex)
        XCTAssertEqual(counted.allstarTotalMatches, 2)
        XCTAssertEqual(allstarNodes(counted.results), ["1999", "2002"])
        let none = dir.searchCountingAllStar("", filter: nil, echolink: nil, allstar: allstarIndex)
        XCTAssertEqual(none.allstarTotalMatches, 0, "no AllStar search, no total")
    }

    func testAllStarFilterWithEmptyQueryListsOnlyAllStar() throws {
        let dir = try directory()
        let results = dir.search("", filter: .allstar, echolink: index, allstar: allstarIndex)
        XCTAssertEqual(allstarNodes(results), ["1999", "2002"])
        XCTAssertEqual(results.count, 2)
        XCTAssertEqual(results.map(\.id), ["allstar:1999", "allstar:2002"])
        XCTAssertTrue(results.allSatisfy { $0.network == .allstar })
    }

    func testAllNetworksWithEmptyQueryOmitsAllStar() throws {
        let dir = try directory()
        let results = dir.search("", filter: nil, echolink: nil, allstar: allstarIndex)
        XCTAssertTrue(allstarNodes(results).isEmpty)
        XCTAssertEqual(reflectorCount(results), dir.entries.count)
    }

    func testAllNetworksWithAQueryEndsWithAllStarMatches() throws {
        let dir = try directory()
        let results = dir.search("parrot", filter: nil, echolink: nil, allstar: allstarIndex)
        XCTAssertEqual(allstarNodes(results), ["1999"])
        guard case .allstar = results.last else { return XCTFail("AllStar should come last") }
        let withSpaces = dir.search("  ", filter: nil, echolink: nil, allstar: allstarIndex)
        XCTAssertTrue(allstarNodes(withSpaces).isEmpty, "a blank query is an empty query")
    }

    func testNoAllStarIndexMeansNoAllStarRows() throws {
        let dir = try directory()
        XCTAssertTrue(dir.search("", filter: .allstar, echolink: nil).isEmpty)
        XCTAssertTrue(allstarNodes(dir.search("parrot", filter: nil, echolink: nil)).isEmpty)
    }

    func testAllStarIsTheLastFilterChoiceWhenAvailable() throws {
        let dir = try directory()
        let with = DirectorySearchNetwork.available(
            reflectors: dir.entries, echolinkLoggedIn: true, allstarAvailable: true)
        XCTAssertEqual(with.last, .allstar)
        XCTAssertEqual(with.dropLast().last, .echolink)
        let without = DirectorySearchNetwork.available(
            reflectors: dir.entries, echolinkLoggedIn: true)
        XCTAssertFalse(without.contains(.allstar))
        XCTAssertEqual(DirectorySearchNetwork.allstar.appNetwork, .allstar)
        XCTAssertEqual(DirectorySearchNetwork.allstar.displayName, "AllStar")
        XCTAssertEqual(DirectorySearchNetwork.allstar.id, "allstar")
    }
}
