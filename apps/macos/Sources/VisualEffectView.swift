// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import SwiftUI
    import AppKit

    /// A SwiftUI wrapper over `NSVisualEffectView` for a translucent, blurred window
    /// background — the frosted look the old popover had, now applied to the main
    /// window. `.behindWindow` blending blurs the desktop behind, so the host window
    /// must be non-opaque with a clear background (see `StatusItemController`).
    struct VisualEffectView: NSViewRepresentable {
        var material: NSVisualEffectView.Material = .popover
        var blendingMode: NSVisualEffectView.BlendingMode = .behindWindow

        func makeNSView(context: Context) -> NSVisualEffectView {
            let view = NSVisualEffectView()
            view.material = material
            view.blendingMode = blendingMode
            view.state = .active
            return view
        }

        func updateNSView(_ view: NSVisualEffectView, context: Context) {
            view.material = material
            view.blendingMode = blendingMode
        }
    }
#endif
