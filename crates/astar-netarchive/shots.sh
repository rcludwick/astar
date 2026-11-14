#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Offscreen screenshots of the recordings pages into crates/astar-netarchive/.shots/.
#
# Starts the fixture preview (127.0.0.1, free port, temp dir) and drives
# headless Chrome at it. Nothing opens a window and nothing touches the screen.
#
# Usage:  ./shots.sh [--check]
#   --check   also dump the session DOM at #t=40 and require exactly one
#             `b now` element whose data-seq is the over playing then.
# Env:    ASTAR_CHROME   path to a Chrome/Chromium binary
#         ASTAR_CHROME_NARROW  a chrome-headless-shell for the phone shot. Chrome's
#                        new headless mode will not lay out narrower than 500 px,
#                        so a 390 px shot would really be a cropped 500 px page.
#                        Found in Playwright's cache when unset.
#         NETS_MOCKUPS   folder holding recordings-page.html, video-frames.html
set -euo pipefail

cd "$(dirname "$0")"
CRATE="$(pwd)"
OUT="$CRATE/.shots"
CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1

CHROME=""
if [ -n "${ASTAR_CHROME:-}" ] && [ -x "$ASTAR_CHROME" ]; then
  CHROME="$ASTAR_CHROME"
elif [ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ]; then
  CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
elif command -v chromium >/dev/null 2>&1; then
  CHROME="$(command -v chromium)"
elif command -v google-chrome >/dev/null 2>&1; then
  CHROME="$(command -v google-chrome)"
fi
if [ -z "$CHROME" ]; then
  if [ "$CHECK" = 1 ]; then
    echo "no Chrome found (set ASTAR_CHROME); --check cannot run" >&2
    exit 1
  fi
  echo "no Chrome found (set ASTAR_CHROME); skipping screenshots"
  exit 0
fi

NARROW=""
if [ -n "${ASTAR_CHROME_NARROW:-}" ] && [ -x "$ASTAR_CHROME_NARROW" ]; then
  NARROW="$ASTAR_CHROME_NARROW"
else
  NARROW="$(ls -d "$HOME"/Library/Caches/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell \
    "$HOME"/.cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell 2>/dev/null | sort -V | tail -n 1 || true)"
fi

echo "==> building the preview"
cargo build -q -p astar-netarchive --features fixture --example nets_preview
TARGET="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')"
BIN="$TARGET/debug/examples/nets_preview"

WORK="$(mktemp -d)"
LOG="$WORK/preview.log"
PID=""
cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

NETS_PREVIEW_ACCESS="${NETS_PREVIEW_ACCESS:-admin}" "$BIN" "$WORK/archive" 0 >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 100); do
  grep -q '^now_seq ' "$LOG" 2>/dev/null && break
  kill -0 "$PID" 2>/dev/null || { cat "$LOG"; echo "preview exited"; exit 1; }
  sleep 0.1
done
URL="$(sed -n 's/^url //p' "$LOG")"
SESSION="$(sed -n 's/^session //p' "$LOG")"
NOW_SEQ="$(sed -n 's/^now_seq //p' "$LOG")"
[ -n "$URL" ] && [ -n "$SESSION" ] || { cat "$LOG"; echo "preview did not start"; exit 1; }
BASE="${URL%/nets/}"
echo "==> preview at $URL"

mkdir -p "$OUT"
PROFILE="$WORK/chrome"
CHROME_LOG="$WORK/chrome.log"
# run_chrome <stdout-file> <args...>: Chrome with a 60 s wall-clock limit (macOS
# has no `timeout`). Output goes to $CHROME_LOG; its tail is printed on failure.
run_chrome() {
  local out="$1" cp wp rc=0
  shift
  "$@" >"$out" 2>>"$CHROME_LOG" &
  cp=$!
  ( sleep 60; pkill -KILL -P "$cp" 2>/dev/null; kill -KILL "$cp" 2>/dev/null ) &
  wp=$!
  wait "$cp" || rc=$?
  kill "$wp" 2>/dev/null || true
  wait "$wp" 2>/dev/null || true
  if [ "$rc" != 0 ]; then
    echo "chrome failed (exit $rc; killed after 60 s if hung). Last of its output:" >&2
    tail -n 20 "$CHROME_LOG" >&2
    return 1
  fi
}
shot() { # name url width height [extra flags...]
  local name="$1" url="$2" w="$3" h="$4"
  shift 4
  run_chrome /dev/null "$CHROME" --headless=new --disable-gpu --no-first-run --hide-scrollbars \
    --user-data-dir="$PROFILE" --force-device-scale-factor=2 \
    --virtual-time-budget=4000 --window-size="$w,$h" "$@" \
    --screenshot="$OUT/$name.png" "$url" || { echo "chrome failed for $name" >&2; exit 1; }
  echo "    $name.png"
}
urlencode() { python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1]))' "$1"; }

# The scheme is set outright: headless Chrome otherwise follows the Mac's own
# appearance, so a "light" shot on a dark Mac came out dark. (Chrome's
# WebContentsForceDark is not used: it repaints pages that declare no dark
# scheme, which distorted the mockups.)
DARK=(--blink-settings=preferredColorScheme=0)
LIGHT=(--blink-settings=preferredColorScheme=1)
shot list-dark "$URL" 1280 900 "${DARK[@]}"
shot list-light "$URL" 1280 900 "${LIGHT[@]}"
shot session-dark "$BASE${SESSION}#t=40" 1280 1800 "${DARK[@]}"
shot session-light "$BASE${SESSION}#t=40" 1280 1800 "${LIGHT[@]}"
if [ -n "$NARROW" ]; then
  run_chrome /dev/null "$NARROW" --disable-gpu --no-first-run --hide-scrollbars \
    --user-data-dir="$PROFILE-narrow" --force-device-scale-factor=2 \
    --virtual-time-budget=4000 --window-size=390,1600 "${DARK[@]}" \
    --screenshot="$OUT/session-phone.png" "$BASE${SESSION}#t=40" ||
    { echo "chrome-headless-shell failed for session-phone" >&2; exit 1; }
  echo "    session-phone.png (390 px, $(basename "$(dirname "$(dirname "$NARROW")")"))"
else
  echo "    (no chrome-headless-shell: session-phone is Chrome's 500 px minimum, cropped to 390)"
  shot session-phone "$BASE${SESSION}#t=40" 390 1600 "${DARK[@]}"
fi
shot replay-dark "$BASE${SESSION}#replay&t=40" 1280 800 "${DARK[@]}"
if [ -n "${NETS_MOCKUPS:-}" ]; then
  shot mockup-page "file://$(urlencode "$NETS_MOCKUPS/recordings-page.html")" 1280 1800 "${DARK[@]}"
  shot mockup-frames "file://$(urlencode "$NETS_MOCKUPS/video-frames.html")" 1280 1800 "${DARK[@]}"
fi

if [ "$CHECK" = 1 ]; then
  run_chrome "$WORK/dom.html" "$CHROME" --headless=new --disable-gpu --no-first-run \
    --user-data-dir="$PROFILE" --virtual-time-budget=4000 --dump-dom \
    "$BASE${SESSION}#t=40" || { echo "check failed: Chrome could not dump the DOM" >&2; exit 1; }
  # grep exits 1 on no match; `|| true` keeps pipefail from hiding the diagnosis.
  NOWS="$(grep -oE '<[^>]*class="b now"[^>]*>' "$WORK/dom.html" || true)"
  if [ -z "$NOWS" ]; then COUNT=0; else COUNT="$(printf '%s\n' "$NOWS" | wc -l | tr -d ' ')"; fi
  if [ "$COUNT" != 1 ]; then
    echo "check failed: at $BASE${SESSION}#t=40 expected exactly one 'b now' element, found $COUNT" >&2
    echo "  (the DOM is $(wc -c <"$WORK/dom.html" | tr -d ' ') bytes; 'b' elements: $(grep -oE 'class="b[ "][^>]*' "$WORK/dom.html" | wc -l | tr -d ' ' || true))" >&2
    exit 1
  fi
  SEQ="$(printf '%s' "$NOWS" | sed -n 's/.*data-seq="\([^"]*\)".*/\1/p')"
  if [ "$SEQ" != "$NOW_SEQ" ]; then
    echo "check failed: 'b now' has data-seq=${SEQ:-<none>}, expected $NOW_SEQ" >&2
    exit 1
  fi
  echo "check ok: one 'b now' at data-seq=$NOW_SEQ"
fi
