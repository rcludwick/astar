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
        /// The cached reflector directory (astar-refl-ui), for the search
        /// pane and the module picker. The *dial* path does not go through
        /// this — it holds the directory's frozen `index` value instead, so
        /// resolution can run off the main thread (see `ReflectorIndex`).
        @EnvironmentObject private var reflectors: ReflectorDirectory
        /// Which pane to show. Shared (not `@State`) so the main menu's `Settings…`
        /// item can open this same pane — see `AppNavigation`.
        @EnvironmentObject private var navigation: AppNavigation
        @State private var node = ""
        @State private var errorText: String?
        @State private var keyed = false  // PTT currently held
        @State private var keyMonitor: Any?  // spacebar hold-to-talk event monitor
        @State private var showFavoriteEditor = false  // inline "save favorite" popover
        /// Last module used per reflector, offered pre-selected in the picker.
        /// A recollection, never a default — nothing reads it at dial time.
        private let moduleMemory = ReflectorModuleMemory()
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
        /// Live device lists, for the duplicate-name warning below the dial
        /// card (astar-9d41). Already in the environment for QuickConfigView.
        @EnvironmentObject private var deviceMonitor: AudioDeviceMonitor
        /// Owns the mic analyzer's model, so the pane keeps its picker + profile
        /// name across a trip back to the call card.
        @EnvironmentObject private var micAnalyzer: MicAnalyzerController
        /// The devices this rig is actually using, so the warning below can be
        /// limited to a clash that affects them (astar-9d41). Same keys
        /// `AudioSettings` persists, read-only here.
        @AppStorage("audio.input") private var selectedInputDevice: String?
        @AppStorage("audio.output") private var selectedOutputDevice: String?
        @AppStorage("ui.network") private var networkRaw = Network.allstar.rawValue
        /// The networks the engine can drive right now. `dstar` is a fact
        /// about the desk rather than the build — the segment appears when a
        /// ThumbDV is attached, because D-Star voice is AMBE and astar has no
        /// software vocoder to offer without one.
        private var availableNetworks: [Network] {
            Network.available(
                m17: session.m17Available, dstar: session.dstarAvailable,
                ysf: session.ysfAvailable, nxdn: session.nxdnAvailable,
                dmr: session.dmrAvailable)
        }

        private var selectedNetwork: Network {
            Network.resolve(
                networkRaw, m17: session.m17Available, dstar: session.dstarAvailable,
                ysf: session.ysfAvailable, nxdn: session.nxdnAvailable,
                dmr: session.dmrAvailable)
        }

        /// The network picker's binding, and the one place a network change
        /// clears the dial field.
        ///
        /// The three networks do not share an address space: `45192` is an
        /// AllStarLink node number and means nothing to M17 or D-Star, and a
        /// reflector name means nothing to AllStar. Text left behind by a
        /// hand-made switch is never a target on the network now selected —
        /// it just sits there looking dialable, and `admitsDialCharacter`
        /// won't remove it either, because that filter only runs on what is
        /// typed, not on what a switch stranded.
        ///
        /// Scoped to the picker deliberately. Favorites, recents and the
        /// reflector search pane also switch networks, but each sets the
        /// matching dial text in the same breath — they assign `networkRaw`
        /// directly, so they never come through here and never lose the
        /// target they just filled in.
        private var networkSelection: Binding<String> {
            Binding(
                get: { networkRaw },
                set: { raw in
                    guard raw != networkRaw else { return }
                    networkRaw = raw
                    node = ""
                }
            )
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
        /// The DMR picker's groups, cached.
        ///
        /// Grouping walks all 3,415 directory rows and sorts what it keeps,
        /// and this pane re-renders at the 20 Hz poll rate while the meters
        /// are live — so it is recomputed when one of its two inputs changes
        /// (the loaded feed itself, the consent flag) and not on every tick.
        /// Keyed on the feed rather than its row COUNT: a sync that replaces
        /// the directory with a same-sized one is exactly the case a count
        /// would miss.
        @State private var dmrGroupsCache: [DmrSystemCatalog.Group] = []
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
                || (isInCall && availableNetworks.count > 1
                    && session.activeCallNetwork != nil)
        }

        var body: some View {
            Group {
                switch navigation.pane {
                case .call: mainPane
                case .settings: devicesPane
                case .reflectors: reflectorsPane
                case .micAnalyzer: micAnalyzerPane
                }
            }
            // Flexible sizing so the host window is resizable: a usable minimum, a
            // comfortable default, and free to grow. (Settings and the reflector
            // list both want more room than the dial card, so their ideals are
            // larger — but the user's window size wins.)
            .frame(
                minWidth: 310, idealWidth: idealPaneWidth, maxWidth: .infinity,
                minHeight: 450, idealHeight: idealPaneHeight, maxHeight: .infinity
            )
            // Translucent, blurred backing (the host window is non-opaque/clear).
            .background(VisualEffectView().ignoresSafeArea())
            .onAppear {
                installKeyMonitor()
                applyPollState(for: navigation.pane)
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
            .onChange(of: navigation.pane) { pane in
                applyPollState(for: pane)
            }
            // Entering/leaving the live serial state while Settings is open flips
            // whether the self-test needs polling, so re-evaluate.
            .onChange(of: serial.isActive) { _ in
                applyPollState(for: navigation.pane)
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
                                // Orange, not red: this is the app's warning
                                // colour everywhere else — the credentials
                                // prompt, the config-transfer notice, the
                                // serial warnings, the mic analyzer's own
                                // error line. A dial that failed is something
                                // to fix, not a fault in the app.
                                .foregroundStyle(.orange)
                                .padding(.horizontal, 6)
                        }

                        // astar-9d41 — two devices reporting one name. Sits
                        // directly under the dial card rather than down in
                        // Quick settings: it explains why a device you just
                        // plugged in is not in the list, which is a question
                        // you ask before you go looking for the picker.
                        //
                        // Gated on the SELECTED devices. A clash among hardware
                        // this rig is not using does not affect the call you are
                        // about to make, and a banner that is always on is one
                        // people learn to stop reading. Settings keeps the
                        // unconditional warning, where choosing devices is the
                        // job at hand.
                        if let clash = AudioDeviceList.collisionWarning(
                            inputs: deviceMonitor.inputs,
                            outputs: deviceMonitor.outputs,
                            selectedInput: selectedInputDevice,
                            selectedOutput: selectedOutputDevice)
                        {
                            Label(clash, systemImage: "exclamationmark.triangle.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .fixedSize(horizontal: false, vertical: true)
                                .padding(.horizontal, 6)
                                .accessibilityLabel("Duplicate device names")
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
                        navigation.goBack()
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
                    // Who you are comes before what you own: the callsign and
                    // radio ID identify the operator, everything below is
                    // equipment (astar-c9d2).
                    StationIdentityView()
                    Section("Account") {
                        CredentialsView()
                            .listRowSeparator(.hidden)
                    }
                    // Below the account, not beside the callsign: a DMR ID is
                    // one network's credential, and that network is not
                    // dialable yet (astar-a7c5).
                    DmrSettingsView()
                    // Its own section, not a second field in DMR's: NXDN ids
                    // are 16-bit and a registered DMR ID does not fit in one,
                    // so they are two numbers, not one shown twice.
                    NxdnSettingsView()
                    SetupsView()
                    FavoritesSettingsView(directoryRevision: $directoryRevision)
                    MicProfilesView()
                    ReflectorSettingsView()
                    SpectrumSettingsView()
                    ConfigTransferView(directoryRevision: $directoryRevision)
                }
                .listStyle(.inset)
                .scrollContentBackground(.hidden)  // let the window's blur show through
                .environment(\.defaultMinListRowHeight, 4)
            }
        }

        /// The reflector directory as a pane of this window (astar-5a41),
        /// entered from the dial card's magnifying glass and left the same way
        /// Settings is.
        private var reflectorsPane: some View {
            ReflectorSearchPane(
                preferredNetwork: selectedNetwork.reflectorNetwork,
                onBack: { navigation.goBack() },
                onSelect: { text, network in
                    // Switch the picker to the chosen reflector's own network
                    // before filling the field — the pane can browse past the
                    // network the dial is set to, and text resolved against the
                    // wrong one silently finds nothing. Same move the favorites
                    // menu already makes. Unavailable networks cannot appear here:
                    // `Network.resolve` refuses them, so this cannot select a
                    // segment that is not offered.
                    if let appNetwork = Network.matching(network),
                        availableNetworks.contains(appNetwork)
                    {
                        networkRaw = appNetwork.rawValue
                    }
                    // The pane hands back dial text, not a target: the dial field
                    // stays the single source of truth for what Connect will dial.
                    // Selecting never connects — that is still a deliberate second
                    // action.
                    node = text
                }
            )
        }

        /// The mic analyzer, with the same header chrome as Settings — a Back
        /// chevron (⌘[) and a title — so every pane is entered and left the same
        /// way. The analyzer's own controls carry no close button for that reason.
        private var micAnalyzerPane: some View {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    Button {
                        navigation.goBack()
                    } label: {
                        Label("Back", systemImage: "chevron.left")
                            .labelStyle(.iconOnly)
                            .frame(width: 22, height: 22)  // full hit area, no clip
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.borderless)
                    .keyboardShortcut("[", modifiers: .command)  // ⌘[ to go back
                    Text("Mic analyzer").font(.headline)
                    Spacer()
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                Divider()
                // The view starts the monitor in `onAppear` and releases it in
                // `onDisappear`; the switch above rebuilds it per visit, so the mic
                // is open only while the pane is on screen.
                MicAnalyzerView(vm: micAnalyzer.vm)
                    .environmentObject(session)
            }
        }

        /// Default window width per pane. The user's own window size wins over
        /// all of these; they only set what a fresh window opens at.
        private var idealPaneWidth: CGFloat {
            switch navigation.pane {
            case .call: return 330
            case .settings: return 390
            case .reflectors: return 390
            case .micAnalyzer: return 480
            }
        }

        private var idealPaneHeight: CGFloat {
            switch navigation.pane {
            case .call: return 550
            case .settings: return 670
            case .reflectors: return 620
            case .micAnalyzer: return 560
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
                    // Title never wraps and never elides (astar-cfc1, astar-5e2c).
                    // lineLimit(1) stops it breaking mid-word; fixedSize makes it
                    // render at its ideal width instead of accepting a narrower
                    // proposal, which is what put an ellipsis on "Connected" while
                    // the row still had room. layoutPriority alone did not cover it:
                    // it orders who gives way, but Text stays willing to compress, so
                    // a tight proposal still truncated the one string in this row that
                    // is a fixed, known word rather than user data. Same treatment the
                    // badges below and the TX toggle at the trailing edge already use.
                    Text(statusTitle)
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
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
                            if isInCall, availableNetworks.count > 1,
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
                            // One line, truncating (astar-5e2c). Without a
                            // lineLimit this wrapped: a repeater name plus node
                            // number split across two lines, which grew the card
                            // vertically and pushed the level graphs down. It is
                            // also the row's pressure valve — an unbounded
                            // wrapping Text refuses to compress below its longest
                            // word, so the width had nowhere to go but the window.
                            // This is the one string here that is user data and
                            // can be arbitrarily long, which makes it the right
                            // thing to elide, unlike the fixed status title.
                            Text(connectedNodeLabel(for: dialedNode))
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                                .truncationMode(.tail)
                            talkTimerDot
                        }
                    }
                    // Who last keyed up, on whichever digital network is
                    // live — and, on D-Star, anything they sent as slow data.
                    // It is most of the point of listening in: a reflector
                    // with no talker line is an anonymous voice. AllStar is
                    // excluded because IAX2 carries no talker identity at all
                    // (`Network.isDigitalVoice`); a new network opts in there
                    // and gets this line without touching the popover.
                    if isInCall, session.activeCallNetwork?.isDigitalVoice == true {
                        talkerLine
                    }
                }
                // astar-5e2c: the text column takes its ideal width BEFORE the
                // Spacer gets any. Without this the column, the RTT readout and
                // the Spacer all sat at priority 0, so an HStack split the spare
                // width between them — the Spacer claimed a share it did not need
                // and the status text was squeezed into eliding "Connected" and
                // wrapping the node label, while the row visibly still had room.
                // Priority orders who is satisfied first; the Spacer now collapses
                // to whatever is genuinely left over.
                .layoutPriority(1)
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
                if availableNetworks.count > 1 {
                    Picker("Network", selection: networkSelection) {
                        ForEach(availableNetworks, id: \.rawValue) {
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
                    reflectorSearchButton
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
                            || needsDMRRadioIDToConnect || session.isConnecting)
                }
                // DMR's target is four things and the field can only hold two
                // of them comfortably, so the network and the slot get
                // controls of their own — see `dmrTargetRow`.
                if selectedNetwork == .dmr {
                    dmrTargetRow
                        .transition(.opacity.combined(with: .move(edge: .top)))
                        .onAppear(perform: refreshDMRGroups)
                        .onChange(of: session.brandmeisterConsent) { _ in refreshDMRGroups() }
                        .onChange(of: reflectors.feed) { _ in refreshDMRGroups() }
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
                // DMR's other credential. Said here, with Connect already off,
                // rather than thrown on press: the operator cannot supply a
                // radioid.net registration from this field, so the useful thing
                // to do is name where it goes.
                if needsDMRRadioIDToConnect {
                    Text("Enter your DMR radio ID in Settings to connect via DMR.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .transition(.opacity.combined(with: .move(edge: .top)))
                }
                // The one place the directory is visible so far
                // (astar-refl-ship): what the typed name resolved to, and —
                // when the module is still blank — what is still missing. Not
                // styled as an error: nothing is wrong, the form is
                // unfinished. The picker and search pane are a separate item.
                if let resolvedReflectorLine {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(resolvedReflectorLine)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .accessibilityLabel("Resolved reflector")
                            .accessibilityValue(resolvedReflectorLine)
                        // The letter, offered where the gap is. Without this
                        // the typed path dead-ends at a correct-but-unfinished
                        // line: the operator is told a module is missing and
                        // given nowhere to supply one but the keyboard.
                        if case .needsModule(let entry) = reflectorResolution {
                            modulePicker(for: entry)
                        }
                        Spacer(minLength: 0)
                    }
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
            CallSession.requiresCallsign(selectedNetwork)
                && session.operatorCallsign.trimmingCharacters(in: .whitespaces).isEmpty
                && callsignDraft.trimmingCharacters(in: .whitespaces).isEmpty
        }

        /// Whether the M17 callsign prompt belongs in the dial card right now
        /// (astar-c2e5 Task 9): only for the M17 network, and only until a
        /// callsign is set (here or in Settings — either writes
        /// `session.m17Callsign`, so this hides either way).
        private var needsM17Callsign: Bool {
            CallSession.requiresCallsign(selectedNetwork)
                && session.operatorCallsign.trimmingCharacters(in: .whitespaces).isEmpty
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
                    .onAppear { callsignDraft = session.operatorCallsign }
                    .onChange(of: callsignDraft) { value in
                        let upper = value.uppercased()
                        if upper != value { callsignDraft = upper }
                    }
                    .onSubmit(commitCallsignDraft)
                    .accessibilityLabel("Your callsign")
                // One callsign, both reflector networks — M17 sends it in
                // every frame and D-Star puts it in every header, and it is
                // the same callsign. Naming the network the operator is
                // actually on keeps that concrete without implying there are
                // two settings.
                // DMR is the odd one: a `DMRD` frame carries a number and no
                // callsign at all, so the callsign rides in the LOGIN. Saying
                // "transmits" there would be wrong, and the difference is the
                // whole reason the radio ID is a separate field.
                Text(
                    selectedNetwork == .dmr
                        ? "DMR logs in with your callsign — set it once here or in Settings."
                        : "\(selectedNetwork.displayName) transmits your callsign — "
                            + "set it once here or in Settings."
                )
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
        }

        /// DMR's target row: the network, the talkgroup and the timeslot.
        ///
        /// A DMR target is four things — the network, the master, the talkgroup
        /// and the slot — because a talkgroup number names nothing on its own:
        /// TG 91 exists on several of these networks and is a different room on
        /// each. The dial field alone would mean typing all four; these three
        /// controls fill it in instead.
        ///
        /// **They edit the dial field, they do not shadow it.** Every control
        /// here reads its third of `node` and writes back the whole string
        /// (`DmrDialText`), so the field stays the single source of truth for
        /// what will be dialled — the same rule the D-Star module picker
        /// follows, and for the same reason: two places each holding half a
        /// target is how a UI comes to show one thing and dial another.
        private var dmrTargetRow: some View {
            HStack(spacing: 8) {
                dmrSystemMenu
                dmrTalkgroupField
                Picker("", selection: dmrTimeslotBinding) {
                    Text("TS1").tag(UInt8(1))
                    Text("TS2").tag(UInt8(2))
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(width: 96)
                .accessibilityLabel("Timeslot")
                .help("Which of the master's two timeslots to join. TS2 is the hotspot convention.")
            }
        }

        /// The master picker, grouped by network family — nine families over
        /// the directory's 111 system slugs, with everything they do not claim
        /// under one heading and dialled exactly the same way.
        ///
        /// BrandMeister's group is absent until the consent box in Settings is
        /// ticked (`DmrSystemCatalog.grouped(_:consented:)`). A submenu per
        /// family rather than one flat list: 185 masters in a single menu is
        /// not something anyone can scan.
        private var dmrSystemMenu: some View {
            Menu {
                ForEach(dmrGroupsCache) { group in
                    Menu(group.title) {
                        ForEach(group.systems) { system in
                            Button(system.name) { selectDMRSystem(system.entryID) }
                        }
                    }
                }
            } label: {
                Text(dmrSystemLabel)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            .menuStyle(.borderlessButton)
            .frame(maxWidth: 140)
            .disabled(dmrGroupsCache.isEmpty)
            .accessibilityLabel("DMR network")
            .accessibilityValue(dmrSystemLabel)
            .help(
                dmrGroupsCache.isEmpty
                    ? "No DMR masters in the directory yet — type an address as "
                        + "system:host:port/talkgroup/timeslot."
                    : "Pick the network and master to log in to. Each issues its own password.")
        }

        /// The talkgroup: typed, and picked from a list where one exists.
        ///
        /// Typing the number is the normal case and a complete one — it is how
        /// every DMR radio codeplug works. `session.dmrTalkgroups` is empty
        /// today because the directory publishes no per-system list yet, so the
        /// menu appears only if one ever arrives.
        private var dmrTalkgroupField: some View {
            HStack(spacing: 4) {
                TextField("Talkgroup", text: dmrTalkgroupBinding)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 84)
                    .accessibilityLabel("Talkgroup")
                if !dmrTalkgroupOptions.isEmpty {
                    Menu {
                        ForEach(dmrTalkgroupOptions) { talkgroup in
                            Button("\(talkgroup.tg) · \(talkgroup.name)") {
                                dmrTalkgroupBinding.wrappedValue = String(talkgroup.tg)
                            }
                        }
                    } label: {
                        Image(systemName: "list.bullet")
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .frame(width: 22)
                    .accessibilityLabel("Choose a talkgroup")
                }
            }
        }

        /// Refill `dmrGroupsCache`. Called when the feed loads or is replaced
        /// by a sync, and when the consent flag flips — the only two things
        /// that can change the answer.
        private func refreshDMRGroups() {
            dmrGroupsCache = DmrSystemCatalog.grouped(
                reflectors.entries, consented: session.brandmeisterConsent)
        }

        /// What the master picker reads: the chosen row's name, or a prompt.
        /// Looked up in the cached groups (185 masters at most) rather than in
        /// the whole 3,415-row directory.
        private var dmrSystemLabel: String {
            let address = DmrDialText.parts(node).address
            guard !address.isEmpty else { return "Network…" }
            for group in dmrGroupsCache {
                if let match = group.systems.first(where: { $0.entryID == address }) {
                    return match.name
                }
            }
            return address
        }

        /// The talkgroups published for the selected master's network, if any.
        /// Nothing publishes them yet, so the empty check short-circuits before
        /// any lookup runs.
        private var dmrTalkgroupOptions: [DmrTalkgroup] {
            guard !session.dmrTalkgroups.isEmpty else { return [] }
            let address = DmrDialText.parts(node).address
            guard let slug = DmrSystemCatalog.slug(forEntryID: address, in: reflectors.entries)
            else { return [] }
            return session.dmrTalkgroups[slug] ?? []
        }

        private var dmrTalkgroupBinding: Binding<String> {
            Binding(
                get: { DmrDialText.parts(node).talkgroup },
                set: { talkgroup in
                    let parts = DmrDialText.parts(node)
                    node = DmrDialText.compose(
                        address: parts.address, talkgroup: talkgroup, timeslot: parts.timeslot)
                })
        }

        private var dmrTimeslotBinding: Binding<UInt8> {
            Binding(
                get: { DmrDialText.parts(node).timeslot },
                set: { timeslot in
                    let parts = DmrDialText.parts(node)
                    node = DmrDialText.compose(
                        address: parts.address, talkgroup: parts.talkgroup, timeslot: timeslot)
                })
        }

        private func selectDMRSystem(_ entryID: String) {
            let parts = DmrDialText.parts(node)
            node = DmrDialText.compose(
                address: entryID, talkgroup: parts.talkgroup, timeslot: parts.timeslot)
        }

        /// Whether DMR's radio-ID requirement is unmet right now — mirrors
        /// `ConnectError.missingDMRRadioID` so Connect is off for the same
        /// reason it would otherwise throw.
        ///
        /// The master PASSWORD is deliberately not gated the same way: reading
        /// it means a Keychain round trip, and a view body runs at 20 Hz while
        /// the meters are live. Its refusal is explained on press instead, by a
        /// message that names where the network issues one.
        private var needsDMRRadioIDToConnect: Bool {
            selectedNetwork == .dmr && RadioID.sanitized(session.dmrRadioID).isEmpty
        }

        /// Commit the local callsign draft into `session.m17Callsign` (whose
        /// `didSet` persists it) — called on Enter in `m17CallsignField` and
        /// again right before a `.m17` Connect, so a dial started without
        /// leaving the field still picks up what was typed.
        private func commitCallsignDraft() {
            let trimmed = callsignDraft.trimmingCharacters(in: .whitespaces).uppercased()
            guard !trimmed.isEmpty else { return }
            session.operatorCallsign = trimmed
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

        /// Magnifying glass beside the dial field: opens the reflector search
        /// pane (astar-refl-ui, astar-5a41).
        ///
        /// Shown only for a network that HAS a reflector directory. AllStar
        /// nodes are not reflectors and never appear in this data, so offering
        /// to search it there would be an empty promise — the same reasoning
        /// that keeps the network picker itself hidden until a second network
        /// exists.
        @ViewBuilder
        private var reflectorSearchButton: some View {
            if selectedNetwork.reflectorNetwork != nil {
                Button {
                    navigation.pane = .reflectors
                } label: {
                    Image(systemName: "magnifyingglass")
                }
                .buttonStyle(.borderless)
                .disabled(needsCredentials)
                .help("Search the reflector directory")
                // Icon-only: `.help` is a hover tooltip, not the a11y label.
                .accessibilityLabel("Search the reflector directory")
            }
        }

        /// The module letters for a reflector the dial field has resolved but
        /// not finished — a menu rather than 26 buttons, because it sits
        /// inline under a field in a 330 pt popover.
        ///
        /// Writes back into `node` rather than holding a module of its own:
        /// two places each holding half a target is how a UI comes to show
        /// `XLX836 A` and dial `XLX836`.
        private func modulePicker(for entry: DirectoryEntry) -> some View {
            let options = ReflectorModuleOptions.options(for: entry.dial)
            let remembered = moduleMemory.module(for: entry)
            return Menu {
                ForEach(options, id: \.self) { letter in
                    Button {
                        moduleMemory.remember(letter, for: entry)
                        node = ReflectorDialText.applying(module: letter, to: node)
                    } label: {
                        // The remembered letter is marked, not pre-applied:
                        // it is a thing the operator did, shown back to them,
                        // and still their choice to repeat.
                        if letter == remembered {
                            Label("Module \(String(letter))", systemImage: "clock.arrow.circlepath")
                        } else {
                            Text("Module \(String(letter))")
                        }
                    }
                }
            } label: {
                Text("Set module")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .font(.caption)
            .disabled(options.isEmpty)
            .accessibilityLabel("Set module")
            .accessibilityHint(
                remembered.map { "Last used module \(String($0))" } ?? "No module chosen yet")
        }

        /// The last-heard line: whoever most recently keyed up on the live
        /// digital network, and — on D-Star only, because only D-Star has one
        /// — the slow-data message they sent with it.
        ///
        /// Network-agnostic by design. `session.lastHeard` is already the
        /// active network's own talker (D-Star's header callsign, YSF's
        /// frame-header callsign, M17's LSF source), so a network added later
        /// lights this line up without a change here.
        ///
        /// **Last heard, not talking now.** The engine keeps every one of
        /// those past end-of-transmission on purpose, so this names whoever
        /// most recently keyed up rather than whoever is keyed right now —
        /// the status dot and the RX meter are what say that. Absent entirely
        /// until the first transmission, because an empty "Hearing —" reads
        /// as a fault when the truth is that the reflector has simply been
        /// quiet.
        ///
        /// **Why a history and not just one name.** One line is a lie on a
        /// busy reflector: a station that keys a short tail after every over
        /// — observed on M17-KCW — overwrites the name of whoever was
        /// actually talking, so the operator reads the courtesy tone as the
        /// conversation. The two rows under the first put the real talker
        /// back on screen. They are the tail of `session.heardHistory`, which
        /// `CallSession` already caps at three rows from the active network.
        ///
        /// The first history row is *usually* the station the `Last heard`
        /// line already names, so it is dropped — but only when the callsigns
        /// actually match. They do not always: a blank or whitespace callsign
        /// is dropped from the history and not from `lastHeard`, the history
        /// is empty for one poll after a network switch, and it is empty when
        /// the engine read fails. Comparing rather than blindly dropping the
        /// first row is what keeps a real station from vanishing in those
        /// cases.
        ///
        /// **Attacker-controlled text.** Every line here — the talker, the
        /// slow data, each history callsign — is typed or keyed by whoever is
        /// on the reflector. `Text` renders them verbatim and interprets
        /// nothing, which is the whole requirement; the rows are keyed by
        /// offset because a callsign is neither unique nor trustworthy as
        /// identity.
        @ViewBuilder
        private var talkerLine: some View {
            if let talker = session.lastHeard {
                let rows = session.heardHistory
                // The history's newest row is the station `lastHeard` already
                // names — usually. When it is, it is dropped from the rows
                // below and its age dates the header instead, so the caption
                // says how stale the name is rather than implying it is live.
                let current = rows.first?.callsign == talker ? rows.first : nil
                let rest = current == nil ? rows : Array(rows.dropFirst())
                VStack(alignment: .leading, spacing: 1) {
                    let headerAge = current.map { HeardAge.label(ms: $0.ageMs) }
                    Text(
                        headerAge.map { "Last heard \(talker) · \($0)" }
                            ?? "Last heard \(talker)"
                    )
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .accessibilityLabel("Last heard")
                    .accessibilityValue(
                        headerAge.map { HeardAge.spoken(callsign: talker, age: $0) } ?? talker)
                    // Slow data is typed by whoever is transmitting on the
                    // reflector — attacker-controlled text from astar's point
                    // of view. `Text` renders it verbatim and interprets
                    // nothing, which is the whole requirement; the line limit
                    // stops a long one reflowing the status card. The bubble
                    // symbol is what keeps it from reading as another
                    // callsign row: it is a message, and it is the only line
                    // here that is not a station.
                    if session.activeCallNetwork == .dstar,
                        let message = session.dstarSlowText, !message.isEmpty
                    {
                        Label(message, systemImage: "text.bubble")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                            .truncationMode(.tail)
                            .help(message)
                            .accessibilityLabel("Message")
                            .accessibilityValue(message)
                    }
                    // The history is its own group: a couple of points of air
                    // above the first row separate the stations heard earlier
                    // from the talker (and the message) above them.
                    ForEach(Array(rest.enumerated()), id: \.offset) { index, row in
                        let age = HeardAge.label(ms: row.ageMs)
                        Text("\(row.callsign) · \(age)")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                            .truncationMode(.tail)
                            .padding(.top, index == 0 ? 2 : 0)
                            .help("\(row.callsign) · \(age)")
                            .accessibilityLabel("Heard earlier, \(index + 1) of \(rest.count)")
                            .accessibilityValue(HeardAge.spoken(callsign: row.callsign, age: age))
                    }
                }
            }
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
                if !session.canTransmit {
                    // Checked BEFORE listen-only: that is a setting the
                    // operator can turn off, this is a property of the
                    // network. Showing "TX disabled" here would invite
                    // someone to go looking for the switch that fixes it.
                    receiveOnlyIndicator
                } else if session.txDisabled {
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

        /// Replaces the PTT/VOX control on a network astar can receive but not
        /// transmit — NXDN today, System Fusion until 2026-09-06.
        ///
        /// A PTT button that did nothing would be worse than an absent one:
        /// the operator would key, hear nothing happen, and reasonably assume
        /// the radio or the link was broken. Naming the reason costs one line
        /// and answers the question before it is asked.
        ///
        /// The network is read live rather than written into the string, so
        /// the sentence stays true as networks gain and lose transmit paths —
        /// this text and `CallSession.canTransmit` cannot drift apart.
        private var receiveOnlyIndicator: some View {
            let name = (session.activeCallNetwork ?? .allstar).displayName
            return Label(
                "Receive only — astar can't transmit \(name) yet",
                systemImage: "antenna.radiowaves.left.and.right"
            )
            .font(.callout.weight(.semibold))
            .frame(maxWidth: .infinity)
            .padding(.vertical, 10)
            .background(
                Color.secondary.opacity(0.18),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous)
            )
            .foregroundStyle(.secondary)
            .accessibilityElement()
            .accessibilityLabel("Receive only, astar cannot transmit \(name) yet")
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
        private func applyPollState(for pane: AppPane) {
            // Any pane but the call card: no meters on screen, and the 20 Hz
            // churn re-renders whatever list is there (device pickers in
            // Settings, 1,400 reflector rows here) on every tick.
            if pane != .call && !serial.isActive {
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
                guard session.status == .answered, !session.voxEnabled, session.canTransmit
                else { return event }
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
                    navigation.showsSettings = true
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

        /// Whether the dial field currently holds a COMPLETE target for the
        /// selected network — gates the Connect button and, via `connect()`,
        /// the Enter/onSubmit path too.
        ///
        /// The decision itself lives in `CallSession.canDial` (astar-refl-ship)
        /// so the button and the dial agree by construction: reflector names
        /// resolve through the directory first and addresses parse second,
        /// with each network's own grammar underneath (astar-c2e5 Task 9).
        /// A resolved reflector with no module yet answers `false` — nothing
        /// is wrong, the form is unfinished, and `resolvedReflectorLine` says
        /// so below the field.
        private var isDialTargetValid: Bool {
            session.canDial(node, network: selectedNetwork)
        }

        /// The resolved-target line under the dial field (astar-refl-ship):
        /// `XLX836 · module — · 45.56.69.219:30001`, filling itself in as the
        /// module arrives. `nil` — and so absent — whenever the text names
        /// nothing in the directory, which includes every address and every
        /// launch before the directory has loaded.
        private var resolvedReflectorLine: String? { reflectorResolution.statusLine }

        /// What the dial field's text resolves to in the directory, for the
        /// line under the field AND the module picker beside it. One read, so
        /// the sentence and the control that completes it cannot disagree
        /// about which reflector is being talked about.
        ///
        /// `.notInDirectory` for a network with no directory counterpart
        /// (AllStar), which is also what it means: nothing here names a
        /// reflector, so nothing is shown.
        private var reflectorResolution: ReflectorDialResolution {
            guard let network = selectedNetwork.reflectorNetwork else { return .notInDirectory }
            return session.resolveReflector(node, network: network)
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
            case .m17, .dstar, .ysf, .nxdn, .dmr:
                // Every reflector network resolves its target engine-side
                // (`CallSession.connect(node:network:)` → `m17Target` /
                // `dstarTarget` / `ysfTarget` / `nxdnTarget` / `dmrTarget`,
                // directory first and address second) — this is
                // only the same "unreachable via the disabled button, but
                // refuse it on Enter too" guard as above.
                guard session.canDial(node, network: network) else { return }
                // Pick up whatever's in the callsign prompt (if it's still
                // showing) before dialing, so a dial started without leaving
                // that field still uses what was typed (astar-c2e5 Task 9).
                commitCallsignDraft()
                dispatchConnect(node: trimmedNode, network: network, address: nil)
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
                // Whole or not at all (astar-5e2c). No amount of layout priority
                // can win this row: the codec/network badges beside it are
                // fixedSize, so the status column cannot compress below their
                // combined width, while a Text's minimum is zero. Every point of
                // deficit therefore lands here. Measured on a 310pt window with a
                // legacy scroller the row has 243pt to spend and wants ~255, and
                // the readout was handed 5pt of it — rendering "93 ms" as a bare
                // "9". Priority tweaks only moved which wrong thing was shown:
                // unbounded it wrapped one character per line into a vertical
                // strip, lineLimit(1) truncated it to a single digit.
                //
                // A latency figure clipped to its first digit is a WRONG NUMBER on
                // screen, which is worse than no number: "9" and "93" and "935" ms
                // are three very different calls. So ViewThatFits renders it at its
                // full ideal width or drops it entirely, and the width it would
                // have taken goes back to the connected-node label. It returns by
                // itself the moment the window is widened.
                //
                // Still not fixedSize at the row level — that made the window jump
                // wider the moment a call answered. The pin lives inside the first
                // branch, where it means "this width or nothing" rather than
                // "grow the window to fit me".
                ViewThatFits(in: .horizontal) {
                    Text("\(rtt) ms")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                    Color.clear.frame(width: 0, height: 0)
                }
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
        let previewNavigation = AppNavigation()
        MenuPopover()
            .environmentObject(previewSession)
            .environmentObject(SerialController())
            .environmentObject(SetupController())
            .environmentObject(
                MicAnalyzerController(session: previewSession, navigation: previewNavigation)
            )
            .environmentObject(AudioDeviceMonitor(session: previewSession))
            .environmentObject(previewNavigation)
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
