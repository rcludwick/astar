#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# The feature matrix: builds the feature combinations `just ci` never sees.
#
#   ci/feature-matrix.sh            run everything
#   ci/feature-matrix.sh --list     print the steps, run nothing
#   ci/feature-matrix.sh <regex>    run only the steps whose name matches
#
# Why it exists. `just ci` builds the workspace once, and Cargo unifies
# features across a workspace build: if any member turns a feature on, every
# crate is compiled with it. So a crate whose own Cargo.toml forgets to ask
# for a feature it needs still compiles there, because a neighbour asked for
# it. Built alone, it does not. Four breaks of exactly that shape sat unseen
# until 2026-11-01 (backlog `astar-feature-matrix`): astar-cli's listen
# features calling `Station::new` without `astar-station/device-audio`,
# astar-console's `serde` not reaching `astar-audio/serde`, astar-server's
# WebSocket fixtures assuming `stream`, and clippy lints in code only the
# opt-in features compile.
#
# What it runs, every step per crate (`-p`), so nothing is unified away:
#
#   1. all-features   clippy --all-targets --all-features, every crate below
#                     that has features.
#   2. each-feature   clippy --all-targets --no-default-features, then once
#                     more per feature with that feature alone. The same set
#                     `cargo hack --each-feature` would build; cargo-hack is
#                     not assumed to be installed, so the features are read
#                     from `cargo metadata` instead. A feature added to a
#                     Cargo.toml joins the matrix with no edit here.
#   3. tests          full `cargo test` for the crates the four breaks came
#                     from (astar-cli, astar-console and astar-server with
#                     --all-features) and for astar-server's two builds
#                     without `stream`: bare, and with `auth`.
#   4. release        astar-server --no-default-features --features release,
#                     clippy and its tests: the native-host twin of the
#                     static musl binary `just server-release` cross-builds.
#
# Clippy rather than `cargo test` for steps 1 and 2: it type-checks every
# target, tests included, at check speed, and `-D warnings` catches the lints
# that only feature-gated code trips. Running every suite under every feature
# would take hours on the 4-core runner for little more coverage.
#
# Exclusions, each on purpose:
#
#   vendor/*          ambe-thumbdv, mp3lame-sys and mp3lame-encoder are
#                     upstream code kept as published (see each VENDORED.md).
#                     Their features (`decoder`, `target_host`, `std`) are
#                     upstream's, astar never turns them on, and a lint
#                     there is not ours to fix. They build as dependencies
#                     wherever astar's own features pull them in.
#   astar-gui         has no features; `just ci` already builds it.
#   crates with no    nothing to vary. `just ci` builds them.
#   [features]
#   `default`         is the build `just ci` already makes.
#   `release` alone   astar-server's release set gets its own step (4), with
#                     tests, rather than a second clippy in step 2.
#   hardware suites   `ambe-hw` (and every feature that pulls it: `dstar`,
#                     `ysf`, `nxdn`, `dmr`) compiles here and its tests run,
#                     but the ThumbDV tests inside skip unless
#                     IAX_THUMBDV_TESTS=1, and the BLE tests unless
#                     IAX_BLE_TESTS is set. This script refuses to start if
#                     either is set, so no step can open a serial port, a
#                     dongle or a Bluetooth device.
#
# No platform-only feature exists today (nothing needs a macOS framework or a
# Windows API to build), so the same matrix runs on the Mac and on the Debian
# runner. A feature that cannot build on one OS belongs in the exclusions
# above, with its reason, not silently dropped.
#
# Every step runs even after one fails; the summary lists the failures and
# the exit status is non-zero if there were any.
set -uo pipefail
cd "$(dirname "$0")/.."

# The whole script is one function, read in full before any of it runs, so
# editing this file while a long run is going cannot change what that run
# executes.
main() {

mode=run
filter=""
case "${1:-}" in
  --list) mode=list ;;
  -h|--help) sed -n '5,10p' "$0"; exit 0 ;;
  "") ;;
  *) filter="$1" ;;
esac

if [ "$mode" = run ]; then
  ./ci/guard-safety-env.sh >/dev/null || exit 1
  for var in IAX_BLE_TESTS; do
    if [ -n "${!var-}" ]; then
      echo "REFUSING TO RUN: $var is set; it arms the Bluetooth hardware tests." >&2
      exit 1
    fi
  done
fi

# Crates excluded from steps 1 and 2 (see the header).
EXCLUDED_CRATES="ambe-thumbdv mp3lame-sys mp3lame-encoder astar-gui"
# Features never built alone in step 2 (see the header), as crate:feature.
EXCLUDED_FEATURES="astar-server:release"

# "<crate> <feature> <feature> ..." for every workspace member with features,
# in workspace order.
feature_table() {
  cargo metadata --no-deps --format-version 1 | python3 -I -c '
import json, sys
skip = set(sys.argv[1].split())
for p in json.load(sys.stdin)["packages"]:
    feats = sorted(f for f in p["features"] if f != "default")
    if p["name"] not in skip and feats:
        print(" ".join([p["name"], *feats]))
' "$EXCLUDED_CRATES"
}

names=()
cmds=()
add() {
  local name="$1"; shift
  names+=("$name")
  cmds+=("$*")
}

clippy() { echo "cargo clippy $* --all-targets -- -D warnings"; }

table=$(feature_table) || { echo "FAIL: cargo metadata"; exit 1; }

# 1. --all-features per crate.
while read -r crate _; do
  add "all-features $crate" "$(clippy -p "$crate" --all-features)"
done <<<"$table"

# 2. No default features, then each feature alone.
while read -r crate feats; do
  add "each-feature $crate (none)" "$(clippy -p "$crate" --no-default-features)"
  for f in $feats; do
    case " $EXCLUDED_FEATURES " in *" $crate:$f "*) continue ;; esac
    add "each-feature $crate $f" "$(clippy -p "$crate" --no-default-features --features "$f")"
  done
done <<<"$table"

# 3. Full test runs where the 2026-11-01 breaks were.
add "tests astar-cli --all-features"     "cargo test -p astar-cli --all-features"
add "tests astar-console --all-features" "cargo test -p astar-console --all-features"
add "tests astar-server --all-features"  "cargo test -p astar-server --all-features"
add "tests astar-server (no stream)"     "cargo test -p astar-server --no-default-features"
add "tests astar-server auth (no stream)" "cargo test -p astar-server --no-default-features --features auth"

# 4. The release feature set.
add "release astar-server clippy" "$(clippy -p astar-server --no-default-features --features release)"
add "release astar-server tests"  "cargo test -p astar-server --no-default-features --features release"

if [ "$mode" = list ]; then
  for i in "${!names[@]}"; do
    printf '%-48s %s\n' "${names[$i]}" "${cmds[$i]}"
  done
  echo "${#names[@]} steps"
  exit 0
fi

failed=()
ran=0
start_all=$SECONDS
for i in "${!names[@]}"; do
  name="${names[$i]}"
  if [ -n "$filter" ] && ! grep -Eq -- "$filter" <<<"$name"; then
    continue
  fi
  ran=$((ran + 1))
  echo "==> $name"
  echo "    ${cmds[$i]}"
  t0=$SECONDS
  if bash -c "${cmds[$i]}"; then
    echo "ok   $name ($((SECONDS - t0)) s)"
  else
    echo "FAIL $name ($((SECONDS - t0)) s)"
    failed+=("$name")
  fi
done

elapsed=$((SECONDS - start_all))
echo
if [ "${#failed[@]}" -gt 0 ]; then
  echo "feature matrix: ${#failed[@]} of $ran steps FAILED in ${elapsed} s:"
  printf '  %s\n' "${failed[@]}"
  exit 1
fi
echo "feature matrix: all $ran steps passed in ${elapsed} s"
}

main "$@"
