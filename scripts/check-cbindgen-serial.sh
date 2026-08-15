#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Regenerate the astar-serial-sys C header and fail if it drifts from the
# committed copy. Run after changing any extern "C" fn / #[repr(C)] type / error
# const in astar-serial-sys.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="$ROOT/crates/astar-serial-sys/cbindgen.toml"
HEADER="$ROOT/crates/astar-serial-sys/include/astarserial.h"

GEN="$(mktemp)"
trap 'rm -f "$GEN"' EXIT

cbindgen --config "$CONFIG" --crate astar-serial-sys --output "$GEN" --quiet

# Secret-free guard: this ABI carries only a port_path string; no secret material.
if grep -Eiq '\b(secret|password|token)\b' "$GEN"; then
  echo "ERROR: a secret/password/token name leaked into the serial C header." >&2
  grep -Ein '\b(secret|password|token)\b' "$GEN" >&2
  exit 1
fi

if ! diff -u "$HEADER" "$GEN"; then
  echo "" >&2
  echo "astarserial.h is out of date. Regenerate with:" >&2
  echo "  cbindgen --config $CONFIG --crate astar-serial-sys --output $HEADER" >&2
  exit 1
fi
echo "astarserial.h up to date."
