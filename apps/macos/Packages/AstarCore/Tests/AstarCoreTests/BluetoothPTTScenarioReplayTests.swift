// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// Replays the same JSON traces the Rust crate replays
/// (`crates/astar-ble-ptt/scenarios`), through the controller's `feed`, the
/// real FFI machine and a real `CallSession` on an answered `FakeStation`.
/// Every expected Press must reach the station as one lease press and every
/// expected Release as one Bluetooth release, and nothing else may. Each
/// step also checks the published status, and, where the trace gives them,
/// `rawPressed`, `signal` and the forced-stop reason a keyed release
/// publishes. That pins the Swift path to the Rust machine: a trace cannot
/// pass in one language and fail in the other.
@MainActor
final class BluetoothPTTScenarioReplayTests: XCTestCase {

    /// Seven levels up from this file is the repository root.
    private static let scenarioDirectory: URL = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // Tests/AstarCoreTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // AstarCore
        .deletingLastPathComponent()  // Packages
        .deletingLastPathComponent()  // macos
        .deletingLastPathComponent()  // apps
        .deletingLastPathComponent()  // repository root
        .appendingPathComponent("crates/astar-ble-ptt/scenarios", isDirectory: true)

    private static let requiredNames: Set<String> = [
        "idle_disconnect_first_press_keys", "press_release_basic", "disconnect_mid_press",
        "resubscribe_while_held",
        "stale_notify_after_disconnect", "bt_off_mid_press", "sleep_mid_press", "malformed_value",
        "malformed_while_pressed_releases", "unconfirmed_never_presses", "rssi_two_misses_release",
        "rssi_reply_resets_misses", "idle_probe_misses_never_release",
        "notify_during_settle_latches",
        "notify_before_subscribe_ignored", "press_after_linklost_needs_release",
        "reconnect_does_not_clear_latch", "lease_lost_latches", "disable_mid_press",
        "gatt_rejected_lookalike", "unsubscribed_mid_press",
        "double_press_without_release_single_press", "adapter_unauthorized_status",
    ]

    private static let statuses: [String: BLELinkStatus] = [
        "disabled": .disabled, "bluetooth_off": .bluetoothOff, "unauthorized": .unauthorized,
        "unsupported": .unsupported, "searching": .searching, "connecting": .connecting,
        "not_supported_device": .notSupportedDevice, "needs_confirm": .needsConfirm,
        "ready": .ready, "release_to_arm": .releaseToArm, "reconnecting": .reconnecting,
    ]

    private static let reasons: [String: BLEReleaseReason] = [
        "button": .button, "malformed": .malformed, "link_lost": .linkLost,
        "adapter_off": .adapterOff, "disabled": .disabled, "sleep": .sleep,
    ]

    private static let signals: [String: BLESignalBand] = [
        "strong": .strong, "fair": .fair, "weak": .weak,
    ]

    private static let adapters: [String: BLEAdapterState] = [
        "on": .on, "off": .off, "unauthorized": .unauthorized, "resetting": .resetting,
        "unsupported": .unsupported,
    ]

    private struct Scenario: Decodable {
        let name: String
        let device: String
        let steps: [Step]

        struct Step: Decodable {
            let atMS: UInt64
            let event: Event
            let expect: Expect
            enum CodingKeys: String, CodingKey {
                case atMS = "at_ms"
                case event, expect
            }
        }

        struct Event: Decodable {
            let kind: String
            let confirmed: Bool?
            let state: String?
            let epoch: UInt32?
            let bytes: [Int]?
            let rssi: Int?
        }

        /// The optional keys are checked only when present, as the schema
        /// says. `signal` needs a hand-written decoder: an absent key means
        /// "not checked", while `null` means "no signal band".
        struct Expect: Decodable {
            let intent: String
            let reason: String?
            let status: String
            let pressed: Bool?
            let rawPressed: Bool?
            /// Required by the schema: the exact set of commands, a subset
            /// of `["probe", "drop_link"]`.
            let cmds: [String]
            let checksSignal: Bool
            let signal: String?

            enum CodingKeys: String, CodingKey {
                case intent, reason, status, pressed, signal, cmds
                case rawPressed = "raw_pressed"
            }

            init(from decoder: Decoder) throws {
                let c = try decoder.container(keyedBy: CodingKeys.self)
                intent = try c.decode(String.self, forKey: .intent)
                reason = try c.decodeIfPresent(String.self, forKey: .reason)
                status = try c.decode(String.self, forKey: .status)
                pressed = try c.decodeIfPresent(Bool.self, forKey: .pressed)
                rawPressed = try c.decodeIfPresent(Bool.self, forKey: .rawPressed)
                cmds = try c.decode([String].self, forKey: .cmds)
                checksSignal = c.contains(.signal)
                signal = try c.decodeIfPresent(String.self, forKey: .signal)
            }
        }
    }

    private struct UnknownKind: Error { let kind: String }

    /// Every key the shared schema allows. Any other key (a misspelt
    /// `"presssed"`, a stray event field) is a problem, so a typo in a trace
    /// can never switch a check off silently. The Rust replay applies the
    /// same rule.
    private static let stepKeys: Set<String> = ["at_ms", "event", "expect"]
    private static let expectKeys: Set<String> = [
        "intent", "reason", "status", "cmds", "pressed", "raw_pressed", "signal",
    ]
    private static let eventFields: [String: Set<String>] = [
        "enable": ["kind", "confirmed"], "adapter": ["kind", "state"],
        "notify": ["kind", "epoch", "bytes"], "probe_ok": ["kind", "epoch", "rssi"],
        "connected": ["kind", "epoch"], "gatt_accepted": ["kind", "epoch"],
        "gatt_rejected": ["kind", "epoch"], "subscribed": ["kind", "epoch"],
        "unsubscribed": ["kind", "epoch"], "probe_failed": ["kind", "epoch"],
        "disconnected": ["kind", "epoch"],
    ]

    /// The schema problems in one trace, in file order; `[]` when it is clean.
    static func schemaProblems(_ data: Data, name: String) throws -> [String] {
        guard let root = try JSONSerialization.jsonObject(with: data) as? [String: Any],
            let steps = root["steps"] as? [[String: Any]]
        else { return ["\(name): not a scenario object with steps"] }
        var problems = Set(root.keys).subtracting(["name", "device", "steps"]).sorted()
            .map { "\(name): unknown key \($0)" }
        for (index, step) in steps.enumerated() {
            let at = "\(name) step \(index)"
            problems += Set(step.keys).subtracting(stepKeys).sorted()
                .map { "\(at): unknown step key \($0)" }
            let event = step["event"] as? [String: Any] ?? [:]
            let kind = event["kind"] as? String ?? ""
            problems += Set(event.keys).subtracting(eventFields[kind] ?? ["kind"]).sorted()
                .map { "\(at): unknown \(kind) field \($0)" }
            let expect = step["expect"] as? [String: Any] ?? [:]
            problems += Set(expect.keys).subtracting(expectKeys).sorted()
                .map { "\(at): unknown expect key \($0)" }
        }
        return problems
    }

    private static func linkEvent(_ e: Scenario.Event) throws -> BLELinkEvent {
        switch e.kind {
        case "enable": return .enable(confirmed: try XCTUnwrap(e.confirmed))
        case "disable": return .disable
        case "confirmed": return .confirmed
        case "adapter": return .adapter(try XCTUnwrap(adapters[try XCTUnwrap(e.state)]))
        case "connected": return .connected(epoch: try XCTUnwrap(e.epoch))
        case "gatt_accepted": return .gattAccepted(epoch: try XCTUnwrap(e.epoch))
        case "gatt_rejected": return .gattRejected(epoch: try XCTUnwrap(e.epoch))
        case "subscribed": return .subscribed(epoch: try XCTUnwrap(e.epoch))
        case "unsubscribed": return .unsubscribed(epoch: try XCTUnwrap(e.epoch))
        case "notify":
            let bytes = try XCTUnwrap(e.bytes).map { try XCTUnwrap(UInt8(exactly: $0)) }
            return .notify(epoch: try XCTUnwrap(e.epoch), bytes: bytes)
        case "probe_ok":
            let rssi = try XCTUnwrap(Int16(exactly: try XCTUnwrap(e.rssi)))
            return .probeOK(epoch: try XCTUnwrap(e.epoch), rssi: rssi)
        case "probe_failed": return .probeFailed(epoch: try XCTUnwrap(e.epoch))
        case "disconnected": return .disconnected(epoch: try XCTUnwrap(e.epoch))
        case "sleep": return .sleep
        case "wake": return .wake
        case "lease_lost": return .leaseLost
        case "tick": return .tick
        default: throw UnknownKind(kind: e.kind)
        }
    }

    private func scenarioFiles() throws -> [URL] {
        try FileManager.default
            .contentsOfDirectory(at: Self.scenarioDirectory, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "json" }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
    }

    func testEveryScenarioFileIsFoundAndNamed() throws {
        let files = try scenarioFiles()
        XCTAssertGreaterThanOrEqual(files.count, 20, "read from \(Self.scenarioDirectory.path)")
        let names = Set(files.map { $0.deletingPathExtension().lastPathComponent })
        XCTAssertEqual(Self.requiredNames.subtracting(names), [])
    }

    func testAMisspeltKeyIsASchemaProblem() throws {
        let good = Data(
            """
            {"name": "x", "device": "inrico-b02ptt", "steps": [{"at_ms": 0,
             "event": {"kind": "enable", "confirmed": true},
             "expect": {"intent": "none", "status": "searching", "cmds": []}}]}
            """.utf8)
        XCTAssertEqual(try Self.schemaProblems(good, name: "x"), [])
        let typo = Data(
            """
            {"name": "x", "device": "inrico-b02ptt", "steps": [{"at_ms": 0,
             "event": {"kind": "enable", "confirmed": true, "epoch": 1},
             "expect": {"intent": "none", "status": "searching", "cmds": [],
                        "presssed": true}}]}
            """.utf8)
        XCTAssertEqual(
            try Self.schemaProblems(typo, name: "x"),
            ["x step 0: unknown enable field epoch", "x step 0: unknown expect key presssed"])
    }

    func testScenariosDriveTheStationExactlyAsTheMachineSays() throws {
        let files = try scenarioFiles()
        XCTAssertGreaterThanOrEqual(files.count, 20)
        for file in files {
            let data = try Data(contentsOf: file)
            XCTAssertEqual(try Self.schemaProblems(data, name: file.lastPathComponent), [])
            let scenario = try JSONDecoder().decode(Scenario.self, from: data)
            XCTAssertEqual(scenario.name, file.deletingPathExtension().lastPathComponent)

            let station = FakeStation()
            station.snapshotToReturn = answeredSnapshot()
            let defaults = bluetoothScratchDefaults(cleanup: self)
            let session = hermeticSession(station, defaults: defaults)
            session.poll()
            let clock = TestClock()
            let factory = CountingCentralFactory()
            let controller = BluetoothPTTController(
                centralFactory: { factory.make() },
                store: UserDefaultsBluetoothPTTLocalStore(defaults),
                ticker: ManualTicker(), clockMS: { clock.ms })
            // Unconfirmed attach: a fresh machine, no central, no ticker. The
            // trace itself carries its own `enable` event.
            controller.attach(session: session, spec: BluetoothPTTSpec(deviceID: scenario.device))

            var expected: [String] = []
            // Whether the machine holds the key, tracked from the trace: a
            // Press sets it; a Release or a LeaseLost event clears it. That is
            // exactly when the Rust machine's `pressed` changes.
            var keyed = false
            var forcedSeq = 0
            for step in scenario.steps {
                let at = "\(scenario.name) at \(step.atMS) ms"
                clock.ms = step.atMS
                controller.feed(try Self.linkEvent(step.event))
                let keyedBefore = keyed
                switch step.expect.intent {
                case "press":
                    expected.append(BLETestFixtures.leasePress)
                    keyed = true
                case "release":
                    expected.append(BLETestFixtures.release)
                    keyed = false
                    let reason = try XCTUnwrap(
                        Self.reasons[try XCTUnwrap(step.expect.reason, at)], at)
                    // Only a stop that cut a live over is news.
                    if keyedBefore, reason != .button, reason != .disabled {
                        forcedSeq += 1
                        XCTAssertEqual(controller.lastForcedStop?.reason, reason, at)
                    }
                case "none":
                    if step.event.kind == "lease_lost" { keyed = false }
                default:
                    XCTFail("\(at): unknown intent \(step.expect.intent)")
                }
                XCTAssertEqual(controller.lastForcedStop?.seq ?? 0, forcedSeq, at)
                XCTAssertEqual(controller.status, Self.statuses[step.expect.status], at)
                // Read the machine through the controller, never the trace:
                // these are the commands the transport would have carried out.
                let out = try XCTUnwrap(controller.lastOutput, "\(at): no machine output")
                let cmds = [out.probe ? "probe" : nil, out.dropLink ? "drop_link" : nil]
                    .compactMap { $0 }
                XCTAssertEqual(Set(cmds), Set(step.expect.cmds), at)
                if let pressed = step.expect.pressed {
                    XCTAssertEqual(out.pressed, pressed, at)
                }
                if let rawPressed = step.expect.rawPressed {
                    XCTAssertEqual(controller.rawPressed, rawPressed, at)
                }
                if step.expect.checksSignal {
                    let band = try step.expect.signal.map { try XCTUnwrap(Self.signals[$0], at) }
                    XCTAssertEqual(controller.signal, band, at)
                }
            }
            XCTAssertEqual(bluetoothKeyLog(station), expected, scenario.name)
            XCTAssertEqual(factory.made.count, 0, "\(scenario.name): replay never builds a central")
        }
    }
}
