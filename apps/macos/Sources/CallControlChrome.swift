// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import SwiftUI

    /// The shared look of the in-call control row: push-to-talk (and the VOX,
    /// listen-only and receive-only blocks that stand in its place), Record and
    /// Disconnect.
    ///
    /// The row's height is set here once, rather than grown from each control's
    /// content. Padding alone let the content decide: the Disconnect button's
    /// `phone.down.fill` symbol is only 8 pt tall, so it came out at 28 pt while a
    /// line of text or the taller `record.circle` came out at 35 pt. Disconnect
    /// was the one that looked right, so 28 pt is its natural height, and every
    /// control now stands at least that tall whatever its label or clock says.
    /// A label that wraps at a narrow window grows rather than spilling out.
    struct CallControlChrome: ViewModifier {
        /// The Disconnect button's natural height: an 8 pt symbol plus 10 pt
        /// above and below.
        static let height: CGFloat = 28
        static let cornerRadius: CGFloat = 8

        var fill: Color

        func body(content: Content) -> some View {
            let shape = RoundedRectangle(cornerRadius: Self.cornerRadius, style: .continuous)
            content
                .font(.callout.weight(.semibold))
                .frame(minHeight: Self.height)
                .background(fill, in: shape)
                .contentShape(shape)
        }
    }

    extension View {
        /// Font, height, fill and hit shape of one in-call control.
        func callControlChrome(fill: Color) -> some View {
            modifier(CallControlChrome(fill: fill))
        }
    }
#endif
