#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# build.sh — build the astar macOS app from source.
#
# Does the full cold-start dance: checks the gitignored xcframeworks are built,
# regenerates astar.xcodeproj from project.yml, then builds. It does NOT launch
# (see Tools/run.sh) or package a .dmg (see Tools/make-dmg.sh).
#
# Usage:
#   Tools/build.sh                 # Debug build (default)
#   Tools/build.sh --release       # Release build
#   Tools/build.sh --clean         # clean build folder first
#   Tools/build.sh --verbose       # stream xcodebuild output instead of logging
#
# The built app lands at build/DD/Build/Products/<Config>/astar.app.
# Requires full Xcode (xcodebuild) + xcodegen.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

SCHEME="astar (macOS)"
CONFIG="Debug"
DD="build/DD"
CLEAN=0
VERBOSE=0

for arg in "$@"; do
  case "$arg" in
    --release)  CONFIG="Release" ;;
    --debug)    CONFIG="Debug" ;;
    --clean)    CLEAN=1 ;;
    --verbose|-v) VERBOSE=1 ;;
    -h|--help)
      sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "!! unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

# 1. Preflight: both xcframeworks must exist (gitignored, regenerable).
#
# HARD FAIL, never a silent fallback. bindings/swift/Package.swift picks its
# linking path by whether the xcframework is on disk; if it is missing the app
# would quietly drop to the host-only static-lib path (no iOS slice, confusing
# link errors) instead of saying what is wrong.
REPO="$(cd "$HERE/../.." && pwd)"
missing=0
[ -e "$REPO/bindings/swift/astar.xcframework" ] || { echo "!! missing bindings/swift/astar.xcframework" >&2; missing=1; }
[ -e "$REPO/bindings/swift-serial/astarserial.xcframework" ] || { echo "!! missing bindings/swift-serial/astarserial.xcframework" >&2; missing=1; }
if [ "$missing" -ne 0 ]; then
  echo "!! run \`just xcframework\` (builds BOTH) and try again." >&2
  exit 1
fi

# 2. Regenerate the Xcode project from project.yml so new files are picked up.
echo ">> xcodegen $(xcodegen --version 2>&1 | head -1) / $(xcodebuild -version 2>&1 | head -1)"
echo ">> xcodegen generate"
xcodegen generate >/dev/null
# xcodegen has been observed to exit 0 without producing a project on some CI
# images; fail loudly here rather than hitting an opaque xcodebuild error.
[ -d astar.xcodeproj ] || { echo "!! xcodegen produced no astar.xcodeproj" >&2; exit 1; }

# 3. Build (ad-hoc signed so it runs locally).
#
# Pin ARCHS to the host arch: astarserial.xcframework carries only a
# macOS host slice (arm64 on Apple Silicon) — an unpinned build that resolves to
# x86_64 or tries universal fails to link libastar_serial_sys.a. The
# resulting .app therefore matches the build host's architecture.
ARCH="$(uname -m)"
LOG="/tmp/astar-build.log"
XCODEBUILD=(xcodebuild -project astar.xcodeproj -scheme "$SCHEME" -configuration "$CONFIG"
  -derivedDataPath "$DD" -destination "platform=macOS,arch=$ARCH"
  ARCHS="$ARCH" ONLY_ACTIVE_ARCH=YES
  CODE_SIGN_IDENTITY="-" CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM="")

[ "$CLEAN" -eq 1 ] && { echo ">> clean"; "${XCODEBUILD[@]}" clean >/dev/null; }

echo ">> building astar (macOS) [$CONFIG, $ARCH]…"
if [ "$VERBOSE" -eq 1 ]; then
  "${XCODEBUILD[@]}" build
elif ! "${XCODEBUILD[@]}" build >"$LOG" 2>&1; then
  echo "!! BUILD FAILED — last lines of $LOG:" >&2
  grep -iE "error:" "$LOG" | tail -20 >&2 || tail -20 "$LOG" >&2
  exit 1
fi

APP="$DD/Build/Products/$CONFIG/astar.app"
[ -d "$APP" ] || { echo "!! build reported success but no app at $APP" >&2; exit 1; }
echo ">> build ok → $APP"
