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
    public func selectDevices(input: String?, output: String?) throws {
        try station.setDevices(input: input, output: output)
    }

    /// Set the input (TX/mic) gain multiplier (the station clamps to `0...2`).
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
