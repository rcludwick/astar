// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Combine
import Foundation

/// Observable view-model wrapping a `Station` behind a poll loop. SwiftUI
/// observes the published state; `poll()` advances it from one snapshot.
///
/// Poll + snapshot only (no callbacks). Secrets flow in at connect, never stored.
public final class CallSession: ObservableObject {
    @Published public private(set) var status: IaxStatus = .idle
    @Published public private(set) var ptt = false
    @Published public private(set) var remotePTT = false
    /// Whether the far end is currently talking. Derived from the rx audio level
    /// (gated, with hangover) OR'd with `remotePTT` — most AllStar nodes never
    /// send RADIO_KEY, so `remotePTT` alone misses incoming audio. This is the
    /// reliable "receiving" signal for the RX meter and the menu-bar star.
    @Published public private(set) var receiving = false
    /// High-churn telemetry (VU levels, RTT) lives on its own observable so its
    /// ~20 Hz updates re-render only the meter views, never every view watching
    /// the session (astar-3e04 — the old layout saturated the main thread).
    public let meters = CallMeters()
    /// Negotiated voice codec of the active call, or `nil` while idle / still
    /// negotiating (astar-eb6c). The status card names it via `badge` whenever
    /// set — green-tinted when `.slin16` (wideband) is live (astar-ef35).
    @Published public private(set) var negotiatedFormat: VoiceFormat?
    /// Digits already sent of the active `sendDTMF(sequence:)` command
    /// (astar-7d21); `0` when no sequence is playing. The dialpad dims the
    /// played prefix from this.
    @Published public private(set) var dtmfPlayed = 0
    /// Total digits of the active `sendDTMF(sequence:)` command; `0` when no
    /// sequence is playing — the falling edge to 0 is the dialpad's
    /// "sequence finished" signal.
    @Published public private(set) var dtmfTotal = 0

    /// Whether the engine can dial M17 (iax-f2b8 Task 8): mirrors the
    /// snapshot's `m17Available` flag — codec2 resolved, M17 session support
    /// compiled into the running engine. Gates the M17 network/picker
    /// (`Network.available(m17:)`) until the build actually supports it.
    /// Edge-guarded in `poll()` like the rest of this published state.
    @Published public private(set) var m17Available = false

    /// The node most recently dialed (set at `connect`, cleared at `disconnect`).
    /// Surfaces who we're connected to — e.g. the menu-bar right-click menu.
    /// Display should still gate on `status`, since a stale value can outlive a
    /// remote hangup.
    @Published public private(set) var dialedNode: String?

    /// The network the active/most-recent dial went out on (astar-9b3e). Set
    /// at successful dial dispatch, cleared alongside `dialedNode` wherever
    /// that clears — same "can outlive the call" caveat applies.
    @Published public private(set) var activeCallNetwork: Network?

    /// User-facing message for a dial that went out but was never answered
    /// (astar-9f48). The core reports its give-up as a plain `.hangup` — the
    /// `Failed{reason}` detail doesn't cross the FFI — so `poll()` detects the
    /// edge: `.dialing` observed, then hangup/idle without ever reaching
    /// `.answered`. Cleared on the next dial attempt and on a successful answer;
    /// never set for a user-initiated `disconnect()`.
    @Published public private(set) var lastDialFailure: String?

    /// Mic voice compression (dynamics) is engaged. Mirrors the station; toggled
    /// from the dialing page. Persisted via the audio store.
    @Published public private(set) var compression = false
    /// Compression strength (0…1) applied when `compression` is on. Default 0.90
    /// reproduces today's feel. Persisted via the audio store.
    @Published public private(set) var compressionLevel: Float = 0.90
    /// TX trim (0…2, 1.0 = unity): the always-on final TX gain stage after
    /// compression.
    @Published public private(set) var txTrim: Float = 1.0
    /// Mic noise reduction (denoise) is engaged. Persisted via the audio store.
    @Published public private(set) var noiseReduction = false
    /// RX/output compression (automatic leveling of the received audio,
    /// iax-a4e7) is engaged. Shared across networks — listener-side, not
    /// part of the per-network M17 TX override. Persisted via the audio store.
    @Published public private(set) var rxCompression = false
    /// RX/output compression strength (0…1) applied when `rxCompression` is
    /// on. Default 0.90. Persisted via the audio store.
    @Published public private(set) var rxCompressionLevel: Float = 0.90
    /// Voice-activated PTT: when on, `poll()` keys/unkeys from the mic level
    /// (see `VoxGate`). Persisted via the audio store.
    @Published public private(set) var voxEnabled = false
    /// VOX trigger level (dBFS): mic level at/above which VOX keys. Mirrors the
    /// `VoxGate` threshold; adjustable from the quick-settings slider. Persisted.
    @Published public private(set) var voxThresholdDBFS: Float = -40
    /// VOX hang time (ms): how long PTT stays keyed after the mic drops below the
    /// threshold, bridging brief speech pauses. Mirrors the `VoxGate` hangover;
    /// adjustable from the quick-settings slider. Persisted.
    @Published public private(set) var voxHangtimeMS: Int = 500
    /// Listen-only (monitor) mode. When set, every transmit path — hold-to-talk,
    /// the Spacebar, VOX, and the serial handset — is hard-muted so the radio
    /// never keys. Useful for monitoring a repeater, especially with VOX on.
    /// Persisted via the audio store.
    @Published public private(set) var txDisabled = false
    /// Full-duplex audio. When false (default, half-duplex), VOX won't key while
    /// `receiving` — preventing speaker bleed (e.g. the parrot's playback) from
    /// feeding back. Enable for headphones. Persisted via the audio store.
    @Published public private(set) var fullDuplex = false
    /// Selected mic-profile id (the live choice); `nil` = the built-in Default
    /// (unfiltered). Mirrors the active config; drives the mic-profile pickers.
    @Published public private(set) var micProfileID: String?
    /// The saved mic-profile library (drives the pickers + the Settings list).
    /// Republished on save/rename/delete.
    @Published public private(set) var micProfiles: [MicProfile] = []

    /// When the current continuous transmission started (the instant `ptt` rose
    /// to true), or `nil` while unkeyed. Set on the key edge and cleared on the
    /// unkey edge in `poll()`, so the talk timer measures *this* over-the-air
    /// hold and resets every unkey — mirroring the repeater's TOT. Published so
    /// the dot updates as the poll loop advances it.
    @Published public private(set) var keyedSince: Date?

    /// True while a connect attempt is in flight. Lets the UI show progress and
    /// disable the Connect button to prevent double-taps. Mutated on the main
    /// thread only (see `setConnecting`).
    @Published public private(set) var isConnecting = false

    // MARK: - Dial generation + single-flight (astar-dialrace)
    //
    // Fixes the mid-Connecting hang-up wedge: `dispatchConnect` (MenuPopover)
    // runs the blocking `connect`/`connectM17` engine call on a background
    // queue with no cancellation, while Hang Up calls `disconnect()`
    // synchronously on the main thread. A disconnect landing before the
    // queued connect reaches the engine used to be a silent no-op — the late
    // connect then established a live session the UI no longer tracked, and
    // every subsequent dial saw `AlreadyConnected` (the engine itself is not
    // at fault: it clears a mid-Connecting session correctly whenever
    // `disconnect()`/`m17_disconnect()` actually run — see
    // `.superpowers/sdd/dial-wedge-report.md`).
    //
    // Two mechanisms, both required — a code-review round after the first
    // pass found the generation counter alone wasn't enough (every
    // post-completion write has to be gated, not just the success path;
    // and the check-then-publish step has to be atomic with `disconnect()`),
    // and that a stale dial's own teardown could in principle clobber a
    // genuinely NEWER live session unless overlapping dials are impossible
    // by construction:
    //
    // 1. `_dialGeneration` is a monotonically increasing counter.
    //    - `disconnect()` bumps it first, before its own teardown.
    //    - `claimDial()` also bumps it, right after a dial's validation
    //      guards pass (so a rejected dial — bad target, no credentials,
    //      already in flight — never invalidates an unrelated one).
    //    - `releaseDial(generation:onCurrent:onStale:)` is the ONLY place a
    //      dial's completion may run `onCurrent` (publish success/failure)
    //      — every exit path (success AND the catch/error path) funnels
    //      through it, so no post-completion write can ever escape the
    //      generation guard. If stale, `onStale` runs instead (best-effort
    //      teardown of the session THIS dial just established) and nothing
    //      is published.
    //    - `releaseDial` performs its generation re-check and the resulting
    //      publish/teardown inside `onMainSync` — the SAME mechanism
    //      `disconnect()` uses for its own bump+teardown. GCD's main queue
    //      is strictly serial, so whichever of the two reaches it first runs
    //      to completion before the other starts: the check and the writes
    //      can never interleave with `disconnect()`'s bump+clear (a plain
    //      lock around the counter alone doesn't protect this, since the
    //      writes below it may need to hop to main asynchronously).
    // 2. `dialInFlight` is a single-flight guard, separate from the
    //    generation counter: `claimDial()` throws `ConnectError
    //    .dialInProgress` — touching nothing — if another dial is already
    //    in flight, rather than letting a second engine call start
    //    alongside it. This is what makes the "inverse teardown" hazard (an
    //    older, now-stale dial's completion tearing down a NEWER dial's
    //    just-established live session via `station.disconnect()`)
    //    impossible BY CONSTRUCTION: with only one dial ever in flight per
    //    session, there is never a second, newer live session for a stale
    //    completion to clobber — not merely "unreachable in today's timing"
    //    (latency-based reasoning that a future change could quietly
    //    invalidate), but structurally ruled out. `disconnect()` is exempt
    //    from this guard — hanging up must always be possible mid-dial,
    //    which is the entire point of this fix — it only bumps the
    //    generation; it never claims/releases the single-flight slot.
    //
    // Touched from both the main thread (`disconnect()`, and any `connect`
    // call made directly on main) and the background queue `dispatchConnect`
    // runs on, so the raw counter/flag are a lock-protected `Int`/`Bool`
    // rather than `@Published`/main-thread-only properties like `dialedNode`.
    private let dialGenerationLock = NSLock()
    private var _dialGeneration = 0
    private var dialInFlight = false

    /// Bump the generation, invalidating any dial already in flight. Used by
    /// `disconnect()`, which doesn't touch the single-flight slot (see the
    /// block comment above).
    @discardableResult
    private func bumpDialGeneration() -> Int {
        dialGenerationLock.lock()
        defer { dialGenerationLock.unlock() }
        _dialGeneration += 1
        return _dialGeneration
    }

    /// Whether `generation` (captured via `claimDial()` at the start of a
    /// dial attempt) is still the current one. `false` means a
    /// `disconnect()` has since bumped past it — the caller's
    /// just-established session is stale and must be torn down rather than
    /// published.
    private func isDialGenerationCurrent(_ generation: Int) -> Bool {
        dialGenerationLock.lock()
        defer { dialGenerationLock.unlock() }
        return _dialGeneration == generation
    }

    /// Claim exclusive dialing rights (astar-dialrace, single-flight): throws
    /// `ConnectError.dialInProgress`, touching nothing, if another dial is
    /// already in flight; otherwise marks one in flight and returns a fresh
    /// generation. Always pair with `releaseDial`.
    private func claimDial() throws -> Int {
        dialGenerationLock.lock()
        defer { dialGenerationLock.unlock() }
        guard !dialInFlight else {
            throw ConnectError.dialInProgress
        }
        dialInFlight = true
        _dialGeneration += 1
        return _dialGeneration
    }

    /// Release the in-flight claim and — atomically with respect to
    /// `disconnect()` (both run through `onMainSync`) — run `onCurrent()` if
    /// `generation` is still current, or `onStale()` if a disconnect
    /// invalidated it while the engine call was in flight. Every dial
    /// completion (the success path AND the catch/error path) must funnel
    /// through this; see the block comment above for the full contract.
    @discardableResult
    private func releaseDial<T>(
        generation: Int, onCurrent: () -> T, onStale: () -> T
    ) -> T {
        onMainSync {
            defer {
                dialGenerationLock.lock()
                dialInFlight = false
                dialGenerationLock.unlock()
            }
            return isDialGenerationCurrent(generation) ? onCurrent() : onStale()
        }
    }

    /// Test-only seam (astar-dialrace): if set, invoked on the dial's own
    /// thread the instant its engine call (`connectWT`/`connectM17`)
    /// returns — success or throw — BEFORE `releaseDial`'s generation
    /// re-check runs. Lets a test deterministically interleave a
    /// `disconnect()` in that exact gap to prove the critical section (not
    /// just "usually consistent") handles it. `nil` in production and in
    /// every test that doesn't set it.
    var testPostEngineCallHook: (() -> Void)?

    /// Run `body` on the main thread, synchronously: directly if already
    /// there, `.sync`-dispatched otherwise. This — not the lock above — is
    /// what makes a dial's completion atomic with respect to `disconnect()`:
    /// GCD's main queue is strictly serial, so as long as both this and
    /// `disconnect()` funnel their critical section through here, whichever
    /// reaches the queue first runs to completion before the other starts.
    private func onMainSync<T>(_ body: () throws -> T) rethrows -> T {
        if Thread.isMainThread {
            return try body()
        }
        return try DispatchQueue.main.sync(execute: body)
    }

    var station: StationDriving
    /// Whether AllStar portal credentials are configured. Required to connect:
    /// when true, `connect` dials via the authenticated WebTransceiver path (on
    /// air through the user's node); when false, `connect` refuses with
    /// `ConnectError.needsAccount` (guest dialing was removed, au-1517).
    /// Published so the UI can gate the Connect button on having an account.
    @Published public private(set) var hasCredentials: Bool

    /// The user's M17 callsign (astar-c2e5/iax-f2b8 Task 8): required to
    /// `connect(node:network:)` on `.m17` — the engine sends it verbatim in
    /// every M17 frame, so dialing with an empty one is refused before it
    /// reaches the station (`ConnectError.missingCallsign`). Persisted under
    /// `UserDefaults` key `"m17.callsign"` so it survives relaunches; prefilled
    /// once at `init` from the AllStarLink portal user when nothing is saved
    /// yet and it looks like a callsign (`callsignPrefill(from:)`).
    @Published public var m17Callsign: String = "" {
        didSet {
            guard m17Callsign != oldValue else { return }
            m17Defaults.set(m17Callsign, forKey: Self.m17CallsignKey)
        }
    }
    private static let m17CallsignKey = "m17.callsign"
    /// Backing store for `m17Callsign`. Injected (defaults to `.standard`) so
    /// tests can assert persistence without touching real defaults.
    private let m17Defaults: UserDefaults

    /// The persisted M17 TX-processing override (astar-5d8e): noise
    /// reduction, compression, compression strength, TX trim, and mic
    /// (input) gain (the last joined at astar-m17defaults), applied to the
    /// station in place of the shared `AudioSettings` values while an M17
    /// call is being dialed/live. `UserDefaults`-backed under the same
    /// `m17Defaults` instance as `m17Callsign` (so tests share one scratch
    /// suite for all M17 state); mutated only via the `setM17*` setters below,
    /// which publish + persist + (edge-triggered) push live.
    @Published public private(set) var m17Overrides: M17AudioOverrides
    private enum M17OverrideKey {
        static let noiseReduction = "m17.audio.noiseReduction"
        static let compression = "m17.audio.compression"
        static let compressionLevel = "m17.audio.compressionLevel"
        static let txTrim = "m17.audio.txTrim"
        static let inputGain = "m17.audio.inputGain"
    }
    private static func loadM17Overrides(_ defaults: UserDefaults) -> M17AudioOverrides {
        M17AudioOverrides(
            noiseReduction: defaults.bool(forKey: M17OverrideKey.noiseReduction),
            // `compression`'s default flipped to `true` (astar-m17defaults, Rob's
            // field-tested recipe) — unlike `noiseReduction` (default `false`,
            // where `.bool(forKey:)`'s own absent-key fallback already matches),
            // an absent key needs the explicit `true` fallback `.bool(forKey:)`
            // can't express, so this reads like the Float fields below.
            compression: defaults.object(forKey: M17OverrideKey.compression) != nil
                ? defaults.bool(forKey: M17OverrideKey.compression) : true,
            compressionLevel: defaults.object(forKey: M17OverrideKey.compressionLevel) != nil
                ? defaults.float(forKey: M17OverrideKey.compressionLevel) : 0.80,
            txTrim: defaults.object(forKey: M17OverrideKey.txTrim) != nil
                ? defaults.float(forKey: M17OverrideKey.txTrim) : 0.80,
            inputGain: defaults.object(forKey: M17OverrideKey.inputGain) != nil
                ? defaults.float(forKey: M17OverrideKey.inputGain) : 0.25)
    }
    private func saveM17Overrides() {
        m17Defaults.set(m17Overrides.noiseReduction, forKey: M17OverrideKey.noiseReduction)
        m17Defaults.set(m17Overrides.compression, forKey: M17OverrideKey.compression)
        m17Defaults.set(m17Overrides.compressionLevel, forKey: M17OverrideKey.compressionLevel)
        m17Defaults.set(m17Overrides.txTrim, forKey: M17OverrideKey.txTrim)
        m17Defaults.set(m17Overrides.inputGain, forKey: M17OverrideKey.inputGain)
    }

    private var timer: Timer?

    /// Optional PTT source, ticked once per poll. Lets a platform-specific PTT
    /// driver (e.g. the macOS-only serial UCI150 interface) key the call without
    /// `AstarCore` importing that driver — keeping the core multiplatform and
    /// IOKit-free. Called inside `poll()` after the snapshot is read, with the
    /// snapshot's `remotePTT` + `rxDB`. Return a non-nil `Bool` to drive
    /// `setPTT` (true = key, false = unkey); return `nil` for "no change this
    /// tick" so the call's PTT is left untouched. Errors from the underlying
    /// driver are the driver's concern — it should return `nil` (and tear itself
    /// down) on failure.
    public var pttSourceTick: ((_ remoteKeyed: Bool, _ rxDb: Float) -> Bool?)?

    /// Store used to persist mic-processing + VOX prefs from their setters
    /// (read-modify-write so device/gain fields are preserved).
    private let audioStore: AudioSettingsStore
    /// Node directory: a successful connect auto-records the dialed node as a
    /// recent (see `poll()`). Injected so tests can use an in-memory store.
    private let directoryStore: NodeDirectoryStore
    /// Resolves a node number to a display name. Seeded with the directory as its
    /// only source; a future online lookup (astar-6c65) appends a second source.
    private let nameResolver: NameResolver
    /// Per-mic characterization profiles, keyed by device name. Injected so
    /// tests can use an in-memory store.
    private let micProfileStore: MicProfileStore
    /// Whether the current call already recorded its recent. Set when status
    /// reaches `.answered`; reset on a fresh dial so each connect records once.
    private var recordedRecentForCall = false
    /// Armed once `poll()` observes `.dialing` for the current dial; dropped on
    /// answer, on the no-answer edge firing, and — crucially — at the top of
    /// `disconnect()` (before the hangup reaches the station) so a user-initiated
    /// hangup can never be misreported as "didn't answer".
    private var dialAwaitingAnswer = false
    /// The app-global spectrum peak-hold decay (dB/s) last pushed to the engine,
    /// so `poll()` can re-assert it on the edge into `.answered` (when the call's
    /// TX/RX analyzers come up at the engine's default and need the configured
    /// value re-applied). Defaults to the engine default (100). Updated by
    /// `setSpectrumDecay(_:)`. `internal` so the `CallSession+Devices` passthrough
    /// (a separate file) can record the value.
    var spectrumDecayDbPerSec: Float = 100
    /// Tracks the previous poll's status so we can fire one-shot work on the
    /// edge into `.answered` (re-asserting the spectrum decay onto the freshly
    /// created TX/RX analyzers).
    private var lastPolledStatus: IaxStatus = .idle
    /// VOX gate state; advanced from `poll()`'s `txDB` while `voxEnabled`.
    /// `internal` (not `private`) so tests can assert the threshold/hangtime
    /// setters reach the gate's config.
    var voxGate = VoxGate()
    /// Detects "receiving" from the rx audio level with a hangover so it doesn't
    /// flicker on speech gaps. Same tuning the menu-bar star used previously.
    private var rxActivityGate = VoxGate(config: VoxConfig(thresholdDBFS: -50, hangoverMS: 400))
    /// ~250 ms sliding-max smoothers behind `txDBHeld`/`rxDBHeld`.
    private var txPeak = PeakHold(window: 0.25)
    private var rxPeak = PeakHold(window: 0.25)
    /// Last PTT level VOX drove, so we only call `setPTT` on a change (edge).
    private var voxKeyed = false

    /// The user's intended RX (output/speaker) gain multiplier — the value to
    /// restore when half-duplex RX suppression releases. Tracks `setOutputGain`
    /// and `applyAudioSettings`; defaults to unity. `internal` so tests can
    /// assert the suppress/restore drives the right value.
    var desiredOutputGain: Float = 1.0
    /// Whether RX is currently hard-muted for half-duplex (PTT keyed while not
    /// full-duplex). Edge-tracked in `poll()` so we only push a gain change to
    /// the station on the key/unkey transition, not every tick. `internal` for
    /// tests.
    var rxMutedForHalfDuplex = false

    public init(
        station: StationDriving, hasCredentials: Bool = false,
        audioStore: AudioSettingsStore = UserDefaultsAudioSettingsStore(),
        directoryStore: NodeDirectoryStore = UserDefaultsNodeDirectoryStore(),
        micProfileStore: MicProfileStore = UserDefaultsMicProfileStore(),
        credentials: Credentials? = nil,
        userDefaults: UserDefaults = .standard
    ) {
        self.station = station
        self.hasCredentials = hasCredentials
        self.audioStore = audioStore
        self.directoryStore = directoryStore
        // Directory first; the online AllStarLink-DB source (astar-6c65) appends
        // after it so a user's saved label always wins over a remote callsign.
        self.nameResolver = NameResolver(sources: [DirectoryNameSource(directoryStore)])
        self.micProfileStore = micProfileStore
        self.micProfileID = audioStore.load().micProfileID
        self.micProfiles = micProfileStore.all()
        self.m17Defaults = userDefaults
        self.m17Overrides = CallSession.loadM17Overrides(userDefaults)
        // Load the saved callsign; when nothing's saved yet, prefill from the
        // portal user if it's shaped like a callsign (astar-c2e5).
        let storedCallsign = userDefaults.string(forKey: Self.m17CallsignKey) ?? ""
        self.m17Callsign =
            storedCallsign.isEmpty
            ? (CallSession.callsignPrefill(from: credentials?.portalUser) ?? "")
            : storedCallsign
    }

    /// Prefill the M17 callsign field from the configured AllStarLink portal
    /// user (astar-c2e5/iax-f2b8 Task 8) — many hams reuse the same handle as
    /// their portal login. Matches only when `user` is shaped like a ham
    /// callsign (`^[A-Za-z]{1,2}[0-9][A-Za-z]{1,3}$`, case-insensitive, e.g.
    /// "AJ7HR" or "W1AW"); returns `nil` for `nil`/empty/non-matching input so
    /// the caller falls back to an empty field. Pure — testable without
    /// constructing a session.
    public static func callsignPrefill(from user: String?) -> String? {
        guard let user, !user.isEmpty else { return nil }
        guard
            user.range(of: "^[A-Za-z]{1,2}[0-9][A-Za-z]{1,3}$", options: .regularExpression)
                != nil
        else { return nil }
        return user.uppercased()
    }

    /// Swap the underlying station + credential availability at runtime (e.g.
    /// after the user saves/clears AllStar credentials, which requires rebuilding
    /// the station). Restarts polling if it was running.
    public func reconfigure(station: StationDriving, hasCredentials: Bool) {
        let wasRunning = timer != nil
        stop()
        self.station = station
        self.hasCredentials = hasCredentials
        if wasRunning { start() }
    }

    /// Begin polling at `interval` (default 20 Hz). Polls once immediately so the
    /// UI shows fresh state without waiting a tick. Idempotent.
    public func start(interval: TimeInterval = 0.05) {
        stop()
        poll()
        timer = Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { [weak self] _ in
            self?.poll()
        }
    }

    /// Stop the poll loop.
    public func stop() {
        timer?.invalidate()
        timer = nil
    }

    /// Advance published state from one snapshot, then drain queued lifecycle
    /// events so the station's queue never grows. Called on each poll tick.
    public func poll() {
        if let snap = try? station.readSnapshot() {
            // Publish-on-change throughout (astar-3e04): `@Published` fires
            // objectWillChange on every write, changed or not, and this runs
            // 20×/s — unconditional assignment kept the whole UI re-rendering
            // forever. Only a real edge may touch a published property.
            if status != snap.status { status = snap.status }
            if ptt != snap.ptt { ptt = snap.ptt }
            // Talk-timer key tracking: stamp the start of a continuous transmit
            // on the rising edge and clear it on the falling edge, so the timer
            // measures *this* hold and resets every unkey (mirrors the repeater
            // TOT). Covers every key source — the snapshot's `ptt` already folds
            // in the PTT button, VOX, and the serial handset.
            if snap.ptt {
                if keyedSince == nil { keyedSince = Date() }
            } else if keyedSince != nil {
                keyedSince = nil
            }
            if remotePTT != snap.remotePTT { remotePTT = snap.remotePTT }
            if negotiatedFormat != snap.negotiatedFormat {
                negotiatedFormat = snap.negotiatedFormat
            }
            if dtmfPlayed != snap.dtmfPlayed { dtmfPlayed = snap.dtmfPlayed }
            if dtmfTotal != snap.dtmfTotal { dtmfTotal = snap.dtmfTotal }
            if m17Available != snap.m17Available { m17Available = snap.m17Available }
            // Quarter-second peak-hold for the VU meters (astar-f78a) so they read
            // steadily instead of flickering at the poll rate.
            let meterNow = Date()
            meters.update(
                txDB: snap.txDB, rxDB: snap.rxDB, inputDB: snap.inputDB,
                txDBHeld: txPeak.push(snap.txDB, now: meterNow),
                rxDBHeld: rxPeak.push(snap.rxDB, now: meterNow),
                rttMS: snap.rttMS)

            // Half-duplex RX suppression (astar-eaab). When NOT full-duplex and
            // the local transmit is keyed, hard-mute RX so the operator doesn't
            // hear their own TX returned — a local AllStar node mixes the keyed
            // audio back into the RX stream (the "echo"). Restore the user's
            // configured output gain the instant PTT drops. Driven off the
            // snapshot's `ptt` so it covers every key source (hold-to-talk, VOX,
            // serial UCI150). Full-duplex (headphones) leaves RX untouched so
            // simultaneous TX+RX still works. Edge-only: push a gain change to
            // the station solely on the mute/unmute transition.
            let shouldMuteRX = !fullDuplex && snap.ptt
            if shouldMuteRX != rxMutedForHalfDuplex {
                rxMutedForHalfDuplex = shouldMuteRX
                try? station.setOutputGain(shouldMuteRX ? 0 : desiredOutputGain)
            }

            // On a SUCCESSFUL connect (status reaches .answered), auto-record the
            // dialed node as a recent — once per call (the flag is reset on each
            // fresh dial). Re-dialing the same node updates its lastUsed rather
            // than duplicating (recordRecent upserts by node).
            if snap.status == .answered, !recordedRecentForCall, let node = dialedNode {
                recordedRecentForCall = true
                // Stamp the network the call actually went out on (astar-c2e5)
                // rather than always defaulting to AllStar.
                directoryStore.recordRecent(
                    node: node, label: node, network: activeCallNetwork ?? .allstar)
            }

            // The complementary edge (astar-9f48): a dial went out (`.dialing`
            // observed) and the status fell back to hangup/idle without ever
            // answering — the core's give-up, reported over the FFI as a plain
            // `.hangup`. Publish the no-answer message. `dialedNode` is nil
            // after a user `disconnect()` (which also drops the latch first),
            // so an intentional hangup can't misfire.
            switch snap.status {
            case .dialing:
                if dialedNode != nil { dialAwaitingAnswer = true }
            case .answered:
                dialAwaitingAnswer = false
                if lastDialFailure != nil { lastDialFailure = nil }
            case .hangup, .idle:
                if dialAwaitingAnswer {
                    dialAwaitingAnswer = false
                    if let node = dialedNode {
                        lastDialFailure = "Node \(node) didn’t answer."
                    }
                }
            }

            // Re-assert the configured spectrum decay on the edge into `.answered`
            // (astar-68a6). The engine's `setSpectrumDecay` only touches analyzers
            // that exist when it's called; the call's TX/RX analyzers come up here,
            // at the engine's 100 dB/s default — so push the app-global value again
            // now that they're live. Edge-only (one push per connect) to stay cheap.
            if snap.status == .answered, lastPolledStatus != .answered {
                try? station.setSpectrumDecay(dbPerSecond: spectrumDecayDbPerSec)
            }
            // M17 TX-override teardown on the poll path (astar-5d8e): a call
            // that ends without the user calling `disconnect()` — the remote
            // end/node drops us — never reaches that method, so this edge is
            // the only place that restores the standard TX chain and clears
            // the stale `activeCallNetwork` for that case. `disconnect()`
            // already clears `activeCallNetwork` synchronously before this
            // ever runs again, so a user-initiated hangup can't double-fire
            // this (edge-triggered on the transition, like the spectrum-decay
            // check above).
            if activeCallNetwork == .m17, snap.status == .hangup || snap.status == .idle,
                lastPolledStatus != .hangup, lastPolledStatus != .idle
            {
                restoreStandardTxProcessing()
                setActiveCallNetwork(nil)
            }
            lastPolledStatus = snap.status
            // "Receiving" = far end keyed (when the node reports it) OR live rx
            // audio activity (the reliable signal for most AllStar nodes). The
            // gate runs every tick so its hangover advances.
            let nowReceiving =
                snap.remotePTT || rxActivityGate.update(level: snap.rxDB, now: Date())
            if receiving != nowReceiving { receiving = nowReceiving }

            // Software VOX: feed the mic level to the gate and key/unkey on the
            // edge. A separate PTT source from `pttSourceTick`/hold-to-talk; when
            // enabled it auto-keys from the voice level.
            if voxEnabled && !txDisabled {
                // Half-duplex (default): don't let VOX key while receiving — the
                // far end's audio bleeding from the speaker into the mic would
                // feed back (the parrot loop). Release any held key.
                if !fullDuplex && receiving {
                    if voxKeyed {
                        voxKeyed = false
                        keyStation(false)
                    }
                } else {
                    // Key from the continuous mic input level (metered even while
                    // unkeyed, iax-5c30) — not txDB, which is floored until we key.
                    let keyed = voxGate.update(level: snap.inputDB, now: Date())
                    if keyed != voxKeyed {
                        voxKeyed = keyed
                        keyStation(keyed)
                    }
                }
            }

            // Drive PTT from the optional source (serial UCI150, software VOX,
            // …) off this same snapshot, on the main thread, satisfying the
            // station's single-thread contract. A non-nil result is a keying
            // edge to forward; `nil` means "leave PTT alone this tick".
            if let pttSourceTick, let on = pttSourceTick(snap.remotePTT, snap.rxDB) {
                keyStation(on)
            }
        }
        while (try? station.readEvent()) != nil {}
    }

    // MARK: - Commands (pass through to the station)

    /// Why a connect was refused before dialing.
    public enum ConnectError: Error, LocalizedError {
        /// No AllStarLink account is configured. Connections require one — guest
        /// dialing was removed (au-1517) so every call goes on air as the user's
        /// node, never anonymously.
        case needsAccount
        /// The requested network isn't dialable yet (astar-9b3e): the engine
        /// only speaks AllStar today. Defensive-only — the UI never offers a
        /// network outside `Network.available`, so this should be unreachable
        /// from normal use.
        case unsupportedNetwork
        /// The M17 dial field's text doesn't parse as `host[:port]/module`
        /// (astar-c2e5) — see `M17Dial.parse(_:)`.
        case badM17Target
        /// M17 requires a callsign: the engine sends it in every frame, so a
        /// dial with an empty `m17Callsign` is refused before it ever reaches
        /// the station (astar-c2e5).
        case missingCallsign
        /// Another dial is already in flight (astar-dialrace, single-flight):
        /// refused, touching nothing, rather than let a second engine call
        /// start alongside it. Transient — retry once the in-flight dial's
        /// engine call returns (which it always does; nothing here can hang
        /// forever). This is what makes the "inverse teardown" hazard (a
        /// stale dial's completion tearing down a NEWER dial's live session)
        /// impossible by construction rather than merely unreachable today.
        case dialInProgress

        public var errorDescription: String? {
            switch self {
            case .needsAccount:
                return "Add your AllStarLink account in Settings to connect."
            case .unsupportedNetwork:
                return "astar's engine doesn't support Hamlink yet."
            case .badM17Target:
                return
                    "Enter an M17 target as host[:port]/module, "
                    + "e.g. m17-reflector.example:17000/A."
            case .missingCallsign:
                return "Enter your callsign to connect via M17."
            case .dialInProgress:
                return "Already connecting — try again in a moment."
            }
        }
    }

    /// Dial a node via the authenticated WebTransceiver path (routes through the
    /// user's node — on air). **Requires an AllStarLink account**: with no
    /// credentials configured this throws `ConnectError.needsAccount` and does
    /// not dial (guest mode was removed in au-1517).
    ///
    /// **Blocking worker** — the WT path mints a portal token over HTTP (~400ms),
    /// so callers must run this OFF the main thread and hop back to the main actor
    /// to publish results. Before dialing it tears down any prior call (a failed
    /// or partial connect leaves the station mid-call, which makes a fresh dial
    /// return IAX -4 "a call is already in progress"). The teardown is best-effort;
    /// its error is ignored.
    public func connect(node: String) throws {
        try connect(node: node, address: nil)
    }

    /// Dial on a specific network (astar-9b3e). `.allstar` is today's dial,
    /// verbatim; `.hamlink` is defensive-only until the engine gains reflector
    /// support (iax-b3d7) — the UI never offers it while unavailable.
    public func connect(node: String, network: Network) throws {
        switch network {
        case .allstar:
            // `publishNetwork: true` makes `connectAllStar` stamp
            // `activeCallNetwork = .allstar` itself, INSIDE its
            // `releaseDial` `onCurrent` closure (astar-dialrace, review
            // round 3) — atomic with the generation re-check, same as
            // `connectM17` does for `.m17`. This used to be a separate
            // `setActiveCallNetwork(.allstar)` call made by this wrapper
            // AFTER `connectAllStar` returned: a disconnect landing in the
            // gap between "connectAllStar's atomic critical section decided
            // success" and "this out-of-band write actually ran" could get
            // clobbered right back to `.allstar` — a smaller instance of the
            // exact bug this whole fix targets, just one layer up.
            try connectAllStar(node: node, address: nil, publishNetwork: true)
        case .hamlink:
            throw ConnectError.unsupportedNetwork
        case .m17:
            try connectM17(target: node)
        }
    }

    /// The `.m17` arm of `connect(node:network:)` (astar-c2e5/iax-f2b8 Task 8).
    /// Parses `target` via `M17Dial.parse` and requires a non-empty
    /// `m17Callsign` BEFORE touching any state (mirrors the `needsAccount`
    /// guard: a bad dial string or missing callsign must leave the session
    /// untouched). Only then tears down any stale prior call, records the
    /// raw dial string as `dialedNode` (so the status card shows it, same as
    /// the AllStar arm), and dials — `activeCallNetwork` is set only once
    /// `connectM17` itself succeeds, so a failed dial never reports `.m17` as
    /// active.
    private func connectM17(target: String) throws {
        guard let parsed = M17Dial.parse(target) else {
            throw ConnectError.badM17Target
        }
        let callsign = m17Callsign.trimmingCharacters(in: .whitespaces)
        guard !callsign.isEmpty else {
            throw ConnectError.missingCallsign
        }
        // Note (astar-5d8e): if the PREVIOUS M17 call already ended engine-side
        // but `poll()`'s teardown edge hasn't ticked over it yet,
        // `activeCallNetwork` is still stale at `.m17` here and the standard
        // chain was never restored — this M17→M17 redial doesn't need that to
        // have happened, though: `pushM17TxOverrides()` below unconditionally
        // re-pushes the (possibly just-edited) M17 set on every successful
        // dial, so the race self-heals rather than leaking anything.
        //
        // Claim exclusive dialing rights (astar-dialrace) now that validation
        // has passed — throws `.dialInProgress`, touching nothing, if
        // another dial is already in flight (single-flight; see
        // `claimDial()`'s doc comment).
        let generation = try claimDial()
        try? station.disconnect()
        setDialedNode(target)
        recordedRecentForCall = false  // new call → allow one recents record on .answered
        dialAwaitingAnswer = false  // fresh dial: latch re-arms when .dialing is observed
        setLastDialFailure(nil)  // and any stale no-answer message clears
        do {
            try station.connectM17(
                host: parsed.host, port: parsed.port, module: parsed.module, callsign: callsign)
        } catch {
            testPostEngineCallHook?()
            // Every post-completion write, success AND failure, is
            // generation-gated (astar-dialrace): a disconnect that landed
            // while `connectM17` was in flight already published whatever
            // the UI should show, so a stale failure here must not clobber
            // it back to "failed" (`dialedNode` was never established at
            // the engine on this path, so there's nothing to tear down).
            releaseDial(
                generation: generation,
                onCurrent: { setDialedNode(nil) },  // failed dial — drop the intent
                onStale: {})
            throw error
        }
        testPostEngineCallHook?()
        // A disconnect bumped the generation past ours while `connectM17`
        // was blocking: the session we just established is stale. Tear it
        // back down (best-effort) and publish nothing — no
        // `activeCallNetwork`, no TX-override push, no recents latch — the
        // superseding disconnect already published whatever the UI should
        // show. Both the check and the writes below run atomically with
        // `disconnect()`'s own bump+teardown (`releaseDial` → `onMainSync`).
        releaseDial(
            generation: generation,
            onCurrent: {
                setActiveCallNetwork(.m17)
                // The M17 TX-processing override goes live the instant the
                // dial succeeds (astar-5d8e) — not at answer — mirroring how
                // `activeCallNetwork` itself is set here.
                pushM17TxOverrides()
            },
            onStale: {
                try? station.m17Disconnect()
                try? station.disconnect()
            })
    }

    /// Dial `node` over the WebTransceiver path, optionally overriding the dialed
    /// address. When `address` is a non-empty "host:port" (IP or hostname), the WT
    /// token is minted exactly as usual but the call dials `address` directly
    /// instead of resolving the node through the registrar — for reaching a node
    /// whose registrar-advertised IP is unroutable from here (Advanced options;
    /// e.g. your own node on localhost). A `nil`/empty `address` is the normal
    /// registrar-resolved dial. Same account requirement and blocking-worker
    /// contract as `connect(node:)`.
    public func connect(node: String, address: String?) throws {
        // `publishNetwork: false` preserves this entry point's existing
        // semantics (astar-c7a1 point 6): the Advanced-options manual-address
        // path — and `connect(node:)` below, which also lands here — has
        // never stamped `activeCallNetwork` itself; `poll()`'s recents
        // recording already falls back to `.allstar` when it's `nil`
        // (`activeCallNetwork ?? .allstar`), so nothing regresses. Only
        // `connect(node:network:)`'s `.allstar` arm passes `true`.
        try connectAllStar(node: node, address: address, publishNetwork: false)
    }

    /// Core of the AllStar/WT dial, shared by `connect(node:)` (via
    /// `connect(node:address:)`) and `connect(node:network:)`'s `.allstar`
    /// arm. `publishNetwork` controls whether a successful dial stamps
    /// `activeCallNetwork = .allstar` itself — done, when true, INSIDE
    /// `releaseDial`'s `onCurrent` closure, atomically with the generation
    /// re-check (astar-dialrace, review round 3): a separate out-of-band
    /// write by the caller, made AFTER this function returns, would reopen
    /// a narrower version of the same race — a disconnect landing in the gap
    /// between "this function decided success" and "the caller's write
    /// actually runs" would get clobbered right back to `.allstar`.
    private func connectAllStar(node: String, address: String?, publishNetwork: Bool) throws {
        // No account → refuse before mutating any state or dialing.
        guard hasCredentials else {
            NSLog("[astar] connect refused node=%@: no account", node)
            throw ConnectError.needsAccount
        }
        // Claim exclusive dialing rights (astar-dialrace) — throws
        // `.dialInProgress`, touching nothing, if another dial is already in
        // flight (single-flight; see `claimDial()`'s doc comment). Done
        // before `wasM17`/`restoreStandardTxProcessing()` below so a refused
        // dial never touches TX-processing state either.
        let generation = try claimDial()
        // An AllStar dial starting undoes a still-live M17 TX override
        // (astar-5d8e) — covers both the network-aware `connect(node:network:)`
        // wrapper (which sets `activeCallNetwork` to `.allstar` only after this
        // returns) and the Advanced-options manual-address path, which calls
        // straight through to here without ever going via that wrapper.
        // `wasM17` is captured now (not re-read below) so the failure path
        // knows whether THIS attempt was the one leaving M17, even though
        // `activeCallNetwork` itself isn't touched again until success. This
        // read-then-immediate-act is NOT subject to the same staleness
        // concern as the writes below: single-flight guarantees no other
        // dial can be running concurrently to make it stale between being
        // read and acted on a statement later.
        let wasM17 = activeCallNetwork == .m17
        if wasM17 {
            restoreStandardTxProcessing()
        }
        try? station.disconnect()
        setDialedNode(node)
        recordedRecentForCall = false  // new call → allow one recents record on .answered
        dialAwaitingAnswer = false  // fresh dial: latch re-arms when .dialing is observed
        setLastDialFailure(nil)  // and any stale no-answer message clears
        applyMicProfile(id: micProfileID)
        do {
            if let address, !address.isEmpty {
                NSLog(
                    "[astar] connect node=%@ addr=%@ path=WT(account, manual addr)", node, address)
                try station.connectWT(destNode: node, address: address)
            } else {
                NSLog("[astar] connect node=%@ path=WT(account, on-air)", node)
                try station.connectWT(destNode: node)
            }
        } catch {
            testPostEngineCallHook?()
            // Every post-completion write, success AND failure, is
            // generation-gated (astar-dialrace): a disconnect that landed
            // while `connectWT` was in flight already published whatever the
            // UI should show (including undoing a stale M17 TX override via
            // its own `restoreStandardTxProcessing()` call) — a stale
            // failure here must not clobber `activeCallNetwork`/`dialedNode`
            // back on top of that. When still current, this mirrors the
            // original behavior: a dial that started leaving M17 but failed
            // (portal token mint, unreachable address, …) must not leave
            // `activeCallNetwork` stuck at `.m17` — the standard TX chain was
            // already restored above, so the network context has to agree —
            // and the attempted node is dropped from `dialedNode` the same
            // way MenuPopover used to (moved here so the drop is
            // generation-gated too; see astar-0217/dialrace).
            releaseDial(
                generation: generation,
                onCurrent: {
                    if wasM17 { setActiveCallNetwork(nil) }
                    setDialedNode(nil)
                },
                onStale: {})
            throw error
        }
        testPostEngineCallHook?()
        // A disconnect bumped the generation past ours while `connectWT` was
        // blocking: the session we just established is stale
        // (astar-dialrace). Tear it back down (best-effort) and publish
        // nothing on top of it. Both the check and the publish/teardown run
        // atomically with `disconnect()`'s own bump+teardown (`releaseDial`
        // → `onMainSync`) — `activeCallNetwork = .allstar` is stamped HERE,
        // inside `onCurrent`, not by the caller afterward (see the doc
        // comment above), so there's no out-of-band write left for a
        // disconnect to race against.
        releaseDial(
            generation: generation,
            onCurrent: {
                if publishNetwork { setActiveCallNetwork(.allstar) }
            },
            onStale: {
                try? station.disconnect()
            })
    }

    /// Set the in-flight flag. Hops to the main thread because `isConnecting` is
    /// `@Published`; safe to call from any thread (e.g. a background connect task).
    public func setConnecting(_ value: Bool) {
        if Thread.isMainThread {
            isConnecting = value
        } else {
            DispatchQueue.main.async { [weak self] in self?.isConnecting = value }
        }
    }

    /// Key (`true`) or unkey (`false`) the local transmit. Honors listen-only:
    /// a key request while `txDisabled` is forced to unkeyed.
    public func setPTT(_ on: Bool) throws {
        try station.setPTT(on && !txDisabled)
    }

    /// Single choke point for the auto/edge keying sources (VOX, serial). Forces
    /// unkeyed while listen-only so the radio never transmits. Best-effort.
    private func keyStation(_ on: Bool) {
        try? station.setPTT(on && !txDisabled)
    }

    /// Enable/disable listen-only (monitor) mode. When enabling, fail-safe unkey
    /// and drop any VOX hold so nothing is left on air. Persisted.
    public func setTxDisabled(_ on: Bool) {
        txDisabled = on
        if on {
            voxGate = VoxGate()
            voxKeyed = false
            try? station.setPTT(false)
        }
        persistAudio { $0.txDisabled = on }
    }

    /// Enable/disable full-duplex audio. Half-duplex (off) keeps VOX from keying
    /// while receiving, so speaker bleed can't feed back. Persisted.
    public func setFullDuplex(_ on: Bool) {
        fullDuplex = on
        // Turning full-duplex ON while RX is suppressed for a keyed half-duplex
        // transmit: restore RX now rather than waiting for the next poll edge,
        // so headphone users hear the far end immediately.
        if on && rxMutedForHalfDuplex {
            rxMutedForHalfDuplex = false
            try? station.setOutputGain(desiredOutputGain)
        }
        persistAudio { $0.fullDuplex = on }
    }

    /// Validate WebTransceiver token minting (login + mint + discard) without
    /// placing a call (astar-2fde). Blocking network call — run it off the main
    /// thread. Throws on bad credentials / unknown node / network failure.
    public func testTokenMint() throws {
        try station.testMintToken()
    }

    /// Hang up the current call.
    public func disconnect() throws {
        // The entire body runs through `onMainSync` (astar-dialrace): this is
        // what makes it atomic with respect to a dial's own completion
        // (`releaseDial`, which funnels through the same helper) — GCD's
        // main queue is strictly serial, so whichever of the two reaches it
        // first runs to completion before the other starts. Without this, a
        // disconnect landing between a dial's generation re-check and its
        // (possibly main-thread-hopping) publish could recreate a smaller
        // version of the original race.
        try onMainSync {
            // Bump the dial generation FIRST, before anything else here —
            // this invalidates any dial currently in flight so that if it
            // completes later, it tears its own zombie session down instead
            // of silently reviving `dialedNode`/`activeCallNetwork`. See the
            // block comment above `dialGenerationLock` for the full
            // contract. Note: this does NOT touch the single-flight
            // `dialInFlight` slot — hanging up must always be possible
            // mid-dial regardless of whether one is in flight; only the
            // dial that claimed it ever releases it.
            bumpDialGeneration()
            // Drop the no-answer latch BEFORE the hangup reaches the station: the
            // poll loop must never see hangup/idle with the latch still armed for a
            // user-initiated disconnect (that would misreport "didn't answer").
            dialAwaitingAnswer = false
            // `Station.disconnect()` already tears down an M17 session engine-side
            // (`ConsoleSession.disconnect`), so this is belt-and-suspenders — but
            // calling `m17Disconnect()` explicitly keeps the M17-specific teardown
            // observable on `NullStation`/`FakeStation` (astar-c2e5) and reads
            // clearly at the call site rather than relying on an implementation
            // detail one layer down. Best-effort: `disconnect()` below still runs
            // (and still propagates) regardless.
            if activeCallNetwork == .m17 {
                try? station.m17Disconnect()
                // The M17 call is ending — undo its TX override (astar-5d8e)
                // before the network is cleared below.
                restoreStandardTxProcessing()
            }
            try station.disconnect()
            setDialedNode(nil)
            setActiveCallNetwork(nil)
        }
    }

    /// Record (or clear) the dialed node. `@Published`, so hop to the main thread —
    /// `connect` runs on a background worker.
    public func setDialedNode(_ node: String?) {
        if Thread.isMainThread {
            dialedNode = node
        } else {
            DispatchQueue.main.async { [weak self] in self?.dialedNode = node }
        }
    }

    /// Record (or clear) the active call's network. `@Published`, so hop to the
    /// main thread — same reason as `setDialedNode`: `connect` runs on a
    /// background worker, and this mirrors `dialedNode`'s lifecycle exactly
    /// (set at dial dispatch, cleared at `disconnect()`).
    private func setActiveCallNetwork(_ network: Network?) {
        if Thread.isMainThread {
            activeCallNetwork = network
        } else {
            DispatchQueue.main.async { [weak self] in self?.activeCallNetwork = network }
        }
    }

    /// Set (or clear) the no-answer message. `@Published`, so same main-thread
    /// hop as `setDialedNode` — `connect` clears it from a background worker.
    private func setLastDialFailure(_ message: String?) {
        if Thread.isMainThread {
            lastDialFailure = message
        } else {
            DispatchQueue.main.async { [weak self] in self?.lastDialFailure = message }
        }
    }

    /// Toggle mic voice compression: set the published flag, push to the station,
    /// and persist (preserving the other audio prefs).
    public func setCompression(_ on: Bool) {
        compression = on
        try? station.setCompression(on)
        persistAudio { $0.compression = on }
    }

    /// Set the mic compression strength (0…1): publish, push to the station, and
    /// persist (preserving the other audio prefs).
    public func setCompressionLevel(_ level: Float) {
        compressionLevel = level
        try? station.setCompressionLevel(level)
        persistAudio { $0.compressionLevel = level }
    }

    /// Set the TX trim gain (0…2, 1.0 = unity; the engine clamps): publish, push
    /// to the station, and persist (preserving the other audio prefs).
    public func setTxTrim(_ gain: Float) {
        txTrim = gain
        try? station.setTxTrim(gain)
        persistAudio { $0.txTrim = gain }
    }

    /// Toggle mic noise reduction: flag + station + persist.
    public func setNoiseReduction(_ on: Bool) {
        noiseReduction = on
        try? station.setNoiseReduction(on)
        persistAudio { $0.noiseReduction = on }
    }

    /// Toggle RX/output compression (automatic leveling of the received
    /// audio, iax-a4e7): set the published flag, push to the station, and
    /// persist. Shared across networks — no M17 override, unlike the TX
    /// processing toggles above.
    public func setRxCompression(_ on: Bool) {
        rxCompression = on
        try? station.setRxCompression(on)
        persistAudio { $0.rxCompression = on }
    }

    /// Set the RX/output compression strength (0…1): publish, push to the
    /// station, and persist.
    public func setRxCompressionLevel(_ level: Float) {
        rxCompressionLevel = level
        try? station.setRxCompressionLevel(level)
        persistAudio { $0.rxCompressionLevel = level }
    }

    // MARK: - M17 TX-processing override (astar-5d8e)

    /// Toggle the M17 override's noise reduction: publish, persist, and — if
    /// an M17 call is live right now — push it to the station immediately
    /// (Rob: "people may want to use it though if they find they have a
    /// better result", so editing mid-call must take effect right away, not
    /// just at the next M17 dial).
    public func setM17NoiseReduction(_ on: Bool) {
        m17Overrides.noiseReduction = on
        saveM17Overrides()
        if activeCallNetwork == .m17 {
            try? station.setNoiseReduction(on)
        }
    }

    /// Toggle the M17 override's voice compression: publish, persist, push
    /// live while an M17 call is active (see `setM17NoiseReduction`).
    public func setM17Compression(_ on: Bool) {
        m17Overrides.compression = on
        saveM17Overrides()
        if activeCallNetwork == .m17 {
            try? station.setCompression(on)
        }
    }

    /// Set the M17 override's compression strength (0…1): publish, persist,
    /// push live while an M17 call is active (see `setM17NoiseReduction`).
    public func setM17CompressionLevel(_ level: Float) {
        m17Overrides.compressionLevel = level
        saveM17Overrides()
        if activeCallNetwork == .m17 {
            try? station.setCompressionLevel(level)
        }
    }

    /// Set the M17 override's TX trim (0…2, 1.0 = unity): publish, persist,
    /// push live while an M17 call is active (see `setM17NoiseReduction`).
    public func setM17TxTrim(_ gain: Float) {
        m17Overrides.txTrim = gain
        saveM17Overrides()
        if activeCallNetwork == .m17 {
            try? station.setTxTrim(gain)
        }
    }

    /// Set the M17 override's mic (input) gain (0…2, unity 1.0): publish,
    /// persist, push live while an M17 call is active (see
    /// `setM17NoiseReduction`). Output gain has no M17 counterpart — it
    /// stays whatever the shared `AudioSettings` says for both networks.
    public func setM17InputGain(_ gain: Float) {
        m17Overrides.inputGain = gain
        saveM17Overrides()
        if activeCallNetwork == .m17 {
            try? station.setInputGain(gain)
        }
    }

    /// Whether voice compression is on for whichever profile is in effect
    /// right now (astar-5d8e): the M17 override while an M17 call is active,
    /// otherwise the standard/AllStar setting. Every reader of "the current
    /// compression flag" outside `QuickConfigView` (which has its own local
    /// `m17Context`, folding in the network *picker* too) should read this
    /// rather than `compression` directly, so a menu-bar toggle can't
    /// silently act on the wrong profile.
    public var effectiveCompression: Bool {
        activeCallNetwork == .m17 ? m17Overrides.compression : compression
    }

    /// Whether noise reduction is on for whichever profile is in effect right
    /// now — see `effectiveCompression`.
    public var effectiveNoiseReduction: Bool {
        activeCallNetwork == .m17 ? m17Overrides.noiseReduction : noiseReduction
    }

    /// Flip voice compression for whichever profile is in effect right now
    /// (astar-5d8e): the M17 override while an M17 call is active, otherwise
    /// the standard/AllStar setting. Extracted so every "flip the current
    /// value" entry point (the menu-bar right-click toggle; `QuickConfigView`
    /// binds its own switch instead, since it already knows which profile
    /// it's showing) shares this one routing decision rather than each
    /// re-deriving it — a toggle that always called `setCompression` would
    /// clobber a live M17 override and quietly persist into the shared
    /// `AudioSettings` once the call ends and the standard chain is restored.
    public func toggleCompression() {
        if activeCallNetwork == .m17 {
            setM17Compression(!m17Overrides.compression)
        } else {
            setCompression(!compression)
        }
    }

    /// Flip noise reduction for whichever profile is in effect right now —
    /// see `toggleCompression`.
    public func toggleNoiseReduction() {
        if activeCallNetwork == .m17 {
            setM17NoiseReduction(!m17Overrides.noiseReduction)
        } else {
            setNoiseReduction(!noiseReduction)
        }
    }

    /// Push the M17 TX-processing override onto the station — called once a
    /// `.m17` dial succeeds. Reuses the same five station setters
    /// `applyAudioSettings` pushes, just fed `m17Overrides` instead of the
    /// shared `AudioSettings` (astar-5d8e; `inputGain` joined the rest at
    /// astar-m17defaults). Output gain is deliberately absent — it has no
    /// M17 override, so it's never pushed here; it's already whatever the
    /// shared `AudioSettings` last set it to.
    private func pushM17TxOverrides() {
        try? station.setNoiseReduction(m17Overrides.noiseReduction)
        try? station.setCompression(m17Overrides.compression)
        try? station.setCompressionLevel(m17Overrides.compressionLevel)
        try? station.setTxTrim(m17Overrides.txTrim)
        try? station.setInputGain(m17Overrides.inputGain)
    }

    /// Restore the shared `AudioSettings`' TX-processing fields (plus mic
    /// gain, astar-m17defaults) onto the station and mirror them into
    /// published state — called when an M17 call ends (`disconnect()`, or
    /// the poll-path edge for a remote hangup) or an AllStar dial starts,
    /// undoing whatever M17 override was live. Mirrors the relevant slice of
    /// `applyAudioSettings`; devices, output gain, and VOX are untouched
    /// here — M17 never overrides those (astar-5d8e).
    private func restoreStandardTxProcessing() {
        let s = audioStore.load()
        try? station.setCompression(s.compression)
        try? station.setCompressionLevel(s.compressionLevel)
        try? station.setTxTrim(s.txTrim)
        try? station.setNoiseReduction(s.noiseReduction)
        try? station.setInputGain(s.inputGain)
        compression = s.compression
        compressionLevel = s.compressionLevel
        txTrim = s.txTrim
        noiseReduction = s.noiseReduction
    }

    /// Enable/disable voice-activated PTT. When turning off, fail-safe unkey if
    /// VOX currently held the line. Persisted.
    public func setVoxEnabled(_ on: Bool) {
        voxEnabled = on
        if !on {
            voxGate = VoxGate()
            if voxKeyed {
                voxKeyed = false
                try? station.setPTT(false)
            }
        }
        persistAudio { $0.voxEnabled = on }
    }

    /// Set the VOX trigger level (dBFS). Updates the live gate and persists.
    public func setVoxThreshold(_ dbfs: Float) {
        voxThresholdDBFS = dbfs
        voxGate.config.thresholdDBFS = dbfs
        persistAudio { $0.voxThresholdDBFS = dbfs }
    }

    /// Set the VOX hang time (ms). Updates the live gate and persists.
    public func setVoxHangtime(_ ms: Int) {
        voxHangtimeMS = ms
        voxGate.config.hangoverMS = ms
        persistAudio { $0.voxHangtimeMS = ms }
    }

    // MARK: - Mic characterization

    /// Open the mic lane without a call so a front-end can preview/characterize it
    /// (the engine shares the live path if a call is active).
    public func monitorStart(input: String?) throws {
        try station.monitorStart(input: input)
        // The mic analyzer comes up at the engine's default decay; re-assert the
        // app-global value onto it now that it's live (astar-68a6). Best-effort.
        try? station.setSpectrumDecay(dbPerSecond: spectrumDecayDbPerSec)
    }

    /// Close the monitor mic lane (no-op if a call is using it).
    public func monitorStop() throws { try station.monitorStop() }

    /// How many front-ends currently want the monitor mic lane open (the Mic
    /// Analyzer and the VOX calibration meter can both want it at once). The lane
    /// is opened on the 0→1 edge and closed on the 1→0 edge so one consumer
    /// closing doesn't pull the mic out from under the other.
    private var monitorRetainCount = 0

    /// Reference-counted `monitorStart`: open the mic lane if it isn't already
    /// (re-asserting `input` each time is harmless — the engine guards a double
    /// open). Pair every call with `monitorRelease()`. Lets independent UI (Mic
    /// Analyzer, VOX calibration) share the live mic without fighting over it.
    public func monitorRetain(input: String?) throws {
        // Re-assert on the cold first retain so the chosen device is honored;
        // later retains piggyback on the already-open lane.
        if monitorRetainCount == 0 {
            try station.monitorStart(input: input)
            // Fresh mic-analyzer lane → re-assert the app-global spectrum decay
            // onto it (astar-68a6); the engine brings it up at its own default.
            try? station.setSpectrumDecay(dbPerSecond: spectrumDecayDbPerSec)
        }
        monitorRetainCount += 1
    }

    /// Balance a `monitorRetain()`: close the lane only when the last holder
    /// releases. Over-releasing is clamped (never goes negative).
    public func monitorRelease() throws {
        guard monitorRetainCount > 0 else { return }
        monitorRetainCount -= 1
        if monitorRetainCount == 0 {
            try station.monitorStop()
        }
    }

    /// The live peak-held voice-band spectrum (dBFS bins). Poll ~20 Hz while a
    /// spectrum view is visible.
    public func micSpectrum() throws -> [Float] { try station.micSpectrum() }

    /// The live peak-held TX (transmit) voice-band spectrum (dBFS bins), for the
    /// in-call overlaid FFT. Poll ~20 Hz only while the spectrum view is open.
    public func txSpectrum() throws -> [Float] { try station.txSpectrum() }

    /// The live peak-held RX (receive) voice-band spectrum (dBFS bins), for the
    /// in-call overlaid FFT. Poll ~20 Hz only while the spectrum view is open.
    public func rxSpectrum() throws -> [Float] { try station.rxSpectrum() }

    /// Characterize the monitored mic from buffered silence; returns opaque
    /// `MicProfile` JSON (`""` when not enough silence has been captured).
    public func characterize(harmonicComb: Bool) throws -> String {
        try station.characterize(harmonicComb: harmonicComb)
    }

    /// Apply (or clear) a raw profile JSON on the live/next call.
    public func setMicProfile(_ json: String?) throws { try station.setMicProfile(json) }

    /// The saved profile with `id`, or `nil`.
    public func micProfile(id: String?) -> MicProfile? {
        id.flatMap { micProfileStore.profile(id: $0) }
    }

    /// Add/replace a profile in the library (republishes `micProfiles`).
    public func saveMicProfile(_ profile: MicProfile) {
        micProfileStore.save(profile)
        micProfiles = micProfileStore.all()
    }

    /// Rename a saved profile (republishes `micProfiles`).
    public func renameMicProfile(id: String, to name: String) {
        guard var p = micProfileStore.profile(id: id) else { return }
        p.name = name
        micProfileStore.save(p)
        micProfiles = micProfileStore.all()
    }

    /// Delete a profile from the library. If it was the live selection, fall back
    /// to the Default (unfiltered) profile. Republishes `micProfiles`.
    public func deleteMicProfile(id: String) {
        micProfileStore.remove(id: id)
        micProfiles = micProfileStore.all()
        if micProfileID == id { setMicProfileSelection(id: nil) }
    }

    /// Apply the profile with `id` to the live/next call: its characterization
    /// JSON when found, otherwise (incl. `nil` = Default) clear to unfiltered.
    /// Single engine-apply path.
    public func applyMicProfile(id: String?) {
        if let id, let p = micProfileStore.profile(id: id) {
            try? station.setMicProfile(p.characterizationJSON)
        } else {
            try? station.setMicProfile(nil)  // Default — no filtering
        }
    }

    /// Set the live mic-profile selection: publish it, persist it (so call-start
    /// recalls it), and apply it now. `nil` = Default (unfiltered).
    public func setMicProfileSelection(id: String?) {
        micProfileID = id
        persistAudio { $0.micProfileID = id }
        applyMicProfile(id: id)
    }

    // MARK: - Node directory (favorites + recents)

    /// Favorited nodes, sorted by label — for the dialing-page directory picker.
    public func directoryFavorites() -> [NodeEntry] { directoryStore.favorites }

    /// Recently-connected nodes, newest first (capped) — for the picker.
    public func directoryRecents() -> [NodeEntry] { directoryStore.recents }

    /// Resolve a node number to a display name (saved favorite/directory label),
    /// or `nil` when unknown — for showing names wherever a bare number appears.
    /// Backed by a `NameResolver` so a second source (the online AllStarLink-DB
    /// lookup, astar-6c65) can plug in later without touching call sites.
    public func name(forNode node: String) -> String? {
        nameResolver.name(forNode: node)
    }

    /// The display name for a node, falling back to the bare number when unknown.
    public func displayName(forNode node: String) -> String {
        nameResolver.displayName(forNode: node)
    }

    /// The connected-call header text for `node` — the saved name when known
    /// (e.g. "AJ7HR (77777)"), else "node 77777". `node` is whatever
    /// `dialedNode` holds, which for an M17 call is the raw dial string
    /// rather than an AllStar node number (the directory simply won't have a
    /// name for it, so this falls back to "node <target>" the same way).
    /// Shared by the status card (`MenuPopover`) and the accessibility
    /// announcer's "Connected to …" text (astar-b167) so the two can never
    /// format the same call differently.
    public func connectedTargetLabel(for node: String) -> String {
        if let name = nameResolver.name(forNode: node) {
            return "\(name) (\(node))"
        }
        return "node \(node)"
    }

    /// The directory entry for a node number, or `nil` when not saved — for
    /// resolving the per-node talk-timer override.
    public func directoryEntry(forNode node: String) -> NodeEntry? {
        directoryStore.all().first { $0.node == node }
    }

    /// Set (or clear) the per-node talk-timer override on a favorite by id.
    /// Passing `nil` for a field inherits the global default for that aspect.
    /// Bumps `lastUsed`-independent; preserves the rest of the entry. No-op when
    /// the id is unknown.
    public func directorySetTalkTimer(
        id: String, enabled: Bool?, seconds: Int?
    ) {
        guard var entry = directoryStore.all().first(where: { $0.id == id }) else { return }
        entry.talkTimerEnabled = enabled
        entry.talkTimerSeconds = seconds
        directoryStore.upsert(entry)
    }

    /// The live talk-timer phase for the connected node, or `nil` when the dot
    /// should not show: not keyed, no dialed node, or the timer is disabled for
    /// that node. The view reads this each poll and maps it to a color.
    ///
    /// - `defaultEnabled` / `defaultLimitSeconds` come from the app-global
    ///   `@AppStorage` defaults (the view passes them in so the core stays free
    ///   of UI-storage concerns).
    public func talkTimerPhase(
        defaultEnabled: Bool, defaultLimitSeconds: Int, now: Date = Date()
    ) -> TalkTimer.Phase? {
        guard ptt, let keyedSince, let node = dialedNode else { return nil }
        let entry = directoryEntry(forNode: node)
        guard
            let limit = TalkTimer.resolvedLimitSeconds(
                entry: entry, defaultEnabled: defaultEnabled,
                defaultLimitSeconds: defaultLimitSeconds)
        else { return nil }
        let elapsed = now.timeIntervalSince(keyedSince)
        return TalkTimer.phase(elapsed: elapsed, limit: TimeInterval(limit))
    }

    /// Add (or update) a favorite from the Settings manager: a label + node.
    /// Validates the node (non-empty digits) and dedupes **by node** — an existing
    /// entry for that number is favorited and relabeled in place (preserving its
    /// id/lastUsed) rather than duplicated. Returns `true` when saved.
    @discardableResult
    public func directoryAddFavorite(node: String, label: String) -> Bool {
        let trimmedNode = node.trimmingCharacters(in: .whitespaces)
        guard !trimmedNode.isEmpty, trimmedNode.allSatisfy(\.isNumber) else {
            return false
        }
        addFavorite(node: trimmedNode, label: label)
        return true
    }

    /// Rename a directory entry by id (preserving node/favorite/lastUsed). Empty
    /// labels are ignored so a row can't lose its name.
    public func directoryRename(id: String, to label: String) {
        let trimmed = label.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty,
            var entry = directoryStore.all().first(where: { $0.id == id })
        else { return }
        entry.label = trimmed
        directoryStore.upsert(entry)
    }

    /// Remove a directory entry by id — the Settings manager's delete.
    public func directoryRemove(id: String) {
        directoryStore.remove(id: id)
    }

    /// Whether `node` is currently a favorite.
    public func isFavorite(node: String) -> Bool {
        directoryStore.all().contains { $0.node == node && $0.favorite }
    }

    /// Favorite a node with a label (callsign/name). Upserts by node so it merges
    /// with an existing recent/entry rather than duplicating, preserving lastUsed.
    /// `network` stamps a BRAND NEW entry's network (astar-c2e5, default
    /// `.allstar` — today's only caller-reachable network); an existing
    /// entry's network is left as-is (it was already correctly stamped by
    /// whatever `recordRecent`/earlier `addFavorite` created it, and a caller
    /// re-favoriting it may not have the right network in scope — e.g. the
    /// Settings "Add favorite" row, which only ever adds AllStar numbers).
    public func addFavorite(node: String, label: String, network: Network = .allstar) {
        let trimmedNode = node.trimmingCharacters(in: .whitespaces)
        guard !trimmedNode.isEmpty else { return }
        let label = label.trimmingCharacters(in: .whitespaces)
        var list = directoryStore.all()
        if let idx = list.firstIndex(where: { $0.node == trimmedNode }) {
            list[idx].favorite = true
            if !label.isEmpty { list[idx].label = label }
            directoryStore.upsert(list[idx])
        } else {
            directoryStore.upsert(
                NodeEntry(
                    label: label.isEmpty ? trimmedNode : label,
                    node: trimmedNode, favorite: true, network: network))
        }
    }

    /// Un-favorite a node (keeps it as a recent if it has a `lastUsed`; otherwise
    /// removes it entirely so the directory doesn't accumulate dead entries).
    public func removeFavorite(node: String) {
        let entries = directoryStore.all().filter { $0.node == node }
        for var e in entries {
            if e.lastUsed != nil {
                e.favorite = false
                directoryStore.upsert(e)
            } else {
                directoryStore.remove(id: e.id)
            }
        }
    }

    /// Read-modify-write the persisted audio prefs so toggling one field doesn't
    /// clobber the devices/gains/other toggles.
    private func persistAudio(_ mutate: (inout AudioSettings) -> Void) {
        var s = audioStore.load()
        mutate(&s)
        audioStore.save(s)
    }

    /// Push persisted audio preferences to the station (called at launch and
    /// whenever the user changes them). Best-effort — errors are ignored. Also
    /// reflects the mic-processing + VOX flags into published state.
    public func applyAudioSettings(_ settings: AudioSettings) {
        try? station.setDevices(input: settings.input, output: settings.output)
        try? station.setInputGain(settings.inputGain)
        // Remember the user's intended RX gain (the value to restore after a
        // half-duplex mute). Only push it through now if RX isn't currently
        // suppressed; if it is, keep it muted and let the unkey edge restore it.
        desiredOutputGain = settings.outputGain
        if !rxMutedForHalfDuplex {
            try? station.setOutputGain(settings.outputGain)
        }
        try? station.setCompression(settings.compression)
        try? station.setCompressionLevel(settings.compressionLevel)
        try? station.setTxTrim(settings.txTrim)
        try? station.setNoiseReduction(settings.noiseReduction)
        try? station.setRxCompression(settings.rxCompression)
        try? station.setRxCompressionLevel(settings.rxCompressionLevel)
        compression = settings.compression
        compressionLevel = settings.compressionLevel
        txTrim = settings.txTrim
        noiseReduction = settings.noiseReduction
        rxCompression = settings.rxCompression
        rxCompressionLevel = settings.rxCompressionLevel
        voxEnabled = settings.voxEnabled
        txDisabled = settings.txDisabled
        fullDuplex = settings.fullDuplex
        voxThresholdDBFS = settings.voxThresholdDBFS
        voxGate.config.thresholdDBFS = settings.voxThresholdDBFS
        voxHangtimeMS = settings.voxHangtimeMS
        voxGate.config.hangoverMS = settings.voxHangtimeMS
        micProfileID = settings.micProfileID
        applyMicProfile(id: settings.micProfileID)
    }
}

// MARK: - Connect-failure messages (astar-0217)

/// Turn a connect-time failure into user-facing text for the popover.
///
/// `StationError` is not `LocalizedError`, so its `localizedDescription` is the
/// useless Foundation default ("The operation couldn't be completed…"); the two
/// failures a user can actually act on get plain-language wording, any other
/// `StationError` shows its `description` (which carries the code + the
/// library's generic text), and everything else (e.g. `ConnectError`, which
/// already has good `errorDescription`) keeps `localizedDescription`.
public func connectFailureMessage(for error: Error, node: String) -> String {
    guard let stationError = error as? StationError else {
        return error.localizedDescription
    }
    // Raw codes, not the IAX_ERR_* constants: those live in the CAstarStation C
    // module, which the vendored AstarStation wrapper imports without re-exporting.
    switch stationError.code {
    case -6:  // IAX_ERR_RESOLVE — the lookup found nothing. For a node dial
        // that means no live registration (wording per Rob: short and plain);
        // for an address-shaped dial (astar-427f) the registrar was never
        // asked, so "not registered" would mislead — name the address instead.
        if case .address = DialTarget.parse(node) {
            return "Couldn’t reach \(node)."
        }
        return "Node \(node) is not registered in Allstar."
    case -5:  // IAX_ERR_PORTAL — the portal token mint failed; the fix is on
        // the account side, not the node.
        return "Couldn’t sign in to AllStarLink — check your AllStarLink "
            + "account in Settings."
    default:
        return stationError.description
    }
}
