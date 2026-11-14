// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// BLEButtonMachine.swift — the Bluetooth LE PTT button's state machine and
// device table, over the astar-sys `iax_bleptt_*` C-ABI.
//
// Every decision about a Bluetooth button is made in Rust, in a pure machine
// that takes events with the time passed in and returns an intent, a status
// and transport commands. Those decisions are whether a notify is a press,
// whether the link is dead and whether the button must be released before it
// re-arms. This file only translates. Swift never hard-codes a UUID or a byte
// value: both come from the engine's device table.

import CAstarStation

/// The Bluetooth adapter's state, as CoreBluetooth reports it.
public enum BLEAdapterState: UInt32, Sendable {
    case on = 0, off = 1, unauthorized = 2, resetting = 3, unsupported = 4
}

/// Why the machine released a held button.
public enum BLEReleaseReason: UInt32, Sendable {
    case button = 1, malformed = 2, linkLost = 3, adapterOff = 4, disabled = 5, sleep = 6
}

/// What the station must now be told.
public enum BLEIntent: Equatable, Sendable {
    case none, press, release(BLEReleaseReason)
}

/// The link's state for the status chip and the settings row. The only
/// meanings of each case are the engine's status derivation.
public enum BLELinkStatus: UInt32, Sendable {
    case disabled = 0, bluetoothOff, unauthorized, unsupported, searching, connecting,
        notSupportedDevice, needsConfirm, ready, releaseToArm, reconnecting
}

/// The signal band of the last RSSI reply.
public enum BLESignalBand: UInt32, Sendable { case strong = 1, fair = 2, weak = 3 }

/// Whether a button also carries a mic and speaker (over classic HFP).
public enum BLEDeviceKind: UInt32, Sendable { case speakerMic = 0, buttonOnly = 1 }

/// How the machine knows the link is alive while the button is held.
public enum BLELiveness: UInt32, Sendable { case none = 0, rssi = 1 }

/// One transport fact fed to the machine. Epochs are allocated by the client,
/// strictly increasing per connect attempt, and never 0. The machine ignores
/// anything from an epoch that is not live.
public enum BLELinkEvent: Equatable, Sendable {
    case enable(confirmed: Bool), disable, confirmed, adapter(BLEAdapterState)
    case connected(epoch: UInt32), gattAccepted(epoch: UInt32), gattRejected(epoch: UInt32)
    case subscribed(epoch: UInt32), unsubscribed(epoch: UInt32)
    case notify(epoch: UInt32, bytes: [UInt8]), probeOK(epoch: UInt32, rssi: Int16),
        probeFailed(epoch: UInt32)
    case disconnected(epoch: UInt32), sleep, wake, leaseLost, tick
}

/// The machine's answer to one event. `probe` and `dropLink` are the only
/// transport commands that exist: there is no write.
public struct BLEMachineOutput: Equatable, Sendable {
    public let intent: BLEIntent
    public let probe: Bool
    public let dropLink: Bool
    public let status: BLELinkStatus
    public let signal: BLESignalBand?
    public let lastRSSI: Int16?
    /// The last live notify was a press, whatever the gates said. For the
    /// self-test only, never for keying.
    public let rawPressed: Bool
    /// The machine holds the key, which drives lease renewals.
    public let pressed: Bool
}

/// The machine's timing and bands, from the engine's `MachineConfig::DEFAULT`.
public struct BLEMachineConfig: Equatable, Sendable {
    public let settleMS: UInt32, probeKeyedMS: UInt32, probeMissesDead: UInt32,
        probeIdleMS: UInt32
    public let leaseTTLMS: UInt32, renewMS: UInt32, bandStrongDBM: Int16, bandFairDBM: Int16
}

/// One supported button, a row of the engine's device table.
public struct BLEButtonDevice: Equatable, Hashable, Sendable {
    /// Its index in the engine's table, the handle the C-ABI takes.
    public let index: Int
    public let id: String
    public let displayName: String
    public let namePrefix: String
    /// The GATT service UUID, uppercase and hyphenated.
    public let serviceUUID: String
    /// The notify characteristic's UUID, uppercase and hyphenated.
    public let characteristicUUID: String
    public let kind: BLEDeviceKind
    public let liveness: BLELiveness
    /// Where the protocol facts were verified.
    public let verified: String
}

/// The engine's table of supported buttons, read once.
public enum BLEButtonDevices {
    /// Every supported button, in the engine's order.
    public static let all: [BLEButtonDevice] = {
        let count = Int(iax_bleptt_device_count())
        var rows: [BLEButtonDevice] = []
        for index in 0..<count {
            var raw = IaxBleDevice()
            guard iax_bleptt_device_at(UInt(index), &raw) == IAX_OK else { continue }
            rows.append(BLEButtonDevice(index: index, raw: raw))
        }
        return rows
    }()

    /// The row whose `id` is `id`, or `nil`.
    public static func device(id: String) -> BLEButtonDevice? {
        all.first { $0.id == id }
    }

    /// The row whose name prefix `advertisedName` starts with (case-sensitive),
    /// or `nil`. A peripheral with no name never matches.
    public static func match(advertisedName: String?) -> BLEButtonDevice? {
        guard let advertisedName else { return nil }
        let index = advertisedName.withCString { iax_bleptt_match_name($0) }
        guard index >= 0 else { return nil }
        return all.first { $0.index == Int(index) }
    }

    /// True only for `device`'s service, holding its characteristic with
    /// NOTIFY. A lookalike that advertises the right name but not this GATT
    /// shape is rejected.
    public static func acceptsGATT(
        _ device: BLEButtonDevice, service: String, characteristic: String, properties: UInt8
    ) -> Bool {
        service.withCString { s in
            characteristic.withCString { c in
                iax_bleptt_accepts_gatt(UInt(device.index), s, c, properties)
            }
        }
    }
}

/// The engine's pure button machine for one device. It is single-threaded:
/// drive it from one actor. The handle is freed in `deinit`.
public final class BLEButtonMachine {
    /// The engine's default timing: 300 ms settle, 1.5 s lease, 250 ms renew,
    /// and so on.
    public static let defaultConfig: BLEMachineConfig = {
        var raw = IaxBleConfig()
        // It can only fail on a NULL out-pointer, which `&raw` never is.
        _ = iax_bleptt_default_config(&raw)
        return BLEMachineConfig(raw: raw)
    }()

    /// The engine's handle. It is named `raw`, not `handle`, so it cannot be
    /// confused with `handle(_:nowMS:)`.
    private let raw: OpaquePointer

    /// Creates a machine for `device`. Throws `StationError` (-1) if the
    /// engine returns NULL, which means a bad index or a caught panic.
    public init(device: BLEButtonDevice) throws {
        guard let raw = iax_bleptt_new(UInt(device.index)) else {
            throw StationError.from(-1)
        }
        self.raw = raw
    }

    deinit {
        iax_bleptt_free(raw)
    }

    /// Feed one event at `nowMS` (a monotonic offset from any origin) and get
    /// back what to do. A notify payload is copied whole, so its true length
    /// reaches the engine and anything but exactly one byte is malformed.
    ///
    /// A Swift `BLELinkEvent` can only make a valid C event, so a refusal
    /// should be unreachable. A non-zero code (a caught panic, -1, or a
    /// refusal, -26) means the machine's state can no longer be trusted, so
    /// it fails safe: that call returns `failSafe` (a link-lost release, drop
    /// the link, not pressed) and the machine is poisoned. Every later
    /// `handle` and `output` returns `poisonedState` (no intent, no commands,
    /// not pressed) until the machine is recreated. Returning the old state
    /// instead would keep reporting `pressed`, and the controller renews the
    /// lease while `pressed`, so a machine that panicked on every event would
    /// hold the key forever.
    public func handle(_ event: BLELinkEvent, nowMS: UInt64) -> BLEMachineOutput {
        var ev = IaxBleEvent()
        var payload: [UInt8] = []
        switch event {
        case .enable(let confirmed):
            ev.kind = UInt32(IAX_BLE_EV_ENABLE)
            ev.confirmed = confirmed
        case .disable: ev.kind = UInt32(IAX_BLE_EV_DISABLE)
        case .confirmed: ev.kind = UInt32(IAX_BLE_EV_CONFIRMED)
        case .adapter(let state):
            ev.kind = UInt32(IAX_BLE_EV_ADAPTER)
            ev.adapter = state.rawValue
        case .connected(let epoch):
            ev.kind = UInt32(IAX_BLE_EV_CONNECTED)
            ev.epoch = epoch
        case .gattAccepted(let epoch):
            ev.kind = UInt32(IAX_BLE_EV_GATT_ACCEPTED)
            ev.epoch = epoch
        case .gattRejected(let epoch):
            ev.kind = UInt32(IAX_BLE_EV_GATT_REJECTED)
            ev.epoch = epoch
        case .subscribed(let epoch):
            ev.kind = UInt32(IAX_BLE_EV_SUBSCRIBED)
            ev.epoch = epoch
        case .unsubscribed(let epoch):
            ev.kind = UInt32(IAX_BLE_EV_UNSUBSCRIBED)
            ev.epoch = epoch
        case .notify(let epoch, let bytes):
            ev.kind = UInt32(IAX_BLE_EV_NOTIFY)
            ev.epoch = epoch
            payload = bytes
        case .probeOK(let epoch, let rssi):
            ev.kind = UInt32(IAX_BLE_EV_PROBE_OK)
            ev.epoch = epoch
            ev.rssi = rssi
        case .probeFailed(let epoch):
            ev.kind = UInt32(IAX_BLE_EV_PROBE_FAILED)
            ev.epoch = epoch
        case .disconnected(let epoch):
            ev.kind = UInt32(IAX_BLE_EV_DISCONNECTED)
            ev.epoch = epoch
        case .sleep: ev.kind = UInt32(IAX_BLE_EV_SLEEP)
        case .wake: ev.kind = UInt32(IAX_BLE_EV_WAKE)
        case .leaseLost: ev.kind = UInt32(IAX_BLE_EV_LEASE_LOST)
        case .tick: ev.kind = UInt32(IAX_BLE_EV_TICK)
        }
        return handle(rawEvent: ev, payload: payload, nowMS: nowMS)
    }

    /// The one place an event crosses into the engine. It is internal, not
    /// private, so a test can send a C event no `BLELinkEvent` can build (an
    /// unknown kind) and watch the machine poison itself.
    func handle(
        rawEvent event: IaxBleEvent, payload: [UInt8], nowMS: UInt64
    ) -> BLEMachineOutput {
        guard !poisoned else { return Self.poisonedState }
        var ev = event
        var out = IaxBleOutput()
        let rc: Int32 = payload.withUnsafeBufferPointer { buf in
            ev.payload = buf.baseAddress
            ev.payload_len = UInt(buf.count)
            return iax_bleptt_event(raw, &ev, nowMS, &out)
        }
        if rc != IAX_OK { poisoned = true }
        return Self.output(forCode: rc, decoded: BLEMachineOutput(raw: out))
    }

    /// The current state, with `intent == .none` and no commands, as the
    /// contract promises. A poisoned machine reports `poisonedState`. So does
    /// a status read the engine refuses; that read does not poison the
    /// machine, because only an event can deliver the one release and drop.
    public var output: BLEMachineOutput {
        guard !poisoned else { return Self.poisonedState }
        var out = IaxBleOutput()
        guard iax_bleptt_status(raw, &out) == IAX_OK else { return Self.poisonedState }
        return BLEMachineOutput(raw: out)
    }

    /// What the call that poisons the machine returns: release because the
    /// link is lost, drop the link, and never pressed. The controller stops
    /// renewing the lease, so the engine lets it expire, and tears the link
    /// down. It is returned once, so the banner and announcement happen once.
    static let failSafe = BLEMachineOutput(
        intent: .release(.linkLost), probe: false, dropLink: true, status: .reconnecting,
        signal: nil, lastRSSI: nil, rawPressed: false, pressed: false)

    /// What a poisoned machine reports from then on: no intent, no commands,
    /// never pressed. The release and drop were already delivered by
    /// `failSafe`; repeating them on every 250 ms tick would raise a new
    /// banner, a new VoiceOver announcement and a new cancel each time.
    static let poisonedState = BLEMachineOutput(
        intent: .none, probe: false, dropLink: false, status: .reconnecting,
        signal: nil, lastRSSI: nil, rawPressed: false, pressed: false)

    /// The decision behind every engine call: `decoded` only when the engine
    /// returned `IAX_OK`, `failSafe` for any other code. Pure, so a test can
    /// feed it codes the engine never returns on purpose.
    static func output(forCode rc: Int32, decoded: BLEMachineOutput) -> BLEMachineOutput {
        rc == IAX_OK ? decoded : failSafe
    }

    /// Set by the first non-zero code from the engine and never cleared.
    private var poisoned = false
}

// MARK: - C-ABI conversions

extension BLEMachineOutput {
    init(raw: IaxBleOutput) {
        self.init(
            intentCode: raw.intent, releaseReasonCode: raw.release_reason,
            statusCode: raw.status, raw: raw)
    }

    /// The decoding behind `init(raw:)`, with the three enum codes passed
    /// separately, so a test can feed it codes this build does not know.
    init(intentCode: UInt32, releaseReasonCode: UInt32, statusCode: UInt32, raw: IaxBleOutput) {
        switch intentCode {
        case UInt32(IAX_BLE_INTENT_NONE): intent = .none
        case UInt32(IAX_BLE_INTENT_PRESS): intent = .press
        // A release, or an intent this build does not know: either way, fail
        // towards unkeyed.
        default:
            intent = .release(BLEReleaseReason(rawValue: releaseReasonCode) ?? .linkLost)
        }
        probe = raw.cmd_probe
        dropLink = raw.cmd_drop_link
        status = BLELinkStatus(rawValue: statusCode) ?? .disabled
        signal = BLESignalBand(rawValue: raw.signal)  // 0 = none → nil
        lastRSSI = raw.has_rssi ? raw.last_rssi : nil
        rawPressed = raw.raw_pressed
        pressed = raw.pressed
    }
}

extension BLEMachineConfig {
    init(raw: IaxBleConfig) {
        self.init(
            settleMS: raw.settle_ms, probeKeyedMS: raw.probe_keyed_ms,
            probeMissesDead: raw.probe_misses_dead, probeIdleMS: raw.probe_idle_ms,
            leaseTTLMS: raw.lease_ttl_ms, renewMS: raw.renew_ms,
            bandStrongDBM: raw.band_strong_dbm, bandFairDBM: raw.band_fair_dbm)
    }
}

extension BLEButtonDevice {
    init(index: Int, raw: IaxBleDevice) {
        self.init(
            index: index, id: cTupleString(raw.id), displayName: cTupleString(raw.display_name),
            namePrefix: cTupleString(raw.name_prefix), serviceUUID: cTupleString(raw.service),
            characteristicUUID: cTupleString(raw.characteristic),
            kind: BLEDeviceKind(rawValue: raw.kind) ?? .buttonOnly,
            liveness: BLELiveness(rawValue: raw.liveness) ?? .none,
            verified: cTupleString(raw.verified))
    }
}

/// A fixed `char[N]` from the C-ABI, which Swift imports as a tuple, read up
/// to its first NUL. The engine always NUL-terminates. Stopping at the
/// buffer's end anyway means a missing NUL cannot read past it.
private func cTupleString<T>(_ tuple: T) -> String {
    withUnsafeBytes(of: tuple) { raw in
        String(decoding: raw.prefix { $0 != 0 }, as: UTF8.self)
    }
}
