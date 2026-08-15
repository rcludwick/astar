// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// A combined USB device (Jabra Link 390, AllScan UCI150) exposes its mic and
/// speaker as two CoreAudio devices that share the exact same name. When the user
/// picks such an input, the matching output should auto-pair so they don't have to
/// hunt for it (the speaker otherwise stays on the system default).
final class AudioDevicePairingTests: XCTestCase {
    func testPairsOutputWithSameNamedInput() {
        let outputs = ["Mac mini Speakers", "Jabra Link 390", "KT USB Audio"]
        XCTAssertEqual(
            AudioDevicePairing.matchingOutput(forInput: "Jabra Link 390", in: outputs),
            "Jabra Link 390"
        )
    }

    func testNoMatchWhenNoSameNamedOutput() {
        let outputs = ["Mac mini Speakers", "KT USB Audio"]
        XCTAssertNil(AudioDevicePairing.matchingOutput(forInput: "Jabra Link 390", in: outputs))
    }

    func testSystemDefaultInputDoesNotPair() {
        // nil input = system default; nothing to pair against.
        XCTAssertNil(AudioDevicePairing.matchingOutput(forInput: nil, in: ["Jabra Link 390"]))
    }
}
