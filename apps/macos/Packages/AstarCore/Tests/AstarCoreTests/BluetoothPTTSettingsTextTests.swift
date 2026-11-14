// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// What the PTT hardware picker and the B02PTT settings show. The SwiftUI
/// views have no test target, so every decision and every string they display
/// is pinned here, with `==`.
final class BluetoothPTTSettingsTextTests: XCTestCase {
    private let unitA = UUID(uuidString: "0A0A0A0A-0000-4000-8000-00000000000A")!
    private let unitB = UUID(uuidString: "0B0B0B0B-0000-4000-8000-00000000000B")!
    private let when = Date(timeIntervalSince1970: 1_791_000_000)

    private func b02ptt() throws -> BLEButtonDevice {
        try XCTUnwrap(BLEButtonDevices.device(id: "inrico-b02ptt"))
    }

    private func found(_ id: UUID, _ name: String) -> BLEDiscoveredButton {
        BLEDiscoveredButton(
            identifier: id, advertisedName: name, deviceID: "inrico-b02ptt", rssi: -60)
    }

    private func confirmed(_ id: UUID, _ name: String) -> BLEConfirmation {
        BLEConfirmation(platformID: id, advertisedName: name, confirmedAt: when)
    }

    // MARK: - The picker

    func testPickerStringsAreExact() {
        XCTAssertEqual(PTTHardwarePickerModel.rowLabel, "PTT")
        XCTAssertEqual(PTTHardwarePickerModel.noneHelp, "On-screen button and Space only")
        XCTAssertEqual(
            PTTHardwarePickerModel.systemDefaultCaption,
            "Add a config in Settings → Audio configs to choose a PTT device.")
    }

    /// System Default has no stored record to write to: the row shows None,
    /// disabled, with the caption that says how to get a choice.
    func testSystemDefaultShowsNoneDisabledWithCaption() {
        let setup = SystemDefaultSetup.setup
        XCTAssertFalse(PTTHardwarePickerModel.isEditable(setup))
        XCTAssertEqual(PTTHardwarePickerModel.selection(for: setup), PTTHardwareChoice.none)
        XCTAssertEqual(
            PTTHardwarePickerModel.caption(for: setup),
            "Add a config in Settings → Audio configs to choose a PTT device.")
    }

    /// Even a hand-edited System Default record carrying a spec reads as None:
    /// the built-in never opens PTT hardware.
    func testSystemDefaultShowsNoneEvenWithSpec() {
        var setup = SystemDefaultSetup.setup
        setup.bluetoothPTT = BluetoothPTTSpec(
            deviceID: "inrico-b02ptt", preferredName: "B02PTT-6522")
        XCTAssertEqual(PTTHardwarePickerModel.selection(for: setup), PTTHardwareChoice.none)
        XCTAssertFalse(PTTHardwarePickerModel.isEditable(setup))
    }

    /// No config selected at all (the selected one was deleted): the row is
    /// disabled, and the System Default caption would not apply, so there is
    /// no caption.
    func testNoSelectionIsNotEditable() {
        XCTAssertFalse(PTTHardwarePickerModel.isEditable(nil))
        XCTAssertEqual(PTTHardwarePickerModel.selection(for: nil), PTTHardwareChoice.none)
        XCTAssertNil(PTTHardwarePickerModel.caption(for: nil))
    }

    func testSavedConfigShowsItsHardware() {
        var setup = Setup(
            id: "rig-1", name: "Desk", hardwareProfileID: HardwareProfileRegistry.uci150ID)
        XCTAssertTrue(PTTHardwarePickerModel.isEditable(setup))
        XCTAssertEqual(PTTHardwarePickerModel.selection(for: setup), PTTHardwareChoice.uci150)
        XCTAssertNil(PTTHardwarePickerModel.caption(for: setup))

        setup = setup.withPTTHardware(.bluetooth(deviceID: "inrico-b02ptt"))
        XCTAssertEqual(
            PTTHardwarePickerModel.selection(for: setup),
            PTTHardwareChoice.bluetooth(deviceID: "inrico-b02ptt"))
    }

    func testOptionsAreTheFourChoicesInOrder() {
        let options = PTTHardwarePickerModel.options(selection: PTTHardwareChoice.none)
        XCTAssertEqual(
            options,
            [.none, .uci150, .customSerial, .bluetooth(deviceID: "inrico-b02ptt")])
        XCTAssertEqual(
            options.map(\.title), ["None", "AllScan UCI150", "Custom serial", "Inrico B02PTT"])
    }

    /// A config naming a device this build's table lacks still shows what it
    /// holds, rather than a blank menu.
    func testUnknownBluetoothSelectionStaysListed() {
        let options = PTTHardwarePickerModel.options(
            selection: .bluetooth(deviceID: "retired-button"))
        XCTAssertEqual(
            options,
            [
                .none, .uci150, .customSerial, .bluetooth(deviceID: "inrico-b02ptt"),
                .bluetooth(deviceID: "retired-button"),
            ])
    }

    /// A pick on a non-editable row (System Default) never reaches onChoose.
    func testSystemDefaultPickNeverApplies() {
        XCTAssertNil(
            PTTHardwarePickerModel.choiceToApply(
                .bluetooth(deviceID: "inrico-b02ptt"), current: .none, isEditable: false))
    }

    func testSamePickDoesNotApply() {
        XCTAssertNil(
            PTTHardwarePickerModel.choiceToApply(.uci150, current: .uci150, isEditable: true))
    }

    func testEditablePickApplies() {
        XCTAssertEqual(
            PTTHardwarePickerModel.choiceToApply(
                .bluetooth(deviceID: "inrico-b02ptt"), current: .uci150, isEditable: true),
            PTTHardwareChoice.bluetooth(deviceID: "inrico-b02ptt"))
    }

    // MARK: - Where a pick goes

    /// The active config's pick goes through setActivePTTHardware, which
    /// applies it live (and may open serial or Bluetooth).
    func testActivePickAppliesLive() {
        let setup = Setup(
            id: "rig-1", name: "Desk", hardwareProfileID: HardwareProfileRegistry.uci150ID)
        XCTAssertEqual(
            PTTHardwarePickerModel.route(
                .bluetooth(deviceID: "inrico-b02ptt"), setup: setup, isActive: true),
            PTTHardwareRoute.applyLive(.bluetooth(deviceID: "inrico-b02ptt")))
    }

    /// Any other config only stores the choice: nothing live, so no serial
    /// and no Bluetooth open for a config Rob is not using.
    func testInactivePickOnlyStores() {
        let setup = Setup(
            id: "rig-2", name: "Portable", hardwareProfileID: HardwareProfileRegistry.uci150ID)
        XCTAssertEqual(
            PTTHardwarePickerModel.route(
                .bluetooth(deviceID: "inrico-b02ptt"), setup: setup, isActive: false),
            PTTHardwareRoute.storeOnly(
                setup.withPTTHardware(.bluetooth(deviceID: "inrico-b02ptt"))))
    }

    /// Set up from a card that was not active applies the config first. Only
    /// if that apply made it the selected config may setup start; a refused
    /// apply (a missing HFP device) leaves another config live, whose
    /// controller must not be put into setup.
    func testSetUpAllowedOnlyOnceThisConfigIsLive() {
        XCTAssertTrue(BluetoothPTTSettingsText.setUpAllowed(selectedID: "rig-2", setupID: "rig-2"))
        XCTAssertFalse(BluetoothPTTSettingsText.setUpAllowed(selectedID: "rig-1", setupID: "rig-2"))
        XCTAssertFalse(BluetoothPTTSettingsText.setUpAllowed(selectedID: nil, setupID: "rig-2"))
    }

    // MARK: - Device rows

    /// Exactly one found: listed as "name · model" and pre-highlighted, but
    /// NOT confirmed. Only a press confirms.
    func testSingleFoundUnitIsHighlightedNotConfirmed() {
        let rows = BluetoothPTTSettingsText.deviceRows(
            discovered: [found(unitA, "B02PTT-6522")], connected: nil, confirmations: [],
            displayName: "Inrico B02PTT")
        XCTAssertEqual(
            rows,
            [
                BluetoothPTTDeviceRow(
                    id: unitA, text: "B02PTT-6522 · Inrico B02PTT", isPresent: true,
                    isConfirmed: false, isHighlighted: true)
            ])
    }

    func testTwoFoundNeitherHighlighted() {
        let rows = BluetoothPTTSettingsText.deviceRows(
            discovered: [found(unitA, "B02PTT-6522"), found(unitB, "B02PTT-7731")],
            connected: nil, confirmations: [], displayName: "Inrico B02PTT")
        XCTAssertEqual(rows.map(\.isHighlighted), [false, false])
        XCTAssertEqual(
            rows.map(\.text), ["B02PTT-6522 · Inrico B02PTT", "B02PTT-7731 · Inrico B02PTT"])
    }

    func testStoredButAbsentUnitShowsNotConnected() {
        let rows = BluetoothPTTSettingsText.deviceRows(
            discovered: [], connected: nil, confirmations: [confirmed(unitA, "B02PTT-6522")],
            displayName: "Inrico B02PTT")
        XCTAssertEqual(
            rows,
            [
                BluetoothPTTDeviceRow(
                    id: unitA, text: "B02PTT-6522 (not connected)", isPresent: false,
                    isConfirmed: true, isHighlighted: false)
            ])
    }

    func testConnectedUnitIsHighlightedAndListedOnce() {
        let rows = BluetoothPTTSettingsText.deviceRows(
            discovered: [found(unitA, "B02PTT-6522"), found(unitB, "B02PTT-7731")],
            connected: found(unitA, "B02PTT-6522"),
            confirmations: [confirmed(unitA, "B02PTT-6522")], displayName: "Inrico B02PTT")
        XCTAssertEqual(
            rows,
            [
                BluetoothPTTDeviceRow(
                    id: unitA, text: "B02PTT-6522 · Inrico B02PTT", isPresent: true,
                    isConfirmed: true, isHighlighted: true),
                BluetoothPTTDeviceRow(
                    id: unitB, text: "B02PTT-7731 · Inrico B02PTT", isPresent: true,
                    isConfirmed: false, isHighlighted: false),
            ])
    }

    /// Confirmation is bound to the peripheral identifier, never the name. A
    /// second unit advertising the same name is shown unconfirmed, and the
    /// confirmed one is shown absent.
    func testSameNameDifferentIdentifierRowIsNotConfirmed() {
        let rows = BluetoothPTTSettingsText.deviceRows(
            discovered: [found(unitB, "B02PTT-6522")], connected: nil,
            confirmations: [confirmed(unitA, "B02PTT-6522")], displayName: "Inrico B02PTT")
        XCTAssertEqual(
            rows,
            [
                BluetoothPTTDeviceRow(
                    id: unitB, text: "B02PTT-6522 · Inrico B02PTT", isPresent: true,
                    isConfirmed: false, isHighlighted: true),
                BluetoothPTTDeviceRow(
                    id: unitA, text: "B02PTT-6522 (not connected)", isPresent: false,
                    isConfirmed: true, isHighlighted: false),
            ])
    }

    // MARK: - Search

    func testSearchFootnoteOnlyAfterAFruitlessSearch() {
        XCTAssertEqual(
            BluetoothPTTSettingsText.searchFootnote(
                isSearching: false, hasSearched: true, foundCount: 0, shortName: "B02PTT"),
            "No B02PTT found. Turn it on and keep it near this Mac.")
        XCTAssertNil(
            BluetoothPTTSettingsText.searchFootnote(
                isSearching: true, hasSearched: true, foundCount: 0, shortName: "B02PTT"))
        XCTAssertNil(
            BluetoothPTTSettingsText.searchFootnote(
                isSearching: false, hasSearched: false, foundCount: 0, shortName: "B02PTT"))
        XCTAssertNil(
            BluetoothPTTSettingsText.searchFootnote(
                isSearching: false, hasSearched: true, foundCount: 1, shortName: "B02PTT"))
    }

    // MARK: - Audio helper

    func testAudioHelperOffersTheMatchingPair() {
        XCTAssertEqual(
            BluetoothPTTSettingsText.audioHelper(
                kind: .speakerMic, name: "B02PTT-6522",
                inputs: ["MacBook Pro Microphone", "B02PTT-6522"],
                outputs: ["MacBook Pro Speakers", "B02PTT-6522"]),
            BluetoothPTTAudioHelper.use(
                input: "B02PTT-6522", output: "B02PTT-6522",
                title: "Use B02PTT-6522 for mic and speaker"))
    }

    func testAudioHelperHintsWhenNotPaired() {
        XCTAssertEqual(
            BluetoothPTTSettingsText.audioHelper(
                kind: .speakerMic, name: "B02PTT-6522",
                inputs: ["MacBook Pro Microphone"], outputs: ["MacBook Pro Speakers"]),
            BluetoothPTTAudioHelper.pairHint(
                "Pair B02PTT-6522 in Bluetooth settings to use its mic and speaker. The button already works."
            ))
    }

    func testButtonOnlyDeviceOffersNoAudioHelper() {
        XCTAssertNil(
            BluetoothPTTSettingsText.audioHelper(
                kind: .buttonOnly, name: "B02PTT-6522", inputs: ["B02PTT-6522"],
                outputs: ["B02PTT-6522"]))
    }

    func testDisplayNamePrefersConnectedThenPreferredThenShortName() {
        let spec = BluetoothPTTSpec(deviceID: "inrico-b02ptt", preferredName: "B02PTT-7731")
        XCTAssertEqual(
            BluetoothPTTSettingsText.displayName(
                connected: found(unitA, "B02PTT-6522"), spec: spec, shortName: "B02PTT"),
            "B02PTT-6522")
        XCTAssertEqual(
            BluetoothPTTSettingsText.displayName(connected: nil, spec: spec, shortName: "B02PTT"),
            "B02PTT-7731")
        XCTAssertEqual(
            BluetoothPTTSettingsText.displayName(
                connected: nil, spec: BluetoothPTTSpec(deviceID: "inrico-b02ptt"),
                shortName: "B02PTT"),
            "B02PTT")
        XCTAssertEqual(
            BluetoothPTTSettingsText.displayName(connected: nil, spec: nil, shortName: "B02PTT"),
            "B02PTT")
    }

    /// A config that is not active has no live link: its name comes from the
    /// spec, then from this Mac's first confirmation, then the short name.
    func testStoredDisplayNamePrefersSpecThenConfirmationThenShortName() {
        XCTAssertEqual(
            BluetoothPTTSettingsText.storedDisplayName(
                spec: BluetoothPTTSpec(deviceID: "inrico-b02ptt", preferredName: "B02PTT-7731"),
                confirmations: [confirmed(unitA, "B02PTT-6522")], shortName: "B02PTT"),
            "B02PTT-7731")
        XCTAssertEqual(
            BluetoothPTTSettingsText.storedDisplayName(
                spec: BluetoothPTTSpec(deviceID: "inrico-b02ptt"),
                confirmations: [confirmed(unitA, "B02PTT-6522")], shortName: "B02PTT"),
            "B02PTT-6522")
        XCTAssertEqual(
            BluetoothPTTSettingsText.storedDisplayName(
                spec: BluetoothPTTSpec(deviceID: "inrico-b02ptt"), confirmations: [],
                shortName: "B02PTT"),
            "B02PTT")
    }

    // MARK: - Forget on a config that is not active

    /// Forget on a card that is not active writes the per-Mac store directly.
    /// When the live controller drives the same model it would keep a stale
    /// list, so that card's Forget is disabled and the active card does it.
    func testInactiveForgetOnlyWhenTheControllerDrivesAnotherModel() {
        XCTAssertTrue(
            BluetoothPTTSettingsText.canForgetFromInactive(
                hasConfirmations: true, liveDeviceID: nil, deviceID: "inrico-b02ptt"))
        XCTAssertTrue(
            BluetoothPTTSettingsText.canForgetFromInactive(
                hasConfirmations: true, liveDeviceID: "other-button", deviceID: "inrico-b02ptt"))
        XCTAssertFalse(
            BluetoothPTTSettingsText.canForgetFromInactive(
                hasConfirmations: true, liveDeviceID: "inrico-b02ptt", deviceID: "inrico-b02ptt"))
        XCTAssertFalse(
            BluetoothPTTSettingsText.canForgetFromInactive(
                hasConfirmations: false, liveDeviceID: nil, deviceID: "inrico-b02ptt"))
    }

    /// "Forget this button" forgets one unit, the same one on every card: the
    /// one Advanced shows as Peripheral. On a card that is not active that is
    /// the first remembered unit; the others stay listed.
    func testInactiveForgetTargetsThePeripheralShown() {
        let confirmations = [confirmed(unitB, "B02PTT-7731"), confirmed(unitA, "B02PTT-6522")]
        XCTAssertEqual(
            BluetoothPTTSettingsText.inactiveForgetTarget(
                confirmations: confirmations, preferredName: nil),
            unitB)
        XCTAssertEqual(
            BluetoothPTTSettingsText.peripheralText(
                connected: nil, confirmations: confirmations, preferredName: nil),
            unitB.uuidString)
        XCTAssertNil(
            BluetoothPTTSettingsText.inactiveForgetTarget(confirmations: [], preferredName: nil))
    }

    /// A config that names its unit shows, and forgets, that unit only: the
    /// same filter the controller applies when it picks what to link, so
    /// Peripheral, Forget and the linked unit always agree.
    func testPreferredNameNarrowsPeripheralAndForget() {
        let confirmations = [confirmed(unitA, "B02PTT-6522"), confirmed(unitB, "B02PTT-7731")]
        XCTAssertEqual(
            BluetoothPTTSettingsText.matchingConfirmations(
                confirmations, preferredName: "B02PTT-7731"
            ).map(\.platformID),
            [unitB])
        XCTAssertEqual(
            BluetoothPTTSettingsText.inactiveForgetTarget(
                confirmations: confirmations, preferredName: "B02PTT-7731"),
            unitB)
        XCTAssertEqual(
            BluetoothPTTSettingsText.peripheralText(
                connected: nil, confirmations: confirmations, preferredName: "B02PTT-7731"),
            unitB.uuidString)
        XCTAssertNil(
            BluetoothPTTSettingsText.inactiveForgetTarget(
                confirmations: confirmations, preferredName: "B02PTT-0000"))
    }

    // MARK: - Fixed strings

    func testSectionStringsAreExact() throws {
        XCTAssertEqual(BluetoothPTTSettingsText.searchButton, "Search")
        XCTAssertEqual(BluetoothPTTSettingsText.confirmedOnThisMac, "Confirmed on this Mac")
        XCTAssertEqual(
            BluetoothPTTSettingsText.setUpButton(shortName: try b02ptt().namePrefix),
            "Set up the B02PTT on this Mac")
        XCTAssertEqual(BluetoothPTTSettingsText.selfTestHeading, "PTT self-test")
        XCTAssertEqual(BluetoothPTTSettingsText.selfTestIdle, "Press and hold the PTT button.")
        XCTAssertEqual(BluetoothPTTSettingsText.selfTestDetected, "Button press detected.")
        XCTAssertEqual(
            BluetoothPTTSettingsText.selfTestCaption, "During a call, pressing it transmits.")
        XCTAssertEqual(BluetoothPTTSettingsText.advancedTitle, "Advanced")
        XCTAssertEqual(BluetoothPTTSettingsText.modelLabel, "Model")
        XCTAssertEqual(BluetoothPTTSettingsText.serviceLabel, "Service")
        XCTAssertEqual(BluetoothPTTSettingsText.characteristicLabel, "Characteristic")
        XCTAssertEqual(BluetoothPTTSettingsText.peripheralLabel, "Peripheral")
        XCTAssertEqual(BluetoothPTTSettingsText.lastRSSILabel, "Last RSSI")
        XCTAssertEqual(BluetoothPTTSettingsText.forgetButton, "Forget this button")
    }

    func testLastRSSIText() {
        XCTAssertEqual(BluetoothPTTSettingsText.lastRSSIText(-58), "-58 dBm")
        XCTAssertEqual(BluetoothPTTSettingsText.lastRSSIText(nil), "—")
    }

    func testPeripheralTextPrefersTheLinkedUnit() {
        XCTAssertEqual(
            BluetoothPTTSettingsText.peripheralText(
                connected: found(unitA, "B02PTT-6522"),
                confirmations: [confirmed(unitB, "B02PTT-7731")], preferredName: nil),
            "0A0A0A0A-0000-4000-8000-00000000000A")
        XCTAssertEqual(
            BluetoothPTTSettingsText.peripheralText(
                connected: nil, confirmations: [confirmed(unitB, "B02PTT-7731")],
                preferredName: nil),
            "0B0B0B0B-0000-4000-8000-00000000000B")
        XCTAssertEqual(
            BluetoothPTTSettingsText.peripheralText(
                connected: nil, confirmations: [], preferredName: nil),
            "—")
    }

    /// The two problems Rob can fix in System Settings get a button there;
    /// every other status gets none.
    func testSettingsLinksOnlyForOffAndUnauthorized() throws {
        XCTAssertEqual(
            BluetoothPTTSettingsText.settingsLink(for: .bluetoothOff),
            BluetoothPTTSettingsLink(
                title: "Bluetooth Settings…",
                url: "x-apple.systempreferences:com.apple.BluetoothSettings"))
        XCTAssertEqual(
            BluetoothPTTSettingsText.settingsLink(for: .unauthorized),
            BluetoothPTTSettingsLink(
                title: "Open Privacy & Security…",
                url: "x-apple.systempreferences:com.apple.preference.security?Privacy_Bluetooth"))
        for raw in UInt32(0)...10 {
            let status = try XCTUnwrap(BLELinkStatus(rawValue: raw))
            if status == .bluetoothOff || status == .unauthorized { continue }
            XCTAssertNil(BluetoothPTTSettingsText.settingsLink(for: status), "\(status)")
        }
    }

    func testRejectedDetailOnlyForALookalike() {
        XCTAssertEqual(
            BluetoothPTTSettingsText.rejectedDetail(
                status: .notSupportedDevice, rejectedName: "B02PTT-7731"),
            "Found B02PTT-7731, but it does not expose the PTT button service.")
        XCTAssertNil(
            BluetoothPTTSettingsText.rejectedDetail(status: .ready, rejectedName: "B02PTT-7731"))
        XCTAssertNil(
            BluetoothPTTSettingsText.rejectedDetail(status: .notSupportedDevice, rejectedName: nil))
    }

    /// "Set up the B02PTT on this Mac" shows for an attached, unconfirmed spec
    /// (an imported config) until Rob starts setup, and never otherwise.
    func testSetUpButtonShowsOnlyBeforeSetupStarts() {
        XCTAssertTrue(
            BluetoothPTTSettingsText.showsSetUpButton(
                needsSetup: true, awaitingConfirmation: false))
        XCTAssertFalse(
            BluetoothPTTSettingsText.showsSetUpButton(needsSetup: true, awaitingConfirmation: true))
        XCTAssertFalse(
            BluetoothPTTSettingsText.showsSetUpButton(
                needsSetup: false, awaitingConfirmation: false))
    }

    /// Search, the device list and the status line appear only once setup
    /// has started or a unit is confirmed. Before Set up is clicked the card
    /// offers that one button, so Search can never reach Bluetooth ahead of it.
    func testSearchHiddenUntilSetupStarts() {
        XCTAssertFalse(
            BluetoothPTTSettingsText.showsSearch(needsSetup: true, awaitingConfirmation: false))
        XCTAssertTrue(
            BluetoothPTTSettingsText.showsSearch(needsSetup: true, awaitingConfirmation: true))
        XCTAssertTrue(
            BluetoothPTTSettingsText.showsSearch(needsSetup: false, awaitingConfirmation: false))
    }
}
