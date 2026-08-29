// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Who you are on the air — the first section of Settings (astar-c9d2).
    ///
    /// This is one fact about the operator, not one per network, and it belongs
    /// above everything else because every other setting is about equipment.
    /// The callsign used to live at the bottom of the AllStarLink account panel
    /// under an "M17" heading, which said two wrong things at once: that it was
    /// M17's, and that it was part of an AllStarLink account. It is neither —
    /// M17 sends it in every frame, D-Star puts it in every header, YSF carries
    /// it in every data packet, and AllStarLink is the one network that never
    /// transmits it at all, because there you dial as a node number.
    ///
    /// The radio ID sits beside it rather than inside it. DMR addresses radios
    /// numerically, and `docs/design/dmr-networks.md` is explicit that a
    /// registered ID is a *different* credential from a callsign and must not
    /// be bolted onto the callsign field.
    struct StationIdentityView: View {
        @EnvironmentObject private var session: CallSession

        var body: some View {
            Section("Operator") {
                VStack(alignment: .leading, spacing: 4) {
                    SettingsField("Callsign") {
                        // No placeholder: the label carries the name, and an
                        // example callsign in grey is one more string to
                        // mistake for a saved value.
                        TextField("", text: $session.operatorCallsign)
                            .textFieldStyle(.roundedBorder)
                            // Callsigns are transmitted upper case; correcting
                            // as you type beats correcting you afterwards.
                            .onChange(of: session.operatorCallsign) { value in
                                let upper = value.uppercased()
                                if upper != value { session.operatorCallsign = upper }
                            }
                            // The visible label is not wired to the field for
                            // VoiceOver, so it still needs saying out loud.
                            .accessibilityLabel("Your callsign")
                    }
                    Text(
                        "Transmitted by M17, D-Star and YSF. AllStarLink doesn’t use it — "
                            + "there you dial as your node number."
                    )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .settingsCaptionIndent()
                }
                .font(.callout)
                .listRowSeparator(.hidden)

                VStack(alignment: .leading, spacing: 4) {
                    SettingsField("DMR Radio ID") {
                        TextField("", text: $session.dmrRadioID)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityLabel("Your DMR radio ID")
                    }
                    Text(radioIDCaption)
                        .font(.caption)
                        .foregroundStyle(.secondary)
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
        private var radioIDCaption: String {
            if !session.dmrRadioID.isEmpty && !RadioID.isPlausible(session.dmrRadioID) {
                return "A registered ID is at least \(RadioID.minPlausibleDigits) digits."
            }
            return "Your registered ID from radioid.net. DMR addresses radios by number, "
                + "not by callsign."
        }
    }
#endif
