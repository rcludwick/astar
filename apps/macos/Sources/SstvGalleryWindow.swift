// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import SwiftUI

    /// The SSTV Pictures window's root: reads the SSTV folder's index and
    /// hands the pictures to `SstvGalleryView`.
    ///
    /// It reads again whenever a new picture is saved, the folder changes,
    /// or the window comes to the front (a picture deleted in Finder drops
    /// out then). The read runs off the main thread: the index grows by one
    /// line per picture, forever.
    struct SstvGalleryWindowContent: View {
        @EnvironmentObject private var session: CallSession
        @State private var items: [SstvGalleryItem] = []
        /// Bumped on each request so only the latest read lands.
        @State private var generation = 0

        var body: some View {
            SstvGalleryView(items: items) { file in
                NSWorkspace.shared.activateFileViewerSelecting([file])
            }
            .frame(minWidth: 380, minHeight: 300)
            .onAppear(perform: reload)
            .onChange(of: session.sstvSettings.folder) { _ in reload() }
            .onChange(of: session.sstvLive?.file) { _ in reload() }
            .onReceive(
                NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)
            ) { note in
                if (note.object as? NSWindow)?.title == StatusItemController.sstvGalleryTitle {
                    reload()
                }
            }
        }

        private func reload() {
            generation &+= 1
            let token = generation
            let folder = session.sstvSettings.folder
            DispatchQueue.global(qos: .userInitiated).async {
                let loaded = SstvGallery.load(folder: folder)
                DispatchQueue.main.async {
                    guard token == generation else { return }
                    items = loaded
                }
            }
        }
    }

    extension SstvPanelContent {
        /// The panel's view of the session's picture.
        init(_ live: SstvLive) {
            self.init(
                image: live.image, mode: live.mode, talker: live.talker, time: live.startedAt,
                lines: live.lines, height: live.height, width: live.width,
                expectedSecs: live.expectedSecs,
                elapsedSecs: live.elapsedSecs, finished: live.finished, lost: live.lost,
                partial: live.partial)
        }
    }
#endif
