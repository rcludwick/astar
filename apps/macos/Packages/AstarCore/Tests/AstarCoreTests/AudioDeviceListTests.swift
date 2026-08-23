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
        XCTAssertNil(
            AudioDeviceList.collisionWarning(
                inputs: ["USB Audio Device", "KT USB Audio"],
                outputs: ["USB Audio Device", "KT USB Audio"],
                selectedInput: "USB Audio Device",
                selectedOutput: "USB Audio Device"))
    }

    func testCollisionInInputsAloneIsReported() {
        XCTAssertNotNil(
            AudioDeviceList.collisionWarning(
                inputs: ["USB Audio Device", "USB Audio Device"],
                outputs: ["USB Audio Device"],
                selectedInput: "USB Audio Device",
                selectedOutput: "USB Audio Device"))
    }

    func testCollisionInOutputsAloneIsReported() {
        XCTAssertNotNil(
            AudioDeviceList.collisionWarning(
                inputs: ["USB Audio Device"],
                outputs: ["USB Audio Device", "USB Audio Device"],
                selectedInput: "USB Audio Device",
                selectedOutput: "USB Audio Device"))
    }

    func testNoDevicesAtAllIsNoWarning() {
        XCTAssertNil(
            AudioDeviceList.collisionWarning(
                inputs: [], outputs: [], selectedInput: nil, selectedOutput: nil))
    }

    // MARK: - Is THIS selection ambiguous (the red picker border)

    func testAnAmbiguousSelectionIsFlagged() {
        XCTAssertTrue(
            AudioDeviceList.isAmbiguous(
                "USB Audio Device", in: ["USB Audio Device", "USB Audio Device"]))
    }

    func testAUniqueSelectionIsNotFlagged() {
        XCTAssertFalse(
            AudioDeviceList.isAmbiguous(
                "KT USB Audio", in: ["USB Audio Device", "USB Audio Device", "KT USB Audio"]))
    }

    func testSystemDefaultIsNotFlagged() {
        XCTAssertFalse(AudioDeviceList.isAmbiguous(nil, in: ["A", "A"]))
    }

    func testAMissingSelectionIsNotFlagged() {
        XCTAssertFalse(AudioDeviceList.isAmbiguous("Unplugged", in: ["A", "A"]))
    }

    func testFlaggingIsPerDirection() {
        // The list passed in IS the direction. A name unique among inputs is
        // addressable there even if it clashes among outputs, so the caller
        // must get false for the input picker.
        XCTAssertFalse(AudioDeviceList.isAmbiguous("Thing", in: ["Thing"]))
        XCTAssertTrue(AudioDeviceList.isAmbiguous("Thing", in: ["Thing", "Thing"]))
    }

    func testACombinedGadgetIsNotFlagged() {
        // A UCI150 appears once in inputs and once in outputs — one entry per
        // direction, which is a pair, not a clash.
        XCTAssertFalse(
            AudioDeviceList.isAmbiguous(
                "USB Audio Device", in: ["USB Audio Device", "KT USB Audio"]))
    }

    // MARK: - The short caption under a picker

    func testNoCaptionForAUniqueSelection() {
        XCTAssertNil(
            AudioDeviceList.ambiguityNote(for: "KT USB Audio", in: ["KT USB Audio", "A", "A"]))
    }

    func testNoCaptionForTheSystemDefault() {
        XCTAssertNil(AudioDeviceList.ambiguityNote(for: nil, in: ["A", "A"]))
    }

    func testCaptionNamesTheDevice() {
        let text = try! XCTUnwrap(
            AudioDeviceList.ambiguityNote(
                for: "ASTAR DUP TEST", in: ["ASTAR DUP TEST", "ASTAR DUP TEST"]))
        XCTAssertTrue(text.contains("ASTAR DUP TEST"), text)
    }

    func testCaptionIsShortEnoughToSitUnderAPicker() {
        // The full explanation lives in the main-page banner; this one sits in a
        // narrow column under a control and has to stay one short line.
        let text = try! XCTUnwrap(
            AudioDeviceList.ambiguityNote(
                for: "USB Audio Device", in: ["USB Audio Device", "USB Audio Device"]))
        XCTAssertLessThan(text.count, 60, "too long for a caption: \(text)")
        XCTAssertFalse(
            text.lowercased().contains("audio midi setup"),
            "the how-to-fix belongs in the banner, not the caption: \(text)")
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

    func testCapturedEnumerationWarnsOnceWhenTheClashingDeviceIsSelected() {
        let text = try! XCTUnwrap(
            AudioDeviceList.collisionWarning(
                inputs: Self.capturedInputs, outputs: Self.capturedOutputs,
                selectedInput: "ASTAR DUP TEST", selectedOutput: "ASTAR DUP TEST"))
        XCTAssertEqual(text.components(separatedBy: "ASTAR DUP TEST").count - 1, 1)
        // The genuinely-duplex UCI150 appears once in each list and must NOT be
        // reported: one entry per direction is a pair, not a collision.
        XCTAssertFalse(text.contains("USB Audio Device"), text)
    }

    func testCapturedEnumerationStaysQuietOnTheRealHardware() {
        // Same machine, but the rig is running the UCI150. The ASTAR DUP TEST
        // clash is real and still nothing to do with this call.
        XCTAssertNil(
            AudioDeviceList.collisionWarning(
                inputs: Self.capturedInputs, outputs: Self.capturedOutputs,
                selectedInput: "USB Audio Device", selectedOutput: "USB Audio Device"))
    }

    func testCapturedEnumerationFlagsOnlyTheClashingPicker() {
        XCTAssertTrue(AudioDeviceList.isAmbiguous("ASTAR DUP TEST", in: Self.capturedInputs))
        XCTAssertFalse(AudioDeviceList.isAmbiguous("USB Audio Device", in: Self.capturedInputs))
        XCTAssertFalse(AudioDeviceList.isAmbiguous("KT USB Audio", in: Self.capturedInputs))
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
