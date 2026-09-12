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
        /// `seedsFromProfile` is that fill-in, and a caller turns it OFF when `nil`
        /// is a real answer rather than an absent one: a saved config whose row says
        /// "System Default" means the system default, and seeding it with the active
        /// profile's named mic would analyze a microphone that config does not use.
        /// Mic Profiles and Quick settings keep the default — neither of them can
        /// say "system default" as a deliberate choice.
        ///
        /// Seeding before the switch matters: the pane is built by
        /// `switch navigation.pane`, so the view's `onAppear` starts the monitor on
        /// whatever `selectedInput` already says.
        ///
        /// `show(_:)` rather than assigning `pane`: the analyzer is reachable from
        /// Settings (Mic Profiles, a saved config) and from Quick settings on the
        /// call card, so Back has to return to whichever one opened it.
        func open(input: String?, seedsFromProfile: Bool = true) {
            let seed = resolveSeed(input: input, seedsFromProfile: seedsFromProfile)
            // Clear on a device change, here rather than in the view: the analyzer
            // is a pane now, so the view is destroyed between visits and its
            // `.onChange(of: vm.selectedInput)` — which used to do this while the
            // old window kept the view alive — never fires across one. Without
            // this, a second visit on a different mic would open showing mic A's
            // peaks and a Save would stamp them onto mic B.
            if seed != vm.selectedInput {
                vm.clear()
            }
            vm.selectedInput = seed
            navigation?.show(.micAnalyzer)
        }

        /// Start a NEW mic profile: drop any unsaved result, name and "Saved"
        /// confirmation, put the keyboard in the name field, and show the pane.
        ///
        /// Distinct from `open(input:)`, which preserves an in-progress
        /// characterization when you come back to the same mic — "+" is a promise
        /// of a blank sheet, so it clears unconditionally.
        func startNew(input: String?, seedsFromProfile: Bool = true) {
            vm.selectedInput = resolveSeed(input: input, seedsFromProfile: seedsFromProfile)
            vm.clear()
            vm.requestNameFocus()
            // `show` already no-ops when the analyzer pane is up (preserving
            // whatever Back target got us here), which is what makes it safe to
            // call this from the "+" inside the pane itself.
            navigation?.show(.micAnalyzer)
        }

        /// Same fill-in `open` and `startNew` both use: the caller's explicit
        /// device, or (when it's allowed to guess) the active profile's mic.
        private func resolveSeed(input: String?, seedsFromProfile: Bool) -> String? {
            seedsFromProfile
                ? MicAnalyzerSeed.input(
                    explicit: input, stored: UserDefaultsAudioSettingsStore().load().input)
                : input
        }
    }
#endif
