#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# link-builds.sh — keeps this checkout's build output on another drive.
#
#     ci/link-builds.sh <root>          e.g. ci/link-builds.sh /Volumes/data/builds
#     ASTAR_BUILD_ROOT=<root> ci/link-builds.sh
#     just link-builds <root>
#
# A checkout's two big build folders are `target/` (Rust, 5-35 GB) and
# `apps/macos/build/` (Xcode, about 1 GB). This moves both to
#
#     <root>/astar/<checkout>/target
#     <root>/astar/<checkout>/macos-build
#
# and leaves a symlink in the old place, so cargo, xcodebuild and every
# script see the same paths as before. <checkout> is `main` for the main
# checkout and the folder name for a worktree (`.worktrees/ble-ptt` is
# `ble-ptt`). Run it once per checkout, and again any time: a folder already
# linked is left as it is, and one that exists is moved, not rebuilt.
#
# `ci/clean-builds.sh` knows about the links: removing a merged worktree also
# removes its folder under <root>, and folders whose worktree is gone are
# swept up. If <root>'s drive is not mounted, builds fail on the dangling link
# rather than quietly filling the main disk.
#
# Bash 3.2 (what macOS ships).
set -euo pipefail

ROOT_DIR="${ASTAR_CHECKOUT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT_DIR"

BUILD_ROOT="${1:-${ASTAR_BUILD_ROOT:-}}"
if [ -z "$BUILD_ROOT" ]; then
  echo "usage: ci/link-builds.sh <root>   (or set ASTAR_BUILD_ROOT)" >&2
  exit 1
fi
[ -d "$BUILD_ROOT" ] || mkdir -p "$BUILD_ROOT" || {
  echo "FAIL: cannot create $BUILD_ROOT (is its drive mounted?)" >&2
  exit 1
}
BUILD_ROOT="$(cd "$BUILD_ROOT" && pwd)"

# The main checkout has a .git directory; a worktree has a .git file.
if [ -d .git ]; then name="main"; else name="$(basename "$ROOT_DIR")"; fi
dest_dir="$BUILD_ROOT/astar/$name"
mkdir -p "$dest_dir"

link() { # path-in-checkout, folder-name-under-dest
  local here="$1" there="$dest_dir/$2"
  if [ -L "$here" ]; then
    echo "   linked already: $here -> $(readlink "$here")"
    return
  fi
  mkdir -p "$there" "$(dirname "$here")"
  if [ -d "$here" ]; then
    rsync -a "$here/" "$there/"
    rm -rf "$here"
    echo "   moved   $here -> $there"
  else
    echo "   linked  $here -> $there"
  fi
  ln -s "$there" "$here"
}

echo "== build output for $name goes to $dest_dir"
link target target
link apps/macos/build macos-build
