// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Everything the SSTV picture panel shows, as plain values, so the panel
    /// can be drawn without a session behind it.
    struct SstvPanelContent: Equatable {
        /// The finished picture's pixels; `nil` while it airs, once lost, or
        /// when they could not be read (the PNG is on disk either way).
        var image: CGImage?
        /// The mode's display name, such as `"PD120"`.
        var mode: String
        /// Who sent it, when the network says.
        var talker: String?
        /// When the picture started.
        var time: Date
        /// Rows heard so far, and the picture's full height in rows.
        var lines: Int
        var height: Int
        /// The picture's width in pixels, from the engine; 0 when unknown.
        var width: Int = 0
        /// How long the picture takes to send and how long it has been
        /// airing, in seconds, while it airs.
        var expectedSecs: Double?
        var elapsedSecs: Double?
        /// The picture has ended: saved, partly saved, or lost.
        var finished: Bool
        /// It ended with nothing worth keeping.
        var lost: Bool
        /// It ended before its last row; the rows heard were kept.
        var partial: Bool = false

        static func == (a: SstvPanelContent, b: SstvPanelContent) -> Bool {
            a.mode == b.mode && a.talker == b.talker && a.time == b.time && a.lines == b.lines
                && a.height == b.height && a.width == b.width && a.expectedSecs == b.expectedSecs
                && a.elapsedSecs == b.elapsedSecs && a.finished == b.finished
                && a.lost == b.lost && a.partial == b.partial && a.image === b.image
        }

        /// How far through the picture is, 0...1: by the clock while it airs,
        /// by rows once it has ended.
        var progress: Double {
            if !finished, let expectedSecs, expectedSecs > 0, let elapsedSecs {
                return min(max(elapsedSecs / expectedSecs, 0), 1)
            }
            guard height > 0 else { return finished ? 1 : 0 }
            return min(max(Double(lines) / Double(height), 0), 1)
        }

        /// Width over height for the picture's frame. The picture's own when
        /// it has one; while it airs, the mode's size as the engine reports it
        /// (PD160 is 512×400, PD290 800×616), and 5:4 when that is unknown.
        var aspectRatio: CGFloat {
            CGFloat(
                SstvLive.aspectRatio(
                    imageWidth: image?.width ?? 0, imageHeight: image?.height ?? 0,
                    width: width, height: height))
        }

        /// The status line under the picture.
        var statusText: String {
            if lost { return "Picture lost" }
            if finished { return partial ? "Partial picture saved" : "Saved" }
            if let expectedSecs, expectedSecs > 0, let elapsedSecs {
                return
                    "Receiving \(mode) · \(Self.clock(elapsedSecs)) of \(Self.clock(expectedSecs))"
            }
            if height > 0 { return "Receiving \(lines) of \(height) lines" }
            return "Receiving \(mode)"
        }

        /// `mode · talker · time`, the talker left out when nobody is named.
        var caption: String {
            var parts = [mode]
            if let talker, !talker.isEmpty { parts.append(talker) }
            parts.append(time.formatted(date: .omitted, time: .shortened))
            return parts.joined(separator: " · ")
        }

        /// `m:ss`, whole seconds, never negative.
        static func clock(_ seconds: Double) -> String {
            let whole = Int(max(seconds, 0).rounded(.down))
            return String(format: "%d:%02d", whole / 60, whole % 60)
        }
    }

    /// The SSTV picture panel on the call card: the picture coming in or just
    /// received, its mode, talker and time, and how it ended.
    ///
    /// While a picture airs the decoder has no pixels to show yet, so the
    /// frame fills from the top as the clock runs, with a thin line at the
    /// row it has reached. The picture itself appears when it ends.
    struct SstvPanelView: View {
        let content: SstvPanelContent
        let onDismiss: () -> Void
        let onShowAll: () -> Void

        /// Tall enough to read a picture, short enough that a 640×496 one in a
        /// wide window leaves the call card in view.
        private static let maxPictureHeight: CGFloat = 300

        var body: some View {
            VStack(alignment: .leading, spacing: 8) {
                header
                picture
                footer
            }
            .padding(.horizontal, 14)
            .padding(.top, 8)
            .padding(.bottom, 10)
            .accessibilityElement(children: .contain)
            .accessibilityLabel("SSTV picture")
        }

        private var header: some View {
            HStack(spacing: 6) {
                Image(systemName: "photo.on.rectangle")
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Text("SSTV")
                    .font(.callout.weight(.semibold))
                Spacer(minLength: 8)
                Button("All Pictures", action: onShowAll)
                    .buttonStyle(.link)
                    .font(.callout)
                    .help("Open the window of every picture received")
                Button(action: onDismiss) {
                    Image(systemName: "xmark.circle.fill")
                        .symbolRenderingMode(.hierarchical)
                        .foregroundStyle(.secondary)
                        .font(.body)
                }
                .buttonStyle(.plain)
                .help("Dismiss picture")
                .accessibilityLabel("Dismiss picture")
            }
        }

        private var picture: some View {
            let shape = RoundedRectangle(cornerRadius: 6, style: .continuous)
            return ZStack(alignment: .top) {
                shape.fill(Color.secondary.opacity(0.12))
                if let image = content.image {
                    Image(decorative: image, scale: 1)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                } else if content.lost {
                    placeholder(symbol: "exclamationmark.triangle", tint: .secondary)
                } else if content.finished {
                    placeholder(symbol: "photo", tint: .secondary)
                } else {
                    scan
                }
            }
            .aspectRatio(content.aspectRatio, contentMode: .fit)
            .frame(maxHeight: Self.maxPictureHeight)
            .clipShape(shape)
            .overlay(shape.strokeBorder(Color.primary.opacity(0.08), lineWidth: 1))
            .frame(maxWidth: .infinity)
            .accessibilityElement()
            .accessibilityLabel(pictureLabel)
            .accessibilityValue(content.finished ? "" : percent)
            .accessibilityAddTraits(content.image == nil ? [] : .isImage)
        }

        /// The airing picture: the rows the clock says have arrived, tinted,
        /// with a thin accent line at the edge still coming in.
        private var scan: some View {
            GeometryReader { geo in
                let reached = geo.size.height * content.progress
                ZStack(alignment: .top) {
                    LinearGradient(
                        colors: [Color.accentColor.opacity(0.10), Color.accentColor.opacity(0.22)],
                        startPoint: .top, endPoint: .bottom
                    )
                    .frame(height: reached)
                    .frame(maxWidth: .infinity, alignment: .top)
                    Rectangle()
                        .fill(Color.accentColor)
                        .frame(height: 1.5)
                        .offset(y: max(reached - 0.75, 0))
                    Image(systemName: "photo")
                        .font(.system(size: 28, weight: .light))
                        .foregroundStyle(.tertiary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
                .frame(width: geo.size.width, height: geo.size.height, alignment: .top)
            }
            .animation(.linear(duration: 0.5), value: content.progress)
        }

        private func placeholder(symbol: String, tint: Color) -> some View {
            Image(systemName: symbol)
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(tint)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }

        private var footer: some View {
            VStack(alignment: .leading, spacing: 2) {
                status
                    .font(.callout)
                Text(content.caption)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }

        @ViewBuilder private var status: some View {
            HStack(spacing: 4) {
                if let symbol = statusSymbol {
                    Image(systemName: symbol)
                        .foregroundStyle(statusTint)
                        .accessibilityHidden(true)
                }
                Text(content.statusText)
                    .monospacedDigit()
            }
            .lineLimit(1)
        }

        private var statusSymbol: String? {
            if content.lost { return "xmark.circle.fill" }
            if content.finished {
                return content.partial ? "exclamationmark.circle.fill" : "checkmark.circle.fill"
            }
            return nil
        }

        private var statusTint: Color {
            if content.lost { return .red }
            return content.partial ? .orange : .green
        }

        private var pictureLabel: String {
            SstvLive.pictureLabel(
                mode: content.mode, talker: content.talker, finished: content.finished,
                lost: content.lost, partial: content.partial)
        }

        private var percent: String {
            "\(Int((content.progress * 100).rounded())) percent"
        }
    }
#endif
