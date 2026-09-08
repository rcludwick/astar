// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Owns the Mic Analyzer's model and the way into (and out of) its pane.
    ///
    /// The analyzer is a **pane of the main window**, not a window of its own:
    /// astar has one window and one navigation model, and a second window would be
    /// a second place for "where am I" to live — reachable from Settings, from a
    /// saved config and from Quick settings, each of which would have to find and
    /// front it. It is also what makes the analyzer portable: an iOS navigation
    /// stack can host a pane, and cannot host a second window.
    ///
    /// The model lives here rather than in the pane so the picker selection and any
    /// unsaved profile name survive a trip back to the call card.
    @MainActor
    final class MicAnalyzerController: ObservableObject {
        /// The analyzer's model; `MenuPopover.micAnalyzerPane` hands it to the view.
        let vm = MicCharacterization()
        /// Weak: the app delegate owns both this controller and the navigation.
        private weak var navigation: AppNavigation?

        init(session: CallSession, navigation: AppNavigation) {
            self.navigation = navigation
            vm.attach(session: session)
            // The model's "close" is now "go back one pane". No macOS view calls
            // `requestClose()` — the pane header's Back chevron does that job —
            // but this is the seam the iOS port will drive its navigation stack
            // from, so it stays wired.
            vm.onClose = { [weak navigation] in navigation?.goBack() }
        }

        /// Show the analyzer pane, defaulting the mic picker to `input`.
        ///
        /// `input` is the caller's explicit choice, if it has one — Quick Config and
        /// Setups both know which device they're editing. `MicAnalyzerSeed` fills in
        /// the rest: the device the active profile is using, else the system default.
        /// So the three buttons pass what they know and the seed fills the rest —
        /// Mic Profiles, which passes nothing, opens on the microphone the operator is
        /// actually using instead of the system default.
        ///
        /// Seeding before the switch matters: the pane is built by
        /// `switch navigation.pane`, so the view's `onAppear` starts the monitor on
        /// whatever `selectedInput` already says.
        ///
        /// `show(_:)` rather than assigning `pane`: the analyzer is reachable from
        /// Settings (Mic Profiles, a saved config) and from Quick settings on the
        /// call card, so Back has to return to whichever one opened it.
        func open(input: String?) {
            vm.selectedInput = MicAnalyzerSeed.input(
                explicit: input, stored: UserDefaultsAudioSettingsStore().load().input)
            navigation?.show(.micAnalyzer)
        }
    }
#endif
