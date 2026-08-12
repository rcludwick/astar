// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarCore
import SwiftUI

/// Advanced audio options in Settings — the bits beyond the simple QuickConfig
/// (devices + volume + mic processing). Today that's the full-duplex switch,
/// which stays out of the simple config (half-duplex is the safe default for
/// speakers; full duplex is a deliberate headphones choice).
struct AudioOptionsView: View {
    @EnvironmentObject private var session: CallSession

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Audio options", systemImage: "slider.horizontal.3")
                .font(.headline)
                .labelStyle(.titleAndIcon)

            VStack(alignment: .leading, spacing: 3) {
                Toggle(
                    isOn: Binding(
                        get: { session.fullDuplex },
                        set: { session.setFullDuplex($0) }
                    )
                ) {
                    Label("Full duplex", systemImage: "headphones")
                }
                .toggleStyle(.switch)
                Text(
                    session.fullDuplex
                        ? "Headphones: transmit and receive at the same time."
                        : "Speaker (half-duplex): VOX won't key while receiving, so the far end can't feed back."
                )
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

#Preview {
    AudioOptionsView()
        .environmentObject(CallSession(station: NullStation()))
        .padding()
}
