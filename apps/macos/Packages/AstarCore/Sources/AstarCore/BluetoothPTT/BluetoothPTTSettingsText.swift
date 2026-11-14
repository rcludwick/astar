// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// What the shared PTT hardware picker shows and allows.
///
/// Pure so the rules are tested: the SwiftUI picker in the app target has no
/// test target, and the System Default rule (shown as None, never editable)
/// is the one that keeps the built-in config from ever opening PTT hardware.
public enum PTTHardwarePickerModel {
    /// The row label, in Quick settings and in each config card.
    public static let rowLabel = "PTT"
    /// Help on "None", which still leaves two ways to transmit.
    public static let noneHelp = "On-screen button and Space only"
    /// Under the disabled row when System Default is active: it has no stored
    /// record to write a choice into, so the caption names the one way to get
    /// a config that does (the + on the Audio configs header).
    public static let systemDefaultCaption =
        "Add a config in Settings → Audio configs to choose a PTT device."

    /// False for no selection and for the built-in System Default, whose
    /// edits would silently go nowhere.
    public static func isEditable(_ setup: Setup?) -> Bool {
        guard let setup else { return false }
        return setup.id != SystemDefaultSetup.id
    }

    /// The choice the row displays. System Default always reads None, even
    /// if a hand-edited record carries a spec, because it never opens PTT
    /// hardware.
    public static func selection(for setup: Setup?) -> PTTHardwareChoice {
        guard let setup, isEditable(setup) else { return PTTHardwareChoice.none }
        return setup.pttHardware
    }

    /// The caption under the row. Only System Default gets one, because only
    /// it is a config that can never take a choice. With no config selected at
    /// all the row is disabled and says nothing.
    public static func caption(for setup: Setup?) -> String? {
        setup?.id == SystemDefaultSetup.id ? systemDefaultCaption : nil
    }

    /// Every choice this build offers, plus the current one if the table no
    /// longer lists it, so the menu never shows a blank selection.
    public static func options(
        selection: PTTHardwareChoice,
        registry: HardwareProfileRegistry = HardwareProfileRegistry(),
        devices: [BLEButtonDevice] = BLEButtonDevices.all
    ) -> [PTTHardwareChoice] {
        let all = PTTHardwareChoice.all(registry: registry, devices: devices)
        return all.contains(selection) ? all : all + [selection]
    }

    /// The choice to hand to `onChoose`, or nil when the row is not editable
    /// or nothing changed. Re-saving the same value would re-run
    /// `applyHardware` and reconnect the button for no reason.
    public static func choiceToApply(
        _ picked: PTTHardwareChoice, current: PTTHardwareChoice, isEditable: Bool
    ) -> PTTHardwareChoice? {
        guard isEditable, picked != current else { return nil }
        return picked
    }

    /// Where a config card sends a pick. Only the active config applies live
    /// (`setActivePTTHardware`, which may open serial or Bluetooth); any
    /// other config only stores the choice, so a card Rob is merely editing
    /// never opens hardware on the live config.
    public static func route(
        _ choice: PTTHardwareChoice, setup: Setup, isActive: Bool
    ) -> PTTHardwareRoute {
        isActive ? .applyLive(choice) : .storeOnly(setup.withPTTHardware(choice))
    }
}

/// What a config card does with a PTT hardware pick.
public enum PTTHardwareRoute: Equatable {
    /// The active config: save and apply live through `setActivePTTHardware`.
    case applyLive(PTTHardwareChoice)
    /// Any other config: store this record with `update`, nothing live.
    case storeOnly(Setup)
}

/// One row of the B02PTT device list in a config card.
public struct BluetoothPTTDeviceRow: Equatable, Identifiable, Sendable {
    /// The CoreBluetooth peripheral identifier. It is also what confirmation
    /// is bound to.
    public let id: UUID
    /// "B02PTT-6522 · Inrico B02PTT", or "B02PTT-6522 (not connected)".
    public let text: String
    /// Found or linked right now, and therefore selectable.
    public let isPresent: Bool
    /// This exact identifier was confirmed on this Mac by a press.
    public let isConfirmed: Bool
    /// The linked unit, or the only unit found. Highlighting never confirms.
    public let isHighlighted: Bool
}

/// The audio helper under the B02PTT status. The button works over BLE
/// alone; the unit's mic and speaker are a separate HFP pairing.
public enum BluetoothPTTAudioHelper: Equatable, Sendable {
    /// CoreAudio lists the unit's HFP mic and speaker: offer to use both.
    case use(input: String, output: String, title: String)
    /// It does not: say how to pair it, and that the button works anyway.
    case pairHint(String)
}

/// A button that opens the System Settings pane where Rob can fix a problem.
public struct BluetoothPTTSettingsLink: Equatable, Sendable {
    public let title: String
    public let url: String
}

/// Every Bluetooth PTT settings string not already in
/// `BluetoothPTTPresentation`, exactly as the settings show it, and the small
/// decisions the settings views make.
public enum BluetoothPTTSettingsText {
    public static let searchButton = "Search"
    public static let confirmedOnThisMac = "Confirmed on this Mac"
    public static let selfTestHeading = "PTT self-test"
    public static let selfTestIdle = "Press and hold the PTT button."
    public static let selfTestDetected = "Button press detected."
    public static let selfTestCaption = "During a call, pressing it transmits."
    public static let advancedTitle = "Advanced"
    public static let modelLabel = "Model"
    public static let serviceLabel = "Service"
    public static let characteristicLabel = "Characteristic"
    public static let peripheralLabel = "Peripheral"
    public static let lastRSSILabel = "Last RSSI"
    public static let forgetButton = "Forget this button"
    /// Shown for a value astar does not know yet.
    public static let noValue = "—"

    /// The one action that starts Bluetooth for an imported or unconfirmed
    /// spec: nothing touches Bluetooth until this is clicked.
    public static func setUpButton(shortName: String) -> String {
        "Set up the \(shortName) on this Mac"
    }

    public static func deviceRowText(advertisedName: String, displayName: String) -> String {
        "\(advertisedName) · \(displayName)"
    }

    public static func absentRowText(advertisedName: String) -> String {
        "\(advertisedName) (not connected)"
    }

    /// Present units first (the linked one leading), then confirmed units
    /// that are not here. `isConfirmed` matches the platform identifier only,
    /// never the name: a second unit with the same name is a new device.
    public static func deviceRows(
        discovered: [BLEDiscoveredButton], connected: BLEDiscoveredButton?,
        confirmations: [BLEConfirmation], displayName: String
    ) -> [BluetoothPTTDeviceRow] {
        let confirmedIDs = Set(confirmations.map(\.platformID))
        var present: [BLEDiscoveredButton] = []
        if let connected { present.append(connected) }
        for button in discovered {
            guard !present.contains(where: { $0.identifier == button.identifier }) else { continue }
            present.append(button)
        }
        let highlighted: UUID? =
            connected?.identifier ?? (present.count == 1 ? present.first?.identifier : nil)
        var rows = present.map { button in
            BluetoothPTTDeviceRow(
                id: button.identifier,
                text: deviceRowText(
                    advertisedName: button.advertisedName, displayName: displayName),
                isPresent: true,
                isConfirmed: confirmedIDs.contains(button.identifier),
                isHighlighted: button.identifier == highlighted)
        }
        let presentIDs = Set(present.map(\.identifier))
        for confirmation in confirmations where !presentIDs.contains(confirmation.platformID) {
            rows.append(
                BluetoothPTTDeviceRow(
                    id: confirmation.platformID,
                    text: absentRowText(advertisedName: confirmation.advertisedName),
                    isPresent: false, isConfirmed: true, isHighlighted: false))
        }
        return rows
    }

    /// Shown once a 30 s search has ended with nothing found, and only then.
    public static func searchFootnote(
        isSearching: Bool, hasSearched: Bool, foundCount: Int, shortName: String
    ) -> String? {
        guard hasSearched, !isSearching, foundCount == 0 else { return nil }
        return "No \(shortName) found. Turn it on and keep it near this Mac."
    }

    public static func lastRSSIText(_ rssi: Int16?) -> String {
        rssi.map { "\($0) dBm" } ?? noValue
    }

    /// The confirmations a config can use: all of them, or, when its spec
    /// names a unit, only those confirmed under that name. The controller
    /// applies the same filter when it picks what to link.
    public static func matchingConfirmations(
        _ confirmations: [BLEConfirmation], preferredName: String?
    ) -> [BLEConfirmation] {
        guard let preferredName else { return confirmations }
        return confirmations.filter { $0.advertisedName == preferredName }
    }

    /// This Mac's identifier for the unit: the linked one, else the first
    /// confirmed one that fits the spec's preferred name.
    public static func peripheralText(
        connected: BLEDiscoveredButton?, confirmations: [BLEConfirmation], preferredName: String?
    ) -> String {
        connected?.identifier.uuidString
            ?? matchingConfirmations(confirmations, preferredName: preferredName)
            .first?.platformID.uuidString
            ?? noValue
    }

    /// The advertised name when one is known, else the model's short name.
    /// This is the name the audio helper matches CoreAudio devices against.
    public static func displayName(
        connected: BLEDiscoveredButton?, spec: BluetoothPTTSpec?, shortName: String
    ) -> String {
        connected?.advertisedName ?? spec?.preferredName ?? shortName
    }

    /// The name for a config that is not active, so has no live link: the
    /// spec's preferred name, else the first unit this Mac confirmed, else the
    /// short name.
    public static func storedDisplayName(
        spec: BluetoothPTTSpec?, confirmations: [BLEConfirmation], shortName: String
    ) -> String {
        spec?.preferredName ?? confirmations.first?.advertisedName ?? shortName
    }

    /// The one unit "Forget this button" clears on a card that is not active:
    /// the one Advanced shows as Peripheral, the first remembered unit that
    /// fits the spec's preferred name. The
    /// active card's `forgetConfirmedButton()` makes the same pick when no
    /// unit is linked, so the label means one thing on every card.
    public static func inactiveForgetTarget(
        confirmations: [BLEConfirmation], preferredName: String?
    ) -> UUID? {
        matchingConfirmations(confirmations, preferredName: preferredName).first?.platformID
    }

    /// Set up on a card that was not active applies that config first. Setup
    /// may start only if the apply made it the selected config: a refused
    /// apply leaves another config live, and its controller must not be put
    /// into setup on this card's behalf.
    public static func setUpAllowed(selectedID: String?, setupID: String) -> Bool {
        selectedID == setupID
    }

    /// Forget on a card that is not active writes the per-Mac store
    /// directly. If the live controller drives the same model it would keep
    /// showing a list the store no longer holds, so that Forget is left to
    /// the active card, which goes through the controller.
    public static func canForgetFromInactive(
        hasConfirmations: Bool, liveDeviceID: String?, deviceID: String
    ) -> Bool {
        hasConfirmations && liveDeviceID != deviceID
    }

    /// An attached spec this Mac has not confirmed (an imported config)
    /// shows only this button until Rob clicks it.
    public static func showsSetUpButton(needsSetup: Bool, awaitingConfirmation: Bool) -> Bool {
        needsSetup && !awaitingConfirmation
    }

    /// Search, the device list, the status line and the rest of the section
    /// appear only once setup has started or a unit is confirmed, so Search
    /// can never reach Bluetooth ahead of Set up.
    public static func showsSearch(needsSetup: Bool, awaitingConfirmation: Bool) -> Bool {
        !showsSetUpButton(needsSetup: needsSetup, awaitingConfirmation: awaitingConfirmation)
    }

    /// Only a speaker-mic has audio to offer. A match needs both a mic and a
    /// speaker named after the unit (`BluetoothAudioMatch`).
    public static func audioHelper(
        kind: BLEDeviceKind, name: String, inputs: [String], outputs: [String]
    ) -> BluetoothPTTAudioHelper? {
        guard kind == .speakerMic else { return nil }
        if let pair = BluetoothAudioMatch.devices(
            advertisedName: name, inputs: inputs, outputs: outputs)
        {
            return .use(
                input: pair.input, output: pair.output, title: "Use \(name) for mic and speaker")
        }
        return .pairHint(
            "Pair \(name) in Bluetooth settings to use its mic and speaker. The button already works."
        )
    }

    /// Bluetooth off and access denied are fixed in System Settings, so they
    /// get a button that opens the right pane. Nothing else does.
    public static func settingsLink(for status: BLELinkStatus) -> BluetoothPTTSettingsLink? {
        switch status {
        case .bluetoothOff:
            return BluetoothPTTSettingsLink(
                title: "Bluetooth Settings…",
                url: "x-apple.systempreferences:com.apple.BluetoothSettings")
        case .unauthorized:
            return BluetoothPTTSettingsLink(
                title: "Open Privacy & Security…",
                url: "x-apple.systempreferences:com.apple.preference.security?Privacy_Bluetooth")
        default:
            return nil
        }
    }

    /// Names the lookalike that was rejected, under "Not a supported PTT button".
    public static func rejectedDetail(status: BLELinkStatus, rejectedName: String?) -> String? {
        guard status == .notSupportedDevice, let rejectedName else { return nil }
        return "Found \(rejectedName), but it does not expose the PTT button service."
    }
}
