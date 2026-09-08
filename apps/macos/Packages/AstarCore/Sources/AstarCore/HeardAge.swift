// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// How long ago a station was heard, in the words a person would use out loud.
///
/// Whole units, no decimals: an age on a heard list is a rough sense of
/// "recently" or "a while back", and a second decimal place is noise that
/// redraws for nothing. Under two seconds reads "now", so a row that has just
/// arrived never flashes "0 s" at the operator.
///
/// The one place that turns `HeardEntry.ageMs` into text — views format
/// through this and nowhere else.
public enum HeardAge {
    /// The label for an age in milliseconds: "now", "12 s", "5 min", "2 h".
    public static func label(ms: UInt64) -> String {
        let seconds = ms / 1_000
        if seconds < 2 { return "now" }
        if seconds < 60 { return "\(seconds) s" }
        let minutes = seconds / 60
        if minutes < 60 { return "\(minutes) min" }
        return "\(seconds / 3_600) h"
    }

    /// A station and its age as VoiceOver should read them: "W6VS, 12 s ago",
    /// or "W6VS, just now" for the "now" bucket — "W6VS, now ago" is not
    /// English. Every heard line speaks through this, the talker caption and
    /// the history rows alike, so the two never drift apart.
    public static func spoken(callsign: String, age: String) -> String {
        "\(callsign), \(age == "now" ? "just now" : "\(age) ago")"
    }
}
