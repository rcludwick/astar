// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// The three colours a Bluetooth PTT line can take. The views map these to
/// green, `.secondary` and orange, the serial self-test's idiom, so the two
/// PTT devices look alike.
public enum BluetoothPTTTone: Equatable, Sendable {
    case ready
    case secondary
    case warning
}

/// Every Bluetooth PTT string, in one pure place. The SwiftUI views and the
/// VoiceOver planner both read from here, so the chip, the settings line and
/// what VoiceOver says can never drift apart. The tests compare with `==`.
public enum BluetoothPTTPresentation {
    /// A forced-stop banner hides this long after the stop it reports.
    static let bannerLifetime: TimeInterval = 8

    /// The status chip on the dial page and the call card. `nil` hides the
    /// chip, which happens while the controller is disabled.
    /// `transmitBlocked` (listen-only or a receive-only network) overrides
    /// everything else, so Rob is never told the button is ready when it
    /// cannot transmit.
    public static func chipText(
        status: BLELinkStatus, signal: BLESignalBand?, shortName: String, transmitBlocked: Bool
    ) -> String? {
        if status == .disabled { return nil }
        if transmitBlocked { return "\(shortName) · won't transmit" }
        return "\(shortName) · \(chipWord(status: status, signal: signal))"
    }

    /// Secondary while things are normal or still coming up; orange when
    /// something needs Rob.
    public static func chipTone(status: BLELinkStatus, transmitBlocked: Bool) -> BluetoothPTTTone {
        if transmitBlocked { return .secondary }
        switch status {
        case .disabled, .ready, .searching, .connecting, .needsConfirm:
            return .secondary
        case .reconnecting, .bluetoothOff, .unauthorized, .unsupported, .notSupportedDevice,
            .releaseToArm:
            return .warning
        }
    }

    public static let chipAccessibilityLabel = "Bluetooth PTT button"

    /// VoiceOver reads the chip as a sentence, with the unit's own name.
    public static func chipAccessibilityValue(
        name: String, status: BLELinkStatus, signal: BLESignalBand?, transmitBlocked: Bool
    ) -> String {
        let base: String
        switch status {
        case .ready:
            if let signal {
                base = "\(name), connected, signal \(signalWord(signal))"
            } else {
                base = "\(name), connected"
            }
        case .searching: base = "\(name), searching"
        case .connecting: base = "\(name), connecting"
        case .needsConfirm: base = "\(name), not confirmed on this Mac"
        case .reconnecting: base = "\(name), reconnecting"
        case .bluetoothOff: base = "\(name), Bluetooth is off"
        case .unauthorized: base = "\(name), Bluetooth access is off for astar"
        case .unsupported: base = "\(name), Bluetooth unavailable"
        case .notSupportedDevice: base = "\(name), not a supported PTT button"
        case .releaseToArm: base = "\(name), release to arm"
        // The chip is hidden while disabled; this only answers a stray query.
        case .disabled: base = "\(name), off"
        }
        return transmitBlocked ? base + ", won't transmit" : base
    }

    /// The status line in the config card.
    public static func settingsStatus(status: BLELinkStatus, signal: BLESignalBand?)
        -> (text: String, tone: BluetoothPTTTone)
    {
        switch status {
        case .ready:
            if let signal { return ("Ready · signal \(signalWord(signal))", .ready) }
            return ("Ready", .ready)
        case .searching: return ("Searching…", .secondary)
        case .connecting: return ("Connecting…", .secondary)
        case .needsConfirm: return ("Press the button once to confirm it's yours.", .secondary)
        case .disabled: return ("Off", .secondary)
        case .reconnecting: return ("Reconnecting…", .warning)
        case .bluetoothOff: return ("Bluetooth is off", .warning)
        case .unauthorized: return ("Bluetooth access is off for astar", .warning)
        case .unsupported: return ("This Mac has no Bluetooth LE", .warning)
        case .notSupportedDevice: return ("Not a supported PTT button", .warning)
        case .releaseToArm: return ("Press ignored: release and press again", .warning)
        }
    }

    /// The compact line in Quick settings. When ready it uses the chip's
    /// wording; problems read the same as in the config card.
    public static func quickStatus(status: BLELinkStatus, signal: BLESignalBand?, shortName: String)
        -> (text: String, tone: BluetoothPTTTone)
    {
        switch status {
        case .searching: return ("Searching…", .secondary)
        case .connecting: return ("Connecting…", .secondary)
        case .needsConfirm: return ("Press the button once to confirm it's yours.", .secondary)
        case .ready:
            return ("\(shortName) · \(chipWord(status: .ready, signal: signal))", .secondary)
        default:
            return settingsStatus(status: status, signal: signal)
        }
    }

    /// The banner for a release the Bluetooth machine forced. An ordinary
    /// release, or detaching the button, needs none.
    public static func releaseBanner(_ reason: BLEReleaseReason, shortName: String) -> String? {
        switch reason {
        case .button, .disabled:
            return nil
        case .linkLost:
            return "Transmit stopped: \(shortName) disconnected. "
                + "Release the button, then press again."
        case .adapterOff:
            return "Transmit stopped: Bluetooth turned off. Release the button, then press again."
        case .malformed:
            return "Transmit stopped: \(shortName) sent a value astar doesn't recognise. "
                + "Release the button, then press again."
        case .sleep:
            return "Transmit stopped: the Mac went to sleep. Release the button, then press again."
        }
    }

    /// The banner for a stop the engine forced. The transmit time-out has one
    /// wording for every source, VOX included, with the minutes taken from the
    /// setting that was in force.
    public static func forcedStopBanner(_ stop: PTTForcedStop) -> String {
        switch stop.reason {
        case .txTimeout:
            let minutes = max(1, Int((Double(stop.txTimeoutSeconds) / 60).rounded()))
            if minutes == 1 { return "Transmit stopped after 1 minute. Release to talk again." }
            return "Transmit stopped after \(minutes) minutes. Release to talk again."
        case .leaseExpired:
            return "Transmit stopped: astar stopped responding. "
                + "Release the button, then press again."
        }
    }

    /// The one banner to show: the newer of the engine's and the button's
    /// forced stops. It hides `bannerLifetime` after that stop, or as soon as
    /// a new key-up starts after it, because the next clean press is the
    /// acknowledgement. On a tie the engine's wins, since it names what the
    /// engine actually did.
    public static func bannerText(
        engine: PTTForcedStop?, bluetooth: BluetoothForcedRelease?, shortName: String,
        now: Date, keyedSince: Date?
    ) -> String? {
        var candidates: [(at: Date, text: String)] = []
        if let engine { candidates.append((engine.at, forcedStopBanner(engine))) }
        if let bluetooth, let text = releaseBanner(bluetooth.reason, shortName: shortName) {
            candidates.append((bluetooth.at, text))
        }
        guard let newest = candidates.max(by: { $0.at < $1.at }) else { return nil }
        if now.timeIntervalSince(newest.at) >= bannerLifetime { return nil }
        if let keyedSince, keyedSince > newest.at { return nil }
        return newest.text
    }

    private static func signalWord(_ band: BLESignalBand) -> String {
        switch band {
        case .strong: return "strong"
        case .fair: return "fair"
        case .weak: return "weak"
        }
    }

    private static func chipWord(status: BLELinkStatus, signal: BLESignalBand?) -> String {
        switch status {
        case .ready: return signal.map(signalWord) ?? "connected"
        case .searching: return "searching…"
        case .connecting: return "connecting…"
        case .needsConfirm: return "not confirmed"
        case .reconnecting: return "reconnecting…"
        case .bluetoothOff: return "Bluetooth off"
        case .unauthorized: return "no Bluetooth access"
        case .unsupported: return "Bluetooth unavailable"
        case .notSupportedDevice: return "not supported"
        case .releaseToArm: return "release to arm"
        case .disabled: return "off"
        }
    }
}

/// Finds the speaker-mic's own HFP devices in CoreAudio, so one click can
/// make them a config's mic and speaker.
public enum BluetoothAudioMatch {
    /// The first input and the first output whose names start with
    /// `advertisedName` (case-sensitive, like the advertised-name match). It
    /// returns `nil` unless both exist, because half a pairing would leave the
    /// config with a dead mic or a silent speaker.
    public static func devices(advertisedName: String, inputs: [String], outputs: [String])
        -> (input: String, output: String)?
    {
        guard !advertisedName.isEmpty,
            let input = inputs.first(where: { $0.hasPrefix(advertisedName) }),
            let output = outputs.first(where: { $0.hasPrefix(advertisedName) })
        else { return nil }
        return (input, output)
    }
}
