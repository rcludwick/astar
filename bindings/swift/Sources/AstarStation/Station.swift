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
    /// Continuous mic INPUT level in dBFS, `-60.0...0.0`, metered even while
    /// unkeyed (post-gain, pre-noise-reduction). Drive VOX from this, not
    /// `txDB`, so you can key from silence (iax-5c30).
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

    /// Which vocoder backed this session. Always ``Backend/thumbdv`` today —
    /// D-Star is hardware-only.
    public enum Backend: String, Sendable {
        case thumbdv, soft
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

// MARK: - Error

/// Thrown when an `iax_station_*` call returns a negative code.
///
/// Carries the integer `code` and the library's generic `iax_error_text`. The
/// underlying C strings are `'static` and secret-free, so this error never
/// carries a secret/password.
public struct StationError: Error, CustomStringConvertible, Equatable {
    /// The `IAX_ERR_*` code (always negative).
    public let code: Int32
    /// The generic, secret-free text from `iax_error_text`.
    public let text: String

    public var description: String { "astarstation error \(code): \(text)" }

    /// Build a `StationError` from a negative C code, resolving its text via the
    /// C-ABI's `iax_error_text` (owned by the library; never freed here).
    static func from(_ code: Int32) -> StationError {
        let text: String
        if let raw = iax_error_text(code) {
            text = String(cString: raw)
        } else {
            text = "unknown error"
        }
        return StationError(code: code, text: text)
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
    /// AllStar node selector for token minting (WT path), or `nil`.
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

    public init(
        input: String? = nil,
        output: String? = nil,
        portalUser: String? = nil,
        portalPass: String? = nil,
        portalNode: String? = nil,
        secret: String? = nil,
        codecPolicy: String? = nil
    ) {
        self.input = input
        self.output = output
        self.portalUser = portalUser
        self.portalPass = portalPass
        self.portalNode = portalNode
        self.secret = secret
        self.codecPolicy = codecPolicy
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

    /// Set the input (TX/mic) gain multiplier (clamped `0.0...2.0`).
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

    /// Set the TX trim gain (`0.0...2.0`, clamped; default `1.0` = unity): the
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

    /// Start monitor mode (iax-2377): open the capture device and run the mic
    /// lane WITHOUT a call so a front-end can preview / characterize the mic
    /// before dialing. `input` is a capture-device name substring, or `nil` for
    /// the system default. Idempotent and call-safe: a no-op if a call is
    /// already active (the device is already open) or if a monitor is already
    /// running. Stop it with `monitorStop()`.
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

    /// Characterize the monitored mic (iax-5fb6): run `characterize()` over the
    /// buffered monitor-mode silence and return the resulting `MicProfile` as a
    /// JSON string (secret-free — plain DSP numbers). Empty while not monitoring;
    /// call after a few seconds of monitored silence. `harmonicComb` enables
    /// harmonic-aware notch detection (**default off**: a learned-fundamental
    /// comb that catches rolled-off upper harmonics). Persist the JSON opaquely
    /// per device and feed it back via `setMicProfile(_:)`.
    public func characterize(harmonicComb: Bool = false) throws -> String {
        let needed = iax_station_characterize(handle, harmonicComb, nil, 0)
        if needed < 0 { throw StationError.from(needed) }
        if needed == 0 { return "" }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_characterize(handle, harmonicComb, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc) }
        return String(cString: buf)
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
        if needed < 0 { throw StationError.from(needed) }
        if needed == 0 { return "" }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_incoming_from(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc) }
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
        if needed < 0 { throw StationError.from(needed) }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_link_roster_json(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc) }
        let json = String(cString: buf)
        guard let data = json.data(using: .utf8),
            let roster = try? JSONDecoder().decode(LinkRoster.self, from: data)
        else {
            throw StationError(code: -1, text: "undecodable link roster json")
        }
        return roster
    }

    /// Drain the next pending link lifecycle event, or `nil` when none.
    public func nextLinkEvent() throws -> LinkEvent? {
        var out = IaxLinkEvent(kind: IaxLinkEventKind_None, call: 0, keyed: false)
        let rc = iax_station_link_next_event(handle, &out)
        if rc < 0 { throw StationError.from(rc) }
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
        if needed < 0 { throw StationError.from(needed) }
        if needed == 0 { return "" }
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_link_event_node(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc) }
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
    /// NOTE: this blocks for a serial-port scan plus a per-port dongle init
    /// before it ever touches the network — on the order of a second. Call it
    /// off the main thread.
    public func connectDStar(
        host: String,
        port: UInt16 = 30001,
        module: Character,
        callsign: String
    ) throws {
        // A non-ASCII Character can never be a valid module letter, so reject
        // it the same way the C-ABI's own byte guard would.
        guard let ascii = module.asciiValue else {
            throw StationError.from(IAX_ERR_DSTAR)
        }
        try check(
            host.withCString { hostPtr in
                callsign.withCString { callsignPtr in
                    iax_station_connect_dstar(
                        handle, hostPtr, port, CChar(bitPattern: ascii), callsignPtr
                    )
                }
            }
        )
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
        if needed < 0 { throw StationError.from(needed) }
        if needed == 0 { return nil }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            iax_station_dstar_state(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc) }
        return DStarState(json: String(cString: buf))
    }

    // MARK: Poll

    /// Latest call state. Cheap; poll it.
    public func snapshot() throws -> Snapshot {
        var out = IaxState()
        try check(iax_station_snapshot(handle, &out))
        return Snapshot(
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
            // 0 (none) and any unknown bit both land as nil.
            negotiatedFormat: VoiceFormat(rawValue: out.negotiated_format),
            dtmfPlayed: Int(out.dtmf_played),
            dtmfTotal: Int(out.dtmf_total),
            m17Available: out.m17_available,
            m17Active: out.m17_active,
            dstarAvailable: out.dstar_available,
            dstarActive: out.dstar_active
        )
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

    /// Throw a `StationError` if `code` is negative; otherwise pass it through.
    @discardableResult
    private func check(_ code: Int32) throws -> Int32 {
        if code < 0 {
            throw StationError.from(code)
        }
        return code
    }

    /// Query the required size (`len == 0`), then fill the buffer. Splits the
    /// newline-joined list into non-empty names.
    private func listDevices(
        _ fn: (OpaquePointer?, UnsafeMutablePointer<CChar>?, UInt) -> Int32
    ) throws -> [String] {
        let needed = fn(handle, nil, 0)
        if needed < 0 { throw StationError.from(needed) }
        if needed == 0 { return [] }
        // +1 for the NUL the C-ABI writes.
        var buf = [CChar](repeating: 0, count: Int(needed) + 1)
        let rc = buf.withUnsafeMutableBufferPointer { ptr -> Int32 in
            fn(handle, ptr.baseAddress, UInt(ptr.count))
        }
        if rc < 0 { throw StationError.from(rc) }
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
