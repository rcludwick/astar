#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Distribution honesty gate.
#
# astar has never been released. There is no Homebrew tap, no cask, no App
# Store listing, no notarized download, no published binary. The only way to
# get the macOS app today is to build it from this repo (`just app`, or
# `just dmg` for a local, ad-hoc-signed .dmg that is a build artifact and not a
# distribution channel).
#
# It is very easy for a doc edit to quietly invent an install channel that does
# not exist — "brew install --cask astar" reads plausibly and is a lie that a
# user would act on. This guard fails the pipeline on any such claim.
#
# Exactly ONE cask is legitimate: the WCH CH34x USB-serial driver, which the
# UCI150's tty path genuinely requires.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ALLOWED_CASK='wch-ch34x-usb-serial-driver'
rc=0

# 1. Every `brew install --cask` must name the allowed cask.
bad_casks="$(git grep -nI -E 'brew[[:space:]]+install[[:space:]]+--cask' \
             -- . ':(exclude)ci/guard-distribution-claims.sh' \
             | grep -v "$ALLOWED_CASK" || true)"
if [ -n "$bad_casks" ]; then
  echo "FAIL: unverified Homebrew cask reference(s):" >&2
  printf '%s\n' "$bad_casks" >&2
  echo "      The only real cask in this project is $ALLOWED_CASK." >&2
  rc=1
fi

# 2. astar itself is never installed via a package manager.
bad_install="$(git grep -nI -E '(brew|port|apt|dnf|pacman|winget|choco|scoop)[[:space:]]+install[[:space:]]+(--cask[[:space:]]+)?astar([[:space:]]|$)' \
               -- . ':(exclude)ci/guard-distribution-claims.sh' || true)"
if [ -n "$bad_install" ]; then
  echo "FAIL: astar is claimed to be installable from a package manager:" >&2
  printf '%s\n' "$bad_install" >&2
  echo "      Nothing has been released. Build from source is the only path." >&2
  rc=1
fi

# 3. No invented download/release URLs for astar itself.
bad_urls="$(git grep -nI -E 'https?://[^ )"]*astar[^ )"]*\.(dmg|pkg|zip|exe|msi)' \
            -- . ':(exclude)ci/guard-distribution-claims.sh' || true)"
if [ -n "$bad_urls" ]; then
  echo "FAIL: download URL(s) for an astar binary that was never published:" >&2
  printf '%s\n' "$bad_urls" >&2
  rc=1
fi

[ "$rc" -eq 0 ] && echo "distribution-claims: clean (build-from-source is the only claimed path)"
exit "$rc"
