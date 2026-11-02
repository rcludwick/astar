// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// The record button's clock, and what VoiceOver says for the button.
public enum RecordingClock {
    /// `00:42` under an hour, `1:02:03` from an hour on.
    public static func text(seconds: Int) -> String {
        let s = max(0, seconds)
        let hours = s / 3600
        let minutes = (s % 3600) / 60
        let secs = s % 60
        func two(_ n: Int) -> String { n < 10 ? "0\(n)" : "\(n)" }
        return hours > 0
            ? "\(hours):\(two(minutes)):\(two(secs))" : "\(two(minutes)):\(two(secs))"
    }

    /// "Record" while idle, "Recording, 42 seconds" while recording. Past a
    /// minute it names minutes and hours, so a long recording is not read
    /// out as thousands of seconds.
    public static func accessibilityLabel(recording: Bool, seconds: Int) -> String {
        guard recording else { return "Record" }
        let s = max(0, seconds)
        func unit(_ n: Int, _ name: String) -> String { "\(n) \(name)\(n == 1 ? "" : "s")" }
        var parts: [String] = []
        if s >= 3600 { parts.append(unit(s / 3600, "hour")) }
        if s >= 60, (s % 3600) / 60 > 0 { parts.append(unit((s % 3600) / 60, "minute")) }
        if s < 60 || s % 60 > 0 { parts.append(unit(s % 60, "second")) }
        return "Recording, " + parts.joined(separator: " ")
    }
}
