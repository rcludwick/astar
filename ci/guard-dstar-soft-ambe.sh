#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Merge invariant for the software D-Star vocoder (astar-dstar-perens).
#
# vendor/ham-digital-modes-ambe is Bruce Perens' AMBE 3600x2400 vocoder,
# LGPL-3.0-or-later. Rob, 2026-11-01: every SHIPPED build carries it (the
# macOS app via build-xcframework.sh, astar-server's `release` set), and the
# operator picks dongle or software for D-Star in config, the dongle being the
# default choice. Each rule below is one careless edit away from vanishing with
# the build still green:
#
#   1. The cargo feature stays EXPLICIT, never a cargo default: a plain
#      `cargo build` of any astar crate links no LGPL code (the same contract
#      as codec2-static), and the feature-off refusal stays tested. Shipped
#      builds name it on purpose.
#   2. The opt-in works, and the shipped recipes do turn it on: astar-server's
#      and astar-netrec's `release` features pull it, and
#      build-xcframework.sh enables it unless ASTAR_DSTAR_SOFT_AMBE=0.
#   3. It stays D-Star only. AMBE+2 (DMR, NXDN, System Fusion) reads on
#      US 8,359,197 until 2028-05-20: the vendored copy must contain no
#      ambe_plus_2 source and declare no ambe_plus_2 feature, and the engine's
#      permission check must name D-Star alone.
#   4. Its licence is hard-coded, never inherited from the AGPL workspace.
#
# Checks 1 and 2 read `cargo tree`'s resolved graph (normal edges only), not
# Cargo.toml text, so feature unification through any path is caught.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VENDOR=vendor/ham-digital-modes-ambe
DEFAULT_CLEAN=(astar-codec astar-console astar-station astar-sys astar-server astar-netrec astar-cli astar-gui)

fail=0
for crate in "${DEFAULT_CLEAN[@]}"; do
  if ! output=$(cargo tree -p "$crate" -e normal --prefix none 2>&1); then
    echo "FAIL: 'cargo tree -p $crate' errored — crate name wrong or missing?" >&2
    echo "$output" >&2
    fail=1
    continue
  fi
  if grep -qE '^ham-digital-modes-ambe v' <<<"$output"; then
    echo "FAIL: a plain cargo build of $crate pulls in the software D-Star vocoder (LGPL)." >&2
    echo "      dstar-soft-ambe stays an explicit cargo feature that shipped builds name;" >&2
    echo "      see crates/astar-codec/Cargo.toml and LICENSE-EXCEPTIONS.md section 9." >&2
    fail=1
  fi
done

for crate in astar-codec astar-console astar-station astar-sys astar-server astar-netrec; do
  if ! cargo tree -p "$crate" -e normal --prefix none --features dstar-soft-ambe 2>/dev/null |
    grep -qE '^ham-digital-modes-ambe v'; then
    echo "FAIL: $crate --features dstar-soft-ambe does NOT pull in the software vocoder." >&2
    fail=1
  fi
done

# The shipped builds include it.
if ! cargo tree -p astar-server -e normal --prefix none --no-default-features --features release 2>/dev/null |
  grep -qE '^ham-digital-modes-ambe v'; then
  echo "FAIL: astar-server's release feature set does not include dstar-soft-ambe." >&2
  fail=1
fi
if ! cargo tree -p astar-netrec -e normal --prefix none --features release 2>/dev/null |
  grep -qE '^ham-digital-modes-ambe v'; then
  echo "FAIL: astar-netrec's release feature set does not include dstar-soft-ambe." >&2
  fail=1
fi
if ! grep -qE 'ASTAR_DSTAR_SOFT_AMBE:-1\}' bindings/swift/build-xcframework.sh ||
  ! grep -qE 'features dstar-soft-ambe' bindings/swift/build-xcframework.sh; then
  echo "FAIL: bindings/swift/build-xcframework.sh no longer builds the software D-Star" >&2
  echo "      vocoder in by default (ASTAR_DSTAR_SOFT_AMBE defaults to 1)." >&2
  fail=1
fi

# D-Star only, in the engine as well as in the vendored tree.
if ! grep -qE 'matches!\(mode, VocoderMode::Dstar\)' crates/astar-codec/src/ambe.rs; then
  echo "FAIL: software_backend_permitted in crates/astar-codec/src/ambe.rs must permit" >&2
  echo "      VocoderMode::Dstar and nothing else." >&2
  fail=1
fi

if [ -n "$(find "$VENDOR" -iname '*ambe_plus_2*' -print -quit)" ]; then
  echo "FAIL: $VENDOR contains AMBE+2 code:" >&2
  find "$VENDOR" -iname '*ambe_plus_2*' >&2
  echo "      AMBE+2 is hardware-only (US 8,359,197 until 2028-05-20). See $VENDOR/VENDORED.md." >&2
  fail=1
fi
if grep -nE '^[[:space:]]*ambe_plus_2[[:space:]]*=' "$VENDOR/Cargo.toml" >&2; then
  echo "FAIL: $VENDOR/Cargo.toml declares an ambe_plus_2 feature." >&2
  fail=1
fi
if ! grep -qE '^license = "LGPL-3.0-or-later"$' "$VENDOR/Cargo.toml"; then
  echo "FAIL: $VENDOR/Cargo.toml must hard-code license = \"LGPL-3.0-or-later\"." >&2
  fail=1
fi
if grep -vE '^[[:space:]]*#' "$VENDOR/Cargo.toml" | grep -nE 'workspace[[:space:]]*=[[:space:]]*true' >&2; then
  echo "FAIL: $VENDOR/Cargo.toml inherits workspace metadata; that would relicense it as AGPL." >&2
  fail=1
fi

[ "$fail" -eq 0 ] || exit 1
echo "dstar-soft-ambe guard: explicit cargo feature, shipped builds carry it, D-Star only, no AMBE+2 vendored, licence not inherited."
