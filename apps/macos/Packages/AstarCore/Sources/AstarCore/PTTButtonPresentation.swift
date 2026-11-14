// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// What the call card's hold-to-talk button shows, and what one VoiceOver
/// activation or Space press does, as pure functions the view only reads.
///
/// The button used to render from its own press, so a Bluetooth, serial or
/// VOX over left it saying "Hold to Talk". Now `onAir` (the radio's state,
/// `CallSession.ptt`) decides everything it shows. The view's own press
/// (`localPress`) is passed in too, so a test pins that a local press alone
/// never claims ON AIR, and a call site that swapped the two goes red.
///
/// While another source holds the key (`CallSession.manualPressStops`) a
/// press is the universal stop: `CallSession.setPTT(true)` runs
/// `pttStopAll()` instead of keying, and every source that was holding must
/// re-arm. That has to stay reachable when VOX hides the button, so Space
/// stops with VOX on too.
public enum PTTButtonPresentation {
    /// One step of a VoiceOver activation. `press` and `release` go through
    /// the view's `setKeyed(_:)`, the one place that calls
    /// `CallSession.setPTT(_:)`. `stopAll` runs `CallSession.stopAllTransmit()`,
    /// which only ever unkeys. A stop is built from `stopAll`, never from a
    /// press, because `manualPressStops` is up to one poll old: a press could
    /// find the other source already gone and key Manual for an instant.
    public enum PTTActivation: Equatable, Sendable {
        case press
        case release
        case stopAll
    }

    public static let pressTitle = "Hold to Talk  (Space)"
    public static let onAirTitle = "ON AIR"
    public static let stopHint = "Stop transmitting"

    /// The button's text. Unchanged wording, no source name: the operator
    /// knows what he pressed.
    public static func title(onAir: Bool, localPress _: Bool) -> String {
        onAir ? onAirTitle : pressTitle
    }

    /// Red fill and white text. Only the radio's state turns the button red;
    /// the view shows a local press as a slightly darker idle fill instead.
    public static func showsOnAirStyle(onAir: Bool, localPress _: Bool) -> Bool {
        onAir
    }

    /// VoiceOver's value for the button.
    public static func accessibilityValue(onAir: Bool, localPress _: Bool) -> String {
        onAir ? "on air" : "not transmitting"
    }

    /// VoiceOver's hint for the PTT button. Empty when
    /// an activation would not stop anything. Every activation while another
    /// source holds is a full stop (see `accessibilityToggleSteps`), so the
    /// hint never promises more than the activation does.
    public static func accessibilityHint(manualPressStops: Bool) -> String {
        manualPressStops ? stopHint : ""
    }

    /// The exact steps one VoiceOver activation makes, in order. VoiceOver
    /// cannot hold, so the button latches: idle, one activation presses;
    /// keyed, one releases. While another source holds, the activation is a
    /// complete stop that never keys: `stopAll`, after releasing a latched
    /// press if there is one. So a VoiceOver user is never left latched on a
    /// press nobody can see, never needs two activations to stop someone
    /// else's over, and a stop can never put a key-up on the network.
    public static func accessibilityToggleSteps(keyed: Bool, manualPressStops: Bool)
        -> [PTTActivation]
    {
        switch (keyed, manualPressStops) {
        case (false, false): return [.press]
        case (false, true): return [.stopAll]
        case (true, false): return [.release]
        case (true, true): return [.release, .stopAll]
        }
    }

    /// Whether a Space keyDown should run `CallSession.stopAllTransmit()`
    /// while VOX is on. With VOX on the call card has no hold-to-talk button
    /// and Space never keys. But a Bluetooth over (or a stuck but linked
    /// button) still needs a keyboard stop. When nothing holds, Space passes
    /// through untouched.
    public static func spaceStopsWithVox(answered: Bool, voxEnabled: Bool, manualPressStops: Bool)
        -> Bool
    {
        answered && voxEnabled && manualPressStops
    }

    /// What the Space key monitor does with one Space event. The monitor in
    /// `MenuPopover` only switches on this, so the order of its rules is
    /// tested here rather than living untested in a view closure.
    public enum SpaceAction: Equatable, Sendable {
        /// Hand the event on untouched (typing, or nothing to do).
        case passThrough
        /// Swallow it (no beep, no scroll) and do nothing else.
        case consume
        /// `setKeyed(true)`.
        case press
        /// `setKeyed(false)`.
        case release
        /// `CallSession.stopAllTransmit()`, which only ever unkeys.
        case stopAll
    }

    /// The Space monitor's rules, in the order that keeps it safe:
    /// 1. a keyUp while this view holds a Space press always releases it,
    ///    whatever VOX, the call state or a focused text field say now, so a
    ///    hold can never outlive the key;
    /// 2. while not keyed, a space typed into a text field passes through
    ///    (astar-e814), before any stop or key;
    /// 3. with VOX on and something holding, a fresh keyDown is the universal
    ///    stop and every other Space event is swallowed;
    /// 4. otherwise hold-to-talk, only on an answered call with VOX off and
    ///    transmit allowed.
    public static func spaceAction(
        isKeyUp: Bool, isRepeat: Bool, keyed: Bool, typing: Bool, answered: Bool,
        voxEnabled: Bool, canTransmit: Bool, manualPressStops: Bool
    ) -> SpaceAction {
        if isKeyUp, keyed { return .release }
        if !keyed, typing { return .passThrough }
        if spaceStopsWithVox(
            answered: answered, voxEnabled: voxEnabled, manualPressStops: manualPressStops)
        {
            return !isKeyUp && !isRepeat ? .stopAll : .consume
        }
        guard answered, !voxEnabled, canTransmit else { return .passThrough }
        if isKeyUp { return .release }
        return isRepeat ? .consume : .press
    }
}
