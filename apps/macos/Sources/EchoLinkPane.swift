// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import SwiftUI
    import AstarCore
    import AstarStation

    // The EchoLink parts of the call window.
    //
    // EchoLink is the one network with two things to hold. The **login**
    // lists this station in the EchoLink directory and lasts until Log out;
    // the **call** is to one station. Neither has a form of its own on the
    // dial card any more: EchoLink dials from the same row as every other
    // network, the callsign and password live in Settings → EchoLink, and
    // what is here is
    //
    // * `EchoLinkLoginLine` — one line under the dial row saying where the
    //   login stands, with the one button that moves it on;
    // * `EchoLinkStationsPane` — the station browser behind the dial row's
    //   magnifying glass, which fills the dial field and returns;
    // * `EchoLinkCallPane` — the far station and the chat, under the call
    //   card during an EchoLink call.

    /// Where the "How to register" link goes: the EchoLink page of the
    /// public docs site.
    enum EchoLinkLinks {
        static let registrationHelp = URL(
            string: "https://rcludwick.github.io/astar/macos/echolink/")!
    }

    // MARK: - The login line

    /// Where the EchoLink login stands, in one line, in the style of the
    /// AllStar "Add your AllStarLink account in Settings" caption:
    ///
    /// * logged in — "Logged in as KX7ABC" and Log out;
    /// * logging in, or waiting to try the servers again — a spinner, the
    ///   status and Cancel;
    /// * logged out with a saved password — "Not logged in" and Log in;
    /// * refused — the reason, Log in again, and the way to Settings;
    /// * nothing saved — a pointer to Settings → EchoLink with a button.
    ///
    /// Log in here always uses what Settings saved: the password never passes
    /// through this view.
    struct EchoLinkLoginLine: View {
        @EnvironmentObject private var session: CallSession
        /// Open Settings → EchoLink.
        var onOpenSettings: () -> Void

        private var callsign: String {
            session.echolinkCallsign.trimmingCharacters(in: .whitespacesAndNewlines)
        }

        /// A callsign and a saved password: enough to log in from here.
        private var canLogIn: Bool {
            !callsign.isEmpty && session.echolinkHasSavedPassword(callsign: callsign)
        }

        private var refused: Bool {
            session.echolinkLogin == .failed && !session.echolinkLoginRetrying
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 4) {
                content
                EchoLinkKeychainNote()
            }
            .font(.caption)
            // Keys only, on the worker: "is a password saved?" is read from
            // the session's copy after this, never from the Keychain per draw.
            .onAppear { session.refreshEchoLinkSavedCallsigns() }
        }

        @ViewBuilder
        private var content: some View {
            switch session.echolinkLogin {
            case .loggedIn:
                HStack(spacing: 6) {
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                        .accessibilityHidden(true)
                    Text(verbatim: session.echolinkStatusLine)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Text("·").foregroundStyle(.tertiary).accessibilityHidden(true)
                    Button("Log out", action: session.echolinkLogOut)
                        .buttonStyle(.link)
                        .accessibilityHint("Logs off the EchoLink directory")
                    Spacer(minLength: 0)
                }
            case .loggingIn:
                progressLine
            case .failed where session.echolinkLoginRetrying:
                progressLine
            case .failed, .loggedOut:
                if refused, !session.echolinkStatusLine.isEmpty {
                    Label {
                        Text(verbatim: session.echolinkStatusLine)
                    } icon: {
                        Image(systemName: "exclamationmark.triangle.fill")
                    }
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityLabel("EchoLink login failed")
                    .accessibilityValue(session.echolinkStatusLine)
                }
                if canLogIn {
                    HStack(spacing: 8) {
                        Text(
                            refused
                                ? "Check your password in Settings, or try again."
                                : "Not logged in to EchoLink."
                        )
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        Spacer(minLength: 4)
                        if refused {
                            Button("Settings…", action: onOpenSettings)
                                .controlSize(.small)
                                .accessibilityLabel("Open EchoLink Settings")
                        }
                        Button(refused ? "Log In Again" : "Log In", action: logIn)
                            .controlSize(.small)
                            .accessibilityHint("Logs in to EchoLink as \(callsign)")
                    }
                } else {
                    HStack(spacing: 8) {
                        Text("Add your EchoLink callsign and password in Settings to connect.")
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        Spacer(minLength: 4)
                        Button("Open EchoLink Settings…", action: onOpenSettings)
                            .controlSize(.small)
                    }
                }
            }
        }

        /// A login on its way, or one waiting to try the servers again.
        private var progressLine: some View {
            HStack(spacing: 6) {
                ProgressView()
                    .controlSize(.mini)
                    .accessibilityHidden(true)
                Text(verbatim: session.echolinkStatusLine)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 4)
                Button("Cancel", action: session.echolinkLogOut)
                    .controlSize(.small)
                    .accessibilityHint("Stops logging in to EchoLink")
            }
            .help(
                session.echolinkLoginRetrying
                    ? "astar keeps trying the EchoLink servers by itself." : "")
        }

        private func logIn() {
            guard canLogIn else { return }
            // Empty means the saved password.
            session.echolinkLogIn(
                callsign: callsign, password: "", autoLogin: session.echolinkAutoLogin)
        }
    }

    // MARK: - The station browser

    /// The EchoLink station list as a pane of the window, entered from the
    /// dial row's magnifying glass and left the way Settings and the
    /// reflector directory are. Choosing a station fills the dial field and
    /// comes back; it never connects. Without a login it says so and offers
    /// the same Log in as the dial row.
    struct EchoLinkStationsPane: View {
        @EnvironmentObject private var session: CallSession
        var onBack: () -> Void
        /// Put this text in the dial field.
        var onChoose: (String) -> Void
        var onOpenSettings: () -> Void

        private var mode: EchoLinkPaneMode {
            EchoLinkPaneMode(
                login: session.echolinkLogin, retrying: session.echolinkLoginRetrying,
                link: session.echolinkLink)
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    Button(action: onBack) {
                        Label("Back", systemImage: "chevron.left")
                            .labelStyle(.iconOnly)
                            .frame(width: 22, height: 22)  // full hit area, no clip
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.borderless)
                    .keyboardShortcut("[", modifiers: .command)
                    .accessibilityLabel("Back to the call")
                    Text("EchoLink Stations").font(.headline)
                    Spacer()
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                Divider()
                Group {
                    switch mode {
                    case .stations, .call:
                        EchoLinkStationList(onChoose: { text in
                            onChoose(text)
                            onBack()
                        })
                    case .signIn, .signingIn:
                        VStack(alignment: .leading, spacing: 10) {
                            Label(
                                "Log in to EchoLink to see who is on.",
                                systemImage: "person.badge.key"
                            )
                            .font(.callout)
                            .fixedSize(horizontal: false, vertical: true)
                            EchoLinkLoginLine(onOpenSettings: onOpenSettings)
                            Link(
                                "How to register and validate your callsign",
                                destination: EchoLinkLinks.registrationHelp
                            )
                            .font(.caption)
                            .help("Opens the EchoLink page of the astar guide in your browser")
                            Spacer()
                        }
                    }
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
            }
        }
    }

    /// The Keychain's failure sentence, where the login line and the station
    /// list can both show it: a save fails at the moment the login succeeds.
    private struct EchoLinkKeychainNote: View {
        @EnvironmentObject private var session: CallSession

        var body: some View {
            if let message = session.echolinkKeychainMessage {
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                    .onAppear {
                        AccessibilityAnnouncer.post(message, priority: .medium)
                    }
                    // A second, different failure while the note is up.
                    .onChange(of: message) { changed in
                        AccessibilityAnnouncer.post(changed, priority: .medium)
                    }
            }
        }
    }

    // MARK: - Logged in

    /// The directory's stations: filter, search, Refresh, the list and
    /// Choose. Choosing hands the station to the dial field; it never calls.
    private struct EchoLinkStationList: View {
        @EnvironmentObject private var session: CallSession
        var onChoose: (String) -> Void

        @State private var filter: EchoLinkFilter = .all
        @State private var query = ""
        @State private var selection: UInt32?
        /// The filtered, sorted rows. Cached: the window re-renders at the
        /// poll rate, and the directory lists thousands of stations, so the
        /// filter runs only when one of its inputs changes.
        @State private var rows: [EchoLinkListRow] = []
        /// The EchoLink favorites' targets from the shared directory, read
        /// when it changes rather than on every draw.
        @State private var favorites: [String] = []
        @FocusState private var listFocused: Bool
        @FocusState private var chooseFocused: Bool
        @FocusState private var searchFocused: Bool

        var body: some View {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 6) {
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                        .accessibilityHidden(true)
                    Text(verbatim: session.echolinkStatusLine)
                        .font(.callout)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Spacer(minLength: 8)
                    Button("Log out", action: session.echolinkLogOut)
                        .controlSize(.small)
                        .accessibilityHint("Logs off the EchoLink directory")
                }

                EchoLinkKeychainNote()

                // Five segments do not fit the window's narrowest width, so
                // fold the way the network picker does.
                ViewThatFits(in: .horizontal) {
                    filterPicker(.segmented)
                    filterPicker(.segmented).controlSize(.small)
                    filterPicker(.menu)
                }

                HStack(spacing: 6) {
                    Image(systemName: "magnifyingglass")
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    TextField("Callsign or node number", text: $query)
                        .textFieldStyle(.roundedBorder)
                        .disableAutocorrection(true)
                        .accessibilityLabel("Search stations")
                        .focused($searchFocused)
                        .onSubmit(searchSubmitted)
                    refreshButton
                }

                stationList
                    .frame(minHeight: 200, maxHeight: .infinity)

                HStack {
                    Text(countLine)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button("Choose", action: chooseSelected)
                        .buttonStyle(.borderedProminent)
                        .disabled(selection == nil)
                        .focused($chooseFocused)
                        .help(
                            "Put the selected station in the dial field. "
                                + "Double-clicking a row does the same. Connect from there.")
                }
            }
            .onAppear {
                favorites = session.echolinkFavoriteTargets
                refilter()
                searchFocused = true
            }
            .onChange(of: session.echolinkStations) { _ in refilter() }
            .onChange(of: session.directoryChangeCount) { _ in
                favorites = session.echolinkFavoriteTargets
                refilter()
            }
            .onChange(of: filter) { _ in refilter() }
            .onChange(of: query) { _ in refilter() }
        }

        private func filterPicker<S: PickerStyle>(_ style: S) -> some View {
            Picker("Show", selection: $filter) {
                ForEach(EchoLinkFilter.allCases, id: \.self) { filter in
                    Text(filter.title).tag(filter)
                }
            }
            .pickerStyle(style)
            .labelsHidden()
            .accessibilityLabel("Show stations")
        }

        @ViewBuilder
        private var stationList: some View {
            if session.echolinkStations.isEmpty {
                VStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Waiting for the station list…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 6))
                .accessibilityElement(children: .combine)
            } else if rows.isEmpty {
                Text(
                    filter == .favorites && favorites.isEmpty
                        ? "No favorites yet. Click the star beside a station to add it."
                        : "No stations match."
                )
                .multilineTextAlignment(.center)
                .padding(.horizontal, 12)
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(
                    Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 6))
            } else {
                List(rows, selection: $selection) { row in
                    if let station = row.station {
                        EchoLinkStationRow(
                            station: station, isFavorite: row.isFavorite,
                            onToggleFavorite: { toggleFavorite(row) }
                        )
                        .tag(station.node)
                        .accessibilityAction(named: "Choose") { choose(station.node) }
                    } else {
                        // Not in the directory's list now, so nothing to
                        // select or call: no tag.
                        EchoLinkUnlistedFavoriteRow(
                            row: row, onToggleFavorite: { toggleFavorite(row) })
                    }
                }
                .listStyle(.inset)
                .focused($listFocused)
                .scrollContentBackground(.hidden)
                .background(Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 6))
                // Double-click (and Return) on a row chooses it.
                .contextMenu(forSelectionType: UInt32.self) { nodes in
                    if let node = nodes.first {
                        Button("Choose") { choose(node) }
                        if let row = rows.first(where: { $0.station?.node == node }) {
                            Button(
                                row.isFavorite ? "Remove from Favorites" : "Add to Favorites"
                            ) { toggleFavorite(row) }
                        }
                    }
                } primaryAction: { nodes in
                    if let node = nodes.first { choose(node) }
                }
                .accessibilityLabel("EchoLink stations")
            }
        }

        private var countLine: String {
            let total = session.echolinkStations.count
            guard total > 0 else { return "" }
            if filter == .favorites {
                let favorites = self.favorites.count
                guard favorites > 0 else { return "" }
                let online = rows.filter(\.isListed).count
                let shown = rows.count
                let noun = favorites == 1 ? "favorite" : "favorites"
                let head =
                    shown == favorites
                    ? "\(favorites) \(noun)" : "\(shown) of \(favorites) \(noun)"
                return "\(head), \(online) online"
            }
            if rows.count == total {
                return total == 1 ? "1 station" : "\(total.formatted()) stations"
            }
            return "\(rows.count.formatted()) of \(total.formatted()) stations"
        }

        private func refilter() {
            rows = echolinkListRows(
                stations: session.echolinkStations, favorites: favorites,
                filter: filter, query: query)
            if let selection, !rows.contains(where: { $0.station?.node == selection }) {
                self.selection = nil
            }
        }

        /// Asks the directory for a fresh list. The engine allows one every
        /// 30 s; inside that the control says "Try again in a moment".
        /// "Refreshed" and that sentence each show for a few seconds.
        private var refreshButton: some View {
            Button(action: session.echolinkRefreshStations) {
                switch session.echolinkRefresh {
                case .idle:
                    Label("Refresh", systemImage: "arrow.clockwise")
                        .labelStyle(.iconOnly)
                case .refreshing:
                    ProgressView()
                        .controlSize(.small)
                        .frame(width: 14, height: 14)
                case .refreshed:
                    Label("Refreshed", systemImage: "checkmark")
                        .labelStyle(.titleAndIcon)
                        .foregroundStyle(.secondary)
                case .tooSoon:
                    Text("Try again in a moment")
                        .foregroundStyle(.secondary)
                }
            }
            .controlSize(.small)
            .fixedSize()
            .disabled(session.echolinkRefresh == .refreshing)
            .help("Fetch a fresh station list from the EchoLink directory")
            .accessibilityLabel("Refresh station list")
            .accessibilityValue(refreshSpokenState)
            .onChange(of: session.echolinkRefresh) { state in
                switch state {
                case .refreshed:
                    AccessibilityAnnouncer.post("Station list refreshing", priority: .medium)
                case .tooSoon:
                    AccessibilityAnnouncer.post(
                        "The list was just fetched. Try again in a moment.", priority: .medium)
                case .idle, .refreshing: break
                }
            }
        }

        private var refreshSpokenState: String {
            switch session.echolinkRefresh {
            case .idle: return ""
            case .refreshing: return "refreshing"
            case .refreshed: return "refreshed"
            case .tooSoon: return "try again in a moment"
            }
        }

        /// Return in the search field: one match is the station meant, so
        /// select it and move the keyboard to Choose; with several, hand the
        /// keyboard to the list (first row selected) so the arrow keys pick
        /// one. Nothing here ever dials.
        private func searchSubmitted() {
            let listed = rows.compactMap(\.station)
            if listed.count == 1 {
                selection = listed[0].node
                chooseFocused = true
            } else if let first = listed.first {
                if selection == nil { selection = first.node }
                listFocused = true
            }
        }

        /// The same shared favorite the dial row's star toggles.
        private func toggleFavorite(_ row: EchoLinkListRow) {
            let adding = !row.isFavorite
            session.toggleFavorite(node: row.callsign, network: .echolink, label: row.callsign)
            AccessibilityAnnouncer.post(
                adding
                    ? "\(row.callsign) added to favorites"
                    : "\(row.callsign) removed from favorites",
                priority: .medium)
        }

        private func chooseSelected() {
            if let selection { choose(selection) }
        }

        /// Hand the station to the dial field: its callsign, the thing a
        /// favorite is saved under and the operator recognises, or its node
        /// number for a callsign the dial field could not hold.
        private func choose(_ node: UInt32) {
            selection = node
            guard let station = session.echolinkStations.first(where: { $0.node == node })
            else {
                onChoose(String(node))
                return
            }
            onChoose(echolinkDialText(for: station))
        }
    }

    /// One station: its kind, callsign, description, node number and the
    /// busy/free dot.
    private struct EchoLinkStationRow: View {
        let station: EchoLinkStationEntry
        let isFavorite: Bool
        var onToggleFavorite: () -> Void

        var body: some View {
            HStack(spacing: 8) {
                EchoLinkStatusDot(status: station.status)
                Image(systemName: kindSymbol)
                    .foregroundStyle(.secondary)
                    .frame(width: 16)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    // Directory text is written by whoever runs the station:
                    // shown verbatim, never as markup.
                    Text(verbatim: station.call)
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                    if !station.description.isEmpty {
                        Text(verbatim: station.description)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    }
                }
                Spacer(minLength: 6)
                Text(verbatim: String(station.node))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                EchoLinkFavoriteStar(isFavorite: isFavorite, action: onToggleFavorite)
            }
            .padding(.vertical, 2)
            .contentShape(Rectangle())
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(accessibilityText)
            // The row is one element, so the star is reached as an action.
            .accessibilityAction(
                named: isFavorite ? "Remove from favorites" : "Add to favorites",
                onToggleFavorite)
        }

        private var kindSymbol: String {
            switch station.kind {
            case .user: return "person.fill"
            case .link: return "link"
            case .repeater: return "antenna.radiowaves.left.and.right"
            case .conference: return "person.3.fill"
            case .unknown: return "questionmark.circle"
            }
        }

        private var kindName: String {
            switch station.kind {
            case .user: return "person"
            case .link: return "link"
            case .repeater: return "repeater"
            case .conference: return "conference"
            case .unknown: return "station"
            }
        }

        private var accessibilityText: String {
            var text =
                "\(station.call), \(kindName), \(station.status.spokenName), node \(station.node)"
            if !station.description.isEmpty { text += ", \(station.description)" }
            if isFavorite { text += ", favorite" }
            return text
        }
    }

    /// A favorite the directory does not list right now: greyed and marked
    /// "Not online". It cannot be chosen; its star takes it off the list.
    private struct EchoLinkUnlistedFavoriteRow: View {
        let row: EchoLinkListRow
        var onToggleFavorite: () -> Void

        var body: some View {
            HStack(spacing: 8) {
                Circle()
                    .fill(Color.secondary.opacity(0.35))
                    .frame(width: 9, height: 9)
                    .help("Not online")
                Image(systemName: kindSymbol)
                    .foregroundStyle(.tertiary)
                    .frame(width: 16)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    Text(verbatim: row.callsign)
                        .font(.callout.weight(.medium))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Text("Not online")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                }
                Spacer(minLength: 6)
                EchoLinkFavoriteStar(isFavorite: true, action: onToggleFavorite)
            }
            .padding(.vertical, 2)
            .contentShape(Rectangle())
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(accessibilityText)
            .accessibilityAction(named: "Remove from favorites", onToggleFavorite)
        }

        /// From the callsign's shape, the way the directory itself sorts
        /// stations into kinds: `-L` a link, `-R` a repeater, stars a
        /// conference.
        private var kindSymbol: String {
            let call = row.callsign.uppercased()
            if call.hasPrefix("*") && call.hasSuffix("*") { return "person.3.fill" }
            if call.hasSuffix("-L") { return "link" }
            if call.hasSuffix("-R") { return "antenna.radiowaves.left.and.right" }
            return "person.fill"
        }

        private var accessibilityText: String {
            "\(row.callsign), not online, favorite"
        }
    }

    /// The favorite star: filled yellow when on, a faint outline when off
    /// that darkens under the pointer. The same star as the dial row's.
    private struct EchoLinkFavoriteStar: View {
        let isFavorite: Bool
        var action: () -> Void
        @State private var hovering = false

        var body: some View {
            Button(action: action) {
                Image(systemName: isFavorite ? "star.fill" : "star")
                    .foregroundStyle(
                        isFavorite
                            ? AnyShapeStyle(Color.yellow)
                            : AnyShapeStyle(
                                hovering
                                    ? HierarchicalShapeStyle.secondary
                                    : HierarchicalShapeStyle.tertiary)
                    )
                    .frame(width: 16, height: 16)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .onHover { hovering = $0 }
            .help(isFavorite ? "Remove from favorites" : "Add to favorites")
            .accessibilityLabel(isFavorite ? "Remove from favorites" : "Add to favorites")
            .accessibilityAddTraits(isFavorite ? .isSelected : [])
        }
    }

    /// Green when free, orange when busy, a grey ring when the directory
    /// cannot say. The ring's different shape, the tooltip and the spoken
    /// name mean colour is never the only signal.
    private struct EchoLinkStatusDot: View {
        let status: EchoLinkStationStatus

        var body: some View {
            Group {
                switch status {
                case .online: Circle().fill(Color.green)
                case .busy: Circle().fill(Color.orange)
                case .unknown: Circle().strokeBorder(Color.gray, lineWidth: 1.5)
                }
            }
            .frame(width: 9, height: 9)
            .help(status.spokenName)
            .accessibilityLabel(status.spokenName)
        }
    }

    // MARK: - On a call

    /// The far station, its information text and the chat. The meters, PTT
    /// and transmit time-out sit in the popover's call card above this.
    struct EchoLinkCallPane: View {
        @EnvironmentObject private var session: CallSession
        var onDisconnect: () -> Void
        @State private var draft = ""
        /// The chat with ids that stay with each line (see
        /// `echolinkChatEntries`), rebuilt whenever the engine's chat changes.
        @State private var entries: [EchoLinkChatEntry] = []
        @State private var nextEntryID = 0

        private var peerName: String {
            session.echolinkPeer ?? session.dialedNode ?? "EchoLink"
        }

        private var linkLine: String {
            session.echolinkLink == .connected ? "Connected on EchoLink" : "Calling on EchoLink…"
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Image(systemName: Network.echolink.symbol)
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(verbatim: peerName)
                            .font(.headline)
                            .lineLimit(1)
                            .textSelection(.enabled)
                        Text(linkLine)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    .accessibilityElement(children: .combine)
                    if let callStation = session.echolinkCallStation {
                        // The same shared favorite as the dial row's star.
                        let _ = session.directoryChangeCount
                        EchoLinkFavoriteStar(isFavorite: session.isEchoLinkFavorite(callStation)) {
                            session.toggleFavorite(
                                node: callStation, network: .echolink, label: callStation)
                        }
                    }
                    Spacer(minLength: 8)
                    Button(role: .destructive, action: onDisconnect) {
                        Label("Disconnect", systemImage: "phone.down.fill")
                    }
                    .controlSize(.small)
                    .accessibilityHint("Hangs up and stays logged in")
                }

                if let notice = session.echolinkNotice {
                    // The 10-minute transmit cap stopped the over.
                    Label {
                        Text(verbatim: notice)
                    } icon: {
                        Image(systemName: "exclamationmark.triangle.fill")
                    }
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityElement(children: .combine)
                    .onAppear { AccessibilityAnnouncer.post(notice, priority: .high) }
                }

                if let info = session.echolinkInfo, !info.isEmpty {
                    // The far station's own text: verbatim, never markup.
                    Text(verbatim: info)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(4)
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                        .help(info)
                        .accessibilityLabel("Station information")
                        .accessibilityValue(info)
                }

                Divider()

                chatHistory
                    .frame(height: 110)

                HStack(spacing: 6) {
                    TextField("Message", text: $draft)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit(send)
                        .accessibilityLabel("Chat message")
                    Button(action: send) {
                        Label("Send", systemImage: "paperplane.fill")
                            .labelStyle(.iconOnly)
                    }
                    .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .accessibilityLabel("Send message")
                }
            }
        }

        private var chatHistory: some View {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 4) {
                        if session.echolinkChat.isEmpty {
                            Text("No messages yet.")
                                .font(.caption)
                                .foregroundStyle(.tertiary)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        ForEach(entries) { entry in
                            chatRow(entry.line).id(entry.id)
                        }
                    }
                    .padding(6)
                }
                .background(Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 6))
                .onAppear {
                    updateEntries()
                    if let last = entries.last { proxy.scrollTo(last.id, anchor: .bottom) }
                }
                // Keyed on the whole chat, not its count: at the engine's
                // 256-line cap the count stops changing while lines still
                // arrive.
                .onChange(of: session.echolinkChat) { _ in
                    let lastBefore = entries.last?.id
                    updateEntries()
                    guard let last = entries.last, last.id != lastBefore else { return }
                    withAnimation(.easeOut(duration: 0.15)) {
                        proxy.scrollTo(last.id, anchor: .bottom)
                    }
                }
                .accessibilityLabel("Chat")
            }
        }

        private func chatRow(_ line: EchoLinkChatLine) -> some View {
            // Both strings come from the far station: verbatim, never markup.
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(verbatim: line.from)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(line.mine ? Color.accentColor : Color.primary)
                Text(verbatim: line.text)
                    .font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityElement(children: .combine)
        }

        private func updateEntries() {
            entries = echolinkChatEntries(
                previous: entries, next: session.echolinkChat, nextID: &nextEntryID)
        }

        private func send() {
            let line = draft
            guard !line.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
            draft = ""
            session.echolinkSendChat(line)
        }
    }
#endif
