#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# test_clean_builds_sh.sh — tests for ci/clean-builds.sh, against a throwaway
# repo with fake build output in it. Runs as part of `just release-test`.
#
# What is pinned: a dry run removes nothing; a real run removes the stale
# output and keeps the Debug app, the xcframeworks, target/server-release and
# the current astar.dmg; --after-publish also removes astar.dmg; a merged,
# clean worktree goes with its branch, while an unmerged one and a dirty one
# stay.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLEAN_SH="$HERE/clean-builds.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/astar-clean-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
REPO="$TMP/repo"
failures=0

ok() { printf '  ok   %s\n' "$1"; }
bad() {
  printf '  FAIL %s\n' "$1" >&2
  failures=$((failures + 1))
}
exists() { if [ -e "$REPO/$2" ]; then ok "$1"; else bad "$1 ($2 is gone)"; fi; }
gone() { if [ -e "$REPO/$2" ]; then bad "$1 ($2 is still there)"; else ok "$1"; fi; }

git_q() { git -C "$REPO" -c user.name=test -c user.email=test@example.com -c commit.gpgsign=false "$@"; }
clean() { ASTAR_RELEASE_ROOT="$REPO" ASTAR_CLEAN_IGNORE_BUSY=1 "$CLEAN_SH" "$@"; }

fixture() {
  rm -rf "$REPO"
  mkdir -p "$REPO"
  git_q init -q -b main
  cp "$HERE/../.gitignore" "$REPO/.gitignore"
  git_q add .gitignore
  git_q commit -q -m init

  for d in target/debug/deps target/debug/incremental target/release target/x86_64-apple-darwin/debug \
    target/flycheck0 target/aarch64-apple-ios/debug/incremental target/aarch64-apple-ios/debug/deps \
    target/server-release apps/macos/build/DD/Build/Products/Release/astar.app \
    apps/macos/build/DD/Build/Products/Debug/astar.app bindings/swift/astar.xcframework; do
    mkdir -p "$REPO/$d"
    echo x >"$REPO/$d/f"
  done
  echo x >"$REPO/apps/macos/build/astar.dmg"
  echo x >"$REPO/apps/macos/build/astar-0.1.12-beta.dmg"

  # A merged, clean worktree; an unmerged one; a merged but dirty one.
  git_q worktree add -q .worktrees/merged -b work/merged
  git_q worktree add -q .worktrees/unmerged -b work/unmerged
  git -C "$REPO/.worktrees/unmerged" -c user.name=t -c user.email=t@e -c commit.gpgsign=false \
    commit -q --allow-empty -m "not on main"
  git_q worktree add -q .worktrees/dirty -b work/dirty
  echo change >"$REPO/.worktrees/dirty/new-file"
}

echo "== a dry run removes nothing"
fixture
out="$(clean --dry-run --after-publish 2>&1)"
exists "dry run keeps target/debug" target/debug
exists "dry run keeps astar.dmg" apps/macos/build/astar.dmg
exists "dry run keeps the merged worktree" .worktrees/merged
case "$out" in *"would remove target/debug"*) ok "dry run lists target/debug" ;; *) bad "dry run lists target/debug" ;; esac

echo "== a real run"
fixture
clean >/dev/null
gone "removes target/debug" target/debug
gone "removes target/release" target/release
gone "removes the Intel cross build" target/x86_64-apple-darwin
gone "removes the editor's flycheck output" target/flycheck0
gone "removes a slice's incremental cache" target/aarch64-apple-ios/debug/incremental
exists "keeps the slice itself" target/aarch64-apple-ios/debug/deps
exists "keeps the static server artefact" target/server-release
gone "removes the Release app" apps/macos/build/DD/Build/Products/Release
exists "keeps the Debug app" apps/macos/build/DD/Build/Products/Debug/astar.app
exists "keeps the xcframework" bindings/swift/astar.xcframework
gone "removes an older release's DMG" apps/macos/build/astar-0.1.12-beta.dmg
exists "keeps astar.dmg without --after-publish" apps/macos/build/astar.dmg
gone "removes the merged, clean worktree" .worktrees/merged
if git_q rev-parse --verify -q work/merged >/dev/null; then bad "deletes the merged branch"; else ok "deletes the merged branch"; fi
exists "keeps the unmerged worktree" .worktrees/unmerged
exists "keeps the dirty worktree" .worktrees/dirty/new-file

echo "== --after-publish"
fixture
clean --after-publish >/dev/null
gone "removes astar.dmg once it is published" apps/macos/build/astar.dmg

echo "== a busy build is left alone"
fixture
mkdir -p "$TMP/bin"
# shellcheck disable=SC2016 # the $2 belongs to the stub, not to this script
printf '#!/usr/bin/env bash\n[ "$2" = cargo ] && exit 0\nexit 1\n' >"$TMP/bin/pgrep"
chmod +x "$TMP/bin/pgrep"
out="$(ASTAR_RELEASE_ROOT="$REPO" PATH="$TMP/bin:$PATH" "$CLEAN_SH" 2>&1)"
exists "keeps target/debug while cargo runs" target/debug
case "$out" in *"running now: cargo"*) ok "says why it skipped" ;; *) bad "says why it skipped" ;; esac

echo "== build folders linked to another drive"
fixture
DATA="$TMP/data"
ASTAR_CHECKOUT="$REPO" "$HERE/link-builds.sh" "$DATA" >/dev/null
ASTAR_CHECKOUT="$REPO/.worktrees/merged" "$HERE/link-builds.sh" "$DATA" >/dev/null
if [ -L "$REPO/target" ] && [ -d "$DATA/astar/main/target/debug" ]; then ok "link-builds moves target/ and links it"; else bad "link-builds moves target/ and links it"; fi
if [ -L "$REPO/apps/macos/build" ] && [ -d "$DATA/astar/main/macos-build/DD/Build/Products/Debug/astar.app" ]; then ok "link-builds moves the app build and links it"; else bad "link-builds moves the app build and links it"; fi
again="$(ASTAR_CHECKOUT="$REPO" "$HERE/link-builds.sh" "$DATA")"
case "$again" in *"linked already"*) ok "link-builds run twice changes nothing" ;; *) bad "link-builds run twice changes nothing" ;; esac
if git -C "$REPO" status --porcelain | grep -q .; then bad "the links are ignored by git"; else ok "the links are ignored by git"; fi
mkdir -p "$DATA/astar/long-gone/target"
clean >/dev/null
if [ -d "$DATA/astar/main/target" ] && [ ! -d "$DATA/astar/main/target/debug" ]; then ok "cleans through the main checkout's link"; else bad "cleans through the main checkout's link"; fi
exists "keeps the Debug app behind the link" apps/macos/build/DD/Build/Products/Debug/astar.app
gone "removes the merged worktree" .worktrees/merged
if [ -e "$DATA/astar/merged" ] && [ -n "$(ls -A "$DATA/astar/merged")" ]; then bad "removes the merged worktree's linked folders"; else ok "removes the merged worktree's linked folders"; fi
if [ -e "$DATA/astar/long-gone" ]; then bad "sweeps folders whose worktree is gone"; else ok "sweeps folders whose worktree is gone"; fi
if [ -d "$DATA/astar/main" ]; then ok "never sweeps main"; else bad "never sweeps main"; fi

echo "== a link of the wrong shape is never swept from"
fixture
mkdir -p "$TMP/elsewhere/keep-me" "$TMP/elsewhere/t"
rm -rf "$REPO/target"
ln -s "$TMP/elsewhere/t" "$REPO/target"
clean >/dev/null
if [ -d "$TMP/elsewhere/keep-me" ]; then ok "leaves an unrelated folder alone"; else bad "leaves an unrelated folder alone"; fi

if [ "$failures" -ne 0 ]; then
  echo "FAIL: $failures clean-builds.sh check(s) failed" >&2
  exit 1
fi
echo "clean-builds.sh: all checks passed"
