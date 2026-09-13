// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import SwiftUI

    /// The "+" that starts something new — a Section header (Saved configs, Mic
    /// Profiles), a config's own mic-profile row, Quick settings' mic-profile
    /// row, and the analyzer pane's own header. It is one affordance living in
    /// four different contexts, and none of those contexts pinned a font: left
    /// to inherit, the glyph came out three different sizes and two different
    /// colours, with a hit target no bigger than the glyph itself. Pinning the
    /// look here — and the 22×22 hit frame `MenuPopover`'s Back chevron already
    /// established as this codebase's "full hit area, no clip" shape — keeps
    /// every one of them the same button wearing a different label.
    struct AddButton: View {
        /// Doubles as the tooltip and the VoiceOver label; callers word it for
        /// what pressing it actually does in their context (the two mic "+"
        /// buttons in Settings are not equivalent, so they say different things).
        let help: String
        let action: () -> Void

        var body: some View {
            Button(action: action) {
                Image(systemName: "plus")
                    .font(.body)
                    .imageScale(.medium)
                    .frame(width: 22, height: 22)  // full hit area, no clip
                    .contentShape(Rectangle())
            }
            .buttonStyle(.borderless)
            .help(help)
            .accessibilityLabel(help)
        }
    }
#endif
