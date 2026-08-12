#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Regenerate the astar-sys C header and fail if it drifts from the committed
# copy (crates/astar-sys/include/astar.h). Run after changing any
# extern "C" fn / #[repr(C)] type / error-code const in astar-sys.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="$ROOT/crates/astar-sys/cbindgen.toml"
HEADER="$ROOT/crates/astar-sys/include/astar.h"

GEN="$(mktemp)"
trap 'rm -f "$GEN"' EXIT

cbindgen --config "$CONFIG" --crate astar-sys --output "$GEN" --quiet

# Secret-free guard (defense-in-depth; the Rust out-structs carry no string
# fields by construction). The `secret`/`portal_pass` fields are permitted ONLY
# inside IaxConfig (in-params the caller passes; never echoed). They must not
# appear in the out-structs IaxState or IaxEvent, and `password`/`token` must
# not appear as a field name anywhere. We extract each out-struct body and
# assert it is free of those names.
out_struct_body() { # $1 = type name; prints the `{ ... }` body if present
  awk -v t="$1" '
    /^typedef struct \{/ { buf=$0"\n"; inb=1; next }
    inb { buf=buf $0"\n"; if ($0 ~ "\\} "t";") { print buf; inb=0 } }
  ' "$GEN"
}
for ty in IaxState IaxEvent; do
  body="$(out_struct_body "$ty")"
  if printf '%s' "$body" | grep -Eiq '\b(secret|pass|password|token)\b'; then
    echo "ERROR: out-struct $ty appears to carry a secret/password/token field." >&2
    printf '%s\n' "$body" >&2
    exit 1
  fi
done
if grep -Eq '\b(password|token)[A-Za-z0-9_]*\s*;' "$GEN"; then
  echo "ERROR: a password/token field leaked into the C header surface." >&2
  grep -En '\b(password|token)[A-Za-z0-9_]*\s*;' "$GEN" >&2
  exit 1
fi

if ! diff -u "$HEADER" "$GEN"; then
  echo "" >&2
  echo "astar.h is out of date. Regenerate with:" >&2
  echo "  cbindgen --config $CONFIG --crate astar-sys --output $HEADER" >&2
  exit 1
fi

echo "cbindgen header up to date."
