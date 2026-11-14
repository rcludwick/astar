// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class VoxGateTests: XCTestCase {
    private let t0 = Date(timeIntervalSince1970: 1_000)

    private func at(_ ms: Double) -> Date { t0.addingTimeInterval(ms / 1000) }

    func testRisingAboveThresholdKeys() {
        var gate = VoxGate(config: VoxConfig(thresholdDBFS: -40, hangoverMS: 250))

        XCTAssertFalse(gate.update(level: -50, now: at(0)), "below threshold → closed")
        XCTAssertTrue(gate.update(level: -20, now: at(10)), "above threshold → keyed")
    }

    func testStaysKeyedUntilHangoverElapses() {
        var gate = VoxGate(config: VoxConfig(thresholdDBFS: -40, hangoverMS: 250))

        XCTAssertTrue(gate.update(level: -20, now: at(0)))  // key
        // Hangover is measured from the moment the level first drops below.
        XCTAssertTrue(
            gate.update(level: -50, now: at(100)),
            "first sample below → hangover starts, still keyed")
        XCTAssertTrue(
            gate.update(level: -50, now: at(349)),
            "just under 250ms after the drop → still keyed")
        XCTAssertFalse(
            gate.update(level: -50, now: at(350)),
            "250ms after the drop → unkeyed")
    }

    func testChatteringBelowThenAboveResetsHangover() {
        var gate = VoxGate(config: VoxConfig(thresholdDBFS: -40, hangoverMS: 250))

        XCTAssertTrue(gate.update(level: -20, now: at(0)))  // key
        XCTAssertTrue(gate.update(level: -50, now: at(100)))  // hangover starts at 100
        // A sample back over threshold resets the hangover window…
        XCTAssertTrue(gate.update(level: -10, now: at(150)))
        // …so the hangover only restarts on the next drop (t=350), and we stay
        // keyed until 250ms after THAT (t=600), not 250ms after the first drop.
        XCTAssertTrue(
            gate.update(level: -50, now: at(350)),
            "new drop restarts hangover; still keyed")
        XCTAssertTrue(
            gate.update(level: -50, now: at(599)),
            "just under 250ms after the restart → still keyed")
        XCTAssertFalse(
            gate.update(level: -50, now: at(600)),
            "250ms after the restarted drop (350) → unkey")
    }

    func testAttackRequiresSustainedLevel() {
        var gate = VoxGate(config: VoxConfig(thresholdDBFS: -40, hangoverMS: 250, attackMS: 50))

        XCTAssertFalse(gate.update(level: -10, now: at(0)), "attack not yet satisfied")
        XCTAssertFalse(gate.update(level: -10, now: at(49)), "still within attack window")
        XCTAssertTrue(gate.update(level: -10, now: at(50)), "attack satisfied → key")
    }

    func testAttackResetsIfLevelDips() {
        var gate = VoxGate(config: VoxConfig(thresholdDBFS: -40, hangoverMS: 250, attackMS: 50))

        XCTAssertFalse(gate.update(level: -10, now: at(0)))
        XCTAssertFalse(gate.update(level: -60, now: at(30)), "dip resets the attack timer")
        XCTAssertFalse(gate.update(level: -10, now: at(60)), "attack restarts; not yet 50ms")
        XCTAssertTrue(gate.update(level: -10, now: at(110)), "50ms after restart → key")
    }

    func testDefaultHangoverIsHalfSecond() {
        // The shipped default (used by the live CallSession VOX gate) holds PTT
        // for a 500 ms tail so a short pause mid-sentence doesn't drop it.
        XCTAssertEqual(VoxConfig().hangoverMS, 500)
    }

    func testDefaultHangoverBridgesAShortPause() {
        var gate = VoxGate()  // default config

        XCTAssertTrue(gate.update(level: -20, now: at(0)), "speech → keyed")
        XCTAssertTrue(gate.update(level: -50, now: at(10)), "pause begins → hangover starts")
        XCTAssertTrue(
            gate.update(level: -50, now: at(400)),
            "400 ms pause is within the 500 ms tail → still keyed")
        XCTAssertTrue(
            gate.update(level: -50, now: at(509)),
            "just under 500 ms after the drop → still keyed")
        XCTAssertFalse(
            gate.update(level: -50, now: at(510)),
            "500 ms after the drop → finally unkeyed")
    }

    // MARK: - The shipped 120 ms attack, polled every 50 ms

    private func shippedGate() -> VoxGate {
        VoxGate(config: VoxConfig(thresholdDBFS: -50, hangoverMS: 1500, attackMS: 120))
    }

    /// Feed `level` at each 50 ms poll in `polls`, quiet otherwise, from 0 to
    /// `untilMS`; return whether the gate ever keyed.
    private func everKeys(loudAt polls: Set<Int>, untilMS: Int = 600) -> Bool {
        var gate = shippedGate()
        var keyed = false
        for ms in stride(from: 0, through: untilMS, by: 50) {
            let level: Float = polls.contains(ms) ? -20 : -60
            keyed = gate.update(level: level, now: at(Double(ms))) || keyed
        }
        return keyed
    }

    func testA50msBurstDoesNotKeyThe120msAttack() {
        // Loud for one 50 ms poll interval (two consecutive samples).
        XCTAssertFalse(everKeys(loudAt: [100, 150]), "a 50 ms burst must not key")
    }

    func testA200msBurstKeysThe120msAttack() {
        XCTAssertTrue(everKeys(loudAt: [100, 150, 200, 250, 300]), "200 ms of voice keys")
    }

    func testADipDuringTheAttackRestartsIt() {
        var gate = shippedGate()
        XCTAssertFalse(gate.update(level: -20, now: at(0)))
        XCTAssertFalse(gate.update(level: -20, now: at(100)))
        XCTAssertFalse(gate.update(level: -60, now: at(110)), "dip: attack restarts")
        XCTAssertFalse(
            gate.update(level: -20, now: at(150)), "loud again: the attack counts from here")
        XCTAssertFalse(gate.update(level: -20, now: at(250)), "100 ms since the restart")
        XCTAssertTrue(gate.update(level: -20, now: at(270)), "120 ms since the restart")
    }

    // MARK: - The meter floor

    func testAFloorReadingNeverKeysEvenAtTheSliderMinimum() {
        var gate = VoxGate(
            config: VoxConfig(
                thresholdDBFS: VoxThreshold.range.lowerBound, hangoverMS: 500, attackMS: 120))
        for ms in stride(from: 0, through: 5_000, by: 50) {
            XCTAssertFalse(gate.update(level: -60, now: at(Double(ms))), "silent room at \(ms) ms")
        }
    }

    func testAFloorReadingNeverKeysEvenWithAThresholdAtTheFloor() {
        // A hand-built config the slider cannot produce: still no keying.
        var gate = VoxGate(config: VoxConfig(thresholdDBFS: -60, hangoverMS: 500))
        XCTAssertFalse(gate.update(level: -60, now: at(0)))
        XCTAssertFalse(gate.update(level: -60, now: at(1_000)))
    }

    func testAFloorReadingClosesAnOpenGate() {
        var gate = VoxGate(config: VoxConfig(thresholdDBFS: -60, hangoverMS: 100))
        XCTAssertTrue(gate.update(level: -40, now: at(0)))
        XCTAssertTrue(gate.update(level: -60, now: at(50)), "hangover running")
        XCTAssertFalse(gate.update(level: -60, now: at(150)), "the floor lets go")
    }
}
