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
# SIGNING (astar-43eb). What comes out depends on what this machine has, and
# the script tells you which of the three you got:
#
#   ad-hoc            no Developer ID identity in the keychain. Fine for local
#                     use; another Mac reports "damaged"/"unidentified
#                     developer" and the recipient must right-click > Open (or
#                     xattr -dr com.apple.quarantine /Applications/astar.app).
#   signed            a Developer ID Application identity was found, so the app
#                     carries the hardened runtime, a secure timestamp, and the
#                     mic entitlement. Gatekeeper still refuses it elsewhere —
#                     "Unnotarized Developer ID" — because a signature alone was
#                     never enough after macOS 10.15.
#   signed+notarized  as above plus an Apple-issued ticket, stapled to the DMG
#                     so it verifies OFFLINE. This is the only output fit for
#                     public download.
#
# Nothing here is hard-coded to one developer: the identity is discovered from
# the keychain (override: ASTAR_SIGN_IDENTITY) and the notary credentials live
# in a keychain profile (override: ASTAR_NOTARY_PROFILE, default astar-notary).
# A clone with neither still builds — it just lands on "ad-hoc".
#
# One-time notary setup, using an App Store Connect API key:
#   xcrun notarytool store-credentials "astar-notary" \
#     --key <AuthKey_XXXXXXXXXX.p8> --key-id <KEY_ID> --issuer <ISSUER_ID>
#
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

# ---------------------------------------------------------------------------
# 3b. Developer ID signing (astar-43eb).
#
# xcodebuild above signs ad-hoc so a build from source works for anyone. If a
# "Developer ID Application" identity is on this machine we re-sign properly on
# top of that: hardened runtime (notarization refuses anything without it), a
# secure timestamp (without one the signature stops validating when the cert
# eventually expires), and the mic entitlement.
#
# The identity is DISCOVERED, never hard-coded — a contributor with their own
# Developer ID gets their own, and someone with none still gets a working
# ad-hoc DMG instead of a build failure. Override with ASTAR_SIGN_IDENTITY.
# ---------------------------------------------------------------------------
SIGN_IDENTITY="${ASTAR_SIGN_IDENTITY:-$(
  security find-identity -v -p codesigning 2>/dev/null |
    sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -1
)}"
ENTITLEMENTS="astar.entitlements"
SIGNED=0

if [ -n "$SIGN_IDENTITY" ]; then
  [ -f "$ENTITLEMENTS" ] || { echo "!! missing $ENTITLEMENTS" >&2; exit 1; }
  echo ">> signing as: $SIGN_IDENTITY"
  # Inside-out: nested Mach-O first, then the bundle. (`--deep` is the lazy
  # equivalent and Apple advises against it — it cannot apply per-binary
  # entitlements and silently re-signs things you did not mean to.)
  while IFS= read -r -d '' nested; do
    codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$nested"
  done < <(find "$APP/Contents" -type f \( -name '*.dylib' -o -name '*.so' \) -print0)

  codesign --force --options runtime --timestamp \
    --entitlements "$ENTITLEMENTS" --sign "$SIGN_IDENTITY" "$APP"

  codesign --verify --deep --strict --verbose=2 "$APP" 2>&1 | sed 's/^/   /'
  SIGNED=1
else
  echo ">> no Developer ID Application identity found — leaving the ad-hoc signature"
  echo "   (set ASTAR_SIGN_IDENTITY, or see the M2 notes at the top of this script)"
fi

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

# ---------------------------------------------------------------------------
# 5. Notarize + staple (astar-43eb). Apple scans the DMG and issues a ticket;
#    stapling attaches it so Gatekeeper clears the app OFFLINE, on a machine
#    that has never seen it. Skipped unless the app was really signed AND a
#    notarytool keychain profile exists — an ad-hoc DMG cannot be notarized,
#    and a missing profile is a setup gap, not a build error.
#
#    Credentials live in the keychain, never in this repo or its environment:
#      xcrun notarytool store-credentials "astar-notary" \
#        --key <AuthKey_XXX.p8> --key-id <KEY_ID> --issuer <ISSUER_ID>
# ---------------------------------------------------------------------------
NOTARY_PROFILE="${ASTAR_NOTARY_PROFILE:-astar-notary}"
NOTARIZED=0

# Probe with notarytool rather than poking at the keychain: `store-credentials`
# does not leave an item `security find-generic-password` can find by service or
# account, so guessing at its storage silently skipped notarization on a machine
# that was correctly configured. `history` is a cheap authenticated round trip —
# it succeeds only if the profile exists AND Apple accepts the credentials, which
# also means an offline machine correctly declines to try.
if [ "$SIGNED" = 1 ] && [ "${ASTAR_SKIP_NOTARIZE:-0}" != 1 ] &&
   xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
  echo ">> notarizing (profile: $NOTARY_PROFILE) — Apple's scan usually takes a few minutes…"
  if xcrun notarytool submit "$DMG_PATH" --keychain-profile "$NOTARY_PROFILE" --wait; then
    xcrun stapler staple "$DMG_PATH"
    NOTARIZED=1
  else
    echo "!! notarization failed — the DMG is signed but NOT notarized." >&2
    echo "   'xcrun notarytool log <submission-id> --keychain-profile $NOTARY_PROFILE' explains why." >&2
  fi
elif [ "$SIGNED" = 1 ]; then
  echo ">> skipping notarization (no '$NOTARY_PROFILE' keychain profile, or ASTAR_SKIP_NOTARIZE=1)"
fi

echo ""
echo ">> DONE — $DMG_PATH"
ls -lh "$DMG_PATH"
if [ "$NOTARIZED" = 1 ]; then
  echo ">> signed + notarized + stapled. Gatekeeper verdict:"
  spctl -a -vvv -t open --context context:primary-signature "$DMG_PATH" 2>&1 | sed 's/^/   /'
elif [ "$SIGNED" = 1 ]; then
  echo ">> signed with Developer ID, NOT notarized — other Macs will still refuse it"
  echo "   ('spctl' reports: Unnotarized Developer ID). Set up the notary profile above."
else
  echo ">> NOTE: ad-hoc signed (M1). On another Mac, recipients must right-click > Open"
  echo "         (or: xattr -dr com.apple.quarantine /Applications/astar.app). See M2 notes in this script."
fi
