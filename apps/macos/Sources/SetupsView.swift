// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Manage named **Saved configs**: each is a one-click rig (hardware + serial +
    /// devices + audio). Cards collapse to a radio + name; expand to edit the whole
    /// config. Editing autosaves; the radio applies; drag to reorder; ★ sets the
    /// launch default. Editing the *active* config also takes effect live.
    struct SetupsView: View {
        @EnvironmentObject private var setups: SetupController
        // Reactive, hotplug-backed device list (no enumeration on appear).
        @EnvironmentObject private var deviceMonitor: AudioDeviceMonitor

        /// Rendered inside the Settings `List`, so `.onMove` gives a native macOS
        /// drag-to-reorder (proper move cursor — no copy "+").
        var body: some View {
            Section("Saved configs") {
                // The built-in System Default always sits at the top (astar-1f7d).
                // It is a real, selectable config — not a hidden fallback — so a
                // fresh install can see what it is running on and get back to a
                // known-good plain-audio state in one click.
                SystemDefaultCard()
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 4, leading: 6, bottom: 4, trailing: 6))

                Button {
                    setups.addNew()
                } label: {
                    Label("Add new config", systemImage: "plus.circle")
                }
                .buttonStyle(.borderless)
                .font(.callout)
                .listRowSeparator(.hidden)

                if setups.managedSetups.isEmpty {
                    Text(
                        "No other configs yet. Add one and expand it to set hardware, devices, and audio."
                    )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .listRowSeparator(.hidden)
                } else {
                    ForEach(setups.managedSetups) { setup in
                        ConfigCard(
                            setup: setup, inputs: deviceMonitor.inputs,
                            outputs: deviceMonitor.outputs
                        )
                        .listRowSeparator(.hidden)
                        .listRowInsets(EdgeInsets(top: 4, leading: 6, bottom: 4, trailing: 6))
                    }
                    .onMove { source, destination in
                        setups.move(fromOffsets: source, toOffset: destination)
                    }
                }

                if let error = setups.lastError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                        .listRowSeparator(.hidden)
                }
            }
        }
    }

    /// The built-in **System Default** row (astar-1f7d): apply it, or ★ it as the
    /// launch default. Deliberately not a `ConfigCard` — it has no stored record to
    /// write back to, so offering the name field, the hardware/device editors, or
    /// the delete button would be offering edits that silently go nowhere. What it
    /// *is* gets said in one caption instead.
    private struct SystemDefaultCard: View {
        @EnvironmentObject private var setups: SetupController

        private var isActive: Bool { setups.selectedID == SystemDefaultSetup.id }
        private var isDefault: Bool { setups.defaultID == SystemDefaultSetup.id }

        var body: some View {
            HStack(spacing: 8) {
                Button {
                    setups.apply(id: SystemDefaultSetup.id)
                } label: {
                    Image(systemName: isActive ? "largecircle.fill.circle" : "circle")
                        .foregroundStyle(isActive ? Color.accentColor : .secondary)
                }
                .buttonStyle(.borderless)
                .help("Apply this config")
                .accessibilityLabel("Apply this config")

                VStack(alignment: .leading, spacing: 1) {
                    Text(SystemDefaultSetup.name)
                        .font(.callout.weight(.medium))
                    Text("Your Mac's current input and output. No serial PTT.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)

                Button {
                    setups.setDefault(isDefault ? nil : SystemDefaultSetup.id)
                } label: {
                    Image(systemName: isDefault ? "star.fill" : "star")
                        .foregroundStyle(isDefault ? .yellow : .secondary)
                }
                .buttonStyle(.borderless)
                .help(isDefault ? "Default config (applied at launch)" : "Set as launch default")
                .accessibilityLabel(
                    isDefault ? "Default config (applied at launch)" : "Set as launch default")
            }
            .padding(10)
            .background(
                Color.secondary.opacity(isActive ? 0.12 : 0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous)
            )
        }
    }

    /// One config card: collapsed (drag handle, radio, name, ★ default, expand,
    /// delete) and, when expanded, the full per-config editor. Holds local slider
    /// state so dragging is smooth; commits (autosave + live-if-active) on release.
    private struct ConfigCard: View {
        let setup: Setup
        let inputs: [String]
        let outputs: [String]

        @EnvironmentObject private var setups: SetupController
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var serial: SerialController
        @EnvironmentObject private var micAnalyzer: MicAnalyzerController

        @State private var expanded = false
        @State private var inputGain = 0.90
        @State private var outputGain = 1.0
        @State private var voxThreshold = -40.0

        private let audioStore = UserDefaultsAudioSettingsStore()
        private static let defaultLabel = "System Default"
        private static let labelWidth: CGFloat = 56

        private var isActive: Bool { setup.id == setups.selectedID }
        private var isDefault: Bool { setup.id == setups.defaultID }
        private var usesSerial: Bool {
            setups.registry.resolve(id: setup.hardwareProfileID).usesSerial
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 0) {
                collapsedRow
                if expanded { expandedBody.padding(.top, 10) }
            }
            .padding(10)
            .background(
                Color.secondary.opacity(isActive ? 0.12 : 0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous)
            )
            .onAppear {
                inputGain = Double(setup.inputGain ?? 0.90)
                outputGain = Double(setup.outputGain ?? 1.0)
                voxThreshold = Double(setup.voxThreshold ?? -40)
            }
            // astar-b167, audit F22: the serial PTT self-test's confirmation
            // (checkmark + green text, `serialSelfTest` above) is visual-only —
            // announce the rising edge so a blind operator setting up a
            // UCI150 gets the exact confirmation the test exists to give.
            // Gated on the SAME composite condition that actually renders
            // `serialSelfTest` below (`expanded` guards `expandedBody`,
            // `usesSerial` guards the `ConfigSerialEditor`/self-test block,
            // `isActive && serial.isActive` guards `serialSelfTest` itself):
            // every saved Setup mounts its own `ConfigCard` — and thus its
            // own subscriber against the one shared `serial.keyDetected` — so
            // without this gate a single key-detected edge would announce
            // once per saved config, including ones whose self-test UI isn't
            // even showing, and ordinary PTT keying during real operation
            // (Settings open, any config active) would stack a redundant
            // "PTT detected" on top of the main planner's "Transmitting".
            .onChange(of: serial.keyDetected) { detected in
                guard expanded, usesSerial, isActive, serial.isActive else { return }
                if detected { AccessibilityAnnouncer.post("PTT detected", priority: .medium) }
            }
        }

        // MARK: - Collapsed

        private var collapsedRow: some View {
            HStack(spacing: 8) {
                Image(systemName: "line.3.horizontal")
                    .foregroundStyle(.tertiary)
                    .help("Drag to reorder")
                    // astar-a9c3 F4: icon-only.
                    .accessibilityLabel("Drag to reorder")

                Button {
                    setups.apply(setup)
                } label: {
                    Image(systemName: isActive ? "largecircle.fill.circle" : "circle")
                        .foregroundStyle(isActive ? Color.accentColor : .secondary)
                }
                .buttonStyle(.borderless)
                .help("Apply this config")
                .accessibilityLabel("Apply this config")

                TextField("Name", text: nameBinding)
                    .textFieldStyle(.roundedBorder)

                Button {
                    setups.setDefault(isDefault ? nil : setup.id)
                } label: {
                    Image(systemName: isDefault ? "star.fill" : "star")
                        .foregroundStyle(isDefault ? .yellow : .secondary)
                }
                .buttonStyle(.borderless)
                .help(isDefault ? "Default config (applied at launch)" : "Set as launch default")
                .accessibilityLabel(
                    isDefault ? "Default config (applied at launch)" : "Set as launch default")

                Button {
                    withAnimation(.easeInOut(duration: 0.15)) { expanded.toggle() }
                } label: {
                    Image(systemName: expanded ? "chevron.up" : "chevron.down")
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.borderless)
                .help(expanded ? "Collapse" : "Expand")
                .accessibilityLabel(expanded ? "Collapse" : "Expand")

                Button(role: .destructive) {
                    setups.delete(id: setup.id)
                } label: {
                    Image(systemName: "trash").foregroundStyle(.red)
                }
                .buttonStyle(.borderless)
                .help("Delete this config")
                .accessibilityLabel("Delete this config")
            }
        }

        // MARK: - Expanded editor

        private var expandedBody: some View {
            VStack(alignment: .leading, spacing: 8) {
                devicePicker("Input", devices: inputs, selection: inputBinding)
                micProfileRow
                devicePicker("Output", devices: outputs, selection: outputBinding)
                gainSlider("Mic", tint: .red, value: $inputGain) { commitInputGain() }
                gainSlider("Vol", tint: .green, value: $outputGain) { commitOutputGain() }

                Toggle("Voice compression", isOn: compressionBinding)
                Toggle("Noise reduction", isOn: noiseReductionBinding)
                Toggle("Full duplex (headphones)", isOn: fullDuplexBinding)
                Toggle("VOX (voice-activated)", isOn: voxBinding)
                    .toggleStyle(.checkbox)
                if setup.voxEnabled == true {
                    voxLevel
                }

                Divider()

                Picker("Radio hardware", selection: hardwareBinding) {
                    Text("None / headset").tag(HardwareProfileRegistry.headsetID)
                    Text("AllScan UCI150").tag(HardwareProfileRegistry.uci150ID)
                    Text("Custom serial").tag(HardwareProfileRegistry.customID)
                }
                if usesSerial {
                    ConfigSerialEditor(spec: serialBinding)
                    // Guided self-test: only meaningful for the live serial device,
                    // i.e. when this is the active config and its port is open.
                    if isActive && serial.isActive {
                        serialSelfTest
                    }
                }
            }
            .toggleStyle(.switch)
            .controlSize(.small)
            .font(.callout)
        }

        private var voxLevel: some View {
            HStack(spacing: 8) {
                label("VOX dB")
                Slider(value: $voxThreshold, in: -60...0) { editing in
                    if !editing { commitVoxThreshold() }
                }
                .tint(.orange)
                .accessibilityLabel("VOX dB")
                .accessibilityValue(AccessibilityValueFormatter.decibels(voxThreshold))
                Text("\(Int(voxThreshold)) dB")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .frame(width: 48, alignment: .trailing)
            }
        }

        /// Guided PTT self-test: the user presses the handset PTT and watches the
        /// live key state flip — verifying the serial CTS input is read end-to-end
        /// (and, by observing the radio's TX light, that the RTS radio line keys).
        private var serialSelfTest: some View {
            VStack(alignment: .leading, spacing: 4) {
                Text("PTT self-test")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                HStack(spacing: 8) {
                    Image(systemName: serial.keyDetected ? "checkmark.circle.fill" : "circle")
                        .foregroundStyle(serial.keyDetected ? .green : .secondary)
                        .imageScale(.large)
                    Text(
                        serial.keyDetected
                            ? "PTT detected — your radio should be keyed (check its TX light)."
                            : "Press and hold your handset PTT."
                    )
                    .font(.caption)
                    .foregroundStyle(serial.keyDetected ? .green : .secondary)
                    .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        }

        /// This config's mic profile: Default (no filter) + every saved profile.
        /// Switching to this config applies the choice; editing it here autosaves
        /// (and applies live if this is the active config).
        private var micProfileRow: some View {
            HStack(spacing: 8) {
                Text("Profile").foregroundStyle(.secondary)
                Picker(
                    "Profile",
                    selection: Binding(
                        get: { setup.micProfileID },
                        set: { v in
                            edit(
                                { $0.micProfileID = v },
                                live: { _ in session.setMicProfileSelection(id: v) })
                        })
                ) {
                    Text("Default (no filter)").tag(String?.none)
                    ForEach(session.micProfiles) { p in
                        Text(p.name).tag(String?.some(p.id))
                    }
                }
                .labelsHidden()
                Button("Analyze…") { micAnalyzer.open(input: setup.inputDevice) }
                    .buttonStyle(.link)
                if let id = setup.micProfileID {
                    Button(role: .destructive) {
                        session.deleteMicProfile(id: id)
                        edit(
                            { $0.micProfileID = nil },
                            live: { _ in session.setMicProfileSelection(id: nil) })
                    } label: {
                        Image(systemName: "trash")
                    }
                    .buttonStyle(.borderless)
                    .help("Delete this mic profile")
                    // astar-a9c3 F4: icon-only.
                    .accessibilityLabel("Delete this mic profile")
                }
                Spacer()
            }
            .font(.caption)
        }

        // MARK: - Editing helpers

        /// Mutate the stored config (autosave); if it's the active rig, run `live`
        /// with the updated config so the change takes effect immediately.
        private func edit(_ mutate: (inout Setup) -> Void, live: (Setup) -> Void) {
            var c = setup
            mutate(&c)
            setups.update(c)
            if isActive { live(c) }
        }

        private var nameBinding: Binding<String> {
            Binding(get: { setup.name }, set: { v in edit({ $0.name = v }, live: { _ in }) })
        }

        private var inputBinding: Binding<String?> {
            Binding(
                get: { setup.inputDevice },
                set: { v in
                    edit(
                        { s in
                            s.inputDevice = v
                            // Pair the speaker to a same-named input (Jabra/UCI150 combo).
                            if let paired = AudioDevicePairing.matchingOutput(
                                forInput: v, in: outputs)
                            {
                                s.outputDevice = paired
                            }
                        }, live: { applyDevicesLive($0) })
                })
        }

        private var outputBinding: Binding<String?> {
            Binding(
                get: { setup.outputDevice },
                set: { v in
                    edit({ $0.outputDevice = v }, live: { applyDevicesLive($0) })
                })
        }

        private var compressionBinding: Binding<Bool> {
            Binding(
                get: { setup.compression ?? false },
                set: { v in edit({ $0.compression = v }, live: { _ in session.setCompression(v) }) }
            )
        }

        private var noiseReductionBinding: Binding<Bool> {
            Binding(
                get: { setup.noiseReduction ?? false },
                set: { v in
                    edit({ $0.noiseReduction = v }, live: { _ in session.setNoiseReduction(v) })
                })
        }

        private var fullDuplexBinding: Binding<Bool> {
            Binding(
                get: { setup.fullDuplex ?? false },
                set: { v in edit({ $0.fullDuplex = v }, live: { _ in session.setFullDuplex(v) }) })
        }

        private var voxBinding: Binding<Bool> {
            Binding(
                get: { setup.voxEnabled ?? false },
                set: { v in edit({ $0.voxEnabled = v }, live: { _ in session.setVoxEnabled(v) }) })
        }

        private var hardwareBinding: Binding<String> {
            Binding(
                get: { setup.hardwareProfileID },
                set: { v in edit({ $0.hardwareProfileID = v }, live: { setups.applyHardware($0) }) }
            )
        }

        private var serialBinding: Binding<SerialLineSpec> {
            Binding(
                get: { setup.serial ?? .uci150 },
                set: { v in edit({ $0.serial = v }, live: { setups.applyHardware($0) }) })
        }

        private func commitInputGain() {
            edit(
                { $0.inputGain = Float(inputGain) },
                live: { _ in
                    try? session.setInputGain(Float(inputGain))
                })
            persistAudio { $0.inputGain = Float(inputGain) }
        }

        private func commitOutputGain() {
            edit(
                { $0.outputGain = Float(outputGain) },
                live: { _ in
                    try? session.setOutputGain(Float(outputGain))
                })
            persistAudio { $0.outputGain = Float(outputGain) }
        }

        private func commitVoxThreshold() {
            edit(
                { $0.voxThreshold = Float(voxThreshold) },
                live: { _ in
                    session.setVoxThreshold(Float(voxThreshold))
                })
        }

        /// Apply + persist device selection when this is the active config (keeps the
        /// quick-config / launch state in sync).
        private func applyDevicesLive(_ s: Setup) {
            try? session.selectDevices(input: s.inputDevice, output: s.outputDevice)
            persistAudio {
                $0.input = s.inputDevice
                $0.output = s.outputDevice
            }
        }

        /// Mirror a live edit into the global AudioSettings only when active, so the
        /// gains/devices the quick config reads stay consistent.
        private func persistAudio(_ mutate: (inout AudioSettings) -> Void) {
            guard isActive else { return }
            var a = audioStore.load()
            mutate(&a)
            audioStore.save(a)
        }

        // MARK: - Small builders

        @ViewBuilder
        private func devicePicker(_ title: String, devices: [String], selection: Binding<String?>)
            -> some View
        {
            HStack(spacing: 8) {
                label(title)
                Picker(title, selection: selection) {
                    Text(Self.defaultLabel).tag(String?.none)
                    ForEach(devices, id: \.self) { Text($0).tag(String?.some($0)) }
                }
                .labelsHidden()
            }
        }

        private func gainSlider(
            _ title: String, tint: Color, value: Binding<Double>,
            commit: @escaping () -> Void
        ) -> some View {
            HStack(spacing: 8) {
                label(title)
                Slider(value: value, in: 0...2) { editing in if !editing { commit() } }
                    .tint(tint)
                    .accessibilityLabel(title)
                    .accessibilityValue(AccessibilityValueFormatter.percent(value.wrappedValue))
                Text(value.wrappedValue, format: .percent.precision(.fractionLength(0)))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .frame(width: 44, alignment: .trailing)
            }
        }

        private func label(_ text: String) -> some View {
            Text(text)
                .foregroundStyle(.secondary)
                .frame(width: Self.labelWidth, alignment: .leading)
        }
    }

    /// Compact per-config serial line editor, bound to a `SerialLineSpec`. RX keying
    /// is always level-driven (the engine forces it), so there's no RX-mode control.
    private struct ConfigSerialEditor: View {
        @Binding var spec: SerialLineSpec
        private static let labelWidth: CGFloat = 80

        private static let keyLines = ["CTS", "DCD", "DSR", "RI"]
        private static let radioLines = ["RTS", "DTR"]

        var body: some View {
            let ports = SerialController.usbSerialPorts()
            let detected = SerialController.detectedPort()
            return VStack(alignment: .leading, spacing: 6) {
                row("Serial port") {
                    Picker("", selection: autodetectBinding(ports: ports, detected: detected)) {
                        Text("Auto-detect").tag(true)
                        Text("Manual").tag(false)
                    }.labelsHidden().fixedSize().pickerStyle(.segmented)
                    if spec.isAutodetect {
                        Text(detected.map { portName($0) } ?? "no device found")
                            .foregroundStyle(detected == nil ? .red : .secondary)
                    } else {
                        Picker("", selection: portBinding) {
                            ForEach(ports, id: \.self) { Text(portName($0)).tag(Optional($0)) }
                            if ports.isEmpty || (spec.portPath.map { !ports.contains($0) } ?? false)
                            {
                                // Keep a stored-but-absent port selectable (USB unplugged).
                                if let p = spec.portPath {
                                    Text("\(portName(p)) (not connected)").tag(Optional(p))
                                }
                                if spec.portPath == nil { Text("no devices").tag(String?.none) }
                            }
                        }.labelsHidden().fixedSize()
                    }
                }
                row("PTT in") {
                    Picker("", selection: keyLine) {
                        ForEach(Array(Self.keyLines.enumerated()), id: \.offset) { i, n in
                            Text(n).tag(UInt32(i))
                        }
                    }.labelsHidden().fixedSize()
                    Toggle("active high", isOn: bool(\.keyActiveHigh)).toggleStyle(.checkbox)
                }
                row("Radio out") {
                    Picker("", selection: radioLine) {
                        ForEach(Array(Self.radioLines.enumerated()), id: \.offset) { i, n in
                            Text(n).tag(UInt32(i))
                        }
                    }.labelsHidden().fixedSize()
                    Toggle("active high", isOn: bool(\.radioActiveHigh)).toggleStyle(.checkbox)
                }
                sliderRow("Debounce", value: uint(\.debounceMs), range: 0...200, unit: "ms")
                sliderRow("RX floor", value: floatVal(\.rxFloorDb), range: -80...0, unit: "dB")
                sliderRow("RX hang", value: uint(\.rxHangMs), range: 0...1000, unit: "ms")
            }
            .font(.caption)
            .padding(8)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        }

        private func row<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
            HStack(spacing: 8) {
                Text(title).foregroundStyle(.secondary).frame(
                    width: Self.labelWidth, alignment: .leading)
                content()
                Spacer()
            }
        }

        private func sliderRow(
            _ title: String, value: Binding<Double>, range: ClosedRange<Double>, unit: String
        ) -> some View {
            HStack(spacing: 8) {
                Text(title).foregroundStyle(.secondary).frame(
                    width: Self.labelWidth, alignment: .leading)
                Slider(value: value, in: range)
                    // astar-a9c3 F3: covers Debounce/RX hang (ms) and RX floor
                    // (dB) — the visible readout's unit tells us which formatter
                    // to use for the announced value.
                    .accessibilityLabel(title)
                    .accessibilityValue(
                        unit == "dB"
                            ? AccessibilityValueFormatter.decibels(value.wrappedValue)
                            : AccessibilityValueFormatter.milliseconds(value.wrappedValue))
                Text("\(Int(value.wrappedValue)) \(unit)")
                    .monospacedDigit().foregroundStyle(.secondary)
                    .frame(width: 56, alignment: .trailing)
            }
        }

        /// Show the short device name (strip the `/dev/` prefix) to keep the row tidy.
        private func portName(_ path: String) -> String {
            path.hasPrefix("/dev/") ? String(path.dropFirst(5)) : path
        }

        /// Auto-detect ⇄ Manual. Flipping to Manual seeds `portPath` with the detected
        /// port (or the first available) so the picker isn't empty.
        private func autodetectBinding(ports: [String], detected: String?) -> Binding<Bool> {
            Binding(
                get: { spec.isAutodetect },
                set: { auto in
                    spec.autodetect = auto
                    if !auto, spec.portPath == nil { spec.portPath = detected ?? ports.first }
                }
            )
        }

        private var portBinding: Binding<String?> {
            Binding(get: { spec.portPath }, set: { spec.portPath = $0 })
        }

        // Field bindings onto the spec.
        private var keyLine: Binding<UInt32> {
            Binding(get: { spec.keyLineRaw }, set: { spec.keyLineRaw = $0 })
        }
        private var radioLine: Binding<UInt32> {
            Binding(get: { spec.radioLineRaw }, set: { spec.radioLineRaw = $0 })
        }
        private func bool(_ kp: WritableKeyPath<SerialLineSpec, Bool>) -> Binding<Bool> {
            Binding(get: { spec[keyPath: kp] }, set: { spec[keyPath: kp] = $0 })
        }
        private func uint(_ kp: WritableKeyPath<SerialLineSpec, UInt32>) -> Binding<Double> {
            Binding(get: { Double(spec[keyPath: kp]) }, set: { spec[keyPath: kp] = UInt32($0) })
        }
        private func floatVal(_ kp: WritableKeyPath<SerialLineSpec, Float>) -> Binding<Double> {
            Binding(get: { Double(spec[keyPath: kp]) }, set: { spec[keyPath: kp] = Float($0) })
        }
    }
#endif
