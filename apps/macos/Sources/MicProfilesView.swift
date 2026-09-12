// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Settings section listing the saved **Mic Profiles** — each renameable +
    /// deletable, showing the frequencies it filters — with the Mic Analyzer launcher
    /// underneath. Rendered inside the Settings `List`, below Saved configs.
    struct MicProfilesView: View {
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var micAnalyzer: MicAnalyzerController

        var body: some View {
            Section {
                if session.micProfiles.isEmpty {
                    Text("No mic profiles yet. Click + to open the analyzer and characterize one.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .listRowSeparator(.hidden)
                } else {
                    ForEach(session.micProfiles) { profile in
                        MicProfileRow(profile: profile)
                            .listRowSeparator(.hidden)
                            .listRowInsets(EdgeInsets(top: 4, leading: 6, bottom: 4, trailing: 6))
                    }
                }
            } header: {
                HStack {
                    Text("Mic Profiles")
                    Spacer(minLength: 8)
                    Button {
                        micAnalyzer.startNew(input: nil)
                    } label: {
                        Image(systemName: "plus")
                    }
                    .buttonStyle(.borderless)
                    .help("Add a mic profile")
                    .accessibilityLabel("Add a mic profile")
                }
            }
        }
    }

    /// One saved mic profile: rename (text field), the notch frequencies it filters,
    /// and a trash button. Mirrors the saved-config card styling.
    private struct MicProfileRow: View {
        let profile: MicProfile
        @EnvironmentObject private var session: CallSession
        @State private var name: String

        init(profile: MicProfile) {
            self.profile = profile
            _name = State(initialValue: profile.name)
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    TextField("Name", text: $name)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit(commitRename)
                    Button(role: .destructive) {
                        session.deleteMicProfile(id: profile.id)
                    } label: {
                        Image(systemName: "trash").foregroundStyle(.red)
                    }
                    .buttonStyle(.borderless)
                    .help("Delete this mic profile")
                    // astar-a9c3 F4: icon-only.
                    .accessibilityLabel("Delete this mic profile")
                }
                let freqs = profile.notchFrequencies
                // A profile that notches nothing is a pass-through: the mic was
                // clean at the margin it was analyzed with, so it adds no extra
                // correction. Say that instead of listing an empty filter set.
                Text(
                    freqs.isEmpty
                        ? "pass-through"
                        : "Filters " + freqs.map { "\(Int($0)) Hz" }.joined(separator: ", ")
                )
                .font(.caption.monospacedDigit())
                .foregroundStyle(freqs.isEmpty ? Color.secondary : Color.red)
                .fixedSize(horizontal: false, vertical: true)
            }
            .padding(8)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        }

        private func commitRename() {
            let trimmed = name.trimmingCharacters(in: .whitespaces)
            guard !trimmed.isEmpty else {
                name = profile.name
                return
            }
            session.renameMicProfile(id: profile.id, to: trimmed)
        }
    }
#endif
