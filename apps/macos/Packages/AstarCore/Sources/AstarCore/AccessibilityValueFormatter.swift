// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// VoiceOver-friendly `accessibilityValue` strings for the sliders across
/// QuickConfigView/SetupsView/SpectrumSettingsView (astar-a9c3 F3). The
/// visible readout text next to each slider already renders a unit ("120%",
/// "−40 dB", "500 ms") using the "-" glyph and "%"/"dB"/"ms" abbreviations —
/// fine for sighted eyes, but VoiceOver reads "-" as "hyphen" (not "minus")
/// and abbreviated units inconsistently. These spell both out in words so
/// the announced value is unambiguous: "120 percent", "minus 40 decibels",
/// "500 milliseconds".
public enum AccessibilityValueFormatter {
    /// A fractional gain (e.g. `1.2` for a slider showing "120%") as
    /// "120 percent".
    public static func percent(_ fraction: Double) -> String {
        "\(Int((fraction * 100).rounded())) percent"
    }

    /// A dB/dBFS value as "minus 40 decibels" (or "0 decibels" at/above
    /// zero) — spelling out "minus" rather than relying on the "-" glyph,
    /// which VoiceOver sometimes reads as "hyphen".
    public static func decibels(_ value: Double) -> String {
        let rounded = Int(value.rounded())
        let unit = abs(rounded) == 1 ? "decibel" : "decibels"
        return rounded < 0 ? "minus \(abs(rounded)) \(unit)" : "\(rounded) \(unit)"
    }

    /// A millisecond duration as "500 milliseconds".
    public static func milliseconds(_ value: Double) -> String {
        let rounded = Int(value.rounded())
        return "\(rounded) \(rounded == 1 ? "millisecond" : "milliseconds")"
    }
}
