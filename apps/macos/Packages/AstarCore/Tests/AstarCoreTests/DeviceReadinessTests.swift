// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `DeviceReadiness.classify` — the decision behind the tick or triangle next
/// to the device pickers. Split out from the CoreAudio queries precisely so it
/// can be tested without hardware.
final class DeviceReadinessTests: XCTestCase {
    func testAbsentDeviceIsNotFoundWhateverElseIsTrue() {
        XCTAssertEqual(
            DeviceReadiness.classify(present: false, hogOwner: -1, ourPID: 42), .notFound)
        // Even a hog owner cannot make a device that is not there "busy".
        XCTAssertEqual(
            DeviceReadiness.classify(present: false, hogOwner: 99, ourPID: 42), .notFound)
    }

    func testPresentAndUnheldIsReady() {
        XCTAssertEqual(DeviceReadiness.classify(present: true, hogOwner: -1, ourPID: 42), .ready)
    }

    func testHeldByAnotherProcessIsBusy() {
        XCTAssertEqual(DeviceReadiness.classify(present: true, hogOwner: 99, ourPID: 42), .busy)
    }

    /// A device astar itself held would be available to astar. It hogs nothing
    /// today; this keeps the rule honest if that ever changes.
    func testHeldByUsIsReady() {
        XCTAssertEqual(DeviceReadiness.classify(present: true, hogOwner: 42, ourPID: 42), .ready)
    }

    /// Aggregate and virtual devices commonly refuse the hog-mode query.
    /// "Cannot tell" must read as ready, not as busy — warning about a device
    /// that works is worse than staying quiet.
    func testUnknownHogSupportReadsAsReady() {
        XCTAssertEqual(DeviceReadiness.classify(present: true, hogOwner: nil, ourPID: 42), .ready)
    }

    /// Only the two failures carry a note, and it names the device so someone
    /// with several does not have to guess which one.
    func testNotesNameTheDeviceAndOnlyAppearOnFailure() {
        XCTAssertNil(DeviceReadiness.ready.note(for: "KT USB Audio"))
        XCTAssertEqual(
            DeviceReadiness.notFound.note(for: "KT USB Audio"),
            "KT USB Audio isn’t connected. Plug it back in, or pick another device.")
        XCTAssertEqual(
            DeviceReadiness.busy.note(for: "KT USB Audio"),
            "KT USB Audio is in use by another app. Quit it, or pick another device.")
        // No device named — the picker is on "System default".
        XCTAssertEqual(
            DeviceReadiness.notFound.note(for: nil),
            "the system default device isn’t connected. Plug it back in, or pick another device.")
    }
}
