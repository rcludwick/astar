// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// Station.swift — an idiomatic Swift wrapper over the astar-sys
// poll+snapshot C-ABI (crates/astar-sys/include/astar.h).
//
// Design contract (matches the C-ABI):
//
//   * Poll + snapshot only. There are NO C function-pointer callbacks into
//     Swift. The app drives the call by polling `snapshot()` / `nextEvent()`.
//   * Secret-free. `secret` is only ever a connect / init argument. It is never
//     stored on the `Station`, never returned from a Snapshot/Event, never in
//     any `StationError`, and never in any `description`.
//   * Vendor-neutral. `connect(...)` is the generic IAX2 path; `connectWT(...)`
//     is the AllStar Web-Transceiver convenience.

import CAstarStation
import Foundation

// MARK: - Enums (mirror IaxStatus / IaxEventKind)

/// Call lifecycle status. Mirrors the C `IaxStatus` enum.
public enum IaxStatus: Int32, Sendable {
    /// No call in progress.
    case idle = 0
    /// NEW sent, awaiting answer.
    case dialing = 1
    /// Peer answered; media flowing.
    case answered = 2
    /// Call ended (normal hangup or failed dial).
    case hangup = 3
}

/// The kind of a drained lifecycle event. Mirrors the C `IaxEventKind` enum.
public enum IaxEventKind: Int32, Sendable {
    /// No event was queued.
    case none = 0
    /// The peer answered; media is flowing.
    case answered = 1
    /// The remote end keyed/unkeyed (see `Event.remotePTT`).
    case remotePTT = 2
    /// The call ended (normal hangup or failed dial).
    case hangup = 3
    /// The operating mode changed (WT ↔ Node). Read the new mode via `mode()`.
    case modeChanged = 4
    /// An inbound call is ringing (Node/Manual). Read the caller id via
    /// `incomingFrom()`.
    case incoming = 5
    /// Outbound node registration succeeded. Secret-free.
    case registered = 6
    /// Outbound node registration failed. Secret-free (no reason carried).
    case registerFailed = 7
}

/// Top-level operating mode. Mirrors the C `IaxMode` enum.
public enum Mode: Int32, Sendable {
    /// Dial-out Web-Transceiver client (default).
    case wt = 0
    /// Inbound IAX2 node (accept calls, bridge to the local handset).
    case node = 1
}

/// How `sendDTMF(_:)` emits digits on the active call. Mirrors the C
/// `IaxDtmfMode` enum (iax-7fff).
public enum DtmfMode: Int32, Sendable {
    /// Synthesize the dual-tone waveform into the call's TX audio path
    /// (in-band; default) — for nodes/paths that only decode audio tones.
    case inBand = 0
    /// Send out-of-band IAX2 protocol DTMF frames (DTMF BEGIN/END) — what
    /// Asterisk/AllStar expects by default. (Named `protocolFrames` because
    /// `protocol` is a Swift keyword; maps to the C `Protocol` case.)
    case protocolFrames = 1
}

/// How a node answers inbound calls. Mirrors the C `IaxAnswerPolicy` enum.
public enum AnswerPolicy: Int32, Sendable {
    /// Auto-accept the inbound offer and bridge it to the local handset.
    case auto = 0
    /// Surface the offer as an `.incoming` event; the operator then calls
    /// `answer()` / `reject()`.
    case manual = 1
}

/// Inbound-authentication policy for a node listener. Mirrors the C
/// `IaxAuthPolicy` enum.
public enum AuthPolicy: Int32, Sendable {
    /// Every inbound NEW must authenticate (unknown user → REJECT).
    case required = 0
    /// Challenge only if the peer's username maps to a held credential.
    case optional = 1
    /// Never challenge (accept anonymous). Permissive — dev dial-in only.
    case off = 2
}

/// Which mic noise-reduction chain is live, as reported by
/// `Snapshot.denoiseChain`. Mirrors the C-ABI `IaxDenoiseChain` and
/// `astar_audio::DenoiseChain`; the raw values are part of the published
/// interface, so add cases at the end.
///
/// This is read-only and reported rather than inferred. The neural stage
/// needs a 48 kHz capture device, and a device that cannot offer one
/// silently falls back to `.filterGate` — which without this is
/// indistinguishable from a bug.
public enum DenoiseChain: UInt32, Sendable, CustomStringConvertible {
    /// No mic lane is open, so no chain is running.
    case notCapturing = 0
    /// Capturing, but noise reduction is switched off.
    case off = 1
    /// Neural (RNNoise) at device rate, hum filter after it, no gate.
    case neural = 2
    /// The classical hum filter + noise gate. Either the device could not
    /// give 48 kHz, or `ASTAR_MIC_DENOISE=legacy` asked for it.
    case filterGate = 3

    public var description: String {
        switch self {
        case .notCapturing: return "Not capturing"
        case .off: return "Off"
        case .neural: return "Neural"
        case .filterGate: return "Filter + gate"
        }
    }
}

/// Negotiated voice codec of the active call, as reported by
/// `Snapshot.negotiatedFormat`. Raw values are the IAX2 format bits carried
/// across the C-ABI (`IaxState.negotiated_format`); `0` = none/unknown, which
/// the snapshot surfaces as `nil` rather than a case here.
public enum VoiceFormat: UInt32, Sendable, CustomStringConvertible {
    /// G.711 µ-law (8 kHz) — the AllStar default.
    case g711u = 4
    /// G.711 A-law (8 kHz).
    case g711a = 8
    /// 16-bit signed linear PCM, 8 kHz.
    case slin = 64
    /// 16-bit signed linear PCM, 16 kHz — wideband (iax-4348).
    case slin16 = 32768

    /// Display-ready name, e.g. for a codec badge in the call UI.
    public var description: String {
        switch self {
        case .g711u: return "µ-law (8 kHz)"
        case .g711a: return "A-law (8 kHz)"
        case .slin: return "slin (8 kHz)"
        case .slin16: return "slin16 (16 kHz wideband)"
        }
    }
}

// MARK: - Value types returned by the poll surface

/// Latest call state from `Station.snapshot()`.
///
/// Secret-free by construction: there is deliberately no secret/password field.
/// Safe to print/log.
public struct Snapshot: Sendable, Equatable {
    /// Current lifecycle status.
    public let status: IaxStatus
    /// Local transmit (PTT) state.
    public let ptt: Bool
    /// Remote-keyed state.
    public let remotePTT: Bool
    /// TX (transmitted, post-DSP) level in dBFS, `-60.0...0.0`. Meters only
    /// while keyed; floors to -60 when unkeyed.
    public let txDB: Float
    /// RX (decoded node audio) level in dBFS, `-60.0...0.0`.
    public let rxDB: Float
    /// Continuous mic INPUT peak in dBFS, `-60.0...0.0`, metered even while
    /// unkeyed (post-gain, pre-noise-reduction) — for level meters. VOX keys
    /// from ``voxDB`` instead, which a single click cannot push up.
    public let inputDB: Float
    /// Smoothed round-trip estimate in milliseconds, or `nil` if unknown.
    public let rttMS: Int?
    /// Current top-level operating mode (WT dial-out vs inbound Node).
    public let mode: Mode
    /// Cumulative voice-ts-ladder re-anchors (>80 ms TX-clock drift events).
    /// A growing value signals choppy TX. A plain health counter, credential-free.
    public let txReanchors: UInt64
    /// Cumulative cpal capture overruns (dropped input buffers — holes in the
    /// captured mic PCM) on the active call's routed mic. The lead suspect for
    /// choppy TX; `0` when monitor-only. A plain health counter, credential-free.
    public let txCaptureOverruns: UInt64
    /// Cumulative RX underruns on the active call's output bus: device
    /// callbacks that got no audio at all while somebody was still talking.
    /// The receive-side counterpart of `txCaptureOverruns` — the number that
    /// grows while received audio stutters. `0` when idle.
    public var rxUnderruns: UInt64 = 0
    /// Estimated network jitter on the active call's receive path, in ms.
    /// `0` when idle, or with the jitter buffer switched off.
    public var rxJitterMS: UInt32 = 0
    /// Current RX jitter-buffer depth in ms: how much received audio is being
    /// held back to ride out the network. `0` when the buffer isn't running.
    public var rxJitterBufferDepthMS: UInt32 = 0
    /// Frames the RX jitter buffer expected and did not play — usually an
    /// interpolation over a frame that never arrived, but it also counts one
    /// the buffer chose to skip to shrink an over-deep cushion, and a late
    /// arrival gives one back. `0` when idle.
    public var rxFramesLost: UInt64 = 0
    /// Frames that arrived after their play time and were thrown away.
    public var rxFramesLate: UInt64 = 0
    /// Frames that arrived out of timestamp order.
    public var rxFramesOutOfOrder: UInt64 = 0
    /// `true` while the RX jitter buffer is running. Reported so a client
    /// renders what the engine is doing, not what it last asked for.
    public var rxJitterBufferEnabled: Bool = true
    /// Floor of the RX jitter buffer's adaptive depth in ms — the effective,
    /// clamped value.
    public var rxJitterBufferMinMS: UInt32 = 40
    /// Ceiling of the RX jitter buffer's adaptive depth in ms — the
    /// effective, clamped value.
    public var rxJitterBufferMaxMS: UInt32 = 200
    /// Which mic noise-reduction chain is live. A plain enum,
    /// credential-free.
    public let denoiseChain: DenoiseChain
    /// The rate the capture stream opened at, in Hz, or `0` when no mic lane
    /// is open. Pair it with `denoiseChain`: at 48000 the neural stage can
    /// run; below that it cannot, and `.filterGate` is why.
    public let denoiseDeviceRate: UInt32
    /// `true` when `denoiseChain` was measured from a running capture
    /// stream; `false` when it is a prediction of what the next one will do.
    /// A prediction is rendered differently — see `denoiseSummary`.
    public let denoiseLive: Bool
    /// A short line naming the chain, and the rate where the rate is the
    /// explanation — `"Neural (48 kHz)"`, `"Filter + gate (device
    /// 44.1 kHz)"`, `"Off"`. Empty when the chain cannot be determined at
    /// all, so a caller can hide the row rather than print "not capturing"
    /// at someone.
    ///
    /// A prediction (`denoiseLive == false`) is suffixed "when you key", so
    /// the line never claims to have observed a stream that is not open.
    /// This must stay in step with `DenoiseStatus::summary()` on the Rust
    /// side, which words it the same way.
    public var denoiseSummary: String {
        let kHz: (UInt32) -> String = { r in
            r % 1000 == 0
                ? "\(r / 1000) kHz"
                : String(format: "%.1f kHz", Double(r) / 1000.0)
        }
        let base: String
        switch denoiseChain {
        case .notCapturing: return ""
        case .off: base = "Off"
        case .neural: base = "Neural (\(kHz(denoiseDeviceRate)))"
        case .filterGate where denoiseDeviceRate == 48000: base = "Filter + gate"
        case .filterGate: base = "Filter + gate (device \(kHz(denoiseDeviceRate)))"
        }
        return denoiseLive ? base : "\(base) when you key"
    }
    /// Negotiated voice codec of the active call, or `nil` while idle or still
    /// negotiating (iax-3e53). Shows `.slin16` when wideband is live. A plain
    /// codec id, credential-free.
    public let negotiatedFormat: VoiceFormat?
    /// Digits already sent of the active `sendDTMF(sequence:)` command
    /// (iax-4b7a); `0` when no sequence is playing. A plain progress counter,
    /// credential-free.
    public let dtmfPlayed: Int
    /// Total digits of the active `sendDTMF(sequence:)` command; `0` when no
    /// sequence is playing. A plain progress counter, credential-free.
    public let dtmfTotal: Int
    /// `true` when M17 voice is available: the `m17` feature is compiled in
    /// AND a working Codec 2 backend was found, probed against the current
    /// ``Station/setCodecDirs(_:)`` value (iax-f2b8 Task 5). Gate a UI's M17
    /// connect affordance on this rather than calling
    /// ``Station/connectM17(host:port:module:callsign:)`` speculatively.
    public let m17Available: Bool
    /// `true` while an M17 session is live (mutually exclusive with an active
    /// IAX2 call — see ``Station/connectM17(host:port:module:callsign:)``).
    public let m17Active: Bool
    /// `true` when D-Star voice is available: the `dstar` feature is compiled
    /// in AND a ThumbDV is attached RIGHT NOW (iax-4c8e).
    ///
    /// Unlike ``m17Available``, this tracks HOTPLUG: D-Star has no software
    /// vocoder, so the dongle's presence is the whole of its availability,
    /// and this flips within ~500 ms of one being plugged in or pulled out.
    /// Grey out a D-Star affordance whenever it is `false`, rather than
    /// calling ``Station/connectDStar(host:port:module:callsign:)``
    /// speculatively.
    public let dstarAvailable: Bool
    /// `true` while a D-Star session is live — mutually exclusive with both
    /// an active IAX2 call and an M17 session (see
    /// ``Station/connectDStar(host:port:module:callsign:)``).
    public let dstarActive: Bool
    /// `true` when System Fusion voice is available: the `ysf` feature is
    /// compiled in AND a ThumbDV is attached right now.
    ///
    /// The same probe ``dstarAvailable`` reads — one dongle, one answer — so
    /// the two always agree, and a UI can grey both affordances together.
    public let ysfAvailable: Bool
    /// `true` while a YSF link is live — mutually exclusive with an IAX2
    /// call, an M17 session and a D-Star session.
    ///
    /// RECEIVE ONLY. There is no YSF transmit, so a UI must not offer a PTT
    /// affordance while this is set.
    public let ysfActive: Bool
    /// `true` when NXDN voice is available: the `nxdn` feature is compiled in
    /// AND a ThumbDV is attached right now.
    ///
    /// The same probe ``dstarAvailable`` and ``ysfAvailable`` read — one
    /// dongle, one answer — so all three always agree and a UI can grey their
    /// affordances together.
    public let nxdnAvailable: Bool
    /// `true` while an NXDN link is live — mutually exclusive with an IAX2
    /// call, an M17 session, a D-Star session and a YSF link.
    ///
    /// RECEIVE ONLY. There is no NXDN transmit path: a key-down is refused,
    /// not queued, so a UI must not offer a PTT affordance while this is set.
    public let nxdnActive: Bool
    /// `true` when DMR voice is available: the `dmr` feature is compiled in
    /// AND a ThumbDV is attached right now.
    ///
    /// The same probe ``dstarAvailable``, ``ysfAvailable`` and
    /// ``nxdnAvailable`` read — one dongle, one answer — so all four always
    /// agree and a UI can grey their affordances together.
    public let dmrAvailable: Bool
    /// `true` while a DMR link is live — mutually exclusive with an IAX2
    /// call, an M17 session, a D-Star session, a YSF link and an NXDN link.
    ///
    /// RECEIVE ONLY. There is no DMR transmit path: a key-down is refused,
    /// not queued, so a UI must not offer a PTT affordance while this is set.
    public let dmrActive: Bool
    /// `true` when the library was built with EchoLink, so the `echolink…`
    /// calls do something. EchoLink needs no hardware, so nothing else goes
    /// into it.
    public let echolinkAvailable: Bool
    /// `true` while an EchoLink call is connecting or connected, or while any
    /// station is on the node's EchoLink leg — mutually exclusive with every
    /// other network.
    public let echolinkActive: Bool
    /// `true` while logged in to the EchoLink directory. A login still
    /// running, or one that failed, reads `false`; ``Station/echolinkState()``
    /// says which.
    public let echolinkLoggedIn: Bool
    /// The name of the output device whose stream stopped and could not be
    /// rebuilt, or `nil` while the speaker is playing. The engine rebuilds a
    /// stream whose device changes under it (a Bluetooth headset switching
    /// to hands-free) by itself; this is set only once that has failed, so a
    /// UI can say "Speaker stopped" instead of leaving the call silently deaf.
    public var speakerStopped: String? = nil
    /// The name of the input device whose stream stopped and could not be
    /// rebuilt, or `nil` while the mic is capturing.
    public var micStopped: String? = nil
    /// Which sources hold the key right now. The radio is keyed while this is
    /// non-empty (`ptt` is the OR's result).
    public var pttSources: StationPTTSourceSet = []
    /// The sources a forced stop latched. Each must be released before it can
    /// key again (release-to-re-arm).
    public var pttLatched: StationPTTSourceSet = []
    /// It goes up by one on every forced stop and is `0` until the first one,
    /// so a UI announces each stop exactly once.
    public var pttForcedSeq: UInt32 = 0
    /// The sources that were holding when the latest forced stop fired.
    public var pttForcedSources: StationPTTSourceSet = []
    /// Why the latest forced stop fired, or `nil` if none has.
    public var pttForcedReason: StationPTTForcedReason? = nil
    /// The transmit time-out in force, in ms. It can never be 0: the engine
    /// starts at 120 000 and refuses anything outside 60 000...600 000.
    public var pttTxTimeoutMS: UInt32 = 120_000
    /// The loudness VOX judges, in dBFS `-60.0...0.0`: the mean power of the
    /// same input as ``inputDB`` over the last 50 ms. Drive VOX and its
    /// calibration meter from this. A 5 ms click reads 10 dB under its own
    /// peak and is forgotten 50 ms later.
    public var voxDB: Float = -60
}

/// The YSF-shaped state of a live link, from ``Station/ysfState()``.
///
/// Everything network-agnostic — the level meters, the call status — lives on
/// ``Station/Snapshot``. Credential-free: callsigns and counters only.
public struct YSFState: Equatable, Sendable {
    /// State of the link to the reflector.
    public enum Link: String, Sendable {
        case idle, linking, linked, unlinking, failed
    }

    /// Which vocoder is decoding. Always ``Backend/thumbdv`` — YSF voice is
    /// AMBE+2 and there is no software vocoder.
    public enum Backend: String, Sendable {
        case thumbdv
    }

    /// A frame mode astar cannot decode.
    ///
    /// astar decodes DN — V/D modes 1 and 2 — and nothing else. A reflector
    /// carrying one of these would otherwise be indistinguishable from a
    /// broken one: silence, with nothing saying why. Surface it.
    public enum UnsupportedMode: String, Sendable {
        /// VW: full-rate voice.
        case voiceFullRate = "voice-fr"
        /// A data frame, which is not voice at all.
        case dataFullRate = "data-fr"

        /// What to tell the operator.
        public var localizedDescription: String {
            switch self {
            case .voiceFullRate:
                return "This reflector is sending VW (wide) voice, which astar cannot decode."
            case .dataFullRate:
                return "This reflector is sending data frames, which carry no voice."
            }
        }
    }

    public let link: Link
    /// The callsign of the most recently heard transmission, or `nil` until
    /// one arrives.
    ///
    /// Read from the frame header in clear — no vocoder involved, so it is
    /// populated even without a dongle. PERSISTS past end-of-transmission:
    /// this is "last heard", not "transmitting right now". Read ``receiving``
    /// for that.
    ///
    /// Attacker-controlled: it is whatever whoever is transmitting put in the
    /// header. Render it as text, never as markup.
    public let lastHeard: String?
    /// Radio frames received since the link came up.
    ///
    /// A liveness counter: a link that is up and silent and one that is
    /// receiving look identical from ``link`` alone.
    public let framesRX: UInt64
    /// `true` while a transmission is in progress.
    public let receiving: Bool
    /// The last mode this link could not decode, or `nil` — the usual case.
    ///
    /// Never cleared once set: a caller that has seen it has something true
    /// to tell the operator, and clearing it on the next DN frame would make
    /// a mixed-mode reflector flicker.
    public let unsupportedMode: UnsupportedMode?
    /// The vocoder decoding this link, or `nil` for a link opened without
    /// audio (or one whose backend this binding does not recognise).
    public let backend: Backend?
    /// `true` while this station is transmitting — the engine's
    /// ACTUALLY-APPLIED state, not an echo of the last key request. A
    /// key-down refused for want of a capture device never sets it.
    public let ptt: Bool

    /// Construct one directly.
    ///
    /// Public, unlike ``DStarState``'s, because this type is worth building
    /// without an engine: a SwiftUI preview showing the unsupported-mode
    /// warning, and a test proving a client renders it, both need a value and
    /// neither has a reflector sending VW to hand.
    public init(
        link: Link, lastHeard: String? = nil, framesRX: UInt64 = 0, receiving: Bool = false,
        unsupportedMode: UnsupportedMode? = nil, backend: Backend? = nil, ptt: Bool = false
    ) {
        self.link = link
        self.lastHeard = lastHeard
        self.framesRX = framesRX
        self.receiving = receiving
        self.unsupportedMode = unsupportedMode
        self.backend = backend
        self.ptt = ptt
    }

    /// Decode from the C-ABI's JSON. Returns `nil` for the `{}` no-link
    /// document.
    ///
    /// An unrecognized `link` string means a newer engine is talking to an
    /// older binding, and lands as ``Link/failed`` — the safe direction. An
    /// unrecognized `unsupported_mode` lands as `nil` rather than being
    /// invented, so a UI never names a mode it does not understand.
    init?(json: String) {
        guard let data = json.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let linkString = obj["link"] as? String
        else { return nil }
        link = Link(rawValue: linkString) ?? .failed
        lastHeard = obj["last_heard"] as? String
        framesRX = (obj["frames_rx"] as? NSNumber)?.uint64Value ?? 0
        receiving = obj["receiving"] as? Bool ?? false
        unsupportedMode = (obj["unsupported_mode"] as? String)
            .flatMap(UnsupportedMode.init(rawValue:))
        backend = (obj["backend"] as? String).flatMap(Backend.init(rawValue:))
        ptt = obj["ptt"] as? Bool ?? false
    }
}

/// The NXDN-shaped state of a live link, from ``Station/nxdnState()``.
///
/// Everything network-agnostic — the level meters, the call status — lives on
/// ``Station/Snapshot``. Credential-free: numbers and counters only.
public struct NXDNState: Equatable, Sendable {
    /// State of the link to the reflector.
    ///
    /// The same vocabulary ``DStarState/Link`` and ``YSFState/Link`` use, so
    /// one decoder covers every network.
    public enum Link: String, Sendable {
        case idle, linking, linked, unlinking, failed
    }

    /// Which vocoder is decoding. Always ``Backend/thumbdv`` — NXDN voice is
    /// AMBE+2 and there is no software vocoder.
    public enum Backend: String, Sendable {
        case thumbdv
    }

    public let link: Link
    /// The most recently heard station's NXDN id, as text, or `nil` until one
    /// transmits.
    ///
    /// A NUMBER, not a callsign: an `NXDND` frame carries `srcId` and no
    /// callsign at all, so naming the sender is a directory lookup astar does
    /// not hold. Read from the header in clear — no vocoder involved — and it
    /// PERSISTS past end-of-transmission: this is "last heard", not
    /// "transmitting right now". Read ``receiving`` for that.
    ///
    /// Attacker-controlled: it is whatever the transmitting station put on
    /// the wire. Render it as text, never as markup.
    public let lastHeard: String?
    /// That same id, unformatted, for a caller that has a directory to
    /// resolve it against.
    public let lastHeardID: UInt16?
    /// Network frames received since the link came up.
    ///
    /// A liveness counter: a link that is up and silent and one that is
    /// receiving look identical from ``link`` alone.
    public let framesRX: UInt64
    /// `true` while a transmission is in progress.
    public let receiving: Bool
    /// The vocoder decoding this link, or `nil` for a link opened without
    /// audio (or one whose backend this binding does not recognise).
    public let backend: Backend?
    /// `true` while this station is transmitting.
    ///
    /// ALWAYS `false` today: NXDN is receive-only and a key is refused rather
    /// than queued. It exists so a client polling every network through one
    /// shape need not special-case this one.
    public let ptt: Bool

    /// Construct one directly.
    ///
    /// Public, like ``YSFState``'s and ``M17State``'s, because a SwiftUI
    /// preview or a client test that renders the last-heard line needs a
    /// value and has no reflector to hand.
    public init(
        link: Link, lastHeard: String? = nil, lastHeardID: UInt16? = nil, framesRX: UInt64 = 0,
        receiving: Bool = false, backend: Backend? = nil, ptt: Bool = false
    ) {
        self.link = link
        self.lastHeard = lastHeard
        self.lastHeardID = lastHeardID
        self.framesRX = framesRX
        self.receiving = receiving
        self.backend = backend
        self.ptt = ptt
    }

    /// Decode from the C-ABI's JSON. Returns `nil` for the `{}` no-link
    /// document.
    ///
    /// An unrecognized `link` string means a newer engine is talking to an
    /// older binding, and lands as ``Link/failed`` — the same answer
    /// ``DStarState``, ``YSFState`` and ``M17State`` give, and the safe
    /// direction: a UI that believes the link is down offers nothing on it.
    init?(json: String) {
        guard let data = json.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let linkString = obj["link"] as? String
        else { return nil }
        link = Link(rawValue: linkString) ?? .failed
        lastHeard = obj["last_heard"] as? String
        lastHeardID = (obj["last_heard_id"] as? NSNumber)?.uint16Value
        framesRX = (obj["frames_rx"] as? NSNumber)?.uint64Value ?? 0
        receiving = obj["receiving"] as? Bool ?? false
        backend = (obj["backend"] as? String).flatMap(Backend.init(rawValue:))
        ptt = obj["ptt"] as? Bool ?? false
    }
}

/// The DMR-shaped state of a live link, from ``Station/dmrState()``.
///
/// Everything network-agnostic — the level meters, the call status — lives on
/// ``Station/Snapshot``. Credential-free: numbers and counters only. The
/// master password given to ``Station/connectDMR(system:host:port:radioID:callsign:talkgroup:timeslot:password:)``
/// is not here, and there is no field it could occupy.
public struct DMRState: Equatable, Sendable {
    /// State of the link to the master.
    ///
    /// Longer than ``NXDNState/Link``'s vocabulary because a homebrew login is
    /// a CONVERSATION — login, then authenticate, then configure, each a round
    /// trip that can stall on its own — and an operator watching a connect
    /// hang is owed which one stalled.
    public enum Link: String, Sendable {
        case idle
        case loggingIn = "logging_in"
        case authenticating
        case configuring
        case linked
        case closing
        case failed
    }

    /// Where a failed link failed.
    ///
    /// ``auth`` ("your password is wrong") and ``login`` ("your ID is not
    /// permitted on this master") are the two an operator can act on, and on
    /// the wire the only difference between them is which packet the refusal
    /// answered. Show it.
    public enum Failure: String, Sendable {
        case login, auth, config, session, closed, timeout
    }

    /// Which vocoder is decoding. Always ``Backend/thumbdv`` — DMR voice is
    /// AMBE+2 and there is no software vocoder.
    public enum Backend: String, Sendable {
        case thumbdv
    }

    public let link: Link
    /// Why the link failed, or `nil` while it has not.
    public let failure: Failure?
    /// The most recently heard station's DMR id, as text, or `nil` until one
    /// transmits.
    ///
    /// A NUMBER, not a callsign: a `DMRD` frame carries `srcId` and no
    /// callsign at all, so naming the sender is a directory lookup (radioid.net)
    /// astar does not hold. Read from the header in clear — no vocoder
    /// involved — and it PERSISTS past end-of-transmission: this is "last
    /// heard", not "transmitting right now". Read ``receiving`` for that.
    ///
    /// Attacker-controlled: it is whatever the transmitting station put on
    /// the wire. Render it as text, never as markup.
    public let lastHeard: String?
    /// That same id, unformatted, for a caller that has a directory to
    /// resolve it against.
    public let lastHeardID: UInt32?
    /// The talkgroup this link joined.
    public let talkgroup: UInt32
    /// The timeslot it joined it on — `1` or `2`.
    public let timeslot: UInt8
    /// Network frames received since the link came up.
    ///
    /// A liveness counter: a link that is up and silent and one that is
    /// receiving look identical from ``link`` alone.
    public let framesRX: UInt64
    /// `true` while a transmission is in progress.
    public let receiving: Bool
    /// The vocoder decoding this link, or `nil` for a link opened without
    /// audio (or one whose backend this binding does not recognise).
    public let backend: Backend?
    /// `true` while this station is transmitting.
    ///
    /// ALWAYS `false` today: DMR is receive-only and a key is refused rather
    /// than queued. It exists so a client polling every network through one
    /// shape need not special-case this one.
    public let ptt: Bool

    /// Construct one directly.
    ///
    /// Public, like ``NXDNState``'s and ``YSFState``'s, because a SwiftUI
    /// preview or a client test that renders the last-heard line needs a
    /// value and has no master to hand.
    public init(
        link: Link, failure: Failure? = nil, lastHeard: String? = nil, lastHeardID: UInt32? = nil,
        talkgroup: UInt32 = 0, timeslot: UInt8 = 0, framesRX: UInt64 = 0, receiving: Bool = false,
        backend: Backend? = nil, ptt: Bool = false
    ) {
        self.link = link
        self.failure = failure
        self.lastHeard = lastHeard
        self.lastHeardID = lastHeardID
        self.talkgroup = talkgroup
        self.timeslot = timeslot
        self.framesRX = framesRX
        self.receiving = receiving
        self.backend = backend
        self.ptt = ptt
    }

    /// Decode from the C-ABI's JSON. Returns `nil` for the `{}` no-link
    /// document.
    ///
    /// An unrecognized `link` string means a newer engine is talking to an
    /// older binding, and lands as ``Link/failed`` — the same answer
    /// ``DStarState``, ``YSFState``, ``NXDNState`` and ``M17State`` give, and
    /// the safe direction: a UI that believes the link is down offers nothing
    /// on it. An unrecognized `failure` lands as `nil`, which is the same
    /// safety in the other shape — better no diagnosis than a wrong one.
    init?(json: String) {
        guard let data = json.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let linkString = obj["link"] as? String
        else { return nil }
        link = Link(rawValue: linkString) ?? .failed
        failure = (obj["failure"] as? String).flatMap(Failure.init(rawValue:))
        lastHeard = obj["last_heard"] as? String
        lastHeardID = (obj["last_heard_id"] as? NSNumber)?.uint32Value
        talkgroup = (obj["talkgroup"] as? NSNumber)?.uint32Value ?? 0
        timeslot = (obj["timeslot"] as? NSNumber)?.uint8Value ?? 0
        framesRX = (obj["frames_rx"] as? NSNumber)?.uint64Value ?? 0
        receiving = obj["receiving"] as? Bool ?? false
        backend = (obj["backend"] as? String).flatMap(Backend.init(rawValue:))
        ptt = obj["ptt"] as? Bool ?? false
    }
}

/// Where the EchoLink directory login stands.
///
/// Each raw value is the exact string the engine's state JSON carries in
/// `login`.
public enum EchoLinkLoginState: String, Sendable {
    case loggedOut = "logged_out"
    case loggingIn = "logging_in"
    case loggedIn = "logged_in"
    case failed
}

/// Where an EchoLink call stands. Each raw value is the engine's `link`
/// string.
public enum EchoLinkLink: String, Sendable {
    case connecting, connected, closed
}

/// What kind of station a directory entry is, read from its callsign: a
/// `-L` suffix is a link, `-R` a repeater, and a callsign in stars (like
/// `*ECHOTEST*`) a conference. ``unknown`` is what a string this binding does
/// not recognise decodes to.
public enum EchoLinkStationKind: String, Sendable {
    case user, link, repeater, conference, unknown
}

/// Whether a directory entry is free to call. ``online`` is free, ``busy`` is
/// in a contact, and ``unknown`` is both the directory's own "can't say" and
/// what an unrecognised string decodes to — the grey dot.
public enum EchoLinkStationStatus: String, Sendable {
    case online, busy, unknown
}

/// One line of an EchoLink call's text chat.
///
/// `from` and `text` come from the far station: render them as text, never as
/// markup.
public struct EchoLinkChatLine: Equatable, Sendable {
    /// The callsign that sent the line.
    public let from: String
    /// The line itself.
    public let text: String
    /// `true` for a line this station sent.
    public let mine: Bool

    public init(from: String, text: String, mine: Bool) {
        self.from = from
        self.text = text
        self.mine = mine
    }
}

/// The EchoLink login and call, from ``Station/echolinkState()``.
///
/// There is no password field. The password given to
/// ``Station/echolinkLogin(callsign:password:servers:)`` is not here and never
/// comes back out of the engine.
///
/// The call fields (``link`` through ``chat``) are filled only while a call is
/// in hand; otherwise they are `nil`, `false` or empty.
public struct EchoLinkState: Equatable, Sendable {
    /// Where the directory login stands.
    public let login: EchoLinkLoginState
    /// The callsign the login is for.
    public let callsign: String
    /// The sentence saying why the login failed, for the status line, or
    /// `nil` when it has not.
    public let error: String?
    /// `true` while the login is backing off after no server answered and
    /// will try again by itself — ``login`` reads ``EchoLinkLoginState/loggingIn``
    /// or ``EchoLinkLoginState/failed`` meanwhile. `false` for a refusal,
    /// which only a new login can get past, and while logged in or out. This
    /// is what says whether "Log in again" would do anything.
    public let retrying: Bool
    /// Where the call stands, or `nil` with no call in hand.
    public let link: EchoLinkLink?
    /// The far station's callsign.
    ///
    /// Comes from the far station: render it as text, never as markup.
    public let peer: String?
    /// The far station's node number.
    public let node: UInt32?
    /// `true` while this station is transmitting, as the link applied it.
    public let ptt: Bool
    /// `true` while the far end is talking.
    public let receiving: Bool
    /// The far station's information text, or `nil` until it sends one.
    ///
    /// Comes from the far station: render it as text, never as markup.
    public let info: String?
    /// The call's text chat, oldest first.
    public let chat: [EchoLinkChatLine]
    /// Why the engine stopped transmitting on its own, a plain sentence for
    /// the status line: the 10-minute transmit cap ends an over that runs
    /// that long. `nil` otherwise; the next over clears it.
    public let notice: String?

    /// Construct one directly, for a SwiftUI preview or a client test that has
    /// no directory to hand.
    public init(
        login: EchoLinkLoginState, callsign: String, error: String? = nil,
        retrying: Bool = false, link: EchoLinkLink? = nil, peer: String? = nil, node: UInt32? = nil,
        ptt: Bool = false, receiving: Bool = false, info: String? = nil,
        chat: [EchoLinkChatLine] = [], notice: String? = nil
    ) {
        self.login = login
        self.callsign = callsign
        self.error = error
        self.retrying = retrying
        self.link = link
        self.peer = peer
        self.node = node
        self.ptt = ptt
        self.receiving = receiving
        self.info = info
        self.chat = chat
        self.notice = notice
    }

    /// Decode the engine's state JSON. Returns `nil` for the `{}` document
    /// (logged out with nothing to say) and for anything that is not a JSON
    /// object with a `login` string.
    ///
    /// Public, unlike the other networks' decoders, so AstarCore's tests can
    /// feed it real documents.
    ///
    /// A `login` string this binding does not know decodes to
    /// ``EchoLinkLoginState/loggedOut``, and an unknown `link` to
    /// ``EchoLinkLink/closed``: both are the safe direction, since a UI that
    /// believes there is no login or no call offers nothing on it, PTT
    /// included.
    public init?(json: String) {
        guard let data = json.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let loginString = obj["login"] as? String
        else { return nil }
        login = EchoLinkLoginState(rawValue: loginString) ?? .loggedOut
        callsign = obj["callsign"] as? String ?? ""
        error = obj["error"] as? String
        // Absent reads as `false`: a document without the field never says
        // the engine will retry, so a UI offers "Log in again" rather than
        // waiting on something that is not happening.
        retrying = obj["retrying"] as? Bool ?? false
        link = (obj["link"] as? String).map { EchoLinkLink(rawValue: $0) ?? .closed }
        peer = obj["peer"] as? String
        node = (obj["node"] as? NSNumber)?.uint32Value
        ptt = obj["ptt"] as? Bool ?? false
        receiving = obj["receiving"] as? Bool ?? false
        info = obj["info"] as? String
        chat = (obj["chat"] as? [[String: Any]] ?? []).compactMap { line in
            guard let from = line["from"] as? String, let text = line["text"] as? String
            else { return nil }
            return EchoLinkChatLine(from: from, text: text, mine: line["mine"] as? Bool ?? false)
        }
        notice = obj["notice"] as? String
    }
}

/// One station in the EchoLink directory's list, from
/// ``Station/echolinkStations()``. No IP address: the engine keeps those to
/// itself.
public struct EchoLinkStationEntry: Equatable, Sendable, Identifiable {
    /// The station's callsign, as the directory lists it.
    public let call: String
    /// The station's location or description text.
    ///
    /// Written by whoever runs that station: render it as text, never as
    /// markup.
    public let description: String
    /// The station's node number, which is also its identity in a list.
    public let node: UInt32
    public let kind: EchoLinkStationKind
    public let status: EchoLinkStationStatus

    public var id: UInt32 { node }

    public init(
        call: String, description: String, node: UInt32, kind: EchoLinkStationKind,
        status: EchoLinkStationStatus
    ) {
        self.call = call
        self.description = description
        self.node = node
        self.kind = kind
        self.status = status
    }

    /// Decode the engine's station-list JSON array. Returns `nil` when the
    /// document is not an array; an entry without a `call` or a `node` is
    /// skipped. An unknown `kind` or `status` decodes to `.unknown`.
    ///
    /// Public so AstarCore's tests can feed it real documents.
    public static func decodeStations(json: String) -> [EchoLinkStationEntry]? {
        guard let data = json.data(using: .utf8),
            let rows = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else { return nil }
        return rows.compactMap { row in
            guard let call = row["call"] as? String,
                let node = (row["node"] as? NSNumber)?.uint32Value
            else { return nil }
            return EchoLinkStationEntry(
                call: call,
                description: row["description"] as? String ?? "",
                node: node,
                kind: (row["kind"] as? String).flatMap(EchoLinkStationKind.init(rawValue:))
                    ?? .unknown,
                status: (row["status"] as? String).flatMap(EchoLinkStationStatus.init(rawValue:))
                    ?? .unknown)
        }
    }
}

/// The M17-shaped state of a live session, from ``Station/m17State()``.
///
/// Everything network-agnostic — the level meters, the call status, PTT as
/// the snapshot sees it — lives on ``Station/Snapshot``. What is only here is
/// ``talker``. Credential-free: one callsign and two flags.
public struct M17State: Equatable, Sendable {
    /// State of the reflector link.
    ///
    /// The same vocabulary ``DStarState/Link`` and ``YSFState/Link`` use, so
    /// one decoder covers every network. M17 has no half-open teardown, so it
    /// never reports ``Link/unlinking``.
    public enum Link: String, Sendable {
        case idle, linking, linked, unlinking, failed
    }

    public let link: Link
    /// `true` while voice-stream packets are arriving from the reflector.
    public let receiving: Bool
    /// `true` while this station is transmitting — the engine's
    /// ACTUALLY-APPLIED state, not an echo of the last key request.
    public let ptt: Bool
    /// The callsign of the most recently heard transmission, or `nil` until
    /// one arrives.
    ///
    /// Read from the stream's LSF — no vocoder involved. PERSISTS past
    /// end-of-transmission: this is "last heard", not "transmitting right
    /// now". Read ``receiving`` for that.
    ///
    /// Attacker-controlled: it is whatever callsign whoever keyed up put in
    /// their LSF. Render it as text, never as markup.
    public let talker: String?

    /// Construct one directly.
    ///
    /// Public, like ``YSFState``'s and unlike ``DStarState``'s, because a
    /// SwiftUI preview or a client test that renders the last-heard line
    /// needs a value and has no reflector to hand.
    public init(link: Link, receiving: Bool = false, ptt: Bool = false, talker: String? = nil) {
        self.link = link
        self.receiving = receiving
        self.ptt = ptt
        self.talker = talker
    }

    /// Decode from the C-ABI's JSON. Returns `nil` for the `{}` no-session
    /// document.
    ///
    /// An unrecognized `link` string means a newer engine is talking to an
    /// older binding, and lands as ``Link/failed`` — the safe direction,
    /// since a UI that believes the link is down will not offer PTT.
    init?(json: String) {
        guard let data = json.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let linkString = obj["link"] as? String
        else { return nil }
        link = Link(rawValue: linkString) ?? .failed
        receiving = obj["receiving"] as? Bool ?? false
        ptt = obj["ptt"] as? Bool ?? false
        talker = obj["talker"] as? String
    }
}

/// Which vocoder D-Star uses (``Station/setDStarVocoder(_:)``). The raw values
/// are the engine's own spelling, shared with `node.toml`. D-Star only.
public enum DStarVocoder: String, Sendable, CaseIterable {
    /// The ThumbDV / DVstick dongle. The default.
    case dongle
    /// The software vocoder (Bruce Perens' AMBE 3600×2400 implementation).
    case software
}

/// The D-Star-shaped state of a live session, from ``Station/dstarState()``
/// (iax-4c8e).
///
/// Everything that means the same thing on every network — PTT, the three
/// level meters, the call status — lives on ``Station/Snapshot`` instead:
/// poll that on a metering tick and read this only for the D-Star-specific
/// fields. Credential-free: callsigns and levels only.
public struct DStarState: Equatable, Sendable {
    /// State of the `DExtra` link to the reflector.
    public enum Link: String, Sendable {
        case idle, linking, linked, unlinking, failed
    }

    /// Which vocoder backed this session: the dongle, or — when the operator
    /// chose it in a build that has it — the software vocoder.
    public enum Backend: String, Sendable {
        case thumbdv
        case software
    }

    public let link: Link
    /// The MY callsign of the most recently heard transmission, or `nil`
    /// until a header has arrived.
    ///
    /// PERSISTS past end-of-transmission — this is "last heard", not
    /// "transmitting right now". Read ``Station/Snapshot/rxDB`` to tell
    /// whether audio is actually flowing.
    public let talker: String?
    /// The most recently completed slow-data free-text message, or `nil` if
    /// none has arrived since connecting. Persists like ``talker``.
    ///
    /// Attacker-controlled: it comes from whoever is transmitting on the
    /// reflector. Render it as text, never as markup.
    public let slowText: String?
    /// The vocoder backing this session, or `nil` if the engine named a
    /// backend this binding does not know.
    public let backend: Backend?
    /// `true` when this session can transmit — always `true` for a session
    /// that exists at all today.
    public let txCapable: Bool
    /// `true` while transmit is keyed: the engine's ACTUALLY-APPLIED state,
    /// not an echo of the last ``Station/setPTT(_:)`` request. A refused
    /// key-down never sets it, and a forced unkey (link lost mid-over, or the
    /// 5-minute timeout) clears it without the operator asking.
    public let ptt: Bool
    /// Transmit level in dBFS, or -60 when nothing is transmitting.
    public let txDB: Float
    /// Receive level in dBFS on this session's output bus.
    public let rxDB: Float
    /// Raw microphone level in dBFS, metered even while unkeyed once the
    /// capture device is open. Stays at -60 until the first key-down —
    /// D-Star opens the microphone lazily.
    public let inputDB: Float
    /// The loudness VOX judges (dBFS, mean power over 50 ms) of the same
    /// input. -60 from an engine too old to send it.
    public let voxDB: Float

    /// Decode from the C-ABI's JSON. Returns `nil` for the `{}` no-session
    /// document.
    ///
    /// An unrecognized `link` string means a newer engine is talking to an
    /// older binding. Rather than throw, it lands as ``Link/failed``: that is
    /// the safe direction, since a UI that believes the link is down will not
    /// offer PTT.
    // Internal, not fileprivate: the decoder is hand-written and worth
    // testing directly against engine JSON, which needs `@testable import`
    // reach. Still out of the public API — callers get `Station.dstarState()`.
    init?(json: String) {
        guard let data = json.data(using: .utf8),
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let linkString = obj["link"] as? String
        else { return nil }
        link = Link(rawValue: linkString) ?? .failed
        talker = obj["talker"] as? String
        slowText = obj["slow_text"] as? String
        backend = (obj["backend"] as? String).flatMap(Backend.init(rawValue:))
        txCapable = obj["tx_capable"] as? Bool ?? false
        ptt = obj["ptt"] as? Bool ?? false
        txDB = (obj["tx_db"] as? Double).map(Float.init) ?? -60.0
        rxDB = (obj["rx_db"] as? Double).map(Float.init) ?? -60.0
        inputDB = (obj["input_db"] as? Double).map(Float.init) ?? -60.0
        voxDB = (obj["vox_db"] as? Double).map(Float.init) ?? -60.0
    }
}

/// A drained lifecycle event from `Station.nextEvent()`.
///
/// Modeled as a Swift enum (the C-ABI carries a `kind` + a `remote_ptt` bool).
/// `nextEvent()` returns `nil` for `IaxEventKind.none`, so this enum never
/// needs a `.none` case.

/// Mode of a node-to-node link (iax-1075).
public enum LinkMode: UInt32, Sendable {
    /// Transmit to and hear the peer (mic routed; key-able).
    case transceive = 0
    /// Hear the peer (relayed onward in conference mode); never transmit.
    case monitor = 1
    /// Hear the peer on the local speaker only; never relayed, never sent to.
    case localMonitor = 2
}

/// One link lifecycle event drained from ``Station/nextLinkEvent()``.
public enum LinkEvent: Sendable, Equatable {
    /// The link's call reached Active.
    case connected(node: String, call: UInt64)
    /// The link's call ended / dropped.
    case disconnected(node: String, call: UInt64)
    /// The link's local PTT changed.
    case keyed(node: String, call: UInt64, keyed: Bool)
}

/// One row of ``Station/linkRoster()`` — secret-free.
public struct LinkSnapshot: Sendable, Equatable, Codable {
    /// Opaque call id (process-level, not a secret).
    public let call: UInt64
    /// Node number / peer label.
    public let node: String
    /// Link mode (`transceive` / `monitor` / `local_monitor` on the wire).
    public let mode: String
    /// Liveness (`connecting` / `up`).
    public let state: String
    /// Local PTT state for this link.
    public let keyed: Bool
}

/// Decoded ``Station/linkRoster()`` payload.
public struct LinkRoster: Sendable, Equatable, Codable {
    /// One row per live link.
    public let links: [LinkSnapshot]
}

/// One row of ``Station/heard()``: whatever callsign the far end put on the
/// wire, kept verbatim — it is remote text, not a validated identity.
public struct HeardEntry: Sendable, Equatable, Codable {
    /// The callsign as received, unmodified.
    public let callsign: String
    /// Which network heard it — the app's `Network.rawValue`: "m17", "dstar",
    /// "ysf", "nxdn", "dmr".
    public let network: String
    /// How long ago it was heard, in milliseconds.
    public let ageMs: UInt64

    enum CodingKeys: String, CodingKey {
        case callsign
        case network
        case ageMs = "age_ms"
    }

    public init(callsign: String, network: String, ageMs: UInt64) {
        self.callsign = callsign
        self.network = network
        self.ageMs = ageMs
    }
}

public enum Event: Sendable, Equatable {
    /// The peer answered; media is flowing.
    case answered
    /// The remote end keyed (`true`) or unkeyed (`false`).
    case remotePTT(Bool)
    /// The call ended (normal hangup or failed dial). The C-ABI does not carry a
    /// reason string in the event struct, so none is surfaced here.
    case hangup
    /// The operating mode changed (WT ↔ Node). The new mode is not carried in
    /// the event; read it from `mode()`.
    case modeChanged
    /// An inbound call is ringing (Node/Manual). The caller id is not carried in
    /// the event; read it from `incomingFrom()`.
    case incoming
    /// Outbound node registration succeeded. Secret-free.
    case registered
    /// Outbound node registration failed. Secret-free (no reason carried).
    case registerFailed
}

// MARK: - Recording

/// How a recording lays out receive and transmit audio. The raw values are
/// the C ABI's `layout` argument to `iax_station_record_start`.
public enum RecordingLayout: Int32, Sendable, CaseIterable {
    /// One file: receive on the left channel, transmit on the right.
    case stereo = 0
    /// Two mono files, `… rx.flac` and `… tx.flac`, started and stopped
    /// together.
    case twoMono = 1
    /// One mono file, receive and transmit summed.
    case mixed = 2
}

/// What the recorder is doing, from ``Station/recordState()``. After a stop
/// it is the last recording's final state; before any recording, idle.
public struct RecordState: Decodable, Equatable, Sendable {
    /// A recording is running.
    public let recording: Bool
    /// How long it has run, in milliseconds.
    public let elapsedMS: UInt64
    /// The files it writes (or wrote), as absolute paths.
    public let files: [String]
    /// Audio chunks or frames lost between the taps and the file.
    public let droppedFrames: UInt64
    /// Why the recording stopped by itself (low disk, folder gone), if it did.
    public let error: String?
    /// `"stereo"`, `"two-mono"` or `"mixed"`.
    public let layout: String

    enum CodingKeys: String, CodingKey {
        case recording
        case elapsedMS = "elapsed_ms"
        case files
        case droppedFrames = "dropped_frames"
        case error
        case layout
    }
}

/// What the packet log is doing, from ``Station/packetLogState()``. While
/// no capture runs it is the last capture's final state, or why the last start
/// was refused.
public struct PacketLogState: Decodable, Equatable, Sendable {
    /// Packets are being written to disk.
    public let capturing: Bool
    /// How many calls have a log file open right now.
    public let openCalls: UInt32
    /// Bytes the logs take up in the folder.
    public let usedBytes: UInt64
    /// Packets that never reached a file, for any reason.
    public let dropped: UInt64
    /// The first thing that went wrong (folder unusable, disk full), if any.
    public let error: String?

    public init(
        capturing: Bool, openCalls: UInt32, usedBytes: UInt64, dropped: UInt64, error: String?
    ) {
        self.capturing = capturing
        self.openCalls = openCalls
        self.usedBytes = usedBytes
        self.dropped = dropped
        self.error = error
    }

    enum CodingKeys: String, CodingKey {
        case capturing
        case openCalls = "open_calls"
        case usedBytes = "used_bytes"
        case dropped
        case error
    }
}

// MARK: - Error

/// Thrown when an `iax_station_*` call returns a negative code.
///
/// Carries the integer `code` and the library's generic `iax_error_text`. The
/// underlying C strings are `'static` and secret-free, so this error never
/// carries a secret/password.
public struct StationError: Error, CustomStringConvertible, Equatable {
    /// The `IAX_ERR_*` code (always negative).
    public let code: Int32
    /// The generic, secret-free text from `iax_error_text` — the error
    /// FAMILY, e.g. "audio error".
    public let text: String
    /// What actually went wrong, from the station's `iax_station_last_error`,
    /// when the failing call came from a station. Empty otherwise.
    ///
    /// The codes are coarse on purpose: `IAX_ERR_AUDIO` alone covers a device
    /// that vanished, a config the device will not accept, and a stream that
    /// failed to build. `text` says which family; this says which one.
    /// Secret-free — see the C-ABI's documentation on that accessor.
    public let detail: String

    /// `text`, plus `detail` when there is one. What a UI should show.
    public var message: String { detail.isEmpty ? text : detail }

    /// `detail` defaults to empty so a caller constructing an error directly
    /// — and every existing call site — need not supply one.
    public init(code: Int32, text: String, detail: String = "") {
        self.code = code
        self.text = text
        self.detail = detail
    }

    public var description: String { "astarstation error \(code): \(message)" }

    /// Build a `StationError` from a negative C code, resolving its text via the
    /// C-ABI's `iax_error_text` (owned by the library; never freed here).
    /// `detail` comes from the station when one is available.
    static func from(_ code: Int32, detail: String = "") -> StationError {
        let text: String
        if let raw = iax_error_text(code) {
            text = String(cString: raw)
        } else {
            text = "unknown error"
        }
        return StationError(code: code, text: text, detail: detail)
    }
}

// MARK: - Vocoder trust

/// A vocoder-class USB serial port offered for enrolment. Listing never
/// opens it.
public struct VocoderCandidate: Codable, Equatable, Sendable, Hashable {
    /// USB serial number: the identity trust is recorded against.
    public let serial: String
    /// The OS port path it enumerates as now, for display.
    public let port: String

    public init(serial: String, port: String) {
        self.serial = serial
        self.port = port
    }
}

/// The outcome of ``Station/enrolVocoder(serial:)``. Raw values mirror the
/// C ABI's `IaxVocoderEnrol` and must not be renumbered.
public enum VocoderEnrolResult: Int32, Sendable {
    /// An AMBE-3000 answered: trust it.
    case trusted = 0
    /// An AMBE-3003 (DVstick 33) answered: recognised, not supported yet.
    /// Not trusted.
    case unsupported = 1
    /// Something answered with another PRODID. Refused.
    case wrongDevice = 2
    /// Nothing answered as a vocoder (timeout or garbage): possibly a CAT
    /// cable. Refused.
    case noAnswer = 3
    /// The port would not open, or its DTR/RTS could not be driven low.
    /// Refused; nothing was written.
    case openFailed = 4
    /// No vocoder-class USB device with that serial is attached.
    case notFound = 5
    /// The library has no hardware vocoder support.
    case notCompiled = 6
}

/// What enrolment found, with an operator-facing message.
public struct VocoderEnrolment: Equatable, Sendable {
    public let result: VocoderEnrolResult
    public let serial: String?
    public let port: String?
    /// PRODID as reported (e.g. `"AMBE3000R"`), when the chip answered.
    public let prodid: String?
    /// Model label (`"DVstick 30 / ThumbDV-class"`), for trusted and
    /// unsupported sticks.
    public let model: String?
    /// One sentence saying what happened and why. Show it.
    public let message: String

    /// `true` only for ``VocoderEnrolResult/trusted``.
    public var isTrusted: Bool { result == .trusted }

    public init(
        result: VocoderEnrolResult, serial: String?, port: String?, prodid: String?,
        model: String?, message: String
    ) {
        self.result = result
        self.serial = serial
        self.port = port
        self.prodid = prodid
        self.model = model
        self.message = message
    }

    private struct Wire: Decodable {
        let serial: String?
        let port: String?
        let prodid: String?
        let model: String?
        let message: String?
    }

    /// Build from the C ABI's return code and JSON detail. The code is
    /// authoritative; an unknown code reads as a refusal.
    static func decode(code: Int32, json: String) -> VocoderEnrolment {
        let wire = json.data(using: .utf8).flatMap { try? JSONDecoder().decode(Wire.self, from: $0) }
        return VocoderEnrolment(
            result: VocoderEnrolResult(rawValue: code) ?? .openFailed,
            serial: wire?.serial,
            port: wire?.port,
            prodid: wire?.prodid,
            model: wire?.model,
            message: wire?.message ?? "Enrolment failed."
        )
    }
}

// MARK: - Configuration

/// Configuration for `Station(config:)`.
///
/// Secret-free except `secret`, which is a guest secret (default `"allstar"`)
/// and `portalPass` (a WT portal password). Both are consumed into the station
/// at construction and never retained on the Swift object. The `portal*` triple
/// enables the WT path only when all three are non-nil.
public struct StationConfig: Sendable {
    /// Capture device name substring, or `nil` for the system default.
    public var input: String?
    /// Playback device name substring, or `nil` for the system default.
    public var output: String?
    /// AllStar portal user (WT path), or `nil`.
    public var portalUser: String?
    /// AllStar portal password (WT path), or `nil`. Consumed; never retained.
    public var portalPass: String?
    /// AllStar node selector for token minting (WT path), or `nil`. Optional:
    /// the portal mints without one, so user + password alone enable the path.
    public var portalNode: String?
    /// Guest secret, or `nil` for the default `"allstar"`. Consumed; never
    /// retained.
    public var secret: String?
    /// Codec negotiation policy for outbound calls (iax-3e53): `"ulaw_only"`
    /// (the library default), `"allow_slin"`, `"prefer_slin"`, or
    /// `"prefer_slin16"` (16 kHz wideband). `nil`, `""`, or `"default"`
    /// selects the library default. An unknown string fails
    /// `Station(config:)`. Construction-time only — the audio pipeline rate
    /// is pinned by this policy when the station engine is built, so there is
    /// no runtime setter; changing the policy means rebuilding the station.
    public var codecPolicy: String?
    /// USB serial numbers of the enrolled AMBE vocoder sticks this station
    /// may open (vocoder trust, on-air safety). Empty — the default — opens
    /// no hardware vocoder at all: D-Star/YSF/NXDN/DMR connects fail with
    /// ``Station/noTrustedVocoderMessage``. Applied at construction; change
    /// it later with ``Station/setVocoderTrust(_:)``. Per-machine data.
    public var vocoderSerials: [String]

    public init(
        input: String? = nil,
        output: String? = nil,
        portalUser: String? = nil,
        portalPass: String? = nil,
        portalNode: String? = nil,
        secret: String? = nil,
        codecPolicy: String? = nil,
        vocoderSerials: [String] = []
    ) {
        self.input = input
        self.output = output
        self.portalUser = portalUser
        self.portalPass = portalPass
        self.portalNode = portalNode
        self.secret = secret
        self.codecPolicy = codecPolicy
        self.vocoderSerials = vocoderSerials
    }
}

/// Node-mode configuration for `Station.setNodeConfig(_:)`.
///
/// Secret-free by construction: there is deliberately no password field. The
/// registrar password (for `registrar`/`registerUser`) is supplied only at
/// runtime through `Station.setCredentialResolver(_:)`.
public struct NodeConfig: Sendable {
    /// Listener bind address `"host:port"`, or `nil` for `"0.0.0.0:4569"`.
    public var bind: String?
    /// Auto vs manual answer.
    public var answer: AnswerPolicy
    /// Inbound auth policy.
    public var auth: AuthPolicy
    /// Upstream registrar `"host:port"` to register AS a node, or `nil` to only
    /// listen (no registration). Resolve the node→host:port yourself.
    public var registrar: String?
    /// The node id to register AS (e.g. `"77777"`), required when `registrar`
    /// is non-nil, otherwise ignored. May be `nil` when not registering.
    public var registerUser: String?
    /// Requested registration refresh interval in seconds, or `0` for `60`.
    public var refreshSecs: UInt32

    public init(
        bind: String? = nil,
        answer: AnswerPolicy = .auto,
        auth: AuthPolicy = .off,
        registrar: String? = nil,
        registerUser: String? = nil,
        refreshSecs: UInt32 = 0
    ) {
        self.bind = bind
        self.answer = answer
        self.auth = auth
        self.registrar = registrar
        self.registerUser = registerUser
        self.refreshSecs = refreshSecs
    }
}

/// WireGuard link-transport configuration for `Station.setWireGuard(_:)`
/// (iax-912e).
///
/// Secret-free except `privateKey` (the station's WireGuard private key,
/// base64), which — like `StationConfig.portalPass` — is consumed into the
/// engine via a one-shot resolver at tunnel build time and never retained on
/// the Swift object, stored in any snapshot/event/log type, or surfaced in an
/// error.
public struct WireGuardConfig: Sendable {
    /// Peer (WireGuard server) endpoint `"host:port"` — an IP literal or a
    /// DNS name.
    public var endpoint: String
    /// The peer's public key, base64 (32 bytes encoded).
    public var peerPublicKey: String
    /// This station's tunnel address in IPv4 CIDR form (e.g. `"10.99.0.2/32"`).
    /// The tunnel-inner network is IPv4-only.
    public var tunnelIP: String
    /// Allowed-IPs CIDRs for the peer (advisory in the userspace stack).
    /// Default empty.
    public var allowedIPs: [String]
    /// Persistent keepalive interval in seconds, or `0` for the library
    /// default (25).
    public var keepaliveSecs: UInt16
    /// This station's WireGuard private key, base64 (32 bytes encoded).
    /// Consumed; never retained.
    public var privateKey: String
    /// Optional plain (non-tunnel) OS UDP listener address `"host:port"` the
    /// engine binds ALONGSIDE the tunnel listener for direct/LAN peers, or
    /// `nil` for none.
    public var alsoBindUDP: String?

    public init(
        endpoint: String,
        peerPublicKey: String,
        tunnelIP: String,
        allowedIPs: [String] = [],
        keepaliveSecs: UInt16 = 0,
        privateKey: String,
        alsoBindUDP: String? = nil
    ) {
        self.endpoint = endpoint
        self.peerPublicKey = peerPublicKey
        self.tunnelIP = tunnelIP
        self.allowedIPs = allowedIPs
        self.keepaliveSecs = keepaliveSecs
        self.privateKey = privateKey
        self.alsoBindUDP = alsoBindUDP
    }
}

// MARK: - Station

/// An IAX2 station over the C-ABI.
///
/// The opaque `IaxStation*` handle is freed in `deinit` (which also tears down
/// any active call, via the Rust `Drop`). All methods `throw` a `StationError`
/// on a negative return code, except those documented to return a value.
///
/// Not thread-safe at the Swift layer: serialize calls from one actor/thread.
/// (The underlying Rust `Station` is internally synchronized, but the handle
/// pointer is exclusively owned here.)
public final class Station {
    private let handle: OpaquePointer

    /// Boxes the Swift credential-resolver closure so a `@convention(c)`
    /// trampoline (which cannot capture Swift state) can reach it via the
    /// opaque `user_data` pointer. Retained for the lifetime of the station; the
    /// raw `Unmanaged` reference is released in `deinit` (and when the resolver
    /// is replaced). The secret never lives here — only the closure that yields
    /// it on demand.
    private final class ResolverBox {
        let fn: (String) -> String
        init(_ fn: @escaping (String) -> String) { self.fn = fn }
    }

    /// The retained box backing the currently-installed credential resolver, if
    /// any. Held so the trampoline's `user_data` stays valid; released on
    /// replace and in `deinit`.
    private var resolverBox: Unmanaged<ResolverBox>?

    /// Create a station from a `StationConfig`.
    ///
    /// `secret` and `portalPass` are passed through to the C-ABI and never
    /// stored on this object. Throws `StationError(IAX_ERR_PANIC)` if the C-ABI
    /// returns NULL (a caught panic, or an unknown `codecPolicy` string —
    /// construction is the only place the codec policy can be set).
    public init(config: StationConfig = StationConfig()) throws {
        _ = Station.layoutChecked
        // withOptionalCString builds a NULL-or-borrowed `const char*` for the
        // duration of `iax_station_new` only. The C-ABI copies what it needs.
        let handle = Station.withConfigPointers(config) { cfg in
            iax_station_new(cfg)
        }
        guard let handle else {
            // -1 == IAX_ERR_PANIC; new returns NULL on NULL cfg or a caught panic.
            throw StationError.from(-1)
        }
        self.handle = handle
        if !config.vocoderSerials.isEmpty {
            try setVocoderTrust(config.vocoderSerials)
        }
    }

    /// Checks, once per process, that the `IaxState` and `IaxEvent` this
    /// binding was compiled against are the sizes the linked library writes.
    ///
    /// `snapshot()` and `nextEvent()` hand the library a Swift-allocated
    /// struct to fill, and the library writes its own size into it. If the
    /// header the binding was built from is older than the library (a stale
    /// xcframework, say), that write runs past the end of the Swift value and
    /// corrupts whatever memory follows. So a mismatch stops the process here,
    /// before any station exists to snapshot, with a message naming both
    /// sizes, rather than letting it overwrite memory quietly.
    private static let layoutChecked: Void = {
        let checks = [
            ("IaxState", MemoryLayout<IaxState>.size, Int(iax_state_size())),
            ("IaxEvent", MemoryLayout<IaxEvent>.size, Int(iax_event_size())),
        ]
        for (type, mirror, library) in checks {
            if let problem = layoutProblem(type: type, mirror: mirror, library: library) {
                fatalError(problem)
            }
        }
    }()

    /// The sentence describing a struct-size mismatch, or `nil` when the two
    /// sizes agree. Separate from ``layoutChecked`` so a test can check the
    /// wording without stopping the process.
    static func layoutProblem(type: String, mirror: Int, library: Int) -> String? {
        guard mirror != library else { return nil }
        return "AstarStation was built against a \(type) of \(mirror) bytes, but the linked "
            + "library writes \(library). The header and the library are out of step: "
            + "rebuild the xcframework (`just xcframework`)."
    }

    deinit {
        // Release the retained resolver box (if any) before freeing the handle
        // so the trampoline's user_data outlives any in-flight callback the
        // Rust Drop might make during teardown, then is reclaimed.
        iax_station_free(handle)
        resolverBox?.release()
    }

    // MARK: Lifecycle

    /// Manual / non-WT connect (the vendor-neutral generic IAX2 path).
    ///
    /// `secret` is a call-time argument; it is passed straight to the C-ABI and
    /// never stored on this object. `nil` uses the configured guest secret.
    ///
    /// NOTE: this performs blocking network I/O (DNS resolve + dial); call it
    /// off any UI thread.
    public func connect(
        dest: String,
        calling: String,
        secret: String? = nil,
        name: String = "astar"
    ) throws {
        try check(
            withOptionalCString(secret) { secretPtr in
                dest.withCString { destPtr in
                    calling.withCString { callingPtr in
                        name.withCString { namePtr in
                            iax_station_connect(handle, destPtr, callingPtr, secretPtr, namePtr)
                        }
                    }
                }
            }
        )
    }

    /// Web-Transceiver connect (the AllStar convenience path).
    ///
    /// Requires the three `portal*` fields to have been set in the config;
    /// otherwise throws `StationError` with `IAX_ERR_PORTAL`.
    public func connectWT(destNode: String) throws {
        try check(destNode.withCString { iax_station_connect_wt(handle, $0) })
    }

    /// Web-Transceiver connect to an explicit address (the "Advanced options"
    /// manual-address path): mints the token from the configured portal
    /// credentials exactly like ``connectWT(destNode:)``, but dials `address`
    /// (`host:port`, or a bare `host` defaulting to the IAX2 port 4569; an IP
    /// literal or a DNS name) instead of the registrar-resolved node address.
    ///
    /// Use this for the NAT-hairpin / localhost / LAN case where the node's
    /// registrar-advertised public IP is unreachable. An empty or unparseable
    /// `address` throws `StationError` with `IAX_ERR_RESOLVE`; a missing portal
    /// config throws `IAX_ERR_PORTAL`.
    public func connectWT(destNode: String, address: String) throws {
        try check(
            destNode.withCString { destPtr in
                address.withCString { addrPtr in
                    iax_station_connect_wt_addr(handle, destPtr, addrPtr)
                }
            }
        )
    }

    /// Validate the configured Web-Transceiver credentials WITHOUT placing a
    /// call (the credentials "Test" button): runs the portal login + WT-token
    /// mint from the portal credentials supplied in the config, then discards
    /// the token. Opens no IAX call and no UDP call socket.
    ///
    /// Takes no secret argument — it reuses the `portal*` credentials passed at
    /// init. Throws `StationError` with `IAX_ERR_PORTAL` when no portal config
    /// was supplied or the mint fails (bad password / unknown node / network);
    /// the error text is generic and secret-free.
    ///
    /// NOTE: performs blocking network I/O (HTTPS to the portal); call it off
    /// any UI thread.
    public func testMintToken() throws {
        try check(iax_station_mint_token(handle))
    }

    /// Tear down any active call. Idempotent (no-op while idle).
    public func disconnect() throws {
        try check(iax_station_disconnect(handle))
    }

    // MARK: Transmit + gains

    /// Set local transmit (PTT). Throws `IAX_ERR_NOT_CONNECTED` while idle.
    public func setPTT(_ on: Bool) throws {
        try check(iax_station_set_ptt(handle, on))
    }

    // MARK: PTT sources, lease, transmit time-out

    /// Press (`on`) or release one PTT source. The engine keys while ANY
    /// source holds, so releasing one never cuts off another.
    ///
    /// A release is never refused, even while idle. A press throws
    /// `pttLatchedCode` (-27) on a latched source, or the console's own
    /// refusal (`-3` with no call, the NXDN/DMR receive-only codes). After
    /// such a refusal the engine has rolled the press back and latched the
    /// source, except `.legacy`, which a refusal never latches (only the
    /// transmit time-out or `pttStopAll` latches it). A press of `.bluetooth`
    /// throws `invalidArgumentCode` (-26) and changes nothing: Bluetooth keys
    /// only through `pttLeasePress`, so the engine bounds every Bluetooth hold.
    public func pttSource(_ source: StationPTTSource, on: Bool) throws {
        try check(iax_station_ptt_source(handle, source.abiValue, on))
    }

    /// Release every source, latch every one that was holding, and unkey. This
    /// is the universal stop: the on-screen button while another source keys,
    /// TX disabled, and quit. It does not throw while idle.
    public func pttStopAll() throws {
        try check(iax_station_ptt_stop_all(handle))
    }

    /// Press `source` (Bluetooth only) under a lease the engine expires by
    /// itself after `ttl` unless `pttLeaseRenew` extends it. A wedged app or
    /// a dead link therefore ends unkeyed. `ttl` is converted to whole
    /// milliseconds, saturating. Anything outside 1...5000 ms, or any source
    /// but `.bluetooth`, throws `invalidArgumentCode` (-26).
    @available(macOS 13.0, iOS 16.0, *)
    public func pttLeasePress(_ source: StationPTTSource, ttl: Duration) throws {
        try check(
            iax_station_ptt_lease_press(
                handle, source.abiValue, Station.saturatingMilliseconds(ttl)))
    }

    /// Extend a held lease by its TTL. It never presses: on a source that
    /// holds no lease (expired, stopped, never pressed) it throws
    /// `notLeasedCode` (-25), and the caller must treat the button as lost.
    public func pttLeaseRenew(_ source: StationPTTSource) throws {
        try check(iax_station_ptt_lease_renew(handle, source.abiValue))
    }

    /// Set the one transmit time-out, which covers every source. It is
    /// converted to whole milliseconds, saturating, so a huge value can never
    /// wrap round into range. Anything outside 60...600 s, including 0,
    /// throws `invalidArgumentCode` (-26) and leaves the old value in force:
    /// the time-out can never be turned off.
    @available(macOS 13.0, iOS 16.0, *)
    public func setTxTimeout(_ timeout: Duration) throws {
        try check(iax_station_set_tx_timeout_ms(handle, Station.saturatingMilliseconds(timeout)))
    }

    /// Send a single DTMF digit to the active call's peer — the dialer keypad
    /// path. How the digit is emitted follows the mode set via
    /// `setDTMFMode(_:)` (iax-7fff): by default one complete, fixed-duration
    /// (~250 ms) in-band tone; in `.protocolFrames` one out-of-band
    /// DTMF BEGIN/END frame pair.
    ///
    /// `digit` must be one of the 16 DTMF keys (`0`-`9`, `*`, `#`, `A`-`D`;
    /// iax-47ae); any other character throws `StationError` with
    /// `IAX_ERR_INVALID_DIGIT` (-15) without touching the call. Throws
    /// `IAX_ERR_NOT_CONNECTED` (-3) while idle.
    ///
    /// Input command only: nothing is stored, returned, or logged.
    public func sendDTMF(_ digit: Character) throws {
        // The C-ABI takes a single ASCII byte. A non-ASCII Character (multi-byte
        // or with no single ASCII scalar) can never be a dialer key, so reject
        // it the same way the C-ABI would.
        guard let ascii = digit.asciiValue else {
            throw StationError.from(IAX_ERR_INVALID_DIGIT)
        }
        try check(iax_station_send_dtmf(handle, CChar(bitPattern: ascii)))
    }

    /// Select how `sendDTMF(_:)` emits digits (iax-7fff): an in-band tone in
    /// the TX audio path (`.inBand`, the default) or out-of-band IAX2 protocol
    /// frames (`.protocolFrames`). The mode is stored on the station and
    /// applies to the digits sent after the change; setting it while idle is
    /// fine.
    /// Send a multi-digit DTMF command to the active call as one engine-timed
    /// sequence (iax-4b7a): ~250 ms per tone with a ~100 ms inter-digit gap,
    /// honoring the mode set via `setDTMFMode(_:)`. Validation is
    /// all-or-nothing: every character must be one of the 16 DTMF keys
    /// (`0`-`9`, `*`, `#`, `A`-`D`) or `IAX_ERR_INVALID_DIGIT` (-15) is thrown
    /// and nothing is sent (an empty command is rejected the same way).
    ///
    /// The queue advances on `snapshot()` polls — keep polling while a
    /// sequence plays; progress is `Snapshot.dtmfPlayed` / `dtmfTotal`.
    /// Throws `IAX_ERR_NOT_CONNECTED` (-3) while idle (nothing queued) and
    /// `IAX_ERR_DTMF_BUSY` (-17) while a previous sequence is still playing.
    ///
    /// Input command only: nothing is stored beyond the queue, returned, or
    /// logged.
    public func sendDTMF(sequence: String) throws {
        // The C-ABI takes a NUL-terminated ASCII string. Any non-ASCII
        // character can never be a dialer key, so reject it the same way the
        // engine's validation would.
        guard !sequence.isEmpty, sequence.allSatisfy({ $0.isASCII }) else {
            throw StationError.from(IAX_ERR_INVALID_DIGIT)
        }
        try sequence.withCString { try check(iax_station_send_dtmf_string(handle, $0)) }
    }

    /// Drop the un-played remainder of a `sendDTMF(sequence:)` command
    /// (iax-4b7a). The digit currently sounding finishes its tone. Safe to
    /// call when nothing is playing.
    public func cancelDTMF() throws {
        try check(iax_station_cancel_dtmf(handle))
    }

    public func setDTMFMode(_ mode: DtmfMode) throws {
        try check(iax_station_set_dtmf_mode(handle, IaxDtmfMode(rawValue: UInt32(mode.rawValue))))
    }

    /// Set the input (TX/mic) gain multiplier (clamped `0.0...4.0`).
    public func setInputGain(_ gain: Float) throws {
        try check(iax_station_set_input_gain(handle, gain))
    }

    /// Set the output (RX/speaker) gain multiplier (clamped `0.0...4.0`:
    /// 100%-400% headroom for boosting a quiet station, iax-a4e7).
    public func setOutputGain(_ gain: Float) throws {
        try check(iax_station_set_output_gain(handle, gain))
    }

    /// Toggle RX/output compression on the live/next call (iax-a4e7 PHASE 1):
    /// automatic leveling of the RECEIVED audio, reusing the mic-path
    /// compressor (makeup gain included) on the output bus, applied BEFORE
    /// the output gain multiply so the 100%-400% output-gain range amplifies
    /// the already-leveled signal. Shared across networks (output is
    /// listener-side). Takes effect immediately on an active call's output
    /// bus.
    public func setRxCompression(_ on: Bool) throws {
        try check(iax_station_set_rx_compression(handle, on))
    }

    /// Set the RX/output compression strength (`0.0...1.0`, clamped): `0.0` =
    /// light, `1.0` = most aggressive, default `0.90`. Takes effect
    /// immediately when RX compression is enabled (iax-a4e7 PHASE 1).
    public func setRxCompressionLevel(_ level: Float) throws {
        try check(iax_station_set_rx_compression_level(handle, level))
    }

    /// Configure the RX jitter buffer: whether received audio is played out of
    /// the adaptive buffer at all, and the window (`minMs`...`maxMs`) its
    /// depth may live in.
    ///
    /// The defaults are Asterisk `chan_iax2`'s — on, 40 ms of slack over
    /// measured jitter, a 200 ms ceiling — because the node at the other end
    /// of an AllStarLink call is Asterisk. A larger `minMs` buys fewer holes
    /// with more latency. Both bounds are clamped to `0...500` ms and a
    /// `maxMs` below `minMs` is raised to meet it: a setting is repaired,
    /// never refused. Takes effect immediately, mid-call, with no reconnect;
    /// read the effective values back from `Snapshot`'s
    /// `rxJitterBufferEnabled` / `rxJitterBufferMinMS` / `rxJitterBufferMaxMS`.
    public func setRxJitter(enabled: Bool, minMs: UInt32, maxMs: UInt32) throws {
        try check(iax_station_set_rx_jitter(handle, enabled, minMs, maxMs))
    }

    /// Toggle mic voice compression on the live/next call. Takes effect
    /// immediately on an active call's capture lane.
    public func setCompression(_ on: Bool) throws {
        try check(iax_station_set_compression(handle, on))
    }

    /// Set the mic voice-compression strength (`0.0...1.0`, clamped): `0.0` =
    /// light, `1.0` = most aggressive, default `0.90`. Takes effect immediately
    /// when compression is enabled.
    public func setCompressionLevel(_ level: Float) throws {
        try check(iax_station_set_compression_level(handle, level))
    }

    /// Set the neural mic noise-reduction strength (`0.0...1.0`, clamped):
    /// `1.0` = full denoise (default), `0.0` = bypass. Takes effect
    /// immediately.
    ///
    /// RNNoise has no strength parameter of its own, so this drives a
    /// delay-compensated dry/wet mix. It has **no effect while the classical
    /// hum-filter-plus-gate chain is running** — there is no wet path to mix
    /// against. Read `Snapshot.denoiseChain` to see which is live.
    public func setDenoiseStrength(_ level: Float) throws {
        try check(iax_station_set_denoise_strength(handle, level))
    }

    /// Set the TX trim gain (`0.0...4.0`, clamped; default `1.0` = unity): the
    /// always-on final TX gain stage after compression. Attenuates a hot mic
    /// that compression makeup gain would otherwise keep loud; values above
    /// 1.0 boost (clamped at full scale). Takes effect immediately on the
    /// live/next call.
    public func setTxTrim(_ gain: Float) throws {
        try check(iax_station_set_tx_trim(handle, gain))
    }

    /// Toggle mic noise reduction (denoise) on the live/next call. Takes effect
    /// immediately on an active call's capture lane.
    public func setNoiseReduction(_ on: Bool) throws {
        try check(iax_station_set_noise_reduction(handle, on))
    }

    /// Set the VOX pre-roll / look-back length in milliseconds (clamped
    /// `0...250`, `0` = disabled, the default). When software VOX keys the call,
    /// the engine flushes this much buffered mic audio ahead of the live stream
    /// so the speech onset is not clipped. Takes effect immediately on the
    /// active routed mic.
    public func setVoxPrerollMs(_ ms: UInt32) throws {
        try check(iax_station_set_vox_preroll_ms(handle, ms))
    }

    /// Set the live spectrum peak-hold decay in dB/second (clamped `1...500`,
    /// default `100`). Drives the fall-rate of the peak-held spectrum bars
    /// shared by the mic monitor, the live-call TX, and the live-call RX
    /// analyzers — a single call scrubs every visible spectrum at once. A larger
    /// value makes peaks track downward changes faster, a smaller value holds
    /// them longer. Applies to the analyzers that are currently live (the mic
    /// monitor if monitoring, the active call's TX/RX if a call is up).
    public func setSpectrumDecay(dbPerSecond: Float) throws {
        try check(iax_station_set_spectrum_decay(handle, dbPerSecond))
    }

    // MARK: Monitor mode

    /// Monitor THIS device (iax-2377): open the capture device and run the mic
    /// lane WITHOUT a call so a front-end can preview / characterize the mic
    /// before dialing. `input` is a capture-device name substring, or `nil` for
    /// the system default.
    ///
    /// Call it again to change mics: asking for a **different** resolved device
    /// stops the running monitor and opens the new one — a device picker moves
    /// the stream, not just its label — while asking for the device already
    /// monitored is a no-op. The device is resolved before the running monitor
    /// is touched, so a name that resolves to nothing throws and leaves the
    /// current monitor running. Still call-safe: a no-op if a call is already
    /// active (the device is already open on the call's mic lane).
    ///
    /// Stop it with `monitorStop()`.
    ///
    /// NOTE: opens an audio device (blocking); call it off any UI thread.
    public func monitorStart(input: String? = nil) throws {
        try check(
            withOptionalCString(input) { inPtr in
                iax_station_monitor_start(handle, inPtr)
            }
        )
    }

    /// Stop monitor mode and release the capture device. Idempotent (no-op if
    /// not monitoring).
    public func monitorStop() throws {
        try check(iax_station_monitor_stop(handle))
    }

    /// Poll the live voice-band mic spectrum (iax-e73e): an array of peak-held
    /// dBFS magnitudes (`-120...0`), log-spaced over ~100 Hz..3.9 kHz. Empty
    /// while not monitoring. Poll-only (no callback); call ~20 Hz to drive a
    /// spectrum view. The array length is `IAX_SPECTRUM_BINS`.
    public func micSpectrum() throws -> [Float] {
        var buf = [Float](repeating: 0, count: Int(IAX_SPECTRUM_BINS))
        let n = try check(iax_station_mic_spectrum(handle, &buf, UInt(buf.count)))
        return Array(buf.prefix(Int(n)))
    }

    /// Poll the live-call **TX** spectrum (iax-2b09): the audio you're sending on
    /// the active network call, as peak-held dBFS bins (same format as
    /// `micSpectrum()`). Empty with no active call. Poll-only; call ~20 Hz.
    public func txSpectrum() throws -> [Float] {
        var buf = [Float](repeating: 0, count: Int(IAX_SPECTRUM_BINS))
        let n = try check(iax_station_tx_spectrum(handle, &buf, UInt(buf.count)))
        return Array(buf.prefix(Int(n)))
    }

    /// Poll the live-call **RX** spectrum (iax-2b09): the audio received from the
    /// far end on the active network call, as peak-held dBFS bins (same format as
    /// `micSpectrum()`). Empty with no active call. Poll-only; call ~20 Hz.
    public func rxSpectrum() throws -> [Float] {
        var buf = [Float](repeating: 0, count: Int(IAX_SPECTRUM_BINS))
        let n = try check(iax_station_rx_spectrum(handle, &buf, UInt(buf.count)))
        return Array(buf.prefix(Int(n)))
    }

    /// The options `characterize(harmonicComb:peakMarginDb:thresholdDbfs:)`
    /// sends across the ABI. Every level key is optional on the wire: a `nil`
    /// is left out entirely (Swift's `Encodable` omits a nil `Optional` by
    /// default) so the engine's own default stays authoritative.
    private struct CharacterizeOptions: Encodable {
        var harmonicComb: Bool
        var peakMarginDb: Float?
        var thresholdDbfs: Float?

        enum CodingKeys: String, CodingKey {
            case harmonicComb = "harmonic_comb"
            case peakMarginDb = "peak_margin_db"
            case thresholdDbfs = "threshold_dbfs"
        }
    }

    /// Characterize the monitored mic (iax-5fb6): run `characterize()` over the
    /// buffered monitor-mode silence and return the resulting `MicProfile` as a
    /// JSON string (secret-free — plain DSP numbers). Empty while not monitoring;
    /// call after a few seconds of monitored silence. `harmonicComb` enables
    /// harmonic-aware notch detection (**default off**: a learned-fundamental
    /// comb that catches rolled-off upper harmonics).
    ///
    /// Two ways to say what counts as a tone, and `thresholdDbfs` wins:
    ///
    /// - `peakMarginDb` is **relative** — how far above the measured noise floor
    ///   a bin must stand. Bigger is fussier.
    /// - `thresholdDbfs` is **absolute** — a level, in the same
    ///   sinusoid-normalised dBFS `micSpectrum()` reports, that a bin must
    ///   exceed. A tone drawn above that line on a spectrum view is above it for
    ///   the detector too, whatever the noise underneath is doing.
    ///
    /// Either way, a mic clean enough that nothing clears the bar characterizes
    /// as a pass-through profile (an empty notch list). `nil` for either keeps
    /// the engine's own default. Persist the JSON opaquely per device and feed
    /// it back via `setMicProfile(_:)`.
    public func characterize(
        harmonicComb: Bool = false,
        peakMarginDb: Float? = nil,
        thresholdDbfs: Float? = nil
    ) throws -> String {
        let options = CharacterizeOptions(
            harmonicComb: harmonicComb, peakMarginDb: peakMarginDb, thresholdDbfs: thresholdDbfs)
        let optionsJSON = String(
            decoding: try JSONEncoder().encode(options), as: UTF8.self)
        return try optionsJSON.withCString { optsPtr -> String in
            // Size, then fill — but each call is a fresh MEASUREMENT of a moving
            // mic ring, not a read of a stored string, so the second run can be
            // longer than the first reported. It does not take a new notch to do
            // it: the floats are printed at full precision, and a floor that
            // lands on "-44.78613" instead of "-44.7861" is one byte more. The C
            // ABI truncates silently, and one byte off the end of an object is a
            // missing "}" — JSON that parses nowhere. So retry once at the size
            // the fill actually reported, the same once-only retry the Python
            // binding does for its lists.
            var needed = iax_station_characterize_opts(handle, optsPtr, nil, 0)
            for _ in 0..<2 {
                if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
                if needed == 0 { return "" }
                // +1 for the NUL the C-ABI writes.
                var buf = [CChar](repeating: 0, count: Int(needed) + 1)
                let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
                    iax_station_characterize_opts(handle, optsPtr, ptr.baseAddress, UInt(ptr.count))
                }
                if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
                if rc <= needed { return String(cString: buf) }
                needed = rc
            }
            // Grew twice running. Report "nothing to characterize" rather than
            // hand back a truncated profile; the caller's next attempt re-measures.
            return ""
        }
    }

    /// Apply (or clear) a calibrated per-mic profile (iax-2095). `json` is a
    /// `MicProfile` JSON string from `characterize(harmonicComb:)` (or persisted
    /// per device), or `nil` to clear back to the generic noise reducer. A
    /// recalled profile rebuilds the live call's noise-reduction comb (and seeds
    /// the next call). The JSON carries plain DSP numbers only — no credentials.
    public func setMicProfile(_ json: String?) throws {
        try check(
            withOptionalCString(json) { jsonPtr in
                iax_station_set_mic_profile(handle, jsonPtr)
            }
        )
    }

    // MARK: Mode + Node

    /// Switch the operating mode (WT dial-out ↔ inbound Node).
    ///
    /// Entering Node mode starts the listener (and fires registration if a
    /// registrar was configured via `setNodeConfig(_:)` and a resolver is set);
    /// leaving it tears the node down and deregisters.
    ///
    /// NOTE: this performs blocking work (device + socket setup, registration);
    /// call it off any UI thread.
    public func setMode(_ mode: Mode) throws {
        try check(iax_station_set_mode(handle, IaxMode(rawValue: UInt32(mode.rawValue))))
    }

    /// The current operating mode (WT dial-out vs inbound Node).
    public func mode() throws -> Mode {
        var out = IaxMode(0)
        try check(iax_station_mode(handle, &out))
        return Mode(rawValue: Int32(out.rawValue)) ?? .wt
    }

    /// Configure Node mode (listener bind, answer/auth policy, optional
    /// register-as-node). Takes effect on the next switch to Node mode.
    ///
    /// The config is secret-free; the registrar password is supplied only
    /// through the resolver set by `setCredentialResolver(_:)`. Throws
    /// `IAX_ERR_NULL` (`registrar` set without `registerUser`), `IAX_ERR_RESOLVE`
    /// (unparseable `bind`/`registrar` address), or `IAX_ERR_UTF8`.
    public func setNodeConfig(_ config: NodeConfig) throws {
        try check(
            withOptionalCString(config.bind) { bind in
                withOptionalCString(config.registrar) { registrar in
                    withOptionalCString(config.registerUser) { registerUser in
                        var cfg = IaxNodeConfig()
                        cfg.bind = bind
                        cfg.answer = IaxAnswerPolicy(rawValue: UInt32(config.answer.rawValue))
                        cfg.auth = IaxAuthPolicy(rawValue: UInt32(config.auth.rawValue))
                        cfg.registrar = registrar
                        cfg.register_user = registerUser
                        cfg.refresh_secs = config.refreshSecs
                        return iax_station_set_node_config(handle, &cfg)
                    }
                }
            }
        )
    }

    /// Start the inbound IAX2 listener with the given node configuration.
    ///
    /// Binds the UDP listener to `config.bind` (defaulting to `"0.0.0.0:4569"`
    /// when `bind` is nil), and configures the auth/answer policy from `config`.
    /// The station's operating mode is **not** changed — the listener runs
    /// independently of the WT/Node mode flag. Call this to bring up the
    /// always-on inbound path without switching modes.
    ///
    /// The registrar credential for any future authentication challenge is
    /// supplied only via the resolver registered with `setCredentialResolver(_:)`
    /// — no credential is passed here.
    ///
    /// NOTE: binds a UDP socket; call it off any UI thread. Throws
    /// `IAX_ERR_NULL` (nil `st`/`cfg`), `IAX_ERR_RESOLVE` (unparseable `bind`),
    /// `IAX_ERR_LISTEN` (bind failed — port in use, permission denied, etc.),
    /// `IAX_ERR_UTF8`, or `IAX_ERR_PANIC`.
    public func enableInbound(_ config: NodeConfig) throws {
        try withNodeConfigPointers(config) { cfg in
            try check(iax_station_enable_inbound(handle, cfg))
        }
    }

    /// Stop the inbound listener. Idempotent — a no-op if the listener is not
    /// running. The station's operating mode is **not** changed.
    ///
    /// Throws `IAX_ERR_NULL` (nil `st`) or `IAX_ERR_PANIC`.
    public func disableInbound() throws {
        try check(iax_station_disable_inbound(handle))
    }

    /// Start outbound node registration using `config.registrar` /
    /// `config.registerUser` / `config.refreshSecs`. The registrar credential
    /// is resolved on demand via the callback registered with
    /// `setCredentialResolver(_:)` — no credential is passed directly here.
    ///
    /// NOTE: opens a UDP socket; call it off any UI thread. Throws
    /// `IAX_ERR_NULL` (nil `st`, `cfg`, or `registrar`/`registerUser` not set),
    /// `IAX_ERR_RESOLVE` (unparseable `registrar`), `IAX_ERR_UTF8`, or
    /// `IAX_ERR_PANIC`.
    public func register(_ config: NodeConfig) throws {
        try withNodeConfigPointers(config) { cfg in
            try check(iax_station_register(handle, cfg))
        }
    }

    /// Stop outbound node registration. Sends REGREL to the registrar and joins
    /// the registration thread. Idempotent — a no-op when not currently
    /// registered.
    ///
    /// Throws `IAX_ERR_NULL` (nil `st`) or `IAX_ERR_PANIC`.
    public func deregister() throws {
        try check(iax_station_deregister(handle))
    }

    /// Register the credential resolver used to obtain secrets at runtime (e.g.
    /// the registrar password for `setNodeConfig(_:)`'s `registrar`).
    ///
    /// This is the **only** channel for a secret across this binding — secrets
    /// never appear in any config value type, snapshot, event, error, or
    /// description. The closure receives the username being resolved and returns
    /// its secret (return `""` for "no secret"). It may be invoked from a
    /// background thread, so it must be thread-safe.
    ///
    /// The closure is boxed and retained for the lifetime of this station (or
    /// until replaced by another call); the previous box, if any, is released.
    public func setCredentialResolver(_ resolver: @escaping (String) -> String) throws {
        let box = ResolverBox(resolver)
        let retained = Unmanaged.passRetained(box)
        let rc = iax_station_set_credential_resolver(
            handle,
            Station.resolverTrampoline,
            retained.toOpaque()
        )
        if rc < 0 {
            // Registration failed; do not leak the box we just retained.
            retained.release()
            throw StationError.from(rc)
        }
        // Success: release any previously-installed box, then keep this one.
        resolverBox?.release()
        resolverBox = retained
    }

    /// The `@convention(c)` trampoline handed to the C-ABI. It cannot capture
    /// Swift state, so it reaches the boxed closure through `userData`.
    private static let resolverTrampoline:
        @convention(c) (
            UnsafePointer<CChar>?, UnsafeMutablePointer<CChar>?, UInt, UnsafeMutableRawPointer?
        ) -> Int32 = { user, out, cap, userData in
            guard let userData, let out, cap > 0 else { return -1 }
            let box = Unmanaged<ResolverBox>.fromOpaque(userData).takeUnretainedValue()
            let name = user.map { String(cString: $0) } ?? ""
            let secret = box.fn(name)
            // Copy the UTF-8 secret into `out`, truncated to cap-1, NUL-terminated.
            var bytes = Array(secret.utf8)
            let maxLen = Int(cap) - 1
            if bytes.count > maxLen { bytes = Array(bytes.prefix(maxLen)) }
            bytes.withUnsafeBufferPointer { src in
                if let base = src.baseAddress {
                    out.withMemoryRebound(to: UInt8.self, capacity: Int(cap)) { dst in
                        dst.update(from: base, count: src.count)
                        dst[src.count] = 0
                    }
                } else {
                    out[0] = 0
                }
            }
            return 0  // IAX_OK
        }

    // MARK: Inbound (Node/Manual)

    /// Answer the pending inbound offer (Node/Manual). Throws
    /// `IAX_ERR_NOT_CONNECTED` (-3) when not in Node mode or no offer is
    /// pending.
    public func answer() throws {
        try check(iax_station_answer(handle))
    }

    /// Reject the pending inbound offer (Node/Manual). Throws
    /// `IAX_ERR_NOT_CONNECTED` (-3) when not in Node mode or no offer is
    /// pending.
    public func reject() throws {
        try check(iax_station_reject(handle))
    }

    /// Caller id of the most recent `.incoming` event, or `""` if none. The id
    /// is a node identifier — secret-free.
    public func incomingFrom() throws -> String {
        let needed = iax_station_incoming_from(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return "" }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_incoming_from(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return String(cString: buf)
    }


    // MARK: Links (iax-1075)

    /// Connect a node link: registrar-resolved dial of `node` in `mode`.
    /// `secret` is consumed at dial time and never retained. Returns when the
    /// dial is submitted; watch ``nextLinkEvent()`` / ``linkRoster()`` for
    /// liveness.
    public func linkConnect(
        node: String,
        mode: LinkMode,
        callerID: String,
        secret: String,
        permanent: Bool = false
    ) throws {
        try check(
            node.withCString { nodePtr in
                callerID.withCString { cidPtr in
                    secret.withCString { secPtr in
                        iax_station_link_connect(
                            handle, nodePtr, IaxLinkMode(rawValue: mode.rawValue),
                            cidPtr, secPtr, permanent)
                    }
                }
            }
        )
    }

    /// ``linkConnect(node:mode:callerID:secret:permanent:)`` with an explicit
    /// `host:port` address instead of registrar resolution.
    public func linkConnect(
        node: String,
        address: String,
        mode: LinkMode,
        callerID: String,
        secret: String,
        permanent: Bool = false
    ) throws {
        try check(
            node.withCString { nodePtr in
                address.withCString { addrPtr in
                    callerID.withCString { cidPtr in
                        secret.withCString { secPtr in
                            iax_station_link_connect_at(
                                handle, nodePtr, addrPtr, IaxLinkMode(rawValue: mode.rawValue),
                                cidPtr, secPtr, permanent)
                        }
                    }
                }
            }
        )
    }

    /// Tear a link down by node label.
    public func linkDisconnect(node: String) throws {
        try check(node.withCString { iax_station_link_disconnect(handle, $0) })
    }

    /// Change a link's mode (switching to `.transceive` routes the default mic
    /// so the link is immediately key-able).
    public func linkSetMode(node: String, mode: LinkMode) throws {
        try check(
            node.withCString {
                iax_station_link_set_mode(handle, $0, IaxLinkMode(rawValue: mode.rawValue))
            })
    }

    /// Key / unkey a link (throws for non-transmit modes).
    public func linkKey(node: String, on: Bool) throws {
        try check(node.withCString { iax_station_link_key(handle, $0, on) })
    }

    /// Live link roster, decoded from the C-ABI's JSON snapshot.
    public func linkRoster() throws -> LinkRoster {
        let needed = iax_station_link_roster_json(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_link_roster_json(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        let json = String(cString: buf)
        guard let data = json.data(using: .utf8),
            let roster = try? JSONDecoder().decode(LinkRoster.self, from: data)
        else {
            throw StationError(code: -1, text: "undecodable link roster json", detail: "")
        }
        return roster
    }

    /// Who has keyed up on the live digital link, newest first. Empty on
    /// AllStar and while idle. Callsigns are whatever the far end sent.
    public func heard() throws -> [HeardEntry] {
        let needed = iax_station_heard_json(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        var rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_heard_json(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        // A station can key between the sizing call and the fill, which makes
        // the JSON longer than the buffer we just sized: the C ABI reports the
        // size it wanted instead of truncating silently, so grow once and fill
        // again. Once only — the same race can always recur, and a retry loop
        // on a busy reflector would spin.
        if rc > Int32(buf.count - 1) {
            buf = [CChar](repeating: 0, count: Int(rc) + 1)
            rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
                iax_station_heard_json(handle, ptr.baseAddress, UInt(ptr.count))
            }
            if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        }
        let json = String(cString: buf)
        guard let data = json.data(using: .utf8),
            let rows = try? JSONDecoder().decode([HeardEntry].self, from: data)
        else {
            throw StationError(code: -1, text: "undecodable heard json", detail: "")
        }
        return rows
    }

    /// Drain the next pending link lifecycle event, or `nil` when none.
    public func nextLinkEvent() throws -> LinkEvent? {
        var out = IaxLinkEvent(kind: IaxLinkEventKind_None, call: 0, keyed: false)
        let rc = iax_station_link_next_event(handle, &out)
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        if rc == 0 { return nil }
        let node = try linkEventNode()
        switch out.kind {
        case IaxLinkEventKind_Connected: return .connected(node: node, call: out.call)
        case IaxLinkEventKind_Disconnected: return .disconnected(node: node, call: out.call)
        case IaxLinkEventKind_Keyed: return .keyed(node: node, call: out.call, keyed: out.keyed)
        default: return nil
        }
    }

    /// Node label of the most recently drained link event.
    private func linkEventNode() throws -> String {
        let needed = iax_station_link_event_node(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return "" }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_link_event_node(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return String(cString: buf)
    }

    // MARK: WireGuard link transport (iax-912e)

    /// Route the whole engine — outgoing dials, the inbound listener, and
    /// outbound registration — through one shared userspace WireGuard tunnel.
    /// Call BEFORE connect/enable-inbound: the transport is immutable while a
    /// session is up (switching = disconnect/reconnect, then set again). Never
    /// calling this is byte-identical to plain OS UDP.
    ///
    /// `config.privateKey` is passed through to the C-ABI and consumed via a
    /// one-shot resolver; it is never retained here. Throws
    /// `IAX_ERR_ALREADY_CONNECTED` (a call is pooled), `IAX_ERR_RESOLVE`
    /// (unresolvable `endpoint` / unparseable `alsoBindUDP`), or
    /// `IAX_ERR_LINK` (bad key/CIDR).
    public func setWireGuard(_ config: WireGuardConfig) throws {
        // Comma-join the allowed-IPs list for the C-ABI; empty → NULL (none).
        let allowed = config.allowedIPs.isEmpty
            ? nil : config.allowedIPs.joined(separator: ",")
        try check(
            config.endpoint.withCString { endpoint in
                config.peerPublicKey.withCString { peerKey in
                    config.tunnelIP.withCString { tunnelIP in
                        config.privateKey.withCString { privateKey in
                            withOptionalCString(allowed) { allowedIPs in
                                withOptionalCString(config.alsoBindUDP) { alsoBind in
                                    var cfg = IaxWireguardConfig()
                                    cfg.endpoint = endpoint
                                    cfg.peer_public_key = peerKey
                                    cfg.tunnel_ip = tunnelIP
                                    cfg.allowed_ips = allowedIPs
                                    cfg.keepalive_secs = config.keepaliveSecs
                                    cfg.private_key = privateKey
                                    cfg.also_bind_udp = alsoBind
                                    return iax_station_set_wireguard(handle, &cfg)
                                }
                            }
                        }
                    }
                }
            }
        )
    }

    /// Clear the link transport back to plain OS UDP — the inverse of
    /// ``setWireGuard(_:)`` (a NULL config across the C-ABI). Same immutability
    /// rule: throws `IAX_ERR_ALREADY_CONNECTED` while a session is up.
    public func clearWireGuard() throws {
        try check(iax_station_set_wireguard(handle, nil))
    }

    // MARK: M17

    /// Connect to an M17 reflector (iax-f2b8 Task 4/5): resolves `host`/`port`
    /// and opens a full-transceive session on `module`, mutually exclusive
    /// with an active IAX2 call. `module` is case-folded and validated `A`-`Z`
    /// (mirrors the Rust `Station::m17_connect` engine call — codec-dir
    /// search, session exclusivity).
    ///
    /// `module` must be representable as a single ASCII character; a
    /// non-ASCII `Character` throws `StationError` with `IAX_ERR_M17` right
    /// here, before it ever crosses the C-ABI (mirrors `sendDTMF(_:)`'s
    /// guard) — the remaining `A`-`Z` validation (and an empty `callsign`) is
    /// the station's job and also maps to `IAX_ERR_M17`. Throws
    /// `IAX_ERR_ALREADY_CONNECTED` when an IAX2 call is live.
    ///
    /// NOTE: this performs blocking work (device resolve, socket bind,
    /// session spawn); call it off any UI thread.
    public func connectM17(
        host: String,
        port: UInt16 = 17000,
        module: Character,
        callsign: String
    ) throws {
        // A non-ASCII Character can never be a valid module letter, so reject
        // it the same way the C-ABI's own byte guard would.
        guard let ascii = module.asciiValue else {
            throw StationError.from(IAX_ERR_M17)
        }
        try check(
            host.withCString { hostPtr in
                callsign.withCString { callsignPtr in
                    iax_station_connect_m17(
                        handle, hostPtr, port, CChar(bitPattern: ascii), callsignPtr
                    )
                }
            }
        )
    }

    /// Disconnect the live M17 session, if any (iax-f2b8 Task 4/5). Idempotent
    /// — a no-op while idle.
    public func m17Disconnect() throws {
        try check(iax_station_m17_disconnect(handle))
    }

    /// Set extra directories to search for a runtime `libcodec2`, ahead of the
    /// hard-coded system paths (iax-f2b8 Task 4/5) — e.g. to point at an app
    /// bundle's own copy of the library. Joined with `":"` for the C-ABI; an
    /// empty array clears the list back to the system search paths only.
    /// Call before ``connectM17(host:port:module:callsign:)`` — it does not
    /// affect a session already in progress.
    public func setCodecDirs(_ dirs: [String]) throws {
        let joined = dirs.isEmpty ? nil : dirs.joined(separator: ":")
        try check(withOptionalCString(joined) { iax_station_set_codec_dirs(handle, $0) })
    }

    // MARK: Vocoder trust

    /// What a refused AMBE connect says when nothing is trusted. Also the
    /// tail of the thrown error's ``StationError/detail``.
    public static let noTrustedVocoderMessage =
        "No trusted vocoder — add one in Settings → Vocoders"

    /// Replace the vocoder trust list: the USB serials of the AMBE sticks
    /// later D-Star/YSF/NXDN/DMR connects may open. An empty array trusts
    /// nothing. A live session keeps the stick it already opened. Call after
    /// a successful ``enrolVocoder(serial:)`` and after a removal.
    public func setVocoderTrust(_ serials: [String]) throws {
        let joined = serials.isEmpty ? nil : serials.joined(separator: "\n")
        try check(withOptionalCString(joined) { iax_station_set_vocoder_trust(handle, $0) })
    }

    /// The station's current vocoder trust list.
    public func vocoderTrust() throws -> [String] {
        try listDevices { h, buf, len in iax_station_vocoder_trust(h, buf, len) }
    }

    /// Choose D-Star's vocoder: the dongle (the default) or the software
    /// vocoder. Applies to every later ``connectDStar(host:port:module:callsign:reflectorCallsign:)``
    /// and to the snapshot's `dstarAvailable`; a live session keeps the
    /// vocoder it opened.
    ///
    /// D-Star only. System Fusion, NXDN and DMR have no such setting: their
    /// AMBE+2 vocoder is hardware-only (US 8,359,197 until 2028-05-20).
    /// ``DStarVocoder/software`` is accepted in every build; in one without it
    /// (``dstarSoftwareVocoderCompiled`` is `false`) the next D-Star connect
    /// fails and says so, rather than quietly using the dongle.
    public func setDStarVocoder(_ vocoder: DStarVocoder) throws {
        try check(vocoder.rawValue.withCString { iax_station_set_dstar_vocoder(handle, $0) })
    }

    /// The station's current D-Star vocoder choice.
    public func dstarVocoder() throws -> DStarVocoder {
        DStarVocoder(rawValue: try readJSON { h, buf, len in iax_station_dstar_vocoder(h, buf, len) })
            ?? .dongle
    }

    /// Whether this build of the engine contains the software D-Star vocoder
    /// (the `dstar-soft-ambe` feature). Shipped builds do; one made with
    /// `ASTAR_DSTAR_SOFT_AMBE=0 just xcframework` does not.
    public static var dstarSoftwareVocoderCompiled: Bool {
        astar_dstar_software_vocoder_compiled() != 0
    }

    /// Every vocoder-class USB serial port that could be enrolled, by serial
    /// number and port path. **Enumeration only: no port is opened.** Ports
    /// with no USB serial are left out. Includes sticks already trusted.
    public static func listVocoderCandidates() -> [VocoderCandidate] {
        let needed = astar_vocoder_list_candidates(nil, 0)
        guard needed > 0 else { return [] }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            astar_vocoder_list_candidates(ptr.baseAddress, UInt(ptr.count))
        }
        guard rc >= 0, let data = String(cString: buf).data(using: .utf8) else { return [] }
        return (try? JSONDecoder().decode([VocoderCandidate].self, from: data)) ?? []
    }

    /// Enrol the stick with USB serial `serial`: open its port, drive DTR/RTS
    /// low before writing anything, ask the chip for its PRODID (bounded),
    /// close it, and say what it is. **The only call that opens a port that
    /// is not yet trusted**: make it only from an explicit user action, after
    /// the user confirms the device is not a radio's CAT/PTT cable.
    ///
    /// It changes no trust list. On ``VocoderEnrolResult/trusted`` the caller
    /// persists the serial and calls ``setVocoderTrust(_:)``. Every other
    /// result is a refusal whose ``VocoderEnrolment/message`` says why.
    ///
    /// Blocks for up to about two seconds: call it off the main thread.
    /// Throws only for an invalid argument or a caught panic.
    public static func enrolVocoder(serial: String) throws -> VocoderEnrolment {
        var buf = [CChar](repeating: 0, count: 1024)
        let rc = serial.withCString { s in
            buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
                astar_vocoder_enrol(s, ptr.baseAddress, UInt(ptr.count))
            }
        }
        if rc < 0 { throw StationError.from(rc) }
        return VocoderEnrolment.decode(code: rc, json: String(cString: buf))
    }

    // MARK: D-Star

    /// Connect to a D-Star `DExtra` reflector (iax-4c8e): resolves
    /// `host`/`port` and opens a full-transceive session on `module`,
    /// mutually exclusive with both an active IAX2 call and an M17 session.
    /// `module` is case-folded and validated `A`-`Z` by the engine.
    ///
    /// D-Star is HARDWARE-ONLY — the vocoder is a DVSI ThumbDV, with no
    /// software fallback — so this fails when no dongle is attached. Gate the
    /// affordance on ``Snapshot/dstarAvailable`` instead of calling this
    /// speculatively.
    ///
    /// A non-ASCII `module` throws `IAX_ERR_DSTAR` right here, before it
    /// crosses the C-ABI (mirrors ``connectM17(host:port:module:callsign:)``);
    /// the remaining `A`-`Z` validation and an empty `callsign` are the
    /// engine's job and map to the same code. Throws
    /// `IAX_ERR_ALREADY_CONNECTED` when another session is live.
    ///
    /// `reflectorCallsign` names the DESTINATION reflector the way the
    /// directories list it (`"XLX836"`, `"XRF757"`) and fills the transmitted
    /// RF header's `RPT1`/`RPT2` — the fields that say where a transmission
    /// is going. Leave it `nil` to derive it from `host`'s first DNS label
    /// (`xlx836.…` → `XLX836`, addressed as `XRF836` on the DExtra wire),
    /// which is right for every reflector reached by its published hostname.
    /// Pass it when the operator dialled a bare IP address: there is nothing
    /// to derive from there, and the header would otherwise transmit with
    /// `RPT1`/`RPT2` blank.
    ///
    /// NOTE: this blocks for a serial-port scan plus a per-port dongle init
    /// before it ever touches the network — on the order of a second. Call it
    /// off the main thread.
    public func connectDStar(
        host: String,
        port: UInt16 = 30001,
        module: Character,
        callsign: String,
        reflectorCallsign: String? = nil
    ) throws {
        // A non-ASCII Character can never be a valid module letter, so reject
        // it the same way the C-ABI's own byte guard would.
        guard let ascii = module.asciiValue else {
            throw StationError.from(IAX_ERR_DSTAR)
        }
        try check(
            host.withCString { hostPtr in
                callsign.withCString { callsignPtr in
                    withOptionalCString(reflectorCallsign) { reflectorPtr in
                        iax_station_connect_dstar(
                            handle, hostPtr, port, CChar(bitPattern: ascii), callsignPtr,
                            reflectorPtr
                        )
                    }
                }
            }
        )
    }

    /// Link to a YSFReflector and decode the audio on it.
    ///
    /// `host` is `host:port`. YSF publishes a port per reflector and has no
    /// conventional default, so there is nothing to assume — the directory
    /// row carries it. `options` is the YCS room request; leave it `nil` for
    /// a plain reflector.
    ///
    /// HARDWARE-ONLY, like D-Star and for the same reason — the vocoder is
    /// AMBE+2 on a ThumbDV. Gate the affordance on ``Snapshot/ysfAvailable``
    /// rather than calling this speculatively. Throws
    /// `IAX_ERR_ALREADY_CONNECTED` when another session is live.
    ///
    /// NOTE: this blocks for a serial-port scan plus a per-port dongle init
    /// before it ever touches the network — on the order of a second. Call it
    /// off the main thread.
    public func connectYSF(
        host: String,
        callsign: String,
        options: String? = nil
    ) throws {
        try check(
            host.withCString { hostPtr in
                callsign.withCString { callsignPtr in
                    withOptionalCString(options) { optionsPtr in
                        iax_station_connect_ysf(handle, hostPtr, callsignPtr, optionsPtr)
                    }
                }
            }
        )
    }

    /// Disconnect the live YSF link, if any. Idempotent — a no-op while idle.
    public func ysfDisconnect() throws {
        try check(iax_station_ysf_disconnect(handle))
    }

    /// The live YSF link's own state, or `nil` when none is active.
    ///
    /// Cheap, but not as cheap as ``snapshot()`` — it crosses the ABI with a
    /// buffer and parses JSON. Poll ``snapshot()`` for meters; call this at
    /// UI rate for the talker, the link state and the unsupported-mode
    /// warning.
    public func ysfState() throws -> YSFState? {
        let needed = iax_station_ysf_state(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return nil }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_ysf_state(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return YSFState(json: String(cString: buf))
    }

    /// Link to an NXDNReflector and decode the audio on it.
    ///
    /// `host` is `host:port` — NXDN publishes a port per reflector, so there
    /// is nothing to assume; the directory row carries it. `radioID` is this
    /// station's registered NXDN number and `talkgroup` the TG to join. Both
    /// are numbers because NXDN addresses stations by number: there is no
    /// callsign on the wire.
    ///
    /// HARDWARE-ONLY, like D-Star and YSF and for the same reason — the
    /// vocoder is AMBE+2 on a ThumbDV. Gate the affordance on
    /// ``Snapshot/nxdnAvailable`` rather than calling this speculatively.
    ///
    /// RECEIVE ONLY. There is no NXDN transmit path: a key-down is refused,
    /// not queued, so do not offer PTT while this link is live.
    ///
    /// Throws `IAX_ERR_NXDN` for an empty `callsign` or a zero `radioID` /
    /// `talkgroup` (zero is what an unset field looks like, not a
    /// registration), and `IAX_ERR_ALREADY_CONNECTED` when another session is
    /// live.
    ///
    /// NOTE: this blocks for a serial-port scan plus a per-port dongle init
    /// before it ever touches the network — on the order of a second. Call it
    /// off the main thread.
    public func connectNXDN(
        host: String,
        callsign: String,
        radioID: UInt16,
        talkgroup: UInt16
    ) throws {
        try check(
            host.withCString { hostPtr in
                callsign.withCString { callsignPtr in
                    iax_station_connect_nxdn(handle, hostPtr, callsignPtr, radioID, talkgroup)
                }
            }
        )
    }

    /// Disconnect the live NXDN link, if any. Idempotent — a no-op while idle.
    public func nxdnDisconnect() throws {
        try check(iax_station_nxdn_disconnect(handle))
    }

    /// The live NXDN link's own state, or `nil` when none is active.
    ///
    /// Cheap, but not as cheap as ``snapshot()`` — it crosses the ABI with a
    /// buffer and parses JSON. Poll ``snapshot()`` for meters; call this at
    /// UI rate for the link state and the last-heard id.
    public func nxdnState() throws -> NXDNState? {
        let needed = iax_station_nxdn_state(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return nil }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_nxdn_state(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return NXDNState(json: String(cString: buf))
    }

    /// Link to a DMR master's talkgroup and decode the audio on it.
    ///
    /// `system` is a network SLUG — `tgif`, `brandmeister`, … — not a
    /// hostname: each network sets its own terms, and a near-miss resolved to
    /// the wrong one would put the operator on the wrong system under their
    /// own registration, so an unknown slug is refused rather than guessed.
    /// `radioID` is that registration (radioid.net), `talkgroup` the room and
    /// `timeslot` 1 or 2. DMR addresses stations by number: a `DMRD` carries
    /// `srcId` and no callsign, so `callsign` rides only in the login.
    ///
    /// The `password` argument is the master password. It is NOT retained by
    /// this binding: it crosses the ABI as a temporary C string, is copied
    /// into the engine's own buffer for one login digest and dropped there.
    /// Nothing on the station, in a snapshot, in an error or in
    /// ``dmrState()`` ever holds it.
    ///
    /// HARDWARE-ONLY, like D-Star, YSF and NXDN and for the same reason — the
    /// vocoder is AMBE+2 on a ThumbDV. Gate the affordance on
    /// ``Snapshot/dmrAvailable`` rather than calling this speculatively.
    ///
    /// RECEIVE ONLY today. There is no DMR transmit path: a key-down is
    /// refused, not queued, so do not offer PTT while this link is live.
    ///
    /// Throws `IAX_ERR_DMR` for an unknown or not-opted-in network, a zero or
    /// over-wide `radioID`, an empty `callsign` or `password`, a zero
    /// `talkgroup`, a `timeslot` that is not 1 or 2, or a login the master
    /// refused; and `IAX_ERR_ALREADY_CONNECTED` when another session is live.
    ///
    /// NOTE: this blocks for a serial-port scan plus a per-port dongle init
    /// and then a multi-round-trip login to the master — seconds, not
    /// milliseconds. Do not call it on the main thread.
    public func connectDMR(
        system: String,
        host: String,
        port: UInt16,
        radioID: UInt32,
        callsign: String,
        talkgroup: UInt32,
        timeslot: UInt8,
        password: String
    ) throws {
        try check(
            system.withCString { systemPtr in
                host.withCString { hostPtr in
                    callsign.withCString { callsignPtr in
                        password.withCString { passwordPtr in
                            iax_station_connect_dmr(
                                handle, systemPtr, hostPtr, port, radioID, callsignPtr, talkgroup,
                                timeslot, passwordPtr)
                        }
                    }
                }
            }
        )
    }

    /// Disconnect the live DMR link, if any. Idempotent — a no-op while idle.
    public func dmrDisconnect() throws {
        try check(iax_station_dmr_disconnect(handle))
    }

    /// The live DMR link's own state, or `nil` when none is active.
    ///
    /// Cheap, but not as cheap as ``snapshot()`` — it crosses the ABI with a
    /// buffer and parses JSON. Poll ``snapshot()`` for meters; call this at UI
    /// rate for the link state, the failure stage and the last-heard id.
    public func dmrState() throws -> DMRState? {
        let needed = iax_station_dmr_state(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return nil }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_dmr_state(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return DMRState(json: String(cString: buf))
    }

    // MARK: EchoLink

    /// Start logging in to the EchoLink directory as `callsign`.
    ///
    /// `servers` are the directory servers to try in turn (`IP`, `IP:port`,
    /// `name` or `name:port`). The library names no server of its own, so the
    /// caller supplies the list. Returns at once: the login runs on its own
    /// thread, and ``echolinkState()`` shows it finish or fail. A login
    /// already running is replaced.
    ///
    /// The `password` argument is NOT retained by this binding. It crosses
    /// the ABI as a temporary C string for the length of this call, and the
    /// engine copies it into a buffer it zeroes when it is done. Nothing on
    /// the station, in a snapshot, in an error or in ``echolinkState()`` ever
    /// holds it.
    ///
    /// Throws `IAX_ERR_ECHOLINK` for an empty callsign or password, no
    /// non-empty server, or a library built without EchoLink. All of those
    /// are checked before anything starts, so a login already running stands.
    public func echolinkLogin(callsign: String, password: String, servers: [String]) throws {
        let joined = servers.joined(separator: "\n")
        try check(
            callsign.withCString { callsignPtr in
                password.withCString { passwordPtr in
                    joined.withCString { serversPtr in
                        iax_station_echolink_login(handle, callsignPtr, passwordPtr, serversPtr)
                    }
                }
            }
        )
    }

    /// Hang up any EchoLink call and every station on the node's EchoLink
    /// leg, then log off the directory. A no-op when not logged in.
    ///
    /// NOTE: the logoff waits on any directory exchange already in flight, so
    /// this can block for over a minute at worst. Do not call it on the main
    /// thread.
    public func echolinkLogout() throws {
        try check(iax_station_echolink_logout(handle))
    }

    /// Call an EchoLink station, named by node number or by callsign as the
    /// directory lists it, over the selected audio devices. Calling out is
    /// always a person's action: nothing should call this on its own.
    ///
    /// Returns once the first SDES is out; the answer shows in
    /// ``echolinkState()``'s ``EchoLinkState/link`` and in
    /// ``Snapshot/echolinkActive``.
    ///
    /// Throws `IAX_ERR_ECHOLINK` for an empty target, when not logged in, when
    /// no listed station matches or when the UDP ports cannot be opened;
    /// `IAX_ERR_ALREADY_CONNECTED` while any other network or another
    /// EchoLink call holds the station; and `IAX_ERR_AUDIO` when the output
    /// device cannot be opened.
    ///
    /// NOTE: this opens sockets and the output audio device before it
    /// returns. Do not call it on the main thread.
    public func echolinkConnect(_ target: String) throws {
        try check(target.withCString { iax_station_echolink_connect(handle, $0) })
    }

    /// Hang up the EchoLink call, if any. The directory login stays up.
    /// Idempotent.
    public func echolinkDisconnect() throws {
        try check(iax_station_echolink_disconnect(handle))
    }

    /// Ask the EchoLink directory for a fresh station list now, rather than at
    /// the next 600 s refresh; ``echolinkStations()`` has it once it arrives.
    ///
    /// Returns `true` when it was asked, and `false` when nothing was asked
    /// because the last list came less than 30 seconds ago — say "try again
    /// in a moment". Throws `IAX_ERR_ECHOLINK` when not logged in, or in a
    /// library built without EchoLink. It sends one short request to the
    /// directory, so call it off the main thread.
    @discardableResult
    public func echolinkRefreshList() throws -> Bool {
        try check(iax_station_echolink_refresh_list(handle)) == 1
    }

    /// Send one line of text chat to the station on the EchoLink call.
    ///
    /// Throws `IAX_ERR_NOT_CONNECTED` with no call in hand, and
    /// `IAX_ERR_ECHOLINK` for an empty or all-blank line.
    public func echolinkSendChat(_ text: String) throws {
        try check(text.withCString { iax_station_echolink_send_chat(handle, $0) })
    }

    /// Set the location text other stations see beside the callsign. It may
    /// come before a login, which then carries it; while logged in it goes
    /// out with the next status refresh.
    public func echolinkSetLocation(_ text: String) throws {
        try check(text.withCString { iax_station_echolink_set_location(handle, $0) })
    }

    /// Where the EchoLink login and call stand, or `nil` when logged out with
    /// nothing to say (and always in a library built without EchoLink).
    ///
    /// Crosses the ABI with a buffer and parses JSON, so poll it at UI rate,
    /// not meter rate.
    public func echolinkState() throws -> EchoLinkState? {
        EchoLinkState(json: try readJSON(iax_station_echolink_state))
    }

    /// The directory's latest station list: empty when not logged in, before
    /// the first list arrives, and in a library built without EchoLink.
    public func echolinkStations() throws -> [EchoLinkStationEntry] {
        EchoLinkStationEntry.decodeStations(json: try readJSON(iax_station_echolink_stations))
            ?? []
    }

    /// The live M17 session's own state, or `nil` when none is active.
    ///
    /// Cheap, but not as cheap as ``snapshot()`` — it crosses the ABI with a
    /// buffer and parses JSON. Poll ``snapshot()`` for meters and PTT; call
    /// this at UI rate for the talker and the link, exactly as with
    /// ``dstarState()`` and ``ysfState()``.
    public func m17State() throws -> M17State? {
        let needed = iax_station_m17_state(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return nil }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_m17_state(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return M17State(json: String(cString: buf))
    }

    // MARK: Recording

    /// Start recording the live call or digital session into `directory`
    /// (created if missing). `label` is the `<network> <node or reflector>`
    /// part of each file name, e.g. `"AllStar 2000"`; `utcOffsetSeconds` puts
    /// the operator's local date and time in it. `frames` also writes the
    /// codec-frame captures under `directory/captured/`.
    ///
    /// Throws `IAX_ERR_NOT_CONNECTED` with nothing live to record and
    /// `IAX_ERR_RECORD` when already recording, the folder cannot be used or
    /// the disk is short of space (the error's `detail` says which). Creates
    /// files; call it off the main thread.
    public func recordStart(
        directory: String, layout: RecordingLayout, frames: Bool, label: String,
        utcOffsetSeconds: Int32
    ) throws {
        try directory.withCString { dirPtr in
            try label.withCString { labelPtr in
                _ = try check(
                    iax_station_record_start(
                        handle, dirPtr, layout.rawValue, frames, labelPtr, utcOffsetSeconds))
            }
        }
    }

    /// Stop the recording, if one runs, and finish its files. Idempotent — a
    /// no-op while idle. Joins the recorder; call it off the main thread.
    public func recordStop() throws {
        try check(iax_station_record_stop(handle))
    }

    /// The recorder's state. Cheap enough to poll at UI rate for the elapsed
    /// time.
    public func recordState() throws -> RecordState {
        let needed = iax_station_record_state_json(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        var rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_record_state_json(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        // A new file can open between the sizing call and the fill (an over
        // in two-mono adds two paths), so grow once and fill again — once
        // only, as in ``heard()``.
        if rc > Int32(buf.count - 1) {
            buf = [CChar](repeating: 0, count: Int(rc) + 1)
            rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
                iax_station_record_state_json(handle, ptr.baseAddress, UInt(ptr.count))
            }
            if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        }
        let json = String(cString: buf)
        guard let data = json.data(using: .utf8),
            let state = try? JSONDecoder().decode(RecordState.self, from: data)
        else {
            throw StationError(code: -1, text: "undecodable record state json", detail: "")
        }
        return state
    }

    /// Turn the packet log on or off, or point it at another folder. With
    /// `enabled` true it logs into `directory`, keeping at most `maxBytes` of
    /// logs there (less than 100 MB is raised to 100 MB); `utcOffsetSeconds`
    /// puts local time in the file names. With `enabled` false it stops and
    /// finishes the open files, and `directory` may be `nil`.
    ///
    /// Throws `IAX_ERR_NULL` when enabling without a directory, and
    /// `IAX_ERR_RECORD` when the folder cannot be used or the disk is short of
    /// space (the error's `detail` says which; no capture runs then).
    ///
    /// This joins the writer thread, which can take as long as the disk does,
    /// and waits out any other change in progress. Call it off the main thread.
    public func packetLogConfigure(
        enabled: Bool, directory: URL?, maxBytes: Int64, utcOffsetSeconds: Int32
    ) throws {
        let limit = UInt64(max(maxBytes, 0))
        if let directory {
            try directory.path.withCString { dirPtr in
                _ = try check(
                    iax_station_packet_log_configure(
                        handle, enabled ? 1 : 0, dirPtr, limit, utcOffsetSeconds))
            }
        } else {
            _ = try check(
                iax_station_packet_log_configure(
                    handle, enabled ? 1 : 0, nil, limit, utcOffsetSeconds))
        }
    }

    /// Tell the engine the call being captured has ended: each open file ends
    /// with `hangup` and the log stops. A no-op when no log runs.
    ///
    /// This joins the writer thread, like ``packetLogConfigure(enabled:directory:maxBytes:utcOffsetSeconds:)``.
    /// Call it off the main thread.
    public func packetLogCallEnded() throws {
        try check(iax_station_packet_log_call_ended(handle))
    }

    /// The packet log's state. Cheap, and never waits on a change in progress.
    public func packetLogState() throws -> PacketLogState {
        let needed = iax_station_packet_log_state_json(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        var rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_packet_log_state_json(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        // The error text can grow between the sizing call and the fill, so
        // grow once and fill again, as ``recordState()`` does.
        if rc > Int32(buf.count - 1) {
            buf = [CChar](repeating: 0, count: Int(rc) + 1)
            rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
                iax_station_packet_log_state_json(handle, ptr.baseAddress, UInt(ptr.count))
            }
            if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        }
        let json = String(cString: buf)
        guard let data = json.data(using: .utf8),
            let state = try? JSONDecoder().decode(PacketLogState.self, from: data)
        else {
            throw StationError(code: -1, text: "undecodable packet log state json", detail: "")
        }
        return state
    }

    /// Disconnect the live D-Star session, if any (iax-4c8e). Idempotent — a
    /// no-op while idle.
    public func dstarDisconnect() throws {
        try check(iax_station_dstar_disconnect(handle))
    }

    /// The live D-Star session's own state, or `nil` when no session is
    /// active (iax-4c8e).
    ///
    /// Cheap, but not as cheap as ``snapshot()`` — it crosses the ABI with a
    /// buffer and parses JSON. Poll ``snapshot()`` for meters and PTT; call
    /// this at UI rate for the talker and link.
    public func dstarState() throws -> DStarState? {
        let needed = iax_station_dstar_state(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return nil }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_dstar_state(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return DStarState(json: String(cString: buf))
    }

    // MARK: Poll

    /// Latest call state. Cheap; poll it.
    public func snapshot() throws -> Snapshot {
        var out = IaxState()
        try check(iax_station_snapshot(handle, &out))
        var snap = Station.makeSnapshot(out)
        // The names cross separately (the C struct carries only the flags),
        // and only when a flag is set, so a healthy poll pays nothing.
        if out.speaker_stopped { snap.speakerStopped = stoppedName(iax_station_speaker_stopped) }
        if out.mic_stopped { snap.micStopped = stoppedName(iax_station_mic_stopped) }
        return snap
    }

    /// The `IaxState` the engine filled, as a `Snapshot`. It is pulled out of
    /// `snapshot()` so a test can hand it PTT masks, a forced stop and a
    /// time-out that a fresh, idle station never reports, and so pin that
    /// every field is read from its own C field. The stopped-device names are
    /// read by `snapshot()` itself: they need the live handle.
    static func makeSnapshot(_ out: IaxState) -> Snapshot {
        Snapshot(
            status: IaxStatus(rawValue: Int32(out.status.rawValue)) ?? .idle,
            ptt: out.ptt,
            remotePTT: out.remote_ptt,
            txDB: out.tx_db,
            rxDB: out.rx_db,
            inputDB: out.input_db,
            rttMS: out.rtt_ms < 0 ? nil : Int(out.rtt_ms),
            mode: Mode(rawValue: Int32(out.mode.rawValue)) ?? .wt,
            txReanchors: out.tx_reanchors,
            txCaptureOverruns: out.tx_capture_overruns,
            rxUnderruns: out.rx_underruns,
            rxJitterMS: out.rx_jitter_ms,
            rxJitterBufferDepthMS: out.rx_jb_depth_ms,
            rxFramesLost: out.rx_frames_lost,
            rxFramesLate: out.rx_frames_late,
            rxFramesOutOfOrder: out.rx_frames_ooo,
            rxJitterBufferEnabled: out.rx_jb_enabled,
            rxJitterBufferMinMS: out.rx_jb_min_ms,
            rxJitterBufferMaxMS: out.rx_jb_max_ms,
            denoiseChain: DenoiseChain(rawValue: out.denoise_chain.rawValue) ?? .notCapturing,
            denoiseDeviceRate: out.denoise_device_rate,
            denoiseLive: out.denoise_live,
            // 0 (none) and any unknown bit both land as nil.
            negotiatedFormat: VoiceFormat(rawValue: out.negotiated_format),
            dtmfPlayed: Int(out.dtmf_played),
            dtmfTotal: Int(out.dtmf_total),
            m17Available: out.m17_available,
            m17Active: out.m17_active,
            dstarAvailable: out.dstar_available,
            dstarActive: out.dstar_active,
            ysfAvailable: out.ysf_available,
            ysfActive: out.ysf_active,
            nxdnAvailable: out.nxdn_available,
            nxdnActive: out.nxdn_active,
            dmrAvailable: out.dmr_available,
            dmrActive: out.dmr_active,
            echolinkAvailable: out.echolink_available,
            echolinkActive: out.echolink_active,
            echolinkLoggedIn: out.echolink_logged_in,
            pttSources: StationPTTSourceSet(rawValue: out.ptt_sources),
            pttLatched: StationPTTSourceSet(rawValue: out.ptt_latched),
            pttForcedSeq: out.ptt_forced_seq,
            pttForcedSources: StationPTTSourceSet(rawValue: out.ptt_forced_sources),
            // 0 (none) and any unknown reason both land as nil.
            pttForcedReason: StationPTTForcedReason(rawValue: out.ptt_forced_reason),
            pttTxTimeoutMS: out.ptt_tx_timeout_ms,
            voxDB: out.vox_db
        )
    }

    /// Read one stopped-device name, as of the snapshot just taken. `nil`
    /// when the engine has none to give; never a reason to fail the poll.
    private func stoppedName(
        _ fn: (OpaquePointer?, UnsafeMutablePointer<CChar>?, UInt) -> Int32
    ) -> String? {
        let needed = fn(handle, nil, 0)
        guard needed > 0 else { return nil }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            fn(handle, ptr.baseAddress, UInt(ptr.count))
        }
        guard rc >= 0 else { return nil }
        return String(cString: buf)
    }

    /// Drain one queued lifecycle event, or `nil` if none is queued.
    public func nextEvent() throws -> Event? {
        var out = IaxEvent()
        try check(iax_station_next_event(handle, &out))
        switch IaxEventKind(rawValue: Int32(out.kind.rawValue)) ?? .none {
        case .none:
            return nil
        case .answered:
            return .answered
        case .remotePTT:
            return .remotePTT(out.remote_ptt)
        case .hangup:
            return .hangup
        case .modeChanged:
            return .modeChanged
        case .incoming:
            return .incoming
        case .registered:
            return .registered
        case .registerFailed:
            return .registerFailed
        }
    }

    // MARK: Devices

    /// Enumerate input (capture) device names.
    public func listInputs() throws -> [String] {
        try listDevices(iax_station_list_inputs)
    }

    /// Enumerate output (playback) device names.
    public func listOutputs() throws -> [String] {
        try listDevices(iax_station_list_outputs)
    }

    /// Set the capture/playback devices applied to the next connect. `nil`
    /// selects the system default for that direction.
    public func setDevices(input: String?, output: String?) throws {
        try check(
            withOptionalCString(input) { inPtr in
                withOptionalCString(output) { outPtr in
                    iax_station_set_devices(handle, inPtr, outPtr)
                }
            }
        )
    }

    // MARK: - Internals

    /// `duration` in whole milliseconds, truncating sub-millisecond parts and
    /// SATURATING at `0...UInt32.max`. It saturates rather than wraps because
    /// the engine range-checks the result: 2^32 ms + 120 000 ms wrapped would
    /// arrive as a valid 2 min time-out, or a 1.5 s lease.
    @available(macOS 13.0, iOS 16.0, *)
    static func saturatingMilliseconds(_ duration: Duration) -> UInt32 {
        let (seconds, attoseconds) = duration.components
        let (fromSeconds, overflow) = seconds.multipliedReportingOverflow(by: 1_000)
        guard !overflow else { return seconds < 0 ? 0 : .max }
        let (total, sumOverflow) = fromSeconds.addingReportingOverflow(
            attoseconds / 1_000_000_000_000_000)
        guard !sumOverflow else { return fromSeconds < 0 ? 0 : .max }
        return UInt32(clamping: total)
    }

    /// Throw a `StationError` if `code` is negative; otherwise pass it through.
    @discardableResult
    private func check(_ code: Int32) throws -> Int32 {
        if code < 0 {
            throw StationError.from(code, detail: lastErrorDetail())
        }
        return code
    }

    /// The station's detail text for the most recent failure. Best-effort:
    /// an empty string when the station has nothing to say, and never a
    /// reason to fail the call that is already failing.
    private func lastErrorDetail() -> String {
        let needed = iax_station_last_error(handle, nil, 0)
        guard needed > 0 else { return "" }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_last_error(handle, ptr.baseAddress, UInt(ptr.count))
        }
        guard rc >= 0 else { return "" }
        return String(cString: buf)
    }

    /// Read one JSON document through a sizing query (`len == 0`) and a fill.
    /// An empty document reads as `""`, which every decoder takes as "none".
    private func readJSON(
        _ fn: (OpaquePointer?, UnsafeMutablePointer<CChar>?, UInt) -> Int32
    ) throws -> String {
        let needed = fn(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return "" }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            fn(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        return String(cString: buf)
    }

    /// Query the required size (`len == 0`), then fill the buffer. Splits the
    /// newline-joined list into non-empty names.
    private func listDevices(
        _ fn: (OpaquePointer?, UnsafeMutablePointer<CChar>?, UInt) -> Int32
    ) throws -> [String] {
        let needed = fn(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed, detail: lastErrorDetail()) }
        if needed == 0 { return [] }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            fn(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc, detail: lastErrorDetail()) }
        let joined = String(cString: buf)
        return joined.split(separator: "\n").map(String.init)
    }

    // MARK: Config-pointer marshalling

    /// Build the four borrowed `const char*` for `IaxNodeConfig` (NULL for nil)
    /// and invoke `body` with a pointer to a fully-populated `IaxNodeConfig`.
    /// All C strings live only for the duration of `body`. Marked `@discardableResult`
    /// so callers that only care about the throw behaviour don't need `_ =`.
    @discardableResult
    private func withNodeConfigPointers<R>(
        _ config: NodeConfig,
        _ body: (UnsafePointer<IaxNodeConfig>) throws -> R
    ) throws -> R {
        try withOptionalCString(config.bind) { bind in
            try withOptionalCString(config.registrar) { registrar in
                try withOptionalCString(config.registerUser) { registerUser in
                    var cfg = IaxNodeConfig()
                    cfg.bind = bind
                    cfg.answer = IaxAnswerPolicy(rawValue: UInt32(config.answer.rawValue))
                    cfg.auth = IaxAuthPolicy(rawValue: UInt32(config.auth.rawValue))
                    cfg.registrar = registrar
                    cfg.register_user = registerUser
                    cfg.refresh_secs = config.refreshSecs
                    return try body(&cfg)
                }
            }
        }
    }

    /// Build the seven borrowed `const char*` for `IaxConfig` (NULL for nil)
    /// and invoke `body` with a pointer to a fully-populated `IaxConfig`. All C
    /// strings live only for the duration of `body`.
    private static func withConfigPointers<R>(
        _ config: StationConfig,
        _ body: (UnsafePointer<IaxConfig>) -> R
    ) -> R {
        // Nest withOptionalCString so every pointer is valid simultaneously.
        withOptionalCString(config.input) { input in
            withOptionalCString(config.output) { output in
                withOptionalCString(config.portalUser) { user in
                    withOptionalCString(config.portalPass) { pass in
                        withOptionalCString(config.portalNode) { node in
                            withOptionalCString(config.secret) { secret in
                                withOptionalCString(config.codecPolicy) { policy in
                                    var cfg = IaxConfig()
                                    cfg.input = input
                                    cfg.output = output
                                    cfg.portal_user = user
                                    cfg.portal_pass = pass
                                    cfg.portal_node = node
                                    cfg.secret = secret
                                    cfg.codec_policy = policy
                                    return body(&cfg)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

extension Station: CustomStringConvertible {
    /// Secret-free: never reflects config/secret. Only liveness.
    public var description: String { "<Station open>" }
}

// MARK: - Optional C-string helper

/// Run `body` with a borrowed `const char*` for `value` (NULL when `value` is
/// nil). The pointer is valid only for the duration of `body`.
///
/// Free function (not a method) so the marshalling code reads cleanly and can be
/// reused by both throwing and non-throwing closures.
func withOptionalCString<R>(
    _ value: String?,
    _ body: (UnsafePointer<CChar>?) -> R
) -> R {
    if let value {
        return value.withCString { body($0) }
    }
    return body(nil)
}

/// Throwing overload of `withOptionalCString` — identical contract but the
/// closure may throw (and this function rethrows).
func withOptionalCString<R>(
    _ value: String?,
    _ body: (UnsafePointer<CChar>?) throws -> R
) rethrows -> R {
    if let value {
        return try value.withCString { try body($0) }
    }
    return try body(nil)
}
