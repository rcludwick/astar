// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// The negotiated-codec surfacing (astar-eb6c, always-on astar-ef35):
/// `CallSnapshot` carries the codec, and the status card names it whenever one
/// is negotiated — G.711 µ / G.711 A / slin8 / slin16 — with only wideband
/// (slin16) earning the celebratory green tint.
final class CodecBadgeTests: XCTestCase {
    func testSnapshotCarriesNegotiatedFormat() {
        let snap = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -30, rttMS: 40, negotiatedFormat: .slin16
        )
        XCTAssertEqual(snap.negotiatedFormat, .slin16)
    }

    func testNegotiatedFormatDefaultsToNil() {
        // Existing fixtures don't pass it; idle carries none.
        let snap = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -30, rttMS: nil
        )
        XCTAssertNil(snap.negotiatedFormat)
        XCTAssertNil(CallSnapshot.idle.negotiatedFormat)
    }

    func testEveryCodecHasABadgeName() {
        XCTAssertEqual(VoiceFormat.g711u.badge, "G.711 µ")
        XCTAssertEqual(VoiceFormat.g711a.badge, "G.711 A")
        XCTAssertEqual(VoiceFormat.slin.badge, "slin8")
        XCTAssertEqual(VoiceFormat.slin16.badge, "slin16")
    }

    func testOnlySlin16IsWideband() {
        XCTAssertTrue(VoiceFormat.slin16.isWideband)
        XCTAssertFalse(VoiceFormat.g711u.isWideband)
        XCTAssertFalse(VoiceFormat.g711a.isWideband)
        XCTAssertFalse(VoiceFormat.slin.isWideband)
    }

    func testEveryCodecHasABitrateLabel() {
        XCTAssertEqual(VoiceFormat.g711u.bitrateLabel, "64 kbit/s")
        XCTAssertEqual(VoiceFormat.g711a.bitrateLabel, "64 kbit/s")
        XCTAssertEqual(VoiceFormat.slin.bitrateLabel, "128 kbit/s")
        XCTAssertEqual(VoiceFormat.slin16.bitrateLabel, "256 kbit/s")
    }
}
