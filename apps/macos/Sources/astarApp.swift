// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarCore
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
            // StatusItemController owns the status item + window. An empty Settings
            // scene satisfies the `App` scene requirement without showing a window.
            //
            // It is NOT the app's settings UI and is unreachable (astar-1f7d):
            // `MainMenu.install` replaces `NSApp.mainMenu` in
            // `applicationDidFinishLaunching`, so ⌘, and astar → Settings… both go
            // to `StatusItemController.showSettings()`. When the app was
            // accessory-only this scene had no menu item at all; once astar-7c31
            // promoted it to `.regular` it acquired one, and 0.1.1beta shipped a
            // Settings… item that opened this empty window. Don't wire anything
            // here — put it in the settings pane the rest of the app uses.
            Settings { EmptyView() }
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
            setups.attach(session: session, serial: serial)
            statusController = StatusItemController(
                session: session, serial: serial, setups: setups, micAnalyzer: micAnalyzer,
                deviceMonitor: deviceMonitor, navigation: navigation)
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
        /// The standard About panel, with the docs/QRZ links as its credits. A
        /// custom window would be a second thing to keep in sync with the app's
        /// real version; this reads `CFBundleShortVersionString` itself.
        func showAbout(_ sender: Any?) {
            NSApp.activate(ignoringOtherApps: true)
            NSApp.orderFrontStandardAboutPanel(options: [.credits: Self.aboutCredits])
        }

        /// ⌘, — open the real settings pane, not the empty placeholder scene.
        func showSettings(_ sender: Any?) {
            statusController?.showSettings()
        }

        func openHome(_ sender: Any?) { NSWorkspace.shared.open(AboutLinks.homePage) }
        func openIssues(_ sender: Any?) { NSWorkspace.shared.open(AboutLinks.issues) }
        func openQRZ(_ sender: Any?) { NSWorkspace.shared.open(AboutLinks.qrz) }

        /// Clickable links for the About panel. `.credits` takes an attributed
        /// string, which is the only way to get real links into the standard panel.
        private static var aboutCredits: NSAttributedString {
            let credits = NSMutableAttributedString()
            let body = NSFont.systemFont(ofSize: NSFont.smallSystemFontSize)
            credits.append(
                NSAttributedString(
                    string: "A native ham-radio client for AllStarLink, M17 and D-Star.\n\n",
                    attributes: [.font: body]))
            credits.append(link("Documentation", AboutLinks.homePage, font: body))
            credits.append(NSAttributedString(string: "\n", attributes: [.font: body]))
            credits.append(link("\(AboutLinks.callsign) on QRZ", AboutLinks.qrz, font: body))
            credits.setAlignment(.center, range: NSRange(location: 0, length: credits.length))
            return credits
        }

        private static func link(_ text: String, _ url: URL, font: NSFont) -> NSAttributedString {
            NSAttributedString(string: text, attributes: [.link: url, .font: font])
        }
    }
#endif
