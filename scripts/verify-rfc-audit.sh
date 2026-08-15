#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Verify every Evidence citation in docs/spec/rfc5456-audit.md resolves.
#
# - Rust function refs (`module::path::fn` or `fn_name`) must exist in `crates/`.
# - Fixture refs (`fixtures/<name>.pcap` etc.) must exist under
#   `crates/astar-conformance/fixtures/`.
# - Ticket refs (`au:iax-XXXX` — the historical citation form; the au and
#   beads trackers are both gone) must be well-formed, and must exist in
#   docs/issues-archive.jsonl when that file is present (it is gitignored /
#   local-only, so CI checks the format only).
# - TODO refs (`TODO(R-X.Y-NN)`) must be present somewhere under `crates/`.
#
# Exits 0 on success, nonzero with a per-citation diff on miss.
#
# Usage:  scripts/verify-rfc-audit.sh [path-to-audit-md]

set -euo pipefail

AUDIT="${1:-docs/spec/rfc5456-audit.md}"
FIXTURES_DIR="crates/astar-conformance/fixtures"
SRC_DIR="crates"

if [[ ! -f "$AUDIT" ]]; then
  echo "verify-rfc-audit: $AUDIT not found" >&2
  exit 2
fi

fail=0

# Extract Evidence cells. Audit rows look like:
#   | R-X.Y-NN | desc | LEVEL | STATUS | ev1, ev2 | notes |
# Skip header rows (those starting with `| ID |` or `|----`).
mapfile -t evidence_cells < <(
  awk -F'|' '
    /^\| R-/ {
      # Field 6 in the split (1-indexed: leading empty, ID, req, level, status, ev, notes)
      gsub(/^ +| +$/, "", $6);
      if ($6 != "") print $6;
    }
  ' "$AUDIT"
)

check_ref() {
  local ref="$1"
  case "$ref" in
    au:iax-*)
      local ticket="${ref#au:}"
      if [[ ! "$ticket" =~ ^iax-[0-9a-f]{4}$ ]]; then
        echo "MISS: $ref (malformed ticket id)" >&2
        fail=1
      elif [[ -f docs/issues-archive.jsonl ]] \
        && ! grep -q "\"$ticket\"" docs/issues-archive.jsonl; then
        echo "MISS: $ref (not in docs/issues-archive.jsonl)" >&2
        fail=1
      fi
      ;;
    fixtures/*)
      if [[ ! -e "$FIXTURES_DIR/${ref#fixtures/}" ]]; then
        echo "MISS: $ref (no such fixture under $FIXTURES_DIR/)" >&2
        fail=1
      fi
      ;;
    TODO\(R-*\))
      # ref looks like `TODO(R-7.1-04)`
      if ! rg --quiet --fixed-strings "$ref" "$SRC_DIR" 2>/dev/null; then
        echo "MISS: $ref (not found in $SRC_DIR)" >&2
        fail=1
      fi
      ;;
    *::*)
      # Rust path like `astar_iax_core::message::build_new`. Verify the last segment exists.
      local fn="${ref##*::}"
      fn="${fn//\`/}"
      fn="${fn// /}"
      if ! rg --quiet "fn\s+${fn}\b|struct\s+${fn}\b|enum\s+${fn}\b" "$SRC_DIR" 2>/dev/null; then
        echo "MISS: $ref (no fn/struct/enum ${fn} found in $SRC_DIR)" >&2
        fail=1
      fi
      ;;
    *)
      local name="${ref//\`/}"
      name="${name// /}"
      if [[ -z "$name" || ! "$name" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
        return
      fi
      if ! rg --quiet "fn\s+${name}\b|struct\s+${name}\b|enum\s+${name}\b" "$SRC_DIR" 2>/dev/null; then
        echo "MISS: $ref (no fn/struct/enum ${name} found in $SRC_DIR)" >&2
        fail=1
      fi
      ;;
  esac
}

for cell in "${evidence_cells[@]}"; do
  IFS=',' read -ra refs <<< "$cell"
  for raw in "${refs[@]}"; do
    ref="$(echo "$raw" | sed -E 's/^[[:space:]]*`?([^`]*)`?[[:space:]]*$/\1/')"
    [[ -z "$ref" ]] && continue
    check_ref "$ref"
  done
done

if (( fail )); then
  echo "verify-rfc-audit: one or more citations did not resolve" >&2
  exit 1
fi

echo "verify-rfc-audit: ok"
