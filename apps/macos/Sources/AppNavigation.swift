// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import Combine
    import Foundation

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
        /// True while the main window shows Settings instead of the call UI.
        @Published var showsSettings = false
    }
#endif
