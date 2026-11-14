// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The packet capture button, beside Record. It exists only while Debug
    /// mode is on in Settings; otherwise its body is empty.
    ///
    /// Idle it is a grey ladybug. Capturing, the ladybug fills orange and the
    /// word "Capturing" sits beside it, so colour is never the only signal and
    /// orange stays apart from Record's red. It wears `callControlChrome`, so
    /// it stands the same 28 pt as the rest of the row. ⇧⌘R toggles it, and
    /// only while a press would do something: not with no answered call, and
    /// not while a dial is about to replace that call.
    struct PacketCaptureButton: View {
        @EnvironmentObject private var session: CallSession

        var body: some View {
            if session.recordingSettings.debugMode {
                let capturing = session.packetCapturing
                let enabled = session.packetCapturePressable
                Button(action: session.togglePacketCapture) {
                    HStack(spacing: 5) {
                        Image(systemName: capturing ? "ladybug.fill" : "ladybug")
                            .foregroundStyle(capturing ? Color.orange : Color.secondary)
                        if capturing {
                            Text(PacketCaptureCopy.title)
                                .foregroundStyle(.primary)
                                .transition(.opacity)
                        }
                    }
                    .padding(.horizontal, 14)
                    .callControlChrome(
                        fill: capturing ? Color.orange.opacity(0.18) : Color.secondary.opacity(0.18)
                    )
                }
                .buttonStyle(.plain)
                .keyboardShortcut("r", modifiers: [.command, .shift])
                .disabled(!enabled)
                .opacity(enabled ? 1 : 0.4)
                .animation(.easeOut(duration: 0.12), value: capturing)
                .help(PacketCaptureCopy.help(capturing: capturing))
                .accessibilityLabel(PacketCaptureCopy.accessibilityLabel)
                .accessibilityValue(PacketCaptureCopy.accessibilityValue(capturing: capturing))
            }
        }
    }
#endif
