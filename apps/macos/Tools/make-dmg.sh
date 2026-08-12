#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# make-dmg.sh — build a distributable, double-clickable astar.dmg for the macOS
# menu-bar app (au-a360, milestone M1: ad-hoc / unsigned).
#
# What it does:
#   1. Checks both Swift xcframeworks are built (gitignored, regenerable —
#      build them with `just xcframework`).
#   2. Regenerates the Xcode project from project.yml (xcodegen generate).
#   3. Builds the Release macOS app (ad-hoc signed, as the app builds today) and
#      locates the produced astar.app.
#   4. Stages astar.app + an /Applications symlink and packages them into a
#      compressed, mountable astar.dmg via hdiutil.
#
# The .dmg lands at build/astar.dmg. Requires full Xcode (xcodebuild) + xcodegen.
#
# Usage:
#   Tools/make-dmg.sh
#
# ---------------------------------------------------------------------------
# M2 (FOLLOW-UP — real public distribution; NOT implemented here):
#
# This M1 DMG is AD-HOC signed (CODE_SIGN_IDENTITY="-"). Gatekeeper on any OTHER
# Mac will flag it as "damaged" / "unidentified developer"; recipients must
# right-click > Open, or run `xattr -dr com.apple.quarantine /Applications/astar.app`.
# That is fine for dev/local sharing but NOT for public download.
#
# For public distribution we need (BLOCKED on a paid Apple Developer ID — there
# is no signing identity yet):
#   1. A "Developer ID Application" certificate (paid Apple Developer account).
#   2. Codesign astar.app with that identity, the hardened runtime
#      (--options runtime), and the microphone entitlement
#      com.apple.security.device.audio-input (the IAX engine captures the mic):
#        codesign --force --deep --options runtime \
#          --entitlements astar.entitlements \
#          --sign "Developer ID Application: <NAME> (<TEAMID>)" astar.app
#   3. Build the DMG (as below), then notarize it via notarytool:
#        xcrun notarytool submit build/astar.dmg \
#          --apple-id "<APPLE_ID>" --team-id "<TEAMID>" \
#          --password "<APP_SPECIFIC_PASSWORD>" --wait
#      (or --keychain-profile / an App Store Connect API key).
#   4. Staple the ticket so it verifies offline:
#        xcrun stapler staple build/astar.dmg
# The WCH CH34x serial driver (UCI150) is a separate user install per onboarding —
# it is NOT bundled in the app.
# ---------------------------------------------------------------------------
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

SCHEME="astar (macOS)"
CONFIG="Release"
DD="build/DD"
APP_NAME="astar.app"
DMG_PATH="build/astar.dmg"
VOL_NAME="astar"

# 1. Preflight: both xcframeworks must exist (gitignored, regenerable).
REPO="$(cd "$HERE/../.." && pwd)"
missing=0
[ -e "$REPO/bindings/swift/astar.xcframework" ] || { echo "!! missing bindings/swift/astar.xcframework" >&2; missing=1; }
[ -e "$REPO/bindings/swift-serial/astarserial.xcframework" ] || { echo "!! missing bindings/swift-serial/astarserial.xcframework" >&2; missing=1; }
if [ "$missing" -ne 0 ]; then
  echo "!! run \`just xcframework\` (builds BOTH) and try again." >&2
  exit 1
fi

# 2. Regenerate the Xcode project from project.yml.
echo ">> xcodegen $(xcodegen --version 2>&1 | head -1) / $(xcodebuild -version 2>&1 | head -1)"
echo ">> xcodegen generate"
xcodegen generate
# xcodegen has been observed to exit 0 without producing a project on some CI
# images; fail loudly here rather than hitting an opaque xcodebuild error.
[ -d astar.xcodeproj ] || { echo "!! xcodegen produced no astar.xcodeproj" >&2; exit 1; }

# 3. Build the Release macOS app (ad-hoc signed so it runs locally).
#
# Pin ARCHS to the host arch: the vendored astarserial.xcframework carries only a
# macOS host slice (arm64 on Apple Silicon) — there is no x86_64 slice unless the
# upstream build was lipo'd universal — so an unpinned build that resolves to
# x86_64 (or tries universal) fails to link libastar_serial_sys.a. The
# resulting .app therefore matches the build host's architecture.
ARCH="$(uname -m)"
echo ">> building astar (macOS) [$CONFIG, $ARCH]…"
if ! xcodebuild -project astar.xcodeproj -scheme "$SCHEME" -configuration "$CONFIG" \
      -derivedDataPath "$DD" -destination "platform=macOS,arch=$ARCH" \
      ARCHS="$ARCH" ONLY_ACTIVE_ARCH=YES \
      CODE_SIGN_IDENTITY="-" CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM="" \
      build >/tmp/astar-dmg-build.log 2>&1; then
  echo "!! BUILD FAILED — last lines of /tmp/astar-dmg-build.log:" >&2
  grep -iE "error:" /tmp/astar-dmg-build.log | tail -20 >&2 || tail -20 /tmp/astar-dmg-build.log >&2
  exit 1
fi
echo ">> build ok"

# Locate the produced astar.app.
APP="$DD/Build/Products/$CONFIG/$APP_NAME"
[ -d "$APP" ] || { echo "!! app not found at $APP" >&2; exit 1; }

# 4. Package the DMG. Prefer dmgbuild for a styled window (astar.app on the left,
#    /Applications on the right, an arrow background, large icons) — it writes the
#    layout's .DS_Store directly, so it works headlessly in CI. Fall back to a
#    plain hdiutil DMG when dmgbuild isn't installed.
mkdir -p "$(dirname "$DMG_PATH")"
rm -f "$DMG_PATH"
BG="Tools/dmg/background.png"
SETTINGS="Tools/dmg/dmg-settings.py"

if command -v dmgbuild >/dev/null 2>&1; then
  echo ">> creating styled $DMG_PATH (dmgbuild)"
  dmgbuild -s "$SETTINGS" -D app="$APP" -D background="$BG" "$VOL_NAME" "$DMG_PATH"
else
  echo ">> dmgbuild not found — plain DMG (run 'pip3 install dmgbuild' for the styled window)"
  STAGE="$(mktemp -d)"
  trap 'rm -rf "$STAGE"' EXIT
  echo ">> staging $APP_NAME + /Applications symlink"
  cp -R "$APP" "$STAGE/$APP_NAME"
  ln -s /Applications "$STAGE/Applications"
  hdiutil create -volname "$VOL_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG_PATH" >/dev/null
fi

echo ""
echo ">> DONE — $DMG_PATH"
ls -lh "$DMG_PATH"
echo ">> NOTE: ad-hoc signed (M1). On another Mac, recipients must right-click > Open"
echo "         (or: xattr -dr com.apple.quarantine /Applications/astar.app). See M2 notes in this script."
