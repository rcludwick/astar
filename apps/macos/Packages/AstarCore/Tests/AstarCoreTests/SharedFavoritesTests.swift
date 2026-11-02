// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// Favorites shared across every network: one node directory, keyed on
/// (network, target), that the dial row's saved-nodes menu, its star, the
/// EchoLink station browser's stars and Settings → Favorites all read and
/// write.
///
/// Every session runs on `FakeStation` or `NullStation`, and every directory
/// lives in memory or a scratch defaults suite. Nothing here resolves a host,
/// opens a socket or reaches the real preferences.
final class SharedFavoritesTests: XCTestCase {

    private var suites: [String] = []

    override func tearDown() {
        for name in suites { UserDefaults.standard.removePersistentDomain(forName: name) }
        suites = []
        super.tearDown()
    }

    private func scratchDefaults() -> UserDefaults {
        let name = "astar.tests.favorites.\(UUID().uuidString)"
        suites.append(name)
        let defaults = UserDefaults(suiteName: name)!
        defaults.removePersistentDomain(forName: name)
        return defaults
    }

    private let listJSON = #"""
        [{"call":"W1AW","description":"Newington","node":1234,"kind":"user","status":"online"},
         {"call":"*ECHOTEST*","description":"Test server","node":9999,"kind":"conference","status":"online"},
         {"call":"KX7ABC-L","description":"Denver link","node":99123,"kind":"link","status":"busy"},
         {"call":"AJ7HR-R","description":"Repeater","node":45678,"kind":"repeater","status":"unknown"}]
        """#

    private var stations: [EchoLinkStationEntry] {
        EchoLinkStationEntry.decodeStations(json: listJSON) ?? []
    }

    /// A session logged in to EchoLink with `listJSON` as its station list.
    private func echolinkSession(
        directory: NodeDirectoryStore = MemoryNodeDirectoryStore(), list: Bool = true
    ) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            echolinkAvailable: true, echolinkActive: false, echolinkLoggedIn: true)
        fake.echolinkStateValue = EchoLinkState(
            json: #"{"login":"logged_in","callsign":"KX7ABC"}"#)
        if list { fake.echolinkStationsValue = stations }
        let session = CallSession(
            station: fake, directoryStore: directory, userDefaults: scratchDefaults())
        session.echolinkWorker = { $0() }
        session.poll()
        return (session, fake)
    }

    // MARK: - Identity is (network, target)

    func testTheSameNumberOnTwoNetworksIsTwoEntries() {
        let store = UserDefaultsNodeDirectoryStore(scratchDefaults())
        store.recordRecent(node: "1234", label: "1234", network: .allstar)
        store.recordRecent(node: "1234", label: "1234", network: .echolink)
        store.recordRecent(node: "1234", label: "1234", network: .allstar)
        XCTAssertEqual(store.all().count, 2, "AllStar 1234 and EchoLink 1234 are different")
        XCTAssertEqual(Set(store.all().map(\.network)), [.allstar, .echolink])

        let session = CallSession(station: NullStation(), directoryStore: store)
        session.addFavorite(node: "1234", label: "W1AW", network: .echolink)
        XCTAssertTrue(session.isFavorite(node: "1234", network: .echolink))
        XCTAssertFalse(session.isFavorite(node: "1234", network: .allstar))
        XCTAssertFalse(session.isFavorite(node: "1234"), "the default network is AllStar")
        XCTAssertEqual(store.all().count, 2, "the favorite merged into its own network's recent")
    }

    func testEchoLinkTargetsMatchInAnyCaseAndOthersKeepTheirCase() {
        XCTAssertEqual(
            NodeEntry.directoryKey(network: .echolink, target: " kx7abc-l "),
            NodeEntry.directoryKey(network: .echolink, target: "KX7ABC-L"))
        XCTAssertNotEqual(
            NodeEntry.directoryKey(network: .dstar, target: "xlx836 a"),
            NodeEntry.directoryKey(network: .dstar, target: "XLX836 A"),
            "no other network's matching changed")
        XCTAssertNotEqual(
            NodeEntry.directoryKey(network: .allstar, target: "1234"),
            NodeEntry.directoryKey(network: .echolink, target: "1234"))
    }

    func testANameIsLookedUpOnItsOwnNetwork() {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [
            NodeEntry(label: "W1AW", node: "1234", favorite: true, network: .echolink)
        ]
        let session = CallSession(station: NullStation(), directoryStore: directory)
        XCTAssertNil(session.name(forNode: "1234"), "an EchoLink label never names AllStar 1234")
        XCTAssertEqual(session.name(forNode: "1234", network: .echolink), "W1AW")
    }

    // MARK: - Stored directories and the v1 fixture

    func testADirectoryStoredBeforeNetworksStillDecodesAsAllStar() {
        let defaults = scratchDefaults()
        let legacy = #"""
            [{"id":"a","label":"AJ7HR","node":"77777","favorite":true},
             {"id":"b","label":"55555","node":"55555","favorite":false,"lastUsed":700000000}]
            """#
        defaults.set(Data(legacy.utf8), forKey: "directory.nodes")
        let store = UserDefaultsNodeDirectoryStore(defaults)
        XCTAssertEqual(store.all().map(\.network), [.allstar, .allstar])

        let session = CallSession(station: NullStation(), directoryStore: store)
        XCTAssertTrue(session.isFavorite(node: "77777"))
        XCTAssertTrue(session.isFavorite(node: "77777", network: .allstar))
        XCTAssertEqual(session.directoryRecents().map(\.node), ["55555"])

        // Re-dialling it on AllStar updates the same entry, as it always did.
        store.recordRecent(node: "77777", label: "77777", network: .allstar)
        XCTAssertEqual(store.all().count, 2)
        XCTAssertEqual(store.all().first { $0.id == "a" }?.label, "AJ7HR")
    }

    private func fixture() throws -> Data {
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: "astar-config-v1", withExtension: "astarconfig",
                subdirectory: "Fixtures"))
        return try Data(contentsOf: url)
    }

    func testTheV1FixtureImportsAndImportingTwiceAddsNothing() throws {
        let defaults = scratchDefaults()
        let session = CallSession(
            station: NullStation(), directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            userDefaults: defaults)
        let archive = try ConfigArchive.decode(try fixture())
        let incoming = try XCTUnwrap(archive.directory)

        let first = ConfigTransfer.apply(
            archive.filtered(to: [.directory]), session: session,
            setupStore: UserDefaultsSetupStore(defaults),
            profileStore: UserDefaultsMicProfileStore(defaults), defaults: defaults)
        XCTAssertEqual(first.directoryAdded, incoming.count)
        for entry in incoming {
            XCTAssertNotNil(
                session.directoryEntry(forNode: entry.node, network: entry.network),
                "\(entry.network) \(entry.node) landed on its own network")
        }

        let second = ConfigTransfer.apply(
            archive.filtered(to: [.directory]), session: session,
            setupStore: UserDefaultsSetupStore(defaults),
            profileStore: UserDefaultsMicProfileStore(defaults), defaults: defaults)
        XCTAssertEqual(second.directoryAdded, 0, "matched on (network, target), not duplicated")
        XCTAssertEqual(session.directoryAll().count, incoming.count)
    }

    // MARK: - A favorite from each network restores its whole target

    func testAFavoriteOnEveryNetworkComesBackWithItsNetworkAndWholeTarget() {
        let dmrTarget = DmrDialText.compose(
            address: "freedmr-network-server-freedmr-eu", talkgroup: "91", timeslot: 1)
        let targets: [(Network, String)] = [
            (.allstar, "77777"),
            (.allstar, "node.example.org:4569"),
            (.m17, "M17-002 C"),
            (.dstar, "XLX836 A"),
            (.ysf, "ysf.example:42000"),
            (.nxdn, "nxdn.example:41400/100"),
            (.dmr, dmrTarget),
            (.echolink, "*ECHOTEST*"),
        ]
        let defaults = scratchDefaults()
        let writer = CallSession(
            station: NullStation(), directoryStore: UserDefaultsNodeDirectoryStore(defaults))
        for (network, target) in targets {
            writer.addFavorite(node: target, label: "\(network.displayName) fav", network: network)
        }

        // A fresh session over the same stored directory: what a relaunch sees.
        let reader = CallSession(
            station: NullStation(), directoryStore: UserDefaultsNodeDirectoryStore(defaults))
        let favorites = reader.directoryFavorites()
        XCTAssertEqual(favorites.count, targets.count)
        for (network, target) in targets {
            let entry = favorites.first { $0.network == network && $0.node == target }
            XCTAssertNotNil(entry, "\(network) \(target) came back whole")
        }

        // DMR keeps all of master, talkgroup and slot, not just a number.
        let dmr = favorites.first { $0.network == .dmr }
        let parts = DmrDialText.parts(dmr?.node ?? "")
        XCTAssertEqual(parts.address, "freedmr-network-server-freedmr-eu")
        XCTAssertEqual(parts.talkgroup, "91")
        XCTAssertEqual(parts.timeslot, 1)
    }

    func testADMRRecentRecordsTheWholeTargetAndDialsItAgain() throws {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            dmrAvailable: true)
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(
            station: fake, directoryStore: directory,
            dmrPasswords: InMemoryDmrPasswordStore(["freedmr-network": "hunter2"]),
            userDefaults: scratchDefaults())
        session.operatorCallsign = "AJ7HR"
        session.dmrRadioID = "3153591"
        session.reflectorIndex = ReflectorIndex(entries: [
            DirectoryEntry(
                network: .dmr, id: "freedmr-network-server-freedmr-eu", name: "FreeDMR EU",
                dial: .mmdvm(system: "freedmr-network", host: "eu.example", port: 62031))
        ])
        session.poll()

        let target = "freedmr-network-server-freedmr-eu/91/1"
        try session.connect(node: target, network: .dmr)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            dmrAvailable: true)
        session.poll()
        let recent = try XCTUnwrap(session.directoryRecents().first)
        XCTAssertEqual(recent.network, .dmr)
        XCTAssertEqual(recent.node, target)

        // Starring it from the dial row and picking it later gives back text
        // that dials the same master, talkgroup and slot.
        XCTAssertTrue(session.toggleFavorite(node: recent.node, network: .dmr))
        let favorite = try XCTUnwrap(session.directoryFavorites().first)
        XCTAssertEqual(directory.entries.count, 1, "the star merged into the recent")
        try session.disconnect()
        XCTAssertTrue(session.canDial(favorite.node, network: favorite.network))
        try session.connect(node: favorite.node, network: favorite.network)
        let call = try XCTUnwrap(fake.connectDMRCalls.last)
        XCTAssertEqual(call.system, "freedmr-network")
        XCTAssertEqual(call.talkgroup, 91)
        XCTAssertEqual(call.timeslot, 1)
    }

    func testEditingAFavoritesTargetIsCheckedForItsOwnNetwork() {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [
            NodeEntry(id: "d", label: "TG 91", node: "sys/91/2", favorite: true, network: .dmr),
            NodeEntry(id: "a", label: "Hub", node: "1234", favorite: true),
            NodeEntry(id: "e", label: "W1AW", node: "W1AW", favorite: true, network: .echolink),
        ]
        let session = CallSession(station: NullStation(), directoryStore: directory)
        XCTAssertTrue(session.directorySetNode(id: "d", to: "sys/3100/1"), "a DMR target")
        XCTAssertEqual(directory.entries[0].node, "sys/3100/1")
        XCTAssertFalse(session.directorySetNode(id: "a", to: "sys/3100/1"), "not an AllStar one")
        XCTAssertTrue(
            session.directorySetNode(id: "e", to: "1234"),
            "EchoLink 1234 does not collide with AllStar 1234")
        XCTAssertFalse(session.directorySetNode(id: "e", to: "W1 AW!"))
    }

    // MARK: - EchoLink: the station browser and the dial row share one star

    func testTheDialRowStarAndTheStationListStarAreOneEntry() {
        let directory = MemoryNodeDirectoryStore()
        let (session, _) = echolinkSession(directory: directory)
        let before = session.directoryChangeCount

        // The dial row, with a node number typed.
        XCTAssertTrue(session.toggleFavorite(node: "9999", network: .echolink))
        XCTAssertGreaterThan(session.directoryChangeCount, before, "observers are told")
        XCTAssertEqual(directory.entries.count, 1)
        XCTAssertEqual(directory.entries[0].node, "*ECHOTEST*", "saved under the listed callsign")
        XCTAssertEqual(directory.entries[0].network, .echolink)

        // The station browser sees it.
        XCTAssertTrue(session.isEchoLinkFavorite("*echotest*"))
        XCTAssertTrue(session.isEchoLinkFavorite("9999"))
        let rows = echolinkListRows(
            stations: session.echolinkStations, favorites: session.echolinkFavoriteTargets,
            filter: .all, query: "")
        XCTAssertEqual(rows.filter(\.isFavorite).map(\.callsign), ["*ECHOTEST*"])

        // The station browser's star takes it off again: the same entry.
        XCTAssertFalse(session.toggleFavorite(node: "*ECHOTEST*", network: .echolink))
        XCTAssertTrue(directory.entries.isEmpty)
        XCTAssertFalse(session.isFavorite(node: "9999", network: .echolink))

        // And the other way round: starred from the list, seen by the dial row.
        session.toggleFavorite(node: "KX7ABC-L", network: .echolink, label: "KX7ABC-L")
        XCTAssertTrue(session.isEchoLinkFavorite("99123"))
        XCTAssertEqual(session.echolinkCanonicalTarget("99123"), "KX7ABC-L")
        XCTAssertTrue(session.isFavorite(node: "kx7abc-l", network: .echolink))
    }

    func testAFavoriteSavedByNumberBeforeTheListStillMatchesItsStation() {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [
            NodeEntry(label: "9999", node: "9999", favorite: true, network: .echolink)
        ]
        let (session, _) = echolinkSession(directory: directory)
        XCTAssertTrue(session.isEchoLinkFavorite("*ECHOTEST*"))
        let rows = echolinkListRows(
            stations: session.echolinkStations, favorites: session.echolinkFavoriteTargets,
            filter: .favorites, query: "")
        XCTAssertEqual(rows.map(\.callsign), ["*ECHOTEST*"], "one row, the listed one")
        XCTAssertEqual(rows.map(\.isListed), [true])

        XCTAssertFalse(session.toggleFavorite(node: "*ECHOTEST*", network: .echolink))
        XCTAssertTrue(directory.entries.isEmpty, "the star took off the number-saved entry")
    }

    func testAnAnsweredEchoLinkCallIsARecentUnderItsCallsign() throws {
        let directory = MemoryNodeDirectoryStore()
        let (session, fake) = echolinkSession(directory: directory)
        try session.connect(node: "99123", network: .echolink)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            echolinkAvailable: true, echolinkActive: true, echolinkLoggedIn: true)
        session.poll()
        XCTAssertEqual(directory.recordedRecents.map(\.node), ["KX7ABC-L"])
        XCTAssertEqual(directory.recordedRecents.map(\.network), [.echolink])
        XCTAssertEqual(session.echolinkCallStation, "KX7ABC-L")
        XCTAssertEqual(session.connectedTargetLabel(for: "99123"), "KX7ABC-L (99123)")

        // The call card's star and the recent are one entry.
        session.toggleFavorite(node: try XCTUnwrap(session.echolinkCallStation), network: .echolink)
        XCTAssertEqual(directory.entries.count, 1)
        XCTAssertTrue(directory.entries[0].favorite)
    }

    func testATypedCallsignDialsAndAnUnlistedOneSaysWhy() throws {
        let (session, fake) = echolinkSession()
        XCTAssertTrue(session.canDial("kx7abc-l", network: .echolink), "any case")
        XCTAssertNil(session.echolinkDialProblem("kx7abc-l"))
        try session.connect(node: "kx7abc-l", network: .echolink)
        XCTAssertEqual(fake.echolinkConnectCalls, ["kx7abc-l"])
        try session.disconnect()

        XCTAssertFalse(session.canDial("N0CALL", network: .echolink), "not listed: Connect off")
        XCTAssertEqual(
            session.echolinkDialProblem("N0CALL"),
            "N0CALL isn’t in the station list. Check the spelling, or refresh the list.")
        XCTAssertNil(session.echolinkDialProblem(""), "an empty field has nothing to explain")
    }

    // MARK: - The station browser's Favorites filter

    func testFavoritesFilterShowsListedFavoritesLiveAndTheRestAsNotOnline() {
        let favorites = ["kx7abc-l", "N0CALL", "9999"]
        let rows = echolinkListRows(
            stations: stations, favorites: favorites, filter: .favorites, query: "")
        XCTAssertEqual(rows.map(\.callsign), ["*ECHOTEST*", "KX7ABC-L", "N0CALL"])
        XCTAssertEqual(rows.map(\.isListed), [true, true, false])
        XCTAssertTrue(rows.allSatisfy(\.isFavorite))
        XCTAssertEqual(rows[1].station?.status, .busy, "a listed favorite carries its live status")
        XCTAssertEqual(rows[1].node, 99123)
        XCTAssertNil(rows[2].node)
        XCTAssertEqual(Set(rows.map(\.id)).count, rows.count, "row ids are unique")
    }

    func testFavoritesFilterSearchAndOtherFilters() {
        let favorites = ["W1AW", "N0CALL"]
        func calls(_ filter: EchoLinkFilter, _ query: String) -> [String] {
            echolinkListRows(stations: stations, favorites: favorites, filter: filter, query: query)
                .map(\.callsign)
        }
        XCTAssertEqual(calls(.favorites, "n0c"), ["N0CALL"])
        XCTAssertEqual(calls(.favorites, "12"), ["W1AW"], "a listed favorite's node by prefix")
        XCTAssertEqual(calls(.favorites, "zzz"), [])
        XCTAssertEqual(
            calls(.all, "").count, stations.count, "All lists the directory, nothing more")
        XCTAssertEqual(calls(.people, ""), ["W1AW"])
        XCTAssertEqual(
            echolinkListRows(stations: stations, favorites: [], filter: .favorites, query: ""), [])
    }

    // MARK: - Export and import with mixed networks

    func testMixedNetworksRoundTripThroughAnExportedConfig() throws {
        let sourceDefaults = scratchDefaults()
        let a = CallSession(
            station: NullStation(), directoryStore: UserDefaultsNodeDirectoryStore(sourceDefaults),
            userDefaults: sourceDefaults)
        a.addFavorite(node: "1234", label: "Hub", network: .allstar)
        a.addFavorite(node: "1234", label: "W1AW by number", network: .echolink)
        a.addFavorite(node: "*ECHOTEST*", label: "Echo test", network: .echolink)
        a.addFavorite(node: "sys/91/2", label: "Worldwide", network: .dmr)

        let data = try ConfigArchive.encode(
            ConfigTransfer.archive(
                sections: [.directory], session: a,
                setupStore: UserDefaultsSetupStore(sourceDefaults),
                profileStore: UserDefaultsMicProfileStore(sourceDefaults),
                defaults: sourceDefaults))
        let archive = try ConfigArchive.decode(data)
        XCTAssertEqual(archive.directory?.count, 4)

        let targetDefaults = scratchDefaults()
        let b = CallSession(
            station: NullStation(), directoryStore: UserDefaultsNodeDirectoryStore(targetDefaults),
            userDefaults: targetDefaults)
        b.addFavorite(node: "1234", label: "Mine", network: .allstar)
        let summary = ConfigTransfer.apply(
            archive, session: b, setupStore: UserDefaultsSetupStore(targetDefaults),
            profileStore: UserDefaultsMicProfileStore(targetDefaults), defaults: targetDefaults)
        XCTAssertEqual(summary.directoryAdded, 3, "EchoLink 1234 is not the local AllStar 1234")
        XCTAssertEqual(summary.directoryUpdated, 1, "AllStar 1234 matched the local one")

        let reread = CallSession(
            station: NullStation(), directoryStore: UserDefaultsNodeDirectoryStore(targetDefaults))
        let keys = Set(reread.directoryFavorites().map(\.directoryKey))
        XCTAssertEqual(
            keys, ["allstar:1234", "echolink:1234", "echolink:*ECHOTEST*", "dmr:sys/91/2"])
        XCTAssertEqual(reread.directoryFavorites().count, 4)
        XCTAssertEqual(ConfigVersion.current, 1, "keying on network moved no version")
    }
}
