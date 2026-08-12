#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# build-xcframework.sh — build astar.xcframework for the AstarStation Swift
# package (Task 5.1 of iax-dedb).
#
# What it does:
#   1. Builds the astar-sys Rust *staticlib* (.a) for every available Apple
#      target. It always builds the host slice (aarch64-apple-darwin) and, if
#      rustup has them installed, also aarch64-apple-ios and the iOS-simulator
#      slice (aarch64-apple-ios-sim). Missing cross-targets are skipped with a
#      warning — the script degrades gracefully to a host-only xcframework.
#   2. Bundles each static lib + the committed C header
#      (crates/astar-sys/include/astar.h) into a single
#      astar.xcframework via `xcodebuild -create-xcframework`.
#
# The produced .xcframework is a GENERATED artifact — it is gitignored and this
# script regenerates it. SwiftPM's binaryTarget consumes it (see Package.swift).
#
# Requirements: a full Xcode (`xcodebuild`) + `rustup`. If you only have the
# Command Line Tools (no `xcodebuild`), you cannot create an xcframework; use
# the plain `swift build` path instead (it links target/<profile>/libastar_sys.a
# directly — see README.md).
#
# Usage:
#   bindings/swift/build-xcframework.sh            # debug
#   PROFILE=release bindings/swift/build-xcframework.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
PROFILE="${PROFILE:-debug}"
OUT_XCFRAMEWORK="$HERE/astar.xcframework"
HEADER="$REPO/crates/astar-sys/include/astar.h"
LIB_NAME="libastar_sys.a"

# Honor the rustup-shim workaround from MEMORY.md: prepend the real toolchain bin.
TOOLCHAIN_BIN="/Users/rob/.rustup/toolchains/stable-aarch64-apple-darwin/bin"
if [ -d "$TOOLCHAIN_BIN" ]; then
  export PATH="$TOOLCHAIN_BIN:$PATH"
fi

command -v xcodebuild >/dev/null 2>&1 || {
  echo "error: xcodebuild not found. A full Xcode is required to create an" >&2
  echo "       xcframework (the Command Line Tools alone are not enough)." >&2
  echo "       Install Xcode and run: sudo xcode-select -s /Applications/Xcode.app" >&2
  echo "       Or use the plain 'swift build' path documented in README.md." >&2
  exit 1
}
command -v rustup >/dev/null 2>&1 || { echo "error: rustup not found" >&2; exit 1; }

cargo_profile_flag=""
profile_dir="debug"
if [ "$PROFILE" = "release" ]; then
  cargo_profile_flag="--release"
  profile_dir="release"
fi

# Apple targets to attempt. The host slice is mandatory; the rest are optional
# and skipped if the rustup target isn't installed.
HOST_TARGET="aarch64-apple-darwin"
OPTIONAL_TARGETS=("aarch64-apple-ios" "aarch64-apple-ios-sim")

installed_targets="$(rustup target list --installed)"

is_installed() { grep -qx "$1" <<<"$installed_targets"; }

build_slice() {
  local target="$1"
  # Progress goes to stderr; ONLY the library path is written to stdout, because
  # the caller captures it via `LIB="$(build_slice ...)"`. (cargo already logs to
  # stderr.) Mixing the progress line into stdout corrupts the captured path and
  # makes `xcodebuild -create-xcframework` reject it as "not a valid library".
  echo ">> building $LIB_NAME for $target ($PROFILE)" >&2
  # Pin the macOS host slice's deployment target to the Package.swift platform
  # floor (.macOS(.v12)) so the object file isn't tagged "built for newer macOS
  # version" when linked into an app deploying to macOS 12/13.
  local dep_env=()
  if [ "$target" = "$HOST_TARGET" ]; then
    dep_env=(MACOSX_DEPLOYMENT_TARGET=12.0)
  fi
  ( cd "$REPO" && env "${dep_env[@]}" cargo build -p astar-sys --target "$target" $cargo_profile_flag )
  echo "$REPO/target/$target/$profile_dir/$LIB_NAME"
}

if [ ! -f "$HEADER" ]; then
  echo "error: header not found at $HEADER" >&2
  exit 1
fi

# Stage a headers dir holding the committed C header *and* a module map, so each
# xcframework slice ships an importable Clang module (SwiftPM's binaryTarget
# treats Headers/<dir>/module.modulemap as the module definition). The module is
# named `CAstarStation` — the SAME name as the path-A systemLibrary target — so
# the Swift wrapper's `import CAstarStation` is identical whether it links the
# raw .a or the xcframework.
#
# The header + module map go in a subdirectory NAMED FOR THE MODULE, not at the
# top of Headers/. Xcode copies each consumed xcframework's headers into the
# shared Products/<Config>/include/ dir; two frameworks that both put a bare
# `module.modulemap` there collide with "Multiple commands produce
# .../include/module.modulemap" in any app that links both this and
# astarserial.xcframework (the macOS app does). Keep the nesting.
STAGE_ROOT="$(mktemp -d)"
trap 'rm -rf "$STAGE_ROOT"' EXIT
STAGE_HEADERS="$STAGE_ROOT/CAstarStation"
mkdir -p "$STAGE_HEADERS"
cp "$HEADER" "$STAGE_HEADERS/astar.h"
cat >"$STAGE_HEADERS/module.modulemap" <<'EOF'
module CAstarStation {
    umbrella header "astar.h"
    export *
}
EOF

LIB_ARGS=()

# Host slice (mandatory).
if ! is_installed "$HOST_TARGET"; then
  echo "error: host target $HOST_TARGET not installed; run: rustup target add $HOST_TARGET" >&2
  exit 1
fi
HOST_LIB="$(build_slice "$HOST_TARGET")"
LIB_ARGS+=(-library "$HOST_LIB" -headers "$STAGE_ROOT")

# Optional cross slices.
for t in "${OPTIONAL_TARGETS[@]}"; do
  if is_installed "$t"; then
    L="$(build_slice "$t")"
    LIB_ARGS+=(-library "$L" -headers "$STAGE_ROOT")
  else
    echo ">> skipping $t (not installed; \`rustup target add $t\` to include it)"
  fi
done

echo ">> assembling $OUT_XCFRAMEWORK"
rm -rf "$OUT_XCFRAMEWORK"
xcodebuild -create-xcframework "${LIB_ARGS[@]}" -output "$OUT_XCFRAMEWORK"

echo "built: $OUT_XCFRAMEWORK"
echo
echo "NOTE: the xcframework bundles the astar-sys static lib + header +"
echo "module map (module CAstarStation) per slice. When you link it into an app you"
echo "must also add the Core Audio frameworks the cpal backend pulls in (macOS"
echo "and iOS — no IOKit, since serial PTT is not part of this library):"
echo "  -framework CoreFoundation -framework CoreAudio -framework AudioUnit \\"
echo "  -framework AudioToolbox"
echo "The AstarStation SwiftPM target declares these via linkerSettings already."

# Force the next macOS app build to relink + re-embed the framework. An
# incremental xcodebuild does not reliably notice that the gitignored
# xcframework binary was swapped underneath it, which can leave the app linked
# against a STALE core (the astar-ec79 parrot-TX bug). Removing the built
# products forces a fresh link/embed; compiled intermediates in
# apps/macos/build/DD are reused, so this is cheap.
rm -rf "$REPO/apps/macos/build/DD/Build/Products"
