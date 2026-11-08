// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

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

    // MARK: Vocoder trust

    /// Replace the vocoder trust list: the USB serials of the AMBE sticks a
    /// D-Star/YSF/NXDN/DMR connect may open. Empty trusts nothing, and the
    /// engine then opens no vocoder port at all. Mirrors
    /// `Station.setVocoderTrust(_:)` 1:1.
    func setVocoderTrust(_ serials: [String]) throws

    /// Choose D-Star's vocoder: the dongle (the default) or the software
    /// vocoder. D-Star only. Mirrors `Station.setDStarVocoder(_:)` 1:1.
    func setDStarVocoder(_ vocoder: DStarVocoder) throws

    // MARK: D-Star (iax-4c8e)

    /// Connect to a D-Star reflector over DExtra and open a full-transceive
    /// session on `module`, mutually exclusive with an IAX2 call and an M17
    /// session. Mirrors `Station.connectDStar(host:port:module:callsign:
    /// reflectorCallsign:)` 1:1.
    ///
    /// This opens the vocoder `setDStarVocoder(_:)` chose — a ThumbDV AMBE
    /// dongle by default — and fails when it is not there. Nothing falls back
    /// to the other vocoder, which is why callers gate on
    /// `CallSnapshot.dstarAvailable` rather than calling speculatively.
    ///
    /// `reflectorCallsign` names the destination the way the directories list
    /// it ("XRF836") and fills the transmitted header's RPT1/RPT2. `nil`
    /// derives it from `host`'s first DNS label, which is right for a
    /// reflector reached by its published hostname and wrong for a bare IP —
    /// so the directory path always passes it explicitly.
    ///
    /// Blocks for a serial scan plus a per-port dongle init before it touches
    /// the network. Call it off the main thread.
    func connectDStar(
        host: String, port: UInt16, module: Character, callsign: String,
        reflectorCallsign: String?
    ) throws
    /// Disconnect the live D-Star session, if any. Idempotent — a no-op while
    /// idle. Mirrors `Station.dstarDisconnect()` 1:1.
    func dstarDisconnect() throws
    /// The live D-Star session's own state — link, last-heard talker, slow-data
    /// text — or `nil` when none is active. Mirrors `Station.dstarState()` 1:1.
    ///
    /// Costlier than `readSnapshot()`: it crosses the ABI with a buffer and
    /// parses JSON. Poll the snapshot for meters; call this at UI rate.
    func dstarState() throws -> DStarState?
    /// The live M17 session's own state — link, whether a stream is arriving,
    /// PTT, and the last-heard talker — or `nil` when none is active. Mirrors
    /// `Station.m17State()` 1:1.
    ///
    /// Costlier than `readSnapshot()`: it crosses the ABI with a buffer and
    /// parses JSON. Poll the snapshot for meters; call this at UI rate.
    func m17State() throws -> M17State?
    /// Station.heard() 1:1.
    func heard() throws -> [HeardEntry]

    // MARK: Recording

    /// Start recording the live call or digital session into `directory`.
    /// `label` is the `<network> <node or reflector>` part of each file name
    /// and `utcOffsetSeconds` puts local time in it. Mirrors
    /// `Station.recordStart(directory:layout:frames:label:utcOffsetSeconds:)`
    /// 1:1. Creates files; call it off the main thread.
    func recordStart(
        directory: String, layout: RecordingLayout, frames: Bool, label: String,
        utcOffsetSeconds: Int32
    ) throws
    /// Stop the recording, if one runs, and finish its files. Idempotent.
    /// Joins the recorder; call it off the main thread.
    func recordStop() throws
    /// The recorder's state. Crosses the ABI with JSON, so the session reads
    /// it at most twice a second.
    func recordState() throws -> RecordState

    // MARK: Packet log

    /// Turn the packet log on or off, or point it at another folder. Mirrors
    /// `Station.packetLogConfigure(enabled:directory:maxBytes:utcOffsetSeconds:)`
    /// 1:1. Joins the writer thread; call it off the main thread.
    func packetLogConfigure(
        enabled: Bool, directory: URL?, maxBytes: Int64, utcOffsetSeconds: Int32
    ) throws
    /// Tell the engine the captured call ended, so the log stops. Mirrors
    /// `Station.packetLogCallEnded()`. Joins the writer thread; call it off
    /// the main thread.
    func packetLogCallEnded() throws
    /// The packet log's state. Cheap, and never waits on a change in progress.
    func packetLogState() throws -> PacketLogState

    // MARK: System Fusion

    /// Link to a YSFReflector and decode the audio on it, mutually exclusive
    /// with every other network. Mirrors `Station.connectYSF(host:callsign:
    /// options:)` 1:1 — `host` is `host:port`, because YSF standardises no
    /// port and every directory row carries its own.
    ///
    /// RECEIVE ONLY: there is no YSF transmit path, so nothing should offer
    /// PTT while a link is live.
    ///
    /// HARDWARE-ONLY, from the same ThumbDV D-Star needs, so callers gate on
    /// `CallSnapshot.ysfAvailable` rather than calling speculatively. Blocks
    /// for a serial scan plus a per-port dongle init; call it off the main
    /// thread.
    func connectYSF(host: String, callsign: String, options: String?) throws
    /// Disconnect the live YSF link, if any. Idempotent — a no-op while idle.
    func ysfDisconnect() throws
    /// The live YSF link's own state — link, last heard, whether a
    /// transmission is in progress, and any mode astar could not decode — or
    /// `nil` when none is active.
    ///
    /// Costlier than `readSnapshot()`: it crosses the ABI with a buffer and
    /// parses JSON. Poll the snapshot for meters; call this at UI rate.
    func ysfState() throws -> YSFState?

    // MARK: NXDN

    /// Link to an NXDNReflector and decode the audio on it, mutually
    /// exclusive with every other network. Mirrors
    /// `Station.connectNXDN(host:callsign:radioID:talkgroup:)` 1:1 — `host`
    /// is `host:port`, because NXDN publishes a port per reflector.
    ///
    /// `radioID` is the operator's own NXDN number and `talkgroup` the TG to
    /// join; both are 16-bit because NXDN addresses stations by number, not
    /// by callsign, and a reflector relays exactly one talkgroup.
    ///
    /// RECEIVE ONLY: there is no NXDN transmit path, so nothing should offer
    /// PTT while a link is live.
    ///
    /// HARDWARE-ONLY, from the same ThumbDV D-Star and YSF need, so callers
    /// gate on `CallSnapshot.nxdnAvailable` rather than calling
    /// speculatively. Blocks for a serial scan plus a per-port dongle init;
    /// call it off the main thread.
    func connectNXDN(host: String, callsign: String, radioID: UInt16, talkgroup: UInt16) throws
    /// Disconnect the live NXDN link, if any. Idempotent — a no-op while idle.
    func nxdnDisconnect() throws
    /// The live NXDN link's own state — link, the last id heard, and whether
    /// a transmission is in progress — or `nil` when none is active.
    ///
    /// Costlier than `readSnapshot()`: it crosses the ABI with a buffer and
    /// parses JSON. Poll the snapshot for meters; call this at UI rate.
    func nxdnState() throws -> NXDNState?

    // MARK: DMR

    /// Link to a DMR master's talkgroup and decode the audio on it, mutually
    /// exclusive with every other network. Mirrors
    /// `Station.connectDMR(system:host:port:radioID:callsign:talkgroup:timeslot:password:)`
    /// 1:1.
    ///
    /// A DMR target is FOUR things — `system` (which network's login),
    /// `host`/`port` (which master), `talkgroup` (which room) and `timeslot`
    /// (which slot) — because a talkgroup number names nothing on its own:
    /// TG 91 exists on several of these networks and is a different room on
    /// each. `radioID` is the operator's 24-bit radioid.net registration, and
    /// `callsign` rides only in the login, since a `DMRD` frame carries a
    /// number and no callsign at all.
    ///
    /// `password` is the master password — a per-network SECRET. It is an
    /// in-arg and nothing else: it is not stored on the session, never
    /// published, never logged, and there is no field on a snapshot, an event
    /// or an error it could occupy.
    ///
    /// RECEIVE ONLY: there is no DMR transmit path, so nothing should offer
    /// PTT while a link is live.
    ///
    /// HARDWARE-ONLY, from the same ThumbDV D-Star, YSF and NXDN need, so
    /// callers gate on `CallSnapshot.dmrAvailable` rather than calling
    /// speculatively. Blocks for a serial scan, a per-port dongle init and
    /// then a multi-round-trip login; call it off the main thread.
    func connectDMR(
        system: String, host: String, port: UInt16, radioID: UInt32, callsign: String,
        talkgroup: UInt32, timeslot: UInt8, password: String
    ) throws
    /// Disconnect the live DMR link, if any. Idempotent — a no-op while idle.
    func dmrDisconnect() throws
    /// The live DMR link's own state — link stage, why a failed login failed,
    /// the last id heard, and whether a transmission is in progress — or `nil`
    /// when none is active. Credential-free: the master password is nowhere in
    /// it.
    ///
    /// Costlier than `readSnapshot()`: it crosses the ABI with a buffer and
    /// parses JSON. Poll the snapshot for meters; call this at UI rate.
    func dmrState() throws -> DMRState?

    // MARK: EchoLink

    /// Start logging in to the EchoLink directory as `callsign`, trying
    /// `servers` in turn. Returns at once; `echolinkState()` shows the login
    /// finish or fail. Mirrors `Station.echolinkLogin(callsign:password:
    /// servers:)` 1:1.
    ///
    /// `password` is a SECRET and an in-arg only: read from the Keychain at
    /// the moment of the login and handed straight here, never stored on the
    /// session, published, exported or logged.
    func echolinkLogin(callsign: String, password: String, servers: [String]) throws
    /// Hang up any EchoLink call and log off the directory.
    ///
    /// BLOCKS: the logoff waits on any directory exchange already in flight,
    /// which can take over a minute. Never call it on the main thread.
    func echolinkLogout() throws
    /// Call an EchoLink station by node number or listed callsign. Calling is
    /// always a person's action. Opens sockets and the output audio device
    /// before it returns, so call it off the main thread.
    func echolinkConnect(_ target: String) throws
    /// Hang up the EchoLink call, if any; the login stays up. Idempotent.
    func echolinkDisconnect() throws
    /// Send one line of text chat to the station on the call.
    func echolinkSendChat(_ text: String) throws
    /// Set the location text other stations see beside the callsign.
    func echolinkSetLocation(_ text: String) throws
    /// Where the login and the call stand, or `nil` when logged out with
    /// nothing to say. Has no password field. Crosses the ABI with a buffer
    /// and parses JSON: call it at UI rate.
    func echolinkState() throws -> EchoLinkState?
    /// The directory's latest station list; empty when not logged in.
    func echolinkStations() throws -> [EchoLinkStationEntry]
    /// Ask the directory for a fresh station list now. `true` when it was
    /// asked; `false` when the last list came less than 30 s ago and nothing
    /// was asked. Throws when not logged in. Sends a request to the
    /// directory, so call it off the main thread.
    @discardableResult
    func echolinkRefreshList() throws -> Bool

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
    /// Set the TX trim gain (0…4, engine clamps; 1.0 = unity): the always-on
    /// final TX gain stage after compression. Takes effect on the live capture lane.
    func setTxTrim(_ gain: Float) throws
    func setNoiseReduction(_ on: Bool) throws
    /// Set the neural noise-reduction strength (0…1; 1.0 = full, 0.0 =
    /// bypass). No effect while the classical filter+gate chain is running.
    func setDenoiseStrength(_ level: Float) throws

    // Output (RX/speaker) processing toggle. Mirrors `Station`'s methods 1:1
    // (iax-a4e7): automatic leveling of the RECEIVED audio, reusing the
    // mic-path compressor on the output bus. Shared across networks —
    // listener-side, not part of the per-network M17 TX override. Takes
    // effect immediately on the live (and next) call's output bus.
    func setRxCompression(_ on: Bool) throws
    /// Set the RX/output compression strength (0…1). Takes effect immediately
    /// on the live output bus.
    func setRxCompressionLevel(_ level: Float) throws

    /// Configure the RX jitter buffer (iax-rxjb): whether received audio is
    /// played out of the adaptive buffer at all, and the window
    /// (`minMs`...`maxMs`, ms) its depth may live in. Mirrors `Station`'s
    /// method 1:1 — both bounds clamped to `0...500` and a `maxMs` below
    /// `minMs` raised to meet it, so a setting is repaired, never refused.
    /// Takes effect immediately, mid-call. Read the effective values back
    /// from `CallSnapshot`'s `rxJitterBuffer*` fields.
    func setRxJitter(enabled: Bool, minMs: UInt32, maxMs: UInt32) throws

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
    /// Two ways to say what counts as a tone, and `thresholdDbfs` wins:
    /// `peakMarginDb` is RELATIVE (how far above the measured noise floor a bin
    /// must stand), `thresholdDbfs` ABSOLUTE (a level in the same dBFS
    /// `micSpectrum()` reports, so what is drawn above the line is what the
    /// detector catches). `nil` for either leaves the engine's own default in
    /// charge.
    func characterize(harmonicComb: Bool, peakMarginDb: Float?, thresholdDbfs: Float?) throws
        -> String
    func setMicProfile(_ json: String?) throws
}

extension StationDriving {
    /// Stations that have no engine (and test fakes that do not care) accept
    /// the choice and do nothing with it. The real `Station` implements it.
    public func setDStarVocoder(_ vocoder: DStarVocoder) throws {}
}
