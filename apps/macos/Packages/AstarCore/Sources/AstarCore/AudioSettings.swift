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
    /// Neural noise-reduction strength (0…1; `1.0` = full, `0.0` = bypass).
    /// Adding a field does NOT bump `ConfigVersion`: an older reader ignores
    /// it, a newer one treats it as unset and gets the default.
    public var denoiseStrength: Float
    /// TX trim (0…4, linear): the always-on final TX gain stage after
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
    /// RX jitter buffer (iax-rxjb): whether received AllStarLink audio is
    /// played out of the adaptive buffer that rides out network timing at
    /// all. On by default — without it any inter-arrival gap longer than one
    /// device callback is an audible hole, which is the stutter this exists
    /// to fix, so a new user must not have to find the switch.
    public var rxJitterBuffer: Bool
    /// Floor of the RX jitter buffer's adaptive depth, ms: the slack it keeps
    /// over the jitter it has measured. Asterisk `chan_iax2`'s `target_extra`,
    /// and its default of 40 — the node at the other end of an AllStarLink
    /// call is Asterisk. Higher buys fewer holes with more latency.
    public var rxJitterMinMS: Int
    /// Ceiling of the RX jitter buffer's adaptive depth, ms: the hard cap the
    /// depth may not grow past. Asterisk's `max_jitterbuf`, default 200.
    public var rxJitterMaxMS: Int
    /// Voice-activated PTT toggle.
    public var voxEnabled: Bool
    /// Listen-only (monitor) mode: hard-mutes all transmit. Handy for just
    /// monitoring a repeater, especially with VOX on.
    public var txDisabled: Bool
    /// Full-duplex audio. When false (default, half-duplex), VOX won't key while
    /// receiving — so speaker bleed (e.g. the parrot's playback) can't feed back.
    /// Enable for headphones, where simultaneous TX+RX is fine.
    public var fullDuplex: Bool
    /// VOX trigger level (dBFS): the mic loudness at/above which voice-activated
    /// PTT keys. Loudness here is the mean power over the last 50 ms (the
    /// snapshot's `voxDB`), not the peak — see `VoxThreshold`. Default −50.
    /// Lower (toward −60) = more sensitive. Stored as
    /// `audio.voxThresholdRMSDBFS`.
    public var voxThresholdDBFS: Float
    /// VOX hang time (ms): how long PTT stays keyed after the mic drops below the
    /// threshold, so brief speech pauses don't drop the transmit. Default 500.
    public var voxHangtimeMS: Int
    /// Transmit time-out, whole seconds: the longest the radio may stay on air
    /// continuously, whatever is keying it (on-screen button, Space, serial,
    /// Bluetooth, VOX). One app-wide value, not per config. It exists so a
    /// stuck source can never hold a transmitter open; that is why it has no
    /// Off and is always a whole number of minutes in `txTimeoutRange`. Not
    /// VOX's Hang Timeout, which only decides when VOX has stopped talking.
    public var txTimeoutSeconds: Int
    /// Selected mic-profile id (the live choice). `nil` = the built-in Default
    /// profile (unfiltered). Mirrors the active config's `Setup.micProfileID`.
    public var micProfileID: String?

    /// Two minutes, Rob's choice, and the engine's own start-up value, so the
    /// gap before the first push is already covered.
    public static let defaultTxTimeoutSeconds = 120
    /// One to ten minutes. The floor is above zero on purpose: no stored,
    /// imported or typed value can turn the time-out off.
    public static let txTimeoutRange: ClosedRange<Int> = 60...600

    /// Nearest whole minute (half away from zero), clamped to 60...600. Never 0,
    /// never "off". A non-finite value is not a choice anyone made, so it reads
    /// as the default rather than either bound.
    public static func normalizedTxTimeoutSeconds(_ raw: Double) -> Int {
        guard raw.isFinite else { return defaultTxTimeoutSeconds }
        let minutes = (raw / 60).rounded(.toNearestOrAwayFromZero)
        let lower = Double(txTimeoutRange.lowerBound)
        let upper = Double(txTimeoutRange.upperBound)
        return Int(min(max(minutes * 60, lower), upper))
    }

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
        denoiseStrength: Float = 1.0,
        txTrim: Float = 1.0,
        noiseReduction: Bool = false,
        rxCompression: Bool = false, rxCompressionLevel: Float = 0.90,
        rxJitterBuffer: Bool = true, rxJitterMinMS: Int = 40, rxJitterMaxMS: Int = 200,
        voxEnabled: Bool = false, txDisabled: Bool = false,
        fullDuplex: Bool = false, voxThresholdDBFS: Float = VoxThreshold.defaultDBFS,
        voxHangtimeMS: Int = 500, micProfileID: String? = nil,
        txTimeoutSeconds: Int = 120
    ) {
        self.input = input
        self.output = output
        self.inputGain = inputGain
        self.outputGain = outputGain
        self.compression = compression
        self.compressionLevel = compressionLevel
        self.denoiseStrength = denoiseStrength
        self.txTrim = txTrim
        self.noiseReduction = noiseReduction
        self.rxCompression = rxCompression
        self.rxCompressionLevel = rxCompressionLevel
        self.rxJitterBuffer = rxJitterBuffer
        self.rxJitterMinMS = rxJitterMinMS
        self.rxJitterMaxMS = rxJitterMaxMS
        self.voxEnabled = voxEnabled
        self.txDisabled = txDisabled
        self.fullDuplex = fullDuplex
        self.voxThresholdDBFS = voxThresholdDBFS
        self.voxHangtimeMS = voxHangtimeMS
        self.micProfileID = micProfileID
        self.txTimeoutSeconds = txTimeoutSeconds
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
        static let denoiseStrength = "audio.denoiseStrength"
        static let txTrim = "audio.txTrim"
        static let noiseReduction = "audio.noiseReduction"
        static let rxCompression = "audio.rxCompression"
        static let rxCompressionLevel = "audio.rxCompressionLevel"
        static let rxJitterBuffer = "audio.rxJitterBuffer"
        static let rxJitterMinMS = "audio.rxJitterMinMS"
        static let rxJitterMaxMS = "audio.rxJitterMaxMS"
        static let voxEnabled = "audio.voxEnabled"
        static let txDisabled = "audio.txDisabled"
        static let fullDuplex = "audio.fullDuplex"
        /// The VOX threshold, measured against the 50 ms mean power.
        static let voxThreshold = VoxThreshold.key
        /// The pre-0.1.21 threshold, measured against the peak. Read once and
        /// translated (`VoxThreshold.fromPeak`), never written.
        static let legacyVoxThreshold = VoxThreshold.legacyPeakKey
        static let voxHangtime = "audio.voxHangtimeMS"
        static let micProfileID = "audio.micProfileID"
        /// Under `audio.` so it travels in the exported settings section, like
        /// the VOX hang time. Additive: no `ConfigVersion` bump.
        static let txTimeoutSeconds = "audio.txTimeoutSeconds"
        // "audio.wideband" is a dead key (astar-e542): the wideband toggle is
        // gone, wideband is always on. A stale key in old saves is ignored.
    }

    /// The transmit time-out's key, for `ConfigTransfer`: an imported value has
    /// to be pushed to the session and the station, so the importer must be
    /// able to tell it arrived. Internal, not public.
    static let txTimeoutSecondsKey = Key.txTimeoutSeconds

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
            denoiseStrength: defaults.object(forKey: Key.denoiseStrength) != nil
                ? defaults.float(forKey: Key.denoiseStrength) : 1.0,
            txTrim: defaults.object(forKey: Key.txTrim) != nil
                ? defaults.float(forKey: Key.txTrim) : 1.0,
            noiseReduction: defaults.bool(forKey: Key.noiseReduction),
            rxCompression: defaults.bool(forKey: Key.rxCompression),
            rxCompressionLevel: defaults.object(forKey: Key.rxCompressionLevel) != nil
                ? defaults.float(forKey: Key.rxCompressionLevel) : 0.90,
            // Absent means "never set", and this one defaults ON — so it can't
            // go through `defaults.bool(forKey:)`, which reads a missing key
            // as false and would silently ship the buffer disabled.
            rxJitterBuffer: defaults.object(forKey: Key.rxJitterBuffer) != nil
                ? defaults.bool(forKey: Key.rxJitterBuffer) : true,
            rxJitterMinMS: defaults.object(forKey: Key.rxJitterMinMS) != nil
                ? defaults.integer(forKey: Key.rxJitterMinMS) : 40,
            rxJitterMaxMS: defaults.object(forKey: Key.rxJitterMaxMS) != nil
                ? defaults.integer(forKey: Key.rxJitterMaxMS) : 200,
            voxEnabled: defaults.bool(forKey: Key.voxEnabled),
            txDisabled: defaults.bool(forKey: Key.txDisabled),
            fullDuplex: defaults.bool(forKey: Key.fullDuplex),
            voxThresholdDBFS: loadVoxThreshold(),
            voxHangtimeMS: defaults.object(forKey: Key.voxHangtime) != nil
                ? defaults.integer(forKey: Key.voxHangtime) : 500,
            micProfileID: defaults.string(forKey: Key.micProfileID),
            txTimeoutSeconds: Self.loadTxTimeoutSeconds(
                defaults.object(forKey: Key.txTimeoutSeconds))
        )
    }

    /// The VOX threshold: the new key as is; else an old peak-calibrated value
    /// translated once — written straight back under the new key, so the
    /// translation happens exactly one time and a later save or import of the
    /// old key cannot re-run it — else the default.
    private func loadVoxThreshold() -> Float {
        if let stored = defaults.object(forKey: Key.voxThreshold) as? NSNumber {
            return VoxThreshold.clamped(stored.floatValue)
        }
        if let legacy = defaults.object(forKey: Key.legacyVoxThreshold) as? NSNumber {
            let translated = VoxThreshold.fromPeak(legacy.floatValue)
            defaults.set(translated, forKey: Key.voxThreshold)
            return translated
        }
        return VoxThreshold.defaultDBFS
    }

    /// Absent → the default. A number of any kind → normalised, so a stored 0
    /// or a negative from a hand-edited or imported file becomes 60, never Off.
    /// Anything else (a string, data) is not a time-out → the default.
    private static func loadTxTimeoutSeconds(_ stored: Any?) -> Int {
        guard let number = stored as? NSNumber else {
            return AudioSettings.defaultTxTimeoutSeconds
        }
        return AudioSettings.normalizedTxTimeoutSeconds(number.doubleValue)
    }

    public func save(_ settings: AudioSettings) {
        defaults.set(settings.input, forKey: Key.input)
        defaults.set(settings.output, forKey: Key.output)
        defaults.set(settings.inputGain, forKey: Key.inputGain)
        defaults.set(settings.outputGain, forKey: Key.outputGain)
        defaults.set(settings.compression, forKey: Key.compression)
        defaults.set(settings.compressionLevel, forKey: Key.compressionLevel)
        defaults.set(settings.denoiseStrength, forKey: Key.denoiseStrength)
        defaults.set(settings.txTrim, forKey: Key.txTrim)
        defaults.set(settings.noiseReduction, forKey: Key.noiseReduction)
        defaults.set(settings.rxCompression, forKey: Key.rxCompression)
        defaults.set(settings.rxCompressionLevel, forKey: Key.rxCompressionLevel)
        defaults.set(settings.rxJitterBuffer, forKey: Key.rxJitterBuffer)
        defaults.set(settings.rxJitterMinMS, forKey: Key.rxJitterMinMS)
        defaults.set(settings.rxJitterMaxMS, forKey: Key.rxJitterMaxMS)
        defaults.set(settings.voxEnabled, forKey: Key.voxEnabled)
        defaults.set(settings.txDisabled, forKey: Key.txDisabled)
        defaults.set(settings.fullDuplex, forKey: Key.fullDuplex)
        defaults.set(settings.voxThresholdDBFS, forKey: Key.voxThreshold)
        defaults.set(settings.voxHangtimeMS, forKey: Key.voxHangtime)
        defaults.set(settings.micProfileID, forKey: Key.micProfileID)
        defaults.set(
            AudioSettings.normalizedTxTimeoutSeconds(Double(settings.txTimeoutSeconds)),
            forKey: Key.txTimeoutSeconds)
    }
}
