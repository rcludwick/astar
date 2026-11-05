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

# 3. Build (ad-hoc signed so it runs locally; step 4 may re-sign it).
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
# 3b. Stamp a Debug build with when it was built and from what.
#
# Every Debug build carries the same marketing version as the release it
# grew from, so "0.1.18-beta" alone can't say which build is running. The
# footer shows AstarBuildStamp when it is there: the local build time, then
# the commit, with "+dirty" when the tree had uncommitted changes. Release
# builds (make-dmg.sh) get no stamp, so a published DMG shows its plain
# version. Editing Info.plist breaks the signature, so step 4 always signs
# a stamped build again, ad-hoc when there is no identity.
STAMPED=0
if [ "$CONFIG" = "Debug" ]; then
  commit="$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  if [ -n "$(git -C "$REPO" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    commit="$commit+dirty"
  fi
  stamp="dev $(date '+%Y-%m-%d %H:%M %Z') · $commit"
  PLIST="$APP/Contents/Info.plist"
  /usr/libexec/PlistBuddy -c "Delete :AstarBuildStamp" "$PLIST" >/dev/null 2>&1 || true
  /usr/libexec/PlistBuddy -c "Add :AstarBuildStamp string $stamp" "$PLIST"
  STAMPED=1
  echo ">> stamped: $stamp"
fi

# 4. Re-sign with a stable identity when this Mac has one.
#
# An ad-hoc signature identifies the app only by the hash of this exact
# binary, so every rebuild is a new app to the Keychain: "Always Allow" is
# granted to the old hash, and each saved password (AllStarLink, every DMR
# talkgroup, EchoLink) asks again for the login password after each build.
# Signed with a Developer ID, the app is "com.aj7hr.astar from team X", which
# survives rebuilds, so "Always Allow" sticks. The identity is discovered the
# same way make-dmg.sh does it, never hard-coded; with none, or with
# ASTAR_ADHOC=1, the ad-hoc signature stays. No hardened runtime and no
# secure timestamp here: this is a local build, not one to ship, and a
# timestamp would need the network on every build.
if [ "${ASTAR_ADHOC:-0}" != "1" ]; then
  SIGN_IDENTITY="${ASTAR_SIGN_IDENTITY:-$(
    security find-identity -v -p codesigning 2>/dev/null |
      sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -1
  )}"
  if [ -n "$SIGN_IDENTITY" ]; then
    while IFS= read -r -d '' nested; do
      codesign --force --timestamp=none --sign "$SIGN_IDENTITY" "$nested" >/dev/null 2>&1
    done < <(find "$APP/Contents" -type f \( -name '*.dylib' -o -name '*.so' \) -print0)
    if codesign --force --timestamp=none --entitlements astar.entitlements \
      --sign "$SIGN_IDENTITY" "$APP" >/dev/null 2>&1; then
      echo ">> signed as: $SIGN_IDENTITY (stable across rebuilds, so Keychain access sticks)"
    else
      echo "!! re-signing with \"$SIGN_IDENTITY\" failed — signing it ad-hoc" >&2
      SIGN_IDENTITY=""
    fi
  fi
fi
# A stamped build that step 4 didn't sign still needs a valid signature.
if [ "$STAMPED" = 1 ] && [ -z "${SIGN_IDENTITY:-}" ]; then
  codesign --force --sign - --entitlements astar.entitlements "$APP" >/dev/null 2>&1 ||
    { echo "!! ad-hoc signing the stamped build failed" >&2; exit 1; }
fi

echo ">> build ok → $APP"
