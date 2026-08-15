// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if canImport(AppKit)
    import AppKit

    /// astar-e814: guards the spacebar hold-to-talk monitor in `MenuPopover`
    /// against keying the transmitter while the user is typing into a text
    /// field (DTMF compose, M17 callsign prompt, favorite-label editor,
    /// credentials form, ...). SwiftUI `TextField`s edit through the window's
    /// field editor, which AppKit represents as an `NSTextView` — so treat
    /// any `NSTextView`-based first responder as "the user is typing."
    public enum SpaceKeyGuard {
        /// True when Space should pass through untouched — not key PTT, not
        /// be consumed — because a text view has keyboard focus.
        public static func spaceIsTyping(firstResponder: NSResponder?) -> Bool {
            firstResponder is NSTextView
        }
    }
#endif
