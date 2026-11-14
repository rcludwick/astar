// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarStation
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
    ///
    /// While EchoLink is logged in, the directory's EchoLink stations are
    /// searched alongside the reflectors and the filter offers EchoLink. They
    /// come from the login, not the reflector feed, so they leave the list
    /// (and the filter) the moment the login ends. The search itself is
    /// AstarCore's (`ReflectorDirectory.search(_:filter:echolink:)`).
    struct ReflectorSearchPane: View {
        /// Pre-selects the network filter — whatever the dial field is set to
        /// dial right now. `nil` means the app network has no directory
        /// counterpart (AllStar), and the pane opens showing everything.
        let preferredNetwork: DirectorySearchNetwork?
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
        /// Leave the pane.
        let onBack: () -> Void
        let onSelect: (String, DirectorySearchNetwork) -> Void

        @EnvironmentObject private var reflectors: ReflectorDirectory
        @EnvironmentObject private var allstar: AllStarDirectory
        @EnvironmentObject private var session: CallSession

        @State private var query = ""
        /// The filter the operator chose. Kept apart from the one shown so a
        /// brief EchoLink log-out does not lose it: while EchoLink is away the
        /// pane shows "All networks", and when the login returns it shows
        /// EchoLink again — unless the operator picked something else in the
        /// meantime, which replaces the choice.
        @State private var chosenFilter: DirectorySearchNetwork?
        /// Cached: the window re-renders at the poll rate, and with EchoLink
        /// logged in the two lists together run to thousands of rows, so the
        /// search runs only when one of its inputs changes.
        @State private var results: [DirectorySearchResult] = []
        /// The EchoLink station list made ready to search, rebuilt only when
        /// the list or the login changes, so a keystroke only filters. `nil`
        /// while EchoLink is not logged in.
        @State private var echolinkIndex: EchoLinkSearchIndex?
        /// How many AllStar nodes the current query matches, before the
        /// result limit cuts the list. Set by `refresh`, for the footer.
        @State private var allstarTotalMatches = 0
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
                rebuildEchoLinkIndex()
                chosenFilter = preferredNetwork
                refresh()
                searchFocused = true
            }
            // The EchoLink rows follow the login: a refreshed station list
            // re-runs the search, and a log-out takes EchoLink out of the
            // filter, showing "All networks" until the login returns.
            .onChange(of: session.echolinkStations) { _ in
                rebuildEchoLinkIndex()
                refresh()
            }
            .onChange(of: session.echolinkLogin) { _ in
                rebuildEchoLinkIndex()
                refresh()
            }
            // A node list that finishes loading or syncing while the pane is
            // open adds the AllStar filter and its rows.
            .onChange(of: allstar.index.count) { _ in refresh() }
        }

        // MARK: - Step one: find the reflector

        private var browser: some View {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    backButton("Back to the call", action: onBack)
                    Text(
                        filter == .allstar
                            ? "AllStar nodes"
                            : echolinkStations == nil
                                ? "Reflectors"
                                : "Reflectors and EchoLink"
                    )
                    .font(.headline)
                    Spacer()
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)

                HStack(spacing: 8) {
                    HStack(spacing: 5) {
                        Image(systemName: "magnifyingglass")
                            .foregroundStyle(.secondary)
                            .accessibilityHidden(true)
                        TextField(
                            echolinkStations == nil
                                ? "Name, place or sponsor" : "Name, place, callsign or node",
                            text: $query
                        )
                        .textFieldStyle(.plain)
                        .disableAutocorrection(true)
                        .focused($searchFocused)
                        .accessibilityLabel(
                            echolinkStations == nil
                                ? "Search reflectors" : "Search reflectors and EchoLink stations")
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
                    Picker(
                        "Network",
                        selection: Binding(get: { filter }, set: { chosenFilter = $0 })
                    ) {
                        Text("All networks").tag(DirectorySearchNetwork?.none)
                        ForEach(availableNetworks, id: \.id) { network in
                            Text(network.displayName).tag(DirectorySearchNetwork?.some(network))
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                    .accessibilityLabel("Network filter")
                }
                .padding(.horizontal, 14)
                .padding(.bottom, 8)
                .onChange(of: query) { _ in refresh() }
                .onChange(of: chosenFilter) { _ in refresh() }

                Divider()

                if results.isEmpty {
                    if filter == .echolink && (echolinkStations ?? []).isEmpty {
                        emptyState(
                            "Waiting for the station list", "EchoLink’s directory is on its way.")
                    } else if filter == .allstar {
                        emptyState("No matches", "No AllStar node matches “\(query)”.")
                    } else if reflectors.entries.isEmpty && (echolinkStations ?? []).isEmpty {
                        emptyState(
                            "No directory yet",
                            "astar could not load a reflector list. Try Sync Now in Settings.")
                    } else {
                        emptyState("No matches", "Nothing in the directory matches “\(query)”.")
                    }
                } else {
                    List(results) { result in
                        Group {
                            switch result {
                            case .reflector(let entry): row(for: entry)
                            case .echolink(let station): row(for: station)
                            case .allstar(let node): row(for: node)
                            }
                        }
                        .listRowSeparator(.visible)
                    }
                    .listStyle(.plain)
                    .scrollContentBackground(.hidden)
                }

                Divider()
                footer
            }
        }

        /// The EchoLink stations to search: the directory's list while
        /// logged in, `nil` (none, and no EchoLink filter) otherwise.
        private var echolinkStations: [EchoLinkStationEntry]? {
            session.echolinkLogin == .loggedIn ? session.echolinkStations : nil
        }

        /// The filter in effect: the chosen one while it is on offer, and
        /// "All networks" (`nil`) while it is not — EchoLink while logged out.
        private var filter: DirectorySearchNetwork? {
            DirectorySearchNetwork.keeping(chosenFilter, available: availableNetworks)
        }

        /// The filter's choices: the networks the loaded feed actually
        /// contains, then EchoLink while logged in. See
        /// `DirectorySearchNetwork.available`.
        private var availableNetworks: [DirectorySearchNetwork] {
            DirectorySearchNetwork.available(
                reflectors: reflectors.entries, echolinkLoggedIn: echolinkStations != nil,
                allstarAvailable: allstarIndex != nil)
        }

        /// The AllStar node list to search: `nil` until one is loaded, so the
        /// filter is not offered with nothing behind it.
        private var allstarIndex: AllStarIndex? {
            allstar.index.count > 0 ? allstar.index : nil
        }

        private func refresh() {
            let counted = reflectors.searchCountingAllStar(
                query, filter: filter, echolink: echolinkIndex, allstar: allstarIndex)
            results = counted.results
            allstarTotalMatches = counted.allstarTotalMatches
        }

        private func rebuildEchoLinkIndex() {
            echolinkIndex = echolinkStations.map(EchoLinkSearchIndex.init)
        }

        /// An EchoLink station: callsign, node number, the EchoLink label and
        /// its location and free/busy status. Always choosable — the
        /// directory lists only stations that are on the air.
        private func row(for station: EchoLinkStationEntry) -> some View {
            let subtitle = echolinkSubtitle(for: station)
            return Button {
                onSelect(echolinkDialText(for: station), .echolink)
                onBack()
            } label: {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        // Directory text is written by whoever runs the
                        // station: shown verbatim, never as markup.
                        Text(verbatim: station.call)
                            .font(.callout.weight(.medium))
                            .lineLimit(1)
                        Text(verbatim: String(station.node))
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                        Spacer(minLength: 6)
                        networkBadge(DirectorySearchNetwork.echolink.displayName)
                    }
                    Text(verbatim: subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
                .contentShape(Rectangle())
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(Text(verbatim: station.call))
            .accessibilityValue(
                Text(verbatim: "EchoLink, node \(station.node), \(subtitle)")
            )
            .accessibilityHint("Fills the dial field")
        }

        /// Location text, then whether the station is free or busy.
        private func echolinkSubtitle(for station: EchoLinkStationEntry) -> String {
            let place = station.description.trimmingCharacters(in: .whitespacesAndNewlines)
            return [place, station.status.spokenName].filter { !$0.isEmpty }
                .joined(separator: " · ")
        }

        /// An AllStar node: number, callsign, then description and location.
        private func row(for node: AllStarNode) -> some View {
            let subtitle = [node.description, node.location]
                .compactMap { $0?.trimmingCharacters(in: .whitespacesAndNewlines) }
                .filter { !$0.isEmpty }
                .joined(separator: " · ")
            return Button {
                onSelect(node.node, .allstar)
                onBack()
            } label: {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(verbatim: "\(node.node)  \(node.callsign)")
                            .font(.callout.weight(.medium))
                            .lineLimit(1)
                        Spacer(minLength: 6)
                        networkBadge(DirectorySearchNetwork.allstar.displayName)
                    }
                    if !subtitle.isEmpty {
                        Text(verbatim: subtitle)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                    }
                }
                .contentShape(Rectangle())
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(Text(verbatim: "\(node.node) \(node.callsign)"))
            .accessibilityValue(Text(verbatim: "AllStar, \(subtitle)"))
            .accessibilityHint("Fills the dial field")
        }

        /// The capsule naming a row's network, the same on both kinds of row.
        private func networkBadge(_ name: String) -> some View {
            Text(name)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 5)
                .padding(.vertical, 1)
                .background(.quaternary, in: Capsule())
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
                        networkBadge(entry.network.displayName)
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
                onSelect(entry.id, .reflector(entry.network))
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
                onSelect(
                    ReflectorDialText.applying(module: letter, to: entry.id),
                    .reflector(entry.network))
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
        private var countLine: String {
            let limit = AllStarIndex.resultLimit
            let allstarClause =
                allstarTotalMatches > limit
                ? "Showing \(limit) of \(allstarTotalMatches.formatted()) AllStar nodes — type to narrow"
                : "\(allstarRowCount.formatted()) of \((allstarIndex?.count ?? 0).formatted()) AllStar nodes"
            if filter == .allstar { return allstarClause }
            let others = results.count - allstarRowCount
            let base: String
            if let echolinkStations {
                let total = reflectors.entries.count + echolinkStations.count
                base =
                    "\(others.formatted()) of \(total.formatted()) reflectors and EchoLink stations"
            } else {
                base = "\(others.formatted()) of \(reflectors.entries.count.formatted()) reflectors"
            }
            return allstarRowCount > 0 ? "\(base) · \(allstarClause)" : base
        }

        private var allstarRowCount: Int {
            results.reduce(0) { count, result in
                if case .allstar = result { return count + 1 }
                return count
            }
        }

        private var footer: some View {
            VStack(alignment: .leading, spacing: 2) {
                Text(countLine)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                if allstarRowCount > 0 || filter == .allstar, !allstar.feed.attribution.isEmpty {
                    Text(allstar.feed.attribution)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if allstarRowCount < results.count || filter != .allstar,
                    let attribution = reflectors.attribution
                {
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
