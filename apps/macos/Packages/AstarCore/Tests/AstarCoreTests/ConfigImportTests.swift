// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-b52e — importing an archive MERGES. It never deletes.
///
/// Someone importing a friend's rig onto a working Mac must not lose their own
/// configs, and someone re-importing their own backup must not end up with
/// everything twice.
final class ConfigImportTests: XCTestCase {

    private func setup(_ id: String, _ name: String) -> Setup {
        Setup(id: id, name: name, hardwareProfileID: "uci150")
    }
    private func node(_ id: String, _ label: String, _ number: String, favorite: Bool = false)
        -> NodeEntry
    {
        NodeEntry(id: id, label: label, node: number, favorite: favorite)
    }

    // MARK: - Identified collections (setups, mic profiles)

    func testImportingIntoAnEmptyStoreAddsEverything() {
        let r = ConfigMerge.byID(incoming: [setup("a", "A"), setup("b", "B")], into: [])
        XCTAssertEqual(r.merged.map(\.id), ["a", "b"])
        XCTAssertEqual(r.added, 2)
        XCTAssertEqual(r.updated, 0)
    }

    func testExistingConfigsAreKept() {
        let r = ConfigMerge.byID(incoming: [setup("b", "B")], into: [setup("a", "A")])
        XCTAssertEqual(r.merged.map(\.id), ["a", "b"], "existing config was dropped")
        XCTAssertEqual(r.added, 1)
    }

    func testReimportingTheSameArchiveIsIdempotent() {
        let mine = [setup("a", "A"), setup("b", "B")]
        let once = ConfigMerge.byID(incoming: mine, into: mine)
        XCTAssertEqual(once.merged.map(\.id), ["a", "b"], "duplicated on re-import")
        XCTAssertEqual(once.added, 0)
        let twice = ConfigMerge.byID(incoming: mine, into: once.merged)
        XCTAssertEqual(twice.merged, once.merged)
    }

    func testAMatchingIDIsUpdatedInPlaceAndKeepsItsPosition() {
        let existing = [setup("a", "A"), setup("b", "B"), setup("c", "C")]
        let r = ConfigMerge.byID(incoming: [setup("b", "B renamed")], into: existing)
        XCTAssertEqual(r.merged.map(\.id), ["a", "b", "c"], "order churned")
        XCTAssertEqual(r.merged[1].name, "B renamed")
        XCTAssertEqual(r.updated, 1)
        XCTAssertEqual(r.added, 0)
    }

    func testAnUnchangedEntryIsNotCountedAsUpdated() {
        let existing = [setup("a", "A")]
        let r = ConfigMerge.byID(incoming: [setup("a", "A")], into: existing)
        XCTAssertEqual(r.updated, 0, "identical entry reported as a change")
    }

    // MARK: - The node directory

    func testDirectoryMergesOnNodeNumberNotID() {
        // recordRecent upserts by NODE, and directorySetNode refuses two entries
        // pointing at one node. Merging on id would create exactly the duplicate
        // those rules exist to prevent.
        let existing = [node("local-1", "My name for it", "12345")]
        let incoming = [node("theirs-9", "Their name", "12345")]
        let r = ConfigMerge.directory(incoming: incoming, into: existing)
        XCTAssertEqual(r.merged.count, 1, "same node stored twice")
        XCTAssertEqual(r.updated, 1)
        XCTAssertEqual(r.added, 0)
    }

    func testAMergedDirectoryEntryKeepsTheLocalID() {
        // Favorites and recents elsewhere reference the local id; rewriting it
        // from the archive would orphan them.
        let existing = [node("local-1", "Mine", "12345")]
        let r = ConfigMerge.directory(
            incoming: [node("theirs-9", "Theirs", "12345")], into: existing)
        XCTAssertEqual(r.merged[0].id, "local-1")
        XCTAssertEqual(r.merged[0].label, "Theirs", "incoming label should win")
    }

    func testANewNodeIsAdded() {
        let r = ConfigMerge.directory(
            incoming: [node("n2", "New", "999")], into: [node("n1", "Old", "12345")])
        XCTAssertEqual(r.merged.map(\.node), ["12345", "999"])
        XCTAssertEqual(r.added, 1)
    }

    func testAnImportedIDColldingWithADifferentNodeIsRebased() {
        // Same id, different node: keeping it would give two entries one id.
        let existing = [node("shared", "Mine", "111")]
        let r = ConfigMerge.directory(
            incoming: [node("shared", "Theirs", "222")], into: existing)
        XCTAssertEqual(r.merged.count, 2)
        XCTAssertEqual(Set(r.merged.map(\.id)).count, 2, "duplicate id survived")
        XCTAssertEqual(Set(r.merged.map(\.node)), ["111", "222"])
    }

    func testFavoriteFlagIsNotSilentlyClearedByAnImport() {
        let existing = [node("n1", "Mine", "12345", favorite: true)]
        let r = ConfigMerge.directory(
            incoming: [node("x", "Theirs", "12345", favorite: false)], into: existing)
        XCTAssertTrue(r.merged[0].favorite, "import un-favorited a saved node")
    }

    func testEmptyImportChangesNothing() {
        let existing = [node("n1", "Mine", "12345")]
        let r = ConfigMerge.directory(incoming: [], into: existing)
        XCTAssertEqual(r.merged, existing)
        XCTAssertEqual(r.added, 0)
        XCTAssertEqual(r.updated, 0)
    }

    // MARK: - Summary

    func testSummaryCountsWhatActuallyChanged() {
        let archive = ConfigArchive(
            version: 1, exportedAt: Date(), appVersion: nil,
            rigs: .init(
                setups: [setup("a", "A"), setup("new", "New")],
                micProfiles: [], defaultSetupID: nil, selectedSetupID: nil),
            directory: [node("n1", "X", "12345")],
            settings: ["audio.inputGain": .double(0.5)],
            callsign: "AJ7HR",
            interface: nil)
        let summary = ConfigMerge.summarize(
            archive,
            existingSetups: [setup("a", "A")],
            existingProfiles: [],
            existingDirectory: [])
        XCTAssertEqual(summary.setupsAdded, 1)
        XCTAssertEqual(summary.setupsUpdated, 0)
        XCTAssertEqual(summary.directoryAdded, 1)
        XCTAssertEqual(summary.settingsApplied, 1)
        XCTAssertTrue(summary.callsignApplied)
    }

    func testSummaryReadsAsASentence() {
        let summary = ConfigMerge.Summary(
            setupsAdded: 2, setupsUpdated: 1, micProfilesAdded: 0, micProfilesUpdated: 0,
            directoryAdded: 3, directoryUpdated: 0, settingsApplied: 12,
            interfaceApplied: 0, callsignApplied: false)
        let text = summary.description
        XCTAssertTrue(text.contains("2"), text)
        XCTAssertFalse(text.isEmpty)
    }

    func testAnEmptySummarySaysNothingChanged() {
        let summary = ConfigMerge.Summary()
        XCTAssertTrue(summary.isEmpty)
        XCTAssertTrue(summary.description.lowercased().contains("nothing"), summary.description)
    }
}
