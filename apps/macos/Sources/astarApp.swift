// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarCore
import Combine
import SwiftUI

/// astar — a native AllStarLink client.
///
/// On macOS, astar is a **menu-bar app**: it lives in the status bar, and by
/// default in the Dock as well (astar-7c31 — `LSUIElement` in project.yml keeps
/// it launching as an accessory, and `DockPolicy.apply()` promotes it). An
/// AppKit `StatusItemController` (via `AppDelegate`) owns the status item; a
/// left click shows/hides the main window (movable / hideable / closable) while
/// a right click shows a quick status + audio/VOX + disconnect menu and the
/// `Show in Dock` toggle. The asterisk stays a live TX/RX/connected indicator.
/// On iOS it is a standard windowed app.
///
/// The app owns a single `CallSession` (au-e00f) — the observable view-model
/// over the AstarStation poll loop — and shares it with the UI. The menu-bar
/// status item uses the `MenuBarRainbow` asset (astar-cdab), tinted per state.
@main
struct AstarApp: App {
    #if os(macOS)
        @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    #else
        @StateObject private var session = CallSession.live()
    #endif

    var body: some Scene {
        #if os(macOS)
            // Menu-bar-only: no SwiftUI window/scene. The AppDelegate's
            // StatusItemController owns the status item + window. An empty
            // Settings scene satisfies the `App` scene requirement without
            // showing a window.
            //
            // It is NOT the app's settings UI, and the menu item it creates is
            // REMOVED below. astar-1f7d assumed `MainMenu.install` made that
            // item unreachable by replacing `NSApp.mainMenu` in
            // `applicationDidFinishLaunching`; it does not. SwiftUI installs
            // its own menu after that runs and wins — the live menu bar carries
            // SwiftUI's View menu, which MainMenu never builds. So the shipped
            // astar → Settings… opened this empty window, exactly the bug
            // astar-1f7d set out to fix.
            //
            // `CommandGroup(replacing: .appSettings)` with no content deletes
            // the item at the source rather than trying to out-race SwiftUI for
            // ownership of the menu bar. Settings stays reachable where it has
            // always actually been: the gear in the popover footer.
            Settings { EmptyView() }
                .commands {
                    CommandGroup(replacing: .appSettings) {}
                }
        #else
            WindowGroup {
                ContentView().environmentObject(session)
            }
        #endif
    }
}

#if os(macOS)
    /// The one place that maps the stored "Show in Dock" preference onto AppKit's
    /// activation policy (astar-7c31), shared by launch and the menu toggle so the
    /// two cannot drift.
    ///
    /// Static, and deliberately not a method on `AppDelegate`: under
    /// `@NSApplicationDelegateAdaptor`, `NSApp.delegate` is SwiftUI's own
    /// `SwiftUI.AppDelegate` wrapper, *not* our `AppDelegate`. Reaching for it with
    /// `NSApp.delegate as? AppDelegate` silently yields nil, which is how the
    /// toggle came to save the preference and change nothing until relaunch.
    ///
    /// `LSUIElement: YES` stays in the Info.plist on purpose: the app launches as
    /// an accessory and promotes itself here, so someone with the preference off
    /// never sees a Dock icon flash.
    enum DockPolicy {
        static func apply() {
            let presence = DockPresence(showInDock: DockPreference().load())
            NSApp.setActivationPolicy(presence.showsInDock ? .regular : .accessory)
        }
    }

    /// Owns the long-lived `CallSession` + serial PTT source and stands up the
    /// menu-bar status item once the app finishes launching.
    @MainActor
    final class AppDelegate: NSObject, NSApplicationDelegate {
        let session = CallSession.live()
        // The cached reflector directory (astar-refl-ship). Constructed here,
        // on the main actor, because it is `@MainActor` — `CallSession.live()`
        // is not, and the dial path holds the directory's `index` value rather
        // than the directory itself for exactly that reason. The bundled
        // snapshot in `Contents/Resources/reflectors.json` means this is
        // populated on a first launch with no network.
        let reflectors = ReflectorDirectory(storage: FileReflectorDirectoryStorage())
        /// Keeps `session.reflectorIndex` equal to the directory's, so a sync
        /// that replaces the feed also replaces what the dial field resolves
        /// against. Retained here; the subscription is what does the work.
        private var reflectorIndexSubscription: AnyCancellable?
        // The macOS-only serial PTT source (UCI150). Owns the IOKit-linked
        // SerialClient and installs CallSession's serial-free pttSourceTick hook, so
        // AstarCore stays multiplatform. Re-opens on launch if previously enabled.
        let serial = SerialController()
        // Named hardware Setups ("UCI150 desk" ↔ "Jabra mobile"): one-click rig
        // switching that drives `serial` + the session's device selection together.
        let setups = SetupController()
        // Reactive audio-device list backed by a CoreAudio hotplug listener, so the
        // pickers never enumerate on view-appear (which froze the Quick-settings
        // reveal) and stay live when a mic/interface is plugged in or removed.
        lazy var deviceMonitor = AudioDeviceMonitor(session: session)
        lazy var micAnalyzer = MicAnalyzerController(session: session)
        // Which pane the main window shows. Owned here because BOTH the popover's
        // footer button and the main menu's Settings… item drive it (astar-1f7d).
        let navigation = AppNavigation()
        private var statusController: StatusItemController?
        // Posts VoiceOver announcements for call-session events (astar-b167,
        // accessibility-audit F6) — a sibling of `statusController`, not owned
        // by the popover, so it lives whether or not the window is open (same
        // reasoning as `session.start()` below). Retained via this property;
        // its Combine subscriptions are what does the actual work.
        private var announcer: AccessibilityAnnouncer?

        /// Promote the app out of `LSUIElement` accessory mode when "Show in Dock"
        /// is on (astar-7c31). The Info.plist keeps `LSUIElement: YES` on purpose:
        /// the app always *launches* as an accessory and promotes itself here, so
        /// someone with the preference off never sees a Dock icon flash. Removing
        /// LSUIElement and demoting instead would produce exactly that flash.
        ///
        /// One apply path for both launch and the menu toggle, so the two cannot
        /// drift.
        func applicationDidFinishLaunching(_ notification: Notification) {
            // Record which config version wrote this preferences domain, before
            // anything reads it (astar-b52e). A domain that has never been
            // stamped already reads as version 1, so this is not what makes
            // migration possible today — it is what makes the NEXT version able
            // to tell 1 from 2 without guessing from which keys happen to exist.
            ConfigVersion.stamp()
            // Hand the dial path the directory's name lookup, and keep handing
            // it: `$index` republishes on every load and every sync. With no
            // snapshot and no cache this is `.empty`, and dialling is
            // address-only — which is what it was before the directory
            // existed, not a failure (astar-refl-ship).
            session.reflectorIndex = reflectors.index
            reflectorIndexSubscription = reflectors.$index
                .sink { [weak session] index in session?.reflectorIndex = index }
            setups.attach(session: session, serial: serial)
            statusController = StatusItemController(
                session: session, serial: serial, setups: setups, micAnalyzer: micAnalyzer,
                deviceMonitor: deviceMonitor, navigation: navigation, reflectors: reflectors)
            // Replace SwiftUI's placeholder menu (whose Settings… item opened the
            // empty `Settings { EmptyView() }` scene) with a real one — astar-1f7d.
            MainMenu.install(target: self)
            announcer = AccessibilityAnnouncer(session: session)
            // Baseline poll for the whole app lifetime, so the menu-bar tint, serial
            // PTT, and right-click status stay live even when the popover is closed.
            // The popover pauses this only while Settings is open (to avoid the device
            // pickers re-rendering at 20 Hz) — see MenuPopover.
            session.start()
            // Apply the persisted app-global "Spectrum decay" preference at launch
            // (astar-68a6) so the engine + the inactive fade use it before the first
            // spectrum renders. Re-asserted later whenever a new analyzer appears.
            session.setSpectrumDecay(Float(SpectrumDecayPref.current()))
            // Last: the status item is up, so the Dock icon (if enabled) appears
            // together with the menu-bar asterisk rather than ahead of it.
            DockPolicy.apply()
            showWelcomeIfUnconfigured()
            // The unattended directory refresh (astar-refl-ui). Started and
            // forgotten on purpose: it must never delay launch, and it has
            // nothing to report to — a failure lands on
            // `reflectors.lastSyncError`, which the Settings section shows.
            // That is why this sync waited for that section to exist.
            //
            // Cheap in the common case. `.automatic` returns
            // `.skipped(.notDue)` without opening a socket until the feed's own
            // `client_refresh_days` (7) has passed since the last definitive
            // answer — the cadence is the publisher's number, not astar's, so a
            // change upstream takes effect without a release. When it does run
            // it is a conditional GET, and hamcall-db's build is byte-stable,
            // so most weeks it costs a 304.
            Task { @MainActor in try? await reflectors.sync(trigger: .automatic) }
        }

        /// First launch with no AllStarLink account: raise the window on Settings
        /// (astar-4e8a). astar is a menu-bar app, so otherwise a new user sees an
        /// asterisk and nothing else, with no hint that an account is needed
        /// before the dial field will accept anything.
        ///
        /// Once only — M17 needs no portal login, so running astar without
        /// AllStarLink credentials is legitimate and must not be nagged at.
        private func showWelcomeIfUnconfigured() {
            let welcome = WelcomePreference()
            guard
                FirstRunPresentation.raisesWindowAtLaunch(
                    hasCredentials: session.hasCredentials,
                    hasShownWelcome: welcome.hasShownWelcome)
            else { return }
            welcome.markShown()
            statusController?.showSettings()
        }

        /// Clicking the Dock icon opens the main window (astar-7c31). AppKit only
        /// calls this when the app is already running; the return value tells AppKit
        /// whether it should do its own default reopen handling, and `false` keeps
        /// it out of the way since we have handled it.
        func applicationShouldHandleReopen(
            _ sender: NSApplication, hasVisibleWindows: Bool
        ) -> Bool {
            if DockPresence.shouldShowWindowOnReopen(hasVisibleWindows: hasVisibleWindows) {
                statusController?.showWindow()
            }
            return false
        }
    }

    // MARK: - Main menu actions (astar-1f7d)

    extension AppDelegate: MainMenuActions {
        /// The standard About panel. Its contents come from the bundle —
        /// `NSHumanReadableCopyright` for the copyright line, `Credits.html`
        /// for the description and links — so this passes no options.
        ///
        /// That is deliberate. Supplying `.credits` here would override the
        /// bundled file, and the panel would then say something different
        /// depending on which menu opened it — and this menu is currently the
        /// one users do NOT get.
        func showAbout(_ sender: Any?) {
            NSApp.activate(ignoringOtherApps: true)
            NSApp.orderFrontStandardAboutPanel()
        }

        /// ⌘, — open the real settings pane, not the empty placeholder scene.
        func showSettings(_ sender: Any?) {
            statusController?.showSettings()
        }

        func openHome(_ sender: Any?) { NSWorkspace.shared.open(AboutLinks.homePage) }
        func openIssues(_ sender: Any?) { NSWorkspace.shared.open(AboutLinks.issues) }
        func openQRZ(_ sender: Any?) { NSWorkspace.shared.open(AboutLinks.qrz) }

    }
#endif
