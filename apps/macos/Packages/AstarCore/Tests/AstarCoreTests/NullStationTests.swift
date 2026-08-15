// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// Exercises `NullStation` — the do-nothing fallback used when a real `Station`
/// can't be constructed (and in SwiftUI previews). Every method is a no-op (or a
/// fixed empty/idle value), so the contract under test is "callable, never
/// throws (except the documented mint), returns the resting value." The live
/// `Station` conformance in `Station+Driving.swift` is IOKit/FFI and is a
/// documented unit-test gap; this covers the testable half of that file.
final class NullStationTests: XCTestCase {
    func testReadSnapshotIsIdle() throws {
        let station = NullStation()
        XCTAssertEqual(try station.readSnapshot(), .idle)
    }

    func testReadEventIsNil() throws {
        XCTAssertNil(try NullStation().readEvent())
    }

    func testDialPathsAreNoOps() {
        let station = NullStation()
        XCTAssertNoThrow(try station.connectWT(destNode: "55553"))
        XCTAssertNoThrow(try station.connectWT(destNode: "55553", address: "127.0.0.1:4569"))
        XCTAssertNoThrow(try station.connect(dest: "p", calling: "c", secret: nil))
        XCTAssertNoThrow(try station.disconnect())
    }

    func testPTTIsNoOp() {
        XCTAssertNoThrow(try NullStation().setPTT(true))
        XCTAssertNoThrow(try NullStation().setPTT(false))
    }

    func testSendDTMFIsNoOp() {
        let station = NullStation()
        XCTAssertNoThrow(try station.sendDTMF("5"))
        XCTAssertNoThrow(try station.sendDTMF("*"))
        XCTAssertNoThrow(try station.sendDTMF("#"))
    }

    func testTestMintTokenThrowsNoEngine() {
        XCTAssertThrowsError(try NullStation().testMintToken()) { error in
            XCTAssertEqual(error as? NullStationError, .noEngine)
        }
    }

    func testDeviceEnumerationIsEmpty() throws {
        XCTAssertEqual(try NullStation().listInputs(), [])
        XCTAssertEqual(try NullStation().listOutputs(), [])
    }

    func testDeviceAndGainSettersAreNoOps() {
        let station = NullStation()
        XCTAssertNoThrow(try station.setDevices(input: "Mic", output: "Speaker"))
        XCTAssertNoThrow(try station.setDevices(input: nil, output: nil))
        XCTAssertNoThrow(try station.setInputGain(1.5))
        XCTAssertNoThrow(try station.setOutputGain(0.5))
    }

    func testProcessingTogglesAreNoOps() {
        let station = NullStation()
        XCTAssertNoThrow(try station.setCompression(true))
        XCTAssertNoThrow(try station.setCompressionLevel(0.7))
        XCTAssertNoThrow(try station.setTxTrim(0.5))
        XCTAssertNoThrow(try station.setNoiseReduction(true))
    }

    func testMonitorAndCharacterizationAreInert() throws {
        let station = NullStation()
        XCTAssertNoThrow(try station.monitorStart(input: "Mic"))
        XCTAssertNoThrow(try station.monitorStop())
        XCTAssertEqual(try station.micSpectrum(), [])
        XCTAssertEqual(try station.characterize(harmonicComb: true), "")
        XCTAssertEqual(try station.characterize(harmonicComb: false), "")
        XCTAssertNoThrow(try station.setMicProfile(#"{"notches":[]}"#))
        XCTAssertNoThrow(try station.setMicProfile(nil))
    }

    /// A `CallSession` driven by `NullStation` polls cleanly to the idle resting
    /// state and never crashes — the preview/fallback path.
    func testCallSessionOverNullStationPollsIdle() {
        let session = CallSession(
            station: NullStation(),
            audioStore: MemoryAudioStore())
        session.poll()
        XCTAssertEqual(session.status, .idle)
        XCTAssertFalse(session.ptt)
        XCTAssertEqual(session.inputs(), [], "NullStation enumerates no devices")
        XCTAssertEqual(session.outputs(), [])
    }
}
