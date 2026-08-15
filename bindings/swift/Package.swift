// swift-tools-version: 5.9
// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// AstarStation — an idiomatic Swift wrapper over the astar-sys poll+snapshot
// C-ABI (crates/astar-sys/include/astar.h).
//
// Targets:
//   * CAstarStation  — the C-ABI as an importable Clang module named
//                    `CAstarStation`. Two backings (see LINKING): a `systemLibrary`
//                    over the committed header (path A), or a `binaryTarget` over
//                    the prebuilt astar.xcframework (path B). Either way the
//                    Swift code does `import CAstarStation` unchanged.
//   * AstarStation   — the Swift API: a `Station` class, `Snapshot`/`Event`/
//                    `StationError` value types, `IaxStatus`/`IaxEventKind`
//                    enums, all throwing methods that map non-zero C codes to a
//                    thrown StationError carrying `iax_error_text`.
//   * astarstation-parrot — a small offline example executable.
//
// The C-ABI is PTT-source-agnostic and serial-free (no `serialport`/IOKit), so
// it builds for every Apple platform including iOS. A hardware PTT source (e.g.
// the UCI150 serial PTT) is a separate, optional library that drives keying via
// `iax_station_set_ptt` (iax-0c79).
//
// LINKING. The C-ABI lives in the astar-sys *static* library. Two modes,
// selected automatically by whether the prebuilt xcframework is on disk — no
// environment variable, because Xcode and XcodeGen cannot reliably set one
// while a manifest is being evaluated:
//
//   (A) astar.xcframework ABSENT — plain `swift build` (no Xcode needed; a
//       CLT-only box): `CAstarStation` is a `systemLibrary` over the committed
//       header, and AstarStation links target/<profile>/libastar_sys.a
//       directly. Build the Rust lib first:
//         cargo build -p astar-sys            # debug   (default below)
//         cargo build --release -p astar-sys  # release (set ASTAR_LINK_RELEASE=1)
//       then `swift build` / `swift test`.
//
//   (B) astar.xcframework PRESENT — multiplatform (macOS + iOS):
//       `CAstarStation` is a `binaryTarget` over the xcframework (which ships a
//       module map per slice), and the static lib comes from the framework —
//       no raw -L/-l. Build the framework first:
//         ./build-xcframework.sh            # debug   (PROFILE=release for release)
//       then an Xcode/SwiftPM app can `import AstarStation` and build for
//       macOS or the iOS simulator. This is the path the macOS app takes;
//       apps/macos/Tools/build.sh hard-fails if the framework is missing
//       rather than letting the app silently drop to the host-only path A.
//
// Either way the Rust staticlib pulls in the cpal CoreAudio backend, so the
// Core Audio system frameworks are linked here once via linkerSettings (needed
// on both macOS and iOS).
//
// To link the release archive in path A, set ASTAR_LINK_RELEASE=1 in the
// environment when invoking swift build.

import Foundation
import PackageDescription

// The repo root is two levels up from this Package.swift (bindings/swift/).
let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let repoRoot = here.deletingLastPathComponent().deletingLastPathComponent()
let profile = ProcessInfo.processInfo.environment["ASTAR_LINK_RELEASE"] == "1"
    ? "release" : "debug"
let staticLibDir = repoRoot
    .appendingPathComponent("target")
    .appendingPathComponent(profile)
    .path

// Path B (xcframework) vs path A (systemLibrary + raw .a). See LINKING above.
// Deterministic on-disk detection, not an env var: manifest evaluation under
// Xcode/XcodeGen cannot be given one.
let xcframeworkPath = here.appendingPathComponent("astar.xcframework").path
let useXCFramework = FileManager.default.fileExists(atPath: xcframeworkPath)

// The C-ABI module: either a binaryTarget over the prebuilt xcframework, or a
// systemLibrary over the committed header. Both expose the module `CAstarStation`.
let cAstarStation: Target = useXCFramework
    ? .binaryTarget(name: "CAstarStation", path: "astar.xcframework")
    : .systemLibrary(name: "CAstarStation", path: "Sources/CAstarStation")

// Core Audio frameworks the cpal backend needs, on both macOS and iOS. (No
// IOKit: serial PTT — the only IOKit user — is no longer part of this library.)
let audioFrameworks: [LinkerSetting] = [
    .linkedFramework("CoreFoundation"),
    .linkedFramework("CoreAudio"),
    .linkedFramework("AudioUnit", .when(platforms: [.macOS])),
    .linkedFramework("AudioToolbox"),
]

// In path A we additionally point the linker at the raw staticlib; in path B the
// binaryTarget supplies it.
let astarStationLinkerSettings: [LinkerSetting] = useXCFramework
    ? audioFrameworks
    : [.unsafeFlags(["-L", staticLibDir, "-lastar_sys"])] + audioFrameworks

let package = Package(
    name: "AstarStation",
    platforms: [
        .macOS(.v12),
        .iOS(.v13),
    ],
    products: [
        .library(name: "AstarStation", targets: ["AstarStation"]),
        .executable(name: "astarstation-parrot", targets: ["astarstation-parrot"]),
    ],
    targets: [
        cAstarStation,
        // Idiomatic Swift wrapper.
        .target(
            name: "AstarStation",
            dependencies: ["CAstarStation"],
            path: "Sources/AstarStation",
            linkerSettings: astarStationLinkerSettings
        ),
        // Offline example: new -> snapshot -> set_ptt(NOT_CONNECTED) -> free.
        .executableTarget(
            name: "astarstation-parrot",
            dependencies: ["AstarStation"],
            path: "Examples/parrot"
        ),
        // Offline unit tests, including the secret-free guard.
        .testTarget(
            name: "AstarStationTests",
            dependencies: ["AstarStation"],
            path: "Tests/AstarStationTests"
        ),
    ]
)
