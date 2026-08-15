// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import SwiftUI

    /// Owns the single resizable Mic Analyzer `NSWindow`. The app is `LSUIElement`, so
    /// `open` activates the app and brings the window to the front. Mirrors the
    /// window pattern in `StatusItemController.makeWindow`.
    @MainActor
    final class MicAnalyzerController: ObservableObject {
        private let session: CallSession
        private let vm = MicCharacterization()
        private var window: NSWindow?

        init(session: CallSession) {
            self.session = session
            vm.attach(session: session)
            vm.onClose = { [weak self] in self?.window?.close() }
        }

        /// Show the analyzer, defaulting the mic picker to `input`.
        func open(input: String?) {
            vm.selectedInput = input
            let w = window ?? makeWindow()
            window = w
            NSApp.activate(ignoringOtherApps: true)
            w.makeKeyAndOrderFront(nil)
        }

        private func makeWindow() -> NSWindow {
            let hosting = NSHostingController(
                rootView: MicAnalyzerView(vm: vm).environmentObject(session))
            let w = NSWindow(contentViewController: hosting)
            w.styleMask = [.titled, .closable, .miniaturizable, .resizable]
            w.title = "Mic Analyzer"
            w.isReleasedWhenClosed = false
            w.contentMinSize = NSSize(width: 640, height: 400)
            w.setContentSize(NSSize(width: 640, height: 400))
            // Bumped autosave key so the new default size takes effect once, instead
            // of restoring a stale smaller frame.
            w.setFrameAutosaveName("astarMicAnalyzer.640x400")
            return w
        }
    }
#endif
