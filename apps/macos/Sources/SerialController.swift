// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import Combine
    import Foundation
    import AstarSerial

    /// Owns the macOS-only serial PTT source (the AllScan UCI150 handset) and bridges
    /// it into the platform-neutral `CallSession`.
    ///
    /// `AstarCore` is multiplatform and must not import `AstarSerial` (which links
    /// IOKit and is macOS-only). So instead of `CallSession` owning a `SerialClient`,
    /// it exposes a serial-free `pttSourceTick` closure hook; this controller — which
    /// lives in the macOS app — owns the `SerialClient` and installs the closure. On
    /// each 20 Hz poll the session calls the closure with the live snapshot's
    /// `remotePTT`/`rxDB`; we tick the serial bridge and forward a keying edge.
    ///
    /// `SerialClient.deinit` drops the radio line, so tearing down (disable, USB
    /// unplug, quit) can never leave the transmitter keyed.
    @MainActor
    final class SerialController: ObservableObject {
        /// Persisted config + enabled flag (UserDefaults).
        @Published private(set) var settings: SerialSettings

        /// True while a `SerialClient` is open and ticking.
        @Published private(set) var isActive = false

        /// Live debounced PTT-key state read from the serial handset (CTS), updated on
        /// every poll. Drives the guided "press your PTT" self-test; resets on
        /// teardown so a torn-down device never reads keyed.
        @Published private(set) var keyDetected = false

        /// Set when the device could not be opened or failed mid-call (e.g. USB
        /// unplug). Surfaced in the settings pane; cleared on a successful enable.
        @Published private(set) var lastError: String?

        /// True while the controller is automatically retrying to re-open the
        /// device after a failure (astar-8f90). Drives the "Reconnecting…"
        /// state in the settings pane; mutually exclusive with `isActive`.
        @Published private(set) var isRetrying = false

        /// The currently selected hardware-profile id (e.g. "uci150", "headset",
        /// "custom"). Drives presets; the persisted `settings` remain the source of
        /// truth for the actual serial config.
        @Published private(set) var selectedProfileID: String

        private weak var session: CallSession?
        private var client: SerialClient?
        private let store: SerialSettingsStore
        private let profileStore: HardwareProfileStore

        /// The auto re-open loop (astar-8f90); non-nil only while retrying.
        private var retryTask: Task<Void, Never>?
        /// Consecutive failed re-open attempts, indexing `SerialRetrySchedule`.
        /// Reset on success and on any explicit user action.
        private var retryAttempt = 0

        init(
            store: SerialSettingsStore = UserDefaultsSerialSettingsStore(),
            profileStore: HardwareProfileStore = UserDefaultsHardwareProfileStore()
        ) {
            self.store = store
            self.profileStore = profileStore
            self.settings = store.load()
            self.selectedProfileID = profileStore.loadSelectedID()
        }

        /// Wire into a session and, if previously enabled, re-open the device. Call
        /// once at launch (and after a session `reconfigure`).
        func attach(to session: CallSession) {
            self.session = session
            if settings.enabled { open() }
        }

        // MARK: - Enable / configure

        /// Toggle the serial source on/off, persisting the choice. Any explicit
        /// user action resets the auto-retry state: enabling starts fresh,
        /// disabling stops retrying entirely.
        func setEnabled(_ on: Bool) {
            settings.enabled = on
            store.save(settings)
            cancelRetry()
            if on { open() } else { teardown() }
        }

        /// Replace the serial config, persist it, and (if enabled) re-open with it.
        /// A manual edit while a *built-in serial preset* (UCI150) is selected means
        /// the config no longer matches that preset, so flip the selection to Custom
        /// — the label then reflects reality and the edits persist as a custom config.
        func update(_ config: SerialConfig) {
            settings.config = config
            store.save(settings)
            if selectedProfileID == HardwareProfileRegistry.uci150ID {
                selectProfileID(HardwareProfileRegistry.customID)
            }
            cancelRetry()
            if settings.enabled { open() }  // re-open applies the new config
        }

        // MARK: - Hardware profiles

        /// Apply a hardware preset on top of the current config + enabled flag.
        ///   * uci150  — load `SerialConfig()` (today's working UCI150 defaults),
        ///               enable serial.
        ///   * headset — disable serial (PTT is the on-screen button / VOX).
        ///   * custom  — keep the current config as-is (manual control); leave the
        ///               enabled flag untouched.
        /// Persists the selected id, persists config/enabled, and re-opens if enabled.
        func applyProfile(_ profile: HardwareProfile) {
            if profile.id == HardwareProfileRegistry.uci150ID {
                // UCI150 defaults + the raw-USB transport (astar-f772): driverless,
                // sandbox/MAS-eligible. The Rust Uci150Usb backend owns the decode.
                settings.config = SerialConfig(spec: .uci150)
                settings.enabled = true
            } else if let spec = profile.serial {
                settings.config = SerialConfig(spec: spec)
                settings.enabled = profile.usesSerial
            } else if !profile.usesSerial {
                // Headset: no serial PTT.
                settings.enabled = false
            }
            // Custom (usesSerial true, serial nil): keep config + enabled as-is.

            selectProfileID(profile.id)
            store.save(settings)
            cancelRetry()
            if settings.enabled { open() } else { teardown() }
        }

        /// Apply a per-config serial spec (or disable serial) directly. Used by
        /// `SetupController` when a saved config's hardware is a serial radio: the
        /// config owns its own line settings, this just drives the live device.
        func applySerial(_ spec: SerialLineSpec?, enabled: Bool) {
            if enabled, let spec {
                settings.config = SerialConfig(spec: spec)
                settings.enabled = true
            } else {
                settings.enabled = false
            }
            store.save(settings)
            cancelRetry()
            if settings.enabled { open() } else { teardown() }
        }

        /// Persist + publish the selected profile id.
        private func selectProfileID(_ id: String) {
            selectedProfileID = id
            profileStore.saveSelectedID(id)
        }

        // MARK: - Lifecycle

        /// Open the device and install the poll-loop tick. Idempotent; replaces any
        /// existing client. Opening failure surfaces via `lastError` (no device,
        /// permissions, driver missing).
        private func open() {
            guard let session else { return }
            do {
                // RX keying is always level-driven now — the "RX key mode" picker was
                // removed because the remote-PTT path doesn't fire for most AllStar
                // nodes. Force rxActivity regardless of any persisted/profile value.
                var cfg = settings.config
                cfg.rxMode = .rxActivity
                let c = try SerialClient(cfg)
                client = c
                isActive = true
                lastError = nil
                // Recovered (or first open): stop any auto-retry loop.
                retryTask?.cancel()
                retryTask = nil
                retryAttempt = 0
                isRetrying = false
                // The session ticks this on its main-thread poll. We capture self
                // weakly; returning nil means "no change this tick".
                //
                // Thread-safety contract: the AstarSerial handle is NOT thread-safe
                // (the underlying iax-serial `ptt_tick` takes `&mut self`, unlike the
                // internally-synchronized AstarStation). We must drive a given handle
                // from a single thread. This holds: `pttTick` only runs from this
                // closure on the session's main-run-loop poll, and `teardown()` /
                // `client = nil` / `deinit` are all @MainActor too — so a tick can
                // never race a close, nor can two threads tick the same handle.
                session.pttSourceTick = { [weak self] remoteKeyed, rxDb in
                    guard let self, let client = self.client else { return nil }
                    do {
                        let (changed, on) = try client.pttTick(remoteKeyed: remoteKeyed, rxDb: rxDb)
                        if changed { self.keyDetected = on }  // mirror for the self-test
                        return changed ? on : nil
                    } catch {
                        // Serial I/O failed (USB unplugged mid-call): tear down so a
                        // dead device can't wedge the loop, and surface it.
                        self.handleTickFailure(error)
                        return nil
                    }
                }
            } catch {
                client = nil
                isActive = false
                lastError = "Couldn’t open serial device: \(error)"
                // Keep trying while enabled — the device may just be unplugged
                // (astar-8f90); a replug re-arms without user action.
                scheduleRetry()
            }
        }

        /// Drop the client + the poll hook (deinit fail-safes the radio line).
        private func teardown() {
            session?.pttSourceTick = nil
            client = nil
            isActive = false
            keyDetected = false
        }

        /// Called from inside the tick closure on a serial error. The closure runs on
        /// the session's main-thread poll, so we are already on the main actor.
        private func handleTickFailure(_ error: Error) {
            teardown()
            lastError = "Serial device error: \(error)"
            scheduleRetry()
        }

        // MARK: - Auto re-arm (astar-8f90)

        /// Start the auto re-open loop if serial is enabled and no loop runs.
        /// Backoff via `SerialRetrySchedule` (2 s doubling to a 30 s cap):
        /// bounded because every attempt against a soured device can strand a
        /// worker (iax-239a trade-off), unbounded in count so a replug always
        /// re-arms eventually. `open()` success cancels the loop and resets it.
        private func scheduleRetry() {
            guard settings.enabled, retryTask == nil else { return }
            isRetrying = true
            retryTask = Task { [weak self] in
                while !Task.isCancelled {
                    guard let self, self.settings.enabled, self.client == nil else { return }
                    let delay = SerialRetrySchedule.delay(attempt: self.retryAttempt)
                    self.retryAttempt += 1
                    try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
                    guard !Task.isCancelled, self.settings.enabled else { return }
                    self.open()  // success cancels this task and resets state
                }
            }
        }

        /// Stop retrying (explicit user action, or disable).
        private func cancelRetry() {
            retryTask?.cancel()
            retryTask = nil
            retryAttempt = 0
            isRetrying = false
        }

        // MARK: - Helpers for the UI

        /// The autodetected WCH USB serial path, or nil if none is plugged in.
        func autodetectedPort() -> String? { SerialClient.autodetect() }

        /// Static form of `autodetectedPort()` for UI that doesn't hold a controller
        /// (the per-config serial editor previews the port that would be detected).
        static func detectedPort() -> String? { SerialClient.autodetect() }

        /// USB-serial ports currently present, for the manual port picker. Filters
        /// `/dev/cu.*` to the USB-serial adapter families (WCH/CH34x on the UCI150,
        /// plus the common FTDI/CP210x/usbmodem names) so the picker doesn't list
        /// Bluetooth/built-in tty devices. Returned as full `/dev/cu.<name>` paths.
        static func usbSerialPorts() -> [String] {
            let prefixes = ["cu.usbserial", "cu.usbmodem", "cu.wchusbserial", "cu.SLAB_USBtoUART"]
            let names = (try? FileManager.default.contentsOfDirectory(atPath: "/dev")) ?? []
            return
                names
                .filter { name in prefixes.contains { name.hasPrefix($0) } }
                .map { "/dev/\($0)" }
                .sorted()
        }
    }
#endif
