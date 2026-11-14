// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// What the call card's PTT button shows, and what one VoiceOver activation
/// or one Space press does. The button must follow the radio (`session.ptt`),
/// never only its own press, and a press while another source holds the key
/// is the universal stop.
final class PTTButtonPresentationTests: XCTestCase {
    func testTitleFollowsTheRadioNotTheLocalPress() {
        // Another source holds the key, no local press: ON AIR.
        XCTAssertEqual(PTTButtonPresentation.title(onAir: true, localPress: false), "ON AIR")
        // A local press the engine has not keyed (refused or latched): not ON AIR.
        XCTAssertEqual(
            PTTButtonPresentation.title(onAir: false, localPress: true), "Hold to Talk  (Space)")
        XCTAssertEqual(PTTButtonPresentation.title(onAir: true, localPress: true), "ON AIR")
        XCTAssertEqual(
            PTTButtonPresentation.title(onAir: false, localPress: false), "Hold to Talk  (Space)")
    }

    func testOnAirStyleFollowsTheRadio() {
        XCTAssertEqual(PTTButtonPresentation.showsOnAirStyle(onAir: true, localPress: false), true)
        XCTAssertEqual(PTTButtonPresentation.showsOnAirStyle(onAir: false, localPress: true), false)
    }

    func testAccessibilityValueFollowsTheRadio() {
        XCTAssertEqual(
            PTTButtonPresentation.accessibilityValue(onAir: true, localPress: false), "on air")
        XCTAssertEqual(
            PTTButtonPresentation.accessibilityValue(onAir: false, localPress: true),
            "not transmitting")
    }

    func testHintSaysStopWhileAnotherSourceHolds() {
        XCTAssertEqual(
            PTTButtonPresentation.accessibilityHint(manualPressStops: true), "Stop transmitting")
        XCTAssertEqual(PTTButtonPresentation.accessibilityHint(manualPressStops: false), "")
    }

    func testIdleActivationKeys() {
        XCTAssertEqual(
            PTTButtonPresentation.accessibilityToggleSteps(keyed: false, manualPressStops: false),
            [.press])
    }

    func testActivationWhileAnotherSourceHoldsIsACompleteStop() {
        // A stop that never keys: no .press. manualPressStops is up to one
        // poll old, so a press here could find the other source already gone
        // and put a short key-up on the network from a control whose hint
        // promised a stop. stopAll only ever unkeys.
        XCTAssertEqual(
            PTTButtonPresentation.accessibilityToggleSteps(keyed: false, manualPressStops: true),
            [.stopAll])
    }

    func testActivationWhileKeyedReleases() {
        XCTAssertEqual(
            PTTButtonPresentation.accessibilityToggleSteps(keyed: true, manualPressStops: false),
            [.release])
    }

    func testActivationWhileKeyedAndAnotherSourceJoinedStopsEverything() {
        // The hint says "Stop transmitting"; releasing only Manual would leave
        // the other source on air. Release the latched press, then stop
        // everything, still without a .press.
        XCTAssertEqual(
            PTTButtonPresentation.accessibilityToggleSteps(keyed: true, manualPressStops: true),
            [.release, .stopAll])
    }

    func testSpaceStopsWithVoxOnlyWhileSomethingHolds() {
        XCTAssertEqual(
            PTTButtonPresentation.spaceStopsWithVox(
                answered: true, voxEnabled: true, manualPressStops: true),
            true)
        // VOX listening, nothing on air: Space passes through untouched.
        XCTAssertEqual(
            PTTButtonPresentation.spaceStopsWithVox(
                answered: true, voxEnabled: true, manualPressStops: false),
            false)
        XCTAssertEqual(
            PTTButtonPresentation.spaceStopsWithVox(
                answered: false, voxEnabled: true, manualPressStops: true),
            false)
        // VOX off: the hold-to-talk path (setKeyed) handles Space.
        XCTAssertEqual(
            PTTButtonPresentation.spaceStopsWithVox(
                answered: true, voxEnabled: false, manualPressStops: true),
            false)
    }

    // MARK: - The Space monitor's routing (the closure only switches on it)

    private func space(
        keyUp: Bool = false, isRepeat: Bool = false, keyed: Bool = false,
        typing: Bool = false, answered: Bool = true, vox: Bool = false,
        canTransmit: Bool = true, holds: Bool = false
    ) -> PTTButtonPresentation.SpaceAction {
        PTTButtonPresentation.spaceAction(
            isKeyUp: keyUp, isRepeat: isRepeat, keyed: keyed, typing: typing,
            answered: answered, voxEnabled: vox, canTransmit: canTransmit,
            manualPressStops: holds)
    }

    func testSpaceKeyUpWhileKeyedReleasesWithVoxOn() {
        // Space held with VOX off, VOX then turned on with the mouse while
        // VOX (or anything else) holds too: the release must still unkey
        // Manual, not be swallowed by the VOX stop branch.
        XCTAssertEqual(space(keyUp: true, keyed: true, vox: true, holds: true), .release)
    }

    func testSpaceKeyUpWhileKeyedReleasesWhileTyping() {
        XCTAssertEqual(space(keyUp: true, keyed: true, typing: true), .release)
    }

    func testSpaceKeyUpWhileKeyedReleasesWhenTransmitIsDisabled() {
        // "TX disabled — listening only" turned on, or the call dropped,
        // mid-hold: the release still reaches setKeyed(false).
        XCTAssertEqual(space(keyUp: true, keyed: true, canTransmit: false), .release)
        XCTAssertEqual(space(keyUp: true, keyed: true, answered: false), .release)
    }

    func testSpaceWithVoxOnStopsOnlyWhileSomethingHolds() {
        XCTAssertEqual(space(vox: true, holds: true), .stopAll)
        XCTAssertEqual(space(vox: true, holds: false), .passThrough)
        XCTAssertEqual(space(isRepeat: true, vox: true, holds: true), .consume)
        XCTAssertEqual(space(keyUp: true, vox: true, holds: true), .consume)
    }

    func testSpaceTypedIntoAFieldPassesThroughWhenNotKeyed() {
        XCTAssertEqual(space(typing: true), .passThrough)
        // The typing guard comes before the VOX stop: a space typed into a
        // field never stops someone's over.
        XCTAssertEqual(space(typing: true, vox: true, holds: true), .passThrough)
    }

    func testHeldSpaceRepeatIntoATextFieldIsConsumed() {
        // astar-e814: once keyed, focus moving to a field does not hand the
        // held Space to it.
        XCTAssertEqual(space(isRepeat: true, keyed: true, typing: true), .consume)
    }

    func testSpaceHoldToTalkWithVoxOff() {
        XCTAssertEqual(space(), .press)
        XCTAssertEqual(space(isRepeat: true, keyed: true), .consume)
        XCTAssertEqual(space(keyUp: true), .release)
        XCTAssertEqual(space(answered: false), .passThrough)
        XCTAssertEqual(space(canTransmit: false), .passThrough)
    }
}
