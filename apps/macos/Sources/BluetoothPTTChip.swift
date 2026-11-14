// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarStation
    import SwiftUI

    /// The B02PTT's status chip on the dial page and the call card: "B02PTT ·
    /// strong", "B02PTT · reconnecting…", "B02PTT · won't transmit".
    ///
    /// It answers "will the button work right now?" before the operator presses
    /// it, which is why it sits beside the controls and not in Settings. Every
    /// word and tone comes from `BluetoothPTTPresentation`, so the chip, the
    /// settings status and VoiceOver cannot drift apart. The glyph fills with the
    /// accent colour while the button is physically down but the radio is not
    /// on air, so a refused or latched press is visible rather than silent.
    /// Static glyphs only, no symbol effects, so Reduce Motion has nothing to
    /// remove.
    ///
    /// Renders nothing until the controller reports a status (`chipText` is nil
    /// while `.disabled`) or when no device is attached.
    struct BluetoothPTTChip: View {
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var bluetooth: BluetoothPTTController

        var body: some View {
            if let device = bluetooth.device,
                let text = BluetoothPTTPresentation.chipText(
                    status: bluetooth.status, signal: bluetooth.signal,
                    shortName: device.namePrefix, transmitBlocked: session.transmitBlocked)
            {
                Label {
                    Text(text)
                } icon: {
                    Image(systemName: symbolName)
                        .foregroundStyle(pressedOffAir ? Color.accentColor : toneColor)
                }
                .font(.caption)
                .foregroundStyle(toneColor)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 14)
                .padding(.bottom, 8)
                .help(text)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(BluetoothPTTPresentation.chipAccessibilityLabel)
                .accessibilityValue(
                    BluetoothPTTPresentation.chipAccessibilityValue(
                        name: displayName(for: device), status: bluetooth.status,
                        signal: bluetooth.signal, transmitBlocked: session.transmitBlocked))
            }
        }

        /// The button is down but nothing is on air: refused (not answered,
        /// listen-only, receive-only network) or latched after a forced stop.
        private var pressedOffAir: Bool { bluetooth.rawPressed && !session.ptt }

        /// `button.horizontal.top.press` is macOS 14+. On macOS 13, the app's
        /// deployment target, `Image(systemName:)` draws nothing for it, which
        /// would lose the glyph and the accent fill that makes a refused or
        /// latched press visible. `button.programmable.square` exists on
        /// macOS 13 and has a filled form too.
        private var symbolName: String {
            if #available(macOS 14, *) {
                return pressedOffAir
                    ? "button.horizontal.top.press.fill" : "button.horizontal.top.press"
            }
            return pressedOffAir ? "button.programmable.square.fill" : "button.programmable.square"
        }

        private var toneColor: Color {
            switch BluetoothPTTPresentation.chipTone(
                status: bluetooth.status, transmitBlocked: session.transmitBlocked)
            {
            case .ready: return .green
            case .secondary: return .secondary
            case .warning: return .orange
            }
        }

        /// The advertised name ("B02PTT-6522") once one is known, else the
        /// config's preferred unit, else the model's short name.
        private func displayName(for device: BLEButtonDevice) -> String {
            bluetooth.connected?.advertisedName ?? bluetooth.spec?.preferredName
                ?? device.namePrefix
        }
    }
#endif
