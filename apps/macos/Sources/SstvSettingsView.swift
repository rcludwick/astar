// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import SwiftUI

    /// Settings → SSTV: whether heard audio is decoded for SSTV pictures,
    /// which folder they are saved in, and the way into the gallery.
    ///
    /// Writes straight into `session.sstvSettings`, whose setter saves and
    /// tells the engine. These keys stay on this Mac: no `.astarconfig`
    /// section carries them.
    struct SstvSettingsView: View {
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var navigation: AppNavigation
        /// Why Show in Finder could not make the folder, shown under the
        /// folder row until the next attempt or a new folder.
        @State private var folderError: String?

        var body: some View {
            SettingsSection(.sstv) {
                VStack(alignment: .leading, spacing: 10) {
                    VStack(alignment: .leading, spacing: 2) {
                        Toggle("Decode SSTV pictures", isOn: $session.sstvSettings.enabled)
                        Text(
                            "Saves pictures sent as SSTV on AllStar and EchoLink. M17 and D-Star voice codecs can’t carry them."
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    }

                    VStack(alignment: .leading, spacing: 2) {
                        folderLine
                        if let folderError {
                            Text(folderError)
                                .font(.caption)
                                .foregroundStyle(.red)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        if let error = session.sstvError {
                            Text(error)
                                .font(.caption)
                                .foregroundStyle(.red)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }

                    Button {
                        navigation.showSstvGallery()
                    } label: {
                        Label("Show Pictures", systemImage: "photo.on.rectangle.angled")
                    }
                    .controlSize(.small)
                    .help("Open the window of every picture received")
                }
                .font(.callout)
                .listRowSeparator(.hidden)
            }
        }

        /// The folder's Finder name, with Choose… and Show in Finder beside
        /// it. The full path is the tooltip and the VoiceOver value.
        private var folderLine: some View {
            let folder = session.sstvSettings.folder
            return HStack(spacing: 8) {
                Image(systemName: "folder")
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Text(FileManager.default.displayName(atPath: folder.path))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(folder.path)
                    .accessibilityLabel("SSTV pictures folder")
                    .accessibilityValue(folder.path)
                Spacer(minLength: 8)
                Button("Choose…", action: chooseFolder)
                    .controlSize(.small)
                Button("Show in Finder") { showInFinder(folder) }
                    .controlSize(.small)
            }
        }

        /// A sheet on the settings window rather than `runModal()`, which
        /// would stall the session's poll timer.
        private func chooseFolder() {
            let panel = NSOpenPanel()
            panel.canChooseDirectories = true
            panel.canChooseFiles = false
            panel.canCreateDirectories = true
            panel.allowsMultipleSelection = false
            panel.prompt = "Choose"
            panel.message = "Choose where astar saves SSTV pictures."
            panel.directoryURL = session.sstvSettings.folder
            let session = self.session
            let chosen: (NSApplication.ModalResponse) -> Void = { response in
                guard response == .OK, let url = panel.url else { return }
                session.sstvSettings.folder = url
                folderError = nil
            }
            if let window = NSApp.keyWindow {
                panel.beginSheetModal(for: window, completionHandler: chosen)
            } else {
                panel.begin(completionHandler: chosen)
            }
        }

        /// Reveal the folder, making it first if no picture has yet.
        private func showInFinder(_ folder: URL) {
            do {
                try FileManager.default.createDirectory(
                    at: folder, withIntermediateDirectories: true)
            } catch {
                NSSound.beep()
                folderError =
                    "Can’t create the folder: \(error.localizedDescription) Choose another one."
                return
            }
            folderError = nil
            NSWorkspace.shared.activateFileViewerSelecting([folder])
        }
    }
#endif
