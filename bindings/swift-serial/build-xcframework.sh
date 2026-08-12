#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# build-xcframework.sh — build astarserial.xcframework for the AstarSerial Swift
# package (iax-6a6f), the serial-PTT sibling of bindings/swift/build-xcframework.sh.
#
# macOS-ONLY: astar-serial-sys rides `serialport`, which links IOKit and does
# not target iOS — so this produces a single macOS slice (no iOS/sim slices).
# The slice is a universal arm64 + x86_64 library when both rustup targets are
# installed, otherwise host-arch (arm64) only.
#
# What it does:
#   1. Builds the astar-serial-sys staticlib (.a) for aarch64-apple-darwin
#      (and x86_64-apple-darwin if installed, lipo'd into a universal lib).
#   2. Bundles it + the committed header (crates/astar-serial-sys/include/
#      astarserial.h) + a generated `module CAstarSerial` map into astarserial.xcframework
#      via `xcodebuild -create-xcframework`.
#
# The produced .xcframework is a GENERATED artifact — gitignored; this script
# regenerates it. SwiftPM's binaryTarget consumes it (see Package.swift, path B).
#
# Requirements: a full Xcode (`xcodebuild`) + `rustup`.
#
# Usage:
#   bindings/swift-serial/build-xcframework.sh            # debug
#   PROFILE=release bindings/swift-serial/build-xcframework.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
PROFILE="${PROFILE:-debug}"
OUT_XCFRAMEWORK="$HERE/astarserial.xcframework"
HEADER="$REPO/crates/astar-serial-sys/include/astarserial.h"
LIB_NAME="libastar_serial_sys.a"

# Honor the rustup-shim workaround from MEMORY.md: prepend the real toolchain bin.
TOOLCHAIN_BIN="/Users/rob/.rustup/toolchains/stable-aarch64-apple-darwin/bin"
if [ -d "$TOOLCHAIN_BIN" ]; then
  export PATH="$TOOLCHAIN_BIN:$PATH"
fi

command -v xcodebuild >/dev/null 2>&1 || {
  echo "error: xcodebuild not found. A full Xcode is required to create an" >&2
  echo "       xcframework (the Command Line Tools alone are not enough)." >&2
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

installed_targets="$(rustup target list --installed)"
is_installed() { grep -qx "$1" <<<"$installed_targets"; }

build_slice() {
  local target="$1"
  # Progress to stderr; ONLY the library path on stdout (the caller captures it).
  echo ">> building $LIB_NAME for $target ($PROFILE)" >&2
  ( cd "$REPO" && cargo build -p astar-serial-sys --target "$target" $cargo_profile_flag )
  echo "$REPO/target/$target/$profile_dir/$LIB_NAME"
}

if [ ! -f "$HEADER" ]; then
  echo "error: header not found at $HEADER" >&2
  exit 1
fi

# Temp scratch: a staged headers dir (header + module map) and a universal-lib dir.
STAGE_ROOT="$(mktemp -d)"
UNIVERSAL_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_ROOT" "$UNIVERSAL_DIR"' EXIT

# The module map names the module `CAstarSerial` — the SAME name as the path-A
# systemLibrary target — so the Swift wrapper's `import CAstarSerial` is identical
# whether it links the raw .a or the xcframework.
#
# The header + module map go in a subdirectory NAMED FOR THE MODULE, not at the
# top of Headers/: Xcode copies every consumed xcframework's headers into the
# same Products/<Config>/include/ dir, and two bare `module.modulemap` files
# there collide ("Multiple commands produce .../include/module.modulemap") in
# any app linking both this and astar.xcframework. Keep the nesting.
STAGE_HEADERS="$STAGE_ROOT/CAstarSerial"
mkdir -p "$STAGE_HEADERS"
cp "$HEADER" "$STAGE_HEADERS/astarserial.h"
cat >"$STAGE_HEADERS/module.modulemap" <<'EOF'
module CAstarSerial {
    umbrella header "astarserial.h"
    export *
}
EOF

# macOS slice: arm64 host (mandatory) + x86_64 (optional, lipo'd into a universal).
HOST_TARGET="aarch64-apple-darwin"
if ! is_installed "$HOST_TARGET"; then
  echo "error: host target $HOST_TARGET not installed; run: rustup target add $HOST_TARGET" >&2
  exit 1
fi
HOST_LIB="$(build_slice "$HOST_TARGET")"

MACOS_LIB="$HOST_LIB"
if is_installed "x86_64-apple-darwin"; then
  X86_LIB="$(build_slice "x86_64-apple-darwin")"
  MACOS_LIB="$UNIVERSAL_DIR/$LIB_NAME"
  lipo -create "$HOST_LIB" "$X86_LIB" -output "$MACOS_LIB"
  echo ">> universal macOS lib (arm64 + x86_64)" >&2
else
  echo ">> macOS slice is arm64-only; \`rustup target add x86_64-apple-darwin\` for a universal (Intel) slice" >&2
fi

echo ">> assembling $OUT_XCFRAMEWORK" >&2
rm -rf "$OUT_XCFRAMEWORK"
xcodebuild -create-xcframework -library "$MACOS_LIB" -headers "$STAGE_ROOT" -output "$OUT_XCFRAMEWORK"

echo "built: $OUT_XCFRAMEWORK"
echo
echo "NOTE: the xcframework bundles the astar-serial-sys static lib + header +"
echo "module map (module CAstarSerial). When you link it into an app you must also"
echo "add the frameworks the serialport USB enumeration needs (macOS):"
echo "  -framework IOKit -framework CoreFoundation"
echo "The AstarSerial SwiftPM target declares these via linkerSettings already."

# Force the next macOS app build to relink + re-embed the framework. An
# incremental xcodebuild does not reliably notice that the gitignored
# xcframework binary was swapped underneath it, which can leave the app linked
# against a STALE core (the astar-ec79 parrot-TX bug). Removing the built
# products forces a fresh link/embed; compiled intermediates in
# apps/macos/build/DD are reused, so this is cheap.
rm -rf "$REPO/apps/macos/build/DD/Build/Products"
