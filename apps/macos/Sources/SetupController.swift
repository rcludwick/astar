// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import Combine
    import Foundation

    /// Owns the user's named **Setups** and applies one in a single action.
    ///
    /// A Setup bundles a hardware profile (serial preset / none) with the input +
    /// output devices, so the user can flip their whole rig — "UCI150 desk" ↔ "Jabra
    /// mobile" — at once. Applying one coordinates `SerialController` (macOS-only
    /// IOKit) and `CallSession` (device selection), which is why this lives in the
    /// app layer rather than `AstarCore`. The data model + persistence + validation
    /// (`Setup`/`SetupStore`/`missingDevices`) are platform-neutral and tested in
    /// `AstarCore`.
    @MainActor
    final class SetupController: ObservableObject {
        /// All setups, in user order.
        @Published private(set) var setups: [Setup]
        /// The applied setup's id (✓ in the switchers), or nil if none applied.
        @Published private(set) var selectedID: String?
        /// Set when an apply was refused because the Setup's USB device(s) aren't
        /// present, or a selection failed. Cleared on the next successful apply.
        @Published private(set) var lastError: String?
        /// The config applied automatically at launch (★ in the cards); nil = none.
        @Published private(set) var defaultID: String?

        let registry = HardwareProfileRegistry()

        /// The built-in **System Default** config (astar-1f7d): system input/output,
        /// no serial. Always present and first in the list; can't be deleted. Its
        /// identity and the launch-default rule live in `AstarCore` where they are
        /// unit-testable — see `SystemDefaultSetup`.
        static let systemDefaultID = SystemDefaultSetup.id

        private weak var session: CallSession?
        private weak var serial: SerialController?
        private let store: SetupStore
        private let audioStore: AudioSettingsStore = UserDefaultsAudioSettingsStore()

        /// User setups the management UI may edit/delete/reorder — everything except
        /// the built-in System Default, which has its own fixed row.
        var managedSetups: [Setup] { setups.filter { $0.id != Self.systemDefaultID } }

        /// The currently-applied setup, if any.
        var selectedSetup: Setup? { setups.first { $0.id == selectedID } }

        init(store: SetupStore = UserDefaultsSetupStore()) {
            self.store = store
            self.setups = [SystemDefaultSetup.setup] + store.all()
            self.selectedID = store.loadSelectedID()
            self.defaultID = store.loadDefaultID()
        }

        /// Wire to the live session + serial controller, then apply the launch
        /// config. Call once at launch.
        ///
        /// astar-1f7d: this used to seed a config seeded from `serial
        /// .selectedProfileID ?? uci150ID` — so a fresh install on a machine that
        /// had never seen a UCI150 got a config named after that hardware, on a
        /// serial profile. Now a fresh install lands on System Default, and an
        /// existing user who never set a ★ is left alone (see
        /// `SystemDefaultSetup.launchApplyID` for why that `nil` matters).
        func attach(session: CallSession, serial: SerialController) {
            self.session = session
            self.serial = serial
            guard
                let applyID = SystemDefaultSetup.launchApplyID(
                    storedDefault: defaultID, savedConfigIDs: store.all().map(\.id))
            else { return }
            // A fresh install has no ★ yet — record where the launch config came
            // from so the star shows against System Default rather than nothing.
            if defaultID == nil { setDefault(applyID) }
            apply(id: applyID)
        }

        // MARK: - Apply

        /// Switch to `setup`: validate its devices are present, apply the hardware
        /// profile (serial) + select the devices, and record the selection. Refuses
        /// (sets `lastError`, changes nothing) when a named USB device is unplugged.
        func apply(_ setup: Setup) {
            guard let session else { return }

            let missing = setup.missingDevices(inputs: session.inputs(), outputs: session.outputs())
            guard missing.isEmpty else {
                lastError =
                    "“\(setup.name)” needs \(missing.joined(separator: ", ")), which isn’t connected."
                return
            }

            applyHardware(setup)
            do {
                try session.selectDevices(input: setup.inputDevice, output: setup.outputDevice)
                session.setMicProfileSelection(id: setup.micProfileID)
                // Persist the applied devices so the rest of the app (quick config,
                // relaunch) reflects this Setup's selection.
                var a = audioStore.load()
                a.input = setup.inputDevice
                a.output = setup.outputDevice
                // Restore this rig's tuned gains, if it has any saved.
                if let g = setup.inputGain {
                    try? session.setInputGain(g)
                    a.inputGain = g
                }
                if let g = setup.outputGain {
                    try? session.setOutputGain(g)
                    a.outputGain = g
                }
                audioStore.save(a)
                // Restore mic processing + VOX + full duplex (the session setters
                // persist these themselves, preserving the devices/gains saved above).
                if let c = setup.compression { session.setCompression(c) }
                if let cl = setup.compressionLevel { session.setCompressionLevel(cl) }
                if let t = setup.txTrim { session.setTxTrim(t) }
                if let n = setup.noiseReduction { session.setNoiseReduction(n) }
                if let t = setup.voxThreshold { session.setVoxThreshold(t) }
                if let v = setup.voxEnabled { session.setVoxEnabled(v) }
                if let fd = setup.fullDuplex { session.setFullDuplex(fd) }
                lastError = nil
            } catch {
                lastError = "Couldn’t select “\(setup.name)” devices: \(error.localizedDescription)"
            }

            selectedID = setup.id
            store.saveSelectedID(setup.id)
        }

        /// Drive the live serial device from a config's hardware. A config with a
        /// serial radio applies its own `serial` spec (falling back to the preset);
        /// a non-serial config (headset / None) disables serial. Also called live
        /// when the active config's hardware/serial is edited in its card.
        func applyHardware(_ setup: Setup) {
            let profile = registry.resolve(id: setup.hardwareProfileID)
            if profile.usesSerial {
                serial?.applySerial(setup.serial ?? profile.serial ?? .uci150, enabled: true)
            } else {
                serial?.applySerial(nil, enabled: false)
            }
        }

        /// Apply by id (the menu/popover switchers pass an id). No-op for unknown ids.
        func apply(id: String) {
            guard let setup = setups.first(where: { $0.id == id }) else { return }
            apply(setup)
        }

        // MARK: - CRUD

        /// Create a fresh, blank config (no serial, system-default devices, no audio
        /// overrides) for the user to expand and fill in. Returns it so the UI can
        /// focus its name field.
        @discardableResult
        func addNew(name: String = "New config") -> Setup {
            let setup = Setup(
                id: UUID().uuidString,
                name: name,
                hardwareProfileID: HardwareProfileRegistry.headsetID
            )
            store.save(setup)
            refresh()
            return setup
        }

        /// Set (or clear, passing nil) the launch-default config. Persisted.
        func setDefault(_ id: String?) {
            defaultID = id
            store.saveDefaultID(id)
        }

        /// Upsert an edited Setup (rename, change hardware/devices).
        func update(_ setup: Setup) {
            store.save(setup)
            refresh()
        }

        /// Delete a Setup. Clears the selection if it was the selected one. The
        /// built-in None can't be deleted.
        func delete(id: String) {
            guard id != Self.systemDefaultID else { return }
            store.remove(id: id)
            if selectedID == id {
                selectedID = nil
                store.saveSelectedID(nil)
            }
            if defaultID == id { setDefault(nil) }
            refresh()
        }

        /// Snapshot the current audio state — mic + volume gains, compression, noise
        /// reduction, VOX — into the selected setup, so switching to it later restores
        /// the whole rig. Reads the live persisted `AudioSettings`. No-op for None /
        /// no selection.
        func saveCurrentToSelected() {
            guard let id = selectedID, id != Self.systemDefaultID,
                var s = store.all().first(where: { $0.id == id })
            else { return }
            let a = audioStore.load()
            s.inputDevice = a.input
            s.outputDevice = a.output
            s.inputGain = a.inputGain
            s.outputGain = a.outputGain
            s.compression = a.compression
            s.compressionLevel = a.compressionLevel
            s.txTrim = a.txTrim
            s.noiseReduction = a.noiseReduction
            s.voxEnabled = a.voxEnabled
            s.voxThreshold = a.voxThresholdDBFS
            s.fullDuplex = a.fullDuplex
            s.micProfileID = a.micProfileID
            store.save(s)
            refresh()
        }

        /// Persist a live device change (from Quick settings) into the active setup
        /// so it survives relaunch. The setup re-applies its own devices at launch
        /// (after the audio store is restored), which would otherwise clobber a
        /// device change that was only written to the audio store. No-op for None.
        func setActiveDevices(input: String?, output: String?) {
            guard let id = selectedID, id != Self.systemDefaultID,
                var s = store.all().first(where: { $0.id == id })
            else { return }
            s.inputDevice = input
            s.outputDevice = output
            store.save(s)
            refresh()
        }

        /// Reorder (drag in the Audio tab list).
        func move(from: Int, to: Int) {
            store.move(from: from, to: to)
            refresh()
        }

        /// Reorder from a SwiftUI `List` `.onMove` (IndexSet + insert offset). Maps the
        /// move-before-offset semantics onto the store's remove-then-insert.
        func move(fromOffsets source: IndexSet, toOffset destination: Int) {
            guard let from = source.first else { return }
            let to = destination > from ? destination - 1 : destination
            store.move(from: from, to: to)
            refresh()
        }

        /// The hardware profile name for a Setup's id (for switcher labels/subtitles).
        func hardwareName(for setup: Setup) -> String {
            registry.resolve(id: setup.hardwareProfileID).name
        }

        // MARK: - Internals

        private func refresh() { setups = [SystemDefaultSetup.setup] + store.all() }
    }
#endif
