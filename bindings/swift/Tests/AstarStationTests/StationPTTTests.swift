// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// StationPTTTests.swift — the PTT source, lease and transmit time-out C-ABI as
// Swift sees it.
//
// Offline and hardware-free: every station here is a fresh, idle `Station()`
// with no call, so the engine refuses every press with -3 and nothing can key.
// The refusal codes asserted below ARE the safety contract: the transmit
// time-out can never be set to 0, below a minute or above ten, and a huge
// Duration must never wrap round to a value the engine would accept.

import CAstarStation
import XCTest

@testable import AstarStation

@available(macOS 13.0, iOS 16.0, *)
final class StationPTTTests: XCTestCase {

    /// Asserts `body` throws a `StationError` whose code is exactly `code`.
    private func assertCode(
        _ code: Int32, _ message: String, file: StaticString = #filePath, line: UInt = #line,
        _ body: () throws -> Void
    ) {
        XCTAssertThrowsError(try body(), message, file: file, line: line) { error in
            guard let e = error as? StationError else {
                return XCTFail("expected StationError, got \(error)", file: file, line: line)
            }
            XCTAssertEqual(e.code, code, message, file: file, line: line)
        }
    }

    // MARK: Types

    func testSourceRawValuesMatchTheHeader() {
        // abiValue reads IAX_PTT_* from astar.h; rawValue is the Swift literal.
        // A renumbered header constant must fail here, not key the wrong source.
        for source in StationPTTSource.allCases {
            XCTAssertEqual(source.abiValue, source.rawValue, "\(source)")
        }
        XCTAssertEqual(StationPTTSource.allCases.map(\.rawValue), [0, 1, 2, 3, 4])
    }

    func testSourceBitsAndDescriptions() {
        XCTAssertEqual(StationPTTSource.manual.bit, .manual)
        XCTAssertEqual(StationPTTSource.vox.bit, .vox)
        XCTAssertEqual(StationPTTSource.serial.bit, .serial)
        XCTAssertEqual(StationPTTSource.bluetooth.bit, .bluetooth)
        XCTAssertEqual(StationPTTSource.legacy.bit, .legacy)
        XCTAssertEqual(StationPTTSourceSet.bluetooth.rawValue, 8)
        XCTAssertEqual(StationPTTSourceSet.legacy.rawValue, 16)
        XCTAssertEqual(
            StationPTTSource.allCases.map(\.description),
            ["manual", "vox", "serial", "bluetooth", "legacy"])
    }

    func testForcedReasonRawValues() {
        XCTAssertNil(StationPTTForcedReason(rawValue: 0), "0 is IAX_PTT_FORCED_NONE")
        XCTAssertEqual(StationPTTForcedReason(rawValue: 1), .leaseExpired)
        XCTAssertEqual(StationPTTForcedReason(rawValue: 2), .txTimeout)
        XCTAssertNil(StationPTTForcedReason(rawValue: 3))
    }

    func testErrorCodeConstants() {
        XCTAssertEqual(StationError.notLeasedCode, -25)
        XCTAssertEqual(StationError.invalidArgumentCode, -26)
        XCTAssertEqual(StationError.pttLatchedCode, -27)
    }

    // MARK: Duration → milliseconds

    func testSaturatingMilliseconds() {
        XCTAssertEqual(Station.saturatingMilliseconds(.milliseconds(1_500)), 1_500)
        XCTAssertEqual(Station.saturatingMilliseconds(.seconds(120)), 120_000)
        XCTAssertEqual(Station.saturatingMilliseconds(.microseconds(1_999)), 1, "whole ms")
        XCTAssertEqual(Station.saturatingMilliseconds(.zero), 0)
        XCTAssertEqual(Station.saturatingMilliseconds(.seconds(-60)), 0)
        XCTAssertEqual(
            Station.saturatingMilliseconds(.milliseconds(Int64(UInt32.max) + 1 + 120_000)),
            UInt32.max, "saturates, never wraps")
        XCTAssertEqual(Station.saturatingMilliseconds(.seconds(Int64.max)), UInt32.max)
    }

    // MARK: Fresh station

    func testFreshStationSnapshotsNoPTTState() throws {
        let snap = try Station().snapshot()
        XCTAssertEqual(snap.pttSources, [])
        XCTAssertEqual(snap.pttLatched, [])
        XCTAssertEqual(snap.pttForcedSeq, 0)
        XCTAssertEqual(snap.pttForcedSources, [])
        XCTAssertNil(snap.pttForcedReason)
        XCTAssertEqual(snap.pttTxTimeoutMS, 120_000, "the engine starts at the 2 min default")
    }

    func testSnapshotConversionMapsEveryPTTField() {
        // A hand-built IaxState: no station, nothing keyed. Every mask is
        // different, so a dropped field or two swapped fields fail here. A
        // fresh station only ever reports the defaults.
        var raw = IaxState()
        raw.ptt_sources = 0b01001
        raw.ptt_latched = 0b00100
        raw.ptt_forced_seq = 3
        raw.ptt_forced_sources = 0b10001
        raw.ptt_forced_reason = 2
        raw.ptt_tx_timeout_ms = 300_000
        let snap = Station.makeSnapshot(raw)
        XCTAssertEqual(snap.pttSources, [.manual, .bluetooth])
        XCTAssertEqual(snap.pttLatched, .serial)
        XCTAssertEqual(snap.pttForcedSeq, 3)
        XCTAssertEqual(snap.pttForcedSources, [.manual, .legacy])
        XCTAssertEqual(snap.pttForcedReason, .txTimeout)
        XCTAssertEqual(snap.pttTxTimeoutMS, 300_000)

        raw.ptt_forced_reason = 1
        XCTAssertEqual(Station.makeSnapshot(raw).pttForcedReason, .leaseExpired)
        raw.ptt_forced_reason = 7
        XCTAssertNil(Station.makeSnapshot(raw).pttForcedReason, "an unknown reason is nil")
        raw.ptt_forced_reason = 0
        XCTAssertNil(Station.makeSnapshot(raw).pttForcedReason, "0 is IAX_PTT_FORCED_NONE")
    }

    func testSnapshotConversionReadsTheVoxLevelFromItsOwnField() {
        // VOX keys from `vox_db`, the meters from `input_db`. Distinct values
        // so a swap or a copy of the wrong one fails here.
        var raw = IaxState()
        raw.input_db = -20
        raw.vox_db = -33.5
        let snap = Station.makeSnapshot(raw)
        XCTAssertEqual(snap.voxDB, -33.5)
        XCTAssertEqual(snap.inputDB, -20)
    }

    func testAFreshStationReportsTheVoxFloor() throws {
        XCTAssertEqual(try Station().snapshot().voxDB, -60)
    }

    // MARK: Transmit time-out

    func testSetTxTimeoutRefusesOutOfRangeAndKeepsTheOldValue() throws {
        let st = try Station()
        for bad: Duration in [
            .seconds(0), .milliseconds(59_999), .seconds(59), .seconds(601),
            .milliseconds(600_001), .seconds(-60),
        ] {
            assertCode(-26, "\(bad) must be refused") { try st.setTxTimeout(bad) }
            XCTAssertEqual(try st.snapshot().pttTxTimeoutMS, 120_000, "\(bad) left it alone")
        }
    }

    func testSetTxTimeoutAcceptsBothEndsAndReadsBack() throws {
        let st = try Station()
        try st.setTxTimeout(.seconds(60))
        XCTAssertEqual(try st.snapshot().pttTxTimeoutMS, 60_000)
        try st.setTxTimeout(.seconds(600))
        XCTAssertEqual(try st.snapshot().pttTxTimeoutMS, 600_000)
        try st.setTxTimeout(.seconds(300))
        XCTAssertEqual(try st.snapshot().pttTxTimeoutMS, 300_000)
    }

    func testHugeTimeoutSaturatesAndIsRefused() throws {
        // 2^32 ms + 120 000 ms truncates to exactly 120 000 ms. A wrapping
        // conversion would turn this into a valid 2 min, so it must saturate
        // and be refused instead.
        let st = try Station()
        try st.setTxTimeout(.seconds(300))
        assertCode(-26, "a wrapped value must not slip through") {
            try st.setTxTimeout(.milliseconds(Int64(UInt32.max) + 1 + 120_000))
        }
        XCTAssertEqual(try st.snapshot().pttTxTimeoutMS, 300_000)
    }

    // MARK: Sources and the lease

    func testLeaseRenewOnAFreshStationIsNotLeased() throws {
        let st = try Station()
        assertCode(-25, "renew never presses") { try st.pttLeaseRenew(.bluetooth) }
        assertCode(StationError.notLeasedCode, "same constant") { try st.pttLeaseRenew(.bluetooth) }
        XCTAssertEqual(try st.snapshot().pttSources, [])
    }

    func testBluetoothPressWithoutALeaseIsRefused() throws {
        // Bluetooth keys only through a lease, so the engine bounds every
        // Bluetooth hold: a plain press is refused before the arbiter is
        // touched, holding and latching nothing.
        let st = try Station()
        assertCode(-26, "bluetooth keys only via a lease") {
            try st.pttSource(.bluetooth, on: true)
        }
        XCTAssertEqual(try st.snapshot().pttLatched, [])
        XCTAssertEqual(try st.snapshot().pttSources, [])
        XCTAssertNoThrow(try st.pttSource(.bluetooth, on: false), "its release is accepted")
    }

    func testRefusedLegacyPressLatchesNothing() throws {
        // Legacy (setPTT, astar-server /key) latches only on the transmit
        // time-out or stop-all. A press the console refuses leaves nothing
        // held or latched, so the next press is refused for the same reason
        // (-3), never as latched (-27).
        let st = try Station()
        assertCode(-3, "no call: the console refuses") { try st.setPTT(true) }
        XCTAssertEqual(try st.snapshot().pttLatched, [], "a refused legacy press latches nothing")
        XCTAssertEqual(try st.snapshot().pttSources, [])
        assertCode(-3, "still the console's refusal, not -27") { try st.setPTT(true) }
        assertCode(-3, "the same through the source call") { try st.pttSource(.legacy, on: true) }
        XCTAssertEqual(try st.snapshot().pttLatched, [])
    }

    func testStopAllWhileIdleDoesNotThrowAndNeverClearsALatch() throws {
        // Idle, nothing can be held, so the observable difference between the
        // universal stop and a release is the latch: stop-all never clears
        // one, while a release clears that source's latch. A refused press
        // latches every source but Legacy, so the four are latched here.
        let st = try Station()
        let four: StationPTTSourceSet = [.manual, .vox, .serial, .bluetooth]
        for source: StationPTTSource in [.manual, .vox, .serial] {
            assertCode(-3, "no call: \(source) is refused and latched") {
                try st.pttSource(source, on: true)
            }
        }
        assertCode(-3, "no call: the lease press is refused and latched") {
            try st.pttLeasePress(.bluetooth, ttl: .milliseconds(1_500))
        }
        XCTAssertEqual(try st.snapshot().pttLatched, four)
        XCTAssertNoThrow(try st.pttStopAll())
        XCTAssertEqual(try st.snapshot().pttSources, [])
        XCTAssertEqual(try st.snapshot().pttLatched, four, "stop-all keeps every latch")
        for source: StationPTTSource in [.manual, .vox, .serial, .bluetooth] {
            try st.pttSource(source, on: false)
        }
        XCTAssertEqual(try st.snapshot().pttLatched, [], "each release re-arms its source")
    }

    func testLeasePressWhileIdleIsRefusedAndLatches() throws {
        let st = try Station()
        assertCode(-3, "no call: the console refuses") {
            try st.pttLeasePress(.bluetooth, ttl: .milliseconds(1_500))
        }
        let snap = try st.snapshot()
        XCTAssertEqual(snap.pttSources, [], "a refused press is rolled back")
        XCTAssertEqual(snap.pttLatched, .bluetooth, "and latched until released")
        assertCode(-27, "latched: release first") {
            try st.pttLeasePress(.bluetooth, ttl: .milliseconds(1_500))
        }
        assertCode(-25, "the rollback left no lease") { try st.pttLeaseRenew(.bluetooth) }
        try st.pttSource(.bluetooth, on: false)
        XCTAssertEqual(try st.snapshot().pttLatched, [], "release clears the latch")
    }

    func testLeasePressRefusesBadArguments() throws {
        let st = try Station()
        assertCode(-26, "zero TTL") { try st.pttLeasePress(.bluetooth, ttl: .zero) }
        assertCode(-26, "sub-millisecond TTL rounds to 0") {
            try st.pttLeasePress(.bluetooth, ttl: .microseconds(999))
        }
        assertCode(-26, "TTL over 5 s") { try st.pttLeasePress(.bluetooth, ttl: .milliseconds(5_001)) }
        assertCode(-26, "a wrapped TTL must not become 1.5 s") {
            try st.pttLeasePress(.bluetooth, ttl: .milliseconds(Int64(UInt32.max) + 1 + 1_500))
        }
        assertCode(-26, "Bluetooth only") { try st.pttLeasePress(.manual, ttl: .milliseconds(1_500)) }
        XCTAssertEqual(try st.snapshot().pttLatched, [], "an invalid argument records nothing")
    }

    func testManualPressWhileIdleIsRefusedLatchedAndReleasable() throws {
        let st = try Station()
        assertCode(-3, "no call") { try st.pttSource(.manual, on: true) }
        XCTAssertEqual(try st.snapshot().pttLatched, .manual)
        assertCode(-27, "latched") { try st.pttSource(.manual, on: true) }
        XCTAssertNoThrow(try st.pttSource(.manual, on: false), "release is never refused")
        XCTAssertEqual(try st.snapshot().pttLatched, [])
    }

    func testReleaseOfEverySourceWhileIdleNeverThrows() throws {
        let st = try Station()
        for source in StationPTTSource.allCases {
            XCTAssertNoThrow(try st.pttSource(source, on: false), "\(source)")
        }
    }
}
