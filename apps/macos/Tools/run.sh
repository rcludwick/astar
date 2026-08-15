#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# run.sh — kill any running astar, (re)build the macOS app, and launch it.
#
# Usage:
#   Tools/run.sh              # regenerate + build + run
#   Tools/run.sh --no-build   # just kill + relaunch the existing build
#
# Requires full Xcode. The generated astar.xcodeproj is gitignored and created
# here; the xcframeworks are built by `just xcframework` and checked for below.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

APP="build/DD/Build/Products/Debug/astar.app"
BIN_GLOB="astar.app/Contents/MacOS/astar"

# 1. Kill any running astar (graceful, then force).
if pgrep -f "$BIN_GLOB" >/dev/null 2>&1; then
  osascript -e 'tell application "astar" to quit' >/dev/null 2>&1 || true
  pkill -f "$BIN_GLOB" 2>/dev/null || true
  echo ">> killed running astar"
fi

if [ "${1:-}" != "--no-build" ]; then
  # 2. Preflight: both xcframeworks must exist (gitignored, regenerable).
  #    Hard fail rather than a silent fallback — see Tools/build.sh.
  REPO="$(cd "$HERE/../.." && pwd)"
  missing=0
  [ -e "$REPO/bindings/swift/astar.xcframework" ] || { echo "!! missing bindings/swift/astar.xcframework" >&2; missing=1; }
  [ -e "$REPO/bindings/swift-serial/astarserial.xcframework" ] || { echo "!! missing bindings/swift-serial/astarserial.xcframework" >&2; missing=1; }
  if [ "$missing" -ne 0 ]; then
    echo "!! run \`just xcframework\` (builds BOTH) and try again." >&2
    exit 1
  fi

  # 3. (Re)generate the Xcode project from project.yml so new files are picked up.
  echo ">> xcodegen generate"
  xcodegen generate >/dev/null

  # 4. Build the macOS app (ad-hoc signed so it runs locally). Quiet unless it fails.
  echo ">> building astar (macOS)…"
  if ! xcodebuild -project astar.xcodeproj -scheme "astar (macOS)" -configuration Debug \
        -derivedDataPath build/DD \
        CODE_SIGN_IDENTITY="-" CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM="" \
        build >/tmp/astar-build.log 2>&1; then
    echo "!! BUILD FAILED — last lines of /tmp/astar-build.log:" >&2
    grep -iE "error:" /tmp/astar-build.log | tail -20 >&2 || tail -20 /tmp/astar-build.log >&2
    exit 1
  fi
  echo ">> build ok"
fi

# 5. Launch.
[ -d "$APP" ] || { echo "!! app not found at $APP (run without --no-build first)" >&2; exit 1; }
open "$APP"
echo ">> launched astar (menu-bar — look for the asterisk in your menu bar)"
