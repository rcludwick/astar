// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarCore
import SwiftUI

/// A scrolling time-history graph of the TX and RX audio levels (dBFS), beyond
/// the instantaneous meters. Its `TimelineView` samples `(txDB, rxDB)` from the
/// session's `CallMeters` on a timer into a `LevelHistory` ring buffer, and
/// draws the two series as filled line graphs that scroll right-to-left over
/// time. The timeline is the only redraw driver — the view deliberately does
/// not observe the session, so meter churn can't re-render it off-schedule
/// (astar-3e04).
///
/// TX is red and RX is green to match the `LevelMeter` bars in the popover.
/// −60…0 dBFS maps to the graph height; a thin baseline anchors the floor.
struct LevelGraphView: View {
    /// The live session whose meters feed the graph.
    let session: CallSession

    /// How many samples the window holds (window seconds = capacity ÷ 20 Hz —
    /// an honest knob only since astar-aafc fixed the schedule; see `anchor`).
    /// 40 = the 2 s window Rob settled on (astar-5879).
    var capacity: Int = 40
    /// How often to sample the meters into the history. 20 Hz — the session's
    /// poll rate, so every sample is fresh.
    var sampleInterval: TimeInterval = 0.05

    @State private var history = LevelHistory(capacity: 40)

    /// Fixed anchor for the periodic schedule, captured once per view identity.
    /// It MUST NOT be `.now`: the schedule expression re-evaluates on every
    /// body re-render, and every `history.append` re-renders the body — with
    /// `.now` each append re-anchored the schedule to the present, which fired
    /// a fresh date immediately, appending again in a feedback loop measured
    /// at ~346 Hz (astar-aafc). A stable anchor makes re-renders reproduce the
    /// same schedule, so ticks arrive at `sampleInterval` as intended.
    @State private var anchor = Date()

    private let txColor = Color.red
    private let rxColor = Color.green

    var body: some View {
        TimelineView(.periodic(from: anchor, by: sampleInterval)) { context in
            Canvas { ctx, size in
                draw(in: ctx, size: size)
            }
            .onChange(of: context.date) { _ in
                // TX = transmitted audio: sample the mic only while keyed, else
                // floor (the mic stays open for metering and never reads 0).
                // Raw levels, not the published ones: those are quantized to
                // 0.5 dB for the VU bars, which staircased the trace (astar-3458).
                history.append(
                    tx: session.ptt ? session.meters.rawTxDB : -60, rx: session.meters.rawRxDB)
            }
        }
        .background(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(.quaternary.opacity(0.5))
        )
        .overlay(alignment: .topLeading) { legend }
        .frame(height: 64)
        .onAppear {
            if history.capacity != capacity {
                history = LevelHistory(capacity: capacity)
            }
        }
    }

    // MARK: - Legend

    private var legend: some View {
        HStack(spacing: 10) {
            legendChip("TX", txColor)
            legendChip("RX", rxColor)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 4)
        // astar-a9c3 F5: decorative — a color key for a Canvas graph that has
        // no accessibility representation of its own (F14, out of scope here).
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

    // MARK: - Drawing

    private func draw(in ctx: GraphicsContext, size: CGSize) {
        // Thin baseline at the floor.
        var baseline = Path()
        baseline.move(to: CGPoint(x: 0, y: size.height - 0.5))
        baseline.addLine(to: CGPoint(x: size.width, y: size.height - 0.5))
        ctx.stroke(baseline, with: .color(.secondary.opacity(0.35)), lineWidth: 1)

        let samples = history.samples
        guard samples.count > 1 else { return }

        // RX first (drawn underneath), then TX on top.
        drawSeries(samples.map(\.rx), color: rxColor, in: ctx, size: size)
        drawSeries(samples.map(\.tx), color: txColor, in: ctx, size: size)
    }

    /// Map one dBFS series to a filled area + line across the full width.
    private func drawSeries(_ series: [Float], color: Color, in ctx: GraphicsContext, size: CGSize)
    {
        let count = series.count
        guard count > 1 else { return }

        let stepX = size.width / CGFloat(capacity - 1)
        // Right-align so newest samples sit at the right edge and scroll left.
        let xOffset = size.width - CGFloat(count - 1) * stepX

        func point(_ i: Int) -> CGPoint {
            let frac = CGFloat(max(0, min(1, (series[i] + 60) / 60)))
            let x = xOffset + CGFloat(i) * stepX
            let y = size.height - frac * size.height
            return CGPoint(x: x, y: y)
        }

        var line = Path()
        line.move(to: point(0))
        for i in 1..<count { line.addLine(to: point(i)) }

        // Filled area under the line.
        var area = line
        area.addLine(to: CGPoint(x: point(count - 1).x, y: size.height))
        area.addLine(to: CGPoint(x: point(0).x, y: size.height))
        area.closeSubpath()

        ctx.fill(
            area,
            with: .linearGradient(
                Gradient(colors: [color.opacity(0.35), color.opacity(0.04)]),
                startPoint: CGPoint(x: 0, y: 0),
                endPoint: CGPoint(x: 0, y: size.height)
            )
        )
        ctx.stroke(line, with: .color(color), style: StrokeStyle(lineWidth: 1.5, lineJoin: .round))
    }
}

#if DEBUG
    #Preview("Sample data") {
        // Feed a history a synthetic keying burst so the preview shows shape.
        LevelGraphPreviewHarness()
            .padding()
            .frame(width: 288)
    }

    /// Drives a `LevelGraphView` with a `CallSession` over `NullStation`, plus a
    /// pre-seeded illustrative shape via a wrapping view for the static preview.
    private struct LevelGraphPreviewHarness: View {
        @StateObject private var session = CallSession(station: NullStation())

        var body: some View {
            LevelGraphView(session: session, capacity: 80, sampleInterval: 0.05)
        }
    }
#endif
