// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The operator's NXDN id, in its own section beneath the DMR one
    /// (iax-b9c2).
    ///
    /// A third identity field, and deliberately not a second use of the DMR
    /// one. NXDN addresses stations by a **16-bit** number — `NXDNGateway`'s
    /// `NXDNNetwork.cpp` packs the source and destination as `unsigned short`
    /// — while a registered DMR ID is six or seven digits and does not fit.
    /// Reusing it would mean truncating, and a truncated ID is somebody
    /// else's number on the air.
    ///
    /// Unlike the DMR field this one is load-bearing today: astar can link
    /// NXDN, and a dial without an id here is refused before it reaches the
    /// dongle (`CallSession.ConnectError.missingRadioID`).
    struct NxdnSettingsView: View {
        @EnvironmentObject private var session: CallSession

        var body: some View {
            Section("NXDN") {
                VStack(alignment: .leading, spacing: 4) {
                    SettingsField("NXDN ID") {
                        TextField("", text: $session.nxdnRadioID)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityLabel("Your NXDN ID")
                    }
                    Text(caption)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .settingsCaptionIndent()
                        .accessibilityLabel(caption)
                }
                .font(.callout)
                .listRowSeparator(.hidden)
            }
        }

        /// Says what the field is for, and — only once there is something to
        /// be wrong about — that what is in it is not a number NXDN can
        /// carry. A refusal rather than a hint, unlike the DMR caption: this
        /// network is dialable, so an unusable id is a dial that will not
        /// happen rather than a note for later.
        private var caption: String {
            if !session.nxdnRadioID.isEmpty, NxdnID.value(session.nxdnRadioID) == nil {
                return "An NXDN ID is a number from \(NxdnID.minimum) to \(NxdnID.maximum)."
            }
            return "Your NXDN station number — not your DMR radio ID. NXDN addresses "
                + "stations by a 16-bit number, which a DMR ID is too long to fit. "
                + "Required to link an NXDN talkgroup."
        }
    }
#endif
