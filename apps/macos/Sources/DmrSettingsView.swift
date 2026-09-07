// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Everything DMR asks of the operator, in one section below the
    /// AllStarLink account (astar-a7c5): the radio ID, one master password per
    /// network, and the BrandMeister consent gate.
    ///
    /// The radio ID started out beside the callsign, on the reasoning that both
    /// identify the operator. They do — but they are not equally load-bearing.
    /// The callsign is transmitted by every digital-voice network astar speaks
    /// and belongs above everything; a DMR ID is one network's credential. It
    /// sits below the account you actually use instead.
    ///
    /// Still a *separate* field from the callsign, and that part has not
    /// changed: DMR addresses radios by a number registered at radioid.net
    /// against a verified licence, which is a different credential with its own
    /// registration story. See `docs/design/dmr-networks.md`.
    struct DmrSettingsView: View {
        @EnvironmentObject private var session: CallSession
        @EnvironmentObject private var reflectors: ReflectorDirectory

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
                        .accessibilityLabel(caption)
                }
                .font(.callout)
                .listRowSeparator(.hidden)

                DmrPasswordView(systems: passwordSystems)
                    .listRowSeparator(.hidden)

                BrandmeisterConsentView()
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
                + "not by callsign. Required to link a DMR talkgroup."
        }

        /// The networks worth offering a password box for: the ones the
        /// directory actually publishes a master for, plus every network a
        /// password is already saved under (so one entered for TGIF — which the
        /// directory does not list at all — never disappears from the UI that
        /// wrote it).
        private var passwordSystems: [DmrPasswordView.SystemChoice] {
            var seen = Set<String>()
            var out: [DmrPasswordView.SystemChoice] = []
            for group in DmrSystemCatalog.grouped(
                reflectors.entries, consented: session.brandmeisterConsent)
            {
                for system in group.systems where seen.insert(system.slug).inserted {
                    out.append(
                        DmrPasswordView.SystemChoice(
                            slug: system.slug,
                            name: DmrSystemCatalog.label(forSlug: system.slug),
                            group: group.title))
                }
            }
            // TGIF is astar's first and recommended target and is in nobody's
            // directory — no row's `system` contains it — so it is offered by
            // name rather than left unreachable.
            if seen.insert(DmrPasswordView.tgifSlug).inserted {
                out.append(
                    DmrPasswordView.SystemChoice(
                        slug: DmrPasswordView.tgifSlug,
                        name: DmrSystemCatalog.label(forSlug: DmrPasswordView.tgifSlug),
                        group: DmrFamily.tgif.displayName))
            }
            return out
        }
    }

    /// One master password per DMR network, in the Keychain and nowhere else.
    ///
    /// A picker plus a `SecureField` rather than a box per network: 111 systems
    /// is not a form. The picker names which network the box below belongs to,
    /// and the saved-state caption says which ones already have one — so the
    /// answer to "did I enter that" does not require re-typing a secret to find
    /// out.
    ///
    /// The password is written straight through to `KeychainCredentialStore`
    /// alongside the AllStarLink account, is never pre-filled (an empty box
    /// means "unchanged", exactly as it does for the portal password), and is
    /// dropped from view state the moment this panel goes away.
    struct DmrPasswordView: View {
        struct SystemChoice: Identifiable, Equatable {
            let slug: String
            /// A readable name for the network, so this picker and the dial
            /// card's master picker speak the same language.
            let name: String
            /// The family heading it sits under, so the picker can group.
            let group: String
            var id: String { slug }
        }

        /// TGIF's directory slug. The network publishes talkgroups to DVRef but
        /// no servers, so it never appears in a directory row — and it is still
        /// the network astar recommends first.
        static let tgifSlug = "tgif"

        let systems: [SystemChoice]

        /// Written straight through, never held. Its OWN Keychain item, not a
        /// field of the AllStarLink account: saving a DMR password must not
        /// bring an empty account into existence, and clearing the account must
        /// not delete these — see `DmrPasswordStore`.
        private let store: DmrPasswordStore = KeychainDmrPasswordStore()

        @State private var selected = DmrPasswordView.tgifSlug
        @State private var password = ""
        @State private var saved: Set<String> = []
        @State private var message: String?
        /// Debounces autosave so the Keychain is written once you stop typing,
        /// not on every keystroke — the same shape the account password uses.
        @State private var saveTask: Task<Void, Never>?

        var body: some View {
            VStack(alignment: .leading, spacing: 4) {
                SettingsField("DMR network") {
                    Picker("", selection: $selected) {
                        ForEach(groupedChoices, id: \.0) { group, choices in
                            Section(group) {
                                ForEach(choices) { choice in
                                    // Name first, slug beneath: the name is
                                    // what the dial card calls this network,
                                    // the slug is what its own paperwork does.
                                    VStack(alignment: .leading, spacing: 0) {
                                        Text(choice.name)
                                        Text(choice.slug)
                                            .font(.caption2)
                                            .foregroundStyle(.secondary)
                                    }
                                    .tag(choice.slug)
                                }
                            }
                        }
                    }
                    .labelsHidden()
                    .accessibilityLabel("Which DMR network this password is for")
                }
                SettingsField("DMR password") {
                    SecureField(
                        saved.contains(selected) ? "Re-enter to change" : "", text: $password
                    )
                    .textFieldStyle(.roundedBorder)
                    .accessibilityLabel("Master password for \(selected)")
                    .accessibilityValue(saved.contains(selected) ? "saved" : "not set")
                }
                Text(caption)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .settingsCaptionIndent()
                    .accessibilityLabel(caption)
                if let message {
                    Text(message)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .settingsCaptionIndent()
                }
                if saved.contains(selected) {
                    Button("Remove password for \(selected)", role: .destructive, action: remove)
                        .buttonStyle(.link)
                        .font(.caption)
                        .settingsCaptionIndent()
                }
            }
            .font(.callout)
            .onAppear(perform: loadExisting)
            .onChange(of: selected) { _ in
                // A secret typed for one network must never be carried over to
                // the next by a picker change.
                saveTask?.cancel()
                password = ""
                message = nil
            }
            .onChange(of: password) { _ in scheduleSave() }
            .onDisappear {
                password = ""
                saveTask?.cancel()
            }
        }

        private var caption: String {
            "Each DMR network issues its own password — TGIF's is on its User Security page, "
                + "BrandMeister's is the Hotspot Security password in SelfCare. Stored in your "
                + "Keychain, never in an exported config."
        }

        /// Picker sections, in the catalog's own order.
        private var groupedChoices: [(String, [SystemChoice])] {
            var order: [String] = []
            var byGroup: [String: [SystemChoice]] = [:]
            for choice in systems {
                if byGroup[choice.group] == nil { order.append(choice.group) }
                byGroup[choice.group, default: []].append(choice)
            }
            return order.map { ($0, byGroup[$0] ?? []) }
        }

        private func loadExisting() {
            // `systems()` answers which networks have one WITHOUT reading a
            // secret back — the saved/not-set caption never needs the value.
            saved = store.systems()
            if let first = systems.first?.slug, !systems.contains(where: { $0.slug == selected }) {
                selected = first
            }
        }

        private func scheduleSave() {
            message = nil
            saveTask?.cancel()
            guard !password.isEmpty else { return }
            saveTask = Task { @MainActor in
                try? await Task.sleep(nanoseconds: 700_000_000)
                guard !Task.isCancelled else { return }
                persist()
            }
        }

        private func persist() {
            let system = selected
            let secret = password
            guard !secret.isEmpty else { return }
            do {
                try store.save(secret, system: system)
                saved.insert(system)
                message = "Saved ✓"
                AccessibilityAnnouncer.post("DMR password saved for \(system)", priority: .medium)
            } catch {
                message = "Couldn’t save to the Keychain."
                AccessibilityAnnouncer.post("Couldn’t save to the Keychain.", priority: .medium)
            }
        }

        /// Same error handling as `persist`: a Keychain write that failed has
        /// to say so, or the password an operator thinks they deleted is still
        /// there.
        private func remove() {
            let system = selected
            do {
                try store.remove(system: system)
                saved.remove(system)
                password = ""
                message = "Removed."
                AccessibilityAnnouncer.post(
                    "DMR password removed for \(system)", priority: .medium)
            } catch {
                message = "Couldn’t remove it from the Keychain."
                AccessibilityAnnouncer.post(
                    "Couldn’t remove it from the Keychain.", priority: .medium)
            }
        }
    }

    /// The BrandMeister consent gate: one checkbox, off by default, never
    /// pre-ticked.
    ///
    /// The wording is verbatim from `docs/design/dmr-networks.md` §"What the
    /// gate looks like", plus the one thing this build has to add: astar
    /// cannot reach BrandMeister yet even with the box ticked. The engine
    /// refuses it unconditionally today, and a control that implied otherwise
    /// would be a promise the next connect breaks.
    ///
    /// No dark patterns in either direction — it is not pre-ticked, and it is
    /// not buried from someone who has read the terms and accepted them.
    struct BrandmeisterConsentView: View {
        @EnvironmentObject private var session: CallSession

        /// BrandMeister's own material, so the operator reads the terms from
        /// the people who enforce them rather than astar's summary of them.
        private static let policyURL = URL(string: "https://wiki.brandmeister.network/")!

        var body: some View {
            VStack(alignment: .leading, spacing: 4) {
                Toggle(isOn: $session.brandmeisterConsent) {
                    Text("Show BrandMeister networks")
                }
                .toggleStyle(.checkbox)
                .accessibilityLabel("Show BrandMeister networks")
                .accessibilityHint(
                    "BrandMeister enforces its own access rules. astar cannot tell you whether "
                        + "connecting this way is within them.")

                VStack(alignment: .leading, spacing: 3) {
                    Text("BrandMeister enforces its own access rules.")
                        .font(.caption.weight(.semibold))
                    // Three runs, one wrapped paragraph: the design doc bolds
                    // the sentence that says who carries the risk, and losing
                    // that emphasis is losing the point of the paragraph.
                    (Text(Self.termsBeforeEmphasis)
                        + Text(Self.termsEmphasis).bold()
                        + Text(Self.termsAfterEmphasis))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityLabel(Self.terms)
                    Link("Read BrandMeister’s own policy", destination: Self.policyURL)
                        .font(.caption)
                    Text(Self.notYet)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .settingsCaptionIndent()
                .accessibilityElement(children: .combine)
            }
            .font(.callout)
        }

        // Verbatim from `docs/design/dmr-networks.md`, split at the one
        // sentence that doc renders bold.
        private static let termsBeforeEmphasis =
            "It is a private network. Its operators set the terms, decide what counts as a "
            + "violation, and have permanently blocked accounts — for conduct and for technical "
            + "reasons. astar is a third-party client and cannot tell you whether connecting this "
            + "way is within their rules. "
        private static let termsEmphasis =
            "If your access is revoked, that is between you and BrandMeister."
        private static let termsAfterEmphasis = " Read their policy before you tick this."

        /// The same paragraph as one string, for VoiceOver — which reads the
        /// run structure as three fragments otherwise.
        private static let terms = termsBeforeEmphasis + termsEmphasis + termsAfterEmphasis

        /// What Task 2 recorded, said plainly rather than implied.
        private static let notYet =
            "astar can’t reach BrandMeister yet even with this ticked — the engine refuses it. "
            + "astar looked for BrandMeister’s position on third-party clients on 2026-09-07 and "
            + "could not read their wiki, so this box grants nothing but visibility."
    }

    #Preview {
        Form {
            DmrSettingsView()
        }
        .environmentObject(CallSession(station: NullStation()))
        .environmentObject(ReflectorDirectory(storage: FileReflectorDirectoryStorage()))
        .frame(width: 340)
        .padding()
    }
#endif
