// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import CoreAudio
    import SwiftUI

    /// Reactive, OS-backed source of truth for the available audio device names.
    ///
    /// Replaces the previous pattern where each view enumerated devices
    /// synchronously in `onAppear` (`session.inputs()`/`outputs()` → cpal /
    /// CoreAudio, several enumerations). That ran on the main thread — most visibly
    /// freezing the Quick-settings turnstile reveal ~1s (regression after
    /// astar-1f48) — and produced a one-shot snapshot that never updated when a
    /// device was plugged in or removed while the UI was open.
    ///
    /// This monitor enumerates once off the main thread on init and republishes
    /// whenever CoreAudio reports a device-list change, so views read the published
    /// `inputs`/`outputs` (instant, no enumeration on appear) and the pickers stay
    /// live on hotplug.
    ///
    /// macOS-only: it relies on CoreAudio's `kAudioHardwarePropertyDevices`
    /// listener. iOS gets an equivalent via `AVAudioSession.routeChangeNotification`
    /// in a follow-up.
    @MainActor
    final class AudioDeviceMonitor: ObservableObject {
        /// Capture (input/mic) device names. Empty until the first enumeration lands.
        @Published private(set) var inputs: [String] = []
        /// Playback (output/speaker) device names.
        @Published private(set) var outputs: [String] = []
        /// Whether the SELECTED capture device can be opened right now.
        /// `nil` until the first probe, and while a call is up.
        @Published private(set) var inputReadiness: DeviceReadiness?
        /// The same for the selected playback device.
        @Published private(set) var outputReadiness: DeviceReadiness?

        /// Device names the readiness probe is asking about. Set by the view
        /// that owns the pickers; `nil` means "the system default".
        private var probedInput: String??
        private var probedOutput: String??
        private var readinessTimer: Timer?

        /// How often the readiness probe runs while idle.
        ///
        /// The probe is two CoreAudio property reads per device and opens
        /// nothing, so this is cheap — but a device being unplugged is not an
        /// event anyone watches for, and a few seconds is soon enough to catch
        /// it before someone dials.
        private static let readinessInterval: TimeInterval = 3

        private let session: CallSession
        // Enumeration runs here, never on the main thread. The CoreAudio listener
        // block is also delivered on this queue.
        private let queue = DispatchQueue(
            label: "com.astar.audio-device-monitor", qos: .userInitiated)
        private var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        private var listenerBlock: AudioObjectPropertyListenerBlock?

        init(session: CallSession) {
            self.session = session
            // Initial population, off-main.
            refresh()
            // Re-enumerate whenever the system device list changes (hotplug).
            let block: AudioObjectPropertyListenerBlock = { [weak self] _, _ in
                self?.refresh()
            }
            listenerBlock = block
            AudioObjectAddPropertyListenerBlock(
                AudioObjectID(kAudioObjectSystemObject), &address, queue, block)
        }

        deinit {
            readinessTimer?.invalidate()
            if let listenerBlock {
                AudioObjectRemovePropertyListenerBlock(
                    AudioObjectID(kAudioObjectSystemObject), &address, queue, listenerBlock)
            }
        }

        /// Tell the monitor which devices to report readiness for, and start
        /// (or restart) the timer. Called by the settings view as it appears
        /// and whenever a picker changes.
        ///
        /// `inCall` stops the probe outright: during a call the engine holds
        /// the devices and the answer is both known and unchanging, so the
        /// work would be pure waste.
        func trackReadiness(input: String?, output: String?, inCall: Bool) {
            probedInput = .some(input)
            probedOutput = .some(output)
            readinessTimer?.invalidate()
            readinessTimer = nil
            guard !inCall else {
                inputReadiness = nil
                outputReadiness = nil
                return
            }
            probeReadiness()
            let timer = Timer.scheduledTimer(withTimeInterval: Self.readinessInterval, repeats: true)
            { [weak self] _ in
                Task { @MainActor in self?.probeReadiness() }
            }
            RunLoop.main.add(timer, forMode: .common)
            readinessTimer = timer
        }

        /// Stop probing — the settings pane went away, or a call started.
        func stopTrackingReadiness() {
            readinessTimer?.invalidate()
            readinessTimer = nil
        }

        /// One probe of both selected devices, off the main thread.
        private func probeReadiness() {
            guard case .some(let wantIn) = probedInput, case .some(let wantOut) = probedOutput
            else { return }
            let ourPID = getpid()
            queue.async { [weak self] in
                guard let self else { return }
                let ins = CoreAudioProbe.readiness(of: wantIn, input: true, ourPID: ourPID)
                let outs = CoreAudioProbe.readiness(of: wantOut, input: false, ourPID: ourPID)
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    if self.inputReadiness != ins { self.inputReadiness = ins }
                    if self.outputReadiness != outs { self.outputReadiness = outs }
                }
            }
        }

        /// Re-enumerate devices off the main thread and publish any change on main.
        /// Safe to call from any thread (the CoreAudio listener delivers on `queue`).
        nonisolated func refresh() {
            queue.async { [weak self] in
                guard let self else { return }
                let ins = self.session.inputs()
                let outs = self.session.outputs()
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    if self.inputs != ins { self.inputs = ins }
                    if self.outputs != outs { self.outputs = outs }
                }
            }
        }
    }
#endif

#if os(macOS)
    import AstarCore
    import CoreAudio
    import Foundation

    /// The CoreAudio half of the readiness check: presence, and who holds the
    /// device in hog mode. Opens nothing.
    ///
    /// Deliberately separate from `DeviceReadiness.classify`, which turns these
    /// facts into a verdict and is unit-tested. This part needs real hardware
    /// to exercise and so is kept as thin as it can be.
    enum CoreAudioProbe {
        /// Readiness of the device named `want`, or of the system default for
        /// that direction when `want` is nil.
        static func readiness(of want: String?, input: Bool, ourPID: pid_t) -> DeviceReadiness {
            guard let id = device(named: want, input: input) else {
                return DeviceReadiness.classify(present: false, hogOwner: nil, ourPID: ourPID)
            }
            return DeviceReadiness.classify(
                present: true, hogOwner: hogOwner(id, input: input), ourPID: ourPID)
        }

        private static func addr(
            _ selector: AudioObjectPropertySelector,
            _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal
        ) -> AudioObjectPropertyAddress {
            AudioObjectPropertyAddress(
                mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain)
        }

        /// The first device matching `want` with streams in this direction —
        /// "first" because that is what the engine's own name lookup does, so
        /// an ambiguous name resolves here exactly as it will at dial time
        /// (astar-9d41).
        private static func device(named want: String?, input: Bool) -> AudioDeviceID? {
            guard let want else { return defaultDevice(input: input) }
            var a = addr(kAudioHardwarePropertyDevices)
            var size: UInt32 = 0
            guard
                AudioObjectGetPropertyDataSize(
                    AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size) == noErr
            else { return nil }
            var ids = [AudioDeviceID](
                repeating: 0, count: Int(size) / MemoryLayout<AudioDeviceID>.size)
            guard
                AudioObjectGetPropertyData(
                    AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size, &ids) == noErr
            else { return nil }
            return ids.first { name(of: $0) == want && hasStreams($0, input: input) }
        }

        private static func defaultDevice(input: Bool) -> AudioDeviceID? {
            var a = addr(
                input
                    ? kAudioHardwarePropertyDefaultInputDevice
                    : kAudioHardwarePropertyDefaultOutputDevice)
            var id = AudioDeviceID(0)
            var size = UInt32(MemoryLayout<AudioDeviceID>.size)
            guard
                AudioObjectGetPropertyData(
                    AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size, &id) == noErr,
                id != 0
            else { return nil }
            return id
        }

        private static func name(of id: AudioDeviceID) -> String {
            var a = addr(kAudioObjectPropertyName)
            var size = UInt32(MemoryLayout<CFString?>.size)
            var cf: CFString?
            guard AudioObjectGetPropertyData(id, &a, 0, nil, &size, &cf) == noErr,
                let s = cf as String?
            else { return "" }
            return s
        }

        private static func hasStreams(_ id: AudioDeviceID, input: Bool) -> Bool {
            var a = addr(
                kAudioDevicePropertyStreams,
                input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput)
            var size: UInt32 = 0
            guard AudioObjectGetPropertyDataSize(id, &a, 0, nil, &size) == noErr else {
                return false
            }
            return size > 0
        }

        /// The pid holding hog mode, `-1` for nobody, or `nil` when the device
        /// refuses the query — aggregate and virtual devices commonly do, and
        /// that must not be reported as busy.
        private static func hogOwner(_ id: AudioDeviceID, input: Bool) -> pid_t? {
            var a = addr(
                kAudioDevicePropertyHogMode,
                input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput)
            var owner: pid_t = -1
            var size = UInt32(MemoryLayout<pid_t>.size)
            guard AudioObjectGetPropertyData(id, &a, 0, nil, &size, &owner) == noErr else {
                return nil
            }
            return owner
        }
    }
#endif
