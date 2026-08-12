// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
import Foundation

/// A repeater-courtesy talk timer: warns the operator to un-key before the
/// repeater's timeout (TOT) drops them. The view shows a small dot next to the
/// connected-node name whose color ramps green → amber → red as the *current
/// continuous* transmission approaches the configured limit. Going red means
/// "un-key now to reset the repeater".
///
/// Platform-neutral (no SwiftUI/AppKit) so the ramp + per-node resolution are
/// unit-testable; the view just reads the phase and maps it to a color.
public enum TalkTimer {
    /// The talk-timer's visual phase for the current continuous transmission.
    public enum Phase: Equatable {
        /// Plenty of hold left.
        case green
        /// Heads-up: within the amber "wrap up" window before the limit.
        case amber
        /// At or past the limit — un-key to reset the repeater.
        case red
    }

    /// How many seconds before the limit the dot turns amber (the "wrap up"
    /// heads-up). Easily tunable; the spec calls for ~15 s.
    public static let amberWindowSeconds: TimeInterval = 15

    /// The global default talk-timer duration (seconds) for nodes the user
    /// hasn't configured. Spec: 2 minutes, enabled.
    public static let defaultLimitSeconds: Int = 120
    /// The global default duration in minutes (the unit the Settings UI uses).
    public static let defaultLimitMinutes: Int = 2
    /// The global default on/off (spec: enabled). Shared by
    /// `FavoritesSettingsView`'s `@AppStorage` default and the accessibility
    /// announcer's talk-timer tick (astar-b167) so a future spec change can't
    /// leave the two out of sync.
    public static let defaultEnabled: Bool = true

    /// Pure ramp: map the elapsed continuous-TX time to a phase.
    ///
    /// - GREEN while more than `amberWindow` seconds remain before `limit`.
    /// - AMBER once within the last `amberWindow` seconds before `limit`.
    /// - RED at or past `limit`.
    ///
    /// Degenerate inputs are handled defensively: a non-positive `limit` is
    /// always RED (no hold time), and an `amberWindow` that meets or exceeds the
    /// limit collapses the green band so it's amber from the start until red.
    public static func phase(
        elapsed: TimeInterval, limit: TimeInterval, amberWindow: TimeInterval = amberWindowSeconds
    ) -> Phase {
        guard limit > 0 else { return .red }
        if elapsed >= limit { return .red }
        let amberStart = max(0, limit - amberWindow)
        if elapsed >= amberStart { return .amber }
        return .green
    }

    /// The courtesy caption for a phase — shared verbatim between the dot's
    /// `.help`/VO value (`MenuPopover`'s talk-timer dot) and the accessibility
    /// announcer (astar-b167), so the two can never disagree.
    public static func help(for phase: Phase) -> String {
        switch phase {
        case .green: return "Talk timer — keep going"
        case .amber: return "Talk timer — wrap up soon"
        case .red: return "Talk timer — un-key to reset the repeater"
        }
    }

    /// Resolve the effective talk-timer limit for the connected node.
    ///
    /// Resolution order:
    /// 1. The node's `NodeEntry` override, when present:
    ///    - `talkTimerEnabled == false` → **disabled** (returns `nil` — no dot).
    ///    - a non-nil `talkTimerSeconds` → that limit.
    ///    - otherwise the field is unset → inherit the global default.
    /// 2. No entry / no override → the global default (`defaultEnabled` /
    ///    `defaultLimitSeconds`).
    ///
    /// Returns the limit in seconds, or `nil` when the timer is disabled for this
    /// node (the caller shows no dot at all, even while keyed).
    public static func resolvedLimitSeconds(
        entry: NodeEntry?,
        defaultEnabled: Bool,
        defaultLimitSeconds: Int
    ) -> Int? {
        // Per-node enable override wins outright. An explicit `false` disables.
        let enabled = entry?.talkTimerEnabled ?? defaultEnabled
        guard enabled else { return nil }
        // Per-node duration override, else the global default.
        let limit = entry?.talkTimerSeconds ?? defaultLimitSeconds
        guard limit > 0 else { return nil }
        return limit
    }
}
