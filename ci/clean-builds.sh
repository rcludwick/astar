#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# clean-builds.sh — removes the build output a published release no longer
# needs, in this checkout.
#
#     ci/clean-builds.sh [--dry-run] [--after-publish]
#     just clean-builds [--dry-run]
#
# `just publish` runs this with --after-publish once GitHub confirms the
# release is live. It is also safe to run by hand at any time.
#
# Every one of these is rebuilt on demand, so removing it costs time, never
# data. A Rust `target/debug` piles up a copy of every crate for every
# compiler and branch it has seen (thousands of copies of astar_server
# alone, and 20-30 GB after a release's gates), and nothing ever prunes it.
#
#   target/debug, target/release           host builds; the next `just ci`
#                                          rebuilds them
#   target/x86_64-*                        Intel and Linux cross builds
#   target/flycheck*                       an editor's background checks
#   target/<slice>/debug/incremental       the xcframework slices' incremental
#                                          caches (the slices themselves stay,
#                                          so `just xcframework` stays quick)
#   apps/macos/build/.../Release           the Release app the DMG was made from
#   apps/macos/build/astar-*.dmg           DMGs of older, published releases
#   apps/macos/build/astar.dmg             only with --after-publish: by then
#                                          it is attached to the GitHub release
#   .worktrees/<item>                      only when clean AND its branch is
#                                          merged into main; the branch goes too,
#                                          and so do its build folders on another
#                                          drive (ci/link-builds.sh)
#   <root>/astar/<item>                    linked build folders whose worktree
#                                          no longer exists
#
# Never touched: the Debug app (apps/macos/build/.../Debug, which is what
# `just run` launches and may be running right now), the xcframeworks,
# target/server-release (the static server artefact), and any worktree that is
# dirty or unmerged.
#
# The Rust and Xcode output is skipped, with a message, while cargo, rustc or
# xcodebuild is running, so a build in progress is never pulled out from
# under. ASTAR_CLEAN_IGNORE_BUSY=1 skips that check (the tests use it).
#
# Bash 3.2 (what macOS ships): no associative arrays, no `mapfile`.
set -euo pipefail

ROOT="${ASTAR_RELEASE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

DRY_RUN=0
AFTER_PUBLISH=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --after-publish) AFTER_PUBLISH=1 ;;
    -h | --help)
      sed -n '6,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "FAIL: unknown option: $1" >&2
      exit 1
      ;;
  esac
  shift
done

verb="removed"
[ "$DRY_RUN" = 1 ] && verb="would remove"

size_of() { du -sh "$1" 2>/dev/null | cut -f1; }

remove() { # path
  [ -e "$1" ] || return 0
  printf '   %s %-50s %s\n' "$verb" "$1" "$(size_of "$1")"
  [ "$DRY_RUN" = 1 ] || rm -rf "$1"
}

echo "== cleaning build output in $ROOT"

busy=""
if [ "${ASTAR_CLEAN_IGNORE_BUSY:-0}" != 1 ]; then
  for p in cargo rustc xcodebuild; do
    if pgrep -x "$p" >/dev/null 2>&1; then busy="$busy $p"; fi
  done
fi

if [ -n "$busy" ]; then
  echo "   skipping target/ and apps/macos/build: running now:$busy"
else
  remove target/debug
  remove target/release
  for d in target/x86_64-* target/flycheck*; do
    if [ -e "$d" ]; then remove "$d"; fi
  done
  for d in target/*/debug/incremental target/*/release/incremental; do
    if [ -e "$d" ]; then remove "$d"; fi
  done
  remove apps/macos/build/DD/Build/Products/Release
  for f in apps/macos/build/astar-*.dmg; do
    if [ -e "$f" ]; then remove "$f"; fi
  done
  if [ "$AFTER_PUBLISH" = 1 ]; then
    remove apps/macos/build/astar.dmg
  fi
fi

# Worktrees: only clean ones whose branch main already contains. A dirty or
# unmerged worktree is someone's work in progress, and is left alone.
if git rev-parse --verify -q main >/dev/null && [ -d .worktrees ]; then
  for wt in .worktrees/*/; do
    wt="${wt%/}"
    [ -e "$wt/.git" ] || continue
    branch="$(git -C "$wt" symbolic-ref -q --short HEAD 2>/dev/null || true)"
    [ -n "$branch" ] || continue
    git merge-base --is-ancestor "$branch" main 2>/dev/null || continue
    [ -z "$(git -C "$wt" status --porcelain 2>/dev/null)" ] || {
      echo "   kept   $wt (merged, but has uncommitted changes)"
      continue
    }
    printf '   %s %-50s %s (branch %s, merged)\n' "$verb" "$wt" "$(size_of "$wt")" "$branch"
    # Build folders linked to another drive (ci/link-builds.sh) live outside
    # the worktree, so removing the worktree alone would strand them there.
    for link in "$wt/target" "$wt/apps/macos/build"; do
      if [ -L "$link" ]; then
        printf '   %s %-50s %s (its linked build folder)\n' "$verb" "$(readlink "$link")" "$(size_of "$(readlink "$link")/")"
        [ "$DRY_RUN" = 1 ] || rm -rf "$(readlink "$link")"
      fi
    done
    if [ "$DRY_RUN" != 1 ]; then
      git worktree remove --force "$wt"
      git branch -d "$branch" >/dev/null
    fi
  done
fi

# Linked build folders whose worktree is gone, for instance removed by hand
# with `git worktree remove`. The main checkout's own `target` link says where
# the build root is; without one there is nothing to sweep.
# Only a link shaped exactly <root>/astar/main/target counts; anything else
# could point the sweep at an unrelated folder, so it is left alone.
build_dir=""
if [ -L target ]; then
  t="$(readlink target)"
  case "$t" in
    /*/astar/main/target) build_dir="${t%/main/target}" ;; # <root>/astar
  esac
fi
if [ -n "$build_dir" ] && [ -d "$build_dir" ]; then
  for d in "$build_dir"/*/; do
    d="${d%/}"
    n="$(basename "$d")"
    [ "$n" = main ] && continue
    [ -e ".worktrees/$n" ] && continue
    printf '   %s %-50s %s (its worktree is gone)\n' "$verb" "$d" "$(size_of "$d")"
    [ "$DRY_RUN" = 1 ] || rm -rf "$d"
  done
fi

if [ "$DRY_RUN" = 1 ]; then
  echo "== dry run: nothing was removed."
else
  echo "== clean."
fi
