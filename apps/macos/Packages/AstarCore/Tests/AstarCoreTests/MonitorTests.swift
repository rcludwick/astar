// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

@MainActor
final class MonitorTests: XCTestCase {
    func testSetupDecodesWithoutMonitorFields() throws {
        let json = #"{"id":"a","name":"A","hardwareProfileID":"none"}"#
        let setup = try JSONDecoder().decode(Setup.self, from: Data(json.utf8))
        XCTAssertNil(setup.monitorDevice)
        XCTAssertNil(setup.monitorGain)
    }

    func testSetupRoundTripsMonitorFields() throws {
        let setup = Setup(
            id: "a", name: "A", hardwareProfileID: "none",
            monitorDevice: "MacBook Speakers", monitorGain: 2.5)
        let back = try JSONDecoder().decode(Setup.self, from: JSONEncoder().encode(setup))
        XCTAssertEqual(back.monitorDevice, "MacBook Speakers")
        XCTAssertEqual(back.monitorGain, 2.5)
    }

    func testAudioSettingsDefaultsMonitorOff() {
        let s = AudioSettings()
        XCTAssertNil(s.monitor)
        XCTAssertEqual(s.monitorGain, 1.0)
    }

    func testAudioSettingsStoreRoundTripsAndDefaults() {
        let defaults = UserDefaults(suiteName: "monitor-\(UUID().uuidString)")!
        let store = UserDefaultsAudioSettingsStore(defaults)
        XCTAssertNil(store.load().monitor)
        XCTAssertEqual(store.load().monitorGain, 1.0)
        var s = store.load()
        s.monitor = "Speakers"
        s.monitorGain = 3
        store.save(s)
        XCTAssertEqual(store.load().monitor, "Speakers")
        XCTAssertEqual(store.load().monitorGain, 3)
        s.monitor = nil
        store.save(s)
        XCTAssertNil(store.load().monitor)
    }

    func testConfigArchiveCarriesMonitorKeys() {
        let slice = ConfigArchive.settingsSlice(from: [
            "audio.monitor": "Speakers", "audio.monitorGain": 2.0,
        ])
        XCTAssertNotNil(slice["audio.monitor"])
        XCTAssertNotNil(slice["audio.monitorGain"])
        XCTAssertTrue(ConfigArchive.isImportable(key: "audio.monitor"))
    }

    func testChoicesExcludeCurrentOutput() {
        XCTAssertEqual(
            MonitorPickerModel.choices(outputs: ["A", "B", "C"], currentOutput: "B"), ["A", "C"])
        XCTAssertEqual(
            MonitorPickerModel.choices(outputs: ["A", "B"], currentOutput: nil), ["A", "B"])
    }

    func testMonitorClearedWhenOutputBecomesMonitorDevice() {
        XCTAssertNil(
            MonitorPickerModel.reconcile(monitor: "MacBook Speakers", newOutput: "MacBook Speakers")
        )
        XCTAssertEqual(
            MonitorPickerModel.reconcile(monitor: "MacBook Speakers", newOutput: "Radio"),
            "MacBook Speakers")
        XCTAssertEqual(
            MonitorPickerModel.reconcile(monitor: "MacBook Speakers", newOutput: nil),
            "MacBook Speakers")
        XCTAssertNil(MonitorPickerModel.reconcile(monitor: nil, newOutput: "Radio"))
    }

    func testSessionWrappersPassThrough() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        try session.selectMonitor("Speakers")
        try session.setMonitorGain(2)
        try session.setMonitorMuted(true)
        XCTAssertEqual(fake.monitorDeviceCalls, ["Speakers"])
        XCTAssertEqual(fake.monitorGainCalls, [2])
        XCTAssertEqual(fake.monitorMutedCalls, [true])
        fake.monitorStatusToReturn = .unavailable("gone")
        XCTAssertEqual(session.monitorStatus(), .unavailable("gone"))
    }

    func testReapplyMonitorResendsSavedDeviceUnlessPlaying() {
        let defaults = UserDefaults(suiteName: "monitor-\(UUID().uuidString)")!
        let store = UserDefaultsAudioSettingsStore(defaults)
        var s = store.load()
        s.monitor = "Speakers"
        s.output = "Radio"
        store.save(s)
        let fake = FakeStation()
        let session = CallSession(
            station: fake, audioStore: store,
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            micProfileStore: UserDefaultsMicProfileStore(defaults), userDefaults: defaults)

        fake.monitorStatusToReturn = .unavailable("missing")
        session.reapplyMonitor()
        XCTAssertEqual(fake.monitorDeviceCalls, ["Speakers"])

        fake.outputsToReturn = ["Radio", "Speakers"]
        fake.monitorStatusToReturn = .playing
        session.reapplyMonitor()
        XCTAssertEqual(
            fake.monitorDeviceCalls, ["Speakers"], "a playing, listed monitor is left alone")

        fake.outputsToReturn = ["Radio"]
        session.reapplyMonitor()
        XCTAssertEqual(
            fake.monitorDeviceCalls, ["Speakers", "Speakers"],
            "playing but unplugged: re-apply")
    }

    func testSelectDevicesClearsMonitorWhenOutputBecomesIt() throws {
        let defaults = UserDefaults(suiteName: "monitor-\(UUID().uuidString)")!
        let store = UserDefaultsAudioSettingsStore(defaults)
        var s = store.load()
        s.monitor = "Speakers"
        store.save(s)
        let fake = FakeStation()
        let session = CallSession(
            station: fake, audioStore: store,
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            micProfileStore: UserDefaultsMicProfileStore(defaults), userDefaults: defaults)

        XCTAssertFalse(try session.selectDevices(input: nil, output: "Radio"))
        XCTAssertEqual(store.load().monitor, "Speakers")
        XCTAssertTrue(try session.selectDevices(input: nil, output: "Speakers"))
        XCTAssertNil(store.load().monitor)
        XCTAssertEqual(fake.monitorDeviceCalls, [nil])
    }

    func testMonitorGainLoadIsClamped() {
        let defaults = UserDefaults(suiteName: "monitor-\(UUID().uuidString)")!
        defaults.set(9.0, forKey: "audio.monitorGain")
        XCTAssertEqual(UserDefaultsAudioSettingsStore(defaults).load().monitorGain, 4)
    }

    func testReapplyMonitorTurnsOffWhenNoneSaved() {
        let defaults = UserDefaults(suiteName: "monitor-\(UUID().uuidString)")!
        let fake = FakeStation()
        let session = CallSession(
            station: fake, audioStore: UserDefaultsAudioSettingsStore(defaults),
            directoryStore: UserDefaultsNodeDirectoryStore(defaults),
            micProfileStore: UserDefaultsMicProfileStore(defaults), userDefaults: defaults)
        session.reapplyMonitor()
        XCTAssertEqual(fake.monitorDeviceCalls, [nil], "no saved monitor: off")
    }

    func testApplyAudioSettingsPushesMonitor() {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        var s = AudioSettings()
        s.monitor = "Speakers"
        s.monitorGain = 1.5
        session.applyAudioSettings(s)
        XCTAssertEqual(fake.monitorDeviceCalls, ["Speakers"])
        XCTAssertEqual(fake.monitorGainCalls, [1.5])
    }

    func testPollPublishesMonitorStateOnChange() {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        XCTAssertEqual(session.monitorState, .off)
        fake.monitorStatusToReturn = .unavailable("Speakers is not connected")
        session.poll()
        XCTAssertEqual(session.monitorState, .unavailable("Speakers is not connected"))
        fake.monitorStatusToReturn = .playing
        session.refreshMonitorState()
        XCTAssertEqual(session.monitorState, .playing)
    }

    func testMonitorMutedTracksTheLastSuccessfulCall() throws {
        let fake = FakeStation()
        let session = CallSession(station: fake)
        XCTAssertFalse(session.monitorMuted)
        try session.setMonitorMuted(true)
        XCTAssertTrue(session.monitorMuted)
        try session.setMonitorMuted(false)
        XCTAssertFalse(session.monitorMuted)
    }
}
