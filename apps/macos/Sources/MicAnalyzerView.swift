// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The Mic Analyzer pane's content: pick a mic, watch the live log-frequency
    /// spectrum, run a stay-silent Analyze, name + Save the resulting profile (or
    /// switch to the no-filter default).
    ///
    /// A pane of the main window, so it carries no chrome of its own — the header
    /// and its Back chevron belong to `MenuPopover.micAnalyzerPane` — and no
    /// minimum size: it has to lay out from the window's 310 pt minimum up.
    struct MicAnalyzerView: View {
        /// The app's device list, already enumerated off the main thread — the
        /// pane appears on a user gesture, and CoreAudio enumeration on the main
        /// thread is a visible hitch on that path.
        @EnvironmentObject private var deviceMonitor: AudioDeviceMonitor
        @ObservedObject var vm: MicCharacterization
        @State private var inputs: [String] = []
        /// The detection threshold persists across visits to the pane — an
        /// operator who found the right level for their shack shouldn't re-find
        /// it. The view model holds the live value; this is only its durable seed.
        /// (The earlier relative slider's key, `micAnalyzer.peakMarginDb`, is
        /// deliberately abandoned — not migrated, it meant a different thing.)
        @AppStorage("micAnalyzer.thresholdDbfs") private var storedThreshold: Double = -60
        /// Claimed from `vm.nameFocusPending` — every "+" entry point asks for
        /// focus here, since the field lives in the view and the model can't
        /// reach into it directly.
        @FocusState private var nameFocused: Bool

        var body: some View {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    Text("Microphone").foregroundStyle(.secondary)
                    Picker("Microphone", selection: $vm.selectedInput) {
                        Text("System Default").tag(String?.none)
                        ForEach(inputs, id: \.self) { Text($0).tag(String?.some($0)) }
                    }
                    .labelsHidden()
                    // Capped, not fixed: long device names get room in a wide
                    // window and the popup compresses (with its own ellipsis)
                    // rather than clipping in a narrow one.
                    .frame(maxWidth: 320)
                    .onChange(of: vm.selectedInput) { _ in
                        vm.clear()  // switching mics → drop the previous mic's results
                        vm.start(input: vm.selectedInput)
                    }
                    Spacer(minLength: 0)
                }

                HStack {
                    Text("Profile name").foregroundStyle(.secondary)
                    TextField("e.g. fake icom", text: $vm.profileName)
                        .textFieldStyle(.roundedBorder)
                        .frame(maxWidth: 220)
                        .focused($nameFocused)
                    Spacer(minLength: 0)
                }

                SpectrumCanvas(
                    bins: vm.spectrum, peaks: vm.detectedPeaks,
                    thresholdDbfs: vm.thresholdDbfs, backgroundDbfs: vm.floorMedianDb
                )
                .frame(minHeight: 220)
                .background(Color.black.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))

                thresholdSlider

                controls

                if let err = vm.lastError {
                    Text(err).font(.caption).foregroundStyle(.orange)
                }
                Spacer()
            }
            .padding(16)
            .onAppear {
                inputs = deviceMonitor.inputs
                // Guarded: an unconditional assignment fires the slider's onChange,
                // which would discard a result the pane already held.
                if vm.thresholdDbfs != storedThreshold { vm.thresholdDbfs = storedThreshold }
                // A seeded device that is no longer plugged in has no row in the
                // picker, which would render blank over a pane monitoring the
                // system default anyway. Fall back to it explicitly instead.
                if let want = vm.selectedInput, !inputs.contains(want) {
                    vm.selectedInput = nil
                }
                vm.start(input: vm.selectedInput)
                // Catches a "+" pressed from OUTSIDE the pane: `startNew` sets
                // the flag and then shows the pane in the same call, so the view
                // is born with it already true and never sees an `onChange`.
                claimNameFocus()
            }
            .onDisappear { vm.stop() }
            // Catches a "+" pressed from INSIDE the pane (the analyzer header's
            // own "+"): the view is already alive, so this is the one that fires.
            .onChange(of: vm.nameFocusPending) { _ in claimNameFocus() }
        }

        /// Move focus to the name field if a "+" asked for it, and mark the ask
        /// handled. The guard is what stops `onChange` from re-entering when this
        /// same method just cleared the flag it's observing.
        private func claimNameFocus() {
            guard vm.nameFocusPending else { return }
            nameFocused = true
            vm.nameFocusPending = false
        }

        /// The absolute level a bin has to exceed to be notched. Sits directly
        /// under the canvas because the canvas draws it on the same axis: drag the
        /// slider, watch the orange line move, and every peak still standing above
        /// it is a peak Analyze will notch. The grey line under it is the ambient
        /// noise, there to aim by — not part of the decision.
        ///
        /// Like `controls`, it folds rather than clips at the window's 310 pt
        /// minimum: one line while the slider still has room, then label + value on
        /// one line with the slider under them.
        private var thresholdSlider: some View {
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 10) {
                    thresholdSliderLabel
                    // The minimum is what makes the fold real: a Slider is fully
                    // flexible, so ViewThatFits would measure this row's ideal width
                    // with the slider at nothing and always "fit" it.
                    slider.frame(minWidth: 120)
                    thresholdSliderValue
                }
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 10) {
                        thresholdSliderLabel
                        Spacer(minLength: 0)
                        thresholdSliderValue
                    }
                    slider
                }
            }
        }

        // The two Texts are the slider's visible label and value, so they are
        // hidden from VoiceOver: the Slider itself carries both, and stays the
        // adjustable element rather than being merged into a static group.
        private var thresholdSliderLabel: some View {
            Text("Threshold")
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
        }

        private var thresholdSliderValue: some View {
            Text(SpectrumAxis.dbfsLabel(vm.thresholdDbfs))
                .font(.body.monospacedDigit())
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
        }

        private var slider: some View {
            Slider(value: $vm.thresholdDbfs, in: -100...(-20), step: 1)
                // Without this the label and value win the row's width and the
                // slider collapses to nothing before the fold ever triggers.
                .layoutPriority(1)
                .accessibilityLabel("Detection threshold")
                // A plain hyphen, not the typographic minus the visible label
                // draws: VoiceOver reads this one as "minus".
                .accessibilityValue("\(Int(vm.thresholdDbfs)) dBFS")
                .onChange(of: vm.thresholdDbfs) { newValue in
                    storedThreshold = newValue
                    // A result captured at the old threshold no longer matches the
                    // line now drawn (or the notches Save would write), so drop
                    // it rather than let the two silently disagree.
                    if vm.hasResult { vm.cancel() }
                }
        }

        /// Analyze / Save / Clear and the harmonic-comb switch.
        ///
        /// The pane is as narrow as the window's 310 pt minimum, so the row folds
        /// instead of clipping: one line while it fits, then the switch drops to a
        /// second line, then the buttons shorten (the instruction moves to their
        /// tooltip). There is no Close button — the pane header's Back chevron
        /// (⌘[) is the way out, the same as every other pane.
        ///
        /// The "Saved" confirmation sits on its own line rather than in the button
        /// row: it appears only after a save, and folding it into the row would
        /// make the row's width depend on state the fold was measured without.
        @ViewBuilder
        private var controls: some View {
            if vm.analyzing {
                HStack(spacing: 10) {
                    ProgressView().controlSize(.small)
                    Text("Analyzing… stay silent").foregroundStyle(.secondary)
                    Spacer(minLength: 0)
                }
            } else {
                VStack(alignment: .leading, spacing: 8) {
                    ViewThatFits(in: .horizontal) {
                        HStack(spacing: 10) {
                            actionButtons(shortTitles: false)
                            harmonicCombToggle
                        }
                        VStack(alignment: .leading, spacing: 8) {
                            HStack(spacing: 10) { actionButtons(shortTitles: false) }
                            harmonicCombToggle
                        }
                        VStack(alignment: .leading, spacing: 8) {
                            HStack(spacing: 8) { actionButtons(shortTitles: true) }
                            harmonicCombToggle
                        }
                    }
                    // What Analyze actually found, before the operator decides to
                    // Save: the measured floor and either the notches or the
                    // pass-through verdict.
                    if vm.hasResult, let readout = vm.floorReadout {
                        Text(readout)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if vm.saved {
                        Label(
                            vm.savedPassThrough
                                ? "Saved as pass-through — no extra correction for this mic."
                                : "Saved",
                            systemImage: "checkmark.circle.fill"
                        )
                        .foregroundStyle(.green)
                        .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        }

        @ViewBuilder
        private func actionButtons(shortTitles: Bool) -> some View {
            Button(shortTitles ? "Analyze" : "Analyze (stay silent)") { vm.analyze() }
                .help("Stay silent while the analyzer measures the mic's noise")
            Button(shortTitles ? "Save" : "Save mic profile") { vm.save(now: Date()) }
                .disabled(!vm.canSave)
                .help("Save the detected frequencies as a named mic profile")
            Button("Clear") { vm.clear() }
                .disabled(!vm.hasResult && vm.detectedPeaks.isEmpty && vm.profileName.isEmpty)
                .help("Clear the detected frequencies to test another mic")
        }

        private var harmonicCombToggle: some View {
            Toggle("Harmonic comb", isOn: $vm.harmonicComb)
                .toggleStyle(.checkbox)
                .help("Experimental harmonic-aware notch detection")
        }
    }

    /// Shared log-frequency / dBFS axis math for the voice-band spectrum canvases.
    /// Mirrors the engine's spectrum band (SPECTRUM_LO_HZ…SPECTRUM_HI_HZ) so the
    /// single-series mic analyzer and the overlaid in-call TX/RX FFT agree.
    enum SpectrumAxis {
        static let fLo = 100.0, fHi = 3900.0

        /// A dBFS level as the UI spells it: a typographic minus (U+2212), which
        /// aligns with digits in a monospaced-digit font where a hyphen does not.
        /// The slider's value and the canvas's line tags share it so the number
        /// under the graph and the number on the graph read identically.
        static func dbfsLabel(_ db: Double) -> String {
            let n = Int(db.rounded())
            return n < 0 ? "−\(-n) dBFS" : "\(n) dBFS"
        }

        /// y for a dBFS value over a fixed −120…0 range (0 dB at the top).
        static func y(_ db: Float, height: CGFloat) -> CGFloat {
            let clamped = min(0, max(-120, db))
            return height * CGFloat(1 - (clamped + 120) / 120)
        }

        /// Fractional x position (0…1) of frequency `f`, **snapped to the engine's log
        /// bin** so a marker/label sits exactly on the equal-width bar for that bin.
        /// Mirrors the engine's `idx = floor(pos · binCount)` mapping; otherwise a
        /// tone drawn in bin `idx` (at `idx/(binCount-1)`) and a marker at the
        /// continuous `pos` disagree by up to a bin width.
        static func binFraction(_ f: Double, binCount: Int) -> Double? {
            guard f > 0, binCount > 1 else { return nil }
            let pos = (log(f) - log(fLo)) / (log(fHi) - log(fLo))
            let clamped = min(1, max(0, pos))
            let idx = min(Int(clamped * Double(binCount)), binCount - 1)
            return Double(idx) / Double(binCount - 1)
        }

        /// 0 / −60 / −120 dB gridlines, drawn faint across the canvas.
        static func drawGridlines(in ctx: GraphicsContext, size: CGSize) {
            for db in [Float(0), -60, -120] {
                var line = Path()
                line.move(to: CGPoint(x: 0, y: y(db, height: size.height)))
                line.addLine(to: CGPoint(x: size.width, y: y(db, height: size.height)))
                ctx.stroke(line, with: .color(.secondary.opacity(0.25)), lineWidth: 0.5)
            }
        }

        /// Frequency-axis labels, spread along the bottom at their log positions.
        static func drawFrequencyTicks(in ctx: GraphicsContext, size: CGSize, binCount: Int) {
            let ticks: [(String, Double)] = [
                ("100", 100), ("500", 500),
                ("1k", 1000), ("2k", 2000), ("3.9k", 3900),
            ]
            for (i, tick) in ticks.enumerated() {
                guard let frac = binFraction(tick.1, binCount: binCount) else { continue }
                let anchor: UnitPoint =
                    i == 0
                    ? .bottomLeading
                    : (i == ticks.count - 1 ? .bottomTrailing : .bottom)
                ctx.draw(
                    Text(tick.0).font(.caption2).foregroundColor(.secondary),
                    at: CGPoint(x: CGFloat(frac) * size.width, y: size.height),
                    anchor: anchor)
            }
        }
    }

    /// Draws peak-held dBFS bins (-120…0) as a filled area on a log frequency axis
    /// (~100 Hz–3.9 kHz), with red markers at the notch frequencies a profile would
    /// filter and two horizontal reference lines: the **background** noise (grey,
    /// where the mic's ambient sits) and the **detection threshold** (orange, the
    /// level Analyze will notch above).
    ///
    /// The threshold is not an estimate. It is an absolute dBFS level, and the
    /// engine measures each bin in this display's own normalisation (a full-scale
    /// sine reads 0 dBFS whatever the FFT length), so a peak drawn above the orange
    /// line is a peak the detector counts. The grey line is informational — the
    /// noise the tone has to be picked out of, not part of the decision.
    private struct SpectrumCanvas: View {
        let bins: [Float]
        var peaks: [Double] = []
        /// The absolute level the detector will notch above — the slider's value,
        /// drawn on the canvas's own dBFS axis. `nil` draws no line.
        var thresholdDbfs: Double?
        /// The one-second average of the scan-band median, from the view model —
        /// smoothed there so the line holds still instead of jittering with the
        /// peak hold. `nil` draws no line.
        var backgroundDbfs: Float?

        private static func binFraction(_ f: Double, binCount: Int) -> Double? {
            SpectrumAxis.binFraction(f, binCount: binCount)
        }

        var body: some View {
            Canvas { ctx, size in
                guard bins.count > 1 else { return }
                func y(_ db: Float) -> CGFloat { SpectrumAxis.y(db, height: size.height) }
                let dx = size.width / CGFloat(bins.count - 1)
                var path = Path()
                path.move(to: CGPoint(x: 0, y: size.height))
                for (i, db) in bins.enumerated() {
                    path.addLine(to: CGPoint(x: CGFloat(i) * dx, y: y(db)))
                }
                path.addLine(to: CGPoint(x: size.width, y: size.height))
                path.closeSubpath()
                ctx.fill(
                    path,
                    with: .linearGradient(
                        Gradient(colors: [.green.opacity(0.7), .green.opacity(0.15)]),
                        startPoint: CGPoint(x: 0, y: 0), endPoint: CGPoint(x: 0, y: size.height)))
                SpectrumAxis.drawGridlines(in: ctx, size: size)
                // Background then threshold, both under the notch markers so a
                // caught peak is drawn over the line that caught it.
                drawLevelLines(in: ctx, size: size)
                // Notch markers the profile would filter, labelled with their
                // frequency (Hz) in red at the top.
                // Every notch gets its marker line; a label only when there is
                // room for it. Notches cluster (a hum comb puts several within
                // a few tens of Hz), and stacked labels overprint into a smear —
                // so labels go left to right and one is skipped when it would
                // land within `labelGap` points of the last one drawn.
                let labelGap: CGFloat = 22
                var lastLabelX: CGFloat = -.infinity
                let placed = peaks.compactMap { f -> (Double, CGFloat)? in
                    Self.binFraction(f, binCount: bins.count).map { (f, CGFloat($0) * size.width) }
                }
                for (f, x) in placed.sorted(by: { $0.1 < $1.1 }) {
                    var marker = Path()
                    marker.move(to: CGPoint(x: x, y: 11))  // leave room for the label
                    marker.addLine(to: CGPoint(x: x, y: size.height))
                    ctx.stroke(
                        marker, with: .color(.red.opacity(0.7)),
                        style: StrokeStyle(lineWidth: 1.5, dash: [3, 2]))
                    guard x - lastLabelX >= labelGap else { continue }
                    lastLabelX = x
                    let anchor: UnitPoint =
                        x < 16
                        ? .topLeading
                        : (x > size.width - 16 ? .topTrailing : .top)
                    ctx.draw(
                        Text("\(Int(f))").font(.system(size: 9)).foregroundColor(.red),
                        at: CGPoint(x: x, y: 0), anchor: anchor)
                }
                SpectrumAxis.drawFrequencyTicks(in: ctx, size: size, binCount: bins.count)
            }
        }

        /// The two reference lines and their tags. The background is drawn first
        /// (under the threshold, and quieter: grey, finely dashed, tagged at the
        /// LEFT edge) so the orange threshold stays the line the eye goes to; its
        /// tag sits at the RIGHT edge. Neither says "(est.)" any more — in this
        /// canvas's units a tone reads the same level on both sides.
        ///
        /// Nothing is drawn before the mic delivers bins.
        private func drawLevelLines(in ctx: GraphicsContext, size: CGSize) {
            let yBackground = backgroundDbfs.map { SpectrumAxis.y($0, height: size.height) }
            let yThreshold = thresholdDbfs.map { SpectrumAxis.y(Float($0), height: size.height) }
            // Close enough that two tags on the same side of their lines would
            // collide: push them apart — background below its line, threshold above
            // its own — so both stay readable when the noise reaches the threshold.
            let crowded: Bool = {
                guard let a = yBackground, let b = yThreshold else { return false }
                return abs(a - b) < 10
            }()

            if let y = yBackground, let db = backgroundDbfs {
                stroke(y, .secondary.opacity(0.7), dash: [2, 3], in: ctx, size: size)
                // Above its line normally; below when crowded, or when the line
                // rides so high there is no room for a tag over it.
                tag(
                    "background \(SpectrumAxis.dbfsLabel(Double(db)))",
                    color: .secondary, y: y, trailing: false, below: crowded || y <= 14,
                    in: ctx, size: size)
            }
            if let y = yThreshold, let db = thresholdDbfs {
                stroke(y, .orange.opacity(0.8), dash: [4, 3], in: ctx, size: size)
                // Above the line unless it rides too high to fit a tag there; when
                // that happens and the two are crowded, drop further so the
                // background's own below-tag still has its row.
                let below = y <= 14
                tag(
                    "threshold \(SpectrumAxis.dbfsLabel(db))",
                    color: .orange, y: y, trailing: true, below: below,
                    gap: below && crowded ? 13 : 2, in: ctx, size: size)
            }
        }

        /// One full-width horizontal reference line.
        private func stroke(
            _ y: CGFloat, _ color: Color, dash: [CGFloat], in ctx: GraphicsContext, size: CGSize
        ) {
            var line = Path()
            line.move(to: CGPoint(x: 0, y: y))
            line.addLine(to: CGPoint(x: size.width, y: y))
            ctx.stroke(line, with: .color(color), style: StrokeStyle(lineWidth: 1, dash: dash))
        }

        /// A reference line's caption, hugging one edge and clearing its line.
        private func tag(
            _ text: String, color: Color, y: CGFloat, trailing: Bool, below: Bool,
            gap: CGFloat = 2, in ctx: GraphicsContext, size: CGSize
        ) {
            let anchor: UnitPoint =
                below
                ? (trailing ? .topTrailing : .topLeading)
                : (trailing ? .bottomTrailing : .bottomLeading)
            // Clear of the frequency ticks along the bottom: a line down at the
            // canvas floor would otherwise print its tag straight through them.
            let baseline = min(y + (below ? gap : -gap), size.height - 12)
            ctx.draw(
                Text(text).font(.system(size: 9)).foregroundColor(color),
                at: CGPoint(x: trailing ? size.width - 2 : 2, y: baseline),
                anchor: anchor)
        }
    }

    /// Overlays TWO peak-held dBFS spectra on one shared log-frequency / −120…0 dBFS
    /// canvas for the in-call view (astar-8b5b): TX red, RX green, each a tinted line
    /// over a faint fill, with a small TX/RX legend. Reuses `SpectrumAxis` so it
    /// agrees exactly with the single-series mic-analyzer canvas. Empty series (e.g.
    /// `NullStation`, or the engine not yet delivering) simply draw nothing.
    struct OverlaidSpectrumCanvas: View {
        let tx: [Float]
        let rx: [Float]

        private let txColor = Color.red
        private let rxColor = Color.green

        var body: some View {
            Canvas { ctx, size in
                SpectrumAxis.drawGridlines(in: ctx, size: size)
                // RX first (underneath), then TX on top, matching the LevelGraph/VU
                // ordering so the keyed (TX) trace stays visible over RX.
                drawSeries(rx, color: rxColor, in: ctx, size: size)
                drawSeries(tx, color: txColor, in: ctx, size: size)
                let binCount = max(tx.count, rx.count)
                if binCount > 1 {
                    SpectrumAxis.drawFrequencyTicks(in: ctx, size: size, binCount: binCount)
                }
            }
            .overlay(alignment: .topLeading) { legend }
        }

        private func drawSeries(
            _ bins: [Float], color: Color, in ctx: GraphicsContext, size: CGSize
        ) {
            guard bins.count > 1 else { return }
            func y(_ db: Float) -> CGFloat { SpectrumAxis.y(db, height: size.height) }
            let dx = size.width / CGFloat(bins.count - 1)

            var line = Path()
            line.move(to: CGPoint(x: 0, y: y(bins[0])))
            for (i, db) in bins.enumerated() where i > 0 {
                line.addLine(to: CGPoint(x: CGFloat(i) * dx, y: y(db)))
            }
            // Faint fill under the trace so two overlaid series stay readable.
            var area = line
            area.addLine(to: CGPoint(x: size.width, y: size.height))
            area.addLine(to: CGPoint(x: 0, y: size.height))
            area.closeSubpath()
            ctx.fill(
                area,
                with: .linearGradient(
                    Gradient(colors: [color.opacity(0.28), color.opacity(0.04)]),
                    startPoint: CGPoint(x: 0, y: 0), endPoint: CGPoint(x: 0, y: size.height)))
            ctx.stroke(
                line, with: .color(color),
                style: StrokeStyle(lineWidth: 1.5, lineJoin: .round))
        }

        private var legend: some View {
            HStack(spacing: 10) {
                legendChip("TX", txColor)
                legendChip("RX", rxColor)
            }
            .padding(.horizontal, 6)
            .padding(.vertical, 4)
            // astar-a9c3 F5: decorative — see LevelGraphView's identical legend.
            .accessibilityHidden(true)
        }

        private func legendChip(_ label: String, _ color: Color) -> some View {
            HStack(spacing: 4) {
                Circle().fill(color).frame(width: 6, height: 6)
                Text(label)
                    .font(.caption2.weight(.semibold).monospaced())
                    .foregroundStyle(.secondary)
            }
        }
    }
#endif
