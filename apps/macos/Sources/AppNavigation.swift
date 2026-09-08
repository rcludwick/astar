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
    /// with itself. Every pane's Back returns to `.call`.
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

        /// Back out of whatever pane is up. One level, because there is only one.
        func goBack() { pane = .call }
    }
#endif
