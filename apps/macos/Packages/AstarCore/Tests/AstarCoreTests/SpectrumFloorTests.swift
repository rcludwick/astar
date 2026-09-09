// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class SpectrumFloorTests: XCTestCase {
    // 11 bins → index fractions 0.0, 0.1, … 1.0.
    private let ramp: [Float] = [-100, -90, -80, -70, -60, -50, -40, -30, -20, -10, 0]

    func testMedianOverWholeBandIsTheMiddleBin() {
        // 11 bins, all in band → odd count, middle sample.
        XCTAssertEqual(
            SpectrumFloor.scanBandMedian(ramp, lowFraction: 0, highFraction: 1), -50)
    }

    func testMedianOverASubBandUsesOnlyThatBand() {
        // fractions 0.2…0.6 → bins 2…6 = [-80, -70, -60, -50, -40] → -60.
        XCTAssertEqual(
            SpectrumFloor.scanBandMedian(ramp, lowFraction: 0.2, highFraction: 0.6), -60)
    }

    func testEvenBandCountAveragesTheTwoMiddleSamples() {
        // fractions 0.2…0.5 → bins 2…5 = [-80, -70, -60, -50] → (-70 + -60)/2.
        XCTAssertEqual(
            SpectrumFloor.scanBandMedian(ramp, lowFraction: 0.2, highFraction: 0.5), -65)
    }

    func testUnsortedBinsStillGiveTheMedianNotTheMiddleElement() {
        let jumbled: [Float] = [-10, -100, -50, -20, -80]
        XCTAssertEqual(
            SpectrumFloor.scanBandMedian(jumbled, lowFraction: 0, highFraction: 1), -50)
    }

    func testFewerThanThreeBinsInBandIsNil() {
        // fractions 0.2…0.3 → only two bins.
        XCTAssertNil(SpectrumFloor.scanBandMedian(ramp, lowFraction: 0.2, highFraction: 0.3))
    }

    func testEmptyAndSingleBinAreNil() {
        XCTAssertNil(SpectrumFloor.scanBandMedian([], lowFraction: 0, highFraction: 1))
        XCTAssertNil(SpectrumFloor.scanBandMedian([-40], lowFraction: 0, highFraction: 1))
    }

    func testInvertedBandIsNil() {
        XCTAssertNil(SpectrumFloor.scanBandMedian(ramp, lowFraction: 0.8, highFraction: 0.2))
    }

    func testNonFiniteBinInBandIsNil() {
        var bins = ramp
        bins[4] = .nan
        XCTAssertNil(SpectrumFloor.scanBandMedian(bins, lowFraction: 0, highFraction: 1))
        bins[4] = .infinity
        XCTAssertNil(SpectrumFloor.scanBandMedian(bins, lowFraction: 0, highFraction: 1))
        // A non-finite bin OUTSIDE the band doesn't spoil the estimate.
        XCTAssertEqual(
            SpectrumFloor.scanBandMedian(bins, lowFraction: 0.6, highFraction: 1.0), -20)
    }

    func testLineIsTheMedianPlusTheMargin() {
        XCTAssertEqual(
            SpectrumFloor.line(bins: ramp, lowFraction: 0, highFraction: 1, marginDb: 12), -38)
        XCTAssertEqual(
            SpectrumFloor.line(bins: ramp, lowFraction: 0.2, highFraction: 0.6, marginDb: 30), -30)
    }

    func testLineIsNilWhenThereIsNoMedian() {
        XCTAssertNil(SpectrumFloor.line(bins: [], lowFraction: 0, highFraction: 1, marginDb: 12))
    }
}
