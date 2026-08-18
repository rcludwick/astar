// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore

    /// Builds astar's main menu bar (astar-1f7d).
    ///
    /// astar started as a menu-bar-only accessory, where `LSUIElement` meant no app
    /// menu existed and nothing needed one. `DockPolicy` (astar-7c31) promotes the
    /// app to `.regular`, which gives it a real menu bar — and the first build to
    /// ship that, 0.1.1beta, shipped it wired to SwiftUI's `Settings { EmptyView() }`
    /// placeholder scene. A user who reasonably went astar → Settings… got a window
    /// with nothing in it, and reported the app as having no settings at all.
    ///
    /// So this menu is built explicitly rather than left to the default: `Settings…`
    /// opens the real settings pane in the main window, and Help carries the two
    /// links people actually need — the docs site and where to file a bug.
    @MainActor
    enum MainMenu {

        /// Install the menu bar. Call once from `applicationDidFinishLaunching`,
        /// which replaces whatever SwiftUI installed for its placeholder scene.
        static func install(target: MainMenuActions) {
            let main = NSMenu()
            main.addItem(appMenuItem(target: target))
            main.addItem(editMenuItem())
            main.addItem(windowMenuItem())
            main.addItem(helpMenuItem(target: target))
            NSApp.mainMenu = main
        }

        // MARK: - astar

        private static func appMenuItem(target: MainMenuActions) -> NSMenuItem {
            let item = NSMenuItem()
            let menu = NSMenu(title: "astar")

            menu.addItem(
                withTitle: "About astar", action: #selector(MainMenuActions.showAbout(_:)),
                keyEquivalent: ""
            ).target = target
            menu.addItem(.separator())

            // ⌘, — the shortcut every Mac user reaches for. Points at the one real
            // settings pane, not a second window.
            menu.addItem(
                withTitle: "Settings…", action: #selector(MainMenuActions.showSettings(_:)),
                keyEquivalent: ","
            ).target = target
            menu.addItem(.separator())

            // Standard AppKit responder actions: no target, so they route through
            // the responder chain to NSApplication.
            menu.addItem(
                withTitle: "Hide astar", action: #selector(NSApplication.hide(_:)),
                keyEquivalent: "h")
            let hideOthers = menu.addItem(
                withTitle: "Hide Others",
                action: #selector(NSApplication.hideOtherApplications(_:)),
                keyEquivalent: "h")
            hideOthers.keyEquivalentModifierMask = [.command, .option]
            menu.addItem(
                withTitle: "Show All", action: #selector(NSApplication.unhideAllApplications(_:)),
                keyEquivalent: "")
            menu.addItem(.separator())

            menu.addItem(
                withTitle: "Quit astar", action: #selector(NSApplication.terminate(_:)),
                keyEquivalent: "q")

            item.submenu = menu
            return item
        }

        // MARK: - Edit

        /// The standard Edit menu.
        ///
        /// Not optional decoration: `install` replaces `NSApp.mainMenu` outright, so
        /// without this the app would have no ⌘X/⌘C/⌘V/⌘A at all — and every text
        /// field astar has (AllStarLink username and password, node number, config
        /// names, M17 callsign) sits behind those shortcuts. Pasting a password is
        /// the single most likely first thing a new user does.
        ///
        /// Every action here is a first-responder selector with no target, so AppKit
        /// routes it to whichever field is focused and greys the item out when none
        /// is.
        private static func editMenuItem() -> NSMenuItem {
            let item = NSMenuItem()
            let menu = NSMenu(title: "Edit")
            menu.addItem(
                withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
            let redo = menu.addItem(
                withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "z")
            redo.keyEquivalentModifierMask = [.command, .shift]
            menu.addItem(.separator())
            menu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
            menu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
            menu.addItem(
                withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
            menu.addItem(
                withTitle: "Delete", action: #selector(NSText.delete(_:)), keyEquivalent: "")
            menu.addItem(
                withTitle: "Select All", action: #selector(NSText.selectAll(_:)),
                keyEquivalent: "a")
            item.submenu = menu
            return item
        }

        // MARK: - Window

        /// The standard Window menu. astar's main window is closable and
        /// miniaturizable, so leaving this out would strip ⌘M and ⌘W from an
        /// otherwise ordinary window.
        private static func windowMenuItem() -> NSMenuItem {
            let item = NSMenuItem()
            let menu = NSMenu(title: "Window")
            menu.addItem(
                withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)),
                keyEquivalent: "m")
            menu.addItem(
                withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
            menu.addItem(.separator())
            menu.addItem(
                withTitle: "Close", action: #selector(NSWindow.performClose(_:)),
                keyEquivalent: "w")
            item.submenu = menu
            NSApp.windowsMenu = menu
            return item
        }

        // MARK: - Help

        private static func helpMenuItem(target: MainMenuActions) -> NSMenuItem {
            let item = NSMenuItem()
            let menu = NSMenu(title: "Help")
            menu.addItem(
                withTitle: "astar Documentation", action: #selector(MainMenuActions.openHome(_:)),
                keyEquivalent: ""
            ).target = target
            menu.addItem(
                withTitle: "Report an Issue…", action: #selector(MainMenuActions.openIssues(_:)),
                keyEquivalent: ""
            ).target = target
            menu.addItem(.separator())
            menu.addItem(
                withTitle: "\(AboutLinks.callsign) on QRZ",
                action: #selector(MainMenuActions.openQRZ(_:)), keyEquivalent: ""
            ).target = target
            item.submenu = menu
            NSApp.helpMenu = menu
            return item
        }
    }

    /// What the main menu can ask the app to do. A protocol so `MainMenu` builds
    /// against the actions rather than against `AppDelegate` itself.
    @MainActor
    @objc protocol MainMenuActions: AnyObject {
        func showAbout(_ sender: Any?)
        func showSettings(_ sender: Any?)
        func openHome(_ sender: Any?)
        func openIssues(_ sender: Any?)
        func openQRZ(_ sender: Any?)
    }
#endif
