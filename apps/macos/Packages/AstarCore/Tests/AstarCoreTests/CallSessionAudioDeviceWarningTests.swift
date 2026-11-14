// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// astar-hfpout: a speaker or mic whose stream stopped for good (the engine
/// already tried rebuilding it) is said in plain words, so a call is never
/// silently deaf.
final class CallSessionAudioDeviceWarningTests: XCTestCase {
    private var fake: FakeStation!
    private var session: CallSession!

    override func setUp() {
        super.setUp()
        fake = FakeStation()
        session = CallSession(station: fake)
    }

    func testNothingStoppedPublishesNoWarning() {
        session.poll()
        XCTAssertNil(session.audioDeviceWarning)
    }

    func testAStoppedSpeakerIsNamed() {
        fake.snapshotToReturn.speakerStopped = "B02PTT-6522"
        session.poll()
        XCTAssertEqual(session.audioDeviceWarning, "Speaker stopped: B02PTT-6522")
    }

    func testAStoppedMicIsNamed() {
        fake.snapshotToReturn.micStopped = "USB Audio Device"
        session.poll()
        XCTAssertEqual(session.audioDeviceWarning, "Microphone stopped: USB Audio Device")
    }

    func testOneHeadsetWithBothStoppedIsNamedOnce() {
        fake.snapshotToReturn.speakerStopped = "B02PTT-6522"
        fake.snapshotToReturn.micStopped = "B02PTT-6522"
        session.poll()
        XCTAssertEqual(
            session.audioDeviceWarning, "Speaker and microphone stopped: B02PTT-6522")
    }

    func testTwoDevicesStoppedAreBothNamed() {
        fake.snapshotToReturn.speakerStopped = "B02PTT-6522"
        fake.snapshotToReturn.micStopped = "USB Audio Device"
        session.poll()
        XCTAssertEqual(
            session.audioDeviceWarning,
            "Speaker stopped: B02PTT-6522\nMicrophone stopped: USB Audio Device")
    }

    func testRecoveryClearsTheWarning() {
        fake.snapshotToReturn.speakerStopped = "B02PTT-6522"
        session.poll()
        fake.snapshotToReturn.speakerStopped = nil
        session.poll()
        XCTAssertNil(session.audioDeviceWarning)
    }

    func testCallSnapshotInitCarriesBothNames() {
        var s = CallSnapshot.idle
        XCTAssertNil(s.speakerStopped)
        XCTAssertNil(s.micStopped)
        s = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            speakerStopped: "B02PTT-6522", micStopped: "USB Audio Device")
        XCTAssertEqual(s.speakerStopped, "B02PTT-6522")
        XCTAssertEqual(s.micStopped, "USB Audio Device")
    }
}
