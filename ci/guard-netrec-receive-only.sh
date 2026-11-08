#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Keeps astar-netrec, the net recorder, unable to transmit.
#
# The daemon only ever holds a ListenOnlyStation, whose allow-list has no
# keying call in it. This guard is the backstop if someone reaches around it.
#
# 1. No keying or transmit-capable call appears anywhere under the crate, src
#    or tests, except in the one `// RECEIVE-ONLY DENY LIST` comment block in
#    src/listen.rs. That includes a plain socket connect: test sockets use
#    send_to and recv_from, so the rule needs no exceptions.
# 2. add_bridge_member, set_bridge_config and set_dstar_vocoder appear only in
#    src/listen.rs.
# 3. The words Station, ConsoleSession and DstarSession appear only in
#    src/listen.rs, so nothing else can hold what could key a radio. Types
#    astar_station re-exports under other names (DstarVocoder) stay allowed,
#    and so does any of these words in a `//` comment.
#
# This is a text match on purpose, so rules 1 and 2 read comments too. A name
# counts as a call when it stands alone before `(`, or as a path (`Station::name`).
# A longer name that merely ends in one of these (m17_monitor_connect,
# dstar_connect) is not a match.
#
# NETREC_DIR points the guard at another tree, for its own tests.
set -euo pipefail
cd "$(dirname "$0")/.."

dir="${NETREC_DIR:-crates/astar-netrec}"
listen="$dir/src/listen.rs"
marker='^[[:space:]]*// RECEIVE-ONLY DENY LIST'

deny='set_ptt|link_key|link_set_mode|sync_link_keying'
deny+='|send_dtmf[A-Za-z0-9_]*|announce[A-Za-z0-9_]*'
deny+='|echolink_send_chat|answer|enable_inbound|set_mode'
deny+='|connect|connect_wt|connect_wt_at|m17_connect'
deny+='|link_connect|link_connect_at|echolink_connect|echolink_node_connect'
deny+='|ysf_connect|nxdn_connect|dmr_connect'
deny+='|start_inbound[A-Za-z0-9_]*|answer_pending|connect_with_[A-Za-z0-9_]*'
listen_only='add_bridge_member|set_bridge_config|set_dstar_vocoder'
holders='Station|ConsoleSession|DstarSession'

# `\b` and `\s` are not portable to every grep this runs under.
call_re() {
  printf '%s' "(^|[^A-Za-z0-9_])($1)[[:space:]]*(::<[^>]*>)?[[:space:]]*\\(|::($1)([^A-Za-z0-9_]|\$)"
}

# Print matching lines as file:line:text. grep's exit 1 is "no match", and
# anything above that is a real failure.
scan() {
  local status=0
  grep -rnE --include='*.rs' -e "$1" "$dir" || status=$?
  if [ "$status" -ge 2 ]; then
    echo "FAIL: the scan of $dir could not run" >&2
    exit 1
  fi
}

if [ ! -f "$listen" ]; then
  echo "FAIL: $listen is missing, so the guard cannot run"
  exit 1
fi

blocks=$(grep -cE "$marker" "$listen" || true)
if [ "$blocks" -ne 1 ]; then
  echo "FAIL: $listen must have exactly one RECEIVE-ONLY DENY LIST block, found $blocks"
  exit 1
fi

# The block runs from the marker to the last unbroken `//` line after it.
read -r block_start block_end < <(awk -v marker="$marker" '
  !start && $0 ~ marker { start = NR; end = NR; next }
  start && !done { if ($0 ~ /^[[:space:]]*\/\//) end = NR; else done = 1 }
  END { print start, end }
' "$listen")

fail=0

hits=$(scan "$(call_re "$deny")" | awk -F: -v f="$listen" -v s="$block_start" -v e="$block_end" \
  '!($1 == f && $2 >= s && $2 <= e)')
if [ -n "$hits" ]; then
  echo "FAIL: astar-netrec names a keying or transmit-capable call. It must stay receive-only:"
  echo "$hits"
  fail=1
fi

hits=$(scan "$(call_re "$listen_only")" | awk -F: -v f="$listen" '$1 != f')
if [ -n "$hits" ]; then
  echo "FAIL: these calls belong in $listen and nowhere else:"
  echo "$hits"
  fail=1
fi

# Whole words, with any `//` comment cut off the line first.
hits=$(find "$dir" -name '*.rs' ! -path "$listen" -exec awk -v words="$holders" '
  BEGIN { re = "(^|[^A-Za-z0-9_])(" words ")([^A-Za-z0-9_]|$)" }
  FNR == 1 { file = FILENAME }
  { code = $0; sub(/\/\/.*/, "", code); if (code ~ re) printf "%s:%d:%s\n", file, FNR, $0 }
' {} +)
if [ -n "$hits" ]; then
  echo "FAIL: only $listen may hold a station or a session:"
  echo "$hits"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "FAIL: netrec receive-only guard"
  exit 1
fi
echo "netrec receive-only guard: ok"
