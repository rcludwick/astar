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
        /// The same for the packet log's folder.
        @State private var packetLogFolderError: String?
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
                }
                .font(.callout)
                .listRowSeparator(.hidden)

                // Codec frames and packet logs are for decoder and network
                // work, not for listening back, so they sit one click further
                // in. Each is its own row of the Settings `List`: a
                // `DisclosureGroup` inside one row toggled, but the row kept
                // its folded height, so nothing appeared beneath it.
                advancedHeader
                    .listRowSeparator(.hidden)
                if advancedExpanded {
                    VStack(alignment: .leading, spacing: 2) {
                        Toggle(
                            "Save codec frames",
                            isOn: $session.recordingSettings.saveCodecFrames)
                        Text(
                            "On digital networks, also keep the raw voice frames, under captured/."
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    }
                    .font(.callout)
                    .padding(.leading, 18)
                    .listRowSeparator(.hidden)

                    Group { packetLogRows }
                        .font(.callout)
                        .padding(.leading, 18)
                        .listRowSeparator(.hidden)
                }
            }
        }

        /// The "Advanced" disclosure row: a triangle and a label, the whole
        /// width clickable, drawn like the section headers above it.
        private var advancedHeader: some View {
            Button {
                withAnimation(.easeInOut(duration: 0.18)) { advancedExpanded.toggle() }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(.secondary)
                        .rotationEffect(.degrees(advancedExpanded ? 90 : 0))
                        .frame(width: 12)
                    Text("Advanced")
                    Spacer(minLength: 8)
                }
                .font(.callout)
                .frame(minHeight: 20)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(
                advancedExpanded ? "Hide the advanced settings" : "Show codec frames and Debug mode"
            )
            .accessibilityLabel("Advanced")
            .accessibilityValue(advancedExpanded ? "expanded" : "collapsed")
            .accessibilityHint(
                advancedExpanded ? "Hides the advanced settings" : "Shows the advanced settings")
        }

        /// The packet log switch and, while it is on, its folder and size cap.
        /// Each is its own `List` row, for the same reason as Advanced: a row
        /// does not grow when Debug mode adds lines inside it.
        @ViewBuilder private var packetLogRows: some View {
            VStack(alignment: .leading, spacing: 2) {
                Toggle(
                    "Debug mode",
                    isOn: $session.recordingSettings.debugMode)
                Text(
                    "Adds a capture button beside Record. Press it during a call to write the packets received on that call to a file Wireshark can open, until you press it again or the call ends. These files hold other people’s voices. Oldest files are deleted past the limit."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            }
            if session.recordingSettings.debugMode {
                VStack(alignment: .leading, spacing: 2) {
                    folderLine(
                        session.recordingSettings.packetLogFolder,
                        label: "Packet logs folder",
                        choose: {
                            chooseFolder(
                                \.packetLogFolder,
                                message: "Choose where astar saves its packet logs.",
                                error: $packetLogFolderError)
                        },
                        show: {
                            showInFinder(
                                session.recordingSettings.packetLogFolder,
                                error: $packetLogFolderError)
                        })
                    if let packetLogFolderError {
                        Text(packetLogFolderError)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                Picker(
                    "Keep at most",
                    selection: $session.recordingSettings.packetLogMaxBytes
                ) {
                    ForEach(RecordingSettings.packetLogLimits, id: \.self) { bytes in
                        Text(Self.sizeLabel(bytes)).tag(bytes)
                    }
                }
                .pickerStyle(.menu)
                if session.packetLogUsedBytes > 0 {
                    Text("Using \(session.packetLogUsedBytes / 1_000_000) MB")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
                if let error = session.packetLogError {
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }

        private static func sizeLabel(_ bytes: Int64) -> String {
            bytes >= 1_000_000_000
                ? "\(bytes / 1_000_000_000) GB" : "\(bytes / 1_000_000) MB"
        }

        /// The folder's Finder name, with Choose… and Show in Finder beside it.
        /// The full path is the row's tooltip and its VoiceOver value, since
        /// the display name alone does not say where the folder lives.
        private var folderRow: some View {
            let folder = session.recordingSettings.folder
            return VStack(alignment: .leading, spacing: 2) {
                folderLine(
                    folder, label: "Recordings folder",
                    choose: {
                        chooseFolder(
                            \.folder, message: "Choose where astar saves its recordings.",
                            error: $folderError)
                    },
                    show: { showInFinder(folder, error: $folderError) })
                if let folderError {
                    Text(folderError)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }

        private func folderLine(
            _ folder: URL, label: String, choose: @escaping () -> Void,
            show: @escaping () -> Void
        ) -> some View {
            HStack(spacing: 8) {
                Image(systemName: "folder")
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Text(FileManager.default.displayName(atPath: folder.path))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(folder.path)
                    .accessibilityLabel(label)
                    .accessibilityValue(folder.path)
                Spacer(minLength: 8)
                Button("Choose…", action: choose)
                    .controlSize(.small)
                Button("Show in Finder", action: show)
                    .controlSize(.small)
            }
        }

        /// A sheet on the settings window rather than `runModal()`: a modal
        /// panel runs the main run loop in its own mode, which would stall
        /// the session's poll timer and freeze the recording clock.
        private func chooseFolder(
            _ keyPath: WritableKeyPath<RecordingSettings, URL>, message: String,
            error: Binding<String?>
        ) {
            let panel = NSOpenPanel()
            panel.canChooseDirectories = true
            panel.canChooseFiles = false
            panel.canCreateDirectories = true
            panel.allowsMultipleSelection = false
            panel.prompt = "Choose"
            panel.message = message
            panel.directoryURL = session.recordingSettings[keyPath: keyPath]
            let session = self.session
            let chosen: (NSApplication.ModalResponse) -> Void = { response in
                guard response == .OK, let url = panel.url else { return }
                session.recordingSettings[keyPath: keyPath] = url
                error.wrappedValue = nil
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
        private func showInFinder(_ folder: URL, error folderError: Binding<String?>) {
            do {
                try FileManager.default.createDirectory(
                    at: folder, withIntermediateDirectories: true)
            } catch {
                NSSound.beep()
                folderError.wrappedValue =
                    "Can’t create the folder: \(error.localizedDescription) Choose another one."
                return
            }
            folderError.wrappedValue = nil
            NSWorkspace.shared.activateFileViewerSelecting([folder])
        }
    }
#endif
