// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// When the main window may pause `CallSession`'s 20 Hz poll.
///
/// Off the call card, the poll normally pauses, because there are no meters to
/// feed and the churn re-renders long lists (device pickers, 1,400 reflector
/// rows) on every tick. Some states must keep it running anyway, because
/// what the user is looking at only moves when the poll runs:
///
/// - a live serial PTT source: its self-test reads `pttSourceTick` output (astar-d00a);
/// - a linked Bluetooth PTT button: its chip and self-test sit beside state the
///   poll publishes;
/// - being on air from any source: the red menu-bar tint and the talk timer are
///   how an unexpected key stays visible.
///
/// The Bluetooth lease renewals do NOT depend on this. They run on the
/// controller's own 250 ms ticker, so a paused poll can never let a held
/// button's lease lapse into a false stop, and can never keep one alive.
///
/// Pure and platform-neutral, so the rule is unit-tested here rather than
/// buried in a SwiftUI view.
public enum PollPolicy {
    /// True when the poll must keep running.
    public static func keepsPolling(
        onCallPane: Bool, serialActive: Bool, bluetoothLinked: Bool, onAir: Bool
    ) -> Bool {
        onCallPane || serialActive || bluetoothLinked || onAir
    }
}
