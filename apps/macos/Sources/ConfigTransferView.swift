// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Settings → **Backup**: export the configuration to a file, or merge one
    /// back in (astar-b52e).
    ///
    /// Export is section-by-section rather than all-or-nothing, because the same
    /// feature serves two different jobs: a personal backup of one Mac, and a
    /// rig you hand to someone else. The checkboxes are what let one file format
    /// do both — most obviously for **Callsign**, which is the field that decides
    /// whether a file identifies you.
    ///
    /// No part of an export ever contains your AllStarLink credentials; they stay
    /// in the Keychain and an import leaves them untouched. The sheet says so
    /// where the user is deciding whether to share the file, not in a doc page.
    struct ConfigTransferView: View {
        @EnvironmentObject private var session: CallSession

        @State private var showingExport = false
        @State private var chosen: Set<ConfigSection> = [.rigs, .settings]
        @State private var status: String?
        @State private var failure: String?

        var body: some View {
            Section("Backup") {
                VStack(alignment: .leading, spacing: 8) {
                    HStack(spacing: 8) {
                        Button("Export…") { showingExport = true }
                        Button("Import…", action: runImport)
                        Spacer()
                    }
                    Text(
                        "Saves your configs, devices and settings to a file. "
                            + "Your AllStarLink account is never included — it stays in the Keychain."
                    )
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)

                    if let status {
                        Label(status, systemImage: "checkmark.circle.fill")
                            .font(.caption2)
                            .foregroundStyle(.green)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if let failure {
                        Label(failure, systemImage: "exclamationmark.triangle.fill")
                            .font(.caption2)
                            .foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .listRowSeparator(.hidden)
            }
            .sheet(isPresented: $showingExport) { exportSheet }
        }

        // MARK: - Export

        private var exportSheet: some View {
            VStack(alignment: .leading, spacing: 14) {
                Text("Export configuration").font(.headline)
                Text("Choose what to include.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                VStack(alignment: .leading, spacing: 10) {
                    ForEach(ConfigSection.allCases, id: \.self) { section in
                        Toggle(isOn: binding(for: section)) {
                            VStack(alignment: .leading, spacing: 1) {
                                Text(section.title)
                                Text(section.detail)
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .toggleStyle(.checkbox)
                    }
                }

                Label(
                    "Your AllStarLink username, node and password are never exported.",
                    systemImage: "lock.fill"
                )
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

                HStack {
                    Spacer()
                    Button("Cancel") { showingExport = false }
                        .keyboardShortcut(.cancelAction)
                    Button("Export…", action: runExport)
                        .keyboardShortcut(.defaultAction)
                        .disabled(chosen.isEmpty)
                }
            }
            .padding(20)
            .frame(width: 380)
        }

        private func binding(for section: ConfigSection) -> Binding<Bool> {
            Binding(
                get: { chosen.contains(section) },
                set: { on in
                    if on { chosen.insert(section) } else { chosen.remove(section) }
                })
        }

        private func runExport() {
            showingExport = false
            status = nil
            failure = nil
            guard
                let url = ConfigTransferPanels.runSavePanel(
                    defaultName: ConfigTransfer.suggestedFilename())
            else { return }
            do {
                let archive = ConfigTransfer.archive(
                    sections: chosen, session: session,
                    setupStore: UserDefaultsSetupStore(),
                    profileStore: UserDefaultsMicProfileStore(),
                    defaults: .standard, appVersion: Self.appVersion)
                try ConfigArchive.encode(archive).write(to: url, options: .atomic)
                status = "Exported to \(url.lastPathComponent)."
            } catch {
                failure = "Could not export: \(error.localizedDescription)"
            }
        }

        private static var appVersion: String? {
            Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        }

        // MARK: - Import

        private func runImport() {
            status = nil
            failure = nil
            guard let url = ConfigTransferPanels.runOpenPanel() else { return }
            do {
                let archive = try ConfigArchive.decode(try Data(contentsOf: url))
                let summary = ConfigTransfer.apply(
                    archive, session: session,
                    setupStore: UserDefaultsSetupStore(),
                    profileStore: UserDefaultsMicProfileStore(),
                    defaults: .standard)
                // Report what changed, not "imported" — a re-imported backup
                // legitimately changes nothing, and saying "imported" would
                // leave the user unsure whether it took.
                status = summary.description
            } catch {
                failure = error.localizedDescription
            }
        }
    }
#endif
