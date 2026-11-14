// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import AstarStation
    import SwiftUI

    // Nothing in this file runs on appear. Bluetooth starts only from a button
    // (Set up, then Search), or from `SetupController.setActivePTTHardware`
    // when Rob picks the B02PTT, so an imported config touches no Bluetooth
    // until he asks. Search is not even shown before Set up is clicked.

    extension BluetoothPTTTone {
        /// The serial self-test's idiom: green when ready, secondary while
        /// waiting, orange for a problem.
        fileprivate var color: Color {
            switch self {
            case .ready: return .green
            case .secondary: return .secondary
            case .warning: return .orange
            }
        }

        fileprivate var glyph: String {
            switch self {
            case .ready: return "checkmark.circle.fill"
            case .secondary: return "ellipsis.circle"
            case .warning: return "exclamationmark.triangle.fill"
            }
        }
    }

    /// The compact B02PTT block under the PTT row in Quick settings, with
    /// the B02PTT chosen: one status line, the set-up button for an
    /// unconfirmed spec, the confirmation, a System Settings button for a
    /// problem Rob can fix there, and the audio helper.
    struct BluetoothPTTQuickStatus: View {
        let inputs: [String]
        let outputs: [String]
        /// Devices are locked during a call; the helper is then disabled too.
        let audioLocked: Bool
        let onUseAudio: (_ input: String, _ output: String) -> Void

        @EnvironmentObject private var bluetooth: BluetoothPTTController

        var body: some View {
            if let device = bluetooth.device {
                let shortName = device.namePrefix
                let line = BluetoothPTTPresentation.quickStatus(
                    status: bluetooth.status, signal: bluetooth.signal, shortName: shortName)
                VStack(alignment: .leading, spacing: 6) {
                    if BluetoothPTTSettingsText.showsSetUpButton(
                        needsSetup: bluetooth.needsSetup,
                        awaitingConfirmation: bluetooth.awaitingConfirmation)
                    {
                        Button(BluetoothPTTSettingsText.setUpButton(shortName: shortName)) {
                            bluetooth.beginSetup()
                        }
                        .controlSize(.small)
                    } else {
                        BluetoothPTTStatusLine(
                            text: line.text, tone: line.tone, busy: isBusy(bluetooth))
                    }
                    if bluetooth.justConfirmed {
                        ConfirmedOnThisMacLabel()
                    }
                    if let detail = BluetoothPTTSettingsText.rejectedDetail(
                        status: bluetooth.status, rejectedName: bluetooth.rejectedName)
                    {
                        Text(detail)
                            .font(.caption2)
                            .foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    BluetoothPTTSettingsLinkButton(status: bluetooth.status)
                    BluetoothPTTAudioHelperRow(
                        kind: device.kind,
                        name: BluetoothPTTSettingsText.displayName(
                            connected: bluetooth.connected, spec: bluetooth.spec,
                            shortName: shortName),
                        inputs: inputs, outputs: outputs, locked: audioLocked,
                        onUseAudio: onUseAudio)
                }
            }
        }
    }

    /// The full B02PTT section of a config card. On the active config it
    /// drives the live controller, and shows and forgets only the units that
    /// fit the spec's preferred name (as the controller links them). On any
    /// other config it reads only this
    /// Mac's stored confirmations: it lists them as "(not connected)", offers
    /// set-up (which applies the config first), the audio helper (saved to
    /// the config, no live apply) and an Advanced section whose Forget clears
    /// a remembered unit without switching the live config.
    struct BluetoothPTTConfigSection: View {
        let setup: Setup
        let isActive: Bool
        let inputs: [String]
        let outputs: [String]
        /// Why the last Set up click on this card started nothing, for
        /// example an apply refused because the config's HFP mic is absent.
        /// Shown under the button where Rob clicked.
        let setUpRefusal: String?
        let onUseAudio: (_ input: String, _ output: String) -> Void
        /// Applies this config (if needed) and starts setup. User action only.
        let onSetUp: () -> Void

        @EnvironmentObject private var bluetooth: BluetoothPTTController
        @Environment(\.accessibilityReduceMotion) private var reduceMotion
        /// Set whenever the controller's `isSearching` changes while this card
        /// is on screen, so "No B02PTT found" shows only after a search ended.
        /// Driven by the controller rather than the Search button because the
        /// first scan comes from the controller's setup call, which Set up (here or in Quick
        /// settings) and a picker choice reach without touching Search.
        @State private var hasSearched = false
        /// Bumped by an inactive card's Forget so the body re-reads the store,
        /// which SwiftUI does not observe.
        @State private var storeRevision = 0

        private let localStore = UserDefaultsBluetoothPTTLocalStore()

        /// This config's own device, from its spec (not the controller's,
        /// which follows the active config).
        private var device: BLEButtonDevice? {
            setup.bluetoothPTT.flatMap { BLEButtonDevices.device(id: $0.deviceID) }
        }

        var body: some View {
            if let device {
                VStack(alignment: .leading, spacing: 8) {
                    if isActive {
                        activeBody(device)
                    } else {
                        inactiveBody(device)
                    }
                }
                .font(.caption)
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    Color.secondary.opacity(0.06),
                    in: RoundedRectangle(cornerRadius: 6, style: .continuous)
                )
                // A change handler, not an appear handler: it only records
                // that a search started or ended, and starts nothing. Either
                // edge counts, so a search already running when the card
                // appeared still earns the footnote when it ends empty.
                .onChange(of: bluetooth.isSearching) { _ in
                    hasSearched = true
                }
            }
        }

        // MARK: - Another config

        @ViewBuilder
        private func inactiveBody(_ device: BLEButtonDevice) -> some View {
            // Reading `storeRevision` makes the body depend on it, so a Forget
            // below redraws the list from the store.
            let _ = storeRevision
            // Only the units this config can use: with a preferred name, the
            // ones confirmed under it, as the controller would link them.
            let confirmations = BluetoothPTTSettingsText.matchingConfirmations(
                localStore.confirmations(deviceID: device.id),
                preferredName: setup.bluetoothPTT?.preferredName)
            let rows = BluetoothPTTSettingsText.deviceRows(
                discovered: [], connected: nil, confirmations: confirmations,
                displayName: device.displayName)
            if rows.isEmpty {
                setUpButton(shortName: device.namePrefix) { onSetUp() }
            } else {
                deviceList(rows)
            }
            BluetoothPTTAudioHelperRow(
                kind: device.kind,
                name: BluetoothPTTSettingsText.storedDisplayName(
                    spec: setup.bluetoothPTT, confirmations: confirmations,
                    shortName: device.namePrefix),
                inputs: inputs, outputs: outputs, locked: false, onUseAudio: onUseAudio)
            advanced(
                device,
                peripheral: BluetoothPTTSettingsText.peripheralText(
                    connected: nil, confirmations: confirmations,
                    preferredName: setup.bluetoothPTT?.preferredName),
                lastRSSI: nil,
                canForget: BluetoothPTTSettingsText.canForgetFromInactive(
                    hasConfirmations: !confirmations.isEmpty,
                    liveDeviceID: bluetooth.device?.id, deviceID: device.id)
            ) {
                // One unit, the one shown as Peripheral above, as the active
                // card's forgetConfirmedButton() does. Other rows stay.
                if let target = BluetoothPTTSettingsText.inactiveForgetTarget(
                    confirmations: confirmations,
                    preferredName: setup.bluetoothPTT?.preferredName)
                {
                    localStore.forget(platformID: target, deviceID: device.id)
                }
                storeRevision += 1
            }
        }

        /// "Set up the B02PTT on this Mac", with the reason underneath when
        /// the last click on this card started nothing.
        private func setUpButton(shortName: String, action: @escaping () -> Void) -> some View {
            VStack(alignment: .leading, spacing: 3) {
                Button(BluetoothPTTSettingsText.setUpButton(shortName: shortName), action: action)
                    .controlSize(.small)
                if let setUpRefusal {
                    Label(setUpRefusal, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption2)
                        .foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }

        // MARK: - The active config

        @ViewBuilder
        private func activeBody(_ device: BLEButtonDevice) -> some View {
            let shortName = device.namePrefix
            let status = BluetoothPTTPresentation.settingsStatus(
                status: bluetooth.status, signal: bluetooth.signal)
            // Only the units the controller would link for this spec, as on
            // the inactive card: with a preferred name, the ones confirmed
            // under it. Peripheral, Forget and the rows then agree with
            // candidateIDs() and forgetConfirmedButton().
            let confirmations = BluetoothPTTSettingsText.matchingConfirmations(
                bluetooth.confirmedButtons, preferredName: bluetooth.spec?.preferredName)
            let rows = BluetoothPTTSettingsText.deviceRows(
                discovered: bluetooth.discovered, connected: bluetooth.connected,
                confirmations: confirmations, displayName: device.displayName)

            if !BluetoothPTTSettingsText.showsSearch(
                needsSetup: bluetooth.needsSetup,
                awaitingConfirmation: bluetooth.awaitingConfirmation)
            {
                // Before Set up is clicked the card offers that one button:
                // no Search, no list, no status, so nothing here can reach
                // Bluetooth ahead of it.
                setUpButton(shortName: shortName) { bluetooth.beginSetup() }
            } else {
                activeDetails(
                    device, rows: rows, confirmations: confirmations, status: status)
            }
        }

        @ViewBuilder
        private func activeDetails(
            _ device: BLEButtonDevice, rows: [BluetoothPTTDeviceRow],
            confirmations: [BLEConfirmation],
            status: (text: String, tone: BluetoothPTTTone)
        ) -> some View {
            let shortName = device.namePrefix
            BluetoothPTTStatusLine(text: status.text, tone: status.tone, busy: isBusy(bluetooth))
            if let detail = BluetoothPTTSettingsText.rejectedDetail(
                status: bluetooth.status, rejectedName: bluetooth.rejectedName)
            {
                Text(detail)
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            BluetoothPTTSettingsLinkButton(status: bluetooth.status)
            if bluetooth.justConfirmed {
                ConfirmedOnThisMacLabel()
            }

            deviceList(rows)
            searchRow(shortName: shortName, foundCount: rows.filter(\.isPresent).count)

            if bluetooth.isLinked {
                BluetoothPTTSelfTest(pressed: bluetooth.rawPressed)
            }
            BluetoothPTTAudioHelperRow(
                kind: device.kind,
                name: BluetoothPTTSettingsText.displayName(
                    connected: bluetooth.connected, spec: setup.bluetoothPTT,
                    shortName: shortName),
                inputs: inputs, outputs: outputs, locked: false, onUseAudio: onUseAudio)
            advanced(
                device,
                // `confirmations` is already filtered by the preferred name.
                peripheral: BluetoothPTTSettingsText.peripheralText(
                    connected: bluetooth.connected, confirmations: confirmations,
                    preferredName: nil),
                lastRSSI: bluetooth.lastRSSI,
                // forgetConfirmedButton()'s own guard:
                // connected?.identifier ?? candidateIDs().first.
                canForget: bluetooth.connected != nil || !confirmations.isEmpty
            ) {
                bluetooth.forgetConfirmedButton()
            }
        }

        private func deviceList(_ rows: [BluetoothPTTDeviceRow]) -> some View {
            VStack(alignment: .leading, spacing: 2) {
                ForEach(rows) { row in
                    Button {
                        select(row)
                    } label: {
                        HStack(spacing: 6) {
                            Image(
                                systemName: row.isHighlighted
                                    ? "largecircle.fill.circle" : "circle"
                            )
                            .foregroundStyle(row.isHighlighted ? Color.accentColor : .secondary)
                            .accessibilityHidden(true)
                            Text(row.text)
                                .foregroundStyle(row.isPresent ? .primary : .secondary)
                            if row.isConfirmed {
                                Image(systemName: "checkmark.seal.fill")
                                    .foregroundStyle(.green)
                                    .help(BluetoothPTTSettingsText.confirmedOnThisMac)
                                    .accessibilityLabel(BluetoothPTTSettingsText.confirmedOnThisMac)
                            }
                            Spacer(minLength: 0)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.borderless)
                    // An absent unit cannot be connected to; it is listed so
                    // Rob can see what this Mac remembers. Selecting drives the
                    // live controller, so only the active config's rows act.
                    .disabled(!row.isPresent || !isActive)
                    .accessibilityElement(children: .combine)
                    .accessibilityAddTraits(row.isHighlighted ? .isSelected : [])
                }
            }
        }

        private func searchRow(shortName: String, foundCount: Int) -> some View {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 8) {
                    Button(BluetoothPTTSettingsText.searchButton) {
                        bluetooth.search()
                    }
                    .controlSize(.small)
                    .disabled(bluetooth.isSearching)
                    if bluetooth.isSearching {
                        // Reduce Motion: a static glyph instead of the spinner.
                        if reduceMotion {
                            Image(systemName: "dot.radiowaves.left.and.right")
                                .foregroundStyle(.secondary)
                                .accessibilityLabel(searchingText)
                        } else {
                            ProgressView()
                                .controlSize(.small)
                                .accessibilityLabel(searchingText)
                        }
                    }
                    Spacer(minLength: 0)
                }
                if let footnote = BluetoothPTTSettingsText.searchFootnote(
                    isSearching: bluetooth.isSearching, hasSearched: hasSearched,
                    foundCount: foundCount, shortName: shortName)
                {
                    Text(footnote)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }

        private var searchingText: String {
            BluetoothPTTPresentation.settingsStatus(status: .searching, signal: nil).text
        }

        /// Model, Service and Characteristic come from the device table;
        /// Peripheral and Last RSSI from the live link on the active card, or
        /// from this Mac's store ("—" for RSSI) on any other.
        private func advanced(
            _ device: BLEButtonDevice, peripheral: String, lastRSSI: Int16?,
            canForget: Bool, onForget: @escaping () -> Void
        ) -> some View {
            DisclosureGroup(BluetoothPTTSettingsText.advancedTitle) {
                VStack(alignment: .leading, spacing: 4) {
                    advancedRow(BluetoothPTTSettingsText.modelLabel, device.displayName)
                    advancedRow(BluetoothPTTSettingsText.serviceLabel, device.serviceUUID)
                    advancedRow(
                        BluetoothPTTSettingsText.characteristicLabel, device.characteristicUUID)
                    advancedRow(BluetoothPTTSettingsText.peripheralLabel, peripheral)
                    advancedRow(
                        BluetoothPTTSettingsText.lastRSSILabel,
                        BluetoothPTTSettingsText.lastRSSIText(lastRSSI))
                    Button(
                        BluetoothPTTSettingsText.forgetButton, role: .destructive,
                        action: onForget
                    )
                    .controlSize(.small)
                    .disabled(!canForget)
                    .padding(.top, 4)
                }
                .padding(.top, 4)
            }
        }

        private func advancedRow(_ title: String, _ value: String) -> some View {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(title)
                    .foregroundStyle(.secondary)
                    .frame(width: 96, alignment: .leading)
                // Read-only and selectable, so a UUID can be copied into a bug report.
                Text(value)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
            .accessibilityElement(children: .combine)
        }

        private func select(_ row: BluetoothPTTDeviceRow) {
            let present = bluetooth.discovered + [bluetooth.connected].compactMap { $0 }
            guard let button = present.first(where: { $0.identifier == row.id }) else { return }
            bluetooth.select(button)
        }
    }

    // MARK: - Shared pieces

    /// Searching and connecting show a spinner, unless Reduce Motion is on.
    @MainActor
    private func isBusy(_ bluetooth: BluetoothPTTController) -> Bool {
        bluetooth.isSearching || bluetooth.status == .searching || bluetooth.status == .connecting
    }

    /// The status `Label`: glyph and text in the tone's semantic colour.
    private struct BluetoothPTTStatusLine: View {
        let text: String
        let tone: BluetoothPTTTone
        let busy: Bool

        @Environment(\.accessibilityReduceMotion) private var reduceMotion

        var body: some View {
            HStack(spacing: 6) {
                if busy && !reduceMotion {
                    ProgressView()
                        .controlSize(.small)
                        .accessibilityHidden(true)
                } else {
                    Image(systemName: tone.glyph)
                        .foregroundStyle(tone.color)
                        .accessibilityHidden(true)
                }
                Text(text)
                    .font(.caption)
                    .foregroundStyle(tone.color)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(BluetoothPTTPresentation.chipAccessibilityLabel)
            .accessibilityValue(text)
        }
    }

    private struct ConfirmedOnThisMacLabel: View {
        var body: some View {
            Label(BluetoothPTTSettingsText.confirmedOnThisMac, systemImage: "checkmark.seal.fill")
                .font(.caption)
                .foregroundStyle(.green)
        }
    }

    /// "Bluetooth Settings…" or "Open Privacy & Security…", only for the two
    /// statuses System Settings can fix.
    private struct BluetoothPTTSettingsLinkButton: View {
        let status: BLELinkStatus

        var body: some View {
            if let link = BluetoothPTTSettingsText.settingsLink(for: status),
                let url = URL(string: link.url)
            {
                Button(link.title) { NSWorkspace.shared.open(url) }
                    .controlSize(.small)
            }
        }
    }

    /// "Use B02PTT-6522 for mic and speaker", or the pairing hint. The button
    /// sets both devices through the caller, which saves them to the config.
    private struct BluetoothPTTAudioHelperRow: View {
        let kind: BLEDeviceKind
        let name: String
        let inputs: [String]
        let outputs: [String]
        let locked: Bool
        let onUseAudio: (_ input: String, _ output: String) -> Void

        var body: some View {
            switch BluetoothPTTSettingsText.audioHelper(
                kind: kind, name: name, inputs: inputs, outputs: outputs)
            {
            case .use(let input, let output, let title)?:
                Button(title) { onUseAudio(input, output) }
                    .controlSize(.small)
                    .disabled(locked)
            case .pairHint(let text)?:
                Label(text, systemImage: "info.circle")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            case nil:
                EmptyView()
            }
        }
    }

    /// The serial self-test's layout for the button. It only reads
    /// `rawPressed`, which the machine sets whatever its gates say, so it
    /// never keys; outside a call nothing can.
    private struct BluetoothPTTSelfTest: View {
        let pressed: Bool

        var body: some View {
            VStack(alignment: .leading, spacing: 4) {
                Text(BluetoothPTTSettingsText.selfTestHeading)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                HStack(spacing: 8) {
                    Image(systemName: pressed ? "checkmark.circle.fill" : "circle")
                        .foregroundStyle(pressed ? .green : .secondary)
                        .imageScale(.large)
                        .accessibilityHidden(true)
                    Text(
                        pressed
                            ? BluetoothPTTSettingsText.selfTestDetected
                            : BluetoothPTTSettingsText.selfTestIdle
                    )
                    .font(.caption)
                    .foregroundStyle(pressed ? .green : .secondary)
                    .fixedSize(horizontal: false, vertical: true)
                }
                Text(BluetoothPTTSettingsText.selfTestCaption)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                Color.secondary.opacity(0.06),
                in: RoundedRectangle(cornerRadius: 6, style: .continuous)
            )
            .accessibilityElement(children: .combine)
            // The checkmark is visual; announce the rising edge, as the serial
            // self-test does. Mounted only on the active config's card, so one
            // press announces once.
            .onChange(of: pressed) { now in
                if now {
                    AccessibilityAnnouncer.post(
                        BluetoothPTTSettingsText.selfTestDetected, priority: .medium)
                }
            }
        }
    }
#endif
