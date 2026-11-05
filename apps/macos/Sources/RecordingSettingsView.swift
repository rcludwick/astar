// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import AstarStation
    import SwiftUI

    /// Settings → Recording: how a call is laid out on disk, whether every
    /// answered call is recorded by itself, whether the raw codec frames are
    /// kept too, and which folder the files go in.
    ///
    /// Every control writes straight into `session.recordingSettings`, whose
    /// setter saves through the recording store. These keys stay on this Mac:
    /// no `.astarconfig` section carries them.
    struct RecordingSettingsView: View {
        @EnvironmentObject private var session: CallSession
        /// Why Show in Finder could not make the folder, shown under the
        /// folder row until the next attempt or a new folder.
        @State private var folderError: String?
        @State private var advancedExpanded = false

        var body: some View {
            SettingsSection(.recording) {
                VStack(alignment: .leading, spacing: 10) {
                    Picker("Layout", selection: $session.recordingSettings.layout) {
                        Text("Stereo — Them left, you right").tag(RecordingLayout.stereo)
                        Text("Two mono files").tag(RecordingLayout.twoMono)
                        Text("Mixed").tag(RecordingLayout.mixed)
                    }
                    .pickerStyle(.menu)

                    VStack(alignment: .leading, spacing: 2) {
                        Toggle("Always record", isOn: $session.recordingSettings.alwaysRecord)
                        Text("Starts when a connection is answered and stops at hang-up.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }

                    folderRow

                    // Codec frames are for decoder work, not for listening
                    // back, so they sit one click further in.
                    DisclosureGroup("Advanced", isExpanded: $advancedExpanded) {
                        VStack(alignment: .leading, spacing: 2) {
                            Toggle(
                                "Save codec frames",
                                isOn: $session.recordingSettings.saveCodecFrames)
                            Text(
                                "On digital networks, also keep the raw voice frames for unamblify, under captured/."
                            )
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        }
                        .padding(.top, 4)
                    }
                }
                .font(.callout)
                .listRowSeparator(.hidden)
            }
        }

        /// The folder's Finder name, with Choose… and Show in Finder beside it.
        /// The full path is the row's tooltip and its VoiceOver value, since
        /// the display name alone does not say where the folder lives.
        private var folderRow: some View {
            let folder = session.recordingSettings.folder
            return VStack(alignment: .leading, spacing: 2) {
                folderLine(folder)
                if let folderError {
                    Text(folderError)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }

        private func folderLine(_ folder: URL) -> some View {
            HStack(spacing: 8) {
                Image(systemName: "folder")
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Text(FileManager.default.displayName(atPath: folder.path))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(folder.path)
                    .accessibilityLabel("Recordings folder")
                    .accessibilityValue(folder.path)
                Spacer(minLength: 8)
                Button("Choose…", action: chooseFolder)
                    .controlSize(.small)
                Button("Show in Finder", action: showInFinder)
                    .controlSize(.small)
            }
        }

        /// A sheet on the settings window rather than `runModal()`: a modal
        /// panel runs the main run loop in its own mode, which would stall
        /// the session's poll timer and freeze the recording clock.
        private func chooseFolder() {
            let panel = NSOpenPanel()
            panel.canChooseDirectories = true
            panel.canChooseFiles = false
            panel.canCreateDirectories = true
            panel.allowsMultipleSelection = false
            panel.prompt = "Choose"
            panel.message = "Choose where astar saves its recordings."
            panel.directoryURL = session.recordingSettings.folder
            let session = self.session
            let chosen: (NSApplication.ModalResponse) -> Void = { response in
                guard response == .OK, let url = panel.url else { return }
                session.recordingSettings.folder = url
                folderError = nil
            }
            if let window = NSApp.keyWindow {
                panel.beginSheetModal(for: window, completionHandler: chosen)
            } else {
                panel.begin(completionHandler: chosen)
            }
        }

        /// Reveal the folder, making it first if no recording has yet: a
        /// Show in Finder that did nothing would read as broken, so a folder
        /// that cannot be made says why, under the row.
        private func showInFinder() {
            let folder = session.recordingSettings.folder
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
