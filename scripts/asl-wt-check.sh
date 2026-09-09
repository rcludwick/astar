#!/bin/sh
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Prove the AllStarLink Web Transceiver token mint with nothing but curl, one
# request at a time, and say what the server answered at each step. This is the
# flow astar-asl3::mint_wt_token performs (docs/wt-web-transceiver.md, Stage 1):
# step 1 is the documented API, which needs no node and is where a good login
# stops; steps 2 and 3 are the legacy portal scrape the engine falls back to,
# run here for comparison whenever the API issued no token.
#
#   ASL_USER=<callsign> ASL_PASS='<portal account password>' ASL_NODE=<owned node> \
#     scripts/asl-wt-check.sh
#
#   --no-node      leave the node off the transceiver request (what the app
#                  does for an account saved without one) — expect no token
#                  from the fallback scrape; the API does not need one, and
#                  without a node the engine has no fallback at all
#   --show-token   print whole tokens instead of their first four characters
#
# The password comes from the environment and is handed to curl through 0600
# temp files, so it appears on no command line and in no `ps` listing.
# Nothing from the server is kept: the temp directory is removed on exit.
# Exit status: 0 a token was minted · 2 the login was refused (by the API, or by
# the portal — after an API refusal the scrape's own result is informational and
# the exit stays 2) · 3 no token in the page · 4 transport failure with no node
# to fall back with · 64 usage.

set -eu

portal="${ASL_PORTAL:-https://www.allstarlink.org/portal}"
node="${ASL_NODE:-}"
show_token=no
for arg in "$@"; do
  case "$arg" in
    --no-node) node="" ;;
    --show-token) show_token=yes ;;
    -h|--help) sed -n '6,28p' "$0"; exit 0 ;;
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

# Mask token values in whatever comes through, unless --show-token: keep the
# first four characters of a long one, all of a short one hidden.
mask() {
  if [ "$show_token" = yes ]; then
    cat
  else
    sed -e 's/"token"[ ]*:[ ]*"\([^"][^"][^"][^"]\)[^"]*"/"token":"\1…"/g' \
        -e 's/"token"[ ]*:[ ]*"[^"]\{1,3\}"/"token":"…"/g'
  fi
}

# ── Request 1: the documented API ─────────────────────────────────────────────
# The endpoint hangs off the portal's ORIGIN, not under /portal.
origin=$(printf '%s' "$portal" | sed 's#^\(https*://[^/]*\).*#\1#')
api="$origin/api/v2/auth-wt-legacy"
say "1. POST $api  {\"username\":\"$ASL_USER\",\"password\":<from ASL_PASS>}  (no node needed)"
# refused=yes means the API said "wrong credentials". The engine STOPS there —
# it never retries a refusal against the portal — so the steps below become
# comparison only and the script must not report success after one.
refused=no
if ! command -v python3 >/dev/null 2>&1; then
  say "   SKIP: no python3 — cannot build the JSON body; the engine still tries this call"
  say "   running the fallback path below for what it can tell you:"
# Built by json.dumps so the password is escaped by something that knows the
# grammar, and written to a 0600 file so it never reaches a command line.
elif ! python3 -c 'import json,os,sys; print(json.dumps({"username":os.environ["ASL_USER"],"password":open(sys.argv[1]).read()}))' \
     "$tmp/pass" > "$tmp/api.json" 2>"$tmp/api.err"; then
  say "   SKIP: could not build the JSON body ($(head -1 "$tmp/api.err" 2>/dev/null))"
  say "   running the fallback path below for what it can tell you:"
elif ! code=$(curl -sS -m 15 -H 'Content-Type: application/json' --data @"$tmp/api.json" \
     -o "$tmp/api.body" -w '%{http_code}' "$api"); then
  say "   FAIL: transport error talking to the API"
  if [ -z "$node" ]; then
    say "   with no node there is nothing to fall back to — this is the Http error astar reports"
    exit 4
  fi
  say "   a node is configured, so the engine falls back to the scrape; doing the same:"
else
  say "   answered HTTP $code"
  say "   $(mask < "$tmp/api.body" | head -c 400)"
  token=$(grep -o '"token"[ ]*:[ ]*"[^"]*"' "$tmp/api.body" 2>/dev/null | sed 's/.*"\([^"]*\)"$/\1/' | head -1)
  if [ -n "$token" ]; then
    if [ "$show_token" = yes ]; then shown=$token; else shown="$(printf '%s' "$token" | cut -c1-4)… (${#token} chars)"; fi
    say "   PASS: the API minted a token: $shown"
    say "   This is what astar puts in the IAX2 CALLING_NAME. It is a session credential — do not paste it anywhere."
    exit 0
  fi
  case "$code" in
    401)
      refused=yes
      say "   the API refused the login (HTTP 401) — wrong account password?"
      say "   the ENGINE STOPS HERE: a refusal is never retried against the portal scrape."
      say "   the steps below run for comparison only — a token from them will not make astar work."
      ;;
    400) say "   the API rejected the request body (HTTP 400) — with a node the engine falls back, without one it reports Http" ;;
    *) say "   the API issued no token (HTTP $code) — with a node the engine falls back, without one it reports Http" ;;
  esac
fi

# ── Request 2: log in (the fallback path) ─────────────────────────────────────
say "2. POST $portal/login.php  user=$ASL_USER pass=<from ASL_PASS>"
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

# ── Request 3: the transceiver page (the fallback path) ───────────────────────
if [ -n "$node" ]; then
  url="$portal/webtransceiver.php?node=$node"
  say "3. GET  $url  with the cookie jar"
else
  url="$portal/webtransceiver.php"
  say "3. GET  $url  with the cookie jar  (no node — the --no-node / unsaved-node case)"
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
  if [ "$refused" = yes ]; then
    say "   INFORMATIONAL ONLY: the API refused this login, and astar stops there — this token is not"
    say "   one astar will ever mint. Fix the account password."
    exit 2
  fi
  say "   This is what astar puts in the IAX2 CALLING_NAME. It is a session credential — do not paste it anywhere."
  exit 0
fi
say "   FAIL: no callingName token in the page"
if grep -qi 'node not found' "$tmp/wt.html"; then
  say "   the page says 'Node not found' — the node is missing or not one this account owns"
elif grep -qi 'login' "$tmp/wt.html" && grep -qi 'password' "$tmp/wt.html"; then
  say "   the page is the login form — the cookie jar did not authenticate the request"
fi
if [ "$refused" = yes ]; then exit 2; fi
exit 3
