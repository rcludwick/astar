// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// BLEButtonMachineTests.swift — the Bluetooth button machine and device table
// through the Swift wrapper.
//
// Pure: the machine is the Rust ButtonMachine behind an opaque handle, with
// time passed in. No CoreBluetooth, no radio, no station.

import CAstarStation
import XCTest

@testable import AstarStation

final class BLEButtonMachineTests: XCTestCase {

    private var b02ptt: BLEButtonDevice {
        get throws { try XCTUnwrap(BLEButtonDevices.device(id: "inrico-b02ptt")) }
    }

    // MARK: Device table

    func testDeviceTableHasExactlyTheB02PTTRow() {
        XCTAssertEqual(
            BLEButtonDevices.all,
            [
                BLEButtonDevice(
                    index: 0, id: "inrico-b02ptt", displayName: "Inrico B02PTT",
                    namePrefix: "B02PTT",
                    serviceUUID: "89A8591D-BB19-485B-9F59-58492BC33E24",
                    characteristicUUID: "894C8042-E841-461C-A5C9-5A73D25DB08E",
                    kind: .speakerMic, liveness: .rssi,
                    verified: "Inrico B02PTT GATT probe, 2026-10-08")
            ])
    }

    func testDeviceLookupByID() throws {
        XCTAssertEqual(try b02ptt.index, 0)
        XCTAssertNil(BLEButtonDevices.device(id: "inrico-b02"))
        XCTAssertNil(BLEButtonDevices.device(id: ""))
    }

    func testMatchAdvertisedName() throws {
        XCTAssertEqual(BLEButtonDevices.match(advertisedName: "B02PTT-6522"), try b02ptt)
        XCTAssertEqual(BLEButtonDevices.match(advertisedName: "B02PTT"), try b02ptt)
        XCTAssertNil(BLEButtonDevices.match(advertisedName: "b02ptt-1"), "case-sensitive")
        XCTAssertNil(BLEButtonDevices.match(advertisedName: "xB02PTT"), "prefix only")
        XCTAssertNil(BLEButtonDevices.match(advertisedName: "B02PT"))
        XCTAssertNil(BLEButtonDevices.match(advertisedName: nil), "no name never matches")
    }

    func testAcceptsGATTOnlyWithNotifyOnTheRightUUIDs() throws {
        let d = try b02ptt
        let service = "89A8591D-BB19-485B-9F59-58492BC33E24"
        let characteristic = "894C8042-E841-461C-A5C9-5A73D25DB08E"
        XCTAssertTrue(
            BLEButtonDevices.acceptsGATT(
                d, service: service, characteristic: characteristic, properties: 0x10))
        XCTAssertTrue(
            BLEButtonDevices.acceptsGATT(
                d, service: service.lowercased(), characteristic: characteristic.lowercased(),
                properties: 0x12), "lowercase UUIDs, NOTIFY among other bits")
        XCTAssertFalse(
            BLEButtonDevices.acceptsGATT(
                d, service: service, characteristic: characteristic, properties: 0x02),
            "no NOTIFY")
        XCTAssertFalse(
            BLEButtonDevices.acceptsGATT(
                d, service: "0000180F-0000-1000-8000-00805F9B34FB",
                characteristic: characteristic, properties: 0x10), "wrong service")
        XCTAssertFalse(
            BLEButtonDevices.acceptsGATT(
                d, service: characteristic, characteristic: service, properties: 0x10),
            "service and characteristic swapped")
    }

    // MARK: Config

    func testDefaultConfigIsTheEngineDefault() {
        XCTAssertEqual(
            BLEButtonMachine.defaultConfig,
            BLEMachineConfig(
                settleMS: 300, probeKeyedMS: 500, probeMissesDead: 2, probeIdleMS: 5_000,
                leaseTTLMS: 1_500, renewMS: 250, bandStrongDBM: -65, bandFairDBM: -80))
    }

    // MARK: Machine

    /// Enable confirmed, adapter on, connect, accept and subscribe epoch 1.
    private func readyMachine() throws -> BLEButtonMachine {
        let m = try BLEButtonMachine(device: try b02ptt)
        XCTAssertEqual(m.handle(.enable(confirmed: true), nowMS: 0).status, .searching)
        XCTAssertEqual(m.handle(.adapter(.on), nowMS: 1).status, .searching)
        XCTAssertEqual(m.handle(.connected(epoch: 1), nowMS: 10).status, .connecting)
        XCTAssertEqual(m.handle(.gattAccepted(epoch: 1), nowMS: 20).status, .connecting)
        XCTAssertEqual(m.handle(.subscribed(epoch: 1), nowMS: 30).status, .ready)
        return m
    }

    func testFreshMachineIsDisabled() throws {
        let m = try BLEButtonMachine(device: try b02ptt)
        XCTAssertEqual(m.output.status, .disabled)
        XCTAssertEqual(m.output.intent, BLEIntent.none)
        XCTAssertFalse(m.output.pressed)
    }

    func testRoundTripPressThenLinkLostReleases() throws {
        let m = try readyMachine()
        XCTAssertEqual(
            m.handle(.notify(epoch: 1, bytes: [0x01]), nowMS: 2_000),
            BLEMachineOutput(
                intent: .press, probe: false, dropLink: false, status: .ready, signal: nil,
                lastRSSI: nil, rawPressed: true, pressed: true))
        let lost = m.handle(.disconnected(epoch: 1), nowMS: 2_100)
        XCTAssertEqual(lost.intent, .release(.linkLost))
        XCTAssertEqual(lost.status, .reconnecting)
        XCTAssertFalse(lost.pressed)
        XCTAssertFalse(lost.probe)
        XCTAssertFalse(lost.dropLink)
        XCTAssertEqual(m.output.intent, BLEIntent.none, "output carries no intent")
        XCTAssertEqual(m.output.status, .reconnecting)
    }

    func testIdleDropRelinksReadyAndTheFirstPressKeys() throws {
        // The button was up at the loss, so nothing latches: the relink reads
        // Ready and the first press past the settle window keys. Only the
        // press itself keys; the relink never does.
        let m = try readyMachine()
        let lost = m.handle(.disconnected(epoch: 1), nowMS: 500)
        XCTAssertEqual(lost.intent, BLEIntent.none, "nothing was held")
        XCTAssertEqual(lost.status, .reconnecting)
        XCTAssertEqual(m.handle(.connected(epoch: 2), nowMS: 600).status, .connecting)
        XCTAssertEqual(m.handle(.gattAccepted(epoch: 2), nowMS: 610).status, .connecting)
        let back = m.handle(.subscribed(epoch: 2), nowMS: 620)
        XCTAssertEqual(back.status, .ready, "an idle drop arms no latch")
        XCTAssertEqual(back.intent, BLEIntent.none, "a relink never keys")
        XCTAssertFalse(back.pressed)
        let press = m.handle(.notify(epoch: 2, bytes: [0x01]), nowMS: 1_000)
        XCTAssertEqual(press.intent, .press)
        XCTAssertTrue(press.pressed)
    }

    func testHeldDropRelinksReleaseToArmUntilAFreshPress() throws {
        // The button was down at the loss, so the relink is latched: a press
        // reported after it is ignored until a live release re-arms.
        let m = try readyMachine()
        XCTAssertEqual(m.handle(.notify(epoch: 1, bytes: [0x01]), nowMS: 2_000).intent, .press)
        XCTAssertFalse(m.handle(.disconnected(epoch: 1), nowMS: 2_100).pressed)
        _ = m.handle(.connected(epoch: 2), nowMS: 2_200)
        _ = m.handle(.gattAccepted(epoch: 2), nowMS: 2_210)
        XCTAssertEqual(m.handle(.subscribed(epoch: 2), nowMS: 2_220).status, .releaseToArm)
        let held = m.handle(.notify(epoch: 2, bytes: [0x01]), nowMS: 2_600)
        XCTAssertEqual(held.intent, BLEIntent.none, "a held button never keys after a relink")
        XCTAssertFalse(held.pressed)
        XCTAssertEqual(held.status, .releaseToArm)
        XCTAssertEqual(m.handle(.notify(epoch: 2, bytes: [0x00]), nowMS: 2_700).status, .ready)
        let fresh = m.handle(.notify(epoch: 2, bytes: [0x01]), nowMS: 2_800)
        XCTAssertEqual(fresh.intent, .press)
        XCTAssertTrue(fresh.pressed)
    }

    func testButtonReleaseReleases() throws {
        let m = try readyMachine()
        XCTAssertEqual(m.handle(.notify(epoch: 1, bytes: [0x01]), nowMS: 2_000).intent, .press)
        let up = m.handle(.notify(epoch: 1, bytes: [0x00]), nowMS: 2_400)
        XCTAssertEqual(up.intent, .release(.button))
        XCTAssertFalse(up.pressed)
    }

    func testNineBytePayloadKeepsItsTrueLengthAndReleasesMalformed() throws {
        // The first byte is 0x01: only the true length (9) makes it malformed.
        let m = try readyMachine()
        XCTAssertEqual(m.handle(.notify(epoch: 1, bytes: [0x01]), nowMS: 2_000).intent, .press)
        let bad = m.handle(
            .notify(epoch: 1, bytes: [0x01, 0, 0, 0, 0, 0, 0, 0, 0]), nowMS: 2_050)
        XCTAssertEqual(bad.intent, .release(.malformed))
        XCTAssertFalse(bad.pressed)
    }

    func testEmptyPayloadReleasesMalformedAndLatches() throws {
        // A notify on the live epoch that is neither the press nor the
        // release byte latches and releases Malformed, pressed or not.
        let m = try readyMachine()
        let out = m.handle(.notify(epoch: 1, bytes: []), nowMS: 2_000)
        XCTAssertEqual(out.intent, .release(.malformed))
        XCTAssertEqual(out.status, .releaseToArm)
        XCTAssertFalse(out.pressed)
    }

    func testUnconfirmedNeverPresses() throws {
        let m = try BLEButtonMachine(device: try b02ptt)
        _ = m.handle(.enable(confirmed: false), nowMS: 0)
        _ = m.handle(.adapter(.on), nowMS: 1)
        _ = m.handle(.connected(epoch: 1), nowMS: 10)
        _ = m.handle(.gattAccepted(epoch: 1), nowMS: 20)
        XCTAssertEqual(m.handle(.subscribed(epoch: 1), nowMS: 30).status, .needsConfirm)
        let out = m.handle(.notify(epoch: 1, bytes: [0x01]), nowMS: 2_000)
        XCTAssertEqual(out.intent, BLEIntent.none)
        XCTAssertFalse(out.pressed)
    }

    func testAdapterStatesMapToTheirStatus() throws {
        let m = try BLEButtonMachine(device: try b02ptt)
        _ = m.handle(.enable(confirmed: true), nowMS: 0)
        XCTAssertEqual(m.handle(.adapter(.off), nowMS: 1).status, .bluetoothOff)
        XCTAssertEqual(m.handle(.adapter(.unauthorized), nowMS: 2).status, .unauthorized)
        XCTAssertEqual(m.handle(.adapter(.unsupported), nowMS: 3).status, .unsupported)
        XCTAssertEqual(m.handle(.adapter(.resetting), nowMS: 4).status, .bluetoothOff)
        XCTAssertEqual(m.handle(.adapter(.on), nowMS: 5).status, .searching)
    }

    func testProbeRepliesSetSignalBandsAtTheBoundaries() throws {
        let m = try readyMachine()
        let cases: [(Int16, BLESignalBand)] = [
            (-65, .strong), (-66, .fair), (-80, .fair), (-81, .weak),
        ]
        var now: UInt64 = 100
        for (rssi, band) in cases {
            let out = m.handle(.probeOK(epoch: 1, rssi: rssi), nowMS: now)
            XCTAssertEqual(out.signal, band, "rssi \(rssi)")
            XCTAssertEqual(out.lastRSSI, rssi)
            now += 100
        }
    }

    func testMachineIsReleasedWhenDropped() throws {
        // Only ARC is checked here: that nothing retains a machine. That
        // iax_bleptt_free frees the handle is pinned by astar-sys's ble_ptt
        // tests.
        weak var weakMachine: BLEButtonMachine?
        for _ in 0..<1_000 {
            let m = try BLEButtonMachine(device: try b02ptt)
            _ = m.handle(.enable(confirmed: true), nowMS: 0)
            weakMachine = m
        }
        XCTAssertNil(weakMachine, "the last machine was released")
    }

    // MARK: Fail-safe decoding

    private static let failSafe = BLEMachineOutput(
        intent: .release(.linkLost), probe: false, dropLink: true, status: .reconnecting,
        signal: nil, lastRSSI: nil, rawPressed: false, pressed: false)

    private static let poisonedState = BLEMachineOutput(
        intent: .none, probe: false, dropLink: false, status: .reconnecting,
        signal: nil, lastRSSI: nil, rawPressed: false, pressed: false)

    func testEngineErrorPoisonsTheMachineAndReleasesOnce() throws {
        let m = try readyMachine()
        XCTAssertEqual(m.handle(.notify(epoch: 1, bytes: [0x01]), nowMS: 2_000).intent, .press)
        XCTAssertTrue(m.output.pressed, "held before the error")

        // Kind 99 does not exist: the engine refuses it with IAX_ERR_INVALID (-26).
        var bad = IaxBleEvent()
        bad.kind = 99
        XCTAssertEqual(
            m.handle(rawEvent: bad, payload: [], nowMS: 2_050), Self.failSafe,
            "the poisoning call releases and drops the link, once")

        // Still held as far as the engine knows, but the machine is poisoned:
        // no new release, no new drop, never pressed.
        XCTAssertEqual(
            m.handle(.notify(epoch: 1, bytes: [0x01]), nowMS: 2_100), Self.poisonedState)
        XCTAssertEqual(m.handle(.tick, nowMS: 2_350), Self.poisonedState)
        XCTAssertEqual(m.output, Self.poisonedState, "output keeps intent none, no commands")
        XCTAssertEqual(BLEButtonMachine.poisonedState, Self.poisonedState)
    }

    func testEngineErrorFailsSafeNotPressed() {
        let pressed = BLEMachineOutput(
            intent: BLEIntent.none, probe: false, dropLink: false, status: .ready, signal: nil,
            lastRSSI: nil, rawPressed: true, pressed: true)
        XCTAssertEqual(BLEButtonMachine.failSafe, Self.failSafe)
        XCTAssertEqual(
            BLEButtonMachine.output(forCode: -1, decoded: pressed), Self.failSafe,
            "a caught panic must not keep reporting pressed")
        XCTAssertEqual(
            BLEButtonMachine.output(forCode: -26, decoded: pressed), Self.failSafe,
            "a refusal must not keep reporting pressed")
        XCTAssertEqual(
            BLEButtonMachine.output(forCode: 0, decoded: pressed), pressed,
            "IAX_OK passes the decoded output through")
    }

    func testUnknownIntentFailsTowardsRelease() {
        var raw = IaxBleOutput()
        raw.intent = 7
        raw.release_reason = 0
        raw.status = 99
        raw.pressed = true
        let out = BLEMachineOutput(raw: raw)
        XCTAssertEqual(out.intent, .release(.linkLost))
        XCTAssertEqual(out.status, .disabled)
        XCTAssertEqual(
            BLEMachineOutput(
                intentCode: 7, releaseReasonCode: 0, statusCode: 99, raw: IaxBleOutput()
            ).intent,
            .release(.linkLost))
    }
}
