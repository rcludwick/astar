// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import Combine
    import Foundation

    /// The panes the main window can show (astar-1f7d, astar-5a41).
    ///
    /// Not a navigation stack: astar's window is one level deep everywhere, so
    /// a flat enum says exactly what a stack would and cannot get out of step
    /// with itself. Back returns to `.call` unless the pane was entered through
    /// `show(_:)`, which remembers where it came from.
    enum AppPane: Equatable {
        /// The dial card, status and meters — astar's actual job.
        case call
        /// Devices, account, favorites, directory.
        case settings
        /// Browse and search the cached reflector directory.
        case reflectors
        /// Live mic spectrum + the stay-silent characterization that saves a
        /// mic profile.
        case micAnalyzer
    }

    /// Which pane the main window is showing (astar-1f7d).
    ///
    /// `MenuPopover` used to own this as private `@State`, which was fine while the
    /// only way into Settings was its own footer button. The main menu's
    /// `Settings…` item (⌘,) has to reach the same pane from outside the view
    /// tree, so the flag moved out here and the popover observes it.
    ///
    /// There is exactly ONE settings surface in astar, and this is what keeps it
    /// that way: ⌘, and the footer button drive the same pane rather than the menu
    /// opening a second, competing settings window.
    @MainActor
    final class AppNavigation: ObservableObject {
        /// The pane on screen. Everything else here is a view onto this.
        @Published var pane: AppPane = .call

        /// True while the main window shows Settings instead of the call UI.
        ///
        /// Derived rather than stored — the menu bar and the footer button both
        /// speak in these terms, and a second stored flag is a second thing that
        /// can disagree with `pane`.
        var showsSettings: Bool {
            get { pane == .settings }
            set { pane = newValue ? .settings : .call }
        }

        /// One remembered "Back goes here instead of `.call`": the pane it applies
        /// to (`onPane`) and where Back should land from it (`returnsTo`).
        ///
        /// One slot, not a stack: astar's window is still one level deep. What
        /// changed is that a pane can be reached from *two* places (the mic
        /// analyzer opens from Settings and from Quick settings on the call card),
        /// and a Back that always went to `.call` threw the Settings context away.
        ///
        /// `onPane` is what keeps a stale target from firing: if something else
        /// moved the window on (⌘, while the analyzer is up, say), the recorded
        /// pane no longer matches and Back falls back to `.call`.
        private var returnTarget: (onPane: AppPane, returnsTo: AppPane)?

        /// Show `pane`, remembering the pane on screen as where Back returns to.
        ///
        /// Only for panes with more than one way in. Entries that are always
        /// reached from the call card (`showsSettings`, the reflector directory)
        /// set `pane` directly and record nothing, so their Back still lands on
        /// `.call`.
        func show(_ pane: AppPane) {
            guard self.pane != pane else { return }
            returnTarget = (onPane: pane, returnsTo: self.pane)
            self.pane = pane
        }

        /// Back out of whatever pane is up — to whoever opened it, or the call
        /// card. One level either way, because there is only one.
        func goBack() {
            if let target = returnTarget, target.onPane == pane {
                pane = target.returnsTo
            } else {
                pane = .call
            }
            returnTarget = nil
        }
    }
#endif
