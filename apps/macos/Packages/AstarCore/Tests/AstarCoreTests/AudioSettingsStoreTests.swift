// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class AudioSettingsStoreTests: XCTestCase {
    private func freshDefaults() -> UserDefaults {
        // A throwaway suite so tests don't touch the real app defaults.
        UserDefaults(
            suiteName: "astar.tests.audio.\(ProcessInfo.processInfo.globallyUniqueString)")!
    }

    func testLoadDefaultsWhenEmpty() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        let s = store.load()
        XCTAssertNil(s.input)
        XCTAssertNil(s.output)
        // Mic (TX) gain defaults below unity (0.90) so transmit audio is backed
        // off — sensible with voice compression on. Output stays unity.
        XCTAssertEqual(s.inputGain, 0.90)
        XCTAssertEqual(s.outputGain, 1.0)
        XCTAssertFalse(s.compression)
        XCTAssertFalse(s.noiseReduction)
        XCTAssertFalse(s.rxCompression)
        XCTAssertEqual(s.rxCompressionLevel, 0.90)
        XCTAssertFalse(s.voxEnabled)
    }

    func testDefaultInitInputGainBackedOff() {
        XCTAssertEqual(AudioSettings().inputGain, 0.90)
        XCTAssertEqual(AudioSettings().outputGain, 1.0)
    }

    func testVoxThresholdDefaultsToTheOldDefaultCarriedOver() {
        // −40 dBFS peak was the old default; on the loudness scale that is −50.
        XCTAssertEqual(AudioSettings().voxThresholdDBFS, -50)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().voxThresholdDBFS, -50)
    }

    func testVoxThresholdRoundTripsUnderTheNewKey() {
        let defaults = freshDefaults()
        let store = UserDefaultsAudioSettingsStore(defaults)
        var s = AudioSettings()
        s.voxThresholdDBFS = -28
        store.save(s)
        XCTAssertEqual(store.load().voxThresholdDBFS, -28)
        XCTAssertEqual(defaults.object(forKey: "audio.voxThresholdRMSDBFS") as? Float, -28)
        XCTAssertNil(
            defaults.object(forKey: "audio.voxThresholdDBFS"), "the old key is never written")
    }

    func testAnOldPeakThresholdIsTranslatedOnceByTheCrestOffset() {
        let defaults = freshDefaults()
        defaults.set(Float(-40), forKey: "audio.voxThresholdDBFS")  // Rob's, on the peak scale
        let store = UserDefaultsAudioSettingsStore(defaults)
        XCTAssertEqual(store.load().voxThresholdDBFS, -50)
        XCTAssertEqual(
            defaults.object(forKey: "audio.voxThresholdRMSDBFS") as? Float, -50,
            "the translation is written back, so it happens once")
        // A later change to the old key (an older astar sharing this domain)
        // is not translated again over the user's new-scale value.
        defaults.set(Float(-20), forKey: "audio.voxThresholdDBFS")
        XCTAssertEqual(store.load().voxThresholdDBFS, -50)
    }

    func testANewScaleThresholdIsUsedAsIs() {
        let defaults = freshDefaults()
        defaults.set(Float(-40), forKey: "audio.voxThresholdDBFS")
        defaults.set(Float(-44), forKey: "audio.voxThresholdRMSDBFS")
        XCTAssertEqual(UserDefaultsAudioSettingsStore(defaults).load().voxThresholdDBFS, -44)
    }

    func testTheTranslationClampsToTheSliderRange() {
        XCTAssertEqual(VoxThreshold.fromPeak(-55), -58, "never below the slider's bottom")
        XCTAssertEqual(VoxThreshold.fromPeak(-60), -58)
        XCTAssertEqual(VoxThreshold.clamped(-60), -58, "a stored -60 cannot reach the floor")
        XCTAssertGreaterThan(VoxThreshold.range.lowerBound, VoxGate.meterFloorDBFS)
        XCTAssertEqual(VoxThreshold.fromPeak(5), -5)
        XCTAssertEqual(VoxThreshold.fromPeak(.nan), VoxThreshold.defaultDBFS)
    }

    func testVoxHangtimeDefaultsTo500() {
        XCTAssertEqual(AudioSettings().voxHangtimeMS, 500)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().voxHangtimeMS, 500)
    }

    func testVoxHangtimeRoundTrips() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        var s = AudioSettings()
        s.voxHangtimeMS = 900
        store.save(s)
        XCTAssertEqual(store.load().voxHangtimeMS, 900)
    }

    func testCompressionLevelDefaultsTo090() {
        XCTAssertEqual(AudioSettings().compressionLevel, 0.90)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().compressionLevel, 0.90)
    }

    func testCompressionLevelRoundTrips() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        var s = AudioSettings()
        s.compressionLevel = 0.35
        store.save(s)
        XCTAssertEqual(store.load().compressionLevel, 0.35)
    }

    func testTxTrimDefaultsToUnity() {
        XCTAssertEqual(AudioSettings().txTrim, 1.0)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().txTrim, 1.0)
    }

    func testTxTrimRoundTrips() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        var s = AudioSettings()
        s.txTrim = 0.6
        store.save(s)
        XCTAssertEqual(store.load().txTrim, 0.6)
    }

    func testPreTrimSavedSettingsLoadUnityTrim() {
        // A save from before txTrim existed: other audio keys present, but no
        // audio.txTrim key. Migration must yield unity, not 0.
        let defaults = freshDefaults()
        defaults.set(Float(0.8), forKey: "audio.inputGain")
        defaults.set(true, forKey: "audio.compression")
        let store = UserDefaultsAudioSettingsStore(defaults)
        XCTAssertEqual(store.load().txTrim, 1.0)
    }

    func testRxCompressionLevelDefaultsTo090() {
        XCTAssertEqual(AudioSettings().rxCompressionLevel, 0.90)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().rxCompressionLevel, 0.90)
    }

    func testRxCompressionRoundTrips() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        var s = AudioSettings()
        s.rxCompression = true
        s.rxCompressionLevel = 0.35
        store.save(s)
        let loaded = store.load()
        XCTAssertTrue(loaded.rxCompression)
        XCTAssertEqual(loaded.rxCompressionLevel, 0.35)
    }

    func testPreRxCompressionSavedSettingsLoadDefaults() {
        // A save from before RX compression existed (iax-a4e7): other audio
        // keys present, but no audio.rxCompression/audio.rxCompressionLevel
        // keys. Back-compat decode must yield the defaults (off, 0.90), not 0.
        let defaults = freshDefaults()
        defaults.set(Float(0.8), forKey: "audio.inputGain")
        defaults.set(true, forKey: "audio.compression")
        let store = UserDefaultsAudioSettingsStore(defaults)
        let loaded = store.load()
        XCTAssertFalse(loaded.rxCompression)
        XCTAssertEqual(loaded.rxCompressionLevel, 0.90)
    }

    // MARK: - RX jitter buffer (iax-rxjb)

    func testRxJitterBufferDefaultsOnAt40To200() {
        // ON by default, and Asterisk chan_iax2's own window — the node at
        // the other end of an AllStarLink call IS Asterisk. A new user must
        // not have to find the switch to stop the stutter it fixes.
        XCTAssertTrue(AudioSettings().rxJitterBuffer)
        XCTAssertEqual(AudioSettings().rxJitterMinMS, 40)
        XCTAssertEqual(AudioSettings().rxJitterMaxMS, 200)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        let s = store.load()
        XCTAssertTrue(s.rxJitterBuffer)
        XCTAssertEqual(s.rxJitterMinMS, 40)
        XCTAssertEqual(s.rxJitterMaxMS, 200)
    }

    func testRxJitterBufferRoundTrips() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        var s = AudioSettings()
        s.rxJitterBuffer = false
        s.rxJitterMinMS = 80
        s.rxJitterMaxMS = 320
        store.save(s)
        let loaded = store.load()
        XCTAssertFalse(loaded.rxJitterBuffer, "an OFF that was chosen survives a reload")
        XCTAssertEqual(loaded.rxJitterMinMS, 80)
        XCTAssertEqual(loaded.rxJitterMaxMS, 320)
    }

    func testPreJitterBufferSavedSettingsLoadDefaults() {
        // A save from before the jitter buffer existed: other audio keys
        // present, none of the three new ones. This is the case a plain
        // `defaults.bool(forKey:)` would get WRONG — absent reads as false,
        // and the default here is true.
        let defaults = freshDefaults()
        defaults.set(Float(0.8), forKey: "audio.inputGain")
        defaults.set(true, forKey: "audio.compression")
        let loaded = UserDefaultsAudioSettingsStore(defaults).load()
        XCTAssertTrue(loaded.rxJitterBuffer)
        XCTAssertEqual(loaded.rxJitterMinMS, 40)
        XCTAssertEqual(loaded.rxJitterMaxMS, 200)
    }

    func testCodecPolicyStringIsAlwaysPreferSlin16() {
        // Wideband is always on (astar-e542): there is no toggle, and the codec
        // policy is unconditionally prefer_slin16 — nodes without allow=slin16
        // answer µ-law in IAX2 negotiation, so the fallback is the node's call.
        XCTAssertEqual(AudioSettings().codecPolicyString, "prefer_slin16")
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().codecPolicyString, "prefer_slin16")
    }

    func testStaleWidebandKeyIsIgnored() {
        // A save from when the wideband toggle existed (astar-eb6c) may carry
        // audio.wideband=false. The key is dead: the policy stays wideband.
        let defaults = freshDefaults()
        defaults.set(false, forKey: "audio.wideband")
        defaults.set(Float(0.8), forKey: "audio.inputGain")
        let store = UserDefaultsAudioSettingsStore(defaults)
        XCTAssertEqual(store.load().codecPolicyString, "prefer_slin16")
    }

    func testRoundTrip() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        let settings = AudioSettings(
            input: "UCI150", output: "Speakers", inputGain: 1.5, outputGain: 0.5)

        store.save(settings)

        XCTAssertEqual(store.load(), settings)
    }

    func testRoundTripWithProcessingAndVox() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        let settings = AudioSettings(
            input: "UCI150", output: "Speakers", inputGain: 1.5, outputGain: 0.5,
            compression: true, noiseReduction: true, voxEnabled: true
        )

        store.save(settings)

        XCTAssertEqual(store.load(), settings)
    }

    // MARK: - Transmit time-out (one app-wide, never off)

    private let txTimeoutKey = "audio.txTimeoutSeconds"

    func testTxTimeoutAbsentReads120() {
        XCTAssertEqual(AudioSettings.defaultTxTimeoutSeconds, 120)
        XCTAssertEqual(AudioSettings.txTimeoutRange, 60...600)
        XCTAssertEqual(AudioSettings().txTimeoutSeconds, 120)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().txTimeoutSeconds, 120, "an absent key is the 2 min default")
    }

    func testTxTimeoutZeroLoadsAsOneMinute() {
        // A hand-edited domain or an imported file may carry 0. The time-out
        // can never be off, so 0 must come back as the 1 minute floor.
        let defaults = freshDefaults()
        defaults.set(0, forKey: txTimeoutKey)
        let store = UserDefaultsAudioSettingsStore(defaults)
        XCTAssertEqual(store.load().txTimeoutSeconds, 60)
    }

    func testTxTimeoutRoundsAndClamps() {
        let cases: [(stored: Double, expected: Int)] = [
            (-5, 60), (29, 60), (89, 60), (90, 120), (900, 600),
            (60, 60), (600, 600), (149, 120), (150, 180), (89.5, 60), (330, 360),
        ]
        for (stored, expected) in cases {
            let defaults = freshDefaults()
            defaults.set(stored, forKey: txTimeoutKey)
            let store = UserDefaultsAudioSettingsStore(defaults)
            XCTAssertEqual(store.load().txTimeoutSeconds, expected, "stored \(stored)")
        }
    }

    func testTxTimeoutNonNumericReads120() {
        let text = freshDefaults()
        text.set("300", forKey: txTimeoutKey)
        XCTAssertEqual(UserDefaultsAudioSettingsStore(text).load().txTimeoutSeconds, 120)
        let blob = freshDefaults()
        blob.set(Data([0x01, 0x02]), forKey: txTimeoutKey)
        XCTAssertEqual(UserDefaultsAudioSettingsStore(blob).load().txTimeoutSeconds, 120)
    }

    func testNormalizedTxTimeoutSecondsRejectsNonFinite() {
        XCTAssertEqual(AudioSettings.normalizedTxTimeoutSeconds(.nan), 120)
        XCTAssertEqual(AudioSettings.normalizedTxTimeoutSeconds(.infinity), 120)
        XCTAssertEqual(AudioSettings.normalizedTxTimeoutSeconds(-.infinity), 120)
        XCTAssertEqual(AudioSettings.normalizedTxTimeoutSeconds(1e300), 600)
        XCTAssertEqual(AudioSettings.normalizedTxTimeoutSeconds(0), 60)
    }

    func testTxTimeoutRoundTrips() {
        let defaults = freshDefaults()
        let store = UserDefaultsAudioSettingsStore(defaults)
        let settings = AudioSettings(txTimeoutSeconds: 300)
        store.save(settings)
        XCTAssertEqual(store.load(), settings)
        XCTAssertEqual(defaults.object(forKey: txTimeoutKey) as? Int, 300)
    }

    func testTxTimeoutSaveWritesTheNormalisedValue() {
        let defaults = freshDefaults()
        let store = UserDefaultsAudioSettingsStore(defaults)
        var settings = AudioSettings()
        settings.txTimeoutSeconds = 0
        store.save(settings)
        XCTAssertEqual(defaults.object(forKey: txTimeoutKey) as? Int, 60)
    }

    func testVoxHangtimeIsNotTheTransmitTimeOut() {
        // VOX's Hang Timeout is its "stopped talking" timer and stays its own
        // key and value; the transmit time-out never rewrites it.
        let defaults = freshDefaults()
        let store = UserDefaultsAudioSettingsStore(defaults)
        store.save(AudioSettings(voxHangtimeMS: 500, txTimeoutSeconds: 600))
        let loaded = store.load()
        XCTAssertEqual(loaded.voxHangtimeMS, 500)
        XCTAssertEqual(loaded.txTimeoutSeconds, 600)
        XCTAssertEqual(defaults.object(forKey: "audio.voxHangtimeMS") as? Int, 500)
    }
}
