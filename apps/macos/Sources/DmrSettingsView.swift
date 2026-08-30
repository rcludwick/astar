// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// The operator's DMR radio ID, in its own section below the AllStarLink
    /// account (astar-a7c5).
    ///
    /// It started out beside the callsign, on the reasoning that both identify
    /// the operator. They do — but they are not equally load-bearing. The
    /// callsign is transmitted by every digital-voice network astar speaks and
    /// belongs above everything; a DMR ID is one network's credential, and that
    /// network is not dialable yet. Putting it at eye level implied a
    /// capability astar does not have. It sits below the account you actually
    /// use instead.
    ///
    /// Still a *separate* field from the callsign, and that part has not
    /// changed: DMR addresses radios by a number registered at radioid.net
    /// against a verified licence, which is a different credential with its own
    /// registration story. See `docs/design/dmr-networks.md`.
    struct DmrSettingsView: View {
        @EnvironmentObject private var session: CallSession

        var body: some View {
            Section("DMR") {
                VStack(alignment: .leading, spacing: 4) {
                    SettingsField("DMR Radio ID") {
                        TextField("", text: $session.dmrRadioID)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityLabel("Your DMR radio ID")
                    }
                    Text(caption)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .settingsCaptionIndent()
                }
                .font(.callout)
                .listRowSeparator(.hidden)
            }
        }

        /// Says what the field is for, and — only once there is something to be
        /// wrong about — that what is in it is too short to be a registration.
        /// A hint, never a refusal: radioid.net is the authority on which
        /// numbers exist, not astar.
        private var caption: String {
            if !session.dmrRadioID.isEmpty && !RadioID.isPlausible(session.dmrRadioID) {
                return "A registered ID is at least \(RadioID.minPlausibleDigits) digits."
            }
            return "Your registered ID from radioid.net. DMR addresses radios by number, "
                + "not by callsign. Saved for when astar can dial DMR — it can’t yet."
        }
    }
#endif
