// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The app-global "Spectrum decay" control (astar-68a6), shown in the full
    /// Settings pane (a small "Spectrum" section in `MenuPopover.devicesPane`).
    ///
    /// A single dB/second value (default 100, range 10…400) that controls how fast
    /// the spectrum peaks fall. It drives BOTH the engine peak-hold decay (the live
    /// mic / TX / RX analyzers, via `session.setSpectrumDecay`) AND astar's
    /// inactive-trace fade in `CallSpectrum` — read from the same `@AppStorage`
    /// key — so active decay and inactive fade stay in lockstep. Global, not
    /// per-Setup / per-mic-profile. Applying it live on every change re-asserts the
    /// value onto whatever analyzers are currently up; the value persists across
    /// relaunch and is re-applied at launch (see `AppDelegate`).
    struct SpectrumSettingsView: View {
        @EnvironmentObject private var session: CallSession

        @AppStorage(SpectrumDecayPref.key) private var decay = SpectrumDecayPref.defaultValue

        var body: some View {
            Section("Spectrum") {
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 8) {
                        Text("Decay")
                            .frame(width: 56, alignment: .leading)
                        Slider(
                            value: $decay,
                            in: SpectrumDecayPref.range,
                            step: SpectrumDecayPref.step
                        ) { editing in
                            // Push to the engine on every move (cheap + idempotent) so
                            // the live spectra update without a rebuild; CallSpectrum's
                            // fade reads the same @AppStorage value on its next tick.
                            _ = editing
                            session.setSpectrumDecay(Float(decay))
                        }
                        .controlSize(.small)
                        // astar-a9c3 F3.
                        .accessibilityLabel("Decay")
                        .accessibilityValue("\(Int(decay)) decibels per second")
                        Text("\(Int(decay)) dB/s")
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                            .frame(width: 56, alignment: .trailing)
                    }
                    Text("How fast the live spectrum peaks fall. Higher = snappier.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .font(.callout)
                .listRowSeparator(.hidden)
            }
        }
    }
#endif
