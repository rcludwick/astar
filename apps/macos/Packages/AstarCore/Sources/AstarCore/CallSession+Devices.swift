// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation

/// Audio device selection + gain control, layered onto `CallSession`.
///
/// These are thin pass-throughs to the underlying station. Device enumeration
/// degrades to an empty list if the station has no engine (e.g. `NullStation`
/// or a station that hasn't been brought up), so callers can bind a `Picker`
/// without guarding. Selecting a device or adjusting gain takes effect on the
/// active call, per the station's contract.
extension CallSession {
    /// Capture (input/mic) device names. `[]` if enumeration is unavailable.
    public func inputs() -> [String] {
        (try? station.listInputs()) ?? []
    }

    /// Playback (output/speaker) device names. `[]` if enumeration is unavailable.
    public func outputs() -> [String] {
        (try? station.listOutputs()) ?? []
    }

    /// Select capture/playback devices. A `nil` direction keeps the system
    /// default for that direction.
    ///
    /// The speaker monitor must never share the output's device: if the new
    /// output is the saved monitor device, the monitor is cleared (saved and on
    /// the station) and this returns `true`, so the UI can say "Monitor turned
    /// off: it was the same device as the output." Otherwise returns `false`.
    @discardableResult
    public func selectDevices(input: String?, output: String?) throws -> Bool {
        try station.setDevices(input: input, output: output)
        var s = audioStore.load()
        guard let monitor = s.monitor,
            MonitorPickerModel.reconcile(monitor: monitor, newOutput: output) == nil
        else { return false }
        s.monitor = nil
        audioStore.save(s)
        try? station.setMonitorDevice(nil)
        return true
    }

    /// Set the input (TX/mic) gain multiplier (the station clamps to `0...4`).
    public func setInputGain(_ gain: Float) throws {
        try station.setInputGain(gain)
    }

    /// Set the output (RX/speaker) gain multiplier. Engine range `0.0...4.0`
    /// (iax-a4e7); the Quick-settings UI only offers `1.0...4.0` — the
    /// internal half-duplex mute still uses this API directly with `0`.
    ///
    /// Records the value as the user's intended RX gain so half-duplex
    /// suppression (astar-eaab) can restore it on unkey. While RX is currently
    /// muted for half-duplex TX, the new gain is remembered but NOT pushed to the
    /// station — that would un-mute mid-transmit and reintroduce the echo; the
    /// unkey edge restores it.
    public func setOutputGain(_ gain: Float) throws {
        desiredOutputGain = gain
        guard !rxMutedForHalfDuplex else { return }
        try station.setOutputGain(gain)
    }

    /// Choose the speaker monitor's device (`nil` = off). Does not persist:
    /// the caller saves `AudioSettings.monitor`, as it does for the output.
    public func selectMonitor(_ device: String?) throws {
        try station.setMonitorDevice(device)
    }

    /// Set the speaker monitor's volume (the engine clamps to `0...4`).
    public func setMonitorGain(_ gain: Float) throws {
        try station.setMonitorGain(gain)
    }

    /// Mute or unmute the speaker monitor. Live-only: never saved.
    public func setMonitorMuted(_ muted: Bool) throws {
        try station.setMonitorMuted(muted)
        if monitorMuted != muted { monitorMuted = muted }
    }

    /// What the monitor is doing. `.playing` means the monitor stream is up,
    /// NOT that a call is live: an AllStar bus stays open after hangup.
    public func monitorStatus() -> MonitorStatus {
        station.monitorState()
    }

    /// Re-read `monitorStatus()` into the published `monitorState`, only on
    /// a change. The poll tick calls this; so does the UI right after it
    /// chooses a monitor device, because the poll pauses off the call pane.
    public func refreshMonitorState() {
        let state = station.monitorState()
        if monitorState != state { monitorState = state }
    }

    /// Send the saved monitor device to the engine again. A device that was
    /// missing when chosen stays unavailable until it is set again, so this
    /// runs whenever the output device list changes. A monitor already
    /// playing is left alone only while its device is still listed (the engine
    /// may not have noticed an unplug yet). No saved monitor, or one equal to
    /// the output, turns the monitor off.
    public func reapplyMonitor() {
        let s = audioStore.load()
        guard let device = MonitorPickerModel.reconcile(monitor: s.monitor, newOutput: s.output)
        else {
            try? station.setMonitorDevice(nil)
            return
        }
        if case .playing = station.monitorState(), outputs().contains(device) { return }
        try? station.setMonitorDevice(device)
    }

    /// Send a single DTMF digit on the active call as one fixed-duration in-band
    /// tone. A thin passthrough to `station.sendDTMF`; the ~250 ms tone duration
    /// is owned by the binding. Kept for engine parity — the dialpad now sends
    /// through `sendDTMF(sequence:)` (astar-7d21), even for one digit (the
    /// sequence path pumps its own completion; see the astar-3f03 findings).
    ///
    /// Throws while idle or for a non-dialer character so the caller can degrade
    /// gracefully.
    public func sendDTMF(_ digit: Character) throws {
        try station.sendDTMF(digit)
    }

    /// Send a composed multi-digit DTMF command as one engine-timed sequence —
    /// the dialpad's Send path (astar-7d21). Thin passthrough to
    /// `station.sendDTMF(sequence:)`; validation is all-or-nothing engine-side
    /// (nothing sent on error), so the caller keeps the command in the field
    /// when this throws. Progress arrives via the published
    /// `dtmfPlayed`/`dtmfTotal`.
    public func sendDTMF(sequence: String) throws {
        try station.sendDTMF(sequence: sequence)
    }

    /// Drop the un-played remainder of the playing command — the dialpad's
    /// Stop button (astar-7d21). Safe when nothing is playing.
    public func cancelDTMF() throws {
        try station.cancelDTMF()
    }

    /// Set the live spectrum peak-hold decay in dB/second — the app-global
    /// "Spectrum decay" preference (`ui.spectrumDecayDbPerSec`, default 100). A
    /// thin passthrough to `station.setSpectrumDecay`; scrubs every analyzer that
    /// is currently live (mic monitor, active-call TX/RX) at once. Idempotent and
    /// cheap, so it's safe to re-assert whenever a new analyzer appears (a call
    /// reaching `.answered`, the mic monitor starting) since the engine setter
    /// only affects analyzers that exist at the moment it's called.
    ///
    /// Best-effort: errors are swallowed (a decay tweak must never break a call).
    public func setSpectrumDecay(_ dbPerSecond: Float) {
        spectrumDecayDbPerSec = dbPerSecond
        try? station.setSpectrumDecay(dbPerSecond: dbPerSecond)
    }
}
