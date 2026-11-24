// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// The rules behind the speaker-monitor picker, kept out of the view so they
/// can be tested. The picker offers "Off" (nil) plus every output device except
/// the one the radio output is using.
public enum MonitorPickerModel {
    /// The selectable devices (not counting "Off", which is `nil`): every
    /// output except the current one. A `nil` current output is the system
    /// default, whose name is unknown here, so nothing is excluded.
    public static func choices(outputs: [String], currentOutput: String?) -> [String] {
        outputs.filter { $0 != currentOutput }
    }

    /// The monitor to keep after the output changes: `nil` if the output now
    /// is the monitor's device, otherwise the monitor unchanged.
    public static func reconcile(monitor: String?, newOutput: String?) -> String? {
        guard let monitor, monitor != newOutput else { return nil }
        return monitor
    }
}
