// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import ImageIO
    import SwiftUI

    /// The SSTV Pictures window's content: every saved picture as a
    /// thumbnail, newest first, with its mode, network target, talker and
    /// time beneath. Double-click or the context menu shows it in Finder.
    struct SstvGalleryView: View {
        let items: [SstvGalleryItem]
        let onShowInFinder: (URL) -> Void

        private let columns = [GridItem(.adaptive(minimum: 168, maximum: 260), spacing: 16)]

        var body: some View {
            if items.isEmpty {
                empty
            } else {
                ScrollView {
                    LazyVGrid(columns: columns, alignment: .leading, spacing: 18) {
                        ForEach(items) { item in
                            SstvGalleryCell(item: item, onShowInFinder: onShowInFinder)
                        }
                    }
                    .padding(16)
                }
            }
        }

        private var empty: some View {
            VStack(spacing: 8) {
                Image(systemName: "photo.on.rectangle.angled")
                    .font(.system(size: 40, weight: .light))
                    .foregroundStyle(.tertiary)
                    .accessibilityHidden(true)
                Text("No pictures yet")
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(.secondary)
                Text("SSTV pictures heard on AllStar and EchoLink appear here.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            .padding(24)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    /// One picture: its thumbnail at the picture's own shape, and two caption
    /// lines under it.
    private struct SstvGalleryCell: View {
        let item: SstvGalleryItem
        let onShowInFinder: (URL) -> Void

        var body: some View {
            let shape = RoundedRectangle(cornerRadius: 6, style: .continuous)
            VStack(alignment: .leading, spacing: 6) {
                // One shape for every cell, filled edge to edge as Photos
                // does: a 640×496 picture loses a sliver at its sides rather
                // than the grid going ragged. The panel and Finder show it
                // whole.
                Color.secondary.opacity(0.12)
                    .aspectRatio(5.0 / 4.0, contentMode: .fit)
                    .overlay {
                        if let image = SstvThumbnails.shared.thumbnail(for: item.file) {
                            Image(decorative: image, scale: 1)
                                .resizable()
                                .interpolation(.high)
                                .aspectRatio(contentMode: .fill)
                        } else {
                            Image(systemName: "photo")
                                .font(.system(size: 24, weight: .light))
                                .foregroundStyle(.tertiary)
                        }
                    }
                    .clipShape(shape)
                    .overlay(shape.strokeBorder(Color.primary.opacity(0.08), lineWidth: 1))
                    .overlay(alignment: .topTrailing) {
                        if item.partial {
                            Text("Partial")
                                .font(.caption2.weight(.semibold))
                                .padding(.horizontal, 6)
                                .padding(.vertical, 2)
                                // Black on orange reads at about 9:1 in
                                // both appearances; white managed barely 2:1.
                                .background(Color.orange, in: Capsule())
                                .foregroundStyle(.black)
                                .padding(6)
                                .accessibilityHidden(true)
                        }
                    }

                VStack(alignment: .leading, spacing: 1) {
                    Text(Self.title(item))
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Text(Self.subtitle(item))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            .contentShape(Rectangle())
            .onTapGesture(count: 2) { onShowInFinder(item.file) }
            .contextMenu {
                Button("Show in Finder") { onShowInFinder(item.file) }
            }
            .help(item.file.lastPathComponent)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(Self.accessibilityLabel(item))
            .accessibilityAddTraits(.isImage)
            .accessibilityAction(named: "Show in Finder") { onShowInFinder(item.file) }
        }

        /// `mode · network target`: what it was and where it was heard.
        static func title(_ item: SstvGalleryItem) -> String {
            let place = [item.network, item.target].filter { !$0.isEmpty }.joined(separator: " ")
            return place.isEmpty ? item.mode : "\(item.mode) · \(place)"
        }

        /// `talker · time`, the talker left out when nobody is named.
        static func subtitle(_ item: SstvGalleryItem) -> String {
            var parts: [String] = []
            if let talker = item.talker, !talker.isEmpty { parts.append(talker) }
            parts.append(item.date.formatted(date: .abbreviated, time: .shortened))
            return parts.joined(separator: " · ")
        }

        static func accessibilityLabel(_ item: SstvGalleryItem) -> String {
            var label = "\(item.mode) picture"
            if let talker = item.talker, !talker.isEmpty { label += " from \(talker)" }
            let place = [item.network, item.target].filter { !$0.isEmpty }.joined(separator: " ")
            if !place.isEmpty { label += " on \(place)" }
            label += ", \(item.date.formatted(date: .abbreviated, time: .shortened))"
            if item.partial { label += ", partial" }
            return label
        }
    }

    /// Downscaled thumbnails, read once per file. SSTV pictures are small
    /// (640×496 at most), so a synchronous read costs little, and a grid of
    /// them only reads the cells on screen.
    final class SstvThumbnails {
        static let shared = SstvThumbnails()

        private let cache = NSCache<NSURL, CGImage>()
        /// Wide enough for the largest grid cell on a Retina screen.
        private let maxPixels = 520

        init() { cache.countLimit = 400 }

        func thumbnail(for url: URL) -> CGImage? {
            if let hit = cache.object(forKey: url as NSURL) { return hit }
            guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
                let image = CGImageSourceCreateThumbnailAtIndex(
                    source, 0,
                    [
                        kCGImageSourceCreateThumbnailFromImageAlways: true,
                        kCGImageSourceThumbnailMaxPixelSize: maxPixels,
                        kCGImageSourceShouldCacheImmediately: true,
                    ] as CFDictionary)
            else { return nil }
            cache.setObject(image, forKey: url as NSURL)
            return image
        }
    }
#endif
