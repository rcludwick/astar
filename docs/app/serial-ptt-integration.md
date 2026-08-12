# Wiring serial PTT into astar (AstarSerial)

**Status:** design / how-to (2026-06-20)
**Upstream:** astar-lib ticket iax-0c79 (shipped) — the standalone serial
radio-interface client.

## Why this exists

The IAX2 core is **PTT-source-agnostic**. `AstarStation` only sets the call's
keyed/unkeyed state via `setPTT(_:)` and reports levels in the snapshot
(`txDB`/`rxDB`/`remotePTT`). *What* drives PTT — a button, a spacebar, a serial
radio interface, a footswitch — is astar's choice. Serial PTT (the AllScan
UCI150) is one optional **source**, shipped as a separate Swift package
`AstarSerial` so the core stays portable and IOKit-free.

Today astar has `setPTT` fully plumbed (`CallSession.setPTT` →
`station.setPTT`, `CallSession.swift:91`) but **nothing triggers it** — PTT is an
intentional stub. This wires up serial PTT as the first real source.

## The AstarSerial API (what you call)

From `astar-lib/bindings/swift-serial` (module `AstarSerial`):

```swift
public struct SerialConfig {
    public var portPath: String?          // nil = autodetect (first WCH USB device)
    public var keyLine: KeyLine = .cts     // operator-key INPUT line: .cts/.dcd/.dsr/.ri
    public var keyActiveHigh: Bool = true
    public var radioLine: RadioLine = .rts // radio-key OUTPUT line: .rts/.dtr
    public var radioActiveHigh: Bool = true
    public var debounceMs: UInt32 = 30     // key de-glitch window; 0 = off
    public var rxMode: RxKeyMode = .remotePTT  // .remotePTT or .rxActivity
    public var rxFloorDb: Float = -45.0
    public var rxHangMs: UInt32 = 250
}

public final class SerialClient {
    public static func autodetect() -> String?            // first WCH USB serial path, or nil
    public init(_ config: SerialConfig) throws            // opens port, clears radio line
    public func pttTick(remoteKeyed: Bool, rxDb: Float)    // ONE keying step:
        throws -> (changed: Bool, ptt: Bool)              //   read key, run bridge, write radio
    // deinit closes the port and drops the radio line (fail-safe)
}
```

The **poll/tick** contract: each tick you feed the latest snapshot's
`remotePTT` + `rxDB`; the call reads the operator-key line and decides whether
the *call* should key (`changed`/`ptt`); the physical **radio-key line is driven
inside the tick**. You forward `ptt` to `setPTT` when `changed`. No callbacks.

The UCI150 default profile is CTS-in / RTS-out; the line selectors exist because
other interfaces route COS/PTT on different pins (DCD is common).

## Prerequisite (astar-lib): a macOS xcframework for the serial lib

astar consumes AstarStation as a **vendored xcframework binaryTarget**
(`Packages/AstarStation`, refreshed by `Tools/update-astarstation.sh`). The new
`AstarSerial` package upstream is currently **path-A only** (a `systemLibrary` +
the raw `target/debug/libastar_serial_sys.a`) — there is no xcframework yet,
so astar cannot vendor it the same way.

**Blocked on** an astar-lib follow-up that packages `astar-serial-sys` as
a macOS `astarserial.xcframework` + a dual-mode `swift-serial/Package.swift`,
mirroring what iax-52ca/iax-bb62 did for AstarStation. (See the astar-lib
tracker.) Do that first; then the steps below apply.

Note: unlike AstarStation, the serial staticlib links **IOKit** (via `serialport`'s
USB enumeration), so the xcframework slice carries IOKit symbols and astar's
`AstarSerial` package must link `IOKit` on macOS.

## Step 1 — Vendor AstarSerial

Add `Tools/update-astarserial.sh` mirroring `Tools/update-astarstation.sh`: copy
`astarserial.xcframework` + the vendored `SerialClient.swift` into a new
`Packages/AstarSerial/`. Its `Package.swift` mirrors `Packages/AstarStation`'s but:

```swift
.binaryTarget(name: "CIaxSerial", path: "astarserial.xcframework"),
.target(
    name: "AstarSerial",
    dependencies: ["CIaxSerial"],
    linkerSettings: [
        .linkedFramework("IOKit", .when(platforms: [.macOS])),  // serialport USB enumeration
    ]
)
```

Then add `Packages/AstarSerial` to `project.yml` (alongside the AstarStation package,
lines 18-22) and to `AstarCore`'s dependencies.

## Step 2 — CallSession owns a SerialClient and ticks it in the poll loop

`CallSession` already runs the right loop: a 20 Hz `Timer` calling `poll()` on the
**main run loop** (`CallSession.swift:44-70`). Hook the serial tick into it.

Add to `CallSession`:

```swift
import AstarSerial

private var serial: SerialClient?
@Published public private(set) var serialActive = false   // device open?

/// Open the serial PTT source. Idempotent; replaces any existing client.
public func enableSerialPTT(_ config: SerialConfig) {
    serial = try? SerialClient(config)          // nil on no-device / open failure
    serialActive = (serial != nil)
}

/// Close the serial source (deinit drops the radio line — fail-safe).
public func disableSerialPTT() {
    serial = nil
    serialActive = false
}
```

Extend `poll()` (`CallSession.swift:60`) — after reading the snapshot, before the
event drain:

```swift
public func poll() {
    let snap = try? station.readSnapshot()
    if let snap {
        status = snap.status; ptt = snap.ptt; remotePTT = snap.remotePTT
        txDB = snap.txDB; rxDB = snap.rxDB; rttMS = snap.rttMS

        // Serial PTT source: one keying tick per poll, off the SAME snapshot.
        if let serial {
            do {
                let (changed, on) = try serial.pttTick(remoteKeyed: snap.remotePTT, rxDb: snap.rxDB)
                if changed { try station.setPTT(on) }
            } catch {
                // serial I/O failed (e.g. USB unplugged mid-call): tear down so a
                // dead device can't wedge the loop; the next snapshot reflects unkey.
                serial = nil
                serialActive = false
                NSLog("[astar] serial PTT error, disabled: \(error)")
            }
        }
    }
    while let _ = try? station.readEvent() {}
}
```

Because this runs inside the existing main-thread timer, it inherits the
required single-thread serialization for `station.setPTT` (see the threading
note). One tick of latency (≤50 ms) between key edge and call-key is
imperceptible.

`StationDriving` is the test seam — the serial wiring lives in `CallSession`
against the *real* `station`, so the existing `FakeStation` tests are unaffected;
add a fake `SerialClient` only if you want to unit-test the tick→setPTT bridge.

## Step 3 — Settings UI for the full UCI150 config

This is the original ask: surface the full serial config to the UI. Add a
`SerialView` (mirror `DevicesView.swift`) with a serial-PTT section:

- **Enable** toggle → `enableSerialPTT(config)` / `disableSerialPTT()`, with a
  status line (`serialActive`: "UCI150 connected" / "no device found" / error).
- **Device**: an "Autodetect" default + an explicit port-path field. (`AstarSerial`
  exposes `SerialClient.autodetect() -> String?`; there is no device-list API
  yet, so MVP = autodetect or a typed path.)
- **Key input line**: Picker `KeyLine` (CTS/DCD/DSR/RI) + an "active high" toggle.
- **Radio key line**: Picker `RadioLine` (RTS/DTR) + an "active high" toggle.
- **Debounce (ms)**: stepper (0 = off).
- **RX-key mode**: Picker `RxKeyMode` (RemotePTT / RxActivity), and when
  `.rxActivity`: **RX floor (dB)** + **RX hang (ms)**.

Persist the `SerialConfig` in `UserDefaults`; re-`enableSerialPTT` on launch if
it was enabled.

## Threading & safety

- `poll()` runs on the **main run loop** (the `Timer` is scheduled from
  `onAppear`). All `station.*` calls — snapshot, setPTT, connect, disconnect —
  already happen there, satisfying `Station`'s "serialize calls from one thread"
  contract (`Station.swift` header). Ticking the serial source in `poll()` keeps
  it on the same thread; **no `DispatchQueue` marshaling needed.**
- Serial **modem-status line reads** (CTS/DCD/DSR/RI) are immediate ioctls — they
  do **not** block on the port's 50 ms data timeout — so `pttTick` will not stall
  the UI timer. (The v2 data facet, if added, would need a background read.)
- `SerialClient.deinit` drops the radio line, so disabling, quitting, or losing
  the handle can never leave the transmitter keyed.

## Optional — other PTT sources (the architecture pays off here)

Because the core is source-agnostic, you can add more sources that each just call
`session.setPTT(_:)`, independent of serial:

- **Software PTT** — a spacebar/key handler or an on-screen press-and-hold
  button. Zero hardware; good default before a UCI150 is configured.
- **Software VOX** — the snapshot's `txDB` is **live while connected but
  un-keyed** (verified upstream: metered from `mgr.tx_dbfs` gated on the call,
  not on PTT — `astar-lib .../session.rs:459`), so a mic-level threshold in
  `poll()` can drive `setPTT`. No core change required.

## Appendix — Is astar using AstarStation correctly? (audit)

Yes. Current usage is idiomatic:

- **Poll + snapshot, single thread** — correct; matches the binding's no-callback
  contract.
- **Secrets in-arg, never stored** — `makeStation` consumes `portalPass` into the
  config and drops it (`Station+Driving.swift:46-56`); guest dials pass the public
  guest secret `"allstar"` inline. Correct, secret-free.
- **Event drain** — `poll()` discards events with
  `while let _ = try? station.readEvent() {}` (`CallSession.swift:69`). This is
  **safe today**: every UI field comes from the snapshot, and the discarded
  `Event`s are either snapshot-redundant (`.answered`/`.remotePTT`/`.hangup`) or
  for features astar doesn't use (`.incoming`, `.registered`, `.registerFailed`,
  `.modeChanged`).
  - **Forward-compat caveat:** when astar adds **Node/inbound mode** or
    **register-as-node**, this blanket drain will silently swallow `.incoming`
    calls and registration results. Those must be handled then (read
    `incomingFrom()` on `.incoming`; surface `.registerFailed`).

Minor nits (not bugs): `connect(dest: node, calling: node, …)` uses the
destination as the caller-id (`CallSession.swift:86`) — harmless for guest/parrot,
but a real caller-id (the user's node/callsign) would be more correct; and the
`try?` event drain silently swallows any binding error.

## References

- astar-lib design spec: `docs/superpowers/specs/2026-06-20-iax-0c79-serial-client-design.md`
- astar-lib plan: `docs/superpowers/plans/2026-06-20-iax-0c79-serial-client.md`
- AstarSerial README: `astar-lib/bindings/swift-serial/README.md`
- VOX/`tx_db` note: `astar-lib/docs/superpowers/notes/2026-06-20-tx-db-while-unkeyed.md`
- astar hooks: `CallSession.swift:60` (poll), `:91` (setPTT); `MenuPopover.swift:174-175` (meters).
