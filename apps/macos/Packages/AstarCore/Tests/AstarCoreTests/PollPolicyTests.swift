// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `PollPolicy.keepsPolling` decides whether the 20 Hz poll may pause when the
/// window leaves the call card. A pause in the wrong place freezes something
/// the user is watching: the serial self-test (astar-d00a), the Bluetooth
/// button's link state, or the red on-air tint and talk timer. So every
/// exemption is pinned on its own, and the whole table is checked against a
/// reference written independently of the implementation.
final class PollPolicyTests: XCTestCase {
    func testSettingsPaneWithNothingLiveStopsPolling() {
        XCTAssertEqual(
            PollPolicy.keepsPolling(
                onCallPane: false, serialActive: false, bluetoothLinked: false, onAir: false),
            false)
    }

    func testCallPaneAlwaysPolls() {
        XCTAssertEqual(
            PollPolicy.keepsPolling(
                onCallPane: true, serialActive: false, bluetoothLinked: false, onAir: false),
            true)
    }

    // astar-d00a: the serial PTT self-test reads state that only the poll advances.
    func testLiveSerialKeepsPollingOffTheCallPane() {
        XCTAssertEqual(
            PollPolicy.keepsPolling(
                onCallPane: false, serialActive: true, bluetoothLinked: false, onAir: false),
            true)
    }

    func testLinkedBluetoothKeepsPollingOffTheCallPane() {
        XCTAssertEqual(
            PollPolicy.keepsPolling(
                onCallPane: false, serialActive: false, bluetoothLinked: true, onAir: false),
            true)
    }

    // Keyed by any source while Settings is open: the tint and the talk timer
    // must keep moving, or an unexpected key would be invisible.
    func testOnAirKeepsPollingOffTheCallPane() {
        XCTAssertEqual(
            PollPolicy.keepsPolling(
                onCallPane: false, serialActive: false, bluetoothLinked: false, onAir: true),
            true)
    }

    /// All 16 rows. The reference is "any input set", stated as `bits != 0`
    /// rather than as the implementation's chain of `||`.
    func testWholeTruthTable() {
        for bits in 0..<16 {
            let onCallPane = bits & 0b0001 != 0
            let serialActive = bits & 0b0010 != 0
            let bluetoothLinked = bits & 0b0100 != 0
            let onAir = bits & 0b1000 != 0
            let expected = bits != 0
            XCTAssertEqual(
                PollPolicy.keepsPolling(
                    onCallPane: onCallPane, serialActive: serialActive,
                    bluetoothLinked: bluetoothLinked, onAir: onAir),
                expected,
                "row \(bits): onCallPane=\(onCallPane) serialActive=\(serialActive) "
                    + "bluetoothLinked=\(bluetoothLinked) onAir=\(onAir)"
            )
        }
    }
}
