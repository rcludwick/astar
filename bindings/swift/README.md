# Swift binding: `AstarStation`

A SwiftPM package wrapping the `astar-sys` poll+snapshot C-ABI in an
idiomatic Swift API. It mirrors `crates/astar-sys/include/astar.h`
exactly and shares the binding contract with the C and Python bindings:

* **Poll + snapshot.** Call state is driven by polling `snapshot()` and
  `nextEvent()`. The one callback into Swift is the optional credential resolver
  (`setCredentialResolver`), bridged through a `@convention(c)` trampoline and
  used solely to fetch a secret on demand.
* **Secret-free.** A `secret` (and the WT `portalPass`) is only ever a
  connect/init argument or the return value of the credential resolver. It is
  never stored on the `Station`, never returned from a `Snapshot`/`Event`, never
  in a `StationError`, and never in any `description`.
* **Vendor-neutral.** `connect(...)` is the generic IAX2 path; `connectWT(...)`
  is the AllStar Web-Transceiver convenience.

## Package layout

```
bindings/swift/
  Package.swift                       SwiftPM manifest (targets below)
  Sources/CAstarStation/                systemLibrary target
    module.modulemap                  exposes the C-ABI as module `CAstarStation`
    shim.h                            #includes the committed astar.h
  Sources/AstarStation/Station.swift    the idiomatic Swift API
  Examples/parrot/main.swift          offline example (live with IAX_PARROT_LIVE=1)
  Tests/AstarStationTests/              offline unit tests + secret-free guard
  build-xcframework.sh                builds astar.xcframework (Task 5.1)
```

`CAstarStation` is a `systemLibrary` whose module map's umbrella `shim.h` includes
`crates/astar-sys/include/astar.h` by relative path — a single source of
truth (regenerating the cbindgen header is picked up automatically).

## The Swift API

```swift
import AstarStation

let st = try Station(config: StationConfig())     // deinit frees + tears down
let snap = try st.snapshot()                       // -> Snapshot (status, meters, rtt)
if snap.status == .idle { /* ... */ }
try st.connect(dest: "55553", calling: "55553",    // generic IAX2 path
               secret: "allstar", name: "me")      // secret is an in-arg only
try st.connectWT(destNode: "55553")                // AllStar WT convenience
try st.setPTT(true)
try st.setInputGain(0.8); try st.setOutputGain(1.2)
try st.setCompression(true); try st.setNoiseReduction(true)  // mic DSP, live
try st.setCompressionLevel(0.9)                              // strength 0..1 (default 0.90)
while let ev = try st.nextEvent() { /* .answered / .remotePTT(Bool) / .hangup */ }
let ins = try st.listInputs(); let outs = try st.listOutputs()
try st.setDevices(input: ins.first, output: outs.first)
try st.disconnect()
```

PTT *source* is the app's choice — a button, a spacebar handler, a serial
device — and it drives keying via `setPTT(_:)`. Hardware sources (e.g. the UCI150
serial PTT) live in a separate, optional library, not in this binding.

Every method maps a non-zero C code to a thrown `StationError(code, text)` whose
`text` comes from `iax_error_text` (generic, secret-free).

## Operating modes (WT + Node)

Two top-level modes (mirrors the Rust `Station`; see
`crates/astar-station/README.md`):

- **WebTransceiver (WT) client** — dial out with `connect(...)` / `connectWT(...)`.
- **Node** — accept inbound calls and bridge them to a local handset. Configure
  with `setNodeConfig(_:)`, switch with `try setMode(.node)`, then poll for
  `.incoming` events and call `answer()` / `reject()` (Manual answer) or let it
  auto-answer; `incomingFrom()` returns the caller id. Optionally register **as**
  a node by setting `NodeConfig.registrar` / `.registerUser`.

```swift
let st = try Station(config: StationConfig(input: "USB Audio", output: "Speakers"))
// The registrar password is supplied ONLY through the resolver — never config.
try st.setCredentialResolver { user in lookupSecret(user) }
var node = NodeConfig()
node.bind = "0.0.0.0:4569"
node.auth = .off
node.registrar = "register.allstarlink.org:4569"   // omit to only listen
node.registerUser = "77777"
try st.setNodeConfig(node)
try st.setMode(.node)   // binds the listener + registers; blocking
```

## Linking

The C-ABI lives in the astar-sys **static** library. Two supported paths:

### (A) Plain `swift build` — no Xcode needed

`Package.swift` links `target/<profile>/libastar_sys.a` directly via
`linkerSettings`, plus the macOS frameworks the Rust staticlib pulls in. Build
the Rust lib first, then `swift build`:

```sh
# from repo root, with the rustup toolchain on PATH (see MEMORY.md):
cargo build -p astar-sys                 # debug (default)
# cargo build --release -p astar-sys     # release

cd bindings/swift
swift build                                  # links target/debug/libastar_sys.a
swift run astarstation-parrot                  # offline example + secret-free guard
swift test                                   # offline unit tests (needs full Xcode:
                                             #   XCTest ships with Xcode, not the
                                             #   Command Line Tools). The example
                                             #   above already runs the secret-free
                                             #   guard on a CLT-only box.
# IAX_PARROT_LIVE=1 swift run astarstation-parrot   # dials parrot 55553

# For the release archive:
# cargo build --release -p astar-sys
# ASTAR_LINK_RELEASE=1 swift build -c release
```

Required frameworks (declared once in `Package.swift`'s `linkerSettings`, on both
macOS and iOS):

```
-framework CoreFoundation -framework CoreAudio -framework AudioUnit \
-framework AudioToolbox
```

(All four are for cpal's CoreAudio backend. There is **no** `IOKit`: serial PTT —
the only IOKit user — is not part of this library, which is why the binding
builds for iOS too.)

### (B) astar.xcframework — multiplatform (macOS + iOS) for app/Xcode distribution

`build-xcframework.sh` builds the astar-sys staticlib for every available
Apple target (always `aarch64-apple-darwin`; also `aarch64-apple-ios` and
`aarch64-apple-ios-sim` if those rustup targets are installed — missing ones are
skipped) and bundles each slice's staticlib + header + a generated
`module.modulemap` (module `CAstarStation`) into `astar.xcframework` via
`xcodebuild -create-xcframework`:

```sh
# requires a full Xcode (xcodebuild), not just the Command Line Tools.
# to include the iOS slices:
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
bindings/swift/build-xcframework.sh           # debug
# PROFILE=release bindings/swift/build-xcframework.sh
```

The resulting `astar.xcframework` is **gitignored** (large + generated; the
script regenerates it).

Build the package against the xcframework (instead of the raw `.a`) by setting
`ASTAR_USE_XCFRAMEWORK=1`: `Package.swift` then backs `CAstarStation` with a
`binaryTarget` over the framework, so a downstream SwiftPM/Xcode app can `import
AstarStation` and build for **macOS or the iOS simulator**, linking the
xcframework. Because each slice ships a module map, no extra header wiring is
needed — only the Core Audio frameworks above (already declared, both platforms):

```sh
ASTAR_USE_XCFRAMEWORK=1 swift build      # links the macOS slice on the host
# in an app: add the package + build for 'generic/platform=iOS Simulator'
```
