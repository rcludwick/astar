// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import SwiftUI

/// Placeholder root view for the scaffold. Replaced as the real UI lands
/// (connect/dial, meters, node directory, …).
struct ContentView: View {
    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "star")
                .font(.system(size: 44))
                .foregroundStyle(.tint)
            Text("astar")
                .font(.title2.weight(.semibold))
            Text("AllStarLink client")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .padding(32)
        .frame(minWidth: 260, minHeight: 200)
    }
}

#Preview {
    ContentView()
}
