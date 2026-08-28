// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The module picker's two halves: what it offers, and what it remembers.
///
/// The line these tests hold is that neither half ever becomes a *default*.
/// The directory refuses to guess a module because on D-Star the module is the
/// room, and a guess does not fail visibly — it succeeds, in someone else's
/// conversation. Offering 26 letters and recalling one the operator picked
/// themselves are both compatible with that; substituting either into a dial
/// is not.
final class ReflectorModulesTests: XCTestCase {

    private func entry(
        network: ReflectorNetwork = .dstar, id: String = "XLX836", dial: ReflectorDial?
    ) -> DirectoryEntry {
        DirectoryEntry(network: network, id: id, name: id, dial: dial)
    }

    // MARK: - What the picker offers

    /// The D-Star case, and the reason the alphabet exists: the XLX registry
    /// publishes no modules at all, so there is nothing to derive an offer
    /// from and the protocol's own range is what is left.
    func testDStarWithNoPublishedModulesOffersTheAlphabet() {
        let dial = ReflectorDial.dextra(
            host: "45.56.69.219", port: 30001, callsign: "XRF836", modules: [])
        let options = ReflectorModuleOptions.options(for: dial)
        XCTAssertEqual(options.count, 26)
        XCTAssertEqual(options.first, "A")
        XCTAssertEqual(options.last, "Z")
    }

    /// A published list wins. Offering the alphabet when the reflector has
    /// told us which four rooms it runs would be 22 dead letters.
    func testPublishedModulesWinOverTheAlphabet() {
        let dial = ReflectorDial.m17(
            host: "m17.example", port: 17000, callsign: "M17-002", modules: ["A", "C", "D"])
        XCTAssertEqual(ReflectorModuleOptions.options(for: dial), ["A", "C", "D"])
    }

    /// Publication order is kept and duplicates collapse — a feed that lists
    /// "A" twice must not produce two buttons that do the same thing.
    func testPublishedModulesAreDeduplicatedInOrder() {
        let dial = ReflectorDial.m17(
            host: "m17.example", port: 17000, callsign: "M17-002", modules: ["c", "A", "C"])
        XCTAssertEqual(ReflectorModuleOptions.options(for: dial), ["C", "A"])
    }

    /// A network with no rooms gets no picker, not an empty one. YSF/NXDN/P25
    /// dial a bare endpoint; asking which module is not a smaller version of
    /// the question, it is a different question.
    func testNetworksWithoutModulesOfferNothing() {
        XCTAssertTrue(
            ReflectorModuleOptions.options(for: .ysf(host: "ysf.example", port: 42000)).isEmpty)
        XCTAssertTrue(ReflectorModuleOptions.options(for: nil).isEmpty)
        XCTAssertTrue(ReflectorModuleOptions.options(for: .unsupported(kind: "dmr")).isEmpty)
    }

    /// Garbage in the published list falls back to the alphabet rather than to
    /// an empty picker — the reflector still has rooms, we just cannot read
    /// which ones.
    func testUnusablePublishedModulesFallBackToTheAlphabet() {
        let dial = ReflectorDial.dextra(
            host: "h", port: 30001, callsign: "XRF836", modules: ["AB", "3", ""])
        XCTAssertEqual(ReflectorModuleOptions.options(for: dial).count, 26)
    }

    // MARK: - What the picker remembers

    private func memory() -> ReflectorModuleMemory {
        let defaults = UserDefaults(suiteName: "astar.tests.modules.\(UUID().uuidString)")!
        return ReflectorModuleMemory(defaults: defaults)
    }

    func testRemembersAndRecallsPerReflector() {
        let memory = memory()
        let xlx836 = entry(dial: .dextra(host: "h", port: 30001, callsign: "XRF836", modules: []))
        let xlx458 = entry(
            id: "XLX458", dial: .dextra(host: "h", port: 30001, callsign: "XRF458", modules: []))

        XCTAssertNil(memory.module(for: xlx836))
        memory.remember("B", for: xlx836)
        XCTAssertEqual(memory.module(for: xlx836), "B")
        // Another reflector is untouched: the recollection is per-box, which
        // is what makes it a recollection and not a preference.
        XCTAssertNil(memory.module(for: xlx458))
    }

    /// Ids collide across networks — NXDN "100" and P25 "100" both exist — so
    /// the key is `network:id`. A collision here would hand one network's
    /// remembered room to another's.
    func testIdenticalIdsOnDifferentNetworksDoNotShareAModule() {
        let memory = memory()
        let nxdn = entry(network: .nxdn, id: "100", dial: .nxdn(host: "h", port: 41400))
        let p25 = entry(network: .p25, id: "100", dial: .p25(host: "h", port: 41000))
        memory.remember("A", for: nxdn)
        XCTAssertEqual(memory.module(for: nxdn), "A")
        XCTAssertNil(memory.module(for: p25))
    }

    func testLastChoiceWinsAndForgetClears() {
        let memory = memory()
        let entry = entry(dial: .dextra(host: "h", port: 30001, callsign: "XRF836", modules: []))
        memory.remember("B", for: entry)
        memory.remember("D", for: entry)
        XCTAssertEqual(memory.module(for: entry), "D")
        memory.forget(entry)
        XCTAssertNil(memory.module(for: entry))
    }

    /// A preferences domain is editable by hand, so a value that is not a
    /// single letter is dropped on read rather than trusted into a dial.
    func testHandEditedGarbageIsIgnoredOnRead() {
        let defaults = UserDefaults(suiteName: "astar.tests.modules.\(UUID().uuidString)")!
        defaults.set(["dstar:XLX836": "not a module"], forKey: ReflectorModuleMemory.defaultsKey)
        let memory = ReflectorModuleMemory(defaults: defaults)
        let entry = entry(dial: .dextra(host: "h", port: 30001, callsign: "XRF836", modules: []))
        XCTAssertNil(memory.module(for: entry))
    }
}
