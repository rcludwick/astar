// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// Wording and slider arithmetic for the app-wide transmit time-out
/// (Settings → Transmit). Kept pure and out of the view so every string and
/// every conversion the slider makes is pinned by a test; the SwiftUI view
/// only lays these out.
///
/// The time-out has one clock for the combined on-air state, covers every
/// source (on-screen button, Space, serial, Bluetooth, VOX and Legacy callers),
/// and can never be turned off. That is why the slider has no Off position
/// and why `seconds(fromMinutes:)` cannot return 0. The engine and
/// `CallSession.setTxTimeoutSeconds` refuse 0 as well; this is the first of
/// three layers, not the only one.
public enum TransmitTimeoutPresentation {
    public static let sectionTitle = "Transmit"
    public static let label = "Transmit time-out"
    public static let caption =
        "Stops transmitting after this long on air, whatever is keying: on-screen button, Space, serial, Bluetooth or VOX. Release, or stop talking, to transmit again."

    /// Whole minutes, derived from `AudioSettings.txTimeoutRange` (60...600 s)
    /// so the slider and the stored value cannot disagree about the bounds.
    public static var sliderRange: ClosedRange<Double> {
        let seconds = AudioSettings.txTimeoutRange
        return Double(seconds.lowerBound / 60)...Double(seconds.upperBound / 60)
    }

    /// The slider position for a stored value. Normalises first, so a value
    /// that somehow escaped the load clamp still lands on a real tick.
    public static func minutes(fromSeconds seconds: Int) -> Int {
        AudioSettings.normalizedTxTimeoutSeconds(Double(seconds)) / 60
    }

    /// A slider position back to stored seconds. It rounds to a whole minute
    /// and clamps to 60...600, so no position, and no value VoiceOver's adjust
    /// action produces, is ever 0 or "off".
    public static func seconds(fromMinutes minutes: Double) -> Int {
        AudioSettings.normalizedTxTimeoutSeconds(minutes * 60)
    }

    /// The visible readout beside the slider: "2 min".
    public static func valueText(seconds: Int) -> String {
        "\(minutes(fromSeconds: seconds)) min"
    }

    /// What VoiceOver reads for the slider: "2 minutes", "1 minute".
    public static func accessibilityValue(seconds: Int) -> String {
        let n = minutes(fromSeconds: seconds)
        return n == 1 ? "1 minute" : "\(n) minutes"
    }
}
