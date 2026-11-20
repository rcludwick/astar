#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Keeps SSTV in astar receive-only, and astar-sstv a leaf.
#
# 1. The `sstv` crate, which can encode (transmit) SSTV, is only ever a
#    dev-dependency: any Cargo.toml naming it outside a `[dev-dependencies]`
#    table fails, whether as a key, a `[...dependencies.sstv]` table or a
#    renamed `package = "sstv"`.
# 2. crates/astar-sstv's normal dependencies (`[dependencies]`, and any
#    target-specific or build ones) name no astar-* crate except
#    astar-record, so the decoder never reaches anything that could key a
#    radio.
# 3. slowrx's headerless starts stay off: neither of its two setters for them
#    appears anywhere under crates/, so a picture starts only on a VIS header.
#
# SSTV_ROOT points the guard at another tree; `--self-test` builds temporary
# trees with each violation and checks the guard catches every one.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

if [ "${1:-}" = "--self-test" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  n=0
  failures=0

  # A clean tree: astar-sstv with its allowed deps, a second crate using sstv
  # as a dev-dependency.
  clean() {
    n=$((n + 1))
    local t="$tmp/t$n"
    mkdir -p "$t/crates/astar-sstv/src" "$t/crates/astar-netrec/src"
    cat > "$t/crates/astar-sstv/Cargo.toml" <<'TOML'
[package]
name = "astar-sstv"

[dependencies]
slowrx = { workspace = true }
astar-record = { path = "../astar-record" }

[dev-dependencies]
sstv = "=0.3.1"
astar-station = { path = "../astar-station" }
TOML
    cat > "$t/crates/astar-netrec/Cargo.toml" <<'TOML'
[package]
name = "astar-netrec"

[dependencies]
astar-sstv = { path = "../astar-sstv" }

[target.'cfg(unix)'.dev-dependencies]
sstv = "=0.3.1"
TOML
    echo 'fn main() {}' > "$t/crates/astar-sstv/src/lib.rs"
    printf '[workspace]\n' > "$t/Cargo.toml"
    echo "$t"
  }

  expect() {
    [ -n "${SSTV_SELFTEST_VERBOSE:-}" ] && echo "== $2"
    local want="$1" name="$2" tree="$3" got=0
    SSTV_ROOT="$tree" "$here/guard-sstv.sh" > "$tree.out" 2>&1 || got=1
    [ -n "${SSTV_SELFTEST_VERBOSE:-}" ] && cat "$tree.out"
    if [ "$want" = pass ] && [ "$got" -ne 0 ]; then
      echo "self-test FAIL: $name should pass:"; cat "$tree.out"; failures=$((failures + 1))
    elif [ "$want" = fail ] && [ "$got" -eq 0 ]; then
      echo "self-test FAIL: $name should fail"; failures=$((failures + 1))
    fi
  }

  t=$(clean); expect pass "a clean tree" "$t"

  t=$(clean)
  printf '\n[dependencies]\nsstv = "=0.3.1"\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect fail "sstv as a normal dependency" "$t"

  t=$(clean)
  printf '\n[features]\nsstv = ["dep:astar-sstv"]\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect pass "a feature named sstv" "$t"

  t=$(clean)
  printf '\n[workspace.dependencies]\nsstv = "0.3"\n' >> "$t/Cargo.toml"
  expect fail "sstv in workspace.dependencies" "$t"

  t=$(clean)
  printf '\n[build-dependencies.sstv]\nversion = "0.3"\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect fail "sstv as a build-dependency table" "$t"

  t=$(clean)
  printf '\n[dependencies]\nenc = { package = "sstv", version = "0.3" }\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect fail "sstv renamed" "$t"

  t=$(clean)
  sed -i.bak 's|^slowrx = .*|&\nastar-station = { path = "../astar-station" }|' "$t/crates/astar-sstv/Cargo.toml"
  expect fail "astar-sstv depending on astar-station" "$t"

  t=$(clean)
  printf '\n[target.'"'"'cfg(unix)'"'"'.dependencies]\nastar-ptt = { path = "../astar-ptt" }\n' >> "$t/crates/astar-sstv/Cargo.toml"
  expect fail "astar-sstv depending on astar-ptt for one target" "$t"

  t=$(clean)
  printf '\n[dependencies]\nsstv.workspace = true\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect fail "sstv with a dotted key" "$t"

  t=$(clean)
  printf '\n[dependencies]\nsstv.version = "0.3"\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect fail "sstv with a dotted version key" "$t"

  t=$(clean)
  printf '\n[dependencies]\n"sstv" = "0.3"\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect fail "sstv with a quoted key" "$t"

  t=$(clean)
  sed -i.bak 's|^slowrx = .*|&\n"astar-station" = { path = "../astar-station" }|' "$t/crates/astar-sstv/Cargo.toml"
  expect fail "astar-sstv depending on a quoted astar-station key" "$t"

  t=$(clean)
  sed -i.bak 's|^slowrx = .*|&\nastar-ptt.workspace = true|' "$t/crates/astar-sstv/Cargo.toml"
  expect fail "astar-sstv depending on astar-ptt with a dotted key" "$t"

  t=$(clean)
  echo "fn f(d: &mut D) { d.set_forced_mode(None); }" >> "$t/crates/astar-sstv/src/lib.rs"
  expect fail "set_forced_mode called" "$t"

  t=$(clean)
  mkdir -p "$t/crates/astar-station/src"
  echo "// d.set_fallback_mode(Some(m))" > "$t/crates/astar-station/src/sstv.rs"
  expect fail "set_fallback_mode under another crate" "$t"

  if [ "$failures" -ne 0 ]; then
    echo "FAIL: sstv guard self-test ($failures)"
    exit 1
  fi
  echo "sstv guard self-test: ok"
  exit 0
fi

root="${SSTV_ROOT:-$(cd "$here/.." && pwd)}"
cd "$root"
fail=0

manifests=$(find . \( -name target -o -name .worktrees -o -name .git -o -name node_modules \) -prune \
  -o -name Cargo.toml -type f -print | sort)

# Rule 1. Walk each manifest, tracking the current table.
for m in $manifests; do
  hits=$(awk '
    /^[[:space:]]*\[/ {
      table = $0
      gsub(/[[:space:]]/, "", table)
      if (table ~ /dependencies\.sstv\]$/ && table !~ /dev-dependencies\.sstv\]$/) print FILENAME ":" NR ":" $0
      next
    }
    {
      line = $0; sub(/#.*/, "", line)
      named = (line ~ /^[[:space:]]*["\047]?sstv["\047]?[[:space:]]*(\.|=)/ || line ~ /package[[:space:]]*=[[:space:]]*"sstv"/)
      # A key under [features] names a feature, not a dependency (the
      # astar-station feature called sstv turns on the decoder). A feature
      # can only reach a crate declared in a dependency table, read above.
      if (named && table !~ /dev-dependencies\]$/ && table != "[features]") print FILENAME ":" NR ":" $0
    }
  ' "$m")
  if [ -n "$hits" ]; then
    echo "FAIL: the sstv crate (an SSTV encoder) may only be a dev-dependency:"
    echo "$hits"
    fail=1
  fi
done

# Rule 2.
sstv_manifest="crates/astar-sstv/Cargo.toml"
if [ ! -f "$sstv_manifest" ]; then
  echo "FAIL: $sstv_manifest is missing, so the guard cannot run"
  exit 1
fi
hits=$(awk '
  /^[[:space:]]*\[/ {
    table = $0; gsub(/[[:space:]]/, "", table)
    normal = (table ~ /dependencies(\..*)?\]$/ && table !~ /dev-dependencies/)
    if (normal && table ~ /dependencies\.astar-/ && table !~ /dependencies\.astar-record\]$/) print FILENAME ":" NR ":" $0
    next
  }
  normal {
    line = $0; sub(/#.*/, "", line)
    if (line ~ /^[[:space:]]*["\047]?astar-[A-Za-z0-9_-]*["\047]?[[:space:]]*(\.|=)/ && line !~ /^[[:space:]]*["\047]?astar-record["\047]?[[:space:]]*(\.|=)/) print FILENAME ":" NR ":" $0
    if (line ~ /package[[:space:]]*=[[:space:]]*"astar-/ && line !~ /package[[:space:]]*=[[:space:]]*"astar-record"/) print FILENAME ":" NR ":" $0
  }
' "$sstv_manifest")
if [ -n "$hits" ]; then
  echo "FAIL: astar-sstv may depend on no astar crate but astar-record:"
  echo "$hits"
  fail=1
fi

# Rule 3. The names are split so this script does not match itself if it is
# ever scanned.
fallback="set_fallback""_mode"
forced="set_forced""_mode"
if [ -d crates ]; then
  status=0
  hits=$(grep -rnE --include='*.rs' -e "($fallback|$forced)" crates) || status=$?
  if [ "$status" -ge 2 ]; then
    echo "FAIL: the scan of crates/ could not run"
    exit 1
  fi
  if [ -n "$hits" ]; then
    echo "FAIL: slowrx's headerless starts must stay off; a picture starts only on a VIS header:"
    echo "$hits"
    fail=1
  fi
fi

# Rule 4. The grep rules above are a fast first pass; this one cannot be
# spelled around. No normal dependency tree of astar-sstv, or of any crate
# that depends on it, may contain the encoder crate. Skipped for the
# self-test trees (SSTV_ROOT) and when cargo is missing.
if [ -z "${SSTV_ROOT:-}" ] && command -v cargo >/dev/null 2>&1 && [ -f Cargo.lock ]; then
  users=$(cargo metadata --offline --locked --format-version 1 --no-deps 2>/dev/null \
    | python3 -I -c '
import json, sys
m = json.load(sys.stdin)
for p in m["packages"]:
    if p["name"] == "astar-sstv" or any(
        d["name"] == "astar-sstv" and d["kind"] is None for d in p["dependencies"]
    ):
        print(p["name"])
') || { echo "FAIL: cargo metadata could not run"; exit 1; }
  for crate in $users; do
    # --all-features: a crate may carry astar-sstv behind a feature
    # (astar-station's `sstv`), and the tree must be checked with it on.
    if tree=$(cargo tree -e normal --offline --locked --all-features -p "$crate" 2>&1); then
      if printf '%s\n' "$tree" | grep -Eq '(^|[^A-Za-z0-9_-])sstv v[0-9]'; then
        echo "FAIL: $crate's normal dependency tree contains the sstv encoder crate"
        fail=1
      fi
    else
      echo "FAIL: cargo tree could not run for $crate:"
      echo "$tree"
      fail=1
    fi
  done
fi

if [ "$fail" -ne 0 ]; then
  echo "FAIL: sstv guard"
  exit 1
fi
echo "sstv guard: ok"
