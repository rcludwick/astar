// swift-tools-version: 5.9
// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// AstarCore — astar's testable logic layer (no SwiftUI). Holds the CallSession
// view-model that wraps the AstarStation binding behind a poll loop. Depends on
// the engine's own Swift binding at bindings/swift; the SwiftUI app depends on
// AstarCore.
import PackageDescription

let package = Package(
    name: "AstarCore",
    platforms: [.macOS(.v13), .iOS(.v16)],
    products: [
        .library(name: "AstarCore", targets: ["AstarCore"]),
    ],
    dependencies: [
        // The engine's Swift binding. Its package identity is the directory
        // name ("swift"), so products must be named with `package:`.
        .package(path: "../../../../bindings/swift"),
    ],
    targets: [
        .target(
            name: "AstarCore",
            dependencies: [.product(name: "AstarStation", package: "swift")]
        ),
        .testTarget(
            name: "AstarCoreTests",
            dependencies: ["AstarCore"]
        ),
    ]
)
