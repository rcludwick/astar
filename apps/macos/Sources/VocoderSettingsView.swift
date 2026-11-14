// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarStation
    import SwiftUI

    /// Settings → **Vocoders**: the AMBE vocoder sticks this Mac trusts.
    ///
    /// astar opens a vocoder port only when its USB serial number is listed
    /// here. A DVstick and many radio CAT/PTT cables use the same FTDI chip
    /// and look identical over USB, and opening a cable's port raises DTR/RTS
    /// — which can key the transmitter on the other end. So nothing is
    /// trusted until the operator adds it, one stick at a time, through the
    /// sheet below; the list stays on this Mac and never travels in an
    /// exported config.
    ///
    /// Above the list, which vocoder each mode uses. D-Star may use the
    /// software vocoder instead of a stick; System Fusion, NXDN and DMR are
    /// dongle-only, and the pane says why.
    struct VocoderSettingsView: View {
        @EnvironmentObject private var vocoders: VocoderSettings
        @EnvironmentObject private var disclosure: SettingsDisclosure
        @State private var showingAdd = false

        var body: some View {
            SettingsSection(.vocoders) {
                VStack(alignment: .leading, spacing: 8) {
                    VocoderModesView()
                    Divider().padding(.vertical, 2)
                    if vocoders.trusted.isEmpty {
                        emptyState
                    } else {
                        ForEach(vocoders.trusted) { stick in
                            TrustedVocoderRow(
                                stick: stick,
                                connected: vocoders.isConnected(serial: stick.serial),
                                port: vocoders.port(serial: stick.serial),
                                remove: { vocoders.remove(serial: stick.serial) })
                        }
                    }
                    Label(
                        "astar opens only the sticks listed here. They stay on this Mac "
                            + "and are never included in an exported config.",
                        systemImage: "lock.shield"
                    )
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                }
                // The scroll target for "Open Vocoder Settings…". On the row,
                // not the Section: a List scrolls to rows.
                .id(SettingsAnchor.vocoders)
                // Off this view rather than the Section, for the reason
                // `ConfigTransferView` documents: a sheet on a Section is
                // presented once per row.
                .sheet(isPresented: $showingAdd) {
                    AddVocoderSheet(isPresented: $showingAdd)
                        .environmentObject(vocoders)
                }
                .listRowSeparator(.hidden)
                // Listing is enumeration only — no port is opened — so it is
                // safe to keep the connected dots live while Settings is up.
                .task {
                    while !Task.isCancelled {
                        vocoders.refresh()
                        try? await Task.sleep(nanoseconds: 2_000_000_000)
                    }
                }
            } accessory: {
                AddButton(help: "Add a vocoder…") {
                    // The sheet hangs off the section's row, which a folded
                    // section has not mounted: open it, then present on the
                    // next turn once the row is there to present from.
                    if disclosure.isExpanded(.vocoders) {
                        showingAdd = true
                    } else {
                        withAnimation(.easeInOut(duration: 0.18)) {
                            disclosure.setExpanded(.vocoders, true)
                        }
                        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                            showingAdd = true
                        }
                    }
                }
            }
        }

        private var emptyState: some View {
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: "memorychip")
                        .font(.title2)
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 3) {
                        Text("No vocoders added")
                            .font(.callout.weight(.medium))
                        Text(
                            "System Fusion, NXDN and DMR need an AMBE vocoder stick "
                                + "(DVstick 30 or ThumbDV), and so does D-Star unless it uses "
                                + "the software vocoder. Add yours to use them."
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    }
                }
                HStack(spacing: 8) {
                    Button("Add Vocoder…") { showingAdd = true }
                        .controlSize(.small)
                    // No stick yet: D-Star has another way in, one click away.
                    if vocoders.softwareVocoderCompiled, vocoders.dstarVocoder == .dongle {
                        Button("Use Software for D-Star") {
                            vocoders.setDStarVocoder(.software)
                        }
                        .controlSize(.small)
                        .help("D-Star runs its voice on this Mac; no stick needed for it")
                    }
                    if vocoders.suggestsAdding {
                        Label("A possible vocoder is plugged in", systemImage: "cable.connector")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .padding(8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        }
    }

    /// Which vocoder each mode uses. D-Star: a picker, Dongle (the default) or
    /// Software. The others: "Dongle only", with the patent that keeps them so.
    private struct VocoderModesView: View {
        @EnvironmentObject private var vocoders: VocoderSettings

        private var dstarChoice: Binding<DStarVocoder> {
            Binding(
                get: { vocoders.dstarVocoder },
                set: { vocoders.setDStarVocoder($0) })
        }

        var body: some View {
            VStack(alignment: .leading, spacing: 6) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text(VocoderModeRow.dstar.title)
                        .font(.callout)
                    Spacer(minLength: 8)
                    Picker("D-Star vocoder", selection: dstarChoice) {
                        Text("Dongle (default)").tag(DStarVocoder.dongle)
                        Text("Software (Bruce Perens’ implementation)")
                            .tag(DStarVocoder.software)
                            .disabled(!vocoders.softwareVocoderCompiled)
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                    .fixedSize()
                    .help("Which vocoder D-Star uses")
                }
                dstarCaption

                ForEach([VocoderModeRow.ysf, .nxdn, .dmr]) { row in
                    HStack(spacing: 10) {
                        Text(row.title)
                            .font(.callout)
                        Spacer(minLength: 8)
                        Label("Dongle only", systemImage: "lock")
                            .labelStyle(.titleAndIcon)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel("\(row.title): dongle only")
                }
                Text(VocoderModeRow.ambePlus2Reason)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }

        @ViewBuilder
        private var dstarCaption: some View {
            if vocoders.softwareChosenButMissing {
                Label(
                    "Software is chosen, but \(VocoderModeRow.softwareNotInBuild.lowercasedFirst) "
                        + "D-Star won’t connect until you choose the dongle.",
                    systemImage: "exclamationmark.triangle.fill"
                )
                .font(.caption)
                .foregroundStyle(.orange)
                .fixedSize(horizontal: false, vertical: true)
            } else if !vocoders.softwareVocoderCompiled {
                Text(VocoderModeRow.softwareNotInBuild + " D-Star uses a vocoder stick.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } else if vocoders.dstarVocoder == .software {
                Text("D-Star runs its voice on this Mac. No vocoder stick is needed for it.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    extension String {
        /// "This build…" → "this build…", for a sentence spliced mid-line.
        fileprivate var lowercasedFirst: String {
            prefix(1).lowercased() + dropFirst()
        }
    }

    /// One trusted stick: connected dot, model, serial, where it is or when it
    /// was last seen, and Remove.
    private struct TrustedVocoderRow: View {
        let stick: TrustedVocoder
        let connected: Bool
        let port: String?
        let remove: () -> Void

        var body: some View {
            HStack(alignment: .center, spacing: 10) {
                Circle()
                    .fill(connected ? Color.green : Color.secondary.opacity(0.35))
                    .frame(width: 8, height: 8)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text(stick.model ?? "AMBE vocoder")
                        .font(.callout)
                        .lineLimit(1)
                    HStack(spacing: 6) {
                        Text(stick.serial)
                            .font(.caption.monospaced())
                            .textSelection(.enabled)
                        if let prodid = stick.prodid {
                            Text(prodid)
                                .font(.caption2.monospaced())
                                .foregroundStyle(.tertiary)
                        }
                    }
                    Text(whereabouts)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 8)
                Button(role: .destructive, action: remove) {
                    Image(systemName: "trash").foregroundStyle(.red)
                }
                .buttonStyle(.borderless)
                .help("Remove this vocoder. astar will no longer open it.")
                .accessibilityLabel("Remove \(stick.model ?? "vocoder") \(stick.serial)")
            }
            .padding(8)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous)
            )
            .contextMenu {
                Button("Remove", role: .destructive, action: remove)
            }
            .accessibilityElement(children: .contain)
            .accessibilityLabel(
                "\(stick.model ?? "AMBE vocoder"), serial \(stick.serial), "
                    + (connected ? "connected" : "not connected"))
        }

        private var whereabouts: String {
            if connected {
                return port.map { "Connected · \($0)" } ?? "Connected"
            }
            guard let seen = stick.lastSeen else { return "Not plugged in" }
            let f = RelativeDateTimeFormatter()
            f.unitsStyle = .full
            return "Not plugged in · last seen \(f.localizedString(for: seen, relativeTo: Date()))"
        }
    }

    /// "Add Vocoder…": pick an attached device, confirm it is not a radio
    /// cable, then check it and show what it turned out to be.
    ///
    /// Nothing is opened until the operator presses Check and Add — the list
    /// comes from USB enumeration alone. The confirmation resets whenever the
    /// selection changes, so it is always about the device being checked.
    private struct AddVocoderSheet: View {
        @EnvironmentObject private var vocoders: VocoderSettings
        @Binding var isPresented: Bool

        @State private var selection: String?
        @State private var confirmedNotACable = false
        @State private var outcome: VocoderEnrolment?

        private var checking: Bool { vocoders.enrollingSerial != nil }

        var body: some View {
            VStack(alignment: .leading, spacing: 14) {
                Text("Add Vocoder").font(.headline)
                if checking {
                    checkingView
                } else if let outcome {
                    outcomeView(outcome)
                } else {
                    pickView
                }
            }
            .padding(20)
            .frame(width: 400)
            .onAppear {
                vocoders.refresh()
                if vocoders.addableCandidates.count == 1 {
                    selection = vocoders.addableCandidates.first?.serial
                }
            }
        }

        // MARK: Pick and confirm

        @ViewBuilder
        private var pickView: some View {
            Text("Choose the vocoder stick to add.")
                .font(.caption)
                .foregroundStyle(.secondary)

            if vocoders.addableCandidates.isEmpty {
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: "cable.connector.slash")
                        .font(.title3)
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 4) {
                        Text("No new vocoder-class USB devices found")
                            .font(.callout)
                        Text("Plug the stick in, then refresh.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Refresh") { vocoders.refresh() }
                        .controlSize(.small)
                }
                .padding(10)
                .background(
                    Color.secondary.opacity(0.06),
                    in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            } else {
                VStack(spacing: 4) {
                    ForEach(vocoders.addableCandidates, id: \.serial) { candidate in
                        CandidateRow(
                            candidate: candidate, selected: selection == candidate.serial
                        ) {
                            if selection != candidate.serial {
                                selection = candidate.serial
                                confirmedNotACable = false
                            }
                        }
                    }
                }
            }

            catCableWarning

            HStack {
                Spacer()
                Button("Cancel") { isPresented = false }
                    .keyboardShortcut(.cancelAction)
                Button("Check and Add", action: check)
                    .keyboardShortcut(.defaultAction)
                    .disabled(selectedCandidate == nil || !confirmedNotACable)
            }
        }

        private var catCableWarning: some View {
            VStack(alignment: .leading, spacing: 8) {
                Label {
                    Text(
                        "Make sure this is not a radio’s CAT/PTT cable. Vocoder sticks and "
                            + "many radio cables use the same USB chip, and checking a device "
                            + "opens it. On a cable wired to a transmitter, that can key the radio."
                    )
                    .fixedSize(horizontal: false, vertical: true)
                } icon: {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                }
                .font(.caption)

                Toggle(isOn: $confirmedNotACable) {
                    Text("This is a vocoder stick, not a radio cable")
                        .font(.callout)
                }
                .toggleStyle(.checkbox)
                .disabled(selectedCandidate == nil)
            }
            .padding(10)
            .background(
                Color.orange.opacity(0.08),
                in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        }

        private var selectedCandidate: VocoderCandidate? {
            vocoders.addableCandidates.first { $0.serial == selection }
        }

        private func check() {
            guard let candidate = selectedCandidate, confirmedNotACable else { return }
            Task { @MainActor in
                outcome = await vocoders.enrol(candidate)
                if let outcome {
                    AccessibilityAnnouncer.post(
                        VocoderOutcomeSummary(outcome).title, priority: .high)
                }
            }
        }

        // MARK: Checking

        private var checkingView: some View {
            HStack(spacing: 12) {
                ProgressView().controlSize(.small)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Checking \(vocoders.enrollingSerial ?? "")…")
                        .font(.callout)
                    Text("Asking the chip what it is. This takes a couple of seconds.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity, minHeight: 80, alignment: .leading)
            .accessibilityElement(children: .combine)
        }

        // MARK: Outcome

        @ViewBuilder
        private func outcomeView(_ outcome: VocoderEnrolment) -> some View {
            let summary = VocoderOutcomeSummary(outcome)
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: summary.systemImage)
                    .font(.title)
                    .foregroundStyle(summary.isSuccess ? Color.green : Color.orange)
                    .symbolRenderingMode(.hierarchical)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 4) {
                    Text(summary.title).font(.callout.weight(.semibold))
                    Text(summary.detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if outcome.result == .unsupported {
                        Text(
                            "Support for the AMBE-3003 is planned. Until then, use a "
                                + "DVstick 30 or ThumbDV."
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            .accessibilityElement(children: .combine)

            HStack {
                Spacer()
                if !summary.isSuccess {
                    Button("Try Another") {
                        self.outcome = nil
                        selection = nil
                        confirmedNotACable = false
                        vocoders.refresh()
                    }
                }
                Button("Done") { isPresented = false }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    /// One attached, not-yet-trusted device in the add sheet. A whole-row
    /// button so the hit target is the row, with a radio-style mark.
    private struct CandidateRow: View {
        let candidate: VocoderCandidate
        let selected: Bool
        let select: () -> Void

        var body: some View {
            Button(action: select) {
                HStack(spacing: 10) {
                    Image(systemName: selected ? "largecircle.fill.circle" : "circle")
                        .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                        .accessibilityHidden(true)
                    Image(systemName: "memorychip")
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(candidate.serial)
                            .font(.callout.monospaced())
                        Text(candidate.port)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                    Spacer()
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .contentShape(Rectangle())
                .background(
                    selected ? Color.accentColor.opacity(0.12) : Color.secondary.opacity(0.06),
                    in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            }
            .buttonStyle(.plain)
            .accessibilityLabel("USB device \(candidate.serial) at \(candidate.port)")
            .accessibilityAddTraits(selected ? [.isSelected] : [])
        }
    }

    /// A listing that finds nothing and an enroller that never opens anything
    /// — for previews, which must not touch hardware.
    struct NoVocoderEnroller: VocoderEnrolling {
        func candidates() -> [VocoderCandidate] { [] }
        func enrol(serial: String) throws -> VocoderEnrolment {
            VocoderEnrolment(
                result: .notCompiled, serial: serial, port: nil, prodid: nil, model: nil,
                message: "Previews never open a device.")
        }
    }
#endif
