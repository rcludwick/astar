// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The "Reflector directory" section of Settings (astar-refl-ui): what is
    /// loaded, how fresh it is, when it refreshes itself, a Sync Now button —
    /// and the attribution.
    ///
    /// The attribution line is a licence condition, not decoration. hamcall-db
    /// publishes the directory under CC BY 4.0 and passes its upstreams' credit
    /// through in the feed itself; showing it is the term astar accepts by
    /// using the data, and a credit that survives only in a repository file is
    /// one refactor away from being lost.
    ///
    /// This section is also where the launch-time automatic sync reports. That
    /// sync is started and forgotten so it can never delay the UI, which means
    /// it has no caller to throw at — `ReflectorDirectory.lastSyncError` is
    /// where its failure lands, and this is the only place it is visible.
    struct ReflectorSettingsView: View {
        @EnvironmentObject private var reflectors: ReflectorDirectory

        /// What the most recent press did. Transient by design — it belongs to
        /// the press, while the freshness line above it is what persists.
        @State private var outcome: String?
        @State private var syncing = false

        var body: some View {
            Section("Reflector directory") {
                VStack(alignment: .leading, spacing: 6) {
                    counts
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        VStack(alignment: .leading, spacing: 1) {
                            Text(freshnessLine)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Text(cadenceLine)
                                .font(.caption2)
                                .foregroundStyle(.tertiary)
                        }
                        Spacer(minLength: 8)
                        syncButton
                    }
                    if let outcome {
                        Text(outcome)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                    // A failure is a quiet line here, never a modal: the last
                    // good copy is still loaded and still dialable, so nothing
                    // is blocked and nothing needs acknowledging.
                    if let error = reflectors.lastSyncError {
                        Label(error, systemImage: "exclamationmark.triangle")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .lineLimit(3)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if let attribution = reflectors.attribution {
                        Text(attribution)
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .font(.callout)
                .listRowSeparator(.hidden)
            }
        }

        @ViewBuilder
        private var counts: some View {
            let line = ReflectorDirectoryStatus.countsLine(reflectors.feed.counts)
            if line.isEmpty {
                Text("No reflectors loaded")
                    .foregroundStyle(.secondary)
            } else {
                Text(line)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityLabel("Directory contents")
                    .accessibilityValue(line)
            }
        }

        private var syncButton: some View {
            Button {
                sync()
            } label: {
                if syncing {
                    ProgressView().controlSize(.small).frame(width: 40)
                } else {
                    Text("Sync Now")
                }
            }
            .controlSize(.small)
            .disabled(syncing)
            .accessibilityLabel("Sync Now")
            .accessibilityValue(syncing ? "syncing" : "")
        }

        private var freshnessLine: String {
            ReflectorDirectoryStatus.freshness(
                lastFetched: reflectors.lastFetched,
                isBundled: reflectors.origin == .bundled,
                now: Date())
        }

        /// The published cadence, shown rather than hard-coded into a
        /// sentence: `client_refresh_days` is data, and if hamcall-db changes
        /// it this line changes with it.
        private var cadenceLine: String {
            ReflectorDirectoryStatus.automaticLine(
                nextDue: reflectors.nextAutomaticSync, now: Date())
        }

        private func sync() {
            syncing = true
            outcome = nil
            Task { @MainActor in
                defer { syncing = false }
                do {
                    outcome = try await reflectors.sync(trigger: .manual).message()
                } catch {
                    // The message is already published on the directory, where
                    // it outlives this view; saying it twice in two styles
                    // would read as two problems.
                    outcome = nil
                }
            }
        }
    }
#endif
