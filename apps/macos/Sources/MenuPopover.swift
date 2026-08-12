// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import SwiftUI
    import AppKit
    import AstarCore
    import AstarStation

    /// The macOS menu-bar popover — astar's primary surface (au-561f shell + au-e00f
    /// live state + au-f811 connect/dial). Enter a node number to dial via the
    /// WebTransceiver path, watch status / RTT / TX-RX meters, and hang up. State is
    /// driven by the live `CallSession`, polled while the popover is open.
    struct MenuPopover: View {
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var serial: SerialController
        @State private var node = ""
        @State private var errorText: String?
        @State private var showDevices = false
        @State private var keyed = false  // PTT currently held
        @State private var keyMonitor: Any?  // spacebar hold-to-talk event monitor
        @State private var showFavoriteEditor = false  // inline "save favorite" popover
        @State private var favoriteLabel = ""  // editable label for the favorite
        /// Local mirror of `session.m17Callsign` while the M17 callsign prompt
        /// (astar-c2e5 Task 9) is showing — see `m17CallsignField`. Only
        /// committed into the session on Enter or right before a `.m17` dial,
        /// so the prompt doesn't vanish mid-keystroke (it hides once
        /// `session.m17Callsign` is non-empty).
        @State private var callsignDraft = ""
        /// Bumped after favorite edits so the (non-@Published) directory UI re-renders.
        @State private var directoryRevision = 0
        /// The network the next dial goes out on (astar-9b3e). Remembered across
        /// launches; resolves through `Network.resolve` so a stale/unavailable
        /// raw value falls back to `.allstar`. Gated on `session.m17Available`
        /// (astar-c2e5/iax-f2b8 Task 8) — the picker itself stays Task 9's job.
        @AppStorage("ui.network") private var networkRaw = Network.allstar.rawValue
        private var selectedNetwork: Network {
            Network.resolve(networkRaw, m17: session.m17Available)
        }
        /// Whether the "Quick settings" box is expanded (remembered across launches).
        @AppStorage("ui.quickSettingsExpanded") private var quickSettingsExpanded = false
        /// Global talk-timer default for nodes without a per-node override: the
        /// repeater-courtesy limit (minutes) and whether the timer is on. Spec
        /// default: 2 minutes, enabled. Shared with the Settings → Favorites UI.
        @AppStorage(TalkTimerDefaults.enabledKey) private var talkTimerDefaultEnabled =
            TalkTimer.defaultEnabled
        @AppStorage(TalkTimerDefaults.minutesKey) private var talkTimerDefaultMinutes = 2
        /// Whether the "Dialpad" box is expanded (remembered across launches).
        /// Default collapsed so the main page stays compact (astar-b74d).
        @AppStorage("ui.dtmfExpanded") private var dtmfExpanded = false
        /// The DTMF command being composed (connected mode) — editable until
        /// Send plays it as one engine-timed tone sequence (astar-7d21).
        @State private var dtmfCommand = ""
        /// The command currently playing out (field locked, played digits dim
        /// from the session's `dtmfPlayed`); `nil` when nothing is playing.
        @State private var dtmfPlaying: String?
        /// Commands sent this call, oldest first — the subdued per-call
        /// history line. Reset whenever a fresh call is dialed.
        @State private var dtmfHistory: [String] = []
        /// The most recently pressed dialpad key, for the tap-flash animation.
        @State private var flashedKey: String?
        /// Whether the in-call "Levels & Spectrum" disclosure is expanded (remembered
        /// across launches). Default collapsed so the call card stays compact and the
        /// FFT poll stays off until opened (astar-8b5b).
        @AppStorage("ui.spectrumExpanded") private var spectrumExpanded = false
        /// Polls the live TX/RX FFT only while the disclosure is open AND connected.
        @StateObject private var callSpectrum = CallSpectrum()

        private var isInCall: Bool { session.status == .dialing || session.status == .answered }

        // Whether the status row's codec/network badge line (astar-cfc1) has
        // anything to show — mirrors the two badges' own gates below so the
        // line doesn't reserve space (or add its VStack spacing) when empty.
        private var hasStatusBadges: Bool {
            session.negotiatedFormat != nil
                || (isInCall && session.activeCallNetwork == .m17)
                || (isInCall && Network.available(m17: session.m17Available).count > 1
                    && session.activeCallNetwork != nil)
        }

        var body: some View {
            Group {
                if showDevices { devicesPane } else { mainPane }
            }
            // Flexible sizing so the host window is resizable: a usable minimum, a
            // comfortable default, and free to grow. (Settings wants a bit more room,
            // so its ideal is larger — but the user's window size wins.)
            .frame(
                minWidth: 310, idealWidth: showDevices ? 390 : 330,
                maxWidth: .infinity,
                minHeight: 450, idealHeight: showDevices ? 670 : 550,
                maxHeight: .infinity
            )
            // Translucent, blurred backing (the host window is non-opaque/clear).
            .background(VisualEffectView().ignoresSafeArea())
            .onAppear {
                installKeyMonitor()
                applyPollState(inSettings: showDevices)
            }
            // Popover closed → resume the app's baseline poll (AppDelegate keeps the
            // call live for the menu-bar tint / serial PTT). Don't fully stop it.
            .onDisappear {
                removeKeyMonitor()
                session.start()
            }
            // Pause the 20 Hz poll while in Settings — no meters there, and the churn
            // was re-rendering the device pickers every tick (sluggish typing). But
            // keep polling when the serial PTT source is live: the PTT self-test in
            // Settings reads `serial.keyDetected`, which is updated ONLY from the poll
            // loop's `pttSourceTick` — pausing froze the indicator (astar-d00a).
            .onChange(of: showDevices) { showing in
                applyPollState(inSettings: showing)
            }
            // Entering/leaving the live serial state while Settings is open flips
            // whether the self-test needs polling, so re-evaluate.
            .onChange(of: serial.isActive) { _ in
                applyPollState(inSettings: showDevices)
            }
            .onChange(of: session.status) { newStatus in
                if newStatus != .answered && keyed { setKeyed(false) }  // unkey when the call ends
                // Call over → compose state and history die with it; the
                // engine already cancelled any in-flight sequence on teardown.
                if newStatus != .answered {
                    dtmfCommand = ""
                    dtmfPlaying = nil
                    dtmfHistory = []
                }
            }
            // A fresh dial starts a clean dialpad — compose + history are per-call.
            .onChange(of: session.dialedNode) { _ in
                dtmfCommand = ""
                dtmfPlaying = nil
                dtmfHistory = []
            }
            // Sequence finished (engine progress fell back to 0): move the
            // played command into the history line and unlock the field.
            .onChange(of: session.dtmfTotal) { total in
                if total == 0 { finishDTMFSequence(playedOnly: false) }
            }
            // A dial that never answers comes back as a plain hangup with no
            // error to catch (astar-9f48); the session detects the edge and
            // publishes the message — surface it in the same errorText slot as
            // the connect-time failures. nil (cleared on redial/answer) leaves
            // errorText alone: connect() already resets it per attempt.
            .onChange(of: session.lastDialFailure) { failure in
                if let failure { errorText = failure }
            }
        }

        private var mainPane: some View {
            VStack(alignment: .leading, spacing: 0) {
                header
                // Scroll the middle (status + connect/call controls + Quick settings)
                // so it clips/scrolls when the window is short instead of pushing the
                // footer off the bottom edge. Header stays pinned above, Divider +
                // footer stay pinned below (the ScrollView takes the flexible space —
                // no Spacer needed).
                ScrollView {
                    VStack(alignment: .leading, spacing: 8) {
                        statusRow.mainCard()

                        if isInCall {
                            VStack(alignment: .leading, spacing: 0) {
                                meters
                                levelsAndSpectrum
                                callControls
                            }
                            .mainCard()
                        } else {
                            connectControls.mainCard()
                        }

                        if let errorText {
                            Text(errorText)
                                .font(.caption)
                                .foregroundStyle(.red)
                                .padding(.horizontal, 6)
                        }

                        if selectedNetwork.showsDialpad {
                            dialpadSection
                                .transition(.opacity.combined(with: .move(edge: .top)))
                        }

                        quickSettings
                    }
                    .padding(.horizontal, 10)
                    .padding(.top, 8)
                    // Animate the network-switch-driven appear/disappear of the dial
                    // card's conditional content (M17 callsign field, credentials
                    // caption, Dialpad disclosure) instead of letting them pop.
                    // Scoped to `networkRaw` specifically — SwiftUI's value-keyed
                    // `.animation(_:value:)` only engages for changes to THAT value,
                    // unlike the old value-less `.animation(_:)` that would sweep up
                    // every update in the subtree. The 20 Hz meters/status ticks
                    // below (`isInCall` card, `levelsAndSpectrum`) are driven by
                    // `session`/`CallMeters` state, not `networkRaw`, so they stay
                    // un-animated here — no risk of mushy VU meters.
                    .animation(.easeInOut(duration: 0.18), value: networkRaw)
                }

                Divider()
                footer
            }
        }

        private var devicesPane: some View {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    Button {
                        showDevices = false
                    } label: {
                        Label("Back", systemImage: "chevron.left")
                            .labelStyle(.iconOnly)
                            .frame(width: 22, height: 22)  // full hit area, no clip
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.borderless)
                    .keyboardShortcut("[", modifiers: .command)  // ⌘[ to go back
                    Text("Settings").font(.headline)
                    Spacer()
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                Divider()
                // A List (not a ScrollView) so Saved configs get native drag-to-
                // reorder via .onMove. Account is its own section on top.
                List {
                    Section("Account") {
                        CredentialsView()
                            .listRowSeparator(.hidden)
                    }
                    SetupsView()
                    FavoritesSettingsView(directoryRevision: $directoryRevision)
                    MicProfilesView()
                    SpectrumSettingsView()
                }
                .listStyle(.inset)
                .scrollContentBackground(.hidden)  // let the window's blur show through
                .environment(\.defaultMinListRowHeight, 4)
            }
        }

        private var header: some View {
            HStack(spacing: 10) {
                // The bare rainbow asterisk (astar-a056) — NOT the badged app
                // icon: NSApp.applicationIconImage goes through icon services,
                // which can serve a stale cached icon after a rebrand.
                Image("BrandAsterisk")
                    .resizable()
                    .frame(width: 34, height: 34)
                VStack(alignment: .leading, spacing: 1) {
                    Text("astar").font(.headline)
                    Text("AllStarLink client")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 12)
        }

        private var statusRow: some View {
            HStack(spacing: 10) {
                Circle()
                    .fill(statusColor)
                    .frame(width: 9, height: 9)
                    // astar-a9c3 F5: purely decorative — the title text right next
                    // to it already carries the connection state.
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    // Title never wraps/hyphenates (astar-cfc1): lineLimit(1) truncates
                    // rather than breaking mid-word if space ever gets tighter than the
                    // popover's tested minimum, and layoutPriority protects it from
                    // being the thing that shrinks when the row is squeezed — the
                    // Spacer gives first, not the status text.
                    Text(statusTitle)
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                        .layoutPriority(1)
                    // Codec/network badges (astar-eb6c/astar-9b3e/astar-cfc1): broken
                    // onto their own line below the title, not sharing it. At the
                    // popover's minimum width there's no room for "Connected" plus
                    // "G.711 µ" plus "ASL" on one line — cramming them in was forcing
                    // both the title and the badge text to wrap internally. fixedSize
                    // keeps each badge's text on one line inside its capsule no
                    // matter how tight the row gets.
                    if hasStatusBadges {
                        HStack(spacing: 6) {
                            // Codec tag (astar-eb6c, always-on astar-ef35): names the
                            // negotiated codec whenever the call has one — green only
                            // for wideband (slin16), muted for the narrowband baseline.
                            if let format = session.negotiatedFormat {
                                let tint: Color = format.isWideband ? .green : .secondary
                                Text(format.badge)
                                    .font(.caption2.weight(.semibold))
                                    .foregroundStyle(tint)
                                    .padding(.horizontal, 5)
                                    .padding(.vertical, 1)
                                    .background(tint.opacity(0.15), in: Capsule())
                                    .fixedSize()
                                    .help(
                                        "This call negotiated \(format.description) audio · \(format.bitrateLabel)"
                                    )
                                    // astar-a9c3 F5: VO otherwise reads the raw badge
                                    // text ("G.711 µ") with no context — name it and
                                    // read the same description + bitrate the .help
                                    // tooltip gives sighted users.
                                    .accessibilityLabel("Codec")
                                    .accessibilityValue(
                                        "\(format.description), \(format.bitrateLabel)")
                            }
                            // M17 codec tag (astar-bitrate): the engine only supports
                            // Codec 2 voice at 3,200 bit/s today (M17 Task 8), so this
                            // is a fixed label, not a per-call negotiated codec — the
                            // AllStar codec tag above stays empty for M17 (M17 doesn't
                            // negotiate a `VoiceFormat`). When the engine gains other
                            // M17 modes it should start reporting one, and this should
                            // read from the snapshot like the AllStar tag does.
                            if isInCall, session.activeCallNetwork == .m17 {
                                Text("C2 3200")
                                    .font(.caption2.weight(.semibold))
                                    .foregroundStyle(Color.secondary)
                                    .padding(.horizontal, 5)
                                    .padding(.vertical, 1)
                                    .background(Color.secondary.opacity(0.15), in: Capsule())
                                    .fixedSize()
                                    .help("Codec 2 voice at 3,200 bit/s (M17)")
                                    // astar-a9c3 F5: same "Codec" context as the
                                    // AllStar codec badge above.
                                    .accessibilityLabel("Codec")
                                    .accessibilityValue("Codec 2 voice at 3,200 bit/s")
                            }
                            // Network tag (astar-9b3e): latent until a second network
                            // is available; shows the ACTIVE CALL's network, never
                            // the (unselectable-today) picker choice. Gated on
                            // `isInCall` (astar-c7a1) — `activeCallNetwork` only
                            // clears in `disconnect()`, so without this a stale
                            // value could badge a card that's no longer live.
                            if isInCall, Network.available(m17: session.m17Available).count > 1,
                                let network = session.activeCallNetwork
                            {
                                Text(network.badge)
                                    .font(.caption2.weight(.semibold))
                                    .foregroundStyle(Color.secondary)
                                    .padding(.horizontal, 5)
                                    .padding(.vertical, 1)
                                    .background(Color.secondary.opacity(0.15), in: Capsule())
                                    .fixedSize()
                                    .help("Connected via \(network.displayName)")
                                    // astar-a9c3 F5: name the badge so VO reads
                                    // "Network, AllStar" rather than the bare "ASL".
                                    .accessibilityLabel("Network")
                                    .accessibilityValue(network.displayName)
                            }
                        }
                    }
                    if let dialedNode = session.dialedNode, isInCall {
                        // Show the saved name (favorite/directory label) when known,
                        // else the bare node number. Resolver lets astar-6c65 add a
                        // callsign source later. `directoryRevision` re-reads on edits.
                        let _ = directoryRevision
                        HStack(spacing: 6) {
                            Text(connectedNodeLabel(for: dialedNode))
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                            talkTimerDot
                        }
                    }
                }
                // Round-trip time right next to the connection status.
                RTTLabel(meters: session.meters)
                Spacer()
                // Enable TX on the trailing edge: on by default. Off = listen-only
                // (monitor) mode. Green when transmit is enabled, red when disabled.
                Toggle(
                    isOn: Binding(
                        get: { !session.txDisabled },
                        set: { session.setTxDisabled(!$0) }
                    )
                ) {
                    Text(session.txDisabled ? "TX disabled" : "TX enabled")
                        .foregroundStyle(session.txDisabled ? Color.red : Color.green)
                }
                .toggleStyle(.switch)
                // astar-a9c3 F24: .mini was a very small hit target for
                // low-vision/motor users flipping listen-only mode — an
                // operating-state change as important as PTT.
                .controlSize(.small)
                .font(.caption)
                .fixedSize()
                .tint(.green)
                .help(session.txDisabled ? "Transmit disabled — listen only" : "Transmit enabled")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
        }

        private var connectControls: some View {
            VStack(alignment: .leading, spacing: 6) {
                // Network picker (astar-9b3e): latent until a second network is
                // available — hidden entirely today so the dial form is
                // pixel-identical to pre-9b3e.
                if Network.available(m17: session.m17Available).count > 1 {
                    Picker("Network", selection: $networkRaw) {
                        ForEach(Network.available(m17: session.m17Available), id: \.rawValue) {
                            network in
                            Label(network.displayName, systemImage: network.symbol)
                                .tag(network.rawValue)
                        }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .accessibilityLabel("Network")
                }
                // M17 callsign prompt (astar-c2e5 Task 9): M17 sends this
                // verbatim in every frame, so ask once, up front, only when
                // it's still unset. `CallSession.init` already prefilled
                // `m17Callsign` from the AllStarLink portal user when it's
                // shaped like a callsign (Task 8) — so this shows only when
                // that prefill didn't apply and nothing's been saved yet.
                if needsM17Callsign {
                    m17CallsignField
                        // Fade/slide in-out on the network switch rather than
                        // popping; the driving `.animation(value: networkRaw)`
                        // lives on the ancestor VStack in `mainPane`.
                        .transition(.opacity.combined(with: .move(edge: .top)))
                }
                // The always-visible dial form: the single source of truth for the
                // dial string when idle. All three input methods feed this one
                // field — the physical keyboard types into it directly, and the
                // Dialpad turnstile's key taps append into it (astar-b74d). One
                // smart field (astar-427f): digits dial a node through the
                // registrar; a host or host:port dials that address directly.
                HStack(spacing: 8) {
                    TextField(selectedNetwork.dialPlaceholder, text: $node)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit(connect)
                        .disabled(needsCredentials)
                        // astar-a9c3 F20: the placeholder doubles as VO's name for
                        // this field, but it changes per network — give it a
                        // stable label so it doesn't change out from under a
                        // blind user switching networks.
                        .accessibilityLabel("Node number or address")
                        // Admit node chars (digits, * # command dials) plus
                        // hostname/IP chars (letters, dots, colons, hyphens);
                        // drop anything else as it's typed or pasted. Filter is
                        // per-network (astar-9b3e); AllStar's rule is identical
                        // to the pre-9b3e behavior verbatim.
                        .onChange(of: node) { value in
                            let filtered = value.filter { selectedNetwork.admitsDialCharacter($0) }
                            if filtered != value { node = filtered }
                        }
                        .help(
                            "A node number dials through the AllStarLink registrar. "
                                + "An IP or hostname (with optional :port, default 4569) "
                                + "dials that address directly — for a node that isn’t "
                                + "reachable at its published address (e.g. your own "
                                + "node on localhost).")
                    directoryMenu
                    favoriteToggle
                    Button(action: connect) {
                        if session.isConnecting {
                            ProgressView()
                                .controlSize(.small)
                                .frame(width: 14, height: 14)
                        } else {
                            Text("Connect")
                        }
                    }
                    .buttonStyle(.borderedProminent)
                    // astar-a9c3 F10: while connecting, the label swaps to a bare
                    // ProgressView — keep a stable name/state instead of an
                    // unnamed spinner.
                    .accessibilityLabel("Connect")
                    .accessibilityValue(session.isConnecting ? "connecting" : "")
                    .disabled(
                        needsCredentials || !isDialTargetValid || needsM17CallsignToConnect
                            || session.isConnecting)
                }
                // Connecting via AllStar requires an account (guest mode removed,
                // au-1517) — `.m17` doesn't (astar-c2e5 Task 9 fix: this used to
                // gate every network, making M17 unreachable without one). Point
                // the user at Settings to add one; `.m17`'s own missing-callsign
                // case is explained by the progressive-disclosure field's own
                // caption instead, so nothing extra shows here for it.
                if needsCredentials {
                    Text("Add your AllStarLink account in Settings to connect.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        // Same fade/slide as the M17 callsign field above.
                        .transition(.opacity.combined(with: .move(edge: .top)))
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .popover(isPresented: $showFavoriteEditor, arrowEdge: .bottom) { favoriteEditor }
        }

        /// Whether an AllStarLink account is required right now (astar-c2e5
        /// Task 9 fix): ONLY the `.allstar` dial needs one —
        /// `CallSession.connect(node:network:)`'s `.m17` arm (`connectM17`)
        /// never touches `hasCredentials`, so gating M17 on it made M17
        /// unreachable for anyone without an AllStar account. Gates the dial
        /// field, the Connect button, and the "Add your account" caption.
        private var needsCredentials: Bool {
            selectedNetwork == .allstar && !session.hasCredentials
        }

        /// Whether `.m17`'s callsign requirement is unmet right now
        /// (astar-c2e5 Task 9 fix) — mirrors `ConnectError.missingCallsign`
        /// (`CallSession.connectM17`) so the Connect button is disabled for
        /// the same reason it would otherwise throw, rather than dialing and
        /// failing. Checks BOTH the committed `session.m17Callsign` and the
        /// still-uncommitted `callsignDraft` (the progressive-disclosure
        /// field only commits on submit/Connect, see `commitCallsignDraft`) —
        /// either one being non-empty satisfies the requirement.
        private var needsM17CallsignToConnect: Bool {
            selectedNetwork == .m17
                && session.m17Callsign.trimmingCharacters(in: .whitespaces).isEmpty
                && callsignDraft.trimmingCharacters(in: .whitespaces).isEmpty
        }

        /// Whether the M17 callsign prompt belongs in the dial card right now
        /// (astar-c2e5 Task 9): only for the M17 network, and only until a
        /// callsign is set (here or in Settings — either writes
        /// `session.m17Callsign`, so this hides either way).
        private var needsM17Callsign: Bool {
            selectedNetwork == .m17
                && session.m17Callsign.trimmingCharacters(in: .whitespaces).isEmpty
        }

        /// One-line "set your callsign" prompt (astar-c2e5 Task 9), shown only
        /// while `needsM17Callsign`. M17 transmits the callsign verbatim in
        /// every frame, so this is asked once, up front, rather than failing
        /// the dial later. `callsignDraft` is a local mirror so the field
        /// doesn't vanish out from under the user mid-keystroke — it commits
        /// into `session.m17Callsign` (which persists it, see `CallSession`)
        /// on Enter or right before a `.m17` Connect (`commitCallsignDraft`).
        /// `CallSession.init` already prefilled `m17Callsign` from the
        /// AllStarLink portal user when it looks like a callsign (Task 8), so
        /// there's no separate credentials read here — reading the session's
        /// own published value is the cleaner seam from a view.
        private var m17CallsignField: some View {
            VStack(alignment: .leading, spacing: 2) {
                TextField("Your callsign", text: $callsignDraft)
                    .textFieldStyle(.roundedBorder)
                    .onAppear { callsignDraft = session.m17Callsign }
                    .onChange(of: callsignDraft) { value in
                        let upper = value.uppercased()
                        if upper != value { callsignDraft = upper }
                    }
                    .onSubmit(commitCallsignDraft)
                    .accessibilityLabel("Your callsign")
                Text("M17 transmits your callsign — set it once here or in Settings.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }

        /// Commit the local callsign draft into `session.m17Callsign` (whose
        /// `didSet` persists it) — called on Enter in `m17CallsignField` and
        /// again right before a `.m17` Connect, so a dial started without
        /// leaving the field still picks up what was typed.
        private func commitCallsignDraft() {
            let trimmed = callsignDraft.trimmingCharacters(in: .whitespaces).uppercased()
            guard !trimmed.isEmpty else { return }
            session.m17Callsign = trimmed
        }

        /// The repeater-courtesy talk-timer dot (astar-fda3): a small circle next
        /// to the connected-node name that's visible ONLY while transmitting and
        /// ramps green → amber → red as the current continuous transmission
        /// approaches the per-node limit. Hidden when unkeyed or when the timer is
        /// disabled for this node. `directoryRevision` re-reads the per-node
        /// override after a Settings edit. Subtle, light+dark; the color change is
        /// animated so it eases between phases rather than snapping.
        @ViewBuilder private var talkTimerDot: some View {
            let _ = directoryRevision
            if let phase = session.talkTimerPhase(
                defaultEnabled: talkTimerDefaultEnabled,
                defaultLimitSeconds: talkTimerDefaultMinutes * 60)
            {
                Circle()
                    .fill(talkTimerColor(phase))
                    .frame(width: 8, height: 8)
                    .animation(.easeInOut(duration: 0.25), value: phase)
                    .help(talkTimerHelp(phase))
                    .accessibilityLabel("Talk timer")
                    .accessibilityValue(talkTimerHelp(phase))
            }
        }

        private func talkTimerColor(_ phase: TalkTimer.Phase) -> Color {
            switch phase {
            case .green: return .green
            case .amber: return .orange
            case .red: return .red
            }
        }

        private func talkTimerHelp(_ phase: TalkTimer.Phase) -> String {
            // astar-b167: wording now lives in AstarCore's `TalkTimer.help(for:)`
            // so the dot's tooltip/VO value and the accessibility announcer
            // (`AccessibilityAnnouncementPlanner`) can never disagree.
            TalkTimer.help(for: phase)
        }

        /// The connected-node header text: the saved name when known (e.g.
        /// "AJ7HR (77777)"), else "node 77777". Delegates to
        /// `CallSession.connectedTargetLabel(for:)` (astar-b167) so the
        /// accessibility announcer's "Connected to …" text always matches
        /// what the status card shows — no separate formatting to drift.
        private func connectedNodeLabel(for node: String) -> String {
            session.connectedTargetLabel(for: node)
        }

        /// Compact directory picker next to the node field: Favorites then Recents.
        /// Selecting an entry **prefills** the node field (no auto-dial, per design).
        private var directoryMenu: some View {
            _ = directoryRevision  // re-read the store when favorites change
            let favorites = session.directoryFavorites()
            let recents = session.directoryRecents()
            return Menu {
                if favorites.isEmpty && recents.isEmpty {
                    Text("No saved nodes yet")
                }
                if !favorites.isEmpty {
                    Section("Favorites") {
                        ForEach(favorites) { entry in
                            Button {
                                // Auto-switch the picker to the favorite's network
                                // (astar-9b3e) before prefilling — a no-op today
                                // since every entry is `.allstar`.
                                networkRaw = entry.network.rawValue
                                node = entry.node
                            } label: {
                                Label("\(entry.label) — \(entry.node)", systemImage: "star.fill")
                            }
                        }
                    }
                }
                if !recents.isEmpty {
                    Section("Recents") {
                        ForEach(recents) { entry in
                            Button {
                                // Auto-switch the picker to the recent's network
                                // (astar-9b3e), same as favorites, before prefilling.
                                networkRaw = entry.network.rawValue
                                node = entry.node
                            } label: {
                                // Resolve the node to a saved name (favorite/directory
                                // label); fall back to the number for unnamed recents.
                                // The resolver lets astar-6c65 add a callsign source.
                                let name = session.name(forNode: entry.node)
                                Label(
                                    name.map { "\($0) — \(entry.node)" } ?? entry.node,
                                    systemImage: "clock")
                            }
                        }
                    }
                }
            } label: {
                Image(systemName: "list.bullet")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .help("Saved nodes — favorites and recents")
            // astar-a9c3 F4: icon-only; `.help` is a hover tooltip, not the a11y
            // label — mirror it explicitly.
            .accessibilityLabel("Saved nodes — favorites and recents")
        }

        /// Star toggle for the node currently in the field: on → opens an inline
        /// editor to set the label and save a favorite; off (already a favorite) →
        /// un-favorites it.
        private var favoriteToggle: some View {
            Button {
                let n = trimmedNode
                guard !n.isEmpty else { return }
                if session.isFavorite(node: n) {
                    session.removeFavorite(node: n)
                    directoryRevision += 1
                } else {
                    favoriteLabel = n  // default the label to the node number
                    showFavoriteEditor = true
                }
            } label: {
                let _ = directoryRevision  // re-evaluate star fill on edits
                Image(systemName: session.isFavorite(node: trimmedNode) ? "star.fill" : "star")
                    .foregroundStyle(
                        session.isFavorite(node: trimmedNode) ? Color.yellow : Color.secondary)
            }
            .buttonStyle(.plain)
            .disabled(trimmedNode.isEmpty)
            .help(
                session.isFavorite(node: trimmedNode) ? "Remove from favorites" : "Add to favorites"
            )
            // astar-a9c3 F4: icon-only (star/star.fill), state conveyed by fill +
            // color only otherwise — mirror the `.help` string as the label and
            // flag the "already a favorite" state with `.isSelected`.
            .accessibilityLabel(
                session.isFavorite(node: trimmedNode) ? "Remove from favorites" : "Add to favorites"
            )
            .accessibilityAddTraits(session.isFavorite(node: trimmedNode) ? .isSelected : [])
        }

        /// Inline editor popover: name the favorite (callsign or label) and save.
        private var favoriteEditor: some View {
            VStack(alignment: .leading, spacing: 8) {
                Text("Save favorite for node \(trimmedNode)")
                    .font(.callout.weight(.medium))
                TextField("Label (callsign or name)", text: $favoriteLabel)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 220)
                    .onSubmit(saveFavorite)
                HStack {
                    Spacer()
                    Button("Cancel") { showFavoriteEditor = false }
                    Button("Save", action: saveFavorite)
                        .buttonStyle(.borderedProminent)
                        .disabled(trimmedNode.isEmpty)
                }
            }
            .padding(12)
        }

        private func saveFavorite() {
            let n = trimmedNode
            guard !n.isEmpty else { return }
            // Stamp the network the favorite is actually for (astar-c2e5 Task
            // 9): while a call is active, that's the call's own network
            // (`activeCallNetwork`) rather than whatever the picker currently
            // shows — a `selectedNetwork` switch mid-call must not relabel an
            // in-call favorite. Idle, there's no active call, so it falls back
            // to the picker's current choice (the network the typed target is
            // actually for).
            session.addFavorite(
                node: n, label: favoriteLabel, network: session.activeCallNetwork ?? selectedNetwork
            )
            directoryRevision += 1
            showFavoriteEditor = false
        }

        /// Collapsible "Quick settings" box under the dialing section: the simple
        /// config (Setup chooser, devices, volume/mic gain, VOX/processing) tucked
        /// behind a native `DisclosureGroup` so the main page stays compact until you
        /// need it. Expansion is remembered across launches (`ui.quickSettingsExpanded`).
        private var quickSettings: some View {
            DisclosureGroup(isExpanded: $quickSettingsExpanded) {
                QuickConfigView(networkContext: selectedNetwork)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } label: {
                Label("Quick settings", systemImage: "slider.horizontal.3")
                    .font(.callout.weight(.medium))
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
        }

        // MARK: - Dialpad (astar-b74d)

        /// The "Dialpad" disclosure: the 16-key DTMF keypad behind a native
        /// `DisclosureGroup`. Default collapsed; expansion is remembered across
        /// launches (`ui.dtmfExpanded`).
        ///
        /// IDLE (not connected): the keypad is purely an input method that appends
        /// into the node-entry field above — the restored Node-number `TextField`
        /// is the single source of truth (the physical keyboard types into it too).
        /// No separate display / Connect here. CONNECTED: compose-then-send
        /// (astar-7d21) — taps and the keyboard build an editable command, and
        /// Send plays it as one engine-timed tone sequence; nothing goes on the
        /// air while composing.
        private var dialpadSection: some View {
            DisclosureGroup(isExpanded: $dtmfExpanded) {
                VStack(spacing: 10) {
                    // Connected mode gets the compose/Send command bar above the
                    // keypad. Idle mode has no display here — the keypad feeds
                    // the node-entry field above.
                    if isInCall { dialpadCommandBar }
                    if isInCall && !dtmfHistory.isEmpty { dialpadHistoryLine }
                    dialpadKeypad
                    Text(
                        isInCall
                            ? "Compose a command, then Send plays it as tones (e.g. *3\u{200B}<node>)."
                            : "Tap to type into the node number above, or use your keyboard."
                    )
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            } label: {
                Label("Dialpad", systemImage: "circle.grid.3x3.fill")
                    .font(.callout.weight(.medium))
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
        }

        /// Whether a composed command is currently playing out. Latches on the
        /// local echo (`dtmfPlaying`) so the field locks the instant Send is
        /// pressed, before the next snapshot poll reports engine progress.
        private var isSequencePlaying: Bool { dtmfPlaying != nil || session.dtmfTotal > 0 }

        /// Connected-mode command bar (astar-7d21): an editable compose field
        /// with Clear + Send while idle; a locked progress readout (played
        /// digits dimmed) with Stop while the sequence plays.
        private var dialpadCommandBar: some View {
            HStack(spacing: 8) {
                if let playing = dtmfPlaying {
                    let split = DialpadComposer.progressSplit(
                        command: playing, played: session.dtmfPlayed)
                    (Text(split.played).foregroundColor(.secondary) + Text(split.pending))
                        .font(.title3.monospaced())
                        .lineLimit(1)
                        .truncationMode(.head)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 8)
                        .background(
                            Color.secondary.opacity(0.10),
                            in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                        )
                        .accessibilityLabel("Sending command")
                        .accessibilityValue(playing)

                    Button(action: stopDTMFCommand) {
                        Image(systemName: "stop.circle.fill").font(.title3)
                    }
                    .buttonStyle(.borderless)
                    .foregroundStyle(.red)
                    .help("Stop sending — drops the rest of the command")
                    .accessibilityLabel("Stop sending")
                } else {
                    TextField("Command (e.g. *3 node)", text: $dtmfCommand)
                        .textFieldStyle(.roundedBorder)
                        .font(.body.monospaced())
                        .onChange(of: dtmfCommand) { value in
                            // Paste-safe 16-key filter; lowercase a-d uppercase.
                            let filtered = DialpadComposer.filtered(value)
                            if filtered != value { dtmfCommand = filtered }
                        }
                        .onSubmit(sendDTMFCommand)
                        .accessibilityLabel("DTMF command")

                    Button {
                        dtmfCommand = ""
                    } label: {
                        Image(systemName: "xmark.circle").font(.body)
                    }
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
                    .disabled(dtmfCommand.isEmpty)
                    .help("Clear the command")
                    .accessibilityLabel("Clear command")

                    Button(action: sendDTMFCommand) {
                        Image(systemName: "paperplane.fill").font(.body)
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(
                        !DialpadComposer.canSend(
                            command: dtmfCommand,
                            answered: session.status == .answered,
                            playing: isSequencePlaying)
                    )
                    .help("Send the command as touch tones")
                    .accessibilityLabel("Send command")
                }
            }
        }

        /// The subdued per-call "Sent" history line under the command bar.
        private var dialpadHistoryLine: some View {
            Text("Sent: \(dtmfHistory.joined(separator: " · "))")
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.head)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityLabel("Commands sent")
                .accessibilityValue(dtmfHistory.joined(separator: ", "))
        }

        /// The full 16-key DTMF grid: 1-9 * 0 #, with the A/B/C/D column on the
        /// right (4×4). Each key taps through to `dialpadTap`, which appends (idle)
        /// or sends one tone (connected). A-D are DTMF-only — they send tones when
        /// connected but are disabled when idle (not valid node-number characters,
        /// see `dialpadKeyEnabled(_:)`).
        private var dialpadKeypad: some View {
            let rows: [[String]] = [
                ["1", "2", "3", "A"],
                ["4", "5", "6", "B"],
                ["7", "8", "9", "C"],
                ["*", "0", "#", "D"],
            ]
            return VStack(spacing: 8) {
                ForEach(rows, id: \.self) { row in
                    HStack(spacing: 8) {
                        ForEach(row, id: \.self) { key in
                            DialpadKey(
                                label: key,
                                flashed: flashedKey == key,
                                enabled: dialpadKeyEnabled(key),
                                action: { dialpadTap(key) }
                            )
                        }
                    }
                }
            }
        }

        /// The A/B/C/D keys (DTMF-only — not valid node-number characters).
        private static let letterKeys: Set<String> = ["A", "B", "C", "D"]

        private var callControls: some View {
            HStack(spacing: 10) {
                if session.txDisabled {
                    listenOnlyIndicator  // monitor mode: TX is hard-muted
                } else if session.voxEnabled {
                    voxIndicator
                } else {
                    pttButton
                }
                // Match the PTT button's height exactly: same vertical padding and
                // rounded-rect background rather than .bordered (whose default
                // control height is shorter than the hold-to-talk button).
                Button(role: .destructive, action: disconnect) {
                    Image(systemName: "phone.down.fill")
                        .font(.callout.weight(.semibold))
                        .padding(.vertical, 10)
                        .padding(.horizontal, 14)
                        .background(
                            Color.secondary.opacity(0.18),
                            in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                        )
                        .contentShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
                }
                .buttonStyle(.plain)
                .foregroundStyle(.red)
                .help("Disconnect")
                // astar-a9c3 F4: icon-only (phone.down.fill) — the single most
                // important in-call control after PTT.
                .accessibilityLabel("Disconnect")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
        }

        /// Hold-to-talk: press (or hold Spacebar) to key the mic, release to unkey.
        /// Only live once the call is answered.
        private var pttButton: some View {
            Text(keyed ? "ON AIR" : "Hold to Talk  (Space)")
                .font(.callout.weight(.semibold))
                .frame(maxWidth: .infinity)
                .padding(.vertical, 10)
                .background(
                    keyed ? Color.red : Color.secondary.opacity(0.18),
                    in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                )
                .foregroundStyle(keyed ? .white : .primary)
                .contentShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
                .gesture(
                    DragGesture(minimumDistance: 0)
                        .onChanged { _ in setKeyed(true) }
                        .onEnded { _ in setKeyed(false) }
                )
                .disabled(session.status != .answered)
                .opacity(session.status == .answered ? 1 : 0.4)
                .animation(.easeOut(duration: 0.08), value: keyed)
                // astar-a9c3 F1: a DragGesture's onChanged/onEnded is unreachable
                // through the accessibility system, and press-and-hold isn't
                // performable through VoiceOver anyway — so this is exposed as one
                // real button element with a LATCHING toggle action instead (press
                // once to key, again to unkey). Both the default action (plain
                // VO+Space) and the named one perform the same toggle; `.disabled`
                // above already keeps `setKeyed` from firing while not answered, and
                // `setKeyed` itself still guards `session.status == .answered`
                // before keying, so this can't bypass that safety.
                .accessibilityElement()
                .accessibilityAddTraits(.isButton)
                .accessibilityLabel("Push to talk")
                .accessibilityValue(keyed ? "on air" : "not transmitting")
                .accessibilityAction { setKeyed(!keyed) }
                .accessibilityAction(named: "Toggle transmit") { setKeyed(!keyed) }
        }

        /// Replaces the hold-to-talk button while VOX is active: the radio keys from
        /// your voice, so there's nothing to hold — show live keyed state instead.
        private var voxIndicator: some View {
            Text(session.ptt ? "ON AIR (VOX)" : "VOX listening…")
                .font(.callout.weight(.semibold))
                .frame(maxWidth: .infinity)
                .padding(.vertical, 10)
                .background(
                    session.ptt ? Color.red : Color.secondary.opacity(0.18),
                    in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                )
                .foregroundStyle(session.ptt ? .white : .primary)
                .animation(.easeOut(duration: 0.08), value: session.ptt)
                // astar-a9c3 F1: a labeled read-only status element — no action,
                // VOX keys itself from voice.
                .accessibilityElement()
                .accessibilityLabel("VOX")
                .accessibilityValue(session.ptt ? "on air" : "listening")
        }

        /// Replaces the PTT/VOX control while listen-only (Disable TX) is on: makes it
        /// obvious the radio can't transmit.
        private var listenOnlyIndicator: some View {
            Label("TX disabled — listening only", systemImage: "mic.slash")
                .font(.callout.weight(.semibold))
                .frame(maxWidth: .infinity)
                .padding(.vertical, 10)
                .background(
                    Color.secondary.opacity(0.18),
                    in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                )
                .foregroundStyle(.secondary)
                // astar-a9c3 F1: labeled read-only status element, no action.
                .accessibilityElement()
                .accessibilityLabel("TX disabled, listening only")
        }

        private func setKeyed(_ on: Bool) {
            guard keyed != on else { return }  // gesture onChanged fires repeatedly
            guard on == false || session.status == .answered else { return }
            keyed = on
            try? session.setPTT(on)
        }

        /// Drive the 20 Hz poll for the current pane. In Settings we normally pause it
        /// (no meters; avoids device pickers re-rendering every tick). The exception is
        /// a live serial PTT source: the Settings PTT self-test reads
        /// `serial.keyDetected`, which only advances from the poll loop's
        /// `pttSourceTick` — so polling must stay on there or the indicator never flips
        /// (astar-d00a).
        private func applyPollState(inSettings: Bool) {
            if inSettings && !serial.isActive {
                session.stop()
            } else {
                session.start()
            }
        }

        /// Spacebar hold-to-talk while the popover is focused (local monitor; a
        /// global hotkey would need Accessibility permission — a later option).
        ///
        /// astar-e814: while NOT currently keyed, Space must pass through
        /// untouched (not consumed, not keying the transmitter) whenever a
        /// text field has focus — SwiftUI `TextField`s edit through the
        /// window's field editor, an `NSTextView` — so typing a space into
        /// the DTMF/M17-callsign/favorite-label/credentials fields mid-call
        /// never keys the radio. The check is skipped once `keyed` is
        /// already true: if the operator is holding Space and focus then
        /// moves to a field, the eventual keyUp must still reach us and
        /// unkey, rather than being swallowed by the field — leaving the
        /// transmitter stuck on.
        private func installKeyMonitor() {
            guard keyMonitor == nil else { return }
            keyMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) { event in
                guard event.keyCode == 49 else { return event }  // 49 = Space
                if !keyed {
                    let firstResponder =
                        event.window?.firstResponder ?? NSApp.keyWindow?.firstResponder
                    if SpaceKeyGuard.spaceIsTyping(firstResponder: firstResponder) { return event }
                }
                guard session.status == .answered, !session.voxEnabled else { return event }
                if event.type == .keyDown {
                    if !event.isARepeat { setKeyed(true) }
                } else {
                    setKeyed(false)
                }
                return nil  // consume Space (no beep / scroll)
            }
        }

        private func removeKeyMonitor() {
            if let m = keyMonitor {
                NSEvent.removeMonitor(m)
                keyMonitor = nil
            }
            if keyed { setKeyed(false) }  // fail-safe: never leave it keyed
        }

        /// The in-call "Levels & Spectrum" disclosure (astar-8b5b): the TX/RX level
        /// history graph plus the new overlaid TX(red)/RX(green) FFT canvas, behind a
        /// native `DisclosureGroup`. Collapsed by default (`ui.spectrumExpanded`).
        /// Only shown while connected (it lives inside the `isInCall` card). The FFT
        /// poll is gated on "expanded AND connected" — it starts on expand and stops
        /// on collapse / disconnect / disappear, so it's zero-cost when collapsed.
        private var levelsAndSpectrum: some View {
            DisclosureGroup(isExpanded: $spectrumExpanded) {
                VStack(alignment: .leading, spacing: 8) {
                    LevelGraphView(session: session)
                        .frame(height: 60)
                    OverlaidSpectrumCanvas(tx: callSpectrum.tx, rx: callSpectrum.rx)
                        .frame(minHeight: 120)
                        .background(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .fill(.quaternary.opacity(0.5)))
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 6)
            } label: {
                Label("Levels & Spectrum", systemImage: "waveform")
                    .font(.callout.weight(.medium))
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .onAppear { callSpectrum.attach(session: session) }
            .onChange(of: spectrumExpanded) { _ in updateSpectrumPolling() }
            .onChange(of: isInCall) { _ in updateSpectrumPolling() }
            .onAppear { updateSpectrumPolling() }
            .onDisappear { callSpectrum.stop() }
        }

        /// Start the FFT poll only while the disclosure is open AND connected; stop it
        /// otherwise (collapse, disconnect). Mirrors MicCharacterization's gating.
        private func updateSpectrumPolling() {
            if spectrumExpanded && isInCall {
                callSpectrum.attach(session: session)
                callSpectrum.start()
            } else {
                callSpectrum.stop()
            }
        }

        private var meters: some View {
            VUMetersPane(session: session, meters: session.meters)
        }

        /// Marketing version of the *running* bundle, read once from
        /// `CFBundleShortVersionString` (XcodeGen fills that from
        /// `MARKETING_VERSION` in `apps/macos/project.yml` — currently
        /// `0.1.0beta`). Read at runtime, never hard-coded here, so the string
        /// in the footer can't drift from the build the user is actually on.
        /// Falls back to a visibly wrong marker rather than an empty gap if the
        /// key is ever missing.
        static let appVersion: String =
            Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? "unknown"

        private var footer: some View {
            HStack(spacing: 12) {
                Button {
                    showDevices = true
                } label: {
                    Image(systemName: "slider.horizontal.3")
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .help("Audio settings")
                // astar-a9c3 F4: icon-only (slider.horizontal.3).
                .accessibilityLabel("Audio settings")
                // The running build's version, so a user can say what they're on
                // without digging through Get Info — and copy it into a bug report
                // (textSelection). Leading-aligned next to the settings button with
                // Quit trailing: a centred label between two buttons of unequal
                // width only *looks* centred at one popover size, and the window is
                // resizable (minWidth 310) with a text size the user controls, so it
                // would drift off-centre or collide. fixedSize + lineLimit(1) keep it
                // from ever truncating; the Spacer absorbs the slack instead.
                Text(Self.appVersion)
                    .font(.caption2)
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .fixedSize()
                    .textSelection(.enabled)
                    .help("astar version \(Self.appVersion)")
                    .accessibilityLabel("astar version \(Self.appVersion)")
                Spacer(minLength: 8)
                Button {
                    NSApp.terminate(nil)
                } label: {
                    Text("Quit astar")
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .keyboardShortcut("q")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
        }

        // MARK: - Actions

        private var trimmedNode: String { node.trimmingCharacters(in: .whitespaces) }

        /// Whether the dial field currently parses for the SELECTED network
        /// (astar-c2e5 Task 9) — gates the Connect button and, via `connect()`,
        /// the Enter/onSubmit path too. Each network parses its own grammar:
        /// AllStar (and the still-unavailable hamlink) via `DialTarget.parse`
        /// (node number or host[:port]), M17 via `M17Dial.parse`
        /// (`host[:port]/module`).
        private var isDialTargetValid: Bool {
            switch selectedNetwork {
            case .allstar, .hamlink:
                return DialTarget.parse(node) != nil
            case .m17:
                return M17Dial.parse(node) != nil
            }
        }

        private func connect() {
            guard !session.isConnecting else { return }
            // The selected network is read here (main thread), once, rather
            // than inside the background closures below.
            let network = selectedNetwork
            switch network {
            case .allstar, .hamlink:
                // Smart dial routing (astar-427f): digits dial as a node through
                // the registrar; a host/host:port dials that address directly,
                // with the typed address doubling as the display/recents label
                // (safe — the WT dial's calling_number is the user's own node,
                // never this string). Unparseable text is unreachable via the
                // button (disabled) but can arrive via onSubmit — refuse it the
                // same way as empty input.
                guard let target = DialTarget.parse(node) else { return }
                switch target {
                case .node(let value):
                    dispatchConnect(node: value, network: network, address: nil)
                case .address(let value):
                    dispatchConnect(node: value, network: network, address: value)
                }
            case .m17:
                // M17's grammar (`host[:port]/module`) is parsed engine-side
                // (`CallSession.connect(node:network:)` re-parses via
                // `M17Dial.parse`) — this is only the same "unreachable via the
                // disabled button, but refuse it on Enter too" guard as above.
                guard M17Dial.parse(node) != nil else { return }
                // Pick up whatever's in the callsign prompt (if it's still
                // showing) before dialing, so a dial started without leaving
                // that field still uses what was typed (astar-c2e5 Task 9).
                commitCallsignDraft()
                dispatchConnect(node: trimmedNode, network: .m17, address: nil)
            }
        }

        /// Common tail of `connect()`: dial `n` on `network` (or `address`
        /// directly, the AllStar manual-address escape hatch) off the main
        /// thread, then hop back to publish the result. `session.connect`
        /// blocks (the WT path mints a portal token over HTTP, ~400ms) — run it
        /// off the main thread so the popover stays responsive. `isConnecting`
        /// drives the spinner and disables the button against double-taps.
        private func dispatchConnect(node n: String, network: Network, address: String?) {
            errorText = nil
            session.setConnecting(true)
            DispatchQueue.global(qos: .userInitiated).async {
                do {
                    // records dialedNode on the session
                    if let address {
                        try session.connect(node: n, address: address)
                    } else {
                        try session.connect(node: n, network: network)
                    }
                    DispatchQueue.main.async {
                        errorText = nil
                        session.setConnecting(false)
                    }
                } catch {
                    NSLog("[astar] connect FAILED node=%@: %@", n, String(describing: error))
                    // `dialedNode` is NOT cleared here (astar-dialrace): a
                    // stale dial's own failure would otherwise clobber a
                    // NEWER dial's already-published intent the same way the
                    // original wedge did, just at this one field. `session
                    // .connect` now drops `dialedNode` itself, generation-
                    // gated, on a genuinely-current failed dial — see
                    // `CallSession.connectAllStar`/`connectM17`.
                    DispatchQueue.main.async {
                        // Mapped, not localizedDescription: StationError isn't
                        // LocalizedError, so the default text is useless (astar-0217).
                        errorText = connectFailureMessage(for: error, node: n)
                        session.setConnecting(false)
                    }
                }
            }
        }

        private func disconnect() {
            do {
                try session.disconnect()  // clears dialedNode on the session
                errorText = nil
            } catch {
                errorText = error.localizedDescription
            }
        }

        // MARK: - Dialpad actions (astar-b74d)

        /// Whether a given dialpad key is live. When connected, keys need an
        /// answered call and no sequence playing — taps compose, they never
        /// interrupt a playing command (astar-7d21). When idle, only the 12
        /// node-number keys (`1-9 * #`) are live — they feed the node field;
        /// A-D are DTMF-only commands, not valid node characters, so they're
        /// greyed until connected.
        private func dialpadKeyEnabled(_ key: String) -> Bool {
            if isInCall {
                return session.status == .answered && !isSequencePlaying
            }
            // Idle: A-D can't append to a node number, so disable them.
            return session.hasCredentials && !Self.letterKeys.contains(key)
        }

        /// One tap = one action. IDLE: append the key to the node field (A-D never
        /// reach here — they're disabled idle). CONNECTED: append the key to the
        /// composed command — nothing goes on the air until Send (astar-7d21).
        /// Either way, flash the key.
        private func dialpadTap(_ key: String) {
            flashKey(key)
            guard let digit = key.first else { return }
            if isInCall {
                guard session.status == .answered, !isSequencePlaying else { return }
                dtmfCommand.append(digit)
            } else {
                // Defensive: A-D are DTMF-only and disabled while idle, so they
                // must never land in a node number even if a tap slips through.
                guard !Self.letterKeys.contains(key) else { return }
                node.append(digit)
            }
        }

        /// Send the composed command as one engine-timed tone sequence
        /// (astar-7d21). On success the field locks (the command moves to
        /// `dtmfPlaying` and progress dims it); on failure the command stays in
        /// the field — engine validation is all-or-nothing, nothing was sent.
        private func sendDTMFCommand() {
            guard
                DialpadComposer.canSend(
                    command: dtmfCommand,
                    answered: session.status == .answered,
                    playing: isSequencePlaying)
            else { return }
            do {
                try session.sendDTMF(sequence: dtmfCommand)
                dtmfPlaying = dtmfCommand
                dtmfCommand = ""
            } catch {
                NSLog("[astar] sendDTMF sequence FAILED: %@", String(describing: error))
            }
        }

        /// Stop: drop the un-played remainder (the digit currently sounding
        /// finishes). The played prefix still lands in the history line.
        private func stopDTMFCommand() {
            try? session.cancelDTMF()
            finishDTMFSequence(playedOnly: true)
        }

        /// Sequence over — completed (`playedOnly: false`) or stopped
        /// (`playedOnly: true`, only the played prefix counts). Moves the
        /// command into the per-call history and unlocks the compose field.
        private func finishDTMFSequence(playedOnly: Bool) {
            guard let playing = dtmfPlaying else { return }
            let sent =
                playedOnly
                ? DialpadComposer.progressSplit(command: playing, played: session.dtmfPlayed)
                    .played
                : playing
            if !sent.isEmpty { dtmfHistory.append(sent) }
            dtmfPlaying = nil
        }

        /// Visual-only key flash (v1 feedback — no synthesized sidetone). Lights the
        /// key, then clears it after a beat with a gentle ease-out.
        private func flashKey(_ key: String) {
            flashedKey = key
            withAnimation(.easeOut(duration: 0.25)) { flashedKey = nil }
        }

        // MARK: - Presentation

        private var statusTitle: String {
            switch session.status {
            case .idle: return "Not connected"
            case .dialing: return "Connecting…"
            case .answered: return "Connected"
            case .hangup: return "Call ended"
            }
        }

        private var statusColor: Color {
            switch session.status {
            case .idle: return .secondary.opacity(0.5)
            case .dialing: return .orange
            case .answered: return .green
            case .hangup: return .secondary.opacity(0.5)
            }
        }
    }

    /// A compact dBFS level bar (−60…0 dB → empty…full), brightened while keyed.
    /// The status card's RTT readout. A leaf that observes `CallMeters` directly
    /// so RTT ticks re-render only this label, not the whole popover (astar-3e04).
    private struct RTTLabel: View {
        @ObservedObject var meters: CallMeters

        var body: some View {
            if let rtt = meters.rttMS {
                Text("\(rtt) ms")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// The TX/RX VU pair. A leaf that observes `CallMeters` directly so the
    /// ~20 Hz level ticks re-render only these two bars, not every view watching
    /// the session (astar-3e04 — the old whole-popover churn saturated the main
    /// thread and beach-balled after hours).
    private struct VUMetersPane: View {
        @ObservedObject var session: CallSession
        @ObservedObject var meters: CallMeters

        var body: some View {
            VStack(spacing: 7) {
                // TX = what you transmit: the mic level only while keyed, else floor
                // (the mic stays open for metering, so ungated it never returns to 0).
                LevelMeter(
                    label: "TX", db: session.ptt ? meters.txDBHeld : -60, tint: .red,
                    active: session.ptt)
                LevelMeter(
                    label: "RX", db: meters.rxDBHeld, tint: .green, active: session.receiving)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
        }
    }

    private struct LevelMeter: View {
        let label: String
        let db: Float
        let tint: Color
        let active: Bool

        private var fraction: CGFloat {
            CGFloat(max(0, min(1, (db + 60) / 60)))
        }

        /// The bar's fill as a whole-number percent — derived from the SAME
        /// `fraction` that sizes the bar, so the readout always matches the fill.
        private var percent: Int {
            Int((fraction * 100).rounded())
        }

        var body: some View {
            HStack(spacing: 8) {
                Text(label)
                    .font(.caption2.weight(.semibold).monospaced())
                    .foregroundStyle(.secondary)
                    .frame(width: 22, alignment: .leading)
                GeometryReader { geo in
                    ZStack(alignment: .leading) {
                        Capsule().fill(.quaternary)
                        Capsule()
                            .fill(tint.opacity(active ? 1.0 : 0.7))
                            .frame(width: geo.size.width * fraction)
                    }
                }
                .frame(height: 6)
                // Right-side percent readout — fixed width (monospaced digits, room
                // for "100%") so the bar's trailing edge doesn't jiggle as the value
                // changes. Matches the bar fill exactly (same `fraction`).
                Text("\(percent)%")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .frame(width: 34, alignment: .trailing)
            }
            // astar-a9c3 F5: the label, bar, and percent were three separate
            // elements ("TX", then silence over the bar, then "47%") — combine
            // into one so VO reads "TX level, 47 percent" exactly once. The
            // explicit label/value below override the merged children text
            // (rather than stacking on top of it), so nothing doubles up.
            .accessibilityElement(children: .combine)
            .accessibilityLabel("\(label) level")
            .accessibilityValue("\(percent) percent")
        }
    }

    /// A single tactile dialpad key: a rounded, filled cell that brightens for a
    /// beat on tap (the v1 visual feedback). The `*`/`#` keys render a touch
    /// larger so the glyphs read cleanly. VoiceOver announces the key label.
    private struct DialpadKey: View {
        let label: String
        let flashed: Bool
        let enabled: Bool
        let action: () -> Void

        /// The key face. Text fonts draw `*` as a small, raised footnote
        /// glyph, so the asterisk key renders the SF Symbol instead — a
        /// geometrically centered, weight-matched star like a phone pad's
        /// (astar-f3e9). Every other key (including `#`) stays a text glyph.
        @ViewBuilder
        private var face: some View {
            if label == "*" {
                Image(systemName: "asterisk")
                    .font(.title3.weight(.semibold))
            } else {
                Text(label)
                    .font(.title2.weight(.medium).monospacedDigit())
            }
        }

        var body: some View {
            Button(action: action) {
                face
                    .frame(maxWidth: .infinity)
                    .frame(height: 38)
                    .background(
                        flashed ? Color.accentColor.opacity(0.85) : Color.secondary.opacity(0.12),
                        in: RoundedRectangle(cornerRadius: 9, style: .continuous)
                    )
                    .foregroundStyle(flashed ? Color.white : Color.primary)
                    .overlay(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .strokeBorder(Color.secondary.opacity(0.12), lineWidth: 0.5)
                    )
                    .contentShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
            }
            .buttonStyle(.plain)
            .disabled(!enabled)
            .opacity(enabled ? 1 : 0.4)
            .accessibilityLabel("Key \(label)")
        }
    }

    #Preview {
        let previewSession = CallSession(station: NullStation())
        MenuPopover()
            .environmentObject(previewSession)
            .environmentObject(SerialController())
            .environmentObject(SetupController())
            .environmentObject(MicAnalyzerController(session: previewSession))
            .environmentObject(AudioDeviceMonitor(session: previewSession))
    }

    extension View {
        /// Subtle rounded panel matching the saved-config cards, so the main page
        /// reads as a stack of cards over the window's blur.
        fileprivate func mainCard() -> some View {
            background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        }
    }
#endif
