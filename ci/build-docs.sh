#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Build the Zensical documentation site.
#
# Zensical is the static site generator from the Material for MkDocs team; the
# config is ./zensical.toml at the repo root (TOML — NOT mkdocs.yml). It sets
# docs_dir = docs/site (the published pages) and site_dir = docs/.site (the
# build output, gitignored); this script reads site_dir back out of the config
# rather than hard-coding it, so a layout change there cannot silently make CI
# verify the wrong directory.
#
# `--strict` aborts on warnings, so a broken link or a nav entry pointing at a
# missing page reddens the pipeline instead of shipping a half-built site.
#
# This is the single build entry point shared by the GitLab `docs-site` job
# (which builds on every push) and the dormant GitHub Pages workflow (which
# uploads this output as the Pages artifact) — the verified site and the
# published site are therefore built by identical code.
#
# Toolchain: uses `uvx zensical` when uv is installed (fast, no venv to manage,
# matches the local recipes), otherwise falls back to a throwaway virtualenv
# and `pip install zensical`. Either way it needs network access once.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CONFIG="$ROOT/zensical.toml"
if [ ! -f "$CONFIG" ]; then
  echo "FAIL: no zensical.toml at the repo root ($CONFIG)." >&2
  echo "      If the docs site moved, point ci/build-docs.sh at its new home." >&2
  exit 1
fi

# site_dir straight from the config; fall back to Zensical's own default.
SITE_DIR="$(python3 - "$CONFIG" <<'PY'
import sys
try:
    import tomllib
    with open(sys.argv[1], "rb") as fh:
        print(tomllib.load(fh).get("project", {}).get("site_dir", "site"))
except Exception:
    print("site")
PY
)"
OUT="$ROOT/$SITE_DIR"

if command -v uvx >/dev/null 2>&1; then
  uvx zensical build --clean --strict
else
  # Built outside the working tree on purpose: nothing to add to .gitignore,
  # and `git clean` between jobs can never half-delete it.
  VENV="${DOCS_VENV:-${TMPDIR:-/tmp}/astar-docs-venv}"
  rm -rf "$VENV"
  python3 -m venv "$VENV"
  # shellcheck disable=SC1091
  . "$VENV/bin/activate"
  python3 -m pip install --quiet --upgrade pip
  python3 -m pip install --quiet zensical
  zensical build --clean --strict
fi

if [ ! -f "$OUT/index.html" ]; then
  echo "FAIL: zensical produced no $SITE_DIR/index.html" >&2
  exit 1
fi

echo "docs: built $(find "$OUT" -type f | wc -l | tr -d ' ') files into $SITE_DIR"
