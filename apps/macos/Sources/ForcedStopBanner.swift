// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarStation
    import SwiftUI

    /// The orange line under the call controls that says why transmitting
    /// stopped when the operator did not stop it: "Transmit stopped after 2
    /// minutes. Release to talk again." or "Transmit stopped: B02PTT
    /// disconnected. Release the button, then press again."
    ///
    /// A transmit time-out latches every source it cut off, and a Bluetooth
    /// link lost while the button was held latches the button (an idle drop
    /// latches nothing: decided 2026-10-09, Rob, conditional latch), so the
    /// next press does nothing until the old one is released. Without this
    /// line that looks
    /// like a broken button. The text, which of the two stops is newer, and when
    /// it hides (8 s after the stop, or once a new key-up starts after it) are
    /// all `BluetoothPTTPresentation.bannerText`. This view re-reads it once a
    /// second so the 8 s window closes on its own. No animation: it appears and
    /// disappears in place, which also respects Reduce Motion.
    struct ForcedStopBanner: View {
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var bluetooth: BluetoothPTTController

        var body: some View {
            if session.lastForcedStop != nil || bluetoothStop != nil {
                TimelineView(.periodic(from: .now, by: 1)) { context in
                    if let text = BluetoothPTTPresentation.bannerText(
                        engine: session.lastForcedStop, bluetooth: bluetoothStop,
                        shortName: shortName, now: context.date,
                        keyedSince: session.keyedSince)
                    {
                        Label(text, systemImage: "exclamationmark.triangle.fill")
                            .font(.caption)
                            .foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 14)
                            .padding(.bottom, 8)
                            // VoiceOver hears this from AccessibilityAnnouncer
                            // the moment it happens; here it is one readable
                            // element for someone exploring the card later.
                            .accessibilityElement(children: .combine)
                    }
                }
            }
        }

        /// A Bluetooth release only counts while a Bluetooth device is attached;
        /// after a switch to a serial config an old release must not resurface.
        private var bluetoothStop: BluetoothForcedRelease? {
            bluetooth.device == nil ? nil : bluetooth.lastForcedStop
        }

        /// Only Bluetooth wording uses the short name, and `bluetoothStop` is nil
        /// whenever there is no device, so the empty fallback is never shown.
        private var shortName: String { bluetooth.device?.namePrefix ?? "" }
    }
#endif
