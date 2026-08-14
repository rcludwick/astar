// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarCore
import SwiftUI

/// astar — a native AllStarLink client.
///
/// On macOS, astar is a **menu-bar app**: it lives in the status bar (no Dock
/// icon — see `LSUIElement` in project.yml). An AppKit `StatusItemController`
/// (via `AppDelegate`) owns the status item; a left click shows/hides the main
/// window (movable / hideable / closable) while a right click shows a quick
/// status + audio/VOX + disconnect menu. The asterisk stays a live TX/RX/
/// connected indicator. On iOS it is a standard windowed app.
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
            // StatusItemController owns the status item + popover. An empty Settings
            // scene satisfies the `App` scene requirement without showing a window.
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
                deviceMonitor: deviceMonitor)
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
#endif
