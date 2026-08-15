#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# render-icons.sh — regenerate the app icon + menu-bar/tray PNGs from the
# vector masters in art/, and (re)write their asset-catalog Contents.json.
#
# Masters (source of truth, diffable in git):
#   art/icon.svg            full-bleed square   -> iOS AppIcon (system masks corners)
#                                                  + Iced client window icon
#   art/icon-macos.svg      rounded squircle    -> macOS AppIcon
#   art/menubar-rainbow.svg rainbow asterisk    -> macOS menu-bar idle (colour) image
#                                                  + Iced client tray icons (all states)
#
# Requires: rsvg-convert (brew install librsvg).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # apps/macos
REPO="$(cd "$HERE/../.." && pwd)"                          # repo root
ART="$HERE/art"
ICONSET="$HERE/Resources/Assets.xcassets/AppIcon.appiconset"
RAINBOWSET="$HERE/Resources/Assets.xcassets/MenuBarRainbow.imageset"
command -v rsvg-convert >/dev/null || { echo "error: rsvg-convert not found (brew install librsvg)" >&2; exit 1; }

mkdir -p "$ICONSET" "$RAINBOWSET"
rm -f "$ICONSET"/*.png "$RAINBOWSET"/*.png

render() { rsvg-convert -w "$2" -h "$2" "$1" -o "$3"; }  # svg, px, out

# --- macOS AppIcon slices (16/32/128/256/512 @1x,2x) ---
mac_imgs=""
for base in 16 32 128 256 512; do
  for scale in 1 2; do
    px=$(( base * scale ))
    name="mac_${base}x${base}@${scale}x.png"
    render "$ART/icon-macos.svg" "$px" "$ICONSET/$name"
    mac_imgs="${mac_imgs}    {\"idiom\":\"mac\",\"size\":\"${base}x${base}\",\"scale\":\"${scale}x\",\"filename\":\"${name}\"},\n"
  done
done

# --- iOS AppIcon (single 1024 universal) ---
render "$ART/icon.svg" 1024 "$ICONSET/ios_1024.png"

printf '{\n  "images" : [\n' > "$ICONSET/Contents.json"
printf '    {"idiom":"universal","platform":"ios","size":"1024x1024","filename":"ios_1024.png"},\n' >> "$ICONSET/Contents.json"
printf "%b" "$mac_imgs" | sed '$ s/,$//' >> "$ICONSET/Contents.json"
printf '  ],\n  "info" : { "author" : "xcode", "version" : 1 }\n}\n' >> "$ICONSET/Contents.json"

# --- menu-bar rainbow, a non-template colour image for idle. Rendered at 24/48
#     px so it stays crisp at the 20pt display size (set in StatusItemController). ---
render "$ART/menubar-rainbow.svg" 24 "$RAINBOWSET/menubar_rainbow_18.png"
render "$ART/menubar-rainbow.svg" 48 "$RAINBOWSET/menubar_rainbow_18@2x.png"
cat > "$RAINBOWSET/Contents.json" <<'JSON'
{
  "images" : [
    { "idiom" : "universal", "scale" : "1x", "filename" : "menubar_rainbow_18.png" },
    { "idiom" : "universal", "scale" : "2x", "filename" : "menubar_rainbow_18@2x.png" }
  ],
  "info" : { "author" : "xcode", "version" : 1 }
}
JSON

# --- in-app brand asterisk (34pt header logo in MenuPopover, astar-a056):
#     the bare rainbow mark, 34/68 px so it stays crisp at @1x/@2x. ---
BRANDSET="$HERE/Resources/Assets.xcassets/BrandAsterisk.imageset"
mkdir -p "$BRANDSET"
rm -f "$BRANDSET"/*.png
render "$ART/menubar-rainbow.svg" 34 "$BRANDSET/brand_asterisk_34.png"
render "$ART/menubar-rainbow.svg" 68 "$BRANDSET/brand_asterisk_34@2x.png"
cat > "$BRANDSET/Contents.json" <<'JSON'
{
  "images" : [
    { "idiom" : "universal", "scale" : "1x", "filename" : "brand_asterisk_34.png" },
    { "idiom" : "universal", "scale" : "2x", "filename" : "brand_asterisk_34@2x.png" }
  ],
  "info" : { "author" : "xcode", "version" : 1 }
}
JSON

# --- Iced client window + tray icons, embedded by apps/gui/src/icons.rs ---
# The window icon is the full-bleed badge (icon.svg) at 256. BOTH tray PNG
# variants render from menubar-rainbow.svg so every status state shares ONE
# geometry and the tray icon never changes size between states (the Mac's
# astar-3f57 fix). The template variant is the same shape filled solid white
# — only its alpha matters, icons.rs tints the RGB per state.
#
# NOTE: this path hangs off the REPO ROOT, not off apps/macos — the Iced client
# lives at apps/gui/, a sibling of apps/macos/. Rooting it at $HERE renders the
# five PNGs into a phantom apps/macos/gui-rs/ directory and silently leaves the
# real, tracked apps/gui/assets/icon/*.png stale.
GUIICON="$REPO/apps/gui/assets/icon"
mkdir -p "$GUIICON"
rm -f "$GUIICON"/*.png
render "$ART/icon.svg" 256 "$GUIICON/astar-256.png"
render "$ART/menubar-rainbow.svg" 32 "$GUIICON/asterisk-rainbow-32.png"
render "$ART/menubar-rainbow.svg" 64 "$GUIICON/asterisk-rainbow-64.png"
tmp_template="$(mktemp -t astar-asterisk-template).svg"
sed 's|url(#rainbow)|#FFFFFF|' "$ART/menubar-rainbow.svg" > "$tmp_template"
render "$tmp_template" 32 "$GUIICON/asterisk-template-32.png"
render "$tmp_template" 64 "$GUIICON/asterisk-template-64.png"
rm -f "$tmp_template"

echo "rendered AppIcon ($(ls "$ICONSET"/*.png | wc -l | tr -d ' ') pngs) + MenuBarRainbow + BrandAsterisk + Iced client window/tray icons"
