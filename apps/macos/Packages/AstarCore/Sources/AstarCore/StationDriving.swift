// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation

/// The slice of the AstarStation `Station` that `CallSession` drives. Abstracted
/// to a protocol so the session can be tested with a fake (no audio/network).
public protocol StationDriving {
    func readSnapshot() throws -> CallSnapshot
    func readEvent() throws -> Event?
    /// Authenticated AllStar WebTransceiver dial (needs portal creds in the station).
    func connectWT(destNode: String) throws
    /// WebTransceiver dial to an explicit `address` ("host:port", IP or hostname),
    /// bypassing registrar resolution. Mints the same WT token (auth unchanged) but
    /// dials `address` instead of the node's registrar-advertised IP — for reaching
    /// a node whose published address is unroutable from here (e.g. your own node on
    /// localhost / LAN, NAT-hairpin case). Pending IAX binding iax-5991.
    func connectWT(destNode: String, address: String) throws
    /// Generic IAX2 dial (the guest/unauthenticated path, e.g. the parrot).
    func connect(dest: String, calling: String, secret: String?) throws
    func setPTT(_ on: Bool) throws
    /// Send a single DTMF digit to the active call as one fixed-duration in-band
    /// tone (the binding owns the ~250 ms duration). Throws while idle or on an
    /// invalid digit. Mirrors `Station.sendDTMF(_:)` 1:1. Kept for engine
    /// parity; the dialpad sends via `sendDTMF(sequence:)` (astar-7d21).
    func sendDTMF(_ digit: Character) throws
    /// Send a multi-digit DTMF command as one engine-timed sequence
    /// (astar-7d21 / iax-4b7a): ~250 ms tone + ~100 ms gap per digit.
    /// All-or-nothing validation (any non-DTMF character throws, nothing
    /// sent); throws while idle or while a sequence is already playing. The
    /// queue advances on snapshot polls; progress arrives via
    /// `CallSnapshot.dtmfPlayed`/`dtmfTotal`. Mirrors
    /// `Station.sendDTMF(sequence:)` 1:1.
    func sendDTMF(sequence: String) throws
    /// Drop the un-played remainder of a `sendDTMF(sequence:)` command — the
    /// Stop button. Safe when nothing is playing. Mirrors
    /// `Station.cancelDTMF()` 1:1.
    func cancelDTMF() throws
    /// Set the live spectrum peak-hold decay in dB/second (the engine clamps,
    /// default 100). Scrubs every analyzer that is currently live (mic monitor,
    /// active-call TX/RX) at once. Mirrors `Station.setSpectrumDecay(dbPerSecond:)`.
    func setSpectrumDecay(dbPerSecond: Float) throws
    func disconnect() throws
    /// Validate WebTransceiver token minting (portal login + mint + discard)
    /// without placing a call. Throws on failure (bad password, unknown node,
    /// network). Needs portal creds configured in the station.
    func testMintToken() throws

    // MARK: M17 (iax-f2b8 Task 8)

    /// Connect to an M17 reflector: resolves `host`/`port` and opens a full-transceive
    /// session on `module`, mutually exclusive with an active IAX2 call. Mirrors
    /// `Station.connectM17(host:port:module:callsign:)` 1:1.
    func connectM17(host: String, port: UInt16, module: Character, callsign: String) throws
    /// Disconnect the live M17 session, if any. Idempotent — a no-op while idle.
    /// Mirrors `Station.m17Disconnect()` 1:1.
    func m17Disconnect() throws
    /// Set extra directories to search for a runtime `libcodec2`, ahead of the hard-coded
    /// system paths — e.g. to point at an app bundle's own copy of the library. Call before
    /// `connectM17`; it does not affect a session already in progress. Mirrors
    /// `Station.setCodecDirs(_:)` 1:1.
    func setCodecDirs(_ dirs: [String]) throws

    // Audio device selection + gain. Mirrors `Station`'s methods 1:1; a `nil`
    // device selects the system default for that direction.
    func listInputs() throws -> [String]
    func listOutputs() throws -> [String]
    func setDevices(input: String?, output: String?) throws
    func setInputGain(_ gain: Float) throws
    /// Set the output (RX/speaker) gain multiplier. Engine range `0.0...4.0`
    /// (iax-a4e7); the Quick-settings UI only offers `1.0...4.0` — the
    /// internal half-duplex mute still uses this API directly with `0`.
    func setOutputGain(_ gain: Float) throws

    // Mic processing toggles. Mirror `Station`'s methods 1:1; take effect on the
    // live (and next) call's capture lane.
    func setCompression(_ on: Bool) throws
    /// Set the compression strength (0…1). Takes effect on the live capture lane.
    func setCompressionLevel(_ level: Float) throws
    /// Set the TX trim gain (0…2, engine clamps; 1.0 = unity): the always-on
    /// final TX gain stage after compression. Takes effect on the live capture lane.
    func setTxTrim(_ gain: Float) throws
    func setNoiseReduction(_ on: Bool) throws

    // Output (RX/speaker) processing toggle. Mirrors `Station`'s methods 1:1
    // (iax-a4e7): automatic leveling of the RECEIVED audio, reusing the
    // mic-path compressor on the output bus. Shared across networks —
    // listener-side, not part of the per-network M17 TX override. Takes
    // effect immediately on the live (and next) call's output bus.
    func setRxCompression(_ on: Bool) throws
    /// Set the RX/output compression strength (0…1). Takes effect immediately
    /// on the live output bus.
    func setRxCompressionLevel(_ level: Float) throws

    // Mic characterization (engine FFI, vendored in AstarStation). Monitor opens the
    // mic with no call; spectrum is poll-only (~20 Hz); characterize returns opaque
    // JSON; setMicProfile applies it (or clears with nil).
    func monitorStart(input: String?) throws
    func monitorStop() throws
    func micSpectrum() throws -> [Float]
    /// Live TX/RX voice-band spectra (dBFS bins) for the in-call overlaid FFT;
    /// poll-only (~20 Hz). Vendored Station exposes these; NullStation returns [].
    func txSpectrum() throws -> [Float]
    func rxSpectrum() throws -> [Float]
    func characterize(harmonicComb: Bool) throws -> String
    func setMicProfile(_ json: String?) throws
}
