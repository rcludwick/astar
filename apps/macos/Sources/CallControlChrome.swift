// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
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

    /// The speaker monitor's mute button, between the packet capture button
    /// and Disconnect. It exists only while a monitor device is set (Quick
    /// settings → Speaker → Monitor); otherwise its body is empty.
    ///
    /// On, it is `speaker.wave.2`; muted, a greyed `speaker.slash`. The glyph
    /// carries the state, so colour is never the only signal, and orange
    /// stays the packet capture button's.
    /// Mute is live-only: the monitor stream stays open, so unmuting is
    /// instant, and nothing is saved.
    ///
    /// It does not wait for the call to be answered: the monitor plays
    /// whatever reaches the output, so it can be silenced while dialling too.
    struct MonitorMuteButton: View {
        @EnvironmentObject private var session: CallSession
        /// The saved monitor device (`AudioSettings.monitor`), read straight
        /// from its key so the button appears and vanishes with the picker.
        @AppStorage("audio.monitor") private var monitorDevice: String?

        var body: some View {
            if monitorDevice != nil {
                let muted = session.monitorMuted
                Button {
                    try? session.setMonitorMuted(!muted)
                } label: {
                    Image(systemName: muted ? "speaker.slash" : "speaker.wave.2")
                        .foregroundStyle(muted ? Color.secondary : Color.primary)
                        // Both glyphs in one fixed box, so the button keeps
                        // its width when the waves disappear.
                        .frame(width: 18)
                        .padding(.horizontal, 12)
                        .callControlChrome(fill: Color.secondary.opacity(0.18))
                }
                .buttonStyle(.plain)
                .animation(.easeOut(duration: 0.12), value: muted)
                .help(muted ? "Unmute the monitor speaker" : "Mute the monitor speaker")
                .accessibilityLabel("Monitor speaker")
                .accessibilityValue(muted ? "Muted" : "On")
            }
        }
    }

    /// A small grey "Muted" capsule beside a Monitor picker while the
    /// monitor is muted, so a mute left on from the last call is visible in
    /// settings and not only in the call controls.
    struct MonitorMutedTag: View {
        var body: some View {
            Text("Muted")
                .font(.caption2.weight(.medium))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 5)
                .padding(.vertical, 1)
                .background(Color.secondary.opacity(0.15), in: Capsule())
                .help("The monitor speaker is muted. Unmute it from the call controls.")
                .accessibilityLabel("Monitor muted")
        }
    }
#endif
