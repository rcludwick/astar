// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Combine
import XCTest

@testable import AstarCore

/// A scriptable stand-in for the real `Station`, so CallSession logic is tested
/// without audio/network. Conforms to `StationDriving`.
final class FakeStation: StationDriving {
    var snapshotToReturn = CallSnapshot.idle
    var events: [Event] = []
    private(set) var pttCalls: [Bool] = []
    private(set) var connectedNode: String?  // set by connectWT
    private(set) var connectedAddress: String?  // set by connectWT(destNode:address:)
    private(set) var genericDest: String?  // set by generic connect
    private(set) var genericCalling: String?
    private(set) var genericSecret: String?
    private(set) var disconnectCount = 0
    /// Ordered log of lifecycle calls, so tests can assert the stale-call
    /// teardown (disconnect runs before the fresh dial).
    private(set) var callLog: [String] = []
    /// When set, `disconnect()` throws — to prove the teardown's error is ignored.
    var disconnectError: Error?
    /// When set, both `connectWT` overloads throw — to prove a failed AllStar
    /// dial doesn't leave stale per-network state behind (astar-5d8e).
    var connectWTError: Error?

    // MARK: - Dial-race gating (astar-dialrace)
    //
    // Lets tests simulate a dial that's genuinely "in flight" so a
    // `disconnect()`/redial can be interleaved against it deterministically,
    // reproducing the mid-Connecting hang-up race without a real network.
    /// Signaled the instant `connectWT`/`connectM17` is entered, before any
    /// gate-wait — a test blocks on this to know the dial has reached the
    /// station (and is now parked on `connectGate`) before racing a
    /// disconnect/second dial against it. `nil` (the default) is a no-op, so
    /// every other test in this file is unaffected.
    var connectEntered: DispatchSemaphore?
    /// When set, `connectWT`/`connectM17` blocks here right after signaling
    /// `connectEntered`, simulating an in-flight dial. Consumed (cleared)
    /// after the first wait so a second, overlapping call isn't also gated —
    /// only the dial under test blocks; a redial issued while it's blocked
    /// dials straight through.
    var connectGate: DispatchSemaphore?

    private func waitForGateIfSet() {
        connectEntered?.signal()
        if let gate = connectGate {
            connectGate = nil
            gate.wait()
        }
    }

    // Device + gain scripting/recording.
    var inputsToReturn: [String] = []
    var outputsToReturn: [String] = []
    private(set) var setDevicesCalls: [(input: String?, output: String?)] = []
    private(set) var inputGainCalls: [Float] = []
    private(set) var outputGainCalls: [Float] = []
    private(set) var compressionCalls: [Bool] = []
    private(set) var compressionLevelCalls: [Float] = []
    private(set) var txTrimCalls: [Float] = []
    private(set) var noiseReductionCalls: [Bool] = []
    private(set) var rxCompressionCalls: [Bool] = []
    private(set) var rxCompressionLevelCalls: [Float] = []
    var spectrumToReturn: [Float] = []
    var txSpectrumToReturn: [Float] = []
    var rxSpectrumToReturn: [Float] = []
    var characterizeJSON = ""
    private(set) var setMicProfileCalls: [String?] = []
    private(set) var monitorStartCount = 0
    private(set) var monitorStopCount = 0

    func readSnapshot() throws -> CallSnapshot { snapshotToReturn }
    func readEvent() throws -> Event? { events.isEmpty ? nil : events.removeFirst() }
    func connectWT(destNode: String) throws {
        waitForGateIfSet()
        if let connectWTError { throw connectWTError }
        connectedNode = destNode
        callLog.append("connectWT")
    }
    func connectWT(destNode: String, address: String) throws {
        waitForGateIfSet()
        if let connectWTError { throw connectWTError }
        connectedNode = destNode
        connectedAddress = address
        callLog.append("connectWT(addr)")
    }
    func connect(dest: String, calling: String, secret: String?) throws {
        genericDest = dest
        genericCalling = calling
        genericSecret = secret
        callLog.append("connect")
    }
    func disconnect() throws {
        disconnectCount += 1
        callLog.append("disconnect")
        if let disconnectError { throw disconnectError }
    }
    func setPTT(_ on: Bool) throws { pttCalls.append(on) }
    private(set) var dtmfCalls: [Character] = []
    /// When set, `sendDTMF` throws (to prove CallSession propagates the failure).
    var dtmfError: Error?
    func sendDTMF(_ digit: Character) throws {
        if let dtmfError { throw dtmfError }
        dtmfCalls.append(digit)
    }
    private(set) var sequenceCalls: [String] = []
    private(set) var cancelDTMFCalls = 0
    func sendDTMF(sequence: String) throws {
        if let dtmfError { throw dtmfError }
        sequenceCalls.append(sequence)
    }
    func cancelDTMF() throws { cancelDTMFCalls += 1 }
    /// Every dB/s value pushed via `setSpectrumDecay`, to prove the passthrough +
    /// the re-assert points (answered edge / monitor start) reach the station.
    private(set) var spectrumDecayCalls: [Float] = []
    func setSpectrumDecay(dbPerSecond: Float) throws { spectrumDecayCalls.append(dbPerSecond) }

    var mintError: Error?
    private(set) var mintCalls = 0
    func testMintToken() throws {
        mintCalls += 1
        if let mintError { throw mintError }
    }

    func listInputs() throws -> [String] { inputsToReturn }
    func listOutputs() throws -> [String] { outputsToReturn }
    func setDevices(input: String?, output: String?) throws {
        setDevicesCalls.append((input, output))
    }
    func setInputGain(_ gain: Float) throws { inputGainCalls.append(gain) }
    func setOutputGain(_ gain: Float) throws { outputGainCalls.append(gain) }
    func setCompression(_ on: Bool) throws { compressionCalls.append(on) }
    func setCompressionLevel(_ level: Float) throws { compressionLevelCalls.append(level) }
    func setTxTrim(_ gain: Float) throws { txTrimCalls.append(gain) }
    func setNoiseReduction(_ on: Bool) throws { noiseReductionCalls.append(on) }
    func setRxCompression(_ on: Bool) throws { rxCompressionCalls.append(on) }
    func setRxCompressionLevel(_ level: Float) throws { rxCompressionLevelCalls.append(level) }
    func monitorStart(input: String?) throws { monitorStartCount += 1 }
    func monitorStop() throws { monitorStopCount += 1 }
    func micSpectrum() throws -> [Float] { spectrumToReturn }
    func txSpectrum() throws -> [Float] { txSpectrumToReturn }
    func rxSpectrum() throws -> [Float] { rxSpectrumToReturn }
    func characterize(harmonicComb: Bool) throws -> String { characterizeJSON }
    func setMicProfile(_ json: String?) throws { setMicProfileCalls.append(json) }

    // M17 (iax-f2b8 Task 8) scripting/recording.
    private(set) var m17Connects:
        [(host: String, port: UInt16, module: Character, callsign: String)] = []
    private(set) var m17Disconnects = 0
    private(set) var codecDirs: [[String]] = []
    /// When set, `connectM17` throws — to prove CallSession propagates the failure
    /// and never marks the network active on a failed dial.
    var m17ConnectError: Error?
    func connectM17(host: String, port: UInt16, module: Character, callsign: String) throws {
        waitForGateIfSet()
        if let m17ConnectError { throw m17ConnectError }
        m17Connects.append((host, port, module, callsign))
        callLog.append("connectM17")
    }
    func m17Disconnect() throws {
        m17Disconnects += 1
        callLog.append("m17Disconnect")
    }
    func setCodecDirs(_ dirs: [String]) throws {
        codecDirs.append(dirs)
    }
}

/// A station whose device enumeration always throws, to verify CallSession's
/// device-listing accessors degrade to `[]` rather than propagating.
private struct ThrowingStation: StationDriving {
    struct Boom: Error {}
    func readSnapshot() throws -> CallSnapshot { .idle }
    func readEvent() throws -> Event? { nil }
    func connectWT(destNode: String) throws {}
    func connectWT(destNode: String, address: String) throws {}
    func connect(dest: String, calling: String, secret: String?) throws {}
    func disconnect() throws {}
    func setPTT(_ on: Bool) throws {}
    func sendDTMF(_ digit: Character) throws { throw Boom() }
    func sendDTMF(sequence: String) throws { throw Boom() }
    func cancelDTMF() throws { throw Boom() }
    func setSpectrumDecay(dbPerSecond: Float) throws { throw Boom() }
    func testMintToken() throws { throw Boom() }
    func listInputs() throws -> [String] { throw Boom() }
    func listOutputs() throws -> [String] { throw Boom() }
    func setDevices(input: String?, output: String?) throws { throw Boom() }
    func setInputGain(_ gain: Float) throws { throw Boom() }
    func setOutputGain(_ gain: Float) throws { throw Boom() }
    func setCompression(_ on: Bool) throws { throw Boom() }
    func setCompressionLevel(_ level: Float) throws { throw Boom() }
    func setTxTrim(_ gain: Float) throws { throw Boom() }
    func setNoiseReduction(_ on: Bool) throws { throw Boom() }
    func setRxCompression(_ on: Bool) throws { throw Boom() }
    func setRxCompressionLevel(_ level: Float) throws { throw Boom() }
    func monitorStart(input: String?) throws { throw Boom() }
    func monitorStop() throws { throw Boom() }
    func micSpectrum() throws -> [Float] { throw Boom() }
    func txSpectrum() throws -> [Float] { throw Boom() }
    func rxSpectrum() throws -> [Float] { throw Boom() }
    func characterize(harmonicComb: Bool) throws -> String { throw Boom() }
    func setMicProfile(_ json: String?) throws { throw Boom() }
    func connectM17(host: String, port: UInt16, module: Character, callsign: String) throws {
        throw Boom()
    }
    func m17Disconnect() throws { throw Boom() }
    func setCodecDirs(_ dirs: [String]) throws { throw Boom() }
}

/// An in-memory audio store so persistence-path tests don't touch real defaults.
final class MemoryAudioStore: AudioSettingsStore {
    var settings = AudioSettings()
    func load() -> AudioSettings { settings }
    func save(_ settings: AudioSettings) { self.settings = settings }
}

/// An in-memory node-directory store so recents tests don't touch real defaults.
final class MemoryNodeDirectoryStore: NodeDirectoryStore {
    var entries: [NodeEntry] = []
    func all() -> [NodeEntry] { entries }
    func upsert(_ entry: NodeEntry) {
        if let i = entries.firstIndex(where: { $0.id == entry.id }) {
            entries[i] = entry
        } else {
            entries.append(entry)
        }
    }
    func remove(id: String) { entries.removeAll { $0.id == id } }
    private(set) var recordedRecents: [(node: String, label: String, network: Network)] = []
    func recordRecent(node: String, label: String, network: Network) {
        recordedRecents.append((node, label, network))
        if let i = entries.firstIndex(where: { $0.node == node }) {
            entries[i].lastUsed = Date()
            entries[i].network = network
        } else {
            entries.append(NodeEntry(label: label, node: node, lastUsed: Date(), network: network))
        }
    }
}

final class CallSessionTests: XCTestCase {
    func testPollReflectsSnapshotIntoPublishedState() throws {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: true, remotePTT: false,
            txDB: -12, rxDB: -30, rttMS: 40
        )
        let session = CallSession(station: fake)

        session.poll()

        XCTAssertEqual(session.status, .answered)
        XCTAssertTrue(session.ptt)
        XCTAssertFalse(session.remotePTT)
        XCTAssertEqual(session.meters.txDB, -12)
        XCTAssertEqual(session.meters.rxDB, -30)
        XCTAssertEqual(session.meters.rttMS, 40)
    }

    func testPollPublishesNegotiatedFormat() throws {
        // The negotiated codec surfaces on the session while a call is live and
        // clears back to nil when the snapshot no longer carries one (astar-eb6c).
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -30, rttMS: 40, negotiatedFormat: .slin16
        )
        let session = CallSession(station: fake)

        session.poll()
        XCTAssertEqual(session.negotiatedFormat, .slin16)

        fake.snapshotToReturn = .idle
        session.poll()
        XCTAssertNil(session.negotiatedFormat)
    }

    func testStationConfigCarriesCredentialsAndCodecPolicy() {
        // The construction-time seam (astar-eb6c): makeStation builds its
        // StationConfig here. Wideband is always on (astar-e542), so the codec
        // policy is unconditionally prefer_slin16 alongside the credentials.
        let creds = Credentials(portalUser: "AJ7HR", portalPass: "pw", portalNode: "77777")
        let audio = AudioSettings()

        let config = CallSession.stationConfig(credentials: creds, audio: audio)
        XCTAssertEqual(config.portalUser, "AJ7HR")
        XCTAssertEqual(config.portalPass, "pw")
        XCTAssertEqual(config.portalNode, "77777")
        XCTAssertEqual(config.codecPolicy, "prefer_slin16")

        let noCreds = CallSession.stationConfig(credentials: nil, audio: audio)
        XCTAssertNil(noCreds.portalUser)
        XCTAssertEqual(noCreds.codecPolicy, "prefer_slin16")
    }

    func testPollDrainsAllPendingEvents() throws {
        let fake = FakeStation()
        fake.events = [.answered, .remotePTT(true), .hangup]
        let session = CallSession(station: fake)

        session.poll()

        XCTAssertTrue(fake.events.isEmpty, "poll should drain the station's event queue")
    }

    func testConnectWithoutCredentialsThrowsNeedsAccountAndDoesNotDial() {
        // Guest mode is gone: with no AllStar account, connect must throw the
        // needs-account error and never touch the station's dial paths.
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: false)

        XCTAssertThrowsError(try session.connect(node: "55553")) { error in
            XCTAssertEqual(error as? CallSession.ConnectError, .needsAccount)
            XCTAssertEqual(
                (error as? LocalizedError)?.errorDescription,
                "Add your AllStarLink account in Settings to connect."
            )
        }

        XCTAssertNil(fake.connectedNode, "no-account connect must not use the WT path")
        XCTAssertNil(fake.genericDest, "no-account connect must not use the generic path")
        XCTAssertTrue(fake.callLog.isEmpty, "no-account connect must not dial at all")
        XCTAssertNil(session.dialedNode, "throwing before dial must leave dialedNode unset")
    }

    func testConnectUsesAccountWhenCredentialed() throws {
        // With credentials, the default dial is the authenticated WT path.
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)

        try session.connect(node: "55553")

        XCTAssertEqual(fake.connectedNode, "55553", "creds → authenticated WT by default")
        XCTAssertNil(fake.genericDest)
    }

    func testConnectWithoutAddressUsesPlainWTPath() throws {
        // An empty/nil address override must take the normal registrar-resolved
        // WT path, not the manual-address one.
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)

        try session.connect(node: "55553", address: nil)
        try session.connect(node: "55553", address: "")

        XCTAssertEqual(fake.callLog.filter { $0 == "connectWT" }.count, 2)
        XCTAssertNil(fake.connectedAddress, "no override → manual-address path untouched")
    }

    func testConnectWithAddressUsesManualAddressWTPath() throws {
        // A non-empty override routes to connectWT(destNode:address:), still WT
        // (token auth) but dialing the explicit address.
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)

        try session.connect(node: "55553", address: "127.0.0.1:4569")

        XCTAssertEqual(fake.connectedNode, "55553", "node stays the WT dest/CALLING_NUMBER")
        XCTAssertEqual(fake.connectedAddress, "127.0.0.1:4569", "override address is dialed")
        XCTAssertTrue(fake.callLog.contains("connectWT(addr)"))
        XCTAssertNil(fake.genericDest, "still WT, not the generic guest path")
    }

    func testConnectRecordsDialedNode() throws {
        // The session remembers the node it dialed, so surfaces (e.g. the
        // menu-bar right-click menu) can show who we're connected to.
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)

        try session.connect(node: "55553")

        XCTAssertEqual(session.dialedNode, "55553")
    }

    // MARK: - Network dispatch (astar-9b3e)

    func testConnectHamlinkThrowsUnsupportedAndTouchesNothing() {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        XCTAssertThrowsError(try session.connect(node: "refl.example.org", network: .hamlink)) {
            guard case CallSession.ConnectError.unsupportedNetwork = $0 else {
                return XCTFail("expected unsupportedNetwork, got \($0)")
            }
        }
        XCTAssertNil(fake.connectedNode, "no dial may reach the station")
        XCTAssertNil(session.activeCallNetwork)
    }

    func testConnectAllStarViaNetworkOverloadMatchesPlainConnect() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "55553", network: .allstar)
        XCTAssertEqual(fake.connectedNode, "55553")
        XCTAssertEqual(session.activeCallNetwork, .allstar)
    }

    func testDisconnectClearsActiveCallNetwork() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "55553", network: .allstar)
        try session.disconnect()
        XCTAssertNil(session.activeCallNetwork)
    }

    // MARK: - M17 dispatch (astar-c2e5/iax-f2b8 Task 8)

    func testConnectM17ParsesDialsAndSetsActiveNetwork() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        try session.connect(node: "m17.example.net:17001/a", network: .m17)

        XCTAssertEqual(fake.m17Connects.count, 1)
        let call = try XCTUnwrap(fake.m17Connects.first)
        XCTAssertEqual(call.host, "m17.example.net")
        XCTAssertEqual(call.port, 17001)
        XCTAssertEqual(call.module, "A", "module is case-folded to uppercase")
        XCTAssertEqual(call.callsign, "AJ7HR")
        XCTAssertEqual(session.activeCallNetwork, .m17)
        XCTAssertEqual(session.dialedNode, "m17.example.net:17001/a", "raw dial string, verbatim")
    }

    func testConnectM17DefaultsPortWhenOmitted() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        try session.connect(node: "m17.example.net/A", network: .m17)

        XCTAssertEqual(fake.m17Connects.first?.port, 17000)
    }

    func testConnectM17BadTargetThrowsAndTouchesNothing() {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        XCTAssertThrowsError(try session.connect(node: "not-a-valid-target", network: .m17)) {
            guard case CallSession.ConnectError.badM17Target = $0 else {
                return XCTFail("expected badM17Target, got \($0)")
            }
        }
        XCTAssertTrue(fake.m17Connects.isEmpty, "no dial may reach the station")
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.dialedNode, "a bad target must leave dialedNode unset")
    }

    func testConnectM17MissingCallsignThrowsAndTouchesNothing() {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "   "  // blank after trim

        XCTAssertThrowsError(try session.connect(node: "m17.example.net/A", network: .m17)) {
            guard case CallSession.ConnectError.missingCallsign = $0 else {
                return XCTFail("expected missingCallsign, got \($0)")
            }
        }
        XCTAssertTrue(fake.m17Connects.isEmpty, "no dial may reach the station")
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.dialedNode)
    }

    func testConnectM17FailureLeavesActiveNetworkNil() {
        let fake = FakeStation()
        fake.m17ConnectError = NSError(domain: "test", code: -4)
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        XCTAssertThrowsError(try session.connect(node: "m17.example.net/A", network: .m17))
        XCTAssertNil(
            session.activeCallNetwork, "a failed dial must never report .m17 as active")
        // astar-dialrace: the "drop the intent" cleanup moved from
        // MenuPopover into CallSession's (generation-gated) failure path, so
        // it's now directly observable here too.
        XCTAssertNil(session.dialedNode, "a failed dial must drop the attempted node")
    }

    func testDisconnectCallsM17DisconnectWhenActiveNetworkIsM17() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        // connectM17's own pre-teardown already called disconnect() once;
        // the explicit disconnect() below is the second.
        XCTAssertEqual(fake.disconnectCount, 1, "pre-teardown from connectM17's own dial")

        try session.disconnect()

        XCTAssertEqual(fake.m17Disconnects, 1)
        XCTAssertEqual(fake.disconnectCount, 2, "the plain disconnect still runs too")
    }

    func testDisconnectDoesNotCallM17DisconnectForAllStar() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "55553", network: .allstar)

        try session.disconnect()

        XCTAssertEqual(fake.m17Disconnects, 0, "m17Disconnect is M17-specific")
        XCTAssertEqual(fake.disconnectCount, 2, "one pre-teardown from connect, one explicit")
    }

    // MARK: - Dial race: disconnect/redial vs. an in-flight connect (astar-dialrace)
    //
    // Reproduces the mid-Connecting hang-up wedge from
    // `.superpowers/sdd/dial-wedge-report.md`: `dispatchConnect` (MenuPopover)
    // runs the blocking connect on a background queue while Hang Up calls
    // `disconnect()` synchronously on the main thread. `FakeStation.connectGate`
    // parks the engine call mid-flight so these tests can deterministically
    // interleave a disconnect/redial against it.
    //
    // Each test drains the pre-dial bookkeeping's main-thread hop (via an
    // empty `DispatchQueue.main.async` + `wait(for:)`, which pumps the run
    // loop) BEFORE triggering the race. That mirrors realistic timing — the
    // main thread is always free to drain long before a human can tap
    // anything — and keeps the test targeting the real race (disconnect vs.
    // the still-in-flight engine call) rather than an artifact of this
    // test's own tight, semaphore-driven synchronization.
    //
    // A second review round found the generation counter alone wasn't
    // enough: the catch/failure path wasn't gated (only the success path
    // was), the check-then-publish step wasn't atomic with `disconnect()`,
    // and nothing prevented two dials from genuinely overlapping (which
    // would let a stale dial's own teardown clobber a NEWER, legitimately
    // live session). Three more tests below cover those findings:
    // `testStaleFailingDialsCatchPathSkipsItsWriteEntirely` (the catch-path
    // gate), `testDisconnectInTheGapBetweenEngineCallAndCompletionIsHandledAtomically`
    // (the atomicity fix, using `testPostEngineCallHook` to hit the gap
    // deterministically), and `testSecondConnectWhileOneIsInFlightThrowsDialInProgress`
    // (single-flight). Single-flight also means the ORIGINAL "rapid redial
    // supersedes" scenario this file used to test is no longer reachable
    // through the public API — a second dial while one is in flight is now
    // refused outright rather than allowed to race — so that test was
    // replaced rather than kept alongside a now-impossible case.

    func testDisconnectRacingAnInFlightAllStarConnectTearsDownAndLetsAFreshDialSucceed() throws {
        let fake = FakeStation()
        let entered = DispatchSemaphore(value: 0)
        let gate = DispatchSemaphore(value: 0)
        fake.connectEntered = entered
        fake.connectGate = gate
        let session = CallSession(station: fake, hasCredentials: true)

        let dialDone = expectation(description: "in-flight dial completed")
        DispatchQueue.global(qos: .userInitiated).async {
            try? session.connect(node: "55553", network: .allstar)
            dialDone.fulfill()
        }
        entered.wait()  // the dial has reached the station and is now parked on `gate`

        let bookkeepingLanded = expectation(description: "pre-dial bookkeeping landed")
        DispatchQueue.main.async { bookkeepingLanded.fulfill() }
        wait(for: [bookkeepingLanded], timeout: 1.0)
        XCTAssertEqual(
            session.dialedNode, "55553", "precondition: the dial is showing as in-flight")

        // The user's Hang Up — synchronous, on the main thread, exactly like
        // `MenuPopover.disconnect()`.
        try session.disconnect()

        gate.signal()  // let the now-superseded dial's blocked engine call finish
        wait(for: [dialDone], timeout: 1.0)

        XCTAssertNil(
            session.activeCallNetwork,
            "a disconnect issued while connectWT was still in flight must not be silently overridden"
        )
        XCTAssertNil(session.dialedNode)
        XCTAssertEqual(
            fake.disconnectCount, 3,
            "pre-dial teardown + the user's disconnect + the late dial's own stale-teardown")

        // A FRESH dial afterward must succeed cleanly — no lingering
        // AlreadyConnected-style wedge.
        try session.connect(node: "55553", network: .allstar)
        XCTAssertEqual(session.activeCallNetwork, .allstar)
    }

    func testDisconnectRacingAnInFlightM17ConnectTearsDownAndLetsAFreshDialSucceed() throws {
        let fake = FakeStation()
        let entered = DispatchSemaphore(value: 0)
        let gate = DispatchSemaphore(value: 0)
        fake.connectEntered = entered
        fake.connectGate = gate
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        let dialDone = expectation(description: "in-flight M17 dial completed")
        DispatchQueue.global(qos: .userInitiated).async {
            try? session.connect(node: "m17.example.net/A", network: .m17)
            dialDone.fulfill()
        }
        entered.wait()

        let bookkeepingLanded = expectation(description: "pre-dial bookkeeping landed")
        DispatchQueue.main.async { bookkeepingLanded.fulfill() }
        wait(for: [bookkeepingLanded], timeout: 1.0)
        XCTAssertEqual(session.dialedNode, "m17.example.net/A", "precondition")

        try session.disconnect()

        gate.signal()
        wait(for: [dialDone], timeout: 1.0)

        XCTAssertNil(
            session.activeCallNetwork,
            "a disconnect issued while connectM17 was still in flight must not be silently overridden"
        )
        XCTAssertNil(session.dialedNode)
        XCTAssertEqual(
            fake.m17Disconnects, 1,
            "the late dial's own stale-teardown must tear down the zombie M17 session")

        try session.connect(node: "m17.example.net/A", network: .m17)
        XCTAssertEqual(session.activeCallNetwork, .m17)
    }

    func testSecondConnectWhileOneIsInFlightThrowsDialInProgress() throws {
        // IMPORTANT 2 (single-flight): a new connect attempt while another is
        // genuinely in flight must be refused outright, not allowed to race
        // it — this is what makes the "inverse teardown" hazard (a stale
        // dial's own teardown killing a NEWER, legitimately live session)
        // impossible by construction rather than merely unreachable given
        // today's call latencies.
        let fake = FakeStation()
        let entered = DispatchSemaphore(value: 0)
        let gate = DispatchSemaphore(value: 0)
        fake.connectEntered = entered
        fake.connectGate = gate
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        let firstDone = expectation(description: "first (slow) dial completed")
        DispatchQueue.global(qos: .userInitiated).async {
            try? session.connect(node: "m17-a.example.net/A", network: .m17)
            firstDone.fulfill()
        }
        entered.wait()  // the first dial has reached the station and is now parked on `gate`

        let bookkeepingLanded = expectation(description: "first dial's bookkeeping landed")
        DispatchQueue.main.async { bookkeepingLanded.fulfill() }
        wait(for: [bookkeepingLanded], timeout: 1.0)

        // A second dial — same or different target, doesn't matter — while
        // the first is still parked on the gate must be refused immediately,
        // touching nothing.
        XCTAssertThrowsError(try session.connect(node: "m17-b.example.net/A", network: .m17)) {
            guard case CallSession.ConnectError.dialInProgress = $0 else {
                return XCTFail("expected dialInProgress, got \($0)")
            }
        }
        XCTAssertEqual(
            session.dialedNode, "m17-a.example.net/A", "the refused dial touched nothing")
        XCTAssertEqual(fake.m17Connects.count, 0, "the refused dial never reached the station")

        gate.signal()  // let the first dial finish
        wait(for: [firstDone], timeout: 1.0)
        XCTAssertEqual(session.activeCallNetwork, .m17, "the first dial completed normally")

        // Once the first dial has released its claim, a fresh one succeeds.
        try session.connect(node: "m17-b.example.net/A", network: .m17)
        XCTAssertEqual(session.dialedNode, "m17-b.example.net/A")
        XCTAssertEqual(fake.m17Connects.count, 2, "both the first and this fresh dial reached it")
    }

    func testStaleFailingDialsCatchPathSkipsItsWriteEntirely() throws {
        // CRITICAL finding: the catch/failure path must be generation-gated
        // exactly like the success path — every post-completion write, not
        // just the success one, has to check "is my generation still
        // current" before touching published state. Single-flight (tested
        // above) independently makes the reviewed concrete interleave (a
        // stale dial's catch clobbering a DIFFERENT, newer dial's success)
        // unreachable through the public API — with only one dial ever in
        // flight per session, nothing else can be live by the time this
        // one's catch runs. That leaves `disconnect()` as the only reachable
        // way to make a dial stale before its catch fires; this test proves
        // the gated catch doesn't re-touch state disconnect() already
        // settled — using a Combine subscription to prove the write is
        // SKIPPED entirely (not merely coincidentally equal), which a naive
        // "does the final value happen to match" assertion can't discriminate
        // (disconnect's own clear and the ungated catch's clear both land on
        // the same nil).
        let fake = FakeStation()
        let session = CallSession(
            station: fake, hasCredentials: true, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        // Establish a live M17 call first, so the AllStar dial below
        // captures `wasM17 == true` and its catch has a write to (not) make.
        // The gate is armed AFTER this — arming it before would block THIS
        // setup dial (called directly on the test's own thread) forever.
        try session.connect(node: "m17.example.net/A", network: .m17)
        XCTAssertEqual(session.activeCallNetwork, .m17, "precondition")

        let entered = DispatchSemaphore(value: 0)
        let gate = DispatchSemaphore(value: 0)
        fake.connectEntered = entered
        fake.connectGate = gate
        fake.connectWTError = NSError(domain: "test", code: -5)
        var activeCallNetworkEmissions: [Network?] = []
        let cancellable = session.$activeCallNetwork.sink { activeCallNetworkEmissions.append($0) }
        defer { cancellable.cancel() }

        let dialDone = expectation(description: "stale, failing AllStar dial completed")
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                try session.connect(node: "55553", network: .allstar)
                XCTFail("expected the AllStar dial to fail")
            } catch {
                // expected
            }
            dialDone.fulfill()
        }
        entered.wait()

        let bookkeepingLanded = expectation(description: "dial bookkeeping landed")
        DispatchQueue.main.async { bookkeepingLanded.fulfill() }
        wait(for: [bookkeepingLanded], timeout: 1.0)

        // Hang up the M17 call — the AllStar dial is now blocked mid-WT-mint
        // and about to become stale.
        try session.disconnect()
        let emissionsAfterDisconnect = activeCallNetworkEmissions.count

        gate.signal()  // let the stale dial's connectWT throw
        wait(for: [dialDone], timeout: 1.0)

        // Drain the main queue once more so a straggling (buggy, ungated)
        // async publish from the catch handler has a chance to land before
        // we check the count.
        let drained = expectation(description: "main queue drained")
        DispatchQueue.main.async { drained.fulfill() }
        wait(for: [drained], timeout: 1.0)

        XCTAssertNil(session.activeCallNetwork)
        XCTAssertEqual(
            activeCallNetworkEmissions.count, emissionsAfterDisconnect,
            "the stale dial's catch handler must not write activeCallNetwork at all — "
                + "not even redundantly to the same value disconnect() already set")
    }

    func testDisconnectInTheGapBetweenEngineCallAndCompletionIsHandledAtomically() throws {
        // IMPORTANT 1: the generation re-check and the resulting publish/
        // teardown must be atomic with respect to `disconnect()`'s own
        // bump+teardown — a disconnect landing between "check passes" and
        // "the write lands" would recreate a smaller version of the
        // original race. That gap is normally too narrow to hit
        // deterministically with real threads; `testPostEngineCallHook`
        // fires exactly in it (after `connectM17` returns, before
        // `releaseDial`'s check), so calling `disconnect()` from there pins
        // the window precisely.
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        let hookFired = expectation(description: "post-engine-call hook fired")
        session.testPostEngineCallHook = {
            try? session.disconnect()
            hookFired.fulfill()
        }

        let dialDone = expectation(description: "dial completed")
        DispatchQueue.global(qos: .userInitiated).async {
            try? session.connect(node: "m17.example.net/A", network: .m17)
            dialDone.fulfill()
        }
        wait(for: [hookFired, dialDone], timeout: 1.0)

        XCTAssertNil(
            session.activeCallNetwork, "the gap disconnect must win cleanly — no zombie state")
        XCTAssertNil(session.dialedNode)
        XCTAssertEqual(fake.m17Disconnects, 1, "the dial's own stale-teardown must fire")

        // A fresh dial afterward must succeed cleanly.
        session.testPostEngineCallHook = nil
        try session.connect(node: "m17.example.net/A", network: .m17)
        XCTAssertEqual(session.activeCallNetwork, .m17)
    }

    func testDialCompletionPublishesSynchronouslyBeforeConnectReturns() throws {
        // IMPORTANT 1, more directly: `releaseDial` funnels its generation
        // re-check + publish through `onMainSync` — a SYNCHRONOUS main-thread
        // hop (`DispatchQueue.main.sync`), not `.async`, which would leave a
        // gap between "connect() returns" and "the write actually lands" for
        // a concurrent disconnect to land in unseen. Proven directly: check
        // `activeCallNetwork` on the SAME background thread, immediately
        // after `connect()` returns, with no run-loop pump/wait in between.
        // An async publish could still be sitting unexecuted on the main
        // queue at this exact instant; a synchronous one cannot.
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        let checked = expectation(description: "checked immediately after connect() returned")
        var sawImmediately: Network?
        DispatchQueue.global(qos: .userInitiated).async {
            try? session.connect(node: "m17.example.net/A", network: .m17)
            sawImmediately = session.activeCallNetwork
            checked.fulfill()
        }
        wait(for: [checked], timeout: 1.0)

        XCTAssertEqual(
            sawImmediately, .m17,
            "the publish must be synchronously visible the instant connect() returns")
    }

    func testDisconnectAfterAllStarEngineCallSucceedsDoesNotGetResurrectedByTheWrapper() throws {
        // Round-3 re-review finding: `connect(node:network:)`'s `.allstar`
        // arm used to stamp `activeCallNetwork = .allstar` in a SEPARATE
        // statement AFTER `connectAllStar` returned — outside its atomic
        // critical section. A disconnect landing in that gap (dial
        // completes → disconnect → the wrapper's out-of-band write still
        // runs) got clobbered right back to `.allstar`. Fixed by having
        // `connectAllStar` stamp the network itself, inside `releaseDial`'s
        // `onCurrent` (atomic, via `onMainSync`) — there is no longer a
        // separate wrapper write left to resurrect anything. Pinned with the
        // same `testPostEngineCallHook` technique as the M17 gap test:
        // fires right after `connectWT` succeeds ("the dial completes"),
        // before any completion processing runs.
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)

        let hookFired = expectation(description: "post-engine-call hook fired")
        session.testPostEngineCallHook = {
            try? session.disconnect()
            hookFired.fulfill()
        }

        let dialDone = expectation(description: "dial completed")
        DispatchQueue.global(qos: .userInitiated).async {
            try? session.connect(node: "55553", network: .allstar)
            dialDone.fulfill()
        }
        wait(for: [hookFired, dialDone], timeout: 1.0)

        XCTAssertNil(
            session.activeCallNetwork,
            "the gap disconnect must win cleanly — no wrapper-window resurrection to .allstar")
        XCTAssertNil(session.dialedNode)

        // A fresh dial afterward must succeed cleanly.
        session.testPostEngineCallHook = nil
        try session.connect(node: "55553", network: .allstar)
        XCTAssertEqual(session.activeCallNetwork, .allstar)
    }

    func testAllStarNetworkStampPublishesSynchronouslyBeforeConnectReturns() throws {
        // The direct counterpart to
        // `testDialCompletionPublishesSynchronouslyBeforeConnectReturns` for
        // the `.allstar` network stamp specifically — this is the test that
        // actually DISCRIMINATES the round-3 fix from the round-2 code (the
        // hook-based test above passes against both, since round-2's
        // internal generation check already handled a disconnect landing
        // BEFORE it ran; the bug was in the gap AFTER that check passed but
        // BEFORE the wrapper's separate, off-thread-hopping write). Checks
        // `activeCallNetwork` on the SAME background thread, immediately
        // after `connect(node:network:)` returns, with no run-loop pump in
        // between: the OLD wrapper's `setActiveCallNetwork(.allstar)` call,
        // made from a background thread, hopped via `DispatchQueue.main
        // .async` and could still be sitting unexecuted at this exact
        // instant; the new, atomic, `onMainSync`-routed stamp cannot.
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)

        let checked = expectation(description: "checked immediately after connect() returned")
        var sawImmediately: Network?
        DispatchQueue.global(qos: .userInitiated).async {
            try? session.connect(node: "55553", network: .allstar)
            sawImmediately = session.activeCallNetwork
            checked.fulfill()
        }
        wait(for: [checked], timeout: 1.0)

        XCTAssertEqual(
            sawImmediately, .allstar,
            "the .allstar stamp must be synchronously visible the instant connect() returns")
    }

    func testM17AvailableEdgeGuardedFromSnapshot() {
        // Mirrors the dtmfPlayed edge-guard pattern: publishes only on change.
        let fake = FakeStation()
        let session = CallSession(station: fake)
        XCTAssertFalse(session.m17Available)

        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            m17Available: true)
        session.poll()
        XCTAssertTrue(session.m17Available)

        fake.snapshotToReturn = .idle
        session.poll()
        XCTAssertFalse(session.m17Available)
    }

    // MARK: - M17 TX-processing override (astar-5d8e)

    func testM17OverridesDefaultToFieldTestedRecipe() {
        // Rob's field-tested M17 recipe (astar-m17defaults, 2026-08-04 on-air
        // testing): 25% mic level, compression ON at 80% strength, 80% TX
        // trim. Noise reduction stays off. This replaced the earlier "clean
        // chain" defaults (compression OFF) once further A/B testing found
        // compression, at these levels, doesn't reproduce the parrot echo.
        let session = CallSession(station: FakeStation(), userDefaults: scratchM17Defaults())
        XCTAssertFalse(session.m17Overrides.noiseReduction)
        XCTAssertTrue(session.m17Overrides.compression)
        XCTAssertEqual(session.m17Overrides.compressionLevel, 0.80)
        XCTAssertEqual(session.m17Overrides.txTrim, 0.80)
        XCTAssertEqual(session.m17Overrides.inputGain, 0.25)
    }

    func testM17OverridesLoadSavedValuesAndPersistOnChange() {
        let defaults = scratchM17Defaults()
        defaults.set(true, forKey: "m17.audio.noiseReduction")
        defaults.set(Float(0.33), forKey: "m17.audio.compressionLevel")
        // An explicitly-persisted `false` (e.g. Rob's own setting from before
        // the astar-m17defaults flip) must NOT be migrated to the new `true`
        // default — defaults only fill an ABSENT key.
        defaults.set(false, forKey: "m17.audio.compression")

        let session = CallSession(station: FakeStation(), userDefaults: defaults)
        XCTAssertTrue(session.m17Overrides.noiseReduction)
        XCTAssertEqual(session.m17Overrides.compressionLevel, 0.33)
        XCTAssertFalse(
            session.m17Overrides.compression,
            "an explicit persisted false is respected, not auto-flipped to the new default")

        session.setM17TxTrim(0.15)
        XCTAssertEqual(defaults.float(forKey: "m17.audio.txTrim"), 0.15)

        session.setM17InputGain(0.5)
        XCTAssertEqual(defaults.float(forKey: "m17.audio.inputGain"), 0.5)
    }

    func testM17OverridesCompressionAbsentKeyDefaultsToTrue() {
        // The back-compat case astar-m17defaults introduces: a set persisted
        // before this change has noiseReduction/compressionLevel/txTrim keys
        // but never touched `compression` (it was always false, so it may
        // never have been explicitly written) — an ABSENT key must now load
        // as `true`, the new field-tested default, not `false`.
        let defaults = scratchM17Defaults()
        defaults.set(Float(0.5), forKey: "m17.audio.txTrim")

        let session = CallSession(station: FakeStation(), userDefaults: defaults)

        XCTAssertTrue(session.m17Overrides.compression, "absent key loads the new true default")
        XCTAssertEqual(session.m17Overrides.inputGain, 0.25, "absent key loads the new default")
    }

    func testConnectM17PushesOverrideValues() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        try session.connect(node: "m17.example.net/A", network: .m17)

        XCTAssertEqual(fake.noiseReductionCalls.last, false)
        XCTAssertEqual(fake.compressionCalls.last, true)
        XCTAssertEqual(fake.compressionLevelCalls.last, 0.80)
        XCTAssertEqual(fake.txTrimCalls.last, 0.80)
        XCTAssertEqual(fake.inputGainCalls.last, 0.25)
    }

    func testConnectM17PushesWhateverOverrideWasEditedBeforeDialing() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        session.setM17NoiseReduction(true)
        session.setM17Compression(true)
        session.setM17CompressionLevel(0.55)
        session.setM17TxTrim(0.4)
        session.setM17InputGain(0.6)

        try session.connect(node: "m17.example.net/A", network: .m17)

        XCTAssertEqual(fake.noiseReductionCalls.last, true)
        XCTAssertEqual(fake.compressionCalls.last, true)
        XCTAssertEqual(fake.compressionLevelCalls.last, 0.55)
        XCTAssertEqual(fake.txTrimCalls.last, 0.4)
        XCTAssertEqual(fake.inputGainCalls.last, 0.6)
    }

    func testAllStarNetworkDialAfterM17RestoresStandardValues() throws {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        audioStore.settings.compression = true
        audioStore.settings.compressionLevel = 0.5
        audioStore.settings.txTrim = 0.3
        audioStore.settings.noiseReduction = true
        audioStore.settings.inputGain = 0.66
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: audioStore,
            userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)
        XCTAssertEqual(session.activeCallNetwork, .m17)

        try session.connect(node: "55553", network: .allstar)

        XCTAssertEqual(fake.noiseReductionCalls.last, true, "standard value restored")
        XCTAssertEqual(fake.compressionCalls.last, true)
        XCTAssertEqual(fake.compressionLevelCalls.last, 0.5)
        XCTAssertEqual(fake.txTrimCalls.last, 0.3)
        XCTAssertEqual(fake.inputGainCalls.last, 0.66, "shared mic gain restored")
        XCTAssertEqual(session.activeCallNetwork, .allstar)
    }

    func testAllStarAddressDialAfterM17RestoresStandardValues() throws {
        // The Advanced-options manual-address path calls straight into
        // `connect(node:address:)`, bypassing `connect(node:network:)`
        // entirely (astar-c7a1 point 6) — the restore must still fire there.
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        audioStore.settings.txTrim = 0.25
        audioStore.settings.inputGain = 0.55
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: audioStore,
            userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        try session.connect(node: "55553", address: "10.0.0.5:4569")

        XCTAssertEqual(fake.txTrimCalls.last, 0.25)
        XCTAssertEqual(fake.inputGainCalls.last, 0.55, "shared mic gain restored")
    }

    func testDisconnectAfterM17RestoresStandardValues() throws {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        audioStore.settings.noiseReduction = true
        audioStore.settings.inputGain = 0.77
        let session = CallSession(
            station: fake, audioStore: audioStore, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        try session.disconnect()

        XCTAssertEqual(fake.noiseReductionCalls.last, true)
        XCTAssertEqual(fake.inputGainCalls.last, 0.77, "shared mic gain restored")
        XCTAssertNil(session.activeCallNetwork)
    }

    func testM17PushAndRestoreCycleNeverTouchesRxCompression() throws {
        // Regression guard (iax-a4e7/astar-outchain review): RX/output
        // compression is a shared listener-side setting with NO M17 override
        // — unlike the five TX-processing knobs, `pushM17TxOverrides` and
        // `restoreStandardTxProcessing` must never call the rx-compression
        // setters at all, in either direction. If `rxCompression` is ever
        // added to `M17AudioOverrides` "for completeness," this must fail.
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        audioStore.settings.rxCompression = true
        audioStore.settings.rxCompressionLevel = 0.42
        let session = CallSession(
            station: fake, audioStore: audioStore, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"

        try session.connect(node: "m17.example.net/A", network: .m17)  // pushM17TxOverrides
        try session.disconnect()  // restoreStandardTxProcessing

        XCTAssertTrue(
            fake.rxCompressionCalls.isEmpty,
            "M17 push/restore must never touch rx compression")
        XCTAssertTrue(
            fake.rxCompressionLevelCalls.isEmpty,
            "M17 push/restore must never touch rx compression level")
    }

    func testRemoteHangupDuringM17RestoresStandardValuesAndClearsActiveNetwork() throws {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        audioStore.settings.txTrim = 0.6
        audioStore.settings.inputGain = 0.44
        let session = CallSession(
            station: fake, audioStore: audioStore, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil)
        session.poll()
        XCTAssertEqual(session.activeCallNetwork, .m17)

        // The far end hangs up — no `disconnect()` call from the user.
        fake.snapshotToReturn = CallSnapshot(
            status: .hangup, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil)
        session.poll()

        XCTAssertNil(session.activeCallNetwork, "the poll-path edge clears the stale network")
        XCTAssertEqual(fake.txTrimCalls.last, 0.6, "standard value restored")
        XCTAssertEqual(fake.inputGainCalls.last, 0.44, "shared mic gain restored")
        let restoreCallCount = fake.txTrimCalls.count

        // A further poll at the same terminal status must not re-fire the restore.
        session.poll()
        XCTAssertEqual(fake.txTrimCalls.count, restoreCallCount, "edge-triggered — no repeat push")
    }

    func testEditingM17OverrideWhileCallLivePushesLive() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        session.setM17NoiseReduction(true)

        XCTAssertEqual(fake.noiseReductionCalls.last, true)
        XCTAssertTrue(session.m17Overrides.noiseReduction)
    }

    func testEditingM17OverrideWhileIdleDoesNotTouchStation() {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())

        session.setM17CompressionLevel(0.42)

        XCTAssertTrue(fake.compressionLevelCalls.isEmpty, "no call is live — nothing to push")
        XCTAssertEqual(session.m17Overrides.compressionLevel, 0.42, "still published + persisted")
    }

    func testEditingM17InputGainWhileCallLivePushesLive() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        session.setM17InputGain(0.5)

        XCTAssertEqual(fake.inputGainCalls.last, 0.5)
        XCTAssertEqual(session.m17Overrides.inputGain, 0.5)
    }

    func testEditingM17InputGainWhileIdleDoesNotTouchStation() {
        let fake = FakeStation()
        let session = CallSession(station: fake, userDefaults: scratchM17Defaults())

        session.setM17InputGain(0.5)

        XCTAssertTrue(fake.inputGainCalls.isEmpty, "no call is live — nothing to push")
        XCTAssertEqual(session.m17Overrides.inputGain, 0.5, "still published + persisted")
    }

    // MARK: - Context-aware toggles (astar-5d8e review fix, CRITICAL)
    //
    // The menu-bar right-click "Voice compression"/"Noise reduction" items
    // used to call `setCompression`/`setNoiseReduction` unconditionally,
    // which during a live M17 call would clobber the pushed M17 override and
    // silently persist the flip into the shared `AudioSettings` instead.
    // `toggleCompression`/`toggleNoiseReduction` (and the `effective*`
    // readers behind the menu's checkmarks) are the extracted, testable fix.

    func testToggleCompressionWhileM17ActiveMutatesOverrideNotStandard() throws {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        let session = CallSession(
            station: fake, audioStore: audioStore, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)
        XCTAssertTrue(
            session.effectiveCompression,
            "precondition: M17 default is on (astar-m17defaults' field-tested recipe)")

        session.toggleCompression()

        XCTAssertFalse(session.m17Overrides.compression, "landed in the M17 override")
        XCTAssertFalse(audioStore.settings.compression, "the shared AudioSettings is untouched")
        XCTAssertFalse(session.effectiveCompression)
        XCTAssertEqual(fake.compressionCalls.last, false, "pushed live")
    }

    func testToggleNoiseReductionWhileM17ActiveMutatesOverrideNotStandard() throws {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        let session = CallSession(
            station: fake, audioStore: audioStore, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        session.toggleNoiseReduction()

        XCTAssertTrue(session.m17Overrides.noiseReduction, "landed in the M17 override")
        XCTAssertFalse(audioStore.settings.noiseReduction, "the shared AudioSettings is untouched")
        XCTAssertTrue(session.effectiveNoiseReduction)
    }

    func testToggleCompressionWhileIdleMutatesStandardSettings() {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        let session = CallSession(
            station: fake, audioStore: audioStore, userDefaults: scratchM17Defaults())

        session.toggleCompression()

        XCTAssertTrue(session.compression)
        XCTAssertTrue(audioStore.settings.compression, "routed to the standard store")
        XCTAssertEqual(
            session.m17Overrides.compression, true, "the M17 override is untouched (its default)")
        XCTAssertTrue(session.effectiveCompression)
    }

    func testToggleNoiseReductionWhileIdleMutatesStandardSettings() {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        let session = CallSession(
            station: fake, audioStore: audioStore, userDefaults: scratchM17Defaults())

        session.toggleNoiseReduction()

        XCTAssertTrue(session.noiseReduction)
        XCTAssertTrue(audioStore.settings.noiseReduction, "routed to the standard store")
        XCTAssertFalse(session.m17Overrides.noiseReduction, "the M17 override is untouched")
    }

    func testToggleCompressionWhileAllStarActiveMutatesStandardSettings() throws {
        // An active call on a network other than M17 must still route to the
        // shared settings — only `.m17` diverts.
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: audioStore,
            userDefaults: scratchM17Defaults())
        try session.connect(node: "55553", network: .allstar)

        session.toggleCompression()

        XCTAssertTrue(audioStore.settings.compression)
        XCTAssertTrue(
            session.m17Overrides.compression, "the M17 override is untouched (its default)")
    }

    // MARK: - Failed AllStar dial out of M17 state (astar-5d8e review fix, IMPORTANT)
    //
    // `activeCallNetwork` only advances to `.allstar` on a SUCCESSFUL dial —
    // a thrown `connectWT` (portal failure, unreachable address) used to
    // leave it stuck at `.m17` even though the standard TX chain had already
    // been restored, so a Quick-settings edit made in that window would
    // misroute into the M17 override and be silently discarded later.

    func testFailedAllStarNetworkDialFromM17StateClearsActiveNetwork() throws {
        let fake = FakeStation()
        let session = CallSession(
            station: fake, hasCredentials: true, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)
        XCTAssertEqual(session.activeCallNetwork, .m17)

        fake.connectWTError = NSError(domain: "test", code: -5)
        XCTAssertThrowsError(try session.connect(node: "55553", network: .allstar))

        XCTAssertNil(
            session.activeCallNetwork,
            "a failed dial out of M17 must not leave activeCallNetwork stuck at .m17")
        // astar-dialrace: moved from MenuPopover into CallSession's
        // (generation-gated) failure path.
        XCTAssertNil(session.dialedNode, "a failed dial must drop the attempted node")
    }

    func testFailedAddressDialFromM17StateClearsActiveNetwork() throws {
        // The Advanced-options manual-address path calls straight into
        // `connect(node:address:)`, bypassing `connect(node:network:)`
        // entirely (astar-c7a1 point 6) — the fix must cover this entry
        // point too, not just the network-aware wrapper.
        let fake = FakeStation()
        let session = CallSession(
            station: fake, hasCredentials: true, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        fake.connectWTError = NSError(domain: "test", code: -5)
        XCTAssertThrowsError(try session.connect(node: "55553", address: "10.0.0.5:4569"))

        XCTAssertNil(session.activeCallNetwork)
    }

    func testEditingAfterFailedAllStarDialFromM17RoutesToStandardSettings() throws {
        let fake = FakeStation()
        let audioStore = MemoryAudioStore()
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: audioStore,
            userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)
        fake.connectWTError = NSError(domain: "test", code: -5)
        XCTAssertThrowsError(try session.connect(node: "55553", network: .allstar))
        XCTAssertNil(session.activeCallNetwork, "precondition")

        session.toggleNoiseReduction()

        XCTAssertTrue(session.noiseReduction)
        XCTAssertTrue(audioStore.settings.noiseReduction, "routed to the standard store")
        XCTAssertFalse(session.m17Overrides.noiseReduction, "the M17 override is untouched")
    }

    func testAnsweredCallRecordsRecentStampsM17Network() throws {
        // astar-c2e5: recordRecent must stamp the network the call actually
        // went out on, not always default to AllStar.
        let fake = FakeStation()
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(
            station: fake, directoryStore: directory, userDefaults: scratchM17Defaults())
        session.m17Callsign = "AJ7HR"
        try session.connect(node: "m17.example.net/A", network: .m17)

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -40, rxDB: -50, rttMS: 30)
        session.poll()

        XCTAssertEqual(directory.recordedRecents.map(\.network), [.m17])
    }

    func testAnsweredCallRecordsRecentForDialedNode() throws {
        // On a successful connect (status reaches .answered), the dialed node is
        // recorded as a recent so the user can re-pick it from the directory.
        let fake = FakeStation()
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(
            station: fake, hasCredentials: true,
            directoryStore: directory)
        try session.connect(node: "77777")

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -40, rxDB: -50, rttMS: 30)
        session.poll()

        XCTAssertEqual(
            directory.recordedRecents.map(\.node), ["77777"],
            "an answered call records a recent for the dialed node")
    }

    func testAddressDialTakesNonNumericLabelThroughToRecents() throws {
        // The smart dial field (astar-427f) passes the typed address as BOTH
        // the node label and the dial address. The label is safe off the wire:
        // the WT dial calls "s" with calling_number = the user's own node, so
        // the non-numeric string is display/recents-only. On answer it must be
        // recorded as a recent keyed by the address string (NodeDirectory keys
        // by string, no numeric assumption).
        let fake = FakeStation()
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(
            station: fake, hasCredentials: true,
            directoryStore: directory)
        try session.connect(node: "my-node.example.com:4569", address: "my-node.example.com:4569")

        XCTAssertEqual(fake.connectedNode, "my-node.example.com:4569")
        XCTAssertEqual(
            fake.connectedAddress, "my-node.example.com:4569",
            "the typed address is what gets dialed")
        XCTAssertEqual(session.dialedNode, "my-node.example.com:4569")

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -40, rxDB: -50, rttMS: 30)
        session.poll()

        XCTAssertEqual(
            directory.recordedRecents.map(\.node), ["my-node.example.com:4569"],
            "an answered address dial records a recent under the address string")
    }

    func testAnsweredRecordsRecentOnlyOnce() throws {
        // Staying answered across multiple polls must not keep re-recording.
        let fake = FakeStation()
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(
            station: fake, hasCredentials: true,
            directoryStore: directory)
        try session.connect(node: "55553")
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -40, rxDB: -50, rttMS: 30)
        session.poll()
        session.poll()
        session.poll()

        XCTAssertEqual(
            directory.recordedRecents.count, 1,
            "recent is recorded once per answered call, not every poll")
    }

    func testDialingDoesNotRecordRecent() throws {
        // A call still dialing (not yet answered) records nothing.
        let fake = FakeStation()
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(
            station: fake, hasCredentials: true,
            directoryStore: directory)
        try session.connect(node: "55553")
        fake.snapshotToReturn = CallSnapshot(
            status: .dialing, ptt: false, remotePTT: false,
            txDB: -40, rxDB: -50, rttMS: nil)
        session.poll()

        XCTAssertTrue(
            directory.recordedRecents.isEmpty,
            "a dialing (unanswered) call records no recent")
    }

    func testAddFavoriteCreatesFavoriteEntry() throws {
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.addFavorite(node: "77777", label: "AJ7HR")

        XCTAssertTrue(session.isFavorite(node: "77777"))
        XCTAssertEqual(session.directoryFavorites().map(\.label), ["AJ7HR"])
    }

    func testAddFavoriteMergesWithExistingRecentPreservingLastUsed() throws {
        // Favoriting a node that's already a recent flips the flag in place and
        // keeps its lastUsed, rather than creating a duplicate.
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [
            NodeEntry(
                label: "77777", node: "77777",
                lastUsed: Date(timeIntervalSince1970: 100))
        ]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.addFavorite(node: "77777", label: "AJ7HR")

        XCTAssertEqual(directory.entries.count, 1, "merge, don't duplicate")
        XCTAssertTrue(directory.entries[0].favorite)
        XCTAssertEqual(directory.entries[0].label, "AJ7HR")
        XCTAssertNotNil(directory.entries[0].lastUsed, "recent's lastUsed preserved")
    }

    func testAddFavoriteStampsNetworkOnNewEntry() throws {
        // astar-c2e5: favoriting a brand-new M17 target stamps its network,
        // so the badge/picker reads it back correctly.
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.addFavorite(node: "m17.example.net/A", label: "W1AW", network: .m17)

        XCTAssertEqual(directory.entries.first?.network, .m17)
    }

    func testAddFavoriteDefaultsToAllStarNetwork() throws {
        // The default parameter preserves today's AllStar-only call sites
        // (MenuPopover, FavoritesSettingsView) verbatim.
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.addFavorite(node: "77777", label: "AJ7HR")

        XCTAssertEqual(directory.entries.first?.network, .allstar)
    }

    func testAddFavoriteLeavesExistingEntryNetworkUntouched() throws {
        // Re-favoriting an entry that's already correctly stamped (e.g. by
        // `recordRecent` while on M17) must not stomp it back to the
        // `network` parameter's default when the caller doesn't know better
        // (astar-c2e5) — only NEW entries get the parameter's value.
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [
            NodeEntry(label: "m17.example.net/A", node: "m17.example.net/A", network: .m17)
        ]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.addFavorite(node: "m17.example.net/A", label: "W1AW")  // default .allstar

        XCTAssertEqual(directory.entries.first?.network, .m17, "existing network preserved")
    }

    func testRemoveFavoriteKeepsRecentButClearsFlag() throws {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [
            NodeEntry(
                label: "AJ7HR", node: "77777", favorite: true,
                lastUsed: Date(timeIntervalSince1970: 100))
        ]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.removeFavorite(node: "77777")

        XCTAssertFalse(session.isFavorite(node: "77777"))
        XCTAssertEqual(directory.entries.count, 1, "still a recent, just not a favorite")
    }

    func testRemoveFavoriteDeletesEntryWithNoRecentHistory() throws {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [NodeEntry(label: "AJ7HR", node: "77777", favorite: true)]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.removeFavorite(node: "77777")

        XCTAssertTrue(directory.entries.isEmpty, "a never-dialed favorite is removed outright")
    }

    func testDirectoryAddFavoriteValidatesDigits() throws {
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        XCTAssertTrue(session.directoryAddFavorite(node: "77777", label: "AJ7HR"))
        XCTAssertFalse(session.directoryAddFavorite(node: "", label: "X"))
        XCTAssertFalse(session.directoryAddFavorite(node: "abc", label: "X"))
        XCTAssertEqual(session.directoryFavorites().map(\.node), ["77777"])
    }

    func testDirectoryAddFavoriteDedupesByNode() throws {
        let directory = MemoryNodeDirectoryStore()
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.directoryAddFavorite(node: "77777", label: "First")
        session.directoryAddFavorite(node: "77777", label: "Renamed")

        XCTAssertEqual(directory.entries.count, 1, "same node merges, no duplicate")
        XCTAssertEqual(directory.entries[0].label, "Renamed")
    }

    func testDirectoryRenamePreservesNodeFavoriteAndLastUsed() throws {
        let directory = MemoryNodeDirectoryStore()
        let when = Date(timeIntervalSince1970: 100)
        directory.entries = [
            NodeEntry(id: "x", label: "Old", node: "77777", favorite: true, lastUsed: when)
        ]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.directoryRename(id: "x", to: "New")

        XCTAssertEqual(directory.entries[0].label, "New")
        XCTAssertEqual(directory.entries[0].node, "77777")
        XCTAssertTrue(directory.entries[0].favorite)
        XCTAssertEqual(directory.entries[0].lastUsed, when)
    }

    func testDirectoryRenameIgnoresEmptyLabel() throws {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [NodeEntry(id: "x", label: "Keep", node: "1", favorite: true)]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.directoryRename(id: "x", to: "   ")

        XCTAssertEqual(directory.entries[0].label, "Keep")
    }

    func testDirectoryRemoveDeletesById() throws {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [
            NodeEntry(id: "a", label: "A", node: "1", favorite: true),
            NodeEntry(id: "b", label: "B", node: "2", favorite: true),
        ]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        session.directoryRemove(id: "a")

        XCTAssertEqual(directory.entries.map(\.id), ["b"])
    }

    func testNameForNodeResolvesSavedFavorite() throws {
        let directory = MemoryNodeDirectoryStore()
        directory.entries = [NodeEntry(label: "AJ7HR", node: "77777", favorite: true)]
        let session = CallSession(station: FakeStation(), directoryStore: directory)

        XCTAssertEqual(session.name(forNode: "77777"), "AJ7HR")
        XCTAssertNil(session.name(forNode: "00000"))
        XCTAssertEqual(session.displayName(forNode: "00000"), "00000")
    }

    func testDisconnectClearsDialedNode() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "77777")
        XCTAssertEqual(session.dialedNode, "77777")

        try session.disconnect()

        XCTAssertNil(session.dialedNode)
    }

    func testConnectTearsDownStaleCallBeforeWTDial() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)

        try session.connect(node: "77777")

        XCTAssertEqual(
            fake.callLog, ["disconnect", "connectWT"],
            "WT connect must also tear down any stale call first")
    }

    func testConnectIgnoresTeardownDisconnectError() throws {
        // The pre-dial teardown is best-effort: if there is no call to hang up,
        // disconnect may throw, and that must not block the fresh dial.
        struct Boom: Error {}
        let fake = FakeStation()
        fake.disconnectError = Boom()
        let session = CallSession(station: fake, hasCredentials: true)

        XCTAssertNoThrow(try session.connect(node: "55553"))
        XCTAssertEqual(fake.connectedNode, "55553", "dial proceeds despite teardown error")
    }

    func testIsConnectingDefaultsFalse() throws {
        let session = CallSession(station: FakeStation())
        XCTAssertFalse(session.isConnecting)
    }

    func testApplyAudioSettingsPushesToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        session.applyAudioSettings(
            AudioSettings(input: "UCI150", output: "Speakers", inputGain: 1.5, outputGain: 0.5)
        )

        XCTAssertEqual(fake.setDevicesCalls.first?.input, "UCI150")
        XCTAssertEqual(fake.setDevicesCalls.first?.output, "Speakers")
        XCTAssertEqual(fake.inputGainCalls, [1.5])
        XCTAssertEqual(fake.outputGainCalls, [0.5])
    }

    func testReconfigureUpdatesStationAndCredentials() throws {
        let session = CallSession(station: FakeStation(), hasCredentials: false)
        let newStation = FakeStation()

        session.reconfigure(station: newStation, hasCredentials: true)
        try session.connect(node: "77777")

        XCTAssertEqual(
            newStation.connectedNode, "77777", "connect should hit the new station via WT")
        XCTAssertTrue(session.hasCredentials)
    }

    func testSetPTTPassesThroughToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.setPTT(true)
        try session.setPTT(false)

        XCTAssertEqual(fake.pttCalls, [true, false])
    }

    func testTokenMintPassesThroughToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.testTokenMint()

        XCTAssertEqual(fake.mintCalls, 1, "testTokenMint hits the station's mint")
    }

    func testTokenMintPropagatesFailure() {
        let fake = FakeStation()
        fake.mintError = NullStationError.noEngine
        let session = CallSession(station: fake)

        XCTAssertThrowsError(try session.testTokenMint(), "a mint failure propagates to the caller")
    }

    func testDisconnectPassesThroughToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.disconnect()

        XCTAssertEqual(fake.disconnectCount, 1)
    }

    func testStartPollsImmediately() throws {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .dialing, ptt: false, remotePTT: false, txDB: -50, rxDB: -55, rttMS: nil
        )
        let session = CallSession(station: fake)

        session.start()
        defer { session.stop() }

        XCTAssertEqual(session.status, .dialing, "start() should poll once immediately")
    }

    // MARK: - Devices + gain

    func testInputsPassesThroughFromStation() throws {
        let fake = FakeStation()
        fake.inputsToReturn = ["Built-in Mic", "UCI150"]
        let session = CallSession(station: fake)

        XCTAssertEqual(session.inputs(), ["Built-in Mic", "UCI150"])
    }

    func testOutputsPassesThroughFromStation() throws {
        let fake = FakeStation()
        fake.outputsToReturn = ["Built-in Output", "UCI150"]
        let session = CallSession(station: fake)

        XCTAssertEqual(session.outputs(), ["Built-in Output", "UCI150"])
    }

    func testInputsReturnsEmptyWhenStationThrows() throws {
        // ThrowingStation simulates a station with no engine; inputs() should
        // swallow and return [].
        let session = CallSession(station: ThrowingStation())
        XCTAssertEqual(session.inputs(), [])
        XCTAssertEqual(session.outputs(), [])
    }

    func testSelectDevicesPassesThroughToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.selectDevices(input: "UCI150", output: "Built-in Output")

        XCTAssertEqual(fake.setDevicesCalls.count, 1)
        XCTAssertEqual(fake.setDevicesCalls.first?.input, "UCI150")
        XCTAssertEqual(fake.setDevicesCalls.first?.output, "Built-in Output")
    }

    func testSetInputGainPassesThroughToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.setInputGain(1.5)

        XCTAssertEqual(fake.inputGainCalls, [1.5])
    }

    func testSetOutputGainPassesThroughToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.setOutputGain(0.75)

        XCTAssertEqual(fake.outputGainCalls, [0.75])
    }

    // MARK: - PTT source tick

    func testPttSourceTickReceivesSnapshotAndDrivesSetPTT() throws {
        // A source returning a non-nil Bool should drive station.setPTT with it,
        // and it must receive the snapshot's remotePTT/rxDB.
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: true,
            txDB: -20, rxDB: -33, rttMS: 25
        )
        let session = CallSession(station: fake)
        var seenRemote: Bool?
        var seenRx: Float?
        session.pttSourceTick = { remote, rx in
            seenRemote = remote
            seenRx = rx
            return true
        }

        session.poll()

        XCTAssertEqual(seenRemote, true, "tick should receive the snapshot's remotePTT")
        XCTAssertEqual(seenRx, -33, "tick should receive the snapshot's rxDB")
        XCTAssertEqual(fake.pttCalls, [true], "a non-nil tick result drives setPTT")
    }

    func testPttSourceTickReturningNilDoesNotCallSetPTT() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        session.pttSourceTick = { _, _ in nil }

        session.poll()

        XCTAssertTrue(fake.pttCalls.isEmpty, "a nil tick result must not call setPTT")
    }

    func testPttSourceTickFalseUnkeys() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        session.pttSourceTick = { _, _ in false }

        session.poll()

        XCTAssertEqual(fake.pttCalls, [false], "a false tick result unkeys via setPTT")
    }

    func testPollWithoutPttSourceLeavesPTTUntouched() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        session.poll()

        XCTAssertTrue(fake.pttCalls.isEmpty, "no source means setPTT is never called from poll")
    }

    // MARK: - Voice compression + noise reduction

    func testSetCompressionPassesThroughAndPublishes() throws {
        let fake = FakeStation()
        let store = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: store)

        session.setCompression(true)

        XCTAssertEqual(fake.compressionCalls, [true], "setCompression hits the station")
        XCTAssertTrue(session.compression, "and publishes the flag")
        XCTAssertTrue(store.settings.compression, "and persists it")
    }

    func testSetCompressionLevelPassesThroughAndPublishes() throws {
        let fake = FakeStation()
        let store = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: store)

        session.setCompressionLevel(0.5)

        XCTAssertEqual(fake.compressionLevelCalls, [0.5], "setCompressionLevel hits the station")
        XCTAssertEqual(session.compressionLevel, 0.5, "and publishes the level")
        XCTAssertEqual(store.settings.compressionLevel, 0.5, "and persists it")
    }

    func testApplyAudioSettingsAppliesCompressionLevel() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())

        session.applyAudioSettings(AudioSettings(compressionLevel: 0.7))

        XCTAssertTrue(
            fake.compressionLevelCalls.contains(0.7), "restore pushes the level to the station")
        XCTAssertEqual(session.compressionLevel, 0.7, "and reflects it in published state")
    }

    func testSetTxTrimPassesThroughAndPublishes() throws {
        let fake = FakeStation()
        let store = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: store)

        session.setTxTrim(0.4)

        XCTAssertEqual(fake.txTrimCalls, [0.4], "setTxTrim hits the station")
        XCTAssertEqual(session.txTrim, 0.4, "and publishes the gain")
        XCTAssertEqual(store.settings.txTrim, 0.4, "and persists it")
    }

    func testApplyAudioSettingsAppliesTxTrim() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())

        session.applyAudioSettings(AudioSettings(txTrim: 0.6))

        XCTAssertTrue(
            fake.txTrimCalls.contains(0.6), "restore pushes the trim to the station")
        XCTAssertEqual(session.txTrim, 0.6, "and reflects it in published state")
    }

    func testSetNoiseReductionPassesThroughAndPublishes() throws {
        let fake = FakeStation()
        let store = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: store)

        session.setNoiseReduction(true)

        XCTAssertEqual(fake.noiseReductionCalls, [true])
        XCTAssertTrue(session.noiseReduction)
        XCTAssertTrue(store.settings.noiseReduction)
    }

    func testSetRxCompressionPassesThroughAndPublishes() throws {
        let fake = FakeStation()
        let store = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: store)

        session.setRxCompression(true)

        XCTAssertEqual(fake.rxCompressionCalls, [true], "setRxCompression hits the station")
        XCTAssertTrue(session.rxCompression, "and publishes the flag")
        XCTAssertTrue(store.settings.rxCompression, "and persists it")
    }

    func testSetRxCompressionLevelPassesThroughAndPublishes() throws {
        let fake = FakeStation()
        let store = MemoryAudioStore()
        let session = CallSession(station: fake, audioStore: store)

        session.setRxCompressionLevel(0.5)

        XCTAssertEqual(
            fake.rxCompressionLevelCalls, [0.5], "setRxCompressionLevel hits the station")
        XCTAssertEqual(session.rxCompressionLevel, 0.5, "and publishes the level")
        XCTAssertEqual(store.settings.rxCompressionLevel, 0.5, "and persists it")
    }

    func testApplyAudioSettingsAppliesRxCompression() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        session.applyAudioSettings(
            AudioSettings(rxCompression: true, rxCompressionLevel: 0.65)
        )

        XCTAssertEqual(fake.rxCompressionCalls, [true], "applyAudioSettings pushes rx compression")
        XCTAssertTrue(
            fake.rxCompressionLevelCalls.contains(0.65), "and its strength")
        XCTAssertTrue(session.rxCompression)
        XCTAssertEqual(session.rxCompressionLevel, 0.65)
    }

    func testTogglingOneProcessingFlagPreservesOthers() throws {
        // Read-modify-write: toggling compression must not clobber a persisted
        // device/gain or the noise-reduction flag.
        let store = MemoryAudioStore()
        store.settings = AudioSettings(input: "UCI150", inputGain: 1.5, noiseReduction: true)
        let session = CallSession(station: FakeStation(), audioStore: store)

        session.setCompression(true)

        XCTAssertEqual(store.settings.input, "UCI150")
        XCTAssertEqual(store.settings.inputGain, 1.5)
        XCTAssertTrue(store.settings.noiseReduction, "other toggle preserved")
        XCTAssertTrue(store.settings.compression)
    }

    func testApplyAudioSettingsAppliesProcessingFlags() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        session.applyAudioSettings(
            AudioSettings(compression: true, noiseReduction: true, voxEnabled: true)
        )

        XCTAssertEqual(fake.compressionCalls, [true], "applyAudioSettings pushes compression")
        XCTAssertEqual(fake.noiseReductionCalls, [true], "and noise reduction")
        XCTAssertTrue(session.compression)
        XCTAssertTrue(session.noiseReduction)
        XCTAssertTrue(session.voxEnabled)
    }

    // MARK: - VOX (CallSession integration)

    func testVoxKeysWhenLevelAboveThresholdInSnapshot() throws {
        // With VOX enabled, a snapshot mic level above threshold should drive
        // setPTT(true) from poll().
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -10, rxDB: -60, inputDB: -10, rttMS: 20  // -10 > -40 threshold
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)

        session.poll()

        XCTAssertEqual(fake.pttCalls, [true], "loud mic with VOX on keys the radio")
    }

    func testVoxKeysFromInputLevelWhileTxFloored() throws {
        // The iax-5c30 fix: VOX keys off the continuous mic input level, so it
        // triggers even though txDB is floored (post-gate, unkeyed) — the
        // chicken-and-egg that previously made VOX never fire.
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, inputDB: -10, rttMS: 20  // txDB floored, mic loud
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)

        session.poll()

        XCTAssertEqual(fake.pttCalls, [true], "VOX keys from inputDB even when txDB is floored")
    }

    func testSetVoxThresholdPublishesAndPersists() {
        let store = MemoryAudioStore()
        let session = CallSession(station: FakeStation(), audioStore: store)
        session.setVoxThreshold(-28)
        XCTAssertEqual(session.voxThresholdDBFS, -28)
        XCTAssertEqual(store.settings.voxThresholdDBFS, -28)
    }

    func testRaisingVoxThresholdStopsKeyingQuieterMic() {
        // Default −40 would key a −10 mic; raising the threshold to −5 closes the
        // gate, proving the slider value reaches the VoxGate.
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, inputDB: -10, rttMS: 20)
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxThreshold(-5)
        session.setVoxEnabled(true)
        session.poll()
        XCTAssertTrue(fake.pttCalls.isEmpty, "mic below the raised VOX threshold doesn't key")
    }

    func testApplyAudioSettingsRestoresVoxThreshold() {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, inputDB: -10, rttMS: 20)
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.applyAudioSettings(AudioSettings(voxEnabled: true, voxThresholdDBFS: -5))
        XCTAssertEqual(session.voxThresholdDBFS, -5)
        session.poll()
        XCTAssertTrue(fake.pttCalls.isEmpty, "the restored threshold applies to the gate")
    }

    func testSetVoxHangtimePublishesAppliesToGateAndPersists() {
        let store = MemoryAudioStore()
        let session = CallSession(station: FakeStation(), audioStore: store)
        session.setVoxHangtime(900)
        XCTAssertEqual(session.voxHangtimeMS, 900)
        XCTAssertEqual(session.voxGate.config.hangoverMS, 900)
        XCTAssertEqual(store.settings.voxHangtimeMS, 900)
    }

    func testApplyAudioSettingsRestoresVoxHangtime() {
        let session = CallSession(station: FakeStation(), audioStore: MemoryAudioStore())
        session.applyAudioSettings(AudioSettings(voxHangtimeMS: 900))
        XCTAssertEqual(session.voxHangtimeMS, 900)
        XCTAssertEqual(session.voxGate.config.hangoverMS, 900)
    }

    func testInputDBPublishedFromSnapshot() {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, inputDB: -22, rttMS: 20)
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.poll()
        XCTAssertEqual(session.meters.inputDB, -22)
    }

    func testVoxDoesNothingWhenDisabled() throws {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -10, rxDB: -60, rttMS: 20
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())

        session.poll()

        XCTAssertTrue(fake.pttCalls.isEmpty, "VOX off → poll never keys from mic level")
    }

    func testDisablingVoxUnkeysIfItWasHolding() throws {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -10, rxDB: -60, inputDB: -10, rttMS: 20
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)
        session.poll()  // keys: [true]

        session.setVoxEnabled(false)  // should fail-safe unkey

        XCTAssertEqual(fake.pttCalls, [true, false], "turning VOX off releases the key")
    }

    // MARK: - Full / half duplex

    func testHalfDuplexInhibitsVoxWhileReceiving() throws {
        // Default half-duplex: VOX must NOT key while receiving (speaker bleed,
        // e.g. the parrot's playback, would otherwise feed back).
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -20, inputDB: -10, rttMS: 20  // loud mic, but RX active
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)

        session.poll()

        XCTAssertFalse(fake.pttCalls.contains(true), "half-duplex blocks VOX while receiving")
    }

    func testFullDuplexAllowsVoxWhileReceiving() throws {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -20, inputDB: -10, rttMS: 20
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)
        session.setFullDuplex(true)

        session.poll()

        XCTAssertTrue(
            fake.pttCalls.contains(true), "full duplex lets VOX key while receiving (headphones)")
    }

    func testHalfDuplexReleasesVoxKeyWhenReceivingStarts() throws {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(  // loud mic, no RX → VOX keys
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, inputDB: -10, rttMS: 20
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)
        session.poll()  // keys: [true]

        fake.snapshotToReturn = CallSnapshot(  // RX now active
            status: .answered, ptt: false, remotePTT: true,
            txDB: -60, rxDB: -20, inputDB: -10, rttMS: 20
        )
        session.poll()

        XCTAssertEqual(fake.pttCalls.last, false, "RX starting releases the VOX key in half-duplex")
    }

    func testSetFullDuplexPublishesAndPersists() throws {
        let store = MemoryAudioStore()
        let session = CallSession(station: FakeStation(), audioStore: store)

        session.setFullDuplex(true)

        XCTAssertTrue(session.fullDuplex)
        XCTAssertTrue(store.settings.fullDuplex)
    }

    // MARK: - Listen-only (Disable TX)

    func testSetTxDisabledPublishesAndPersists() throws {
        let store = MemoryAudioStore()
        let session = CallSession(station: FakeStation(), audioStore: store)

        session.setTxDisabled(true)

        XCTAssertTrue(session.txDisabled, "publishes the flag")
        XCTAssertTrue(store.settings.txDisabled, "and persists it")
    }

    func testTxDisabledBlocksManualPTT() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setTxDisabled(true)

        try session.setPTT(true)

        XCTAssertFalse(fake.pttCalls.contains(true), "listen-only must never key the radio")
    }

    func testTxDisabledBlocksVoxKeying() throws {
        // Loud mic + VOX on, but listen-only is set: poll must not key.
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -10, rxDB: -60, rttMS: 20  // -10 > -40 threshold
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)
        session.setTxDisabled(true)

        session.poll()

        XCTAssertFalse(fake.pttCalls.contains(true), "VOX cannot key while listen-only")
    }

    func testTxDisabledBlocksSerialTick() throws {
        // A serial source asking to key must be suppressed while listen-only.
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setTxDisabled(true)
        session.pttSourceTick = { _, _ in true }

        session.poll()

        XCTAssertFalse(fake.pttCalls.contains(true), "serial handset cannot key while listen-only")
    }

    func testEnablingTxDisabledFailSafeUnkeys() throws {
        // VOX is holding the key; enabling listen-only must release it.
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -10, rxDB: -60, inputDB: -10, rttMS: 20
        )
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setVoxEnabled(true)
        session.poll()  // keys: [true]

        session.setTxDisabled(true)

        XCTAssertEqual(fake.pttCalls.last, false, "enabling listen-only releases any held key")
        XCTAssertTrue(fake.pttCalls.contains(true), "VOX should have keyed first (from inputDB)")
    }

    func testTxReEnabledAllowsPTTAgain() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.setTxDisabled(true)
        session.setTxDisabled(false)

        try session.setPTT(true)

        XCTAssertEqual(fake.pttCalls.last, true, "turning TX back on restores keying")
    }

    func testApplyAudioSettingsAppliesTxDisabled() throws {
        let session = CallSession(station: FakeStation())

        session.applyAudioSettings(AudioSettings(txDisabled: true))

        XCTAssertTrue(session.txDisabled)
    }

    // MARK: - Half-duplex RX suppression (astar-eaab)

    /// Keying PTT while NOT full-duplex hard-mutes RX (gain → 0) so the local
    /// node's TX-return echo isn't heard; unkeying restores the user's gain.
    func testHalfDuplexMutesRXWhileKeyedAndRestoresOnUnkey() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        // Half-duplex (default) with a non-unity output gain to prove the exact
        // restore value, applied through the normal settings path.
        session.applyAudioSettings(AudioSettings(outputGain: 0.75, fullDuplex: false))
        let baseline = fake.outputGainCalls.count  // applyAudioSettings pushed 0.75

        // Key: a snapshot with ptt == true triggers the mute edge.
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: true, remotePTT: false,
            txDB: -10, rxDB: -20, rttMS: 30)
        session.poll()
        XCTAssertTrue(session.rxMutedForHalfDuplex, "keyed half-duplex must suppress RX")
        XCTAssertEqual(fake.outputGainCalls.last, 0, "RX is muted (gain 0) while keyed")

        // Polling again while still keyed must NOT re-push the gain (edge-only).
        let afterMute = fake.outputGainCalls.count
        session.poll()
        XCTAssertEqual(
            fake.outputGainCalls.count, afterMute,
            "no gain change should be pushed while PTT stays keyed")

        // Unkey: ptt == false fires the restore edge back to the user's 0.75.
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -20, rttMS: 30)
        session.poll()
        XCTAssertFalse(session.rxMutedForHalfDuplex, "unkey releases the RX suppression")
        XCTAssertEqual(fake.outputGainCalls.last, 0.75, "RX restored to the user's gain")
        XCTAssertGreaterThan(fake.outputGainCalls.count, baseline)
    }

    /// Full-duplex keeps RX untouched while keyed — simultaneous TX+RX is the
    /// whole point of full-duplex (headphones).
    func testFullDuplexDoesNotMuteRXWhileKeyed() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.applyAudioSettings(AudioSettings(outputGain: 1.0, fullDuplex: true))
        let baseline = fake.outputGainCalls.count

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: true, remotePTT: false,
            txDB: -10, rxDB: -20, rttMS: 30)
        session.poll()

        XCTAssertFalse(session.rxMutedForHalfDuplex, "full-duplex never suppresses RX")
        XCTAssertEqual(
            fake.outputGainCalls.count, baseline,
            "full-duplex keying pushes no output-gain change")
    }

    /// Adjusting the volume slider while RX is muted mid-transmit must NOT
    /// un-mute (that would reintroduce the echo); the new value is remembered and
    /// restored on unkey.
    func testSetOutputGainWhileMutedDefersUntilUnkey() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.applyAudioSettings(AudioSettings(outputGain: 1.0, fullDuplex: false))

        // Key → RX muted to 0.
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: true, remotePTT: false,
            txDB: -10, rxDB: -20, rttMS: 30)
        session.poll()
        XCTAssertEqual(fake.outputGainCalls.last, 0)

        // Move the volume slider mid-transmit: must not push to the station yet.
        let beforeSlider = fake.outputGainCalls.count
        try session.setOutputGain(0.5)
        XCTAssertEqual(
            fake.outputGainCalls.count, beforeSlider,
            "changing gain while muted must not un-mute the station")

        // Unkey → restores the NEW value, not the old one.
        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -20, rttMS: 30)
        session.poll()
        XCTAssertEqual(fake.outputGainCalls.last, 0.5, "unkey restores the latest gain")
    }

    /// Turning full-duplex ON mid-transmit restores RX immediately, without
    /// waiting for the next poll, so headphone users hear the far end at once.
    func testEnablingFullDuplexWhileMutedRestoresRXImmediately() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.applyAudioSettings(AudioSettings(outputGain: 0.8, fullDuplex: false))

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: true, remotePTT: false,
            txDB: -10, rxDB: -20, rttMS: 30)
        session.poll()
        XCTAssertTrue(session.rxMutedForHalfDuplex)
        XCTAssertEqual(fake.outputGainCalls.last, 0)

        session.setFullDuplex(true)

        XCTAssertFalse(session.rxMutedForHalfDuplex, "enabling full-duplex releases the mute")
        XCTAssertEqual(fake.outputGainCalls.last, 0.8, "RX restored to the user's gain at once")
    }

    /// A hangup (or any idle snapshot with ptt == false) while muted self-heals:
    /// the next poll's unkey edge restores RX, so a later call isn't left silent.
    func testHangupWhileMutedRestoresRXOnNextPoll() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, audioStore: MemoryAudioStore())
        session.applyAudioSettings(AudioSettings(outputGain: 1.0, fullDuplex: false))

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: true, remotePTT: false,
            txDB: -10, rxDB: -20, rttMS: 30)
        session.poll()
        XCTAssertTrue(session.rxMutedForHalfDuplex)

        // Call drops: idle snapshot, ptt false.
        fake.snapshotToReturn = .idle
        session.poll()

        XCTAssertFalse(session.rxMutedForHalfDuplex, "a dropped call releases the RX mute")
        XCTAssertEqual(fake.outputGainCalls.last, 1.0, "RX gain restored after the call drops")
    }

    /// Integration smoke over the REAL Station (not the fake): construct via the
    /// live factory, poll, and confirm it reflects the idle resting state. Proves
    /// the Station→CallSnapshot adapter and the binding link end to end.
    func testLiveSessionReflectsRealStationIdle() throws {
        let session = CallSession.live()
        session.poll()
        XCTAssertEqual(session.status, .idle, "a freshly constructed station has no call")
    }

    // MARK: - No-answer dial failure (astar-9f48)

    /// A dial that goes out (`.dialing` observed) and falls back to hangup
    /// without ever answering publishes the no-answer message — the core
    /// reports the give-up as a plain `.hangup`, so this edge is the only
    /// signal the user gets.
    func testDialThatNeverAnswersPublishesNoAnswerFailure() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "77777")

        fake.snapshotToReturn = CallSnapshot(
            status: .dialing, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, rttMS: nil)
        session.poll()
        XCTAssertNil(session.lastDialFailure, "no failure while still dialing")

        fake.snapshotToReturn = CallSnapshot(
            status: .hangup, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, rttMS: nil)
        session.poll()

        XCTAssertEqual(session.lastDialFailure, "Node 77777 didn’t answer.")
    }

    /// A call that answered and later hung up (normal call end) publishes
    /// nothing — the no-answer edge requires never reaching `.answered`.
    func testAnsweredCallEndingPublishesNoFailure() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "77777")

        for status: IaxStatus in [.dialing, .answered, .hangup] {
            fake.snapshotToReturn = CallSnapshot(
                status: status, ptt: false, remotePTT: false,
                txDB: -60, rxDB: -60, rttMS: nil)
            session.poll()
        }

        XCTAssertNil(session.lastDialFailure, "a completed call is not a dial failure")
    }

    /// The user hanging up mid-dial is intentional — no failure message, even
    /// though the status falls back to hangup without answering.
    func testUserDisconnectDuringDialingPublishesNoFailure() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "77777")
        fake.snapshotToReturn = CallSnapshot(
            status: .dialing, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, rttMS: nil)
        session.poll()

        try session.disconnect()
        fake.snapshotToReturn = .idle
        session.poll()

        XCTAssertNil(session.lastDialFailure, "user-initiated hangup must not report no-answer")
    }

    /// A fresh dial clears a stale no-answer message so the banner doesn't
    /// linger over the new attempt.
    func testNewDialClearsStaleNoAnswerFailure() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake, hasCredentials: true)
        try session.connect(node: "77777")
        fake.snapshotToReturn = CallSnapshot(
            status: .dialing, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, rttMS: nil)
        session.poll()
        fake.snapshotToReturn = .idle
        session.poll()
        XCTAssertNotNil(session.lastDialFailure, "precondition: the first dial failed")

        try session.connect(node: "55553")

        XCTAssertNil(session.lastDialFailure, "a new dial attempt clears the stale message")
    }

    // MARK: - sendDTMF passthrough (astar-b74d)

    /// `sendDTMF` forwards the exact digit to the station, once per call — the
    /// dialpad's connected-mode "one tone per tap" path.
    func testSendDTMFForwardsDigitToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.sendDTMF("3")
        try session.sendDTMF("*")

        XCTAssertEqual(fake.dtmfCalls, ["3", "*"], "each tap sends one tone, in order")
    }

    /// The full 16-key set: the A-D column forwards the exact uppercase
    /// character (iax-47ae loosened the binding to accept A-D), unchanged by the
    /// passthrough.
    func testSendDTMFForwardsLetterKeysVerbatim() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        for key: Character in ["A", "B", "C", "D"] {
            try session.sendDTMF(key)
        }

        XCTAssertEqual(
            fake.dtmfCalls, ["A", "B", "C", "D"],
            "A-D are forwarded uppercase, verbatim")
    }

    /// A station-side failure (e.g. idle / invalid digit) propagates so the UI
    /// can degrade gracefully rather than silently swallowing it.
    func testSendDTMFPropagatesStationError() {
        let fake = FakeStation()
        struct Boom: Error {}
        fake.dtmfError = Boom()
        let session = CallSession(station: fake)

        XCTAssertThrowsError(try session.sendDTMF("9"))
        XCTAssertTrue(fake.dtmfCalls.isEmpty, "a throwing send records no tone")
    }

    // MARK: - sendDTMF(sequence:) / cancelDTMF passthrough (astar-7d21)

    /// `sendDTMF(sequence:)` forwards the exact command to the station, once —
    /// the compose-then-send dialpad's Send path.
    func testSendDTMFSequenceForwardsToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.sendDTMF(sequence: "*3546054")

        XCTAssertEqual(fake.sequenceCalls, ["*3546054"], "one Send = one engine call")
    }

    /// A station-side failure (idle, busy, invalid digit) propagates so the UI
    /// can keep the composed command in the field instead of losing it.
    func testSendDTMFSequencePropagatesStationError() {
        let fake = FakeStation()
        struct Boom: Error {}
        fake.dtmfError = Boom()
        let session = CallSession(station: fake)

        XCTAssertThrowsError(try session.sendDTMF(sequence: "*3"))
        XCTAssertTrue(fake.sequenceCalls.isEmpty, "a throwing send records no command")
    }

    /// `cancelDTMF` forwards to the station — the Stop button's path.
    func testCancelDTMFForwardsToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        try session.cancelDTMF()

        XCTAssertEqual(fake.cancelDTMFCalls, 1)
    }

    /// The snapshot's sequence progress lands on the session's published
    /// `dtmfPlayed`/`dtmfTotal` so the popover can dim played digits and detect
    /// completion (total returning to 0).
    func testPollPublishesDTMFProgressFromSnapshot() {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, rttMS: nil, dtmfPlayed: 3, dtmfTotal: 8)
        session.poll()
        XCTAssertEqual(session.dtmfPlayed, 3)
        XCTAssertEqual(session.dtmfTotal, 8)

        fake.snapshotToReturn = CallSnapshot(
            status: .answered, ptt: false, remotePTT: false,
            txDB: -60, rxDB: -60, rttMS: nil)
        session.poll()
        XCTAssertEqual(session.dtmfPlayed, 0, "queue done → progress clears")
        XCTAssertEqual(session.dtmfTotal, 0)
    }

    // MARK: - setSpectrumDecay passthrough (astar-68a6)

    /// `setSpectrumDecay` forwards the exact dB/s value to the station — the
    /// Settings "Spectrum decay" slider's live path.
    func testSetSpectrumDecayForwardsToStation() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)

        session.setSpectrumDecay(75)
        session.setSpectrumDecay(150)

        XCTAssertEqual(
            fake.spectrumDecayCalls, [75, 150], "each call forwards its value, in order")
    }

    /// `NullStation.setSpectrumDecay` is a no-op (no engine) — the app's fallback
    /// station must accept the call without throwing.
    func testSetSpectrumDecayOnNullStationIsNoOp() throws {
        let station = NullStation()
        XCTAssertNoThrow(try station.setSpectrumDecay(dbPerSecond: 100))
    }

    // MARK: - M17 callsign persistence + prefill (astar-c2e5/iax-f2b8 Task 8)

    private func scratchM17Defaults() -> UserDefaults {
        let suite = "astar.tests.m17callsign.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    func testM17CallsignLoadsSavedValue() {
        let defaults = scratchM17Defaults()
        defaults.set("W1AW", forKey: "m17.callsign")

        let session = CallSession(station: FakeStation(), userDefaults: defaults)

        XCTAssertEqual(session.m17Callsign, "W1AW")
    }

    func testM17CallsignPersistsOnChange() {
        let defaults = scratchM17Defaults()
        let session = CallSession(station: FakeStation(), userDefaults: defaults)

        session.m17Callsign = "AJ7HR"

        XCTAssertEqual(defaults.string(forKey: "m17.callsign"), "AJ7HR")
    }

    func testM17CallsignPrefillsFromPortalUserWhenNothingSaved() {
        let defaults = scratchM17Defaults()
        let creds = Credentials(portalUser: "AJ7HR", portalPass: "pw", portalNode: "77777")

        let session = CallSession(
            station: FakeStation(), credentials: creds, userDefaults: defaults)

        XCTAssertEqual(session.m17Callsign, "AJ7HR")
    }

    func testM17CallsignDoesNotPrefillWhenAlreadySaved() {
        // A saved value (even one the user cleared to something else) always
        // wins over the prefill.
        let defaults = scratchM17Defaults()
        defaults.set("N7XYZ", forKey: "m17.callsign")
        let creds = Credentials(portalUser: "AJ7HR", portalPass: "pw", portalNode: "77777")

        let session = CallSession(
            station: FakeStation(), credentials: creds, userDefaults: defaults)

        XCTAssertEqual(session.m17Callsign, "N7XYZ")
    }

    func testM17CallsignPrefillSkipsWhenPortalUserDoesntLookLikeACallsign() {
        let defaults = scratchM17Defaults()
        let creds = Credentials(portalUser: "myallstarlogin", portalPass: "pw", portalNode: "1")

        let session = CallSession(
            station: FakeStation(), credentials: creds, userDefaults: defaults)

        XCTAssertEqual(session.m17Callsign, "")
    }

    func testM17CallsignPrefillTable() {
        // Table tests for the pure helper: `^[A-Za-z]{1,2}[0-9][A-Za-z]{1,3}$`,
        // case-insensitive, uppercased on match.
        let cases: [(String?, String?)] = [
            ("AJ7HR", "AJ7HR"),
            ("aj7hr", "AJ7HR"),
            ("W1AW", "W1AW"),
            ("w1aw", "W1AW"),
            ("K1A", "K1A"),
            ("N7XYZ", "N7XYZ"),
            ("KI7ABCD", nil),  // too many trailing letters
            ("1AW", nil),  // must start with a letter
            ("ABCD1EF", nil),  // too many leading letters
            ("AJHR", nil),  // no digit at all
            ("", nil),
            (nil, nil),
            ("my-allstar-login", nil),
        ]
        for (input, expected) in cases {
            XCTAssertEqual(
                CallSession.callsignPrefill(from: input), expected,
                "callsignPrefill(from: \(input ?? "nil"))")
        }
    }
}
