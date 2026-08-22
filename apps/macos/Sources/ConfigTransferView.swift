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

        @EnvironmentObject private var setups: SetupController
        /// Bumped so the favorites UI re-renders — the node directory is not
        /// `@Published`, so an import into it is otherwise invisible until the
        /// pane is rebuilt.
        @Binding var directoryRevision: Int

        /// Which sheet is up. ONE piece of state driving ONE `.sheet`: stacking
        /// two `.sheet` modifiers on a single view makes SwiftUI race the
        /// dismiss of one against the present of the other, which showed up as
        /// the import chooser closing, reopening and closing again on Import.
        @State private var sheet: SheetKind?

        private enum SheetKind: String, Identifiable {
            case export
            case importChooser
            var id: String { rawValue }
        }
        @State private var chosen: Set<ConfigSection> = [.rigs, .settings]
        @State private var status: String?
        @State private var failure: String?

        /// The decoded file waiting on the import chooser, and the sections
        /// ticked in it. Held rather than applied straight away so you can take
        /// just the node directory out of a full backup without its devices.
        @State private var pending: ConfigArchive?
        @State private var pendingName = ""
        @State private var chosenForImport: Set<ConfigSection> = []

        var body: some View {
            Section("Backup") {
                VStack(alignment: .leading, spacing: 8) {
                    HStack(spacing: 8) {
                        Button("Export…") { sheet = .export }
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
            .sheet(item: $sheet) { kind in
                switch kind {
                case .export: exportSheet
                case .importChooser: importSheet
                }
            }
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
                    Button("Cancel") { sheet = nil }
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
            sheet = nil
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
                guard !archive.presentSections.isEmpty else {
                    failure = "That configuration is empty — there is nothing to import."
                    return
                }
                pendingName = url.lastPathComponent
                // Default to everything the file has; untick to take a subset.
                chosenForImport = archive.presentSections
                pending = archive
                sheet = .importChooser
            } catch {
                failure = error.localizedDescription
            }
        }

        private var importSheet: some View {
            VStack(alignment: .leading, spacing: 14) {
                Text("Import configuration").font(.headline)
                Text(pendingName)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text("Choose what to bring in. Anything you leave off is untouched.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)

                VStack(alignment: .leading, spacing: 10) {
                    // Only what this file actually holds — offering a section it
                    // lacks would be a checkbox that does nothing.
                    ForEach(
                        ConfigSection.allCases.filter {
                            pending?.presentSections.contains($0) == true
                        }, id: \.self
                    ) { section in
                        Toggle(isOn: importBinding(for: section)) {
                            VStack(alignment: .leading, spacing: 1) {
                                Text(sectionLabel(section))
                                Text(section.detail)
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .toggleStyle(.checkbox)
                    }
                }

                Label(
                    "Importing adds and updates. Nothing already on this Mac is deleted.",
                    systemImage: "arrow.triangle.merge"
                )
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

                HStack {
                    Spacer()
                    Button("Cancel") {
                        sheet = nil
                        pending = nil
                    }
                    .keyboardShortcut(.cancelAction)
                    Button("Import", action: applyPending)
                        .keyboardShortcut(.defaultAction)
                        .disabled(chosenForImport.isEmpty)
                }
            }
            .padding(20)
            .frame(width: 380)
        }

        /// Section title plus what the file holds for it, so "Node directory
        /// (15 nodes)" tells you what you are agreeing to before you agree.
        private func sectionLabel(_ section: ConfigSection) -> String {
            guard let pending else { return section.title }
            switch section {
            case .rigs:
                let n = pending.rigs?.setups.count ?? 0
                let m = pending.rigs?.micProfiles.count ?? 0
                var bits = ["\(n) config\(n == 1 ? "" : "s")"]
                if m > 0 { bits.append("\(m) mic profile\(m == 1 ? "" : "s")") }
                return "\(section.title) (\(bits.joined(separator: ", ")))"
            case .directory:
                let n = pending.directory?.count ?? 0
                return "\(section.title) (\(n) node\(n == 1 ? "" : "s"))"
            case .settings:
                return "\(section.title) (\(pending.settings?.count ?? 0) settings)"
            case .callsign:
                return "\(section.title) (\(pending.callsign ?? ""))"
            case .interface:
                return "\(section.title) (\(pending.interface?.count ?? 0) settings)"
            }
        }

        private func importBinding(for section: ConfigSection) -> Binding<Bool> {
            Binding(
                get: { chosenForImport.contains(section) },
                set: { on in
                    if on {
                        chosenForImport.insert(section)
                    } else {
                        chosenForImport.remove(section)
                    }
                })
        }

        private func applyPending() {
            guard let archive = pending else { return }
            sheet = nil
            pending = nil
            status = nil
            failure = nil
            do {
                let summary = ConfigTransfer.apply(
                    archive.filtered(to: chosenForImport), session: session,
                    setupStore: UserDefaultsSetupStore(),
                    profileStore: UserDefaultsMicProfileStore(),
                    defaults: .standard)
                // An import writes straight to the stores, behind the live
                // controllers' backs. Without these two the imported configs do
                // not appear until relaunch, the ★ renders against a stale
                // defaultID, and the next controller write persists that stale
                // value over what was just imported.
                setups.reloadFromStore()
                directoryRevision += 1
                // Report what changed, not "imported" — a re-imported backup
                // legitimately changes nothing, and saying "imported" would
                // leave the user unsure whether it took.
                status = summary.description
            }
        }
    }
#endif
