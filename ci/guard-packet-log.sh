#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Keeps the packet log away from the paths that carry secrets.
#
# The packet log's first defence is its media filter. The second is that some
# secrets never pass a tap at all, and nothing but this check holds that up:
# a test can't fail for a tap that isn't there yet. So these sources must not
# mention the tap, not even in a comment:
#
#   crates/astar-iax/src/registration.rs     the IAX2 registration thread (MD5)
#   crates/astar-console/src/echolink/net.rs EchoLink's password, over TCP
#   crates/astar-asl3/src/mint.rs            the ASL portal client
#
# A missing file fails too, so a rename can't switch the check off unnoticed.
set -euo pipefail
cd "$(dirname "$0")/.."

tap_re='astar_record|CallPacketTap|PacketSlot|capture\(|capture_at\('
secret_paths=(
  crates/astar-iax/src/registration.rs
  crates/astar-console/src/echolink/net.rs
  crates/astar-asl3/src/mint.rs
)

fail=0
for p in "${secret_paths[@]}"; do
  if [ ! -f "$p" ]; then
    echo "FAIL: $p is missing, so the packet tap check cannot run (update this guard if it moved)"
    fail=1
    continue
  fi
  set +e
  hits=$(grep -nE "$tap_re" "$p" 2>&1)
  status=$?
  set -e
  if [ "$status" -eq 0 ]; then
    echo "FAIL: $p carries secrets and must not touch the packet log:"
    echo "$hits"
    fail=1
  elif [ "$status" -ge 2 ]; then
    echo "FAIL: the packet tap grep could not run on $p:"
    echo "$hits"
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "FAIL: packet log guard"
  exit 1
fi
echo "packet log guard: ok"
