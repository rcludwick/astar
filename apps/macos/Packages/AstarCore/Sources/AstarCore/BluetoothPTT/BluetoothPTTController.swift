// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Combine
import Foundation
import os

/// A button the transport has seen, for the setup list.
public struct BLEDiscoveredButton: Hashable, Identifiable, Sendable {
    public let identifier: UUID
    public let advertisedName: String
    public let deviceID: String
    public var rssi: Int16?
    public var id: UUID { identifier }

    public init(identifier: UUID, advertisedName: String, deviceID: String, rssi: Int16? = nil) {
        self.identifier = identifier
        self.advertisedName = advertisedName
        self.deviceID = deviceID
        self.rssi = rssi
    }
}

/// A release the Bluetooth machine forced while it held the key: the link
/// dropped, Bluetooth went off, the Mac slept, or the device sent something
/// unknown. `seq` increases with each one, so the banner and VoiceOver fire
/// once per stop.
public struct BluetoothForcedRelease: Equatable, Sendable {
    public let seq: Int
    public let reason: BLEReleaseReason
    public let at: Date

    public init(seq: Int, reason: BLEReleaseReason, at: Date) {
        self.seq = seq
        self.reason = reason
        self.at = at
    }
}

/// Drives one Bluetooth LE PTT button for the active config.
///
/// Every keying decision is the Rust `ButtonMachine`'s, through the FFI. This
/// class only turns transport callbacks into `BLELinkEvent`s, carries out the
/// machine's two commands (probe as an RSSI read, drop the link as a cancel)
/// and hands its intents to `CallSession.bluetoothPTT(_:)`.
///
/// Rules it keeps:
/// - The central is created only when a config that uses the button is
///   attached with a unit confirmed on this Mac, or when Rob starts setup.
///   Anyone who never uses the feature never sees a Bluetooth prompt.
/// - Outside setup it connects only to identifiers confirmed in the store. A
///   confirmation binds to the identifier, never the name, so a neighbour's
///   button with the same name is listed, never connected, and can never key.
/// - Confirmation happens only in setup, from a raw press on the linked unit
///   made after the settle window. Setup links a unit by itself only while
///   it is the only one in range; with two in range Rob picks one.
///   When the spec names a unit, setup offers and confirms only that name,
///   so the confirmation it writes is one the next attach finds again.
/// - A release sets `lastForcedStop` only when it cut a live over: the
///   machine was pressed before the event and the reason was not the button
///   or a deliberate disable.
/// - Its own 250 ms ticker drives renewals and probes, so pausing the session's
///   poll cannot let the engine lease lapse.
@MainActor
public final class BluetoothPTTController: ObservableObject, BLECentralDelegate {
    /// How long a user-started search scans before it gives up.
    static let searchWindowMS: UInt64 = 30_000
    /// During setup, how long the only unit in range must stay the only one
    /// before the controller links it by itself. With two buttons in range
    /// (a club meeting) it links neither: Rob picks one.
    static let setupSoleUnitMS: UInt64 = 2_000
    /// The statuses in which the link is up and subscribed.
    private static let linkedStatuses: Set<BLELinkStatus> = [.ready, .needsConfirm, .releaseToArm]
    nonisolated static let logger = Logger(subsystem: "com.aj7hr.astar", category: "bluetooth-ptt")

    @Published public private(set) var spec: BluetoothPTTSpec?
    @Published public private(set) var status: BLELinkStatus = .disabled
    @Published public private(set) var signal: BLESignalBand?
    @Published public private(set) var lastRSSI: Int16?
    /// The last live notify was a press, whatever the gates said. This drives
    /// the self-test and the chip's accent fill for a refused press.
    @Published public private(set) var rawPressed = false
    /// Subscribed on a live epoch. While true the app keeps polling, so the
    /// tint and the talk timer stay live.
    @Published public private(set) var isLinked = false
    /// Attached, but no unit of this model is confirmed on this Mac.
    @Published public private(set) var needsSetup = false
    @Published public private(set) var awaitingConfirmation = false
    /// A 30 s user-started scan is running.
    @Published public private(set) var isSearching = false
    @Published public private(set) var discovered: [BLEDiscoveredButton] = []
    @Published public private(set) var connected: BLEDiscoveredButton?
    /// The name of a lookalike that lacks the button's service.
    @Published public private(set) var rejectedName: String?
    @Published public private(set) var lastForcedStop: BluetoothForcedRelease?
    /// Set by a confirming press; it drives "Confirmed on this Mac".
    @Published public private(set) var justConfirmed = false

    /// The device-table row for the attached spec.
    public var device: BLEButtonDevice? {
        spec.flatMap { BLEButtonDevices.device(id: $0.deviceID) }
    }

    /// The units of this model confirmed on this Mac.
    public var confirmedButtons: [BLEConfirmation] {
        spec.map { store.confirmations(deviceID: $0.deviceID) } ?? []
    }

    /// Where a malformed payload's hex goes, once per connection. The default
    /// is the unified log; tests capture it.
    var diagnosticLog: (String) -> Void = { message in
        BluetoothPTTController.logger.notice("\(message, privacy: .public)")
    }

    private let centralFactory: @MainActor () -> BLECentralDriving
    private let store: BluetoothPTTLocalStore
    private let ticker: BluetoothPTTTicker
    private let clockMS: () -> UInt64

    private var central: BLECentralDriving?
    private var machine: BLEButtonMachine?
    private weak var session: BluetoothPTTSink?
    private var enabled = false
    private var sleeping = false
    private var adapterState: BLEAdapterState?
    private var peripheralID: UUID?
    private var liveEpoch: UInt32 = 0
    private var scanning = false
    private var searchDeadlineMS: UInt64?
    private var rejected: Set<UUID> = []
    private var names: [UUID: String] = [:]
    private var forcedSeq = 0
    private var malformedLoggedEpoch: UInt32?
    /// When the live link subscribed. A setup press confirms only once the
    /// settle window has passed since then, so a "current state" notify on
    /// CCCD enable can never confirm a unit nobody pressed.
    private var subscribedAtMS: UInt64?
    /// Setup: when the one unit discovered so far was first seen.
    private var soleUnitSinceMS: UInt64?
    /// Setup: the live link was made automatically (the only unit found),
    /// not chosen by Rob, so a second unit appearing drops it.
    private var setupAutoLinked = false
    /// The machine's output for the last event fed from outside `apply`.
    /// Internal so the scenario replay can compare the machine's commands
    /// and `pressed` with the shared traces through the controller itself.
    private(set) var lastOutput: BLEMachineOutput?

    /// `centralFactory` is called at most once, and only when the feature is
    /// really in use. That is what keeps the Bluetooth permission prompt away
    /// from everyone else.
    public init(
        centralFactory: @escaping @MainActor () -> BLECentralDriving,
        store: BluetoothPTTLocalStore = UserDefaultsBluetoothPTTLocalStore(),
        ticker: BluetoothPTTTicker = DispatchBluetoothPTTTicker(),
        clockMS: @escaping () -> UInt64 = BluetoothPTTController.monotonicMS
    ) {
        self.centralFactory = centralFactory
        self.store = store
        self.ticker = ticker
        self.clockMS = clockMS
    }

    /// Milliseconds since boot. It is monotonic, as the machine requires;
    /// wall-clock time can jump.
    nonisolated public static func monotonicMS() -> UInt64 {
        DispatchTime.now().uptimeNanoseconds / 1_000_000
    }

    // MARK: - Lifecycle

    /// The config's PTT hardware is this device. It creates the central only
    /// if a unit is confirmed on this Mac; otherwise it sets `needsSetup` and
    /// touches no Bluetooth.
    public func attach(session: BluetoothPTTSink, spec: BluetoothPTTSpec) {
        if self.spec == spec, self.session === session { return }
        if self.spec != nil { detach() }
        self.session = session
        self.spec = spec
        guard let device = BLEButtonDevices.device(id: spec.deviceID),
            let machine = try? BLEButtonMachine(device: device)
        else { return }
        self.machine = machine
        names = Dictionary(
            confirmedButtons.map { ($0.platformID, $0.advertisedName) },
            uniquingKeysWith: { first, _ in first })
        guard !candidateIDs().isEmpty else {
            needsSetup = true
            return
        }
        needsSetup = false
        enable(confirmed: true)
    }

    /// The config switched away. It feeds `Disable`, which releases if keyed,
    /// then stops the ticker and cancels the link.
    public func detach() {
        quiesce()
        awaitingConfirmation = false
        needsSetup = false
        justConfirmed = false
        rejectedName = nil
        rejected = []
        discovered = []
        spec = nil
        session = nil
        machine = nil
        status = .disabled
        signal = nil
        lastRSSI = nil
        rawPressed = false
        isLinked = false
    }

    /// User action only (the picker choice, or the "Set up …" button). It
    /// creates the central, enables the machine unconfirmed and starts
    /// discovery. A raw press on the linked unit then confirms it.
    public func beginSetup() {
        guard spec != nil, machine != nil else { return }
        quiesce()
        rejected = []
        rejectedName = nil
        discovered = []
        justConfirmed = false
        awaitingConfirmation = true
        enable(confirmed: false)
        search()
    }

    /// A 30 s scan, started by the user.
    public func search() {
        guard enabled else { return }
        isSearching = true
        searchDeadlineMS = clockMS() + Self.searchWindowMS
        seek()
    }

    /// Rob picked a unit from the list. During setup any unit that fits the
    /// spec's preferred name may be tried; outside setup only a confirmed one.
    public func select(_ button: BLEDiscoveredButton) {
        guard enabled, button.deviceID == spec?.deviceID, button.identifier != peripheralID,
            (awaitingConfirmation && fitsPreferredName(button.advertisedName))
                || candidateIDs().contains(button.identifier)
        else { return }
        dropLink()
        setupAutoLinked = false
        connect(to: button.identifier)
    }

    /// Clears the local confirmation for the linked unit, or else the first
    /// confirmed one. It releases first, so forgetting a held button can never
    /// leave it keyed.
    public func forgetConfirmedButton() {
        guard let spec, let target = connected?.identifier ?? candidateIDs().first else { return }
        quiesce()
        store.forget(platformID: target, deviceID: spec.deviceID)
        awaitingConfirmation = false
        justConfirmed = false
        if candidateIDs().isEmpty {
            needsSetup = true
        } else {
            enable(confirmed: true)
        }
    }

    /// `NSWorkspace.willSleepNotification`: release, then cancel the link.
    public func systemWillSleep() {
        sleeping = true
        guard enabled else { return }
        feed(.sleep)
        if scanning {
            central?.stopScan()
            scanning = false
        }
        dropLink()
    }

    /// `NSWorkspace.didWakeNotification`: retrieve and reconnect. A sleep
    /// with the button held latched the machine, so that button must be
    /// released first; after an idle sleep the first press keys.
    public func systemDidWake() {
        sleeping = false
        guard enabled else { return }
        feed(.wake)
        seek()
    }

    /// App quit: release, cancel and shut the central down before the station
    /// drops.
    public func shutdown() {
        quiesce()
        central?.shutdown()
        central = nil
        session = nil
    }

    /// The one path into the machine. Every event goes through here, so the
    /// tests can replay the shared scenarios through exactly the code the
    /// transport uses.
    func feed(_ event: BLELinkEvent) {
        _ = step(event)
    }

    // MARK: - BLECentralDelegate

    public func centralAdapterChanged(_ state: BLEAdapterState) {
        adapterState = state
        if state != .on {
            // CoreBluetooth invalidates every peripheral when the adapter goes.
            peripheralID = nil
            connected = nil
            scanning = false
        }
        feed(.adapter(state))
        if state == .on { seek() }
    }

    public func centralDiscovered(_ peripheral: BLEPeripheralRef, rssi: Int16?) {
        consider(peripheral, rssi: rssi)
    }

    public func centralConnected(_ id: UUID, epoch: UInt32) {
        guard isLive(id, epoch), let device else { return }
        feed(.connected(epoch: epoch))
        connected = BLEDiscoveredButton(
            identifier: id, advertisedName: names[id] ?? device.namePrefix, deviceID: device.id)
        if scanning, !isSearching {
            central?.stopScan()
            scanning = false
        }
    }

    public func centralConnectFailed(_ id: UUID, epoch: UInt32) {
        feed(.disconnected(epoch: epoch))
        guard isLive(id, epoch) else { return }
        peripheralID = nil
        connected = nil
        startScanIfIdle()
    }

    public func centralDiscoveredGATT(
        _ id: UUID, epoch: UInt32, characteristics: [BLEGattCharacteristic]
    ) {
        guard isLive(id, epoch), let device else { return }
        let accepted = characteristics.contains {
            BLEButtonDevices.acceptsGATT(
                device, service: $0.service, characteristic: $0.uuid, properties: $0.properties)
        }
        if accepted {
            feed(.gattAccepted(epoch: epoch))
            central?.subscribe(
                id, epoch: epoch, service: device.serviceUUID,
                characteristic: device.characteristicUUID)
            return
        }
        // Record the rejection before the machine hears of it. The machine
        // answers `GattRejected` with `drop_link`, and `apply` then calls
        // `dropLink()` (the only cancel) and `reconnect(id)`, which must
        // already see this unit as skipped for the session.
        rejected.insert(id)
        rejectedName = names[id] ?? device.namePrefix
        discovered.removeAll { $0.identifier == id }
        feed(.gattRejected(epoch: epoch))
        startScanIfIdle()
    }

    public func centralSubscribed(_ id: UUID, epoch: UInt32) {
        guard isLive(id, epoch) else { return }
        subscribedAtMS = clockMS()
        feed(.subscribed(epoch: epoch))
    }

    public func centralUnsubscribed(_ id: UUID, epoch: UInt32) {
        feed(.unsubscribed(epoch: epoch))
        guard isLive(id, epoch) else { return }
        dropLink()
        reconnect(id)
    }

    public func centralNotified(_ id: UUID, epoch: UInt32, bytes: [UInt8]) {
        let out = step(.notify(epoch: epoch, bytes: bytes))
        guard isLive(id, epoch) else { return }
        if case .release(.malformed)? = out?.intent, malformedLoggedEpoch != epoch {
            malformedLoggedEpoch = epoch
            diagnosticLog(
                "\(names[id] ?? "Bluetooth PTT button") sent an unrecognised value: "
                    + "\(Self.hex(bytes)) (connection \(epoch))")
        }
        // Confirm only a deliberate press: past the settle window, so the
        // "current state" notify some firmware sends on CCCD enable (which
        // the machine latches without keying, but still reports as a raw
        // press) can never confirm a unit nobody pressed.
        if awaitingConfirmation, out?.rawPressed == true, let since = subscribedAtMS,
            clockMS() >= since + UInt64(BLEButtonMachine.defaultConfig.settleMS)
        {
            confirm(id)
        }
    }

    public func centralReadRSSI(_ id: UUID, epoch: UInt32, rssi: Int16?) {
        guard let rssi else {
            feed(.probeFailed(epoch: epoch))
            return
        }
        feed(.probeOK(epoch: epoch, rssi: rssi))
        if isLive(id, epoch) { connected?.rssi = rssi }
    }

    public func centralDisconnected(_ id: UUID, epoch: UInt32) {
        guard isLive(id, epoch) else {
            feed(.disconnected(epoch: epoch))
            return
        }
        peripheralID = nil
        connected = nil
        feed(.disconnected(epoch: epoch))
        reconnect(id)
    }

    // MARK: - Machine

    private func step(_ event: BLELinkEvent) -> BLEMachineOutput? {
        guard let machine else { return nil }
        let wasPressed = machine.output.pressed
        let out = machine.handle(event, nowMS: clockMS())
        apply(out, wasPressed: wasPressed)
        // Set after `apply`, so an event `apply` feeds in reply (a latch, or
        // the `Disconnected` of a drop) never hides the output of the event
        // that caused it.
        lastOutput = out
        return out
    }

    /// The intent goes to the session before any command. A dying link
    /// releases first, then drops.
    private func apply(_ out: BLEMachineOutput, wasPressed: Bool) {
        publish(out)
        switch out.intent {
        case .none:
            break
        case .press:
            let outcome = session?.bluetoothPTT(.press) ?? .refused
            // Refused or lost: latch, so the button must be released before
            // it can key, matching the arbiter's own latch.
            if outcome != .accepted { feed(.leaseLost) }
        case .release(let reason):
            _ = session?.bluetoothPTT(.release)
            // Only a stop that cut a live over is news. The machine releases
            // on a malformed value even while idle, and that is not news:
            // VoiceOver would otherwise announce a keyed loss nobody had.
            if wasPressed, reason != .button, reason != .disabled {
                forcedSeq += 1
                lastForcedStop = BluetoothForcedRelease(seq: forcedSeq, reason: reason, at: Date())
            }
        }
        if out.probe, let id = peripheralID { central?.readRSSI(id, epoch: liveEpoch) }
        if out.dropLink, let id = peripheralID {
            dropLink()
            reconnect(id)
        }
    }

    private func publish(_ out: BLEMachineOutput) {
        if status != out.status { status = out.status }
        if signal != out.signal { signal = out.signal }
        if lastRSSI != out.lastRSSI { lastRSSI = out.lastRSSI }
        if rawPressed != out.rawPressed { rawPressed = out.rawPressed }
        let linked = Self.linkedStatuses.contains(out.status)
        if isLinked != linked { isLinked = linked }
    }

    private func handleTick() {
        if isSearching, let deadline = searchDeadlineMS, clockMS() >= deadline { finishSearch() }
        if awaitingConfirmation, enabled, !sleeping, adapterState == .on, peripheralID == nil,
            discovered.count == 1, let since = soleUnitSinceMS,
            clockMS() >= since + Self.setupSoleUnitMS
        {
            setupAutoLinked = true
            connect(to: discovered[0].identifier)
        }
        guard let out = step(.tick), out.pressed, let session else { return }
        // Renew never keys. A failure (NotLeased after a transmit time-out,
        // or an engine that has gone) latches the machine to match the arbiter.
        if session.bluetoothPTT(.renew) == .leaseLost { feed(.leaseLost) }
    }

    // MARK: - Link management

    private func enable(confirmed: Bool) {
        _ = ensureCentral()
        feed(.enable(confirmed: confirmed))
        // A fresh machine (every attach makes one) starts with the adapter
        // unknown, and CoreBluetooth reports the state only when it changes.
        // Without this replay a re-attach after a config switch would show
        // Ready while the machine refused every press.
        if let adapterState { feed(.adapter(adapterState)) }
        enabled = true
        ticker.start(everyMS: BLEButtonMachine.defaultConfig.renewMS) { [weak self] in
            self?.handleTick()
        }
        if !awaitingConfirmation { seek() }
    }

    /// Release (through `Disable`), stop ticking and scanning, and drop the link.
    private func quiesce() {
        if enabled {
            feed(.disable)
            enabled = false
        }
        ticker.stop()
        isSearching = false
        searchDeadlineMS = nil
        soleUnitSinceMS = nil
        setupAutoLinked = false
        if scanning {
            central?.stopScan()
            scanning = false
        }
        dropLink()
    }

    private func ensureCentral() -> BLECentralDriving {
        if let central { return central }
        let made = centralFactory()
        made.delegate = self
        central = made
        made.powerUp()
        return made
    }

    /// Confirmed identifiers for the attached spec. With a `preferredName`,
    /// only units confirmed under that name count, so a config that names one
    /// of Rob's two buttons never links the other. Setup and `confirm` apply
    /// the same filter, so every confirmation setup writes is found here.
    private func candidateIDs() -> [UUID] {
        guard let spec else { return [] }
        return store.confirmations(deviceID: spec.deviceID)
            .filter { fitsPreferredName($0.advertisedName) }
            .map(\.platformID)
    }

    /// True when the spec names no unit, or `name` is exactly the one it names.
    private func fitsPreferredName(_ name: String?) -> Bool {
        guard let preferred = spec?.preferredName else { return true }
        return name == preferred
    }

    private func isLive(_ id: UUID, _ epoch: UInt32) -> Bool {
        id == peripheralID && epoch == liveEpoch
    }

    /// Retrieve known and already-connected peripherals, then scan if none
    /// was connected or a search is running.
    private func seek() {
        guard enabled, !sleeping, adapterState == .on, let central, let device else { return }
        let refs = central.retrievePeripherals(
            identifiers: candidateIDs(), connectedWithService: device.serviceUUID)
        for ref in refs { consider(ref, rssi: nil) }
        if peripheralID == nil || isSearching, !scanning {
            central.startScan()
            scanning = true
        }
    }

    private func consider(_ ref: BLEPeripheralRef, rssi: Int16?) {
        guard let device, !rejected.contains(ref.identifier) else { return }
        let isConfirmed = candidateIDs().contains(ref.identifier)
        let matchesName = BLEButtonDevices.match(advertisedName: ref.name) == device
        guard matchesName || isConfirmed else { return }
        let knownName = ref.name ?? names[ref.identifier]
        // Setup for a config that names its unit offers only that unit.
        if awaitingConfirmation, !fitsPreferredName(knownName) { return }
        let name = knownName ?? device.namePrefix
        names[ref.identifier] = name
        if let index = discovered.firstIndex(where: { $0.identifier == ref.identifier }) {
            if let rssi, discovered[index].rssi != rssi { discovered[index].rssi = rssi }
        } else {
            discovered.append(
                BLEDiscoveredButton(
                    identifier: ref.identifier, advertisedName: name, deviceID: device.id,
                    rssi: rssi))
        }
        if awaitingConfirmation {
            // Setup links a unit by itself only while it is the ONLY one in
            // range, once it has stayed alone for `setupSoleUnitMS` (the tick
            // does that). A second unit drops that automatic link: with two
            // buttons in range Rob chooses, so a neighbour's press can never
            // confirm a unit on this Mac.
            if discovered.count == 1 {
                if soleUnitSinceMS == nil { soleUnitSinceMS = clockMS() }
            } else {
                soleUnitSinceMS = nil
                if setupAutoLinked {
                    setupAutoLinked = false
                    dropLink()
                }
            }
            return
        }
        guard enabled, peripheralID == nil, isConfirmed else { return }
        connect(to: ref.identifier)
    }

    private func connect(to id: UUID) {
        guard let central, let device else { return }
        liveEpoch = liveEpoch == UInt32.max ? 1 : liveEpoch + 1
        peripheralID = id
        subscribedAtMS = nil
        central.connect(id, epoch: liveEpoch, service: device.serviceUUID)
    }

    /// Cancel the live link and tell the machine at once, so its state never
    /// waits on CoreBluetooth's own callback (which then arrives stale).
    private func dropLink() {
        guard let id = peripheralID else { return }
        let epoch = liveEpoch
        peripheralID = nil
        connected = nil
        central?.cancelConnection(id)
        feed(.disconnected(epoch: epoch))
    }

    /// Re-issue a pending connect. It never keys: the next `Subscribed`
    /// restarts the settle window, and a latch from a loss while the button
    /// was held still needs a release.
    private func reconnect(_ id: UUID) {
        guard enabled, !sleeping, adapterState == .on, peripheralID == nil,
            !rejected.contains(id), awaitingConfirmation || candidateIDs().contains(id)
        else { return }
        connect(to: id)
    }

    private func startScanIfIdle() {
        guard enabled, !sleeping, adapterState == .on, !scanning, let central else { return }
        central.startScan()
        scanning = true
    }

    private func finishSearch() {
        isSearching = false
        searchDeadlineMS = nil
        if scanning, awaitingConfirmation || peripheralID != nil {
            central?.stopScan()
            scanning = false
        }
    }

    /// Bind the linked unit's identifier, never its name, so a neighbour's
    /// button advertising the same name can never key. It refuses a name the
    /// spec's preferred-name filter would skip, because `candidateIDs()` would
    /// never find that confirmation again.
    private func confirm(_ id: UUID) {
        guard let spec, let device else { return }
        let name = names[id] ?? device.namePrefix
        guard fitsPreferredName(name) else { return }
        store.confirm(
            BLEConfirmation(platformID: id, advertisedName: name, confirmedAt: Date()),
            deviceID: spec.deviceID)
        awaitingConfirmation = false
        needsSetup = false
        justConfirmed = true
        soleUnitSinceMS = nil
        setupAutoLinked = false
        feed(.confirmed)
        finishSearch()
    }

    private static func hex(_ bytes: [UInt8]) -> String {
        bytes.isEmpty ? "(empty)" : bytes.map { String(format: "%02X", $0) }.joined(separator: " ")
    }
}
