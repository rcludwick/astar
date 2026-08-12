// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarSerial
    import SwiftUI

    /// Radio hardware settings. Pick a hardware profile (AllScan UCI150 / USB headset
    /// / Custom) at the top, then refine. Mirrors `DevicesView`'s compact, HIG-aligned
    /// style. For serial profiles, sensible defaults are up front — an enable toggle,
    /// live status, and device autodetect — while the line/polarity/RX knobs live in
    /// a collapsed "Advanced" disclosure so the pane isn't overwhelming. macOS-only.
    struct SerialView: View {
        @EnvironmentObject private var serial: SerialController

        // Local editable mirror of the persisted config; pushed to the controller on
        // change (which re-opens the device if enabled).
        @State private var config = SerialConfig()
        @State private var explicitPort = false  // false = autodetect
        @State private var showAdvanced = false
        @State private var detectedPort: String?

        private let registry = HardwareProfileRegistry()

        /// True when the selected profile keys via the serial interface (UCI150 /
        /// Custom). Headset hides all the serial UI.
        private var isSerialProfile: Bool {
            registry.resolve(id: serial.selectedProfileID).usesSerial
        }

        private var isUCI150: Bool {
            serial.selectedProfileID == HardwareProfileRegistry.uci150ID
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 14) {
                header
                profilePicker

                if isSerialProfile {
                    enableRow
                    statusRow

                    if serial.settings.enabled {
                        deviceSection
                        advancedSection
                    } else if isUCI150 {
                        onboarding
                    }
                } else {
                    headsetNote
                }
            }
            .frame(minWidth: 280)
            .onAppear {
                config = serial.settings.config
                explicitPort = (config.portPath != nil)
                detectedPort = serial.autodetectedPort()
            }
        }

        // MARK: - Header / profile picker / enable / status

        private var header: some View {
            Label("Radio Hardware", systemImage: "cable.connector")
                .font(.headline)
                .labelStyle(.titleAndIcon)
        }

        private var profilePicker: some View {
            Picker(
                "Device",
                selection: Binding(
                    get: { serial.selectedProfileID },
                    set: { newID in
                        serial.applyProfile(registry.resolve(id: newID))
                        // Refresh the local mirror from the freshly applied config.
                        config = serial.settings.config
                        explicitPort = (config.portPath != nil)
                        detectedPort = serial.autodetectedPort()
                    }
                )
            ) {
                ForEach(registry.builtins) { profile in
                    Text(profile.name).tag(profile.id)
                }
            }
            .pickerStyle(.menu)
        }

        private var headsetNote: some View {
            Label(
                "Push-to-talk is the on-screen TX button, the Spacebar, or VOX (enable VOX in Audio).",
                systemImage: "mic.circle"
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        }

        private var enableRow: some View {
            Toggle(
                isOn: Binding(
                    get: { serial.settings.enabled },
                    set: {
                        serial.setEnabled($0)
                        detectedPort = serial.autodetectedPort()
                    }
                )
            ) {
                Text("Use serial handset to key the call")
            }
            .toggleStyle(.switch)
        }

        @ViewBuilder
        private var statusRow: some View {
            if serial.isRetrying {
                // Auto re-arm in progress (astar-8f90): the device dropped and
                // the controller is quietly re-opening on a backoff — replug
                // and it comes back by itself.
                Label(
                    "Serial device lost — reconnecting automatically…",
                    systemImage: "arrow.trianglehead.2.clockwise.rotate.90"
                )
                .font(.caption)
                .foregroundStyle(.orange)
            } else if let err = serial.lastError {
                Label(err, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
            } else if serial.isActive {
                Label(
                    "UCI150 connected — handset PTT is live", systemImage: "checkmark.circle.fill"
                )
                .font(.caption)
                .foregroundStyle(.green)
            } else if serial.settings.enabled {
                Label("Enabled — no serial device found", systemImage: "questionmark.circle")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }

        // MARK: - Device

        private var deviceSection: some View {
            VStack(alignment: .leading, spacing: 8) {
                Picker("Device", selection: $explicitPort) {
                    Text("Autodetect").tag(false)
                    Text("Specific port…").tag(true)
                }
                .pickerStyle(.segmented)
                .onChange(of: explicitPort) { useExplicit in
                    if !useExplicit {
                        config.portPath = nil
                        push()
                    }
                }

                if explicitPort {
                    TextField(
                        "/dev/cu.wchusbserial…",
                        text: Binding(
                            get: { config.portPath ?? "" },
                            set: { config.portPath = $0.isEmpty ? nil : $0 }
                        )
                    )
                    .textFieldStyle(.roundedBorder)
                    .font(.callout.monospaced())
                    .onSubmit(push)
                } else if let detectedPort {
                    Text("Found: \(detectedPort)")
                        .font(.caption2.monospaced())
                        .foregroundStyle(.secondary)
                } else {
                    Text(
                        "No /dev/cu.wchusbserial* port found. Plug in the UCI150 and confirm the CH34x driver is approved."
                    )
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                }
            }
        }

        // MARK: - Advanced (lines, polarity, debounce, RX)

        private var advancedSection: some View {
            DisclosureGroup("Advanced", isExpanded: $showAdvanced) {
                VStack(alignment: .leading, spacing: 12) {
                    lineGroup
                    Divider()
                    rxGroup
                }
                .padding(.top, 8)
            }
            .font(.callout)
        }

        private var lineGroup: some View {
            VStack(alignment: .leading, spacing: 8) {
                // Key INPUT line (operator pushes PTT) — UCI150 default CTS.
                Picker("Key line (in)", selection: $config.keyLine) {
                    Text("CTS").tag(KeyLine.cts)
                    Text("DCD").tag(KeyLine.dcd)
                    Text("DSR").tag(KeyLine.dsr)
                    Text("RI").tag(KeyLine.ri)
                }
                .onChange(of: config.keyLine) { _ in push() }
                polarityToggle("Key active high", $config.keyActiveHigh)

                // Radio OUTPUT line (we key the radio) — UCI150 default RTS.
                Picker("Radio line (out)", selection: $config.radioLine) {
                    Text("RTS").tag(RadioLine.rts)
                    Text("DTR").tag(RadioLine.dtr)
                }
                .onChange(of: config.radioLine) { _ in push() }
                polarityToggle("Radio active high", $config.radioActiveHigh)

                Stepper(value: $config.debounceMs, in: 0...500, step: 5) {
                    Text("Debounce: \(config.debounceMs) ms")
                        .font(.caption.monospacedDigit())
                }
                .onChange(of: config.debounceMs) { _ in push() }
            }
        }

        private var rxGroup: some View {
            // RX keying is always level-driven (the radio keys the transmitter from
            // incoming network audio). Floor = squelch threshold; hang = how long it
            // stays keyed after the audio stops. The old "RX key mode" picker was
            // removed — remote-PTT keying doesn't fire for most AllStar nodes.
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Text("RX squelch floor")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Slider(value: $config.rxFloorDb, in: -90...0) { editing in
                        if !editing { push() }
                    }
                    Text("\(Int(config.rxFloorDb)) dB")
                        .font(.caption.monospacedDigit())
                        .frame(width: 50, alignment: .trailing)
                }
                Stepper(value: $config.rxHangMs, in: 0...2000, step: 50) {
                    Text("RX hang: \(config.rxHangMs) ms")
                        .font(.caption.monospacedDigit())
                }
                .onChange(of: config.rxHangMs) { _ in push() }
            }
        }

        private func polarityToggle(_ title: String, _ value: Binding<Bool>) -> some View {
            Toggle(title, isOn: value)
                .toggleStyle(.checkbox)
                .font(.caption)
                .onChange(of: value.wrappedValue) { _ in push() }
        }

        // MARK: - Onboarding

        /// Raw USB is the default transport, so first-run setup installs no
        /// driver at all (iax-c7e1): the UCI150 profile pins `transportRaw: 1`
        /// and `SerialConfig` falls back to `.usb` when nothing is recorded.
        /// The tty path — the only one wanting WCH's CH34x cask — has no UI and
        /// is reachable only by setting the `serial.transport` default by hand,
        /// so onboarding does not mention a driver.
        private var onboarding: some View {
            VStack(alignment: .leading, spacing: 6) {
                Text("First time? To use the AllScan UCI150:")
                    .font(.caption.weight(.medium))
                onboardStep("1.", "On the UCI150, set the PTT DEST switch to CTS.")
                onboardStep("2.", "Plug in the UCI150, then turn on the toggle above.")
                onboardStep(
                    "3.", "That's it — no driver needed. astar talks to it over raw USB.")
            }
            .padding(.top, 2)
        }

        private func onboardStep(_ num: String, _ text: String) -> some View {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(num)
                    .font(.caption2.monospacedDigit().weight(.semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 14, alignment: .trailing)
                Text(text)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }

        // MARK: - Push edits to the controller (persists + re-opens if enabled)

        private func push() { serial.update(config) }
    }
#endif
