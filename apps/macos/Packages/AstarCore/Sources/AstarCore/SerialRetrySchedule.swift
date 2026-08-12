// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Backoff schedule for automatic serial-device re-open attempts
/// (astar-8f90). Pure so the progression is testable without timers.
///
/// Why bounded growth: each re-open attempt against a soured device can
/// cost a detached worker thread + device handle if the open wedges
/// (iax-239a trade-off), so retries must slow down — but never stop, so a
/// replugged device re-arms without user action.
public enum SerialRetrySchedule {
    /// Seconds to wait before retry number `attempt` (0-based):
    /// 2, 4, 8, 16, then 30 forever.
    public static func delay(attempt: Int) -> TimeInterval {
        let cap: TimeInterval = 30
        guard attempt < 4 else { return cap }
        return min(cap, 2.0 * pow(2.0, TimeInterval(attempt)))
    }
}
