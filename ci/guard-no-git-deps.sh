#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Merge invariant: this repo has NO git dependencies.
#
# The AMBE software vocoder (ambe-dstar / ambe-core) was dropped; the ThumbDV
# driver survives as a vendored path crate at vendor/ambe-thumbdv. That is what
# makes `cargo build --locked --offline` work and what keeps the build from
# reaching out to an external repo mid-CI. If a `git = "..."` dependency ever
# comes back, the offline/locked guarantee dies quietly — so fail loudly here.
#
# Note the deliberate distinction: `repository = "https://github.com/..."` in
# vendor/ambe-thumbdv/Cargo.toml is upstream ATTRIBUTION and must stay. Only
# dependency sources are forbidden, which is why the authoritative check is
# `cargo metadata`'s resolved source list rather than a grep for a URL.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

rc=0

# 1. Authoritative: no resolved package may come from a git source.
meta="$(cargo metadata --format-version 1 --locked --offline)"
if printf '%s' "$meta" | grep -q '"git+'; then
  echo "FAIL: cargo metadata resolves at least one package from a git source:" >&2
  printf '%s' "$meta" | tr ',' '\n' | grep '"git+' | sort -u >&2
  rc=1
fi

# 2. Belt and braces: no manifest may declare a git dependency at all.
#    `repository = "https://..."` (attribution) does not match this pattern.
if git grep -nE '(^|[[:space:],{])git[[:space:]]*=[[:space:]]*"' -- '*Cargo.toml'; then
  echo "FAIL: a Cargo.toml above declares a git dependency." >&2
  rc=1
fi

# 3. The dropped software vocoder must not reappear as a dependency.
if git grep -nE '^[[:space:]]*(ambe-dstar|ambe-core)[[:space:]]*=' -- '*Cargo.toml'; then
  echo "FAIL: ambe-dstar / ambe-core are the SOFTWARE vocoder and were removed." >&2
  echo "      D-Star in this repo is ThumbDV-only (vendor/ambe-thumbdv)." >&2
  rc=1
fi

# 4. ...but the ThumbDV driver itself must still be here. Deleting it breaks
#    D-Star entirely, so guard its presence, not just its absence-of-git.
if [ ! -f vendor/ambe-thumbdv/Cargo.toml ]; then
  echo "FAIL: vendor/ambe-thumbdv is missing — that crate IS the ThumbDV driver." >&2
  rc=1
fi
for lic in LICENSE-MIT LICENSE-APACHE; do
  if [ ! -f "vendor/ambe-thumbdv/$lic" ]; then
    echo "FAIL: vendor/ambe-thumbdv/$lic is missing (upstream is MIT OR Apache-2.0)." >&2
    rc=1
  fi
done

[ "$rc" -eq 0 ] && echo "no-git-deps: clean (fully vendored; --locked --offline holds)"
exit "$rc"
