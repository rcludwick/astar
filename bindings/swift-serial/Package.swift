// swift-tools-version: 5.9
// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// AstarSerial — a Swift wrapper over the astar-serial-sys poll C-ABI
// (crates/astar-serial-sys/include/astarserial.h): a cross-platform serial
// radio-interface client (PTT facet) that composes with AstarStation by driving
// keying via `iax_station_set_ptt` (it links only astar-serial-sys — no
// AstarStation, no Core Audio).
//
// macOS-only: the serial backend rides `serialport`, which links IOKit and does
// not target iOS.
//
// LINKING. The C-ABI lives in the astar-serial-sys *static* library. Two
// modes, selected automatically by whether the prebuilt xcframework is on disk
// — no environment variable, because Xcode and XcodeGen cannot reliably set one
// while a manifest is being evaluated:
//
//   (A) astarserial.xcframework ABSENT — plain `swift build` (no Xcode needed):
//       `CAstarSerial` is a `systemLibrary` over the committed header, and
//       AstarSerial links target/<profile>/libastar_serial_sys.a directly.
//       Build the Rust lib first:
//         cargo build -p astar-serial-sys            # debug (default below)
//         cargo build --release -p astar-serial-sys  # release (ASTAR_LINK_RELEASE=1)
//       then `swift build` / `swift test`.
//
//   (B) astarserial.xcframework PRESENT — for app/Xcode distribution:
//       `CAstarSerial` is a `binaryTarget` over the xcframework (which ships a
//       module map), and the static lib comes from the framework — no raw
//       -L/-l. Build the framework first:
//         ./build-xcframework.sh            # debug (PROFILE=release for release)
//       then a downstream SwiftPM/Xcode app can `import AstarSerial` on macOS.
//
// Either way the serialport USB enumeration needs IOKit + CoreFoundation, linked
// here once via linkerSettings.
//
// To link the release archive in path A, set ASTAR_LINK_RELEASE=1.

import Foundation
import PackageDescription

let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let repoRoot = here.deletingLastPathComponent().deletingLastPathComponent()
let profile = ProcessInfo.processInfo.environment["ASTAR_LINK_RELEASE"] == "1"
    ? "release" : "debug"
let staticLibDir = repoRoot
    .appendingPathComponent("target").appendingPathComponent(profile).path

// Path B (xcframework) vs path A (systemLibrary + raw .a). See LINKING above.
// Deterministic on-disk detection, not an env var: manifest evaluation under
// Xcode/XcodeGen cannot be given one.
let xcframeworkPath = here.appendingPathComponent("astarserial.xcframework").path
let useXCFramework = FileManager.default.fileExists(atPath: xcframeworkPath)

// The C-ABI module: either a binaryTarget over the prebuilt xcframework, or a
// systemLibrary over the committed header. Both expose the module `CAstarSerial`.
let cAstarSerialTarget: Target = useXCFramework
    ? .binaryTarget(name: "CAstarSerial", path: "astarserial.xcframework")
    : .systemLibrary(name: "CAstarSerial", path: "Sources/CAstarSerial")

// Frameworks the serialport USB enumeration needs on macOS.
let serialFrameworks: [LinkerSetting] = [
    .linkedFramework("IOKit"),
    .linkedFramework("CoreFoundation"),
]

// In path A we additionally point the linker at the raw staticlib; in path B the
// binaryTarget supplies it.
let astarSerialLinkerSettings: [LinkerSetting] = useXCFramework
    ? serialFrameworks
    : [.unsafeFlags(["-L", staticLibDir, "-lastar_serial_sys"])] + serialFrameworks

let package = Package(
    name: "AstarSerial",
    platforms: [.macOS(.v12)],
    products: [
        .library(name: "AstarSerial", targets: ["AstarSerial"]),
        .executable(name: "astarserial-parrot", targets: ["astarserial-parrot"]),
    ],
    targets: [
        cAstarSerialTarget,
        .target(
            name: "AstarSerial",
            dependencies: ["CAstarSerial"],
            path: "Sources/AstarSerial",
            linkerSettings: astarSerialLinkerSettings
        ),
        .executableTarget(
            name: "astarserial-parrot",
            dependencies: ["AstarSerial"],
            path: "Examples/serial-parrot"
        ),
        .testTarget(
            name: "AstarSerialTests",
            dependencies: ["AstarSerial"],
            path: "Tests/AstarSerialTests"
        ),
    ]
)
