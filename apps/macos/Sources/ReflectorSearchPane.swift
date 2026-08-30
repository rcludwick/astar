// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Browse and search the cached reflector directory (astar-refl-ui).
    ///
    /// A pane of the main window, reached and left the same way Settings is: a
    /// Back chevron in a header, ⌘[ to leave (astar-5a41). It began as a sheet,
    /// on the reasoning that a list of 3,000 reflectors should not grow a 330 pt
    /// popover — but the window is resizable, Settings had already established
    /// what a full-window pane looks like here, and a sheet over a popover is a
    /// second layer of chrome for something that is not modal. Searching the
    /// directory is browsing, not a decision the app is blocked on.
    ///
    /// **Selecting a row fills the dial field and dismisses — it never
    /// connects.** Dialling stays a deliberate second action, the same as
    /// every other way of putting a target in that field.
    ///
    /// Two steps, not one, for the module-bearing networks: pick a reflector,
    /// then pick a room. That is not ceremony. On D-Star the module *is* the
    /// room, the registry publishes none of them, and a picker that quietly
    /// filled in "A" would put an operator into someone else's conversation
    /// keyed up under their own callsign. The letter is asked for, out loud.
    struct ReflectorSearchPane: View {
        /// Pre-selects the network filter — whatever the dial field is set to
        /// dial right now. `nil` means the app network has no directory
        /// counterpart (AllStar), and the pane opens showing everything.
        let preferredNetwork: ReflectorNetwork?
        /// Hands back the finished dial text **and the network it belongs
        /// to**. The pane writes a string, not a target: the dial field stays
        /// the single source of truth for what will be dialled, so there is no
        /// second place holding half of it.
        ///
        /// The network travels with it because the filter can be changed. Pick
        /// a D-Star reflector while the picker sits on M17 and the text is
        /// perfectly good — it just resolves against the wrong network, finds
        /// nothing, and falls through to an address parser that fails. Handing
        /// the network back lets the caller switch to it, the same way the
        /// favorites menu already switches to a favorite's own network.
        /// Leave the pane. Declared before `onSelect` so `onSelect` can stay
        /// the trailing closure at the call site.
        let onBack: () -> Void
        let onSelect: (String, ReflectorNetwork) -> Void

        @EnvironmentObject private var reflectors: ReflectorDirectory

        @State private var query = ""
        @State private var filter: ReflectorNetwork?
        @State private var results: [DirectoryEntry] = []
        /// The reflector whose module is being chosen — the second step. `nil`
        /// while browsing.
        @State private var choosingModuleFor: DirectoryEntry?
        @FocusState private var searchFocused: Bool

        private let memory = ReflectorModuleMemory()

        var body: some View {
            VStack(alignment: .leading, spacing: 0) {
                if let entry = choosingModuleFor {
                    modulePicker(for: entry)
                } else {
                    browser
                }
            }
            .onAppear {
                filter = preferredNetwork
                refresh()
                searchFocused = true
            }
        }

        // MARK: - Step one: find the reflector

        private var browser: some View {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    backButton("Back to the call", action: onBack)
                    Text("Reflectors").font(.headline)
                    Spacer()
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)

                HStack(spacing: 8) {
                    HStack(spacing: 5) {
                        Image(systemName: "magnifyingglass")
                            .foregroundStyle(.secondary)
                            .accessibilityHidden(true)
                        TextField("Name, place or sponsor", text: $query)
                            .textFieldStyle(.plain)
                            .focused($searchFocused)
                            .accessibilityLabel("Search reflectors")
                        if !query.isEmpty {
                            Button {
                                query = ""
                            } label: {
                                Image(systemName: "xmark.circle.fill")
                                    .foregroundStyle(.secondary)
                            }
                            .buttonStyle(.borderless)
                            .accessibilityLabel("Clear search")
                        }
                    }
                    .padding(.horizontal, 7)
                    .padding(.vertical, 5)
                    .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))

                    // A menu, not segments: six networks plus "All" will not
                    // fit across a 330 pt popover, and the filter is a
                    // narrowing tool rather than a mode.
                    Picker("Network", selection: $filter) {
                        Text("All networks").tag(ReflectorNetwork?.none)
                        ForEach(availableNetworks, id: \.rawValue) { network in
                            Text(network.displayName).tag(ReflectorNetwork?.some(network))
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                    .accessibilityLabel("Network filter")
                }
                .padding(.horizontal, 14)
                .padding(.bottom, 8)
                .onChange(of: query) { _ in refresh() }
                .onChange(of: filter) { _ in refresh() }

                Divider()

                if reflectors.entries.isEmpty {
                    emptyState(
                        "No directory yet",
                        "astar could not load a reflector list. Try Sync Now in Settings.")
                } else if results.isEmpty {
                    emptyState("No matches", "Nothing in the directory matches “\(query)”.")
                } else {
                    List(results, id: \.key) { entry in
                        row(for: entry)
                            .listRowSeparator(.visible)
                    }
                    .listStyle(.plain)
                    .scrollContentBackground(.hidden)
                }

                Divider()
                footer
            }
        }

        /// Networks the loaded feed actually contains, in the order
        /// `ReflectorNetwork.known` lists them, with anything this build has
        /// never heard of after them. Built from the data rather than from the
        /// enum so the filter cannot offer a network with nothing behind it —
        /// and cannot hide one hamcall-db added after this build shipped.
        private var availableNetworks: [ReflectorNetwork] {
            let present = Set(reflectors.entries.map(\.network))
            let known = ReflectorNetwork.known.filter(present.contains)
            let unknown = present.subtracting(known).sorted { $0.rawValue < $1.rawValue }
            return known + unknown
        }

        private func refresh() {
            results = reflectors.search(query, network: filter)
        }

        @ViewBuilder
        private func row(for entry: DirectoryEntry) -> some View {
            let dialable = entry.isDialable
            Button {
                select(entry)
            } label: {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(entry.name)
                            .font(.callout.weight(.medium))
                            .lineLimit(1)
                        // The id only when it is not already the name — for
                        // D-Star they are the same string and printing it
                        // twice is noise.
                        if entry.name.caseInsensitiveCompare(entry.id) != .orderedSame {
                            Text(entry.id)
                                .font(.caption.monospaced())
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                        }
                        Spacer(minLength: 6)
                        Text(entry.network.displayName)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 5)
                            .padding(.vertical, 1)
                            .background(.quaternary, in: Capsule())
                    }
                    if let subtitle = subtitle(for: entry) {
                        Text(subtitle)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                    }
                    // Listed but not dialable is a published state astar is
                    // required to honour: show the entry, say why Connect is
                    // off, refuse to dial it. Hiding these would make the app
                    // look wrong rather than honest.
                    if !dialable {
                        Text(unavailableReason(for: entry))
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
                .contentShape(Rectangle())
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .disabled(!dialable)
            .opacity(dialable ? 1 : 0.55)
            .accessibilityLabel(entry.name)
            .accessibilityValue(
                [entry.network.displayName, subtitle(for: entry)]
                    .compactMap { $0 }.joined(separator: ", ")
            )
            .accessibilityHint(dialable ? "Fills the dial field" : unavailableReason(for: entry))
        }

        private func subtitle(for entry: DirectoryEntry) -> String? {
            let parts = [entry.country, entry.plainDescription ?? entry.plainSponsor]
            let joined = parts.compactMap { $0 }.joined(separator: " · ")
            return joined.isEmpty ? nil : joined
        }

        private func unavailableReason(for entry: DirectoryEntry) -> String {
            guard let dial = entry.dial else { return "Listed with no way to connect" }
            return "astar can’t dial \(dial.kind) yet"
        }

        /// Fill the field — or ask for the room first.
        private func select(_ entry: DirectoryEntry) {
            guard entry.isDialable else { return }
            if ReflectorModuleOptions.options(for: entry.dial).isEmpty {
                onSelect(entry.id, entry.network)
                onBack()
            } else {
                choosingModuleFor = entry
            }
        }

        // MARK: - Step two: pick the room

        @ViewBuilder
        private func modulePicker(for entry: DirectoryEntry) -> some View {
            let options = ReflectorModuleOptions.options(for: entry.dial)
            let remembered = memory.module(for: entry)
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    backButton("Back to the reflector list") { choosingModuleFor = nil }
                    VStack(alignment: .leading, spacing: 1) {
                        Text(entry.name).font(.headline)
                        Text("Choose a module").font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                Divider()

                ScrollView {
                    LazyVGrid(
                        columns: Array(repeating: GridItem(.flexible(), spacing: 6), count: 6),
                        spacing: 6
                    ) {
                        ForEach(options, id: \.self) { letter in
                            moduleButton(letter, for: entry, remembered: remembered)
                        }
                    }
                    .padding(14)
                }

                Divider()
                VStack(alignment: .leading, spacing: 4) {
                    if let remembered {
                        Text("You last used module \(String(remembered)) here.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        // Said plainly, because the honest answer is that
                        // astar does not know. The registry publishes no
                        // module list, so anything else here would be a guess
                        // wearing a caption.
                        Text(
                            "astar can’t tell which modules are active — ask the reflector’s "
                                + "sponsor or its dashboard."
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    }
                    if let dashboard = entry.dashboard, let url = URL(string: dashboard) {
                        Link("Open the reflector dashboard", destination: url)
                            .font(.caption)
                    }
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
            }
        }

        private func moduleButton(
            _ letter: Character, for entry: DirectoryEntry, remembered: Character?
        ) -> some View {
            let isRemembered = letter == remembered
            return Button {
                // A recollection, written down only because the operator
                // actually chose it. Nothing reads this at dial time.
                memory.remember(letter, for: entry)
                onSelect(ReflectorDialText.applying(module: letter, to: entry.id), entry.network)
                onBack()
            } label: {
                Text(String(letter))
                    .font(.callout.monospaced().weight(isRemembered ? .bold : .regular))
                    .frame(maxWidth: .infinity, minHeight: 28)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.bordered)
            .tint(isRemembered ? .accentColor : nil)
            .accessibilityLabel("Module \(String(letter))")
            .accessibilityHint(isRemembered ? "Last used here" : "")
        }

        // MARK: - Chrome

        /// The same chevron Settings uses, with the same ⌘[ — both levels of
        /// this pane go back, and "back" should mean one keystroke everywhere
        /// in this window rather than one per surface.
        private func backButton(
            _ label: String, action: @escaping () -> Void
        ) -> some View {
            Button(action: action) {
                Label("Back", systemImage: "chevron.left")
                    .labelStyle(.iconOnly)
                    .frame(width: 22, height: 22)  // full hit area, no clip
                    .contentShape(Rectangle())
            }
            .buttonStyle(.borderless)
            .keyboardShortcut("[", modifiers: .command)
            .accessibilityLabel(label)
        }

        private func emptyState(_ title: String, _ detail: String) -> some View {
            VStack(alignment: .center, spacing: 6) {
                Spacer()
                Text(title).font(.callout.weight(.medium))
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                Spacer()
            }
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 24)
        }

        /// The count, and the credit. CC BY requires the attribution wherever
        /// the data appears, and this pane is the place it most obviously
        /// appears — a credit that lives only in Settings is one refactor from
        /// being the only copy, and then from being gone.
        private var footer: some View {
            VStack(alignment: .leading, spacing: 2) {
                Text("\(results.count) of \(reflectors.entries.count) reflectors")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                if let attribution = reflectors.attribution {
                    // Not line-limited: a truncated credit is a broken one,
                    // and this is the surface where the data is most visibly
                    // being used.
                    Text(attribution)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
        }
    }
#endif
