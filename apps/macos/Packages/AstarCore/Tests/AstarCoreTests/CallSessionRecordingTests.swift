// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Combine
import XCTest

@testable import AstarCore

/// An in-memory recording-settings store, so tests never touch real defaults.
final class MemoryRecordingSettingsStore: RecordingSettingsStore {
    var settings = RecordingSettings()
    private(set) var saves = 0
    func load() -> RecordingSettings { settings }
    func save(_ settings: RecordingSettings) {
        saves += 1
        self.settings = settings
    }
}

/// `CallSession`'s recording rules: when a recording starts and stops, what it
/// is called, and how often the session asks the engine about it.
///
/// Every dial and every recording lands on `FakeStation`; nothing opens a
/// device, writes a file or leaves the process.
final class CallSessionRecordingTests: XCTestCase {
    private var clock = Date(timeIntervalSince1970: 1_800_000_000)
    private var cancellables = Set<AnyCancellable>()

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.callrecording.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private func snap(_ status: IaxStatus, m17: Bool = false) -> CallSnapshot {
        CallSnapshot(
            status: status, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            m17Available: true, m17Active: m17)
    }

    /// A session with the recorder's work run inline, so a test sees each
    /// start and stop the moment it is asked for.
    private func session(
        settings: RecordingSettings = RecordingSettings()
    ) -> (CallSession, FakeStation, MemoryRecordingSettingsStore) {
        let fake = FakeStation()
        let store = MemoryRecordingSettingsStore()
        store.settings = settings
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: MemoryAudioStore(),
            directoryStore: MemoryNodeDirectoryStore(), userDefaults: scratchDefaults(),
            recordingStore: store, now: { [unowned self] in self.clock })
        session.runRecordingWork = { $0() }
        session.m17Callsign = "AJ7HR"
        session.reflectorIndex = ReflectorIndex(entries: [
            DirectoryEntry(
                network: .m17, id: "M17-XYZ", name: "M17-XYZ",
                dial: .m17(host: "127.0.0.1", port: 17_000, callsign: "M17-XYZ", modules: []))
        ])
        return (session, fake, store)
    }

    /// Dial AllStar 2000 and let the poll see it answer.
    private func answeredAllStar2000(
        settings: RecordingSettings = RecordingSettings()
    ) throws -> (CallSession, FakeStation, MemoryRecordingSettingsStore) {
        let (session, fake, store) = session(settings: settings)
        try session.connect(node: "2000", network: .allstar)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        fake.recordStateValue = try RecordState.make(recording: true)
        fake.snapshotToReturn = snap(.answered)
        session.poll()
        return (session, fake, store)
    }

    private var offset: Int32 { Int32(TimeZone.current.secondsFromGMT()) }

    // MARK: - Starting

    func testToggleStartsWithTheSettingsAndLabel() throws {
        let (session, fake, store) = try answeredAllStar2000()
        session.toggleRecording()

        XCTAssertTrue(session.recording)
        XCTAssertEqual(
            fake.recordStartCalls,
            [
                .init(
                    directory: store.settings.folder.path, layout: .stereo, frames: false,
                    label: "AllStar 2000", utcOffsetSeconds: offset)
            ])

        session.toggleRecording()
        XCTAssertFalse(session.recording)
        XCTAssertEqual(fake.recordStopCount, 1)
    }

    func testToggleCarriesTheChosenLayoutFolderAndFrames() throws {
        let chosen = RecordingSettings(
            layout: .mixed, alwaysRecord: false, saveCodecFrames: true,
            folder: URL(fileURLWithPath: "/tmp/astar test", isDirectory: true))
        let (session, fake, _) = try answeredAllStar2000(settings: chosen)
        session.toggleRecording()
        let call = try XCTUnwrap(fake.recordStartCalls.first)
        XCTAssertEqual(call.directory, "/tmp/astar test")
        XCTAssertEqual(call.layout, .mixed)
        XCTAssertTrue(call.frames)
    }

    func testToggleWhileNotAnsweredDoesNothing() throws {
        let (session, fake, _) = session()
        session.toggleRecording()  // idle
        try session.connect(node: "2000", network: .allstar)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        session.toggleRecording()  // still dialing

        XCTAssertFalse(session.recording)
        XCTAssertTrue(fake.recordStartCalls.isEmpty)
        XCTAssertEqual(fake.recordStopCount, 0)
    }

    func testAlwaysRecordStartsOnAnswered() throws {
        var settings = RecordingSettings()
        settings.alwaysRecord = true
        let (session, fake, _) = try answeredAllStar2000(settings: settings)

        XCTAssertTrue(session.recording)
        XCTAssertEqual(fake.recordStartCalls.map(\.label), ["AllStar 2000"])
        session.poll()  // still answered: not a second edge
        XCTAssertEqual(fake.recordStartCalls.count, 1)
    }

    func testAlwaysRecordOffStartsNothingOnAnswered() throws {
        let (session, fake, _) = try answeredAllStar2000()
        XCTAssertFalse(session.recording)
        XCTAssertTrue(fake.recordStartCalls.isEmpty)
    }

    func testAFailedStartPublishesTheEngineDetail() throws {
        let (session, fake, _) = try answeredAllStar2000()
        fake.recordStartError = StationError(
            code: -23, text: "recording error",
            detail: "disk has only 120 MB free; recording needs 500 MB")
        session.toggleRecording()

        XCTAssertFalse(session.recording)
        XCTAssertEqual(session.recordingError, "disk has only 120 MB free; recording needs 500 MB")
    }

    /// Start, Stop, Start clicked faster than the recorder answers: the first
    /// start's late refusal must not switch off the second, which worked.
    func testALateRefusalDoesNotUndoANewerStart() throws {
        let (session, fake, _) = try answeredAllStar2000()
        var queued: [() -> Void] = []
        session.runRecordingWork = { queued.append($0) }
        session.toggleRecording()
        session.toggleRecording()
        session.toggleRecording()
        XCTAssertEqual(queued.count, 3)

        fake.recordStartError = StationError(code: -23, text: "recording error")
        queued.removeFirst()()
        fake.recordStartError = nil
        for work in queued { work() }

        XCTAssertTrue(session.recording)
        XCTAssertEqual(fake.recordStartCalls.count, 1)
        XCTAssertEqual(fake.recordStopCount, 1)
    }

    func testRecordStartAndStopRunOffTheMainThread() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.runRecordingWork = CallSession.backgroundRecordingWork()
        let started = expectation(description: "recordStart ran")
        fake.onRecordStart = { started.fulfill() }
        session.toggleRecording()
        wait(for: [started], timeout: 5)
        XCTAssertEqual(fake.recordStartOnMainThread, [false])

        // The start's completion hops back to the main queue; let it land.
        let settled = expectation(description: "main queue drained")
        DispatchQueue.main.async { settled.fulfill() }
        wait(for: [settled], timeout: 5)

        let stopped = expectation(description: "recordStop ran")
        fake.onRecordStop = { stopped.fulfill() }
        session.toggleRecording()
        wait(for: [stopped], timeout: 5)
        XCTAssertEqual(fake.recordStopOnMainThread, [false])
    }

    // MARK: - Stopping

    func testRemoteHangupStopsRecording() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        fake.snapshotToReturn = snap(.hangup)
        session.poll()

        XCTAssertEqual(fake.recordStopCount, 1)
        XCTAssertFalse(session.recording)
        session.poll()
        XCTAssertEqual(fake.recordStopCount, 1, "one stop per edge")
    }

    func testDisconnectStopsRecording() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        try session.disconnect()

        XCTAssertEqual(fake.recordStopCount, 1)
        XCTAssertFalse(session.recording)
        fake.snapshotToReturn = snap(.idle)
        session.poll()
        XCTAssertEqual(fake.recordStopCount, 1, "the edge that follows finds nothing to stop")
    }

    // MARK: - A network change (F10)

    func testNetworkChangeOpensANewFile() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        XCTAssertEqual(fake.recordStartCalls.count, 1)

        try session.connect(node: "M17-XYZ C", network: .m17)
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertEqual(fake.recordStopCount, 1, "the AllStar file is finished")
        XCTAssertFalse(session.recording)

        fake.snapshotToReturn = snap(.answered, m17: true)
        session.poll()
        XCTAssertEqual(fake.recordStopCount, 1)
        XCTAssertEqual(
            fake.recordStartCalls.map(\.label),
            ["AllStar 2000", "\(Network.m17.displayName) M17-XYZ C"])
        XCTAssertTrue(session.recording)

        session.poll()
        XCTAssertEqual(fake.recordStartCalls.count, 2, "the resume is used once")
    }

    func testSameNetworkRedialJustStops() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()

        try session.connect(node: "2001", network: .allstar)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        fake.snapshotToReturn = snap(.answered)
        session.poll()

        XCTAssertEqual(fake.recordStopCount, 1)
        XCTAssertEqual(fake.recordStartCalls.count, 1, "no second file without Always record")
        XCTAssertFalse(session.recording)
    }

    func testDisconnectDoesNotResumeRecording() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()

        try session.connect(node: "M17-XYZ C", network: .m17)
        try session.disconnect()
        fake.snapshotToReturn = snap(.answered, m17: true)
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.count, 1)
        XCTAssertFalse(session.recording)
    }

    func testAFailedDialDoesNotResumeRecording() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()

        fake.m17ConnectError = StationError(code: -1, text: "boom", detail: "")
        XCTAssertThrowsError(try session.connect(node: "M17-XYZ C", network: .m17))
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        fake.snapshotToReturn = snap(.answered)
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.count, 1)
    }

    /// A resume armed by a network change dies with a dial that never
    /// answers; the next, unrelated call records nothing by itself.
    func testANoAnswerDropsThePendingResume() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        try session.connect(node: "M17-XYZ C", network: .m17)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        fake.snapshotToReturn = snap(.hangup)
        session.poll()

        try session.connect(node: "2001", network: .allstar)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        fake.snapshotToReturn = snap(.answered)
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.count, 1)
        XCTAssertFalse(session.recording)
    }

    /// The `.dialing → .hangup` edge on its own clears the resume.
    func testTheNoAnswerEdgeAloneClearsTheResume() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        try session.connect(node: "M17-XYZ C", network: .m17)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        fake.snapshotToReturn = snap(.idle)
        session.poll()
        fake.snapshotToReturn = snap(.answered, m17: true)
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.count, 1)
    }

    /// Stop pressed while the dial is in flight — in the exact gap between
    /// the engine call and its completion — means no resume.
    func testStopDuringTheDialCancelsTheResume() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        session.testPostEngineCallHook = { [unowned session] in session.toggleRecording() }
        try session.connect(node: "M17-XYZ C", network: .m17)
        session.testPostEngineCallHook = nil
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        fake.snapshotToReturn = snap(.answered, m17: true)
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.count, 1)
        XCTAssertEqual(fake.recordStopCount, 1)
        XCTAssertFalse(session.recording)
    }

    /// A newer dial attempt kills a pending resume at once — even when its
    /// call answers before that dial's own completion runs, and the poll
    /// never saw the first dial give up.
    func testANewerDialAttemptKillsThePendingResume() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        try session.connect(node: "M17-XYZ C", network: .m17)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()

        session.testPostEngineCallHook = { [unowned session] in
            fake.snapshotToReturn = self.snap(.answered)
            session.poll()
        }
        try session.connect(node: "2001", network: .allstar)
        session.testPostEngineCallHook = nil
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.map(\.label), ["AllStar 2000"])
        XCTAssertFalse(session.recording)
    }

    /// The same race with Always record on: the stale resume is dropped and
    /// the newer call is recorded under its own label, once.
    func testANewerDialAttemptWithAlwaysRecordRecordsTheNewCall() throws {
        var settings = RecordingSettings()
        settings.alwaysRecord = true
        let (session, fake, _) = try answeredAllStar2000(settings: settings)
        XCTAssertTrue(session.recording)
        try session.connect(node: "M17-XYZ C", network: .m17)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()

        session.testPostEngineCallHook = { [unowned session] in
            fake.snapshotToReturn = self.snap(.answered)
            session.poll()
        }
        try session.connect(node: "2001", network: .allstar)
        session.testPostEngineCallHook = nil
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.map(\.label), ["AllStar 2000", "AllStar 2001"])
        XCTAssertTrue(session.recording)
    }

    func testTheReadErrorClearsWhenReadsRecover() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        fake.recordStateError = StationError(code: -1, text: "undecodable record state json")
        for _ in 0..<3 {
            session.poll()
            clock += 0.6
        }
        XCTAssertNotNil(session.recordingError)
        fake.recordStateError = nil
        session.poll()
        XCTAssertNil(session.recordingError)
        XCTAssertTrue(session.recording)
    }

    /// Record pressed with nothing running cancels a pending resume, even
    /// before the new call answers.
    func testToggleWithAPendingResumeCancelsIt() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        try session.connect(node: "M17-XYZ C", network: .m17)
        fake.snapshotToReturn = snap(.dialing)
        session.poll()
        session.toggleRecording()
        fake.snapshotToReturn = snap(.answered, m17: true)
        session.poll()

        XCTAssertEqual(fake.recordStartCalls.count, 1)
        XCTAssertFalse(session.recording)
    }

    /// Always record starts a file on an answer the poll sees before the
    /// dial's own completion runs; that completion must not stop it.
    func testAlwaysRecordStartedDuringTheDialSurvivesIt() throws {
        var settings = RecordingSettings()
        settings.alwaysRecord = true
        let (session, fake, _) = session(settings: settings)
        fake.recordStateValue = try RecordState.make(recording: true)
        session.testPostEngineCallHook = { [unowned session] in
            fake.snapshotToReturn = self.snap(.answered)
            session.poll()
        }
        try session.connect(node: "2000", network: .allstar)
        session.testPostEngineCallHook = nil

        XCTAssertEqual(fake.recordStartCalls.map(\.label), ["AllStar 2000"])
        XCTAssertEqual(fake.recordStopCount, 0)
        XCTAssertTrue(session.recording)
    }

    // MARK: - Reading the engine's state (F25)

    func testRepeatedRecordStateFailuresPublishAnError() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        fake.recordStateError = StationError(code: -1, text: "undecodable record state json")
        for _ in 0..<2 {
            session.poll()
            clock += 0.6
        }
        XCTAssertNil(session.recordingError, "one or two misses are a hiccup")
        session.poll()
        XCTAssertEqual(
            session.recordingError,
            "Can’t read the recording’s state: undecodable record state json")
    }

    func testElapsedPublishesOnlyOnChange() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        var published: [Int] = []
        session.$recordingElapsedSeconds.dropFirst().sink { published.append($0) }
            .store(in: &cancellables)
        var recordingPublishes = 0
        session.$recording.dropFirst().sink { _ in recordingPublishes += 1 }
            .store(in: &cancellables)
        fake.recordStateValue = try RecordState.make(recording: true, elapsedMS: 1200)
        let readsBefore = fake.recordStateReads

        session.poll()
        clock += 0.6
        session.poll()
        XCTAssertEqual(fake.recordStateReads - readsBefore, 2)
        XCTAssertEqual(published, [1])

        clock += 0.1
        session.poll()
        XCTAssertEqual(fake.recordStateReads - readsBefore, 2, "read at most every 0.5 s")
        XCTAssertEqual(recordingPublishes, 0)
    }

    func testIdleNeverReadsRecordState() throws {
        let (session, fake, _) = try answeredAllStar2000()
        for _ in 0..<3 {
            clock += 1
            session.poll()
        }
        XCTAssertEqual(fake.recordStateReads, 0)
    }

    func testAnEngineStopPublishesTheError() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        fake.recordStateValue = try RecordState.make(
            recording: false, elapsedMS: 61_000,
            error: "disk has only 120 MB free; recording needs 500 MB")
        session.poll()

        XCTAssertFalse(session.recording)
        XCTAssertEqual(session.recordingError, "disk has only 120 MB free; recording needs 500 MB")

        fake.recordStateValue = try RecordState.make(recording: true)
        session.toggleRecording()
        XCTAssertNil(session.recordingError, "cleared on the next start")
        XCTAssertTrue(session.recording)
    }

    // MARK: - The error caption belongs to one call

    /// A recording that stopped itself with an error, on a call that is now
    /// over: the next call must not open with that caption.
    private func callWithRecordingError() throws -> (CallSession, FakeStation) {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        fake.recordStateValue = try RecordState.make(
            recording: false, elapsedMS: 3_000, error: "the recordings folder is gone")
        session.poll()
        XCTAssertEqual(session.recordingError, "the recordings folder is gone")
        return (session, fake)
    }

    func testRemoteHangupClearsTheRecordingError() throws {
        let (session, fake) = try callWithRecordingError()
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertNil(session.recordingError)
    }

    func testDisconnectClearsTheRecordingError() throws {
        let (session, _) = try callWithRecordingError()
        try session.disconnect()
        XCTAssertNil(session.recordingError)
    }

    /// A start still queued when the operator hangs up, refused afterwards,
    /// speaks for a call that is over and must not raise the caption.
    func testALateRefusalAfterHangupRaisesNoCaption() throws {
        let (session, fake, _) = try answeredAllStar2000()
        var queued: [() -> Void] = []
        session.runRecordingWork = { queued.append($0) }
        session.toggleRecording()
        try session.disconnect()

        fake.recordStartError = StationError(code: -23, text: "recording error")
        for work in queued { work() }
        XCTAssertNil(session.recordingError)
    }

    // MARK: - Quitting

    func testQuitFinishesARunningRecording() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()

        XCTAssertTrue(session.finishRecordingBeforeQuit(timeout: 1))
        XCTAssertEqual(fake.recordStopCount, 1, "the FLAC is finished before the app exits")
        XCTAssertFalse(session.recording)
    }

    func testQuitWhileIdleTouchesNothing() throws {
        let (session, fake, _) = try answeredAllStar2000()
        XCTAssertTrue(session.finishRecordingBeforeQuit(timeout: 1))
        XCTAssertEqual(fake.recordStopCount, 0)
    }

    func testQuitWaitsForTheStopOffTheMainThread() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        session.runRecordingWork = CallSession.backgroundRecordingWork()

        XCTAssertTrue(session.finishRecordingBeforeQuit(timeout: 5))
        XCTAssertEqual(fake.recordStopCount, 1, "returned only after the stop finished")
        XCTAssertEqual(fake.recordStopOnMainThread, [false])
    }

    func testQuitWaitsBehindAStopAlreadyQueued() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.toggleRecording()
        var queued: [() -> Void] = []
        session.runRecordingWork = { queued.append($0) }
        session.toggleRecording()  // the operator's Stop, not yet run
        XCTAssertFalse(session.recording)

        let background = CallSession.backgroundRecordingWork()
        session.runRecordingWork = background
        background { for work in queued { work() } }
        XCTAssertTrue(session.finishRecordingBeforeQuit(timeout: 5))
        XCTAssertEqual(fake.recordStopCount, 2, "the queued Stop, then quit's own, both finished")
    }

    func testQuitGivesUpAfterTheTimeout() throws {
        let (session, _, _) = try answeredAllStar2000()
        session.toggleRecording()
        session.runRecordingWork = { _ in }  // a recorder that never answers

        let began = Date()
        XCTAssertFalse(session.finishRecordingBeforeQuit(timeout: 0.2))
        XCTAssertLessThan(Date().timeIntervalSince(began), 2, "a wedged stop cannot hold quit")
    }

    // MARK: - Settings

    func testSettingsSaveThroughTheStore() throws {
        let (session, _, store) = session()
        var changed = session.recordingSettings
        changed.alwaysRecord = true
        session.recordingSettings = changed
        XCTAssertEqual(store.settings, changed)
        XCTAssertEqual(store.saves, 1)
    }

    // MARK: - Packet log

    private func plSettings(
        debugMode: Bool = false, folder: String = "/tmp/pl-a", max: Int64 = 1_000_000_000
    ) -> RecordingSettings {
        var s = RecordingSettings()
        s.debugMode = debugMode
        s.packetLogFolder = URL(fileURLWithPath: folder, isDirectory: true)
        s.packetLogMaxBytes = max
        return s
    }

    /// Answered on AllStar 2000 with Debug mode on and the engine reporting a
    /// live capture, so a poll leaves the button alone.
    private func answeredWithDebugMode(
        folder: String = "/tmp/pl-a"
    ) throws -> (CallSession, FakeStation, MemoryRecordingSettingsStore) {
        let (session, fake, store) = try answeredAllStar2000(
            settings: plSettings(debugMode: true, folder: folder))
        fake.packetLogStateValue = plState()
        return (session, fake, store)
    }

    private func plState(
        capturing: Bool = true, usedBytes: UInt64 = 0, error: String? = nil
    ) -> PacketLogState {
        PacketLogState(
            capturing: capturing, openCalls: 0, usedBytes: usedBytes, dropped: 0, error: error)
    }

    /// Hold recording work until the test runs it, one job at a time.
    private final class HeldWork {
        private var jobs: [() -> Void] = []
        func hold(_ session: CallSession) { session.runRecordingWork = { self.jobs.append($0) } }
        var count: Int { jobs.count }
        func runNext() { jobs.removeFirst()() }
    }

    func testDebugModeAloneTouchesNothing() {
        let (session, fake, _) = session()
        session.recordingSettings = plSettings(debugMode: true)
        XCTAssertTrue(fake.packetLogCalls.isEmpty)
        XCTAssertFalse(session.packetCapturing)
    }

    func testLaunchWithDebugModeOnTouchesNothing() {
        let fake = FakeStation()
        let store = MemoryRecordingSettingsStore()
        store.settings = plSettings(debugMode: true)
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: MemoryAudioStore(),
            directoryStore: MemoryNodeDirectoryStore(), userDefaults: scratchDefaults(),
            recordingStore: store, now: { [unowned self] in self.clock })
        // Waits behind anything the launch queued on the real runner.
        XCTAssertTrue(session.finishPacketLogBeforeQuit(timeout: 5))
        XCTAssertTrue(fake.packetLogCalls.isEmpty)
    }

    func testPressStartsACaptureOnTheAnsweredCall() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        XCTAssertEqual(
            fake.packetLogCalls,
            [
                FakeStation.PacketLogCall(
                    enabled: true, directory: URL(fileURLWithPath: "/tmp/pl-a", isDirectory: true),
                    maxBytes: 1_000_000_000, utcOffsetSeconds: offset)
            ])
        XCTAssertTrue(session.packetCapturing)
    }

    func testPressAnswersBeforeTheStartHasRun() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        HeldWork().hold(session)
        session.togglePacketCapture()
        XCTAssertTrue(session.packetCapturing, "the button answers the click")
        XCTAssertTrue(fake.packetLogCalls.isEmpty)
    }

    func testPressDoesNothingWithDebugModeOff() throws {
        let (session, fake, _) = try answeredAllStar2000()
        session.togglePacketCapture()
        XCTAssertTrue(fake.packetLogCalls.isEmpty)
        XCTAssertFalse(session.packetCapturing)
    }

    func testPressDoesNothingWithNoAnsweredCall() {
        let (session, fake, _) = session(settings: plSettings(debugMode: true))
        session.togglePacketCapture()
        XCTAssertTrue(fake.packetLogCalls.isEmpty)
        XCTAssertFalse(session.packetCapturing)
    }

    func testPressDoesNothingWhileADialIsInFlight() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.testPostEngineCallHook = { session.togglePacketCapture() }
        try session.connect(node: "2001", network: .allstar)
        XCTAssertTrue(fake.packetLogCalls.isEmpty, "the answered call is the one being replaced")
        XCTAssertFalse(session.packetCapturing)
    }

    func testSecondPressStops() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        session.togglePacketCapture()
        XCTAssertEqual(fake.packetLogCalls.count, 2)
        XCTAssertEqual(fake.packetLogCalls.last?.enabled, false)
        XCTAssertNil(fake.packetLogCalls.last?.directory)
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 0)
    }

    func testStartRunsOffTheMainThread() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.runRecordingWork = CallSession.backgroundRecordingWork()
        let done = expectation(description: "started")
        fake.onPacketLogConfigure = { done.fulfill() }
        session.togglePacketCapture()
        wait(for: [done], timeout: 5)
        XCTAssertEqual(fake.packetLogOnMainThread, [false])
    }

    func testCallEndingEndsTheCapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 1)
        XCTAssertEqual(fake.packetLogCalls.map(\.enabled), [true], "ended as a hangup, not stopped")
    }

    func testDisconnectEndsTheCapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        try session.disconnect()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 1)
    }

    func testCallEndingAfterAQueuedStartLeavesNothingRunning() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        let held = HeldWork()
        held.hold(session)
        session.togglePacketCapture()
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(held.count, 2)

        held.runNext()
        XCTAssertEqual(fake.packetLogCalls.map(\.enabled), [true], "the start ran first")
        XCTAssertEqual(fake.packetLogCallEndedCount, 0)
        held.runNext()
        XCTAssertEqual(fake.packetLogCallEndedCount, 1, "and the end ran behind it")
        XCTAssertFalse(session.packetCapturing)
    }

    func testANewCallDoesNotResumeCapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        fake.snapshotToReturn = snap(.answered)
        session.poll()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCalls.count, 1, "no new start")
    }

    func testADialReplacingACallEndsTheCapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        try session.connect(node: "2001", network: .allstar)
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 1)
    }

    func testTurningDebugModeOffStopsACapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        session.recordingSettings.debugMode = false
        XCTAssertEqual(fake.packetLogCalls.last?.enabled, false)
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 0)
    }

    func testChangingTheFolderLeavesACaptureRunning() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        session.recordingSettings = plSettings(debugMode: true, folder: "/tmp/pl-b")
        session.recordingSettings = plSettings(
            debugMode: true, folder: "/tmp/pl-b", max: 250_000_000)
        XCTAssertEqual(fake.packetLogCalls.count, 1, "no new configure")
        XCTAssertTrue(session.packetCapturing)

        session.togglePacketCapture()
        session.togglePacketCapture()
        XCTAssertEqual(fake.packetLogCalls.last?.directory?.path, "/tmp/pl-b")
        XCTAssertEqual(fake.packetLogCalls.last?.maxBytes, 250_000_000)
    }

    func testAFailedStartResetsTheButtonAndSetsTheError() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        fake.packetLogConfigureError = StationError(
            code: -23, text: "recording error", detail: "folder is read-only")
        session.togglePacketCapture()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(session.packetLogError, "folder is read-only")

        fake.packetLogConfigureError = nil
        session.togglePacketCapture()
        XCTAssertTrue(session.packetCapturing)
        XCTAssertNil(session.packetLogError, "the next press clears it")
    }

    func testAStaleFailureDoesNotUndoALaterCapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        let held = HeldWork()
        held.hold(session)
        session.togglePacketCapture()  // refused, once it runs
        session.togglePacketCapture()
        session.togglePacketCapture()  // a fresh start, queued behind the stop

        fake.packetLogConfigureError = StationError(
            code: -23, text: "recording error", detail: "folder is read-only")
        held.runNext()
        fake.packetLogConfigureError = nil
        held.runNext()
        held.runNext()

        XCTAssertTrue(session.packetCapturing)
        XCTAssertNil(session.packetLogError)
    }

    func testAnErrorDuringACaptureStopsIt() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.packetLogStateValue = plState(error: "disk full")
        session.poll()
        XCTAssertEqual(session.packetLogError, "disk full")
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCalls.last?.enabled, false)
    }

    func testAnEngineThatStoppedResetsTheButton() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.packetLogStateValue = plState(capturing: false)
        session.poll()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertNil(session.packetLogError)
        XCTAssertEqual(fake.packetLogCalls.count, 1, "nothing left to stop")
    }

    func testAStartStillQueuedIsNotMistakenForAnEngineThatStopped() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        HeldWork().hold(session)
        fake.packetLogStateValue = plState(capturing: false)
        session.togglePacketCapture()
        session.poll()
        XCTAssertTrue(session.packetCapturing)
        XCTAssertEqual(fake.packetLogStateReads, 0)
    }

    func testStateIsNotReadWithNoCapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.poll()
        clock.addTimeInterval(1)
        session.poll()
        XCTAssertEqual(fake.packetLogStateReads, 0)
    }

    func testACaptureReadsTheBytesUsedEveryHalfSecond() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.packetLogStateValue = plState(usedBytes: 4096)
        session.poll()
        XCTAssertEqual(session.packetLogUsedBytes, 4096)
        let reads = fake.packetLogStateReads
        session.poll()
        XCTAssertEqual(fake.packetLogStateReads, reads, "throttled to 0.5 s")
        clock.addTimeInterval(1)
        session.poll()
        XCTAssertEqual(fake.packetLogStateReads, reads + 1)
    }

    func testAStopRefreshesTheBytesUsed() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.packetLogStateValue = plState(capturing: false, usedBytes: 8192)
        session.togglePacketCapture()
        XCTAssertEqual(session.packetLogUsedBytes, 8192)
    }

    func testRebuiltStationEndsTheCapture() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        let rebuilt = FakeStation()
        session.reconfigure(station: rebuilt, hasCredentials: true)
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 1, "the old engine is the one told")
        XCTAssertEqual(rebuilt.packetLogCallEndedCount, 0)
        XCTAssertTrue(rebuilt.packetLogCalls.isEmpty, "a rebuilt station captures nothing")
    }

    func testQuitStopsACaptureAndLeavesDebugModeSaved() throws {
        let (session, fake, store) = try answeredWithDebugMode()
        session.togglePacketCapture()

        XCTAssertTrue(session.finishPacketLogBeforeQuit(timeout: 1))

        XCTAssertEqual(fake.packetLogCalls.last?.enabled, false)
        XCTAssertNil(fake.packetLogCalls.last?.directory)
        XCTAssertFalse(session.packetCapturing)
        XCTAssertTrue(session.recordingSettings.debugMode, "the switch stays on")
        XCTAssertTrue(store.settings.debugMode, "the next launch shows the button")
    }

    func testQuitWithNoCaptureTouchesNothing() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        XCTAssertTrue(session.finishPacketLogBeforeQuit(timeout: 1))
        XCTAssertTrue(fake.packetLogCalls.isEmpty)
        XCTAssertEqual(fake.packetLogCallEndedCount, 0)
        XCTAssertEqual(fake.packetLogStateReads, 0)
    }

    func testQuitStopsACaptureOffTheMainThread() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        session.runRecordingWork = CallSession.backgroundRecordingWork()

        XCTAssertTrue(session.finishPacketLogBeforeQuit(timeout: 5))
        XCTAssertEqual(fake.packetLogCalls.last?.enabled, false, "returned only after the stop")
        XCTAssertEqual(fake.packetLogOnMainThread.last, false)
    }

    func testQuitWaitsForAStopAlreadyQueued() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        var queued: [() -> Void] = []
        session.runRecordingWork = { queued.append($0) }
        session.togglePacketCapture()  // the operator's stop, not yet run

        let background = CallSession.backgroundRecordingWork()
        session.runRecordingWork = background
        background {
            Thread.sleep(forTimeInterval: 0.1)  // a writer slow to finish
            for work in queued { work() }
        }
        XCTAssertTrue(session.finishPacketLogBeforeQuit(timeout: 5))
        XCTAssertEqual(fake.packetLogCalls.last?.enabled, false, "the queued stop finished first")
    }

    func testQuitGivesUpOnAWedgedPacketLog() throws {
        let (session, _, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        session.runRecordingWork = { _ in }  // a stop that never answers

        let began = Date()
        XCTAssertFalse(session.finishPacketLogBeforeQuit(timeout: 0.2))
        XCTAssertLessThan(Date().timeIntervalSince(began), 2, "a wedged stop cannot hold quit")
    }

    // MARK: - Packet log errors

    /// Answered, capturing, and the engine reporting `error`, with the poll
    /// having stopped the capture over it.
    private func answeredWithAPacketLogError(
        _ error: String = "disk full"
    ) throws -> (CallSession, FakeStation, MemoryRecordingSettingsStore) {
        let (session, fake, store) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.packetLogStateValue = plState(error: error)
        session.poll()
        XCTAssertEqual(session.packetLogError, error)
        XCTAssertFalse(session.packetCapturing)
        return (session, fake, store)
    }

    func testTurningDebugModeOffClearsThePacketLogError() throws {
        let (session, _, _) = try answeredWithAPacketLogError()
        session.recordingSettings.debugMode = false
        XCTAssertNil(session.packetLogError)
    }

    func testTheNextAnsweredCallClearsThePacketLogError() throws {
        let (session, fake, _) = try answeredWithAPacketLogError()
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertEqual(session.packetLogError, "disk full", "the call ending leaves it")
        fake.snapshotToReturn = snap(.answered)
        session.poll()
        XCTAssertNil(session.packetLogError)
    }

    func testAPollOnTheSameCallLeavesThePacketLogError() throws {
        let (session, _, _) = try answeredWithAPacketLogError()
        session.poll()
        XCTAssertEqual(session.packetLogError, "disk full", "still answered: not a new call")
    }

    func testAnErrorAtAStopIsPublished() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.packetLogStateValue = plState(error: "write failed")
        session.togglePacketCapture()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(session.packetLogError, "write failed")
    }

    func testAnErrorAtACallEndSurvivesTheCallEnding() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.packetLogStateValue = plState(error: "write failed")
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertEqual(fake.packetLogCallEndedCount, 1)
        XCTAssertEqual(session.packetLogError, "write failed")
    }

    func testAStopStillQueuedDoesNotBringBackAClearedError() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        let held = HeldWork()
        held.hold(session)
        fake.packetLogStateValue = plState(error: "write failed")
        session.recordingSettings.debugMode = false
        held.runNext()
        XCTAssertEqual(fake.packetLogCalls.last?.enabled, false)
        XCTAssertNil(session.packetLogError, "Debug mode went off after the stop was asked for")
    }

    func testAFailedDialLeavesTheCaptureOnTheCallThatSurvives() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.connectWTError = NSError(domain: "test", code: -5)
        XCTAssertThrowsError(try session.connect(node: "2001", network: .allstar))
        XCTAssertTrue(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 0)
    }

    func testAFailedDialThatTookTheCallDownEndsTheCaptureAtThePoll() throws {
        let (session, fake, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        fake.connectWTError = NSError(domain: "test", code: -5)
        XCTAssertThrowsError(try session.connect(node: "2001", network: .allstar))
        fake.snapshotToReturn = snap(.hangup)
        session.poll()
        XCTAssertFalse(session.packetCapturing)
        XCTAssertEqual(fake.packetLogCallEndedCount, 1)
    }

    func testThePacketCaptureButtonIsOffWhileADialIsInFlight() throws {
        let (session, _, _) = try answeredWithDebugMode()
        XCTAssertTrue(session.packetCapturePressable)
        var duringTheDial: Bool?
        session.testPostEngineCallHook = { duringTheDial = session.packetCapturePressable }
        try session.connect(node: "2001", network: .allstar)
        XCTAssertEqual(duringTheDial, false, "the press would be ignored")
    }

    func testAStopIsStillPressableDuringADial() throws {
        let (session, _, _) = try answeredWithDebugMode()
        session.togglePacketCapture()
        var duringTheDial: Bool?
        session.testPostEngineCallHook = { duringTheDial = session.packetCapturePressable }
        try session.connect(node: "2001", network: .allstar)
        XCTAssertEqual(duringTheDial, true, "a press stops the capture")
    }

    func testAlwaysRecordIsUnaffected() throws {
        var settings = plSettings(debugMode: true)
        settings.alwaysRecord = true
        let (_, fake, _) = try answeredAllStar2000(settings: settings)
        XCTAssertEqual(fake.recordStartCalls.count, 1)
        XCTAssertEqual(fake.recordStartCalls.first?.directory, settings.folder.path)
    }
}
