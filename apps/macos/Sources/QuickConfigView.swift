// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The "simple config" shared by the main call window and the top of Settings:
    /// choose a Setup, pick input/output devices, and set output volume + mic gain.
    ///
    /// Rendered in a *fixed* position on the main page (under the status row, above
    /// the dial/PTT region), so it never shifts when a call connects. The same view
    /// sits at the top of Settings; the complex config (create/edit Setups, full
    /// duplex, credentials, serial) lives below it there. Callers supply padding.
    struct QuickConfigView: View {
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var setups: SetupController
        @EnvironmentObject private var micAnalyzer: MicAnalyzerController
        // Reactive device list (CoreAudio hotplug-backed). Reading it is instant —
        // no enumeration on appear — which is what keeps the reveal smooth, and the
        // pickers update live when hardware changes.
        @EnvironmentObject private var deviceMonitor: AudioDeviceMonitor

        /// The network selected in the dial picker (astar-5d8e) — passed in by
        /// the caller rather than read from `@AppStorage` here, since the
        /// picker's persisted key is `MenuPopover`'s concern, not this view's.
        /// Combined with `session.activeCallNetwork` in `m17Context` so the
        /// M17 override stays bound while a call is live even if the picker
        /// itself later moves (defensive; the two are expected to track).
        let networkContext: Network

        /// Whether the five TX-processing controls below (noise reduction,
        /// voice compression, its strength, and TX trim) bind to the M17
        /// override set instead of the shared `AudioSettings` — whenever M17
        /// is the network in play, selected or actively connected (astar-5d8e:
        /// Rob's AllStar-tuned chain fed Codec 2 a quiet, pre-processed signal
        /// and produced a "transmitting from inside a box" echo — M17 gets its
        /// own field-tuned defaults, edited here without disturbing AllStar's).
        private var m17Context: Bool {
            networkContext == .m17 || session.activeCallNetwork == .m17
        }

        @State private var selectedInput: String?
        @State private var selectedOutput: String?
        @State private var inputGain: Double = 0.90
        @State private var outputGain: Double = 1.0
        @State private var deviceError: String?
        /// True while a VOX "Test" is running: the mic monitor is held open and the
        /// Audio Level bar streams the live mic magnitude. Toggled by the Test button;
        /// also released on disappear so the mic is never left open.
        @State private var testing = false

        private let store = UserDefaultsAudioSettingsStore()
        private static let defaultLabel = "System Default"
        // Wide enough for the longest label ("Hang Timeout" / "Audio Level") so the
        // VOX / Audio Level / Hang Timeout rows stay aligned with the other quick
        // settings without truncating.
        private static let labelWidth: CGFloat = 96

        var body: some View {
            VStack(alignment: .leading, spacing: 10) {
                setupRow

                // Duplex is a station behavior, not part of either audio
                // chain — it sits above both so the Mic → Speaker cards
                // read as one continuous input → output audio path.
                groupCard("Station") {
                    switchRow(
                        "Full duplex",
                        isOn: Binding(
                            get: { session.fullDuplex }, set: { session.setFullDuplex($0) })
                    )
                }

                // Input side: device on top, then rows following the TX
                // processing chain top-to-bottom: mic gain → mic profile
                // (feeds the noise reducer) → noise reduction → compression
                // → TX gain (final stage).
                // The "not recommended for M17" caption and "M17" badge
                // (astar-5d8e) were retired here (astar-unwarn): Rob's A/B
                // testing on the air found his AllStar-tuned compression
                // chain actually improved his M17 audio over the raw mic —
                // the per-network override mechanism below stays as-is.
                groupCard("Mic") {
                    devicePicker("In", devices: deviceMonitor.inputs, selection: $selectedInput) {
                        pairOutputToInput()
                        applyDevices()
                    }
                    // The input source caps wideband quality regardless of the
                    // negotiated codec (astar-17b8).
                    .help(
                        "Wideband (slin16) is only as wide as this source: radio and USB-CODEC "
                            + "interfaces such as the UCI150 pass about 300–3400 Hz no matter the "
                            + "codec. Use a direct microphone to send true wideband audio."
                    )
                    .padding(.bottom, 8)
                    gainSlider("Mic Level", tint: .red, value: inputGainBinding) { gain in
                        if !m17Context {
                            try? session.setInputGain(Float(gain))
                            persistGains()
                        }
                    }
                    .padding(.bottom, 8)
                    micProfileRow
                        .padding(.bottom, 8)
                    switchRow("Noise reduction", isOn: noiseReductionBinding)
                        .padding(.bottom, 4)
                    switchRow("Voice compression", isOn: compressionBinding)
                    if compressionOn {
                        HStack(spacing: 8) {
                            sublabel("Strength")
                            Slider(value: compressionLevelBinding, in: 0...1)
                                .tint(.blue)
                                .accessibilityLabel("Strength")
                                .accessibilityValue(
                                    AccessibilityValueFormatter.percent(Double(compressionLevel)))
                            Text("\(Int((compressionLevel * 100).rounded()))%")
                                .font(.caption.monospacedDigit())
                                .foregroundStyle(.secondary)
                                .frame(width: 40, alignment: .trailing)
                        }
                    }
                    // TX Gain (internally txTrim): the always-on final TX
                    // gain stage, applied after compression (100% = unity).
                    HStack(spacing: 8) {
                        label("TX Gain")
                        Slider(value: txTrimBinding, in: 0...2)
                            .tint(.red)
                            .accessibilityLabel("TX Gain")
                            .accessibilityValue(AccessibilityValueFormatter.percent(Double(txTrim)))
                        Text("\(Int((txTrim * 100).rounded()))%")
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                            .frame(width: 40, alignment: .trailing)
                    }
                    .help(
                        "How loud you sound on the air — applied after compression. 100% = no change."
                    )
                    .padding(.top, 12)
                }

                // Output side: device on top, then volume, then RX
                // compression (automatic leveling of the received audio,
                // iax-a4e7) — mirroring how the mic chain's compression row
                // sits under its gain slider. Volume is 100%-400% (floored at
                // unity — no UI attenuation); the internal half-duplex mute
                // still writes 0 straight through the API, unaffected by this
                // floor.
                groupCard("Speaker") {
                    devicePicker("Out", devices: deviceMonitor.outputs, selection: $selectedOutput)
                    {
                        applyDevices()
                    }
                    gainSlider("Vol", tint: .green, range: 1...4, value: $outputGain) {
                        try? session.setOutputGain(Float($0))
                        persistGains()
                    }
                    .padding(.bottom, 8)
                    switchRow("RX compression", isOn: rxCompressionBinding)
                    if session.rxCompression {
                        HStack(spacing: 8) {
                            sublabel("Strength")
                            Slider(value: rxCompressionLevelBinding, in: 0...1)
                                .tint(.blue)
                                .accessibilityLabel("Strength")
                                .accessibilityValue(
                                    AccessibilityValueFormatter.percent(
                                        Double(session.rxCompressionLevel)))
                            Text("\(Int((session.rxCompressionLevel * 100).rounded()))%")
                                .font(.caption.monospacedDigit())
                                .foregroundStyle(.secondary)
                                .frame(width: 40, alignment: .trailing)
                        }
                    }
                }

                groupCard("VOX") {
                    switchRow(
                        "VOX (voice-activated)",
                        isOn: Binding(
                            get: { session.voxEnabled }, set: { session.setVoxEnabled($0) }))
                    if session.voxEnabled {
                        voxCalibration
                    }
                }

                if let s = selectedEditableSetup {
                    Button {
                        setups.saveCurrentToSelected()
                    } label: {
                        Text("Save changes to \(s.name)")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                    .help(
                        "Store the current devices, volume, TX volume, mic gain, compression, noise reduction, and VOX in this config"
                    )
                }
                if let err = deviceError ?? setups.lastError {
                    Label(err, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption2)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .onAppear(perform: load)
            // A Setup applied elsewhere (right-click menu) swaps the devices — keep the
            // pickers in sync while this view is on screen.
            .onReceive(setups.$selectedID) { _ in syncFromStore() }
        }

        /// A labeled rounded sub-card matching the saved-config cards, used to group
        /// the quick settings into Duplex / Mic / Speaker / VOX. An optional `tag`
        /// renders as a small accent capsule next to the title — for making it
        /// obvious which profile a card's controls are editing when it isn't the
        /// standing shared one. Unused for now (astar-5d8e's "M17" badge was
        /// retired here — astar-unwarn — but the capsule stays available for a
        /// future card that needs it).
        @ViewBuilder
        private func groupCard<C: View>(
            _ title: String, tag: String? = nil, @ViewBuilder _ content: () -> C
        )
            -> some View
        {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 6) {
                    Text(title.uppercased())
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                    if let tag {
                        Text(tag)
                            .font(.caption2.weight(.bold))
                            .foregroundStyle(.white)
                            .padding(.horizontal, 5)
                            .padding(.vertical, 1)
                            .background(Color.accentColor, in: Capsule())
                    }
                }
                content()
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        }

        /// A label + trailing switch, so every toggle aligns at the card's right edge
        /// (matching the sliders' value column) instead of floating after the label.
        private func switchRow(_ title: String, isOn: Binding<Bool>) -> some View {
            HStack(spacing: 8) {
                Text(title).font(.callout)
                Spacer(minLength: 8)
                // astar-a9c3 F2: the empty-string label left every switch
                // anonymous to VoiceOver ("off, switch"); passing the title
                // through gives it a real name while `.labelsHidden()` still
                // keeps it out of the visual layout (the sibling Text above is
                // the on-screen label).
                Toggle(title, isOn: isOn)
                    .labelsHidden()
                    .toggleStyle(.switch)
                    .controlSize(.small)
            }
        }

        // MARK: - Rows

        /// Mic profile picker: the built-in Default (no filter) plus every saved
        /// profile. Selecting applies live; Analyze opens the characterizer; trash
        /// deletes the selected profile.
        private var micProfileRow: some View {
            HStack(spacing: 8) {
                label("Mic Profile")
                Picker(
                    "Mic Profile",
                    selection: Binding(
                        get: { session.micProfileID },
                        set: { session.setMicProfileSelection(id: $0) })
                ) {
                    Text("Default (no filter)").tag(String?.none)
                    ForEach(session.micProfiles) { p in
                        Text(p.name).tag(String?.some(p.id))
                    }
                }
                .labelsHidden()
                Button("Analyze…") { micAnalyzer.open(input: selectedInput) }
                    .buttonStyle(.link)
                if let id = session.micProfileID {
                    Button(role: .destructive) {
                        session.deleteMicProfile(id: id)
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
            .font(.callout)
        }

        private var setupRow: some View {
            HStack(spacing: 8) {
                label("Config")
                Picker("Config", selection: setupBinding) {
                    if setups.setups.isEmpty {
                        Text("None").tag(String?.none)
                    }
                    ForEach(setups.setups) { setup in
                        Text(setup.name).tag(String?.some(setup.id))
                    }
                }
                .labelsHidden()
            }
        }

        /// Choosing a Setup applies it (serial + devices). Creating/editing Setups is
        /// in the Settings pane — this control only *picks* an existing one.
        ///
        /// Apply is deferred until after the pop-up's dismiss animation: serial open +
        /// engine `setDevices` briefly block the main thread (single-threaded by
        /// contract, so they can't move off it), and running them while the menu is
        /// still fading froze the fade mid-way. Waiting out the ~250 ms fade keeps it
        /// smooth at the cost of a small delay before the rig actually switches.
        private static let applyDelay: TimeInterval = 0.3

        private var setupBinding: Binding<String?> {
            Binding(
                get: { setups.selectedID },
                set: { newID in
                    guard let id = newID else { return }
                    DispatchQueue.main.asyncAfter(deadline: .now() + Self.applyDelay) {
                        setups.apply(id: id)
                    }
                })
        }

        @ViewBuilder
        private func devicePicker(
            _ title: String, devices: [String],
            selection: Binding<String?>,
            onChange: @escaping () -> Void
        ) -> some View {
            HStack(spacing: 8) {
                label(title)
                Picker(title, selection: selection) {
                    Text(Self.defaultLabel).tag(String?.none)
                    ForEach(devices, id: \.self) { Text($0).tag(String?.some($0)) }
                }
                .labelsHidden()
                .onChange(of: selection.wrappedValue) { _ in onChange() }
            }
        }

        private func gainSlider(
            _ title: String, tint: Color, range: ClosedRange<Double> = 0...2,
            value: Binding<Double>,
            apply: @escaping (Double) -> Void
        ) -> some View {
            HStack(spacing: 8) {
                label(title)
                Slider(value: value, in: range) { editing in
                    if !editing { apply(value.wrappedValue) }
                }
                .tint(tint)
                // astar-a9c3 F3: unlabeled sliders read as a bare percent-of-range
                // to VoiceOver — name it and report the same value the visible
                // readout shows, in words.
                .accessibilityLabel(title)
                .accessibilityValue(AccessibilityValueFormatter.percent(value.wrappedValue))
                Text(value.wrappedValue, format: .percent.precision(.fractionLength(0)))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .frame(width: 40, alignment: .trailing)
            }
        }

        /// VOX trigger-level slider + a live "where's my voice vs. the threshold"
        /// meter. Tap **Test** to open the mic and watch the Audio Level bar cross the
        /// threshold mark as you talk; tap again to stop. The mic is held open only
        /// while a test is running (see `startTest`/`stopTest`), never otherwise.
        private var voxCalibration: some View {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    sublabel("VOX")
                    Slider(value: voxThresholdBinding, in: -60...0)
                        .tint(.orange)
                        .accessibilityLabel("VOX")
                        .accessibilityValue(
                            AccessibilityValueFormatter.decibels(Double(session.voxThresholdDBFS)))
                    Text("\(Int(session.voxThresholdDBFS)) dB")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                        .frame(width: 48, alignment: .trailing)
                }
                // Live mic level row: "Audio Level" label + meter (aligned under the
                // slider track) + the current dBFS readout, matching the VOX/Hang rows.
                VoxLiveLevelRow(
                    meters: session.meters, testing: testing,
                    threshold: session.voxThresholdDBFS, labelWidth: Self.labelWidth)
                // VOX hang time: how long PTT holds after the voice drops below the
                // threshold, so brief pauses don't drop the transmit.
                HStack(spacing: 8) {
                    sublabel("Hang Timeout")
                    Slider(value: voxHangtimeBinding, in: 100...1500, step: 50)
                        .tint(.orange)
                        .accessibilityLabel("Hang Timeout")
                        .accessibilityValue(
                            AccessibilityValueFormatter.milliseconds(
                                Double(session.voxHangtimeMS)))
                    Text("\(session.voxHangtimeMS) ms")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                        .frame(width: 48, alignment: .trailing)
                }
                testControl
            }
            // Safety: if the panel/Settings closes mid-test, release the mic so a
            // running test never leaves it held open.
            .onDisappear(perform: stopTest)
        }

        /// Start a VOX test: open the mic monitor so the Audio Level bar streams the
        /// live mic magnitude. Idempotent. Shares the lane with the Mic Analyzer via
        /// `CallSession`'s reference-counted monitor, so neither closing pulls the mic
        /// from the other.
        private func startTest() {
            guard !testing else { return }
            try? session.monitorRetain(input: selectedInput)
            testing = true
        }

        /// Stop the VOX test: release this view's hold on the mic monitor (balances
        /// `startTest`). The lane only actually closes once the Mic Analyzer has released
        /// too. Idempotent — safe to call on disappear even when not testing.
        private func stopTest() {
            guard testing else { return }
            try? session.monitorRelease()
            testing = false
        }

        /// Live VOX test: a toggle that opens the mic and streams the live level to the
        /// Audio Level bar (Test → Stop), then closes it. Watch the bar against the
        /// orange threshold marker to pick a level your voice clears but room noise
        /// doesn't.
        private var testControl: some View {
            HStack(spacing: 8) {
                Button(testing ? "Stop" : "Test") {
                    if testing { stopTest() } else { startTest() }
                }
                .controlSize(.small)
                if testing {
                    Text("listening — watch the Audio Level bar")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
            }
        }

        // MARK: - M17 override bindings (astar-5d8e)
        //
        // The four TX-processing controls (noise reduction, voice compression,
        // its strength, and TX trim) bind to `session.m17Overrides`/`setM17*`
        // instead of the shared `session.*`/`set*` whenever `m17Context` is
        // true — M17 gets its own field-tuned profile, edited here without
        // touching what AllStar is tuned to.

        /// The mic-level slider's bound value (astar-m17defaults): the M17
        /// override's `inputGain` while `m17Context`, mirroring the four
        /// TX-processing controls below — pushed live on every drag tick via
        /// `setM17InputGain` (matching how `txTrimBinding` et al. bind
        /// directly rather than deferring to `gainSlider`'s release-only
        /// `apply`). Otherwise the local `inputGain` state, unchanged from
        /// before: the release-only path (`persistGains`) still owns the
        /// shared `AudioSettings` value.
        private var inputGainBinding: Binding<Double> {
            Binding(
                get: { m17Context ? Double(session.m17Overrides.inputGain) : inputGain },
                set: { newValue in
                    if m17Context {
                        session.setM17InputGain(Float(newValue))
                    } else {
                        inputGain = newValue
                    }
                })
        }

        private var noiseReductionBinding: Binding<Bool> {
            Binding(
                get: { m17Context ? session.m17Overrides.noiseReduction : session.noiseReduction },
                set: { on in
                    if m17Context {
                        session.setM17NoiseReduction(on)
                    } else {
                        session.setNoiseReduction(on)
                    }
                })
        }

        private var compressionBinding: Binding<Bool> {
            Binding(
                get: { m17Context ? session.m17Overrides.compression : session.compression },
                set: { on in
                    if m17Context {
                        session.setM17Compression(on)
                    } else {
                        session.setCompression(on)
                    }
                })
        }

        /// Whether the compressor is on right now, for whichever profile is
        /// bound — gates the "Strength" sub-row.
        private var compressionOn: Bool {
            m17Context ? session.m17Overrides.compression : session.compression
        }

        /// The live compression strength for whichever profile is bound — the
        /// percent readout next to the slider.
        private var compressionLevel: Float {
            m17Context ? session.m17Overrides.compressionLevel : session.compressionLevel
        }

        /// The live TX trim for whichever profile is bound — the percent
        /// readout next to the slider.
        private var txTrim: Float {
            m17Context ? session.m17Overrides.txTrim : session.txTrim
        }

        private var compressionLevelBinding: Binding<Double> {
            Binding(
                get: { Double(compressionLevel) },
                set: { level in
                    if m17Context {
                        session.setM17CompressionLevel(Float(level))
                    } else {
                        session.setCompressionLevel(Float(level))
                    }
                })
        }

        private var txTrimBinding: Binding<Double> {
            Binding(
                get: { Double(txTrim) },
                set: { gain in
                    if m17Context {
                        session.setM17TxTrim(Float(gain))
                    } else {
                        session.setTxTrim(Float(gain))
                    }
                })
        }

        // RX/output compression (iax-a4e7): shared across networks (output is
        // listener-side), so — unlike the four TX-processing controls above —
        // these bind straight to `session`, never the M17 override.
        private var rxCompressionBinding: Binding<Bool> {
            Binding(
                get: { session.rxCompression },
                set: { session.setRxCompression($0) })
        }

        private var rxCompressionLevelBinding: Binding<Double> {
            Binding(
                get: { Double(session.rxCompressionLevel) },
                set: { session.setRxCompressionLevel(Float($0)) })
        }

        private var voxThresholdBinding: Binding<Double> {
            Binding(
                get: { Double(session.voxThresholdDBFS) },
                set: { session.setVoxThreshold(Float($0)) })
        }

        private var voxHangtimeBinding: Binding<Double> {
            Binding(
                get: { Double(session.voxHangtimeMS) },
                set: { session.setVoxHangtime(Int($0)) })
        }

        /// A top-level row label — primary (white), matching the toggle rows
        /// so every control in a card carries the same visual weight.
        private func label(_ text: String) -> some View {
            Text(text)
                .font(.callout)
                .lineLimit(1)
                .fixedSize(horizontal: true, vertical: false)
                .frame(width: Self.labelWidth, alignment: .leading)
        }

        /// A conditional sub-control label (Strength, the VOX calibration
        /// rows) — secondary (grey) to read subordinate to its parent row.
        private func sublabel(_ text: String) -> some View {
            Text(text)
                .font(.callout)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .fixedSize(horizontal: true, vertical: false)
                .frame(width: Self.labelWidth, alignment: .leading)
        }

        // MARK: - Actions

        private func load() {
            // Devices come from `deviceMonitor` (already populated, off-main) — no
            // enumeration here, so the Quick-settings reveal animates immediately.
            let s = store.load()
            selectedInput = s.input
            selectedOutput = s.output
            inputGain = Double(s.inputGain)
            outputGain = Double(s.outputGain)
        }

        /// Re-read the persisted devices + gains after a Setup applied them.
        private func syncFromStore() {
            let s = store.load()
            selectedInput = s.input
            selectedOutput = s.output
            inputGain = Double(s.inputGain)
            outputGain = Double(s.outputGain)
            deviceError = nil
        }

        /// The applied setup, if it's a real (editable) one — None can't hold state.
        private var selectedEditableSetup: Setup? {
            guard let s = setups.selectedSetup, s.id != SetupController.noneID else { return nil }
            return s
        }

        /// Combined USB gadget (Jabra, UCI150): when the chosen input has a same-named
        /// output, auto-select it so the speaker follows the mic.
        private func pairOutputToInput() {
            // Auto-pair the speaker to a combined USB gadget's mic — but NEVER clobber
            // an output the user has explicitly set to a valid device. Otherwise a
            // programmatic input re-sync (config apply / relaunch) re-fires this and
            // reverts the choice (e.g. UCI150 mic in, separate speakers out).
            if let out = selectedOutput, deviceMonitor.outputs.contains(out) { return }
            if let paired = AudioDevicePairing.matchingOutput(
                forInput: selectedInput, in: deviceMonitor.outputs)
            {
                selectedOutput = paired
            }
        }

        private func applyDevices() {
            do {
                try session.selectDevices(input: selectedInput, output: selectedOutput)
                // re-assert filter on the new lane
                session.applyMicProfile(id: session.micProfileID)
                deviceError = nil
            } catch {
                deviceError = error.localizedDescription
            }
            var s = store.load()
            s.input = selectedInput
            s.output = selectedOutput
            store.save(s)
            // Also write the change into the active config, else the config re-applies
            // its old device at relaunch and the change appears not to persist.
            setups.setActiveDevices(input: selectedInput, output: selectedOutput)
        }

        private func persistGains() {
            var s = store.load()
            s.inputGain = Float(inputGain)
            s.outputGain = Float(outputGain)
            store.save(s)
        }
    }

    /// A live mic-level bar (−60…0 dBFS) with a threshold marker. The fill turns
    /// green once the level reaches the threshold — i.e. when VOX would key — so the
    /// user can set the slider just under their normal speaking level.
    /// The live "Audio Level" row of the VOX calibration block. A leaf that
    /// observes `CallMeters` directly so mic-level ticks during a test re-render
    /// only this row, not the whole settings pane (astar-3e04). Mirrors the
    /// `sublabel(_:)` styling of the sibling rows.
    private struct VoxLiveLevelRow: View {
        @ObservedObject var meters: CallMeters
        let testing: Bool
        let threshold: Float
        let labelWidth: CGFloat

        var body: some View {
            HStack(spacing: 8) {
                Text("Audio Level")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .fixedSize(horizontal: true, vertical: false)
                    .frame(width: labelWidth, alignment: .leading)
                VoxLevelMeter(level: testing ? meters.inputDB : -60, threshold: threshold)
                    .frame(height: 7)
                Text(testing ? "\(Int(meters.inputDB.rounded())) dB" : "—")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .frame(width: 48, alignment: .trailing)
            }
        }
    }

    private struct VoxLevelMeter: View {
        let level: Float  // dBFS
        let threshold: Float  // dBFS

        private func fraction(_ db: Float) -> CGFloat {
            CGFloat(max(0, min(1, (db + 60) / 60)))
        }

        var body: some View {
            GeometryReader { geo in
                let w = geo.size.width
                let keyed = level >= threshold
                ZStack(alignment: .leading) {
                    Capsule().fill(.quaternary)
                    Capsule()
                        .fill((keyed ? Color.green : Color.secondary).opacity(0.85))
                        .frame(width: fraction(level) * w)
                    Rectangle()
                        .fill(Color.orange)
                        .frame(width: 2)
                        .offset(x: fraction(threshold) * w - 1)
                }
            }
        }
    }
#endif
