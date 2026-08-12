// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The Mic Analyzer window content: pick a mic, watch the live log-frequency
    /// spectrum, run a stay-silent Analyze, name + Save the resulting profile (or
    /// switch to the no-filter default).
    struct MicAnalyzerView: View {
        @EnvironmentObject private var session: CallSession
        @ObservedObject var vm: MicCharacterization
        @State private var inputs: [String] = []

        var body: some View {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    Text("Microphone").foregroundStyle(.secondary)
                    Picker("Microphone", selection: $vm.selectedInput) {
                        Text("System Default").tag(String?.none)
                        ForEach(inputs, id: \.self) { Text($0).tag(String?.some($0)) }
                    }
                    .labelsHidden()
                    .onChange(of: vm.selectedInput) { _ in
                        vm.clear()  // switching mics → drop the previous mic's results
                        vm.start(input: vm.selectedInput)
                    }
                    Spacer()
                }

                HStack {
                    Text("Profile name").foregroundStyle(.secondary)
                    TextField("e.g. fake icom", text: $vm.profileName)
                        .textFieldStyle(.roundedBorder)
                        .frame(maxWidth: 220)
                    Spacer()
                }

                SpectrumCanvas(bins: vm.spectrum, peaks: vm.detectedPeaks)
                    .frame(minHeight: 220)
                    .background(Color.black.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))

                controls

                if let err = vm.lastError {
                    Text(err).font(.caption).foregroundStyle(.orange)
                }
                Spacer()
            }
            .padding(16)
            .frame(minWidth: 640, minHeight: 400)
            .onAppear {
                inputs = session.inputs()
                vm.start(input: vm.selectedInput)
            }
            .onDisappear { vm.stop() }
        }

        @ViewBuilder
        private var controls: some View {
            HStack(spacing: 10) {
                if vm.analyzing {
                    ProgressView().controlSize(.small)
                    Text("Analyzing… stay silent").foregroundStyle(.secondary)
                } else {
                    Button("Analyze (stay silent)") { vm.analyze() }
                    Button("Save mic profile") { vm.save(now: Date()) }
                        .disabled(!vm.canSave)
                    Button("Clear") { vm.clear() }
                        .disabled(
                            !vm.hasResult && vm.detectedPeaks.isEmpty && vm.profileName.isEmpty
                        )
                        .help("Clear the detected frequencies to test another mic")
                    if vm.saved {
                        Label("Saved", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                    }
                }
                Spacer()
                Toggle("Harmonic comb", isOn: $vm.harmonicComb)
                    .toggleStyle(.checkbox)
                    .help("Experimental harmonic-aware notch detection")
                Button("Close") { vm.requestClose() }
                    .help("Close the analyzer")
            }
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
    /// filter.
    private struct SpectrumCanvas: View {
        let bins: [Float]
        var peaks: [Double] = []

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
