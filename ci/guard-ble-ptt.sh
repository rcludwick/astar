#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Merge invariant for the Bluetooth PTT button.
#
# crates/astar-ble-ptt decides when a wireless button may key a transmitter.
# Its safety argument holds only for code its own tests cover, so:
#
#   1. it has no normal dependencies at all (proptest and serde_json are
#      dev-dependencies, for the tests);
#   2. the node daemon never links it: astar-server has no Bluetooth adapter
#      and no button, and must not grow a wireless keying source by accident;
#   3. only apps/macos/Sources/CoreBluetoothCentral.swift constructs a
#      CBCentralManager, so the macOS Bluetooth prompt and every connection
#      come from one reviewed file, and AstarCore never imports CoreBluetooth
#      (it stays testable with a fake central);
#   4. nothing under apps/macos writes to a GATT characteristic: subscribing
#      and reading RSSI are the only traffic the app may cause.
#
# Rules 3 and 4 are literal text matches (git grep -F). They include comments,
# doc comments and string literals, so the literal `writeValue(` must not
# appear anywhere under apps/macos, not even in a comment that says the code
# never calls it; describe it as the characteristic-write API instead.
#
# Checked against cargo's resolved graph and against every tracked or new
# (not ignored) file, so a build directory never trips it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The file checks below use git grep; outside a work tree they would pass
# vacuously, so refuse to run there.
git rev-parse --is-inside-work-tree >/dev/null

errfile="$(mktemp)"
trap 'rm -f "$errfile"' EXIT

fail=0

echo "==> astar-ble-ptt must have no normal dependencies"
if ! tree=$(cargo tree -p astar-ble-ptt --edges normal --prefix none 2>"$errfile"); then
  echo "FAIL: 'cargo tree -p astar-ble-ptt' errored — is the crate in the workspace?"
  cat "$errfile"
  fail=1
else
  extra=$(grep -vE '^(astar-ble-ptt v|$)' <<<"$tree" || true)
  if [ -n "$extra" ]; then
    echo "FAIL: astar-ble-ptt has normal dependencies:"
    echo "$extra"
    fail=1
  fi
fi

echo "==> astar-server must not link astar-ble-ptt"
if ! server=$(cargo tree -p astar-server --edges normal --prefix none 2>"$errfile"); then
  echo "FAIL: 'cargo tree -p astar-server' errored"
  cat "$errfile"
  fail=1
elif grep -qE '^astar-ble-ptt v' <<<"$server"; then
  echo "FAIL: astar-server links astar-ble-ptt; the node has no Bluetooth button"
  fail=1
fi

echo "==> CoreBluetooth stays in CoreBluetoothCentral.swift"
managers=$(git grep --untracked -l -F 'CBCentralManager(' -- apps/macos/Sources \
  | grep -vxF 'apps/macos/Sources/CoreBluetoothCentral.swift' || true)
if [ -n "$managers" ]; then
  echo "FAIL: CBCentralManager( outside apps/macos/Sources/CoreBluetoothCentral.swift:"
  echo "$managers"
  fail=1
fi

imports=$(git grep --untracked -l -F 'import CoreBluetooth' -- apps/macos/Packages/AstarCore || true)
if [ -n "$imports" ]; then
  echo "FAIL: import CoreBluetooth in AstarCore:"
  echo "$imports"
  fail=1
fi

echo "==> nothing under apps/macos writes to a characteristic"
writes=$(git grep --untracked -l -F 'writeValue(' -- apps/macos || true)
if [ -n "$writes" ]; then
  echo "FAIL: writeValue( under apps/macos:"
  echo "$writes"
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "See the header of ci/guard-ble-ptt.sh for why each rule exists."
  exit 1
fi

echo "OK: astar-ble-ptt is dependency-free and CoreBluetooth is confined"
