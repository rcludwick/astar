// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class RxJitterBoundsTests: XCTestCase {
    func testTheRangeMirrorsTheEnginesClamp() {
        XCTAssertEqual(RxJitterBounds.range, 0...500)
        XCTAssertEqual(RxJitterBounds.step, 10)
    }

    func testBothBoundsAreClamped() {
        XCTAssertEqual(RxJitterBounds.clamped(-1), 0)
        XCTAssertEqual(RxJitterBounds.clamped(501), 500)
        XCTAssertEqual(RxJitterBounds.clamped(40), 40)
    }

    func testAnOrderedWindowIsLeftAlone() {
        let w = RxJitterBounds.repaired(min: 40, max: 200, movingMin: true)
        XCTAssertEqual(w.min, 40)
        XCTAssertEqual(w.max, 200)
    }

    func testTheBoundThatIsNotMovingGivesWay() {
        // Dragging the floor up past the ceiling lifts the ceiling; dragging
        // the ceiling down under the floor drops the floor. The alternative
        // — pinning the dragged bound to the other one — makes the control
        // fight the drag.
        let up = RxJitterBounds.repaired(min: 300, max: 200, movingMin: true)
        XCTAssertEqual(up.min, 300)
        XCTAssertEqual(up.max, 300)

        let down = RxJitterBounds.repaired(min: 300, max: 200, movingMin: false)
        XCTAssertEqual(down.min, 200)
        XCTAssertEqual(down.max, 200)
    }

    func testClampingHappensBeforeTheOrderIsRepaired() {
        let w = RxJitterBounds.repaired(min: 900, max: 700, movingMin: true)
        XCTAssertEqual(w.min, 500)
        XCTAssertEqual(w.max, 500)
    }
}

final class CallQualityLineTests: XCTestCase {
    private let healthy = RxQuality(
        jitterBufferEnabled: true, jitterMS: 12, bufferDepthMS: 60,
        framesLost: 0, framesLate: 0, underruns: 0)

    func testAHealthyCallReadsAsFourNumbersAndNoAlarm() {
        XCTAssertEqual(
            CallQualityLine.text(network: .allstar, quality: healthy),
            "jitter 12 ms · buffer 60 ms · lost 0 · late 0")
    }

    func testUnderrunsAppendOnlyWhenThereAreSome() {
        // The alarm, not a statistic: a permanent "underruns 0" is one more
        // number to read past before you find the one that matters.
        var q = healthy
        q.underruns = 3
        XCTAssertEqual(
            CallQualityLine.text(network: .allstar, quality: q),
            "jitter 12 ms · buffer 60 ms · lost 0 · late 0 · underruns 3")
    }

    func testABufferThatIsOffSaysSoInsteadOfShowingZeros() {
        // With the buffer off every number above means something different
        // (the design doc is explicit that the two must not be compared), so
        // the line says what is running rather than printing them.
        var q = healthy
        q.jitterBufferEnabled = false
        XCTAssertEqual(CallQualityLine.text(network: .allstar, quality: q), "jitter buffer off")
    }

    func testNothingAtAllOffAllStarLink() {
        // Only the IAX2 path carries a sender clock to schedule against, so
        // on every other network these numbers are flat zeros and the line
        // would be a lie of omission.
        for network in [Network.m17, .dstar, .ysf, .nxdn, .dmr, .hamlink] {
            XCTAssertNil(CallQualityLine.text(network: network, quality: healthy), "\(network)")
        }
        XCTAssertNil(CallQualityLine.text(network: nil, quality: healthy), "no call")
    }

    func testTheSpokenFormSpellsTheUnitsOut() {
        XCTAssertEqual(
            CallQualityLine.spoken(network: .allstar, quality: healthy),
            "Network jitter 12 milliseconds, buffer depth 60 milliseconds,"
                + " 0 frames lost, 0 frames late")
        var q = healthy
        q.underruns = 3
        XCTAssertEqual(
            CallQualityLine.spoken(network: .allstar, quality: q),
            "Network jitter 12 milliseconds, buffer depth 60 milliseconds,"
                + " 0 frames lost, 0 frames late, 3 underruns")
        q.jitterBufferEnabled = false
        XCTAssertEqual(CallQualityLine.spoken(network: .allstar, quality: q), "Jitter buffer off")
        XCTAssertNil(CallQualityLine.spoken(network: .m17, quality: healthy))
    }

    func testIdleQualityIsARestingBufferThatIsStillDeclaredOn() {
        XCTAssertTrue(RxQuality.idle.jitterBufferEnabled)
        XCTAssertEqual(RxQuality.idle.jitterMS, 0)
        XCTAssertEqual(RxQuality.idle.underruns, 0)
    }
}
