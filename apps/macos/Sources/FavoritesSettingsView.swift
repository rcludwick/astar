// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// `@AppStorage` keys + default values for the global talk-timer default
    /// (astar-fda3): the repeater-courtesy limit (minutes) and on/off applied to
    /// nodes without a per-node override. Shared between the call-screen dot
    /// (MenuPopover) and the Settings → Favorites editor so they read/write the
    /// same preference.
    enum TalkTimerDefaults {
        static let enabledKey = "talkTimer.defaultEnabled"
        static let minutesKey = "talkTimer.defaultMinutes"
    }

    /// Settings section: the **Favorites address book**. Lists saved favorites
    /// from every network (network + name + target), each renameable (edit the
    /// name in place), repointable and deletable (trash button + swipe), with
    /// an Add row for AllStar nodes at the bottom — other networks' favorites
    /// are starred from the call screen, where their targets are built.
    /// Mutations go through `CallSession`'s directory helpers and bump the
    /// shared `directoryRevision` so the call-screen dial menu / star re-read
    /// the (non-`@Published`) store.
    struct FavoritesSettingsView: View {
        @EnvironmentObject private var session: CallSession
        /// Shared with the call screen so its directory menu / star re-render after
        /// an add/delete/rename here. Also our own re-read trigger.
        @Binding var directoryRevision: Int

        @State private var newName = ""
        @State private var newNode = ""

        /// Global talk-timer default applied to nodes without a per-node override.
        @AppStorage(TalkTimerDefaults.enabledKey) private var defaultEnabled =
            TalkTimer.defaultEnabled
        @AppStorage(TalkTimerDefaults.minutesKey) private var defaultMinutes = TalkTimer
            .defaultLimitMinutes

        private var favorites: [NodeEntry] {
            _ = directoryRevision  // re-read the (non-@Published) store on edits
            _ = session.directoryChangeCount  // and on stars toggled elsewhere
            return session.directoryFavorites()
        }

        private var canAdd: Bool {
            let node = newNode.trimmingCharacters(in: .whitespaces)
            return !node.isEmpty && node.allSatisfy(\.isNumber)
        }

        var body: some View {
            SettingsSection(.favorites) {
                talkTimerDefaultRow
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 4, leading: 6, bottom: 8, trailing: 6))

                if favorites.isEmpty {
                    Text(
                        "No favorites yet. Add an AllStar node below, or tap the ☆ next to the dial field on the call screen — on any network."
                    )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .listRowSeparator(.hidden)
                } else {
                    ForEach(favorites) { entry in
                        FavoriteRow(entry: entry, directoryRevision: $directoryRevision)
                            .listRowSeparator(.hidden)
                            .listRowInsets(EdgeInsets(top: 4, leading: 6, bottom: 4, trailing: 6))
                    }
                    .onDelete(perform: delete)
                }

                addRow
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 4, leading: 6, bottom: 4, trailing: 6))
            }
        }

        /// The repeater-courtesy talk-timer global default: enable/disable + a
        /// duration (minutes) stepper. Applies to nodes that don't set their own
        /// override below. The on-air dot uses this when a node is unconfigured.
        private var talkTimerDefaultRow: some View {
            VStack(alignment: .leading, spacing: 4) {
                Toggle(isOn: $defaultEnabled) {
                    Text("Talk timer (default)").font(.callout)
                }
                .toggleStyle(.switch)
                .controlSize(.small)
                if defaultEnabled {
                    Stepper(
                        value: $defaultMinutes, in: 1...30
                    ) {
                        Text("\(defaultMinutes) min limit")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    .controlSize(.small)
                }
                Text(
                    "Warns you to un-key before a repeater times out. Per-node settings below override this default."
                )
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            }
        }

        /// Name + node + Add. Validates node is non-empty digits; upsert dedupes by
        /// node, so re-adding an existing number just (re)labels/favorites it.
        private var addRow: some View {
            HStack(spacing: 8) {
                TextField("Name (callsign)", text: $newName)
                    .textFieldStyle(.roundedBorder)
                TextField("AllStar node", text: $newNode)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 96)
                    .onSubmit(add)
                    .accessibilityLabel("AllStar node number")
                Button(action: add) {
                    Label("Add", systemImage: "plus")
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                .disabled(!canAdd)
                .help(
                    "Add this AllStar node to favorites. To save a favorite on another "
                        + "network, star it beside the dial field on the call screen.")
            }
        }

        private func add() {
            guard canAdd else { return }
            // Default the name to the node number when left blank.
            let label = newName.trimmingCharacters(in: .whitespaces)
            let node = newNode.trimmingCharacters(in: .whitespaces)
            if session.directoryAddFavorite(node: node, label: label.isEmpty ? node : label) {
                newName = ""
                newNode = ""
                directoryRevision += 1
            }
        }

        private func delete(at offsets: IndexSet) {
            let entries = favorites
            for index in offsets {
                session.directoryRemove(id: entries[index].id)
            }
            directoryRevision += 1
        }
    }

    /// One favorite: an editable name field (rename in place), the node number, a
    /// trash button, and a per-node **talk-timer override** (Default / custom
    /// duration / Off). Mirrors the mic-profile / saved-config row styling.
    private struct FavoriteRow: View {
        let entry: NodeEntry
        @Binding var directoryRevision: Int
        @EnvironmentObject private var session: CallSession
        @State private var name: String
        /// The editable dial target (astar-6b83). Held in @State like `name` so
        /// the field is live while typing and only written through on commit.
        @State private var node: String
        /// Set when a commit was refused, cleared on the next edit — drives the
        /// red ring and the reason line, matching the credentials form's idiom.
        @State private var nodeRejected = false
        /// The override mode for the talk timer on this node.
        @State private var timerMode: TimerMode
        /// Custom duration (minutes) when `timerMode == .custom`.
        @State private var timerMinutes: Int

        /// How a favorite resolves its talk timer.
        private enum TimerMode: Hashable {
            /// Inherit the global default (`talkTimerEnabled`/`Seconds` both nil).
            case useDefault
            /// A node-specific limit, enabled (`talkTimerEnabled == true`).
            case custom
            /// Disabled for this node — no dot at all (`talkTimerEnabled == false`).
            case off
        }

        init(entry: NodeEntry, directoryRevision: Binding<Int>) {
            self.entry = entry
            _directoryRevision = directoryRevision
            _name = State(initialValue: entry.label)
            _node = State(initialValue: entry.node)
            // Derive the initial mode from the stored override.
            let mode: TimerMode
            if entry.talkTimerEnabled == false {
                mode = .off
            } else if entry.talkTimerEnabled == true || entry.talkTimerSeconds != nil {
                mode = .custom
            } else {
                mode = .useDefault
            }
            _timerMode = State(initialValue: mode)
            _timerMinutes = State(
                initialValue: (entry.talkTimerSeconds ?? TalkTimer.defaultLimitSeconds) / 60)
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    networkTag
                    TextField("Name", text: $name)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit(commitRename)
                    TextField("Node", text: $node)
                        .textFieldStyle(.roundedBorder)
                        .font(.callout.monospacedDigit())
                        .multilineTextAlignment(.trailing)
                        .frame(width: 120)
                        .onSubmit(commitNode)
                        .onChange(of: node) { _ in nodeRejected = false }
                        .overlay(
                            RoundedRectangle(cornerRadius: 5, style: .continuous)
                                .strokeBorder(Color.red, lineWidth: nodeRejected ? 1.5 : 0)
                        )
                        .help(
                            "\(entry.network.displayName) target — edit to repoint this favorite"
                        )
                        .accessibilityLabel("\(entry.network.displayName) target")
                    Button(role: .destructive) {
                        session.directoryRemove(id: entry.id)
                        directoryRevision += 1
                    } label: {
                        Image(systemName: "trash").foregroundStyle(.red)
                    }
                    .buttonStyle(.borderless)
                    .help("Delete this favorite")
                    // astar-a9c3 F4: icon-only.
                    .accessibilityLabel("Delete this favorite")
                }
                if nodeRejected, let reason = nodeRejectionReason {
                    Text(reason)
                        .font(.caption2)
                        .foregroundStyle(.red)
                }
                talkTimerRow
            }
            .padding(8)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        }

        /// Which network the favorite dials on: the same capsule and symbol
        /// the call card's network badge uses, so a mixed list reads at a
        /// glance.
        private var networkTag: some View {
            Label {
                Text(entry.network.badge)
            } icon: {
                Image(systemName: entry.network.symbol)
            }
            .labelStyle(.titleAndIcon)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(Color.secondary.opacity(0.15), in: Capsule())
            .fixedSize()
            .help(entry.network.displayName)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Network")
            .accessibilityValue(entry.network.displayName)
        }

        /// Per-node talk-timer override: a mode picker (Default / Custom / Off)
        /// and, when Custom, a minutes stepper. Writes through to the node entry.
        private var talkTimerRow: some View {
            HStack(spacing: 8) {
                Image(systemName: "timer")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Picker("Talk timer", selection: $timerMode) {
                    Text("Default").tag(TimerMode.useDefault)
                    Text("Custom").tag(TimerMode.custom)
                    Text("Off").tag(TimerMode.off)
                }
                .labelsHidden()
                .pickerStyle(.menu)
                .controlSize(.small)
                .fixedSize()
                .onChange(of: timerMode) { _ in commitTimer() }
                if timerMode == .custom {
                    Stepper(value: $timerMinutes, in: 1...30) {
                        Text("\(timerMinutes) min")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    .controlSize(.small)
                    .fixedSize()
                    .onChange(of: timerMinutes) { _ in commitTimer() }
                }
                Spacer(minLength: 0)
            }
        }

        /// Persist the current talk-timer mode/minutes onto the node entry.
        private func commitTimer() {
            switch timerMode {
            case .useDefault:
                session.directorySetTalkTimer(id: entry.id, enabled: nil, seconds: nil)
            case .custom:
                session.directorySetTalkTimer(
                    id: entry.id, enabled: true, seconds: timerMinutes * 60)
            case .off:
                session.directorySetTalkTimer(id: entry.id, enabled: false, seconds: nil)
            }
            directoryRevision += 1
        }

        /// Why the current text can't be saved, or nil when it can. Reuses the
        /// core's own checks so the message always matches the actual refusal.
        private var nodeRejectionReason: String? {
            let trimmed = node.trimmingCharacters(in: .whitespaces)
            let network = entry.network
            if trimmed.isEmpty {
                return network == .allstar
                    ? "Enter a node number or address." : "Enter a \(network.displayName) target."
            }
            if !CallSession.isSavableTarget(trimmed, network: network) {
                return network == .allstar
                    ? "Not a valid node number or address."
                    : "Not a valid \(network.displayName) target."
            }
            if let other = session.directoryEntry(forNode: trimmed, network: network),
                other.id != entry.id
            {
                return "Already saved as \u{201C}\(other.label)\u{201D}."
            }
            return nil
        }

        /// Write the edited dial target through. On refusal the typed text is
        /// KEPT, not reverted — the red ring plus the reason says what is wrong
        /// so it can be corrected, rather than silently discarding the edit.
        private func commitNode() {
            let trimmed = node.trimmingCharacters(in: .whitespaces)
            guard trimmed != entry.node else {
                node = entry.node
                nodeRejected = false
                return
            }
            if session.directorySetNode(id: entry.id, to: trimmed) {
                node = trimmed
                nodeRejected = false
                directoryRevision += 1
            } else {
                nodeRejected = true
            }
        }

        private func commitRename() {
            let trimmed = name.trimmingCharacters(in: .whitespaces)
            guard !trimmed.isEmpty else {
                name = entry.label  // empty rename reverts
                return
            }
            guard trimmed != entry.label else { return }
            session.directoryRename(id: entry.id, to: trimmed)
            directoryRevision += 1
        }
    }
#endif
