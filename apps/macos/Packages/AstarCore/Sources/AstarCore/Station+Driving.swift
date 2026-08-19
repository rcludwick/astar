// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// Make the real `Station` satisfy `StationDriving` by mapping its `Snapshot`
/// into the testable `CallSnapshot`. (connectWT/setPTT/disconnect already match
/// the protocol, so they need no adapter.) This is the seam to the binding —
/// the testable logic lives in `CallSession`.
extension Station: StationDriving {
    public func readSnapshot() throws -> CallSnapshot {
        let s = try snapshot()
        return CallSnapshot(
            status: s.status, ptt: s.ptt, remotePTT: s.remotePTT,
            txDB: s.txDB, rxDB: s.rxDB, inputDB: s.inputDB, rttMS: s.rttMS,
            negotiatedFormat: s.negotiatedFormat,
            dtmfPlayed: s.dtmfPlayed, dtmfTotal: s.dtmfTotal,
            m17Available: s.m17Available, m17Active: s.m17Active
        )
    }

    public func readEvent() throws -> Event? {
        try nextEvent()
    }

    public func connect(dest: String, calling: String, secret: String?) throws {
        try connect(dest: dest, calling: calling, secret: secret, name: "astar")
    }
    // connectWT(destNode:address:) — the Advanced-options manual-address WT dial —
    // is now provided natively by the vendored Station (iax_station_connect_wt_addr,
    // iax-5991), so it satisfies StationDriving directly with no adapter here.
    // connectM17/m17Disconnect/setCodecDirs (iax-f2b8 Task 8) also match the
    // protocol verbatim (defaulted `port` on the vendored method is call-site
    // sugar only — it doesn't affect conformance), so they need no adapter either.
}

/// Errors from the fallback `NullStation` (no real engine available).
public enum NullStationError: Error { case noEngine }

/// A do-nothing station: always idle, commands are no-ops. Used as the app's
/// fallback when a real `Station` can't be constructed, and for SwiftUI previews.
public struct NullStation: StationDriving {
    public init() {}
    public func readSnapshot() throws -> CallSnapshot { .idle }
    public func readEvent() throws -> Event? { nil }
    public func connectWT(destNode: String) throws {}
    public func connectWT(destNode: String, address: String) throws {}
    public func connect(dest: String, calling: String, secret: String?) throws {}
    public func setPTT(_ on: Bool) throws {}
    public func sendDTMF(_ digit: Character) throws {}
    public func sendDTMF(sequence: String) throws {}
    public func cancelDTMF() throws {}
    public func disconnect() throws {}
    public func testMintToken() throws { throw NullStationError.noEngine }
    public func connectM17(host: String, port: UInt16, module: Character, callsign: String) throws {
    }
    public func m17Disconnect() throws {}
    public func setCodecDirs(_ dirs: [String]) throws {}
    public func listInputs() throws -> [String] { [] }
    public func listOutputs() throws -> [String] { [] }
    public func setDevices(input: String?, output: String?) throws {}
    public func setInputGain(_ gain: Float) throws {}
    public func setOutputGain(_ gain: Float) throws {}
    public func setCompression(_ on: Bool) throws {}
    public func setCompressionLevel(_ level: Float) throws {}
    public func setTxTrim(_ gain: Float) throws {}
    public func setNoiseReduction(_ on: Bool) throws {}
    public func setRxCompression(_ on: Bool) throws {}
    public func setRxCompressionLevel(_ level: Float) throws {}
    public func setSpectrumDecay(dbPerSecond: Float) throws {}
    public func monitorStart(input: String?) throws {}
    public func monitorStop() throws {}
    public func micSpectrum() throws -> [Float] { [] }
    public func txSpectrum() throws -> [Float] { [] }
    public func rxSpectrum() throws -> [Float] { [] }
    public func characterize(harmonicComb: Bool) throws -> String { "" }
    public func setMicProfile(_ json: String?) throws {}
}

extension CallSession {
    /// The `StationConfig` a real station is built from: portal credentials plus
    /// the codec policy, which is always `prefer_slin16` — wideband is always on
    /// (astar-e542) and the node decides the narrowband fallback in IAX2
    /// negotiation. Pure, so tests can prove the wiring without constructing an
    /// engine (astar-eb6c).
    public static func stationConfig(credentials: Credentials?, audio: AudioSettings)
        -> StationConfig
    {
        var config = StationConfig()
        if let c = credentials {
            config.portalUser = c.portalUser
            config.portalPass = c.portalPass
            config.portalNode = c.portalNode
        }
        config.codecPolicy = audio.codecPolicyString
        return config
    }

    /// Build a real `Station` from optional portal credentials + the persisted
    /// audio settings (for construction-time knobs like the codec policy),
    /// returning it plus whether the WT path is available. Falls back to
    /// `NullStation` if the engine can't be constructed (so the UI always has a
    /// session). The password is consumed into the config here and never retained.
    public static func makeStation(credentials: Credentials?, audio: AudioSettings) -> (
        station: StationDriving, hasCredentials: Bool
    ) {
        let config = stationConfig(credentials: credentials, audio: audio)
        if let station = try? Station(config: config) {
            return (station, credentials != nil)
        }
        NSLog("[astar] makeStation: real Station construction FAILED — using NullStation")
        return (NullStation(), false)
    }

    /// Build the app's live session, loading any saved AllStar credentials from
    /// the store. Credentials are required to dial: without them `connect`
    /// refuses with `ConnectError.needsAccount` (see `CallSession.connect`).
    ///
    /// The audio settings are read BEFORE the station is built — the codec
    /// policy (always prefer_slin16, astar-e542) only applies at engine
    /// construction (astar-eb6c).
    public static func live(
        store: CredentialStore = KeychainCredentialStore(),
        audioStore: AudioSettingsStore = UserDefaultsAudioSettingsStore()
    ) -> CallSession {
        let audio = audioStore.load()
        let credentials = store.load()
        let (station, hasCredentials) = makeStation(credentials: credentials, audio: audio)
        // m17: whether a Codec 2 backend resolved (astar-8c4d). Worth a launch
        // line — "M17 doesn't work" is otherwise indistinguishable from "M17 is
        // there but the codec never loaded", and that was the whole failure mode
        // before Codec 2 was linked in.
        NSLog(
            "[astar] live: station=%@ hasCredentials=%@ codecPolicy=%@ m17=%@",
            station is NullStation ? "NULL" : "real", hasCredentials ? "yes" : "no",
            audio.codecPolicyString,
            (try? station.readSnapshot().m17Available) == true ? "available" : "unavailable")
        // `credentials` is also passed through for the M17 callsign prefill
        // (astar-c2e5/iax-f2b8 Task 8) — many hams reuse their portal login as
        // their callsign; see `CallSession.callsignPrefill(from:)`.
        let session = CallSession(
            station: station, hasCredentials: hasCredentials, credentials: credentials)
        session.applyAudioSettings(audio)  // restore saved devices + gains
        return session
    }
}
