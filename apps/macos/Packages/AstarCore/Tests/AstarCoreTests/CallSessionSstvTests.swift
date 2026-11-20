// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// An in-memory SSTV-settings store, so tests never touch real defaults.
final class MemorySstvSettingsStore: SstvSettingsStore {
    var settings = SstvSettings()
    private(set) var saves = 0
    func load() -> SstvSettings { settings }
    func save(_ settings: SstvSettings) {
        saves += 1
        self.settings = settings
    }
}

/// `CallSession`'s SSTV side: configuring the decoder, reading its state at
/// the poll cadence, turning it into the panel's picture, and telling the
/// engine what the station is connected to. Everything lands on
/// `FakeStation`; nothing decodes, writes a file or leaves the process.
final class CallSessionSstvTests: XCTestCase {
    private var clock = Date(timeIntervalSince1970: 1_800_000_000)
    /// The SSTV clocks the session has running, fired by hand.
    private var sstvClocks: [UUID: () -> Void] = [:]

    /// Step the clock half a second and fire every running SSTV clock.
    private func fireSstvClock() {
        clock = clock.addingTimeInterval(0.5)
        for body in Array(sstvClocks.values) { body() }
    }

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.callsstv.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private func snap(_ status: IaxStatus, m17: Bool = false) -> CallSnapshot {
        CallSnapshot(
            status: status, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            m17Available: true, m17Active: m17)
    }

    private func session(
        settings: SstvSettings = SstvSettings()
    ) -> (CallSession, FakeStation, MemorySstvSettingsStore) {
        let fake = FakeStation()
        let store = MemorySstvSettingsStore()
        store.settings = settings
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: MemoryAudioStore(),
            directoryStore: MemoryNodeDirectoryStore(), userDefaults: scratchDefaults(),
            recordingStore: MemoryRecordingSettingsStore(), sstvStore: store,
            now: { [unowned self] in self.clock })
        session.runSstvWork = { $0() }
        session.runRecordingWork = { $0() }
        session.scheduleSstvClock = { [unowned self] _, body in
            let id = UUID()
            self.sstvClocks[id] = body
            return { [unowned self] in self.sstvClocks[id] = nil }
        }
        session.m17Callsign = "KX0AAA"
        session.reflectorIndex = ReflectorIndex(entries: [
            DirectoryEntry(
                network: .m17, id: "M17-XYZ", name: "M17-XYZ",
                dial: .m17(host: "127.0.0.1", port: 17_000, callsign: "M17-XYZ", modules: []))
        ])
        return (session, fake, store)
    }

    /// Dial AllStar 2000 and let the poll see it answer.
    private func answered(
        settings: SstvSettings = SstvSettings()
    ) throws -> (CallSession, FakeStation, MemorySstvSettingsStore) {
        let (session, fake, store) = session(settings: settings)
        try session.connect(node: "2000", network: .allstar)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        fake.snapshotToReturn = snap(.answered)
        session.poll()
        return (session, fake, store)
    }

    /// Step the clock past the read throttle and poll.
    private func tick(_ session: CallSession) {
        clock = clock.addingTimeInterval(0.5)
        session.poll()
    }

    private func airing(seq: UInt64, lines: Int, elapsed: Double = 10) throws -> SstvState {
        try SstvState.make(
            receiving: true, seq: seq, mode: "PD120", width: 640, height: 496, lines: lines,
            expectedSecs: 126, elapsedSecs: elapsed, startedMS: 1_800_000_000_000,
            talker: "N0CALL")
    }

    private func ended(seq: UInt64, lost: Bool = false) throws -> SstvState {
        let row: [String: Any] = [
            "seq": seq, "mode": "PD120", "network": "AllStar", "target": "2000",
            "talker": "N0CALL", "time_ms": 1_800_000_000_000, "partial": false, "lost": lost,
            "file": lost ? NSNull() : "/tmp/astar SSTV/pic \(seq).png" as Any,
        ]
        return try SstvState.make(seq: seq, last: row)
    }

    private func frame(seq: UInt64) -> SstvFrame {
        SstvFrame(
            seq: seq, width: 4, height: 2, lines: 2, rgba: Data(repeating: 0x80, count: 4 * 2 * 4))
    }

    // MARK: - Configure

    func testConfigureCalledAtLaunchAndOnChange() throws {
        let fake = FakeStation()
        let store = MemorySstvSettingsStore()
        let session = CallSession(
            station: fake, audioStore: MemoryAudioStore(),
            directoryStore: MemoryNodeDirectoryStore(), userDefaults: scratchDefaults(),
            recordingStore: MemoryRecordingSettingsStore(), sstvStore: store)
        // The launch configure runs on the session's own queue.
        let deadline = Date().addingTimeInterval(5)
        while fake.sstvConfigureCalls.isEmpty, Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.01))
        }
        let offset = Int32(TimeZone.current.secondsFromGMT())
        XCTAssertEqual(
            fake.sstvConfigureCalls,
            [
                .init(
                    enabled: true, directory: SstvSettings.defaultFolder.path,
                    utcOffsetSeconds: offset)
            ])

        session.runSstvWork = { $0() }
        session.sstvSettings.enabled = false
        session.sstvSettings.folder = URL(fileURLWithPath: "/tmp/elsewhere", isDirectory: true)
        session.sstvSettings = session.sstvSettings  // unchanged: nothing sent, nothing saved

        XCTAssertEqual(
            fake.sstvConfigureCalls.dropFirst().map(\.enabled), [false, false])
        XCTAssertEqual(fake.sstvConfigureCalls.last?.directory, "/tmp/elsewhere")
        XCTAssertEqual(store.saves, 2)
        XCTAssertEqual(store.settings.folder.path, "/tmp/elsewhere")
    }

    func testAConfigureFailureIsShown() throws {
        let (session, fake, _) = session()
        fake.sstvConfigureError = StationError(
            code: -9, text: "recording error", detail: "folder is read-only")
        session.sstvSettings.folder = URL(fileURLWithPath: "/nope", isDirectory: true)
        XCTAssertNotNil(session.sstvError)

        fake.sstvConfigureError = nil
        session.sstvSettings.folder = URL(fileURLWithPath: "/tmp/ok", isDirectory: true)
        XCTAssertNil(session.sstvError)
    }

    // MARK: - Polling

    func testFrameFetchedOnlyWhenTheFinishedPictureChanges() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        XCTAssertEqual(session.sstvLive?.seq, 1)
        XCTAssertEqual(session.sstvLive?.lines, 10)
        XCTAssertEqual(session.sstvLive?.finished, false)
        XCTAssertEqual(session.sstvLive?.expectedSecs, 126)
        XCTAssertTrue(session.sstvPanelVisible)

        fake.sstvStateValue = try airing(seq: 1, lines: 20)
        tick(session)
        XCTAssertEqual(session.sstvLive?.lines, 20)
        XCTAssertEqual(fake.sstvFrameReads, 0, "no pixels exist while a picture airs")

        let reads = fake.sstvStateReads
        session.poll()  // inside the half second: no read
        XCTAssertEqual(fake.sstvStateReads, reads)

        fake.sstvStateValue = try ended(seq: 1)
        fake.sstvFrameValue = frame(seq: 1)
        tick(session)
        XCTAssertEqual(fake.sstvFrameReads, 1)
        let live = try XCTUnwrap(session.sstvLive)
        XCTAssertTrue(live.finished)
        XCTAssertFalse(live.lost)
        XCTAssertNotNil(live.image)
        XCTAssertEqual(live.file?.path, "/tmp/astar SSTV/pic 1.png")

        tick(session)
        tick(session)
        XCTAssertEqual(fake.sstvFrameReads, 1, "the same picture is fetched once")
    }

    func testFinishedPictureStaysUntilDismissed() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        fake.sstvStateValue = try ended(seq: 1)
        fake.sstvFrameValue = frame(seq: 1)
        tick(session)
        tick(session)
        tick(session)
        XCTAssertTrue(session.sstvPanelVisible)
        XCTAssertEqual(session.sstvLive?.finished, true)

        session.dismissSstvPanel()
        XCTAssertFalse(session.sstvPanelVisible)
        tick(session)
        XCTAssertFalse(session.sstvPanelVisible, "the same picture does not come back")
    }

    func testNextPictureReopensAfterDismiss() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        session.dismissSstvPanel()
        XCTAssertFalse(session.sstvPanelVisible)

        fake.sstvStateValue = try airing(seq: 2, lines: 3)
        tick(session)
        XCTAssertTrue(session.sstvPanelVisible)
        XCTAssertEqual(session.sstvLive?.seq, 2)
    }

    func testLostPictureShowsLost() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        fake.sstvStateValue = try ended(seq: 1, lost: true)
        tick(session)
        let live = try XCTUnwrap(session.sstvLive)
        XCTAssertTrue(live.finished)
        XCTAssertTrue(live.lost)
        XCTAssertNil(live.file)
        XCTAssertNil(live.image)
        XCTAssertEqual(fake.sstvFrameReads, 0, "a lost picture has no pixels to fetch")
        XCTAssertTrue(session.sstvPanelVisible)
    }

    func testAPictureFromBeforeIsNotShown() throws {
        let (session, fake, _) = session()
        fake.sstvStateValue = try ended(seq: 4)
        fake.sstvFrameValue = frame(seq: 4)
        try session.connect(node: "2000", network: .allstar)
        fake.snapshotToReturn = snap(.answered)
        session.poll()
        tick(session)
        XCTAssertNil(session.sstvLive)
        XCTAssertEqual(fake.sstvFrameReads, 0)
    }

    func testNoPollingWithoutACall() throws {
        let (session, fake, _) = session()
        session.poll()
        tick(session)
        XCTAssertEqual(fake.sstvStateReads, 0)
    }

    func testOffStopsPolling() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        XCTAssertGreaterThan(fake.sstvStateReads, 0)
        XCTAssertNotNil(session.sstvLive)

        session.sstvSettings.enabled = false
        let reads = fake.sstvStateReads
        tick(session)
        tick(session)
        XCTAssertEqual(fake.sstvStateReads, reads)
        XCTAssertNil(session.sstvLive, "the panel goes with the decoder")
    }

    // MARK: - Context

    func testContextPushedOnAnswerAndTalkerChange() throws {
        let (session, fake, _) = session()
        try session.connect(node: "M17-XYZ C", network: .m17)
        fake.snapshotToReturn = snap(.answered, m17: true)
        fake.m17StateValue = M17State(link: .linked, talker: "N0CALL")
        session.poll()
        let m17 = Network.m17.displayName
        XCTAssertEqual(
            fake.sstvContextCalls.last, .init(network: m17, target: "M17-XYZ C", talker: "N0CALL"))
        let pushed = fake.sstvContextCalls.count

        session.poll()
        tick(session)
        XCTAssertEqual(fake.sstvContextCalls.count, pushed, "nothing changed, nothing sent")

        fake.m17StateValue = M17State(link: .linked, talker: "KX0AAA")
        session.poll()
        XCTAssertEqual(
            fake.sstvContextCalls.last, .init(network: m17, target: "M17-XYZ C", talker: "KX0AAA"))

        try session.disconnect()
        XCTAssertEqual(fake.sstvContextCalls.last, .init(network: "", target: "", talker: nil))
    }

    // MARK: - Engine swap

    func testReconfigureStartsSstvOnTheNewEngine() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 7, lines: 10)
        tick(session)
        fake.sstvStateValue = try ended(seq: 7)
        fake.sstvFrameValue = frame(seq: 7)
        tick(session)
        session.dismissSstvPanel()
        XCTAssertEqual(session.sstvLive?.seq, 7)

        let fresh = FakeStation()
        fresh.snapshotToReturn = snap(.answered)
        session.reconfigure(station: fresh, hasCredentials: true)
        XCTAssertEqual(fresh.sstvConfigureCalls.map(\.enabled), [true], "the new engine decodes")
        XCTAssertNil(session.sstvLive, "the old engine's picture goes with it")

        // The new engine numbers from scratch: its picture 1 is news, and the
        // dismissal of the old engine's picture 7 has nothing to do with it.
        fresh.sstvStateValue = try airing(seq: 1, lines: 4)
        tick(session)
        XCTAssertEqual(session.sstvLive?.seq, 1)
        XCTAssertTrue(session.sstvPanelVisible)
        XCTAssertEqual(
            fresh.sstvContextCalls.last,
            .init(network: Network.allstar.displayName, target: "2000", talker: nil),
            "the new engine is told the connection")
    }

    // MARK: - The SSTV clock

    func testStateReadsContinueWithTheUIPollStopped() throws {
        let (session, fake, _) = try answered()
        XCTAssertFalse(sstvClocks.isEmpty, "a call is up: the clock runs")
        let reads = fake.sstvStateReads
        fireSstvClock()
        fireSstvClock()
        XCTAssertEqual(fake.sstvStateReads, reads + 2, "no UI poll, still read")

        // The UI poll inside the same half second reads nothing more.
        session.poll()
        XCTAssertEqual(fake.sstvStateReads, reads + 2)
    }

    func testTheClockStopsWithNoCallAndNothingAiring() throws {
        let (session, fake, _) = try answered()
        XCTAssertFalse(sstvClocks.isEmpty)
        // The call drops while the popover is on another pane: the clock
        // sees it in the engine's snapshot.
        fake.snapshotToReturn = snap(.hangup)
        fireSstvClock()
        XCTAssertTrue(sstvClocks.isEmpty, "no call, nothing airing: no clock")
        let reads = fake.sstvStateReads
        fireSstvClock()
        tick(session)
        XCTAssertEqual(fake.sstvStateReads, reads)
    }

    func testTheClockStopsWhenDecodingGoesOff() throws {
        let (session, _, _) = try answered()
        XCTAssertFalse(sstvClocks.isEmpty)
        session.sstvSettings.enabled = false
        XCTAssertTrue(sstvClocks.isEmpty)
        session.sstvSettings.enabled = true
        XCTAssertFalse(sstvClocks.isEmpty, "back on mid-call: the clock runs again")
    }

    func testAPictureStillAiringAfterTheCallKeepsTheClock() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        fake.snapshotToReturn = snap(.hangup)
        fireSstvClock()
        XCTAssertFalse(sstvClocks.isEmpty, "the picture is still airing")
        XCTAssertEqual(session.sstvLive?.finished, false)
    }

    func testAWedgedPictureEndsAsLostAndReadsStop() throws {
        let (session, fake, _) = try answered()
        // Started at the clock's start; 126 s long, so overdue after 194 s.
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertEqual(session.sstvLive?.finished, false)

        clock = clock.addingTimeInterval(195)
        fireSstvClock()
        let live = try XCTUnwrap(session.sstvLive)
        XCTAssertTrue(live.finished)
        XCTAssertTrue(live.lost)
        XCTAssertTrue(sstvClocks.isEmpty, "a wedged picture does not keep the clock")
        let reads = fake.sstvStateReads
        tick(session)
        fireSstvClock()
        XCTAssertEqual(fake.sstvStateReads, reads)
    }

    // MARK: - Retries

    func testAFailedContextPushIsRetried() throws {
        let (session, fake, _) = session()
        fake.sstvContextError = StationError(code: -1, text: "busy")
        try session.connect(node: "2000", network: .allstar)
        fake.snapshotToReturn = snap(.answered)
        session.poll()
        let tries = fake.sstvContextCalls.count
        XCTAssertGreaterThan(tries, 0)

        fake.sstvContextError = nil
        session.poll()
        XCTAssertEqual(fake.sstvContextCalls.count, tries + 1, "tried again")
        session.poll()
        XCTAssertEqual(fake.sstvContextCalls.count, tries + 1, "taken: not sent again")
    }

    func testAFailedFrameReadIsRetried() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        fake.sstvStateValue = try ended(seq: 1)
        fake.sstvFrameValue = frame(seq: 1)
        fake.sstvFrameError = StationError(code: -1, text: "busy")
        tick(session)
        XCTAssertEqual(fake.sstvFrameReads, 1)
        XCTAssertNil(session.sstvLive?.image)
        XCTAssertEqual(session.sstvLive?.finished, true)

        fake.sstvFrameError = nil
        tick(session)
        XCTAssertEqual(fake.sstvFrameReads, 2, "the thrown read is tried again")
        XCTAssertNotNil(session.sstvLive?.image)
        tick(session)
        XCTAssertEqual(fake.sstvFrameReads, 2)
    }

    func testAFrameThatKeepsFailingIsGivenUp() throws {
        let (session, fake, _) = try answered()
        // Seen airing first, so its end is news.
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        fake.sstvStateValue = try ended(seq: 1)
        fake.sstvFrameError = StationError(code: -1, text: "broken")
        for _ in 0..<6 { tick(session) }
        XCTAssertEqual(fake.sstvFrameReads, CallSession.sstvFrameAttempts)
        XCTAssertEqual(session.sstvLive?.finished, true)
    }

    func testAFrameForAnotherPictureIsNotRetried() throws {
        let (session, fake, _) = try answered()
        fake.sstvStateValue = try airing(seq: 1, lines: 10)
        tick(session)
        fake.sstvStateValue = try ended(seq: 1)
        fake.sstvFrameValue = frame(seq: 2)
        tick(session)
        tick(session)
        XCTAssertEqual(fake.sstvFrameReads, 1)
        XCTAssertNil(session.sstvLive?.image)
    }

    // MARK: - Shape and spoken label

    /// The airing frame takes the engine's own width: PD160 (512×400) and
    /// PD290 (800×616) are not the 320- or 640-wide shapes a height guesses.
    func testAnAiringPictureTakesTheEnginesWidth() throws {
        let (session, fake, _) = try answered()
        for (seq, mode, width, height) in [
            (UInt64(1), "PD160", 512, 400), (2, "PD290", 800, 616), (3, "Robot 36", 320, 240),
        ] {
            fake.sstvStateValue = try SstvState.make(
                receiving: true, seq: seq, mode: mode, width: width, height: height, lines: 5,
                expectedSecs: 100, elapsedSecs: 5, startedMS: 1_800_000_000_000)
            tick(session)
            let live = try XCTUnwrap(session.sstvLive)
            XCTAssertEqual(live.width, width, mode)
            XCTAssertEqual(live.aspectRatio, Double(width) / Double(height), accuracy: 1e-9, mode)
        }
    }

    func testAspectRatioPrefersThePixelsThenTheSizeThen5by4() {
        var live = SstvLive(
            seq: 1, mode: "PD160", talker: nil, startedAt: Date(), lines: 0, height: 400,
            width: 512)
        XCTAssertEqual(live.aspectRatio, 512.0 / 400.0, accuracy: 1e-9)
        live.width = 0
        live.height = 0
        XCTAssertEqual(live.aspectRatio, 5.0 / 4.0, accuracy: 1e-9)
        live.image = SstvLive.image(
            from: SstvFrame(
                seq: 1, width: 4, height: 2, lines: 2, rgba: Data(count: 4 * 2 * 4)))
        XCTAssertEqual(live.aspectRatio, 2, accuracy: 1e-9)
    }

    func testThePictureLabelLeavesOutAMissingOrEmptyTalker() {
        for talker in [nil, "", "  "] as [String?] {
            let label = SstvLive.pictureLabel(
                mode: "PD120", talker: talker, finished: true, lost: false, partial: false)
            XCTAssertEqual(label, "PD120 picture, saved")
            XCTAssertFalse(label.contains("from"), label)
        }
        XCTAssertEqual(
            SstvLive.pictureLabel(
                mode: "PD120", talker: "N0CALL", finished: true, lost: false, partial: true),
            "PD120 picture from N0CALL, partly received and saved")
        XCTAssertEqual(
            SstvLive.pictureLabel(
                mode: "Robot 36", talker: "N0CALL", finished: false, lost: false, partial: false),
            "Receiving Robot 36 picture from N0CALL")
        XCTAssertEqual(
            SstvLive.pictureLabel(
                mode: "PD120", talker: nil, finished: true, lost: true, partial: false),
            "PD120 picture, lost")
    }
}
