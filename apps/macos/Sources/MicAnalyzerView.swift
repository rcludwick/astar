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
        /// The noise-floor margin persists across visits to the pane — an operator
        /// who found the right number for their shack shouldn't re-find it. The
        /// view model holds the live value; this is only its durable seed.
        @AppStorage("micAnalyzer.peakMarginDb") private var storedMargin: Double = 12

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
                    Spacer(minLength: 0)
                }

                SpectrumCanvas(
                    bins: vm.spectrum, peaks: vm.detectedPeaks, floorMarginDb: vm.peakMarginDb,
                    floorMedianDb: vm.floorMedianDb
                )
                .frame(minHeight: 220)
                .background(Color.black.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))

                floorSlider

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
                if vm.peakMarginDb != storedMargin { vm.peakMarginDb = storedMargin }
                // A seeded device that is no longer plugged in has no row in the
                // picker, which would render blank over a pane monitoring the
                // system default anyway. Fall back to it explicitly instead.
                if let want = vm.selectedInput, !inputs.contains(want) {
                    vm.selectedInput = nil
                }
                vm.start(input: vm.selectedInput)
            }
            .onDisappear { vm.stop() }
        }

        /// How far above the measured noise floor a bin has to stand to be notched.
        /// Sits directly under the canvas because the canvas draws an *estimate* of
        /// where this puts the detector's threshold: drag the slider, watch the
        /// orange line move over the peaks it would roughly catch.
        ///
        /// Like `controls`, it folds rather than clips at the window's 310 pt
        /// minimum: one line while the slider still has room, then label + value on
        /// one line with the slider under them.
        private var floorSlider: some View {
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 10) {
                    floorSliderLabel
                    // The minimum is what makes the fold real: a Slider is fully
                    // flexible, so ViewThatFits would measure this row's ideal width
                    // with the slider at nothing and always "fit" it.
                    slider.frame(minWidth: 120)
                    floorSliderValue
                }
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 10) {
                        floorSliderLabel
                        Spacer(minLength: 0)
                        floorSliderValue
                    }
                    slider
                }
            }
        }

        // The two Texts are the slider's visible label and value, so they are
        // hidden from VoiceOver: the Slider itself carries both, and stays the
        // adjustable element rather than being merged into a static group.
        private var floorSliderLabel: some View {
            Text("Noise floor")
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
        }

        private var floorSliderValue: some View {
            Text("+\(Int(vm.peakMarginDb)) dB")
                .font(.body.monospacedDigit())
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
        }

        private var slider: some View {
            Slider(value: $vm.peakMarginDb, in: 6...30, step: 1)
                // Without this the label and value win the row's width and the
                // slider collapses to nothing before the fold ever triggers.
                .layoutPriority(1)
                .accessibilityLabel("Noise floor margin")
                .accessibilityValue(
                    "\(Int(vm.peakMarginDb)) decibels above the estimated floor"
                )
                .onChange(of: vm.peakMarginDb) { newValue in
                    storedMargin = newValue
                    // A result captured at the old margin no longer matches the
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
    /// filter and a dashed orange line **estimating** where the current margin puts
    /// the detector's threshold.
    ///
    /// It is an estimate, not the threshold itself. These bins are a 2048-point FFT
    /// max-folded into log bins and peak-held for display; the detector runs a finer
    /// FFT and takes its median over linear frequency. Folding many linear bins into
    /// one log bin keeps the loudest of them, so the drawn line can sit several dB
    /// above the detector's effective threshold — a peak just under the line can
    /// still be notched. The line is for aiming the slider, not for predicting each
    /// notch.
    private struct SpectrumCanvas: View {
        let bins: [Float]
        var peaks: [Double] = []
        /// dB above the estimated floor at which the line is drawn — the same margin
        /// Analyze hands the characterizer, applied to a coarser spectrum.
        /// `nil` draws no line.
        var floorMarginDb: Double?
        /// The one-second average of the scan-band median, from the view model —
        /// smoothed there so the line holds still while the slider still moves it
        /// instantly. `nil` draws no line.
        var floorMedianDb: Float?

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
                // The estimated detection threshold, under the notch markers so a
                // caught peak is drawn over the line that approximates it.
                drawFloorLine(in: ctx, size: size)
                // Notch markers the profile would filter, labelled with their
                // frequency (Hz) in red at the top.
                for f in peaks {
                    guard let frac = Self.binFraction(f, binCount: bins.count) else { continue }
                    let x = CGFloat(frac) * size.width
                    var marker = Path()
                    marker.move(to: CGPoint(x: x, y: 11))  // leave room for the label
                    marker.addLine(to: CGPoint(x: x, y: size.height))
                    ctx.stroke(
                        marker, with: .color(.red.opacity(0.7)),
                        style: StrokeStyle(lineWidth: 1.5, dash: [3, 2]))
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

        /// The dashed threshold line plus its "threshold +N dB (est.)" tag at the
        /// right edge — "(est.)" because this is the display spectrum's estimate of
        /// the detector's threshold, not the threshold itself, and because the
        /// readout's "broadband floor" is a different measurement entirely. Orange
        /// reads over the green fill and in both appearances; nothing is drawn
        /// before the mic delivers bins.
        private func drawFloorLine(in ctx: GraphicsContext, size: CGSize) {
            guard let margin = floorMarginDb, let median = floorMedianDb else { return }
            let db = median + Float(margin)
            let yFloor = SpectrumAxis.y(db, height: size.height)
            var line = Path()
            line.move(to: CGPoint(x: 0, y: yFloor))
            line.addLine(to: CGPoint(x: size.width, y: yFloor))
            ctx.stroke(
                line, with: .color(.orange.opacity(0.8)),
                style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
            // Above the line normally; below it when the floor rides so high there
            // is no room, so the tag never clips off the top of the canvas.
            let above = yFloor > 14
            ctx.draw(
                Text("threshold +\(Int(margin)) dB (est.)")
                    .font(.system(size: 9))
                    .foregroundColor(.orange),
                at: CGPoint(x: size.width - 2, y: yFloor + (above ? -2 : 2)),
                anchor: above ? .bottomTrailing : .topTrailing)
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
