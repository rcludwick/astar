#!/bin/sh
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Prove the AllStarLink Web Transceiver login and token mint with nothing but
# curl, one request at a time, and say what the portal answered at each step.
# This is the flow astar-asl3::mint_wt_token performs (docs/wt-web-transceiver.md,
# Stage 1); the point of this script is that anyone can watch it happen.
#
#   ASL_USER=<callsign> ASL_PASS='<portal account password>' ASL_NODE=<owned node> \
#     scripts/asl-wt-check.sh
#
#   --no-node      leave the node off the transceiver request (what the app
#                  does for an account saved without one) — expect no token
#   --show-token   print the whole token instead of its first four characters
#
# The password comes from the environment and is handed to curl through a
# 0600 temp file, so it appears on no command line and in no `ps` listing.
# Nothing from the portal is kept: the temp directory is removed on exit.
# Exit status: 0 every step passed · 2 login refused · 3 no token in the page
# · 4 transport failure · 64 usage.

set -eu

portal="${ASL_PORTAL:-https://www.allstarlink.org/portal}"
node="${ASL_NODE:-}"
show_token=no
for arg in "$@"; do
  case "$arg" in
    --no-node) node="" ;;
    --show-token) show_token=yes ;;
    -h|--help) sed -n '5,22p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 64 ;;
  esac
done
: "${ASL_USER:?set ASL_USER to your allstarlink.org callsign}"
: "${ASL_PASS:?set ASL_PASS to your allstarlink.org ACCOUNT password (not a node secret)}"

umask 077
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
printf '%s' "$ASL_PASS" > "$tmp/pass"

say() { printf '%s\n' "$*"; }

# ── Request 1: log in ─────────────────────────────────────────────────────────
say "1. POST $portal/login.php  user=$ASL_USER pass=<from ASL_PASS>"
if ! curl -sS -m 15 -o "$tmp/login.html" -D "$tmp/login.headers" -c "$tmp/jar" \
     --data-urlencode "user=$ASL_USER" --data-urlencode "pass@$tmp/pass" \
     "$portal/login.php"; then
  say "   FAIL: transport error talking to the portal"; exit 4
fi
status=$(sed -n '1s/^HTTP\/[0-9.]* \([0-9]*\).*/\1/p' "$tmp/login.headers")
location=$(awk 'tolower($1)=="location:" {print $2}' "$tmp/login.headers" | tr -d '\r' | head -1)
setcookies=$(awk 'tolower($1)=="set-cookie:" {print $2}' "$tmp/login.headers" | cut -d= -f1 | tr '\n' ' ')
kept=$(awk -F'\t' '$0 !~ /^#/ && NF>=7 {print $6}' "$tmp/jar" 2>/dev/null | tr '\n' ' ')
say "   answered HTTP $status${location:+ → $location}"
say "   Set-Cookie: ${setcookies:-(none)}"
say "   jar keeps:  ${kept:-(none)}   (curl drops the ones the server expires)"
case " $kept " in
  *" PHPSESSID "*) say "   PASS: session cookie issued — the password was accepted" ;;
  *) say "   FAIL: no PHPSESSID — the portal refused the login (wrong account password?)"; exit 2 ;;
esac

# ── Request 2: the transceiver page ───────────────────────────────────────────
if [ -n "$node" ]; then
  url="$portal/webtransceiver.php?node=$node"
  say "2. GET  $url  with the cookie jar"
else
  url="$portal/webtransceiver.php"
  say "2. GET  $url  with the cookie jar  (no node — the --no-node / unsaved-node case)"
fi
if ! code=$(curl -sS -m 15 -b "$tmp/jar" -o "$tmp/wt.html" -w '%{http_code}' "$url"); then
  say "   FAIL: transport error fetching the transceiver page"; exit 4
fi
size=$(wc -c < "$tmp/wt.html" | tr -d ' ')
say "   answered HTTP $code, $size bytes"
token=$(grep -o 'name="callingName"[^>]*value="[^"]*"' "$tmp/wt.html" | sed 's/.*value="\([^"]*\)".*/\1/' | head -1)
if [ -n "$token" ]; then
  if [ "$show_token" = yes ]; then shown=$token; else shown="$(printf '%s' "$token" | cut -c1-4)… (${#token} chars)"; fi
  say "   PASS: callingName token present: $shown"
  say "   This is what astar puts in the IAX2 CALLING_NAME. It is a session credential — do not paste it anywhere."
  exit 0
fi
say "   FAIL: no callingName token in the page"
if grep -qi 'node not found' "$tmp/wt.html"; then
  say "   the page says 'Node not found' — the node is missing or not one this account owns"
elif grep -qi 'login' "$tmp/wt.html" && grep -qi 'password' "$tmp/wt.html"; then
  say "   the page is the login form — the cookie jar did not authenticate the request"
fi
exit 3
