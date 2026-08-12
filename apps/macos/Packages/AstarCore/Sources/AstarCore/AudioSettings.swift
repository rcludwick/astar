// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Persisted, non-secret audio preferences: the selected capture/playback
/// devices (by name; `nil` = system default) and input/output gain. Stored in
/// `UserDefaults` (credentials live in the Keychain instead).
public struct AudioSettings: Equatable {
    public var input: String?
    public var output: String?
    public var inputGain: Float
    /// Output (RX/speaker) gain multiplier: the engine accepts `0.0...4.0`
    /// (iax-a4e7), but the Quick-settings UI only ever offers `1.0...4.0` —
    /// 100%-400% headroom for boosting a quiet station, floored at unity (no
    /// UI attenuation). The persisted default stays unity; existing stored
    /// values are never migrated.
    public var outputGain: Float
    /// Mic voice compression (dynamics) toggle.
    public var compression: Bool
    /// Compression strength (0…1), passed to the engine when `compression` is on.
    /// Default 0.90 reproduces today's feel (the old mic-gain proxy).
    public var compressionLevel: Float
    /// TX trim (0…2, linear): the always-on final TX gain stage after
    /// compression. Attenuates a hot mic that compression makeup gain would
    /// otherwise keep loud; above 1.0 boosts (engine clamps at full scale).
    /// Default 1.0 (unity).
    public var txTrim: Float
    /// Mic noise reduction (denoise) toggle.
    public var noiseReduction: Bool
    /// RX/output compression toggle (iax-a4e7): automatic leveling of the
    /// RECEIVED audio, reusing the mic-path compressor on the output bus,
    /// applied before the output gain multiply. Shared across networks —
    /// output is listener-side, not per-network like the TX chain.
    public var rxCompression: Bool
    /// RX/output compression strength (0…1), passed to the engine when
    /// `rxCompression` is on. Default 0.90 matches the TX compressor's default.
    public var rxCompressionLevel: Float
    /// Voice-activated PTT toggle.
    public var voxEnabled: Bool
    /// Listen-only (monitor) mode: hard-mutes all transmit. Handy for just
    /// monitoring a repeater, especially with VOX on.
    public var txDisabled: Bool
    /// Full-duplex audio. When false (default, half-duplex), VOX won't key while
    /// receiving — so speaker bleed (e.g. the parrot's playback) can't feed back.
    /// Enable for headphones, where simultaneous TX+RX is fine.
    public var fullDuplex: Bool
    /// VOX trigger level (dBFS): the mic level at/above which voice-activated PTT
    /// keys. Default −40. Lower (toward −60) = more sensitive.
    public var voxThresholdDBFS: Float
    /// VOX hang time (ms): how long PTT stays keyed after the mic drops below the
    /// threshold, so brief speech pauses don't drop the transmit. Default 500.
    public var voxHangtimeMS: Int
    /// Selected mic-profile id (the live choice). `nil` = the built-in Default
    /// profile (unfiltered). Mirrors the active config's `Setup.micProfileID`.
    public var micProfileID: String?

    /// The `StationConfig.codecPolicy` string: always `"prefer_slin16"`.
    /// Wideband (slin16, 16 kHz) is always on (astar-e542) — there is no
    /// preference. astar offers slin16+µ-law capability; nodes without
    /// allow=slin16 answer µ-law in IAX2 negotiation, so the node decides the
    /// fallback. Always a string the binding accepts — an unknown policy would
    /// fail `Station(config:)`.
    public var codecPolicyString: String {
        "prefer_slin16"
    }

    public init(
        input: String? = nil, output: String? = nil,
        inputGain: Float = 0.90, outputGain: Float = 1.0,
        compression: Bool = false, compressionLevel: Float = 0.90,
        txTrim: Float = 1.0,
        noiseReduction: Bool = false,
        rxCompression: Bool = false, rxCompressionLevel: Float = 0.90,
        voxEnabled: Bool = false, txDisabled: Bool = false,
        fullDuplex: Bool = false, voxThresholdDBFS: Float = -40,
        voxHangtimeMS: Int = 500, micProfileID: String? = nil
    ) {
        self.input = input
        self.output = output
        self.inputGain = inputGain
        self.outputGain = outputGain
        self.compression = compression
        self.compressionLevel = compressionLevel
        self.txTrim = txTrim
        self.noiseReduction = noiseReduction
        self.rxCompression = rxCompression
        self.rxCompressionLevel = rxCompressionLevel
        self.voxEnabled = voxEnabled
        self.txDisabled = txDisabled
        self.fullDuplex = fullDuplex
        self.voxThresholdDBFS = voxThresholdDBFS
        self.voxHangtimeMS = voxHangtimeMS
        self.micProfileID = micProfileID
    }
}

public protocol AudioSettingsStore {
    func load() -> AudioSettings
    func save(_ settings: AudioSettings)
}

/// `UserDefaults`-backed store. Absent input gain defaults to 0.90 (mic backed
/// off for compression headroom); output gain defaults to 1.0 (unity).
public final class UserDefaultsAudioSettingsStore: AudioSettingsStore {
    private enum Key {
        static let input = "audio.input"
        static let output = "audio.output"
        static let inputGain = "audio.inputGain"
        static let outputGain = "audio.outputGain"
        static let compression = "audio.compression"
        static let compressionLevel = "audio.compressionLevel"
        static let txTrim = "audio.txTrim"
        static let noiseReduction = "audio.noiseReduction"
        static let rxCompression = "audio.rxCompression"
        static let rxCompressionLevel = "audio.rxCompressionLevel"
        static let voxEnabled = "audio.voxEnabled"
        static let txDisabled = "audio.txDisabled"
        static let fullDuplex = "audio.fullDuplex"
        static let voxThreshold = "audio.voxThresholdDBFS"
        static let voxHangtime = "audio.voxHangtimeMS"
        static let micProfileID = "audio.micProfileID"
        // "audio.wideband" is a dead key (astar-e542): the wideband toggle is
        // gone, wideband is always on. A stale key in old saves is ignored.
    }

    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func load() -> AudioSettings {
        AudioSettings(
            input: defaults.string(forKey: Key.input),
            output: defaults.string(forKey: Key.output),
            inputGain: defaults.object(forKey: Key.inputGain) != nil
                ? defaults.float(forKey: Key.inputGain) : 0.90,
            outputGain: defaults.object(forKey: Key.outputGain) != nil
                ? defaults.float(forKey: Key.outputGain) : 1.0,
            compression: defaults.bool(forKey: Key.compression),
            compressionLevel: defaults.object(forKey: Key.compressionLevel) != nil
                ? defaults.float(forKey: Key.compressionLevel) : 0.90,
            txTrim: defaults.object(forKey: Key.txTrim) != nil
                ? defaults.float(forKey: Key.txTrim) : 1.0,
            noiseReduction: defaults.bool(forKey: Key.noiseReduction),
            rxCompression: defaults.bool(forKey: Key.rxCompression),
            rxCompressionLevel: defaults.object(forKey: Key.rxCompressionLevel) != nil
                ? defaults.float(forKey: Key.rxCompressionLevel) : 0.90,
            voxEnabled: defaults.bool(forKey: Key.voxEnabled),
            txDisabled: defaults.bool(forKey: Key.txDisabled),
            fullDuplex: defaults.bool(forKey: Key.fullDuplex),
            voxThresholdDBFS: defaults.object(forKey: Key.voxThreshold) != nil
                ? defaults.float(forKey: Key.voxThreshold) : -40,
            voxHangtimeMS: defaults.object(forKey: Key.voxHangtime) != nil
                ? defaults.integer(forKey: Key.voxHangtime) : 500,
            micProfileID: defaults.string(forKey: Key.micProfileID)
        )
    }

    public func save(_ settings: AudioSettings) {
        defaults.set(settings.input, forKey: Key.input)
        defaults.set(settings.output, forKey: Key.output)
        defaults.set(settings.inputGain, forKey: Key.inputGain)
        defaults.set(settings.outputGain, forKey: Key.outputGain)
        defaults.set(settings.compression, forKey: Key.compression)
        defaults.set(settings.compressionLevel, forKey: Key.compressionLevel)
        defaults.set(settings.txTrim, forKey: Key.txTrim)
        defaults.set(settings.noiseReduction, forKey: Key.noiseReduction)
        defaults.set(settings.rxCompression, forKey: Key.rxCompression)
        defaults.set(settings.rxCompressionLevel, forKey: Key.rxCompressionLevel)
        defaults.set(settings.voxEnabled, forKey: Key.voxEnabled)
        defaults.set(settings.txDisabled, forKey: Key.txDisabled)
        defaults.set(settings.fullDuplex, forKey: Key.fullDuplex)
        defaults.set(settings.voxThresholdDBFS, forKey: Key.voxThreshold)
        defaults.set(settings.voxHangtimeMS, forKey: Key.voxHangtime)
        defaults.set(settings.micProfileID, forKey: Key.micProfileID)
    }
}
