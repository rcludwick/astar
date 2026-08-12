#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Generate demo screenshots for each UI state into apps/gui/.shots/.
#
# Builds the release binary once, then runs `astar-gui --shot STATE FILE` for
# each scripted state. Each run opens a window, renders the forced state,
# captures it to a PNG via Iced's window::screenshot, and exits.
#
# Usage:  ./shots.sh           # all states
#         ./shots.sh tx rx     # only the named states
set -euo pipefail

cd "$(dirname "$0")"

OUT=".shots"
mkdir -p "$OUT"

echo "==> building astar-gui (release)"
cargo build --release --quiet

# One cargo workspace: the shared target/ dir lives at the repo root.
BIN="$(cd ../.. && pwd)/target/release/astar-gui"

STATES=("$@")
if [ ${#STATES[@]} -eq 0 ]; then
  STATES=(idle connecting connected rx tx error config config-comp config-new favorites dialpad dialpad-idle dialpad-sending network-picker network-picker-idle m17-dial)
fi

for state in "${STATES[@]}"; do
  png="$OUT/$state.png"
  echo "==> shot: $state -> $png"
  # The app self-exits after capture; cap the wallclock as a safety net.
  if command -v timeout >/dev/null 2>&1; then
    timeout 30 "$BIN" --shot "$state" "$png" || true
  else
    "$BIN" --shot "$state" "$png" || true
  fi
  if [ -f "$png" ]; then
    echo "    ok ($(du -h "$png" | cut -f1))"
  else
    echo "    FAILED: no $png produced" >&2
  fi
done

echo "==> done; PNGs in $OUT/"
