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

    func testVoxThresholdDefaultsToMinus40() {
        XCTAssertEqual(AudioSettings().voxThresholdDBFS, -40)
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        XCTAssertEqual(store.load().voxThresholdDBFS, -40)
    }

    func testVoxThresholdRoundTrips() {
        let store = UserDefaultsAudioSettingsStore(freshDefaults())
        var s = AudioSettings()
        s.voxThresholdDBFS = -28
        store.save(s)
        XCTAssertEqual(store.load().voxThresholdDBFS, -28)
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
}
