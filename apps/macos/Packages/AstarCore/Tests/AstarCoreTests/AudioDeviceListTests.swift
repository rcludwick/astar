// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// astar-9d41 — two physical devices can report the SAME CoreAudio name (an
/// IC-7300 and an AllScan UCI150 both enumerate as "USB Audio Device"). The
/// engine addresses devices by name and resolves the FIRST match, so only one
/// of them is reachable; a picker that lists both offers a choice that does not
/// exist, and duplicate SwiftUI tags make every matching row read as selected.
final class AudioDeviceListTests: XCTestCase {

    // MARK: - Collapsing

    func testLeavesDistinctNamesAlone() {
        let names = ["USB Audio Device", "KT USB Audio", "BlackHole 2ch"]
        XCTAssertEqual(AudioDeviceList.selectable(from: names), names)
    }

    func testCollapsesARepeatedNameToOneEntry() {
        let names = ["USB Audio Device", "USB Audio Device", "KT USB Audio"]
        XCTAssertEqual(
            AudioDeviceList.selectable(from: names),
            ["USB Audio Device", "KT USB Audio"])
    }

    func testPreservesEnumerationOrder() {
        // The surviving entry must be the FIRST occurrence, because that is the
        // one find_device() resolves to. Order is the contract, not a detail.
        let names = ["KT USB Audio", "USB Audio Device", "KT USB Audio"]
        XCTAssertEqual(
            AudioDeviceList.selectable(from: names),
            ["KT USB Audio", "USB Audio Device"])
    }

    func testCollapsesThreeOrMore() {
        let names = ["USB Audio Device", "USB Audio Device", "USB Audio Device"]
        XCTAssertEqual(AudioDeviceList.selectable(from: names), ["USB Audio Device"])
    }

    func testEmptyListStaysEmpty() {
        XCTAssertEqual(AudioDeviceList.selectable(from: []), [])
    }

    func testNamesDifferingOnlyByCaseAreDistinctDevices() {
        // CoreAudio names are case-sensitive and cpal compares them exactly, so
        // these two ARE separately addressable. Collapsing them would hide a
        // device the engine can actually open.
        let names = ["USB Audio Device", "USB audio device"]
        XCTAssertEqual(AudioDeviceList.selectable(from: names), names)
    }

    // MARK: - Reporting the collision

    func testReportsNoDuplicatesWhenNamesAreDistinct() {
        XCTAssertTrue(
            AudioDeviceList.duplicated(in: ["USB Audio Device", "KT USB Audio"]).isEmpty)
    }

    func testReportsTheDuplicatedName() {
        let names = ["USB Audio Device", "USB Audio Device", "KT USB Audio"]
        XCTAssertEqual(AudioDeviceList.duplicated(in: names), ["USB Audio Device"])
    }

    func testReportsEveryDuplicatedNameOnce() {
        let names = [
            "USB Audio Device", "KT USB Audio", "USB Audio Device",
            "KT USB Audio", "KT USB Audio",
        ]
        XCTAssertEqual(
            AudioDeviceList.duplicated(in: names), ["USB Audio Device", "KT USB Audio"])
    }

    func testDuplicateReportFollowsFirstAppearanceOrder() {
        let names = ["A", "B", "B", "A"]
        XCTAssertEqual(AudioDeviceList.duplicated(in: names), ["A", "B"])
    }

    // MARK: - The warning the user actually reads

    func testNoWarningWithoutACollision() {
        XCTAssertNil(AudioDeviceList.collisionWarning(for: ["USB Audio Device"]))
    }

    func testWarningNamesTheDeviceAndSaysWhichOneIsUsed() {
        let warning = AudioDeviceList.collisionWarning(
            for: ["USB Audio Device", "USB Audio Device"])
        let text = try! XCTUnwrap(warning)
        XCTAssertTrue(text.contains("USB Audio Device"), "must name the offender: \(text)")
        XCTAssertTrue(
            text.lowercased().contains("audio midi setup"),
            "must point at the fix the user can apply today: \(text)")
    }

    func testWarningCoversEveryCollidingName() {
        let warning = AudioDeviceList.collisionWarning(for: ["A", "A", "B", "B"])
        let text = try! XCTUnwrap(warning)
        XCTAssertTrue(text.contains("A") && text.contains("B"), text)
    }

    // MARK: - Both directions at once

    func testCombinedGadgetIsNotACollision() {
        // A UCI150 exposes a mic AND a speaker under one name. That is one
        // device in each list, not two in either — the case AudioDevicePairing
        // relies on. Concatenating the lists would invent a collision here.
        let warning = AudioDeviceList.collisionWarning(
            inputs: ["USB Audio Device", "KT USB Audio"],
            outputs: ["USB Audio Device", "KT USB Audio"])
        XCTAssertNil(warning)
    }

    func testCollisionInInputsAloneIsReported() {
        let warning = AudioDeviceList.collisionWarning(
            inputs: ["USB Audio Device", "USB Audio Device"],
            outputs: ["USB Audio Device"])
        XCTAssertNotNil(warning)
    }

    func testCollisionInOutputsAloneIsReported() {
        let warning = AudioDeviceList.collisionWarning(
            inputs: ["USB Audio Device"],
            outputs: ["USB Audio Device", "USB Audio Device"])
        XCTAssertNotNil(warning)
    }

    func testANameCollidingInBothDirectionsIsReportedOnce() {
        // The 7300 + UCI150 case: both gadgets are duplex, so the same name
        // collides in inputs AND outputs. The user has one problem, not two.
        let warning = AudioDeviceList.collisionWarning(
            inputs: ["USB Audio Device", "USB Audio Device"],
            outputs: ["USB Audio Device", "USB Audio Device"])
        let text = try! XCTUnwrap(warning)
        let occurrences = text.components(separatedBy: "USB Audio Device").count - 1
        XCTAssertEqual(occurrences, 1, "named twice in: \(text)")
    }

    func testDistinctCollisionsInEachDirectionAreBothReported() {
        let warning = AudioDeviceList.collisionWarning(
            inputs: ["Mic X", "Mic X"],
            outputs: ["Spkr Y", "Spkr Y"])
        let text = try! XCTUnwrap(warning)
        XCTAssertTrue(text.contains("Mic X") && text.contains("Spkr Y"), text)
    }

    func testNoDevicesAtAllIsNoWarning() {
        XCTAssertNil(AudioDeviceList.collisionWarning(inputs: [], outputs: []))
    }

    // MARK: - A real captured enumeration

    /// Verbatim output of `astar-audio`'s own enumeration on a Mac with two
    /// same-named devices present (astar-9d41). Captured rather than invented,
    /// so this pins the exact shape the engine hands the UI — including that
    /// both entries arrive with the identical DeviceId `in:ASTAR DUP TEST`.
    private static let capturedInputs = [
        "USB Audio Device", "KT USB Audio", "BlackHole 2ch",
        "ASTAR DUP TEST", "ASTAR DUP TEST",
    ]
    private static let capturedOutputs = [
        "USB Audio Device", "BlackHole 2ch", "Mac mini Speakers",
        "ASTAR DUP TEST", "ASTAR DUP TEST",
    ]

    func testCapturedEnumerationCollapsesToWhatIsAddressable() {
        XCTAssertEqual(
            AudioDeviceList.selectable(from: Self.capturedInputs),
            ["USB Audio Device", "KT USB Audio", "BlackHole 2ch", "ASTAR DUP TEST"])
    }

    func testCapturedEnumerationWarnsOnceAboutTheCollidingPair() {
        let text = try! XCTUnwrap(
            AudioDeviceList.collisionWarning(
                inputs: Self.capturedInputs, outputs: Self.capturedOutputs))
        XCTAssertEqual(text.components(separatedBy: "ASTAR DUP TEST").count - 1, 1)
        // The genuinely-duplex UCI150 appears in both lists and must NOT be
        // reported: one entry per direction is a pair, not a collision.
        XCTAssertFalse(text.contains("USB Audio Device"), text)
    }

    // MARK: - Selection safety

    func testASelectionThatVanishedIsReportedMissing() {
        // Config restores "USB Audio Device" but the gadget is unplugged.
        XCTAssertFalse(
            AudioDeviceList.isPresent("USB Audio Device", in: ["KT USB Audio"]))
    }

    func testASelectionStillPresentIsNotMissing() {
        XCTAssertTrue(
            AudioDeviceList.isPresent("USB Audio Device", in: ["USB Audio Device"]))
    }

    func testNilSelectionIsAlwaysPresent() {
        // nil means "system default", which always exists.
        XCTAssertTrue(AudioDeviceList.isPresent(nil, in: []))
    }
}
