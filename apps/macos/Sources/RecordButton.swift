// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The in-call Record button, between PTT and Disconnect. It wears
    /// `callControlChrome`, like the rest of the row, so the three controls
    /// stand the same height.
    ///
    /// Idle it is a grey `record.circle`. Recording, the circle fills red and
    /// the elapsed time counts beside it (`● 00:42`), so a glance at the call
    /// card says the call is being kept. Live only once the call is answered:
    /// there is nothing to record while dialling. ⌘R toggles it, and only
    /// while the call is answered, because a disabled button's shortcut is
    /// disabled with it.
    struct RecordButton: View {
        @EnvironmentObject private var session: CallSession

        var body: some View {
            let recording = session.recording
            let seconds = session.recordingElapsedSeconds
            let enabled = session.status == .answered
            Button(action: session.toggleRecording) {
                HStack(spacing: 5) {
                    Image(systemName: recording ? "record.circle.fill" : "record.circle")
                        .foregroundStyle(recording ? Color.red : Color.secondary)
                    if recording {
                        Text(RecordingClock.text(seconds: seconds))
                            .monospacedDigit()
                            .foregroundStyle(.primary)
                            .transition(.opacity)
                    }
                }
                .padding(.horizontal, 14)
                .callControlChrome(fill: Color.secondary.opacity(0.18))
            }
            .buttonStyle(.plain)
            .keyboardShortcut("r", modifiers: .command)
            .disabled(!enabled)
            .opacity(enabled ? 1 : 0.4)
            .animation(.easeOut(duration: 0.12), value: recording)
            .help(recording ? "Stop recording (⌘R)" : "Record (⌘R)")
            .accessibilityLabel(
                RecordingClock.accessibilityLabel(recording: recording, seconds: seconds))
        }
    }
#endif
