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
            if let listenerBlock {
                AudioObjectRemovePropertyListenerBlock(
                    AudioObjectID(kAudioObjectSystemObject), &address, queue, listenerBlock)
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
