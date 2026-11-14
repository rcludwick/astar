// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The app-wide transmit time-out, in its own collapsible "Transmit"
    /// section of the Settings list, right after the mic profiles. Like every
    /// section except Operator and AllStarLink it starts folded.
    ///
    /// It sits outside every config card on purpose: it is one value for the
    /// whole app, not part of a `Setup`, and `saveCurrentToSelected` never
    /// copies it. It times continuous on-air time for every keying source
    /// together and can never be disabled, so the slider runs 1–10 minutes in
    /// whole-minute steps with no Off position (a stepped Slider draws a tick
    /// per step). Every move goes straight to `CallSession.setTxTimeoutSeconds`,
    /// which normalises the value, persists `audio.txTimeoutSeconds` and pushes
    /// it to the engine. The engine is what enforces it; this view only edits it.
    ///
    /// Not to be confused with VOX's Hang Timeout in Quick settings, which is how
    /// long VOX waits after you stop talking, and is unchanged.
    struct TransmitTimeoutSettingsView: View {
        @EnvironmentObject private var session: CallSession

        var body: some View {
            SettingsSection(.transmit) {
                VStack(alignment: .leading, spacing: 4) {
                    // Label and value on their own line, the slider under them:
                    // "Transmit time-out" is too long to share one row with a
                    // usable slider at the window's 310 pt minimum.
                    HStack(spacing: 8) {
                        Text(TransmitTimeoutPresentation.label)
                        Spacer(minLength: 0)
                        Text(
                            TransmitTimeoutPresentation.valueText(
                                seconds: session.txTimeoutSeconds)
                        )
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                    }
                    // The two Texts are the slider's visible label and value;
                    // the Slider carries both for VoiceOver and stays the one
                    // adjustable element.
                    .accessibilityHidden(true)
                    Slider(
                        value: minutesBinding,
                        in: TransmitTimeoutPresentation.sliderRange,
                        step: 1
                    )
                    .controlSize(.small)
                    .accessibilityLabel(TransmitTimeoutPresentation.label)
                    .accessibilityValue(
                        TransmitTimeoutPresentation.accessibilityValue(
                            seconds: session.txTimeoutSeconds))
                    Text(TransmitTimeoutPresentation.caption)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .font(.callout)
                .listRowSeparator(.hidden)
            }
        }

        /// Slider minutes over the session's stored seconds. The setter goes
        /// through `seconds(fromMinutes:)`, which can never produce 0.
        private var minutesBinding: Binding<Double> {
            Binding(
                get: {
                    Double(
                        TransmitTimeoutPresentation.minutes(
                            fromSeconds: session.txTimeoutSeconds))
                },
                set: {
                    session.setTxTimeoutSeconds(
                        TransmitTimeoutPresentation.seconds(fromMinutes: $0))
                }
            )
        }
    }
#endif
