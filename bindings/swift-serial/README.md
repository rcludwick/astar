# AstarSerial — Swift serial radio-interface client (PTT facet)

A Swift wrapper over `astar-serial-sys`. Drive PTT from a serial radio
interface (UCI150 by default; selectable CTS/DCD/DSR/RI in, RTS/DTR out) and
forward it to an `AstarStation` call.

macOS-only: the serial backend rides `serialport`, which links IOKit and does
not target iOS.

## Build

Two linking modes, selected by `ASTAR_USE_XCFRAMEWORK` (mirrors AstarStation):

**(A) Default — `swift build` against the raw staticlib** (no Xcode needed):

```sh
cargo build -p astar-serial-sys      # the staticlib AstarSerial links
cd bindings/swift-serial && swift build && swift test
swift run astarserial-parrot               # live; needs a UCI150 attached
```

**(B) `astarserial.xcframework` — for app/Xcode distribution.** `build-xcframework.sh`
bundles the staticlib + header + a `CAstarSerial` module map into a macOS xcframework
(universal arm64 + x86_64 when both rustup targets are installed). Build it, then
set `ASTAR_USE_XCFRAMEWORK=1` so `Package.swift` backs `CAstarSerial` with a
`binaryTarget` instead of the raw `.a`:

```sh
cd bindings/swift-serial
./build-xcframework.sh                    # requires a full Xcode; PROFILE=release for release
ASTAR_USE_XCFRAMEWORK=1 swift build   # links the xcframework
```

The resulting `astarserial.xcframework` is **gitignored** (large + generated). A
downstream app vendors it as a `binaryTarget` and links `IOKit` + `CoreFoundation`
on macOS (the serialport USB enumeration needs them — the `AstarSerial` target
declares them already).

## Use

```swift
import AstarSerial
var cfg = SerialConfig()
cfg.portPath = SerialClient.autodetect()   // or an explicit path
let serial = try SerialClient(cfg)
// 20 ms loop: feed your AstarStation snapshot, forward the decision:
let (changed, on) = try serial.pttTick(remoteKeyed: snap.remotePTT, rxDb: snap.rxDb)
if changed { try station.setPTT(on) }
```

PTT *source* lives here, not in AstarStation: the library only sets the call's
keyed/unkeyed state via `setPTT`. Secret-free, poll model, no callbacks.
