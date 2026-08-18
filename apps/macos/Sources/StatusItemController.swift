// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import Combine
    import SwiftUI
    import AstarCore

    /// Manages astar's menu-bar status item in AppKit. A **left** click shows/hides
    /// the main window (movable / hideable / closable, hosting the SwiftUI
    /// `MenuPopover` call UI); a **right** click shows a quick menu (status, audio /
    /// VOX toggles, disconnect, quit). The asterisk itself stays as a live TX/RX/
    /// connected indicator. Owns the `NSStatusItem` and the main `NSWindow`.
    @MainActor
    final class StatusItemController: NSObject, NSMenuDelegate {
        private let session: CallSession
        private let serial: SerialController
        private let setups: SetupController
        private let micAnalyzer: MicAnalyzerController
        private let deviceMonitor: AudioDeviceMonitor
        /// Which pane the window shows. Shared with the main menu so ⌘, and the
        /// popover's own footer button drive the same settings pane (astar-1f7d).
        private let navigation: AppNavigation
        private let statusItem: NSStatusItem
        private var cancellables = Set<AnyCancellable>()
        /// The main window — created on first use; the asterisk toggles its visibility.
        /// Replaces the old transient popover so the UI persists and can be moved.
        private lazy var window: NSWindow = makeWindow()

        init(
            session: CallSession, serial: SerialController, setups: SetupController,
            micAnalyzer: MicAnalyzerController, deviceMonitor: AudioDeviceMonitor,
            navigation: AppNavigation
        ) {
            self.session = session
            self.serial = serial
            self.setups = setups
            self.micAnalyzer = micAnalyzer
            self.deviceMonitor = deviceMonitor
            self.navigation = navigation
            self.statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
            super.init()

            if let button = statusItem.button {
                // The brand asterisk, tinted live by call state (see updateIcon /
                // asterisk()); the resting idle state shows the rainbow mark.
                button.image = rainbowAsterisk() ?? asterisk(nil)
                button.toolTip = "astar"
                button.target = self
                button.action = #selector(handleClick(_:))
                button.sendAction(on: [.leftMouseUp, .rightMouseUp])
            }

            // Tint the asterisk by live call state: pale red on TX, green on RX, blue when
            // connected (on a call, nobody talking), default (follows the menu bar)
            // when idle. Driven off the session's published state so it tracks the
            // call without the popover being open. `receiving` is computed in
            // CallSession (rx-audio activity OR remotePTT). "Connected" requires an
            // actual dialed call — status alone can read .answered for an idle WT
            // session, which shouldn't tint the asterisk.
            Publishers.CombineLatest4(
                session.$ptt, session.$receiving, session.$status, session.$dialedNode
            )
            .map { ptt, receiving, status, dialedNode -> StatusIconState in
                StatusIconState(
                    transmitting: ptt,
                    receiving: receiving,
                    connected: status == .answered && dialedNode != nil)
            }
            .removeDuplicates()
            .receive(on: RunLoop.main)
            .sink { [weak self] state in self?.updateIcon(state) }
            .store(in: &cancellables)

            // Install the serial PTT source (was done in MenuPopover.onAppear under
            // MenuBarExtra); do it once here now that we own the lifecycle.
            serial.attach(to: session)
        }

        // MARK: - Click routing

        @objc private func handleClick(_ sender: NSStatusBarButton) {
            let event = NSApp.currentEvent
            let isSecondary =
                event?.type == .rightMouseUp
                || (event?.type == .leftMouseUp && event?.modifierFlags.contains(.control) == true)
            if isSecondary {
                showMenu()
            } else {
                toggleWindow()
            }
        }

        // MARK: - Main window

        /// Build the movable/closable window hosting the SwiftUI call UI. Closing it
        /// hides (we keep the instance); position/size persist via the autosave name.
        private func makeWindow() -> NSWindow {
            let hosting = NSHostingController(
                rootView: MenuPopover()
                    .environmentObject(session)
                    .environmentObject(serial)
                    .environmentObject(setups)
                    .environmentObject(micAnalyzer)
                    .environmentObject(deviceMonitor)
                    .environmentObject(navigation)
            )
            let w = NSWindow(contentViewController: hosting)
            w.styleMask = [.titled, .closable, .miniaturizable, .resizable]
            // Opt into native full-screen so the green button shows the standard
            // full-screen arrows (⤢) instead of the older "+" zoom glyph.
            w.collectionBehavior.insert(.fullScreenPrimary)
            w.title = "astar"
            w.isReleasedWhenClosed = false  // close just hides it
            w.contentMinSize = NSSize(width: 310, height: 450)
            // Non-opaque so the SwiftUI VisualEffectView background can show the
            // translucent/blurred desktop behind the content. The title bar keeps its
            // normal (system-translucent) appearance — making it transparent too left
            // a see-through strip, since the content material doesn't extend under it.
            w.isOpaque = false
            w.backgroundColor = .clear
            // Bumped key so the +10pt larger default takes effect once instead of
            // restoring the old saved frame.
            w.setFrameAutosaveName("astarMainWindow.v2")
            if !w.setFrameUsingName("astarMainWindow.v2") {  // first run: place under the star
                positionUnderStatusItem(w)
            }
            return w
        }

        /// Show and focus the window, never hide it. The Dock-icon reopen path
        /// (astar-7c31) uses this rather than `toggleWindow()`: a second Dock click
        /// hiding the window is behaviour no Mac app has.
        func showWindow() {
            // With no AllStarLink account the dial field is disabled, so the call
            // UI is a form nobody can type into. Land on Settings instead
            // (astar-4e8a) — it self-corrects the moment an account is saved.
            if FirstRunPresentation.opensOnSettings(hasCredentials: session.hasCredentials) {
                navigation.showsSettings = true
            }
            NSApp.activate(ignoringOtherApps: true)
            window.makeKeyAndOrderFront(nil)
        }

        /// Show the window already switched to Settings — what the main menu's
        /// `Settings…` item (⌘,) does (astar-1f7d). Deliberately routes to the same
        /// pane as the popover's footer button rather than opening a second window:
        /// astar has one settings surface.
        func showSettings() {
            navigation.showsSettings = true
            showWindow()
        }

        /// Show (and focus) or hide the window. Since astar can be a menu-bar
        /// (accessory) app, showing must also activate so the window comes to the
        /// front.
        private func toggleWindow() {
            if window.isVisible {
                window.orderOut(nil)
            } else {
                showWindow()
            }
        }

        /// Drop the window just below the status item the first time it opens (before
        /// any saved frame exists).
        private func positionUnderStatusItem(_ w: NSWindow) {
            guard let button = statusItem.button, let btnWindow = button.window else {
                w.center()
                return
            }
            let onScreen = btnWindow.convertToScreen(button.convert(button.bounds, to: nil))
            let origin = NSPoint(
                x: onScreen.midX - w.frame.width / 2,
                y: onScreen.minY - w.frame.height - 4)
            w.setFrameOrigin(origin)
        }

        // MARK: - Icon tint

        /// Tint the asterisk: pale red transmitting, green receiving, blue
        /// connected, and the rainbow brand mark when idle. Every state renders
        /// the SAME asset geometry (`MenuBarRainbow`, solid-filled for the
        /// colored states) — mixing the asset with SF-symbol glyphs made the
        /// item visibly grow/shrink between states (astar-3f57).
        private func updateIcon(_ state: StatusIconState) {
            guard let button = statusItem.button else { return }
            switch state {
            case .idle:
                button.image = rainbowAsterisk() ?? asterisk(nil)
            case .connected:
                button.image = tintedAsterisk(.systemBlue) ?? asterisk(.systemBlue)
            case .transmitting:
                let paleRed = NSColor(red: 0.95, green: 0.45, blue: 0.45, alpha: 1)
                button.image = tintedAsterisk(paleRed) ?? asterisk(paleRed)
            case .receiving:
                button.image = tintedAsterisk(.systemGreen) ?? asterisk(.systemGreen)
            }
        }

        /// Point size of the menu-bar asterisk — noticeably larger than the system
        /// default (~13) so it reads clearly and fills the bar.
        private static let markPointSize: CGFloat = 20

        /// The brand asterisk filled with a solid color: the `MenuBarRainbow`
        /// asset with `color` composited `sourceAtop`, so the colored states share
        /// the idle mark's exact geometry and the menu-bar item never changes
        /// size. Returns nil if the asset can't be loaded (callers fall back to
        /// the SF-symbol asterisk).
        private func tintedAsterisk(_ color: NSColor) -> NSImage? {
            guard let base = NSImage(named: "MenuBarRainbow") else { return nil }
            let size = NSSize(width: Self.markPointSize, height: Self.markPointSize)
            let img = NSImage(size: size, flipped: false) { rect in
                base.draw(in: rect)
                color.set()
                rect.fill(using: .sourceAtop)
                return true
            }
            img.isTemplate = false
            return img
        }

        /// An asterisk glyph at `markPointSize` — the FALLBACK look (SF Symbol)
        /// used only if the brand asset fails to load: template (system-tinted)
        /// when `color` is nil, else rendered in that color.
        private func asterisk(_ color: NSColor?) -> NSImage? {
            let base = NSImage(systemSymbolName: "asterisk", accessibilityDescription: "astar")
            let size = NSImage.SymbolConfiguration(pointSize: Self.markPointSize, weight: .bold)
            guard let color else {
                let img = base?.withSymbolConfiguration(size)
                img?.isTemplate = true
                return img
            }
            let img = base?.withSymbolConfiguration(size.applying(.init(paletteColors: [color])))
            img?.isTemplate = false
            return img
        }

        /// The rainbow brand asterisk (matching the app icon) used for the idle
        /// state — a non-template colour asset. Returns nil if the asset can't be
        /// loaded, so callers fall back to the template glyph (`asterisk(nil)`).
        private func rainbowAsterisk() -> NSImage? {
            guard let img = NSImage(named: "MenuBarRainbow") else { return nil }
            img.isTemplate = false
            img.size = NSSize(width: Self.markPointSize, height: Self.markPointSize)
            return img
        }

        // MARK: - Right-click status menu

        private func showMenu() {
            // Refresh state once so the menu reflects the live call even when the
            // popover (and its poll loop) has been closed.
            session.poll()

            let menu = buildMenu()
            menu.delegate = self
            statusItem.menu = menu  // attach…
            statusItem.button?.performClick(nil)  // …and pop it under the item
        }

        /// Clear the transient menu once it closes, so the next left click opens the
        /// popover again instead of re-showing the menu.
        func menuDidClose(_ menu: NSMenu) {
            statusItem.menu = nil
        }

        private func buildMenu() -> NSMenu {
            let menu = NSMenu()

            let header = NSMenuItem(title: statusLine(), action: nil, keyEquivalent: "")
            header.isEnabled = false
            menu.addItem(header)

            // Disable TX (listen-only) right next to the status line.
            menu.addItem(
                toggleItem("Disable TX", session.txDisabled, #selector(toggleListenOnlyAction)))

            // Quick rig switch: a "Setup ▸" submenu with a ✓ on the active one.
            if !setups.setups.isEmpty {
                menu.addItem(setupSubmenuItem())
            }

            if session.status == .answered || session.status == .dialing {
                menu.addItem(.separator())
                menu.addItem(item("Disconnect", #selector(disconnectAction)))
            }

            // Quick audio / VOX settings — reflect + toggle live session state.
            menu.addItem(.separator())
            let audioHeader = NSMenuItem(title: "Audio", action: nil, keyEquivalent: "")
            audioHeader.isEnabled = false
            menu.addItem(audioHeader)
            menu.addItem(
                toggleItem(
                    "Voice compression", session.effectiveCompression,
                    #selector(toggleCompression)))
            menu.addItem(
                toggleItem(
                    "Noise reduction", session.effectiveNoiseReduction,
                    #selector(toggleNoiseReduction)))
            menu.addItem(
                toggleItem("VOX (voice-activated)", session.voxEnabled, #selector(toggleVoxAction)))

            menu.addItem(.separator())
            menu.addItem(
                item(
                    window.isVisible ? "Hide astar Window" : "Show astar Window",
                    #selector(openAction)))
            menu.addItem(
                toggleItem(
                    "Show in Dock", DockPreference().load(), #selector(toggleDockAction)))
            menu.addItem(item("Quit astar", #selector(quitAction), key: "q"))
            return menu
        }

        /// The "Setup ▸" submenu: one item per Setup (subtitled with its hardware),
        /// ✓ on the active one, applying it on click. The submenu's own items target
        /// `self`, so they fire even though the parent item has no action.
        private func setupSubmenuItem() -> NSMenuItem {
            let parent = NSMenuItem(title: "Config", action: nil, keyEquivalent: "")
            let submenu = NSMenu()
            for setup in setups.setups {
                let i = NSMenuItem(
                    title: setup.name, action: #selector(applySetupAction(_:)), keyEquivalent: "")
                i.target = self
                i.representedObject = setup.id
                i.state = (setup.id == setups.selectedID) ? .on : .off
                i.toolTip = setups.hardwareName(for: setup)
                submenu.addItem(i)
            }
            parent.submenu = submenu
            return parent
        }

        private func item(_ title: String, _ action: Selector, key: String = "") -> NSMenuItem {
            let i = NSMenuItem(title: title, action: action, keyEquivalent: key)
            i.target = self
            return i
        }

        /// A checkable menu item reflecting `isOn`.
        private func toggleItem(_ title: String, _ isOn: Bool, _ action: Selector) -> NSMenuItem {
            let i = item(title, action)
            i.state = isOn ? .on : .off
            return i
        }

        /// Live one-line status, gated on `status` so a stale `dialedNode` (which can
        /// outlive a remote hangup) never shows as "connected".
        private func statusLine() -> String {
            switch session.status {
            case .idle: return "Not connected"
            case .hangup: return "Call ended"
            case .dialing:
                return session.dialedNode.map { "Connecting to node \($0)…" } ?? "Connecting…"
            case .answered:
                return session.dialedNode.map { "Connected to node \($0)" } ?? "Connected"
            }
        }

        // MARK: - Actions

        @objc private func disconnectAction() { try? session.disconnect() }

        /// Apply the Setup whose id is carried in the menu item's represented object.
        @objc private func applySetupAction(_ sender: NSMenuItem) {
            guard let id = sender.representedObject as? String else { return }
            setups.apply(id: id)
        }

        @objc private func openAction() { toggleWindow() }

        // Routed through CallSession's context-aware toggles (astar-5d8e) —
        // NOT `setCompression`/`setNoiseReduction` directly, which would
        // clobber a live M17 override and silently persist into the shared
        // AudioSettings once the call ends and the standard chain restores.
        @objc private func toggleCompression() { session.toggleCompression() }
        @objc private func toggleNoiseReduction() { session.toggleNoiseReduction() }
        @objc private func toggleVoxAction() { session.setVoxEnabled(!session.voxEnabled) }
        @objc private func toggleListenOnlyAction() { session.setTxDisabled(!session.txDisabled) }

        /// Flip "Show in Dock" (astar-7c31), then re-apply through `DockPolicy` —
        /// the same path launch uses, so the two cannot drift.
        @objc private func toggleDockAction() {
            let pref = DockPreference()
            pref.save(!pref.load())
            DockPolicy.apply()
        }

        @objc private func quitAction() { NSApp.terminate(nil) }
    }
#endif
