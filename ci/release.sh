#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# release.sh — cut a release of astar, up to and including the push to `origin`.
#
# This is the release that used to live in somebody's head on release day:
# 0.1.12-beta was cut by hand on 2026-09-07 with exactly these steps, in
# exactly this order. Automating them is not about saving keystrokes — it is
# about the steps that are easy to forget (the reflector snapshot, the path-dep
# versions, the tag) being impossible to forget.
#
#     ci/release.sh <version> [--dry-run] [--skip-reflectors]
#
# What it does, in order:
#
#   1. Preconditions — on `main`, clean tree, `origin` fetched, `main` not
#      behind `origin/main`.
#   2. CHANGELOG.md's newest `## <version> — <date>` heading must already be
#      the version being released. The notes are WRITTEN BY HAND; this script
#      never invents them.
#   3. Bump the version in all five homes, refresh Cargo.lock, and prove it
#      with `ci/version_manifest.py --check`.
#   4. Refresh the bundled reflector snapshot (`just reflectors`).
#   5. Gates: `just ci`, `just xcframework`, `just app-test`, `just dmg`.
#   6. Commit as `chore: <version>`, tag `v<version>`, push both to `origin`.
#
# PUBLISHING IS NOT HERE. Pushing to the public repo and creating the GitHub
# release is a separate, deliberate second step — `ci/publish.sh` — because
# CLAUDE.md says publishing is Rob's call and never a step in a task.
#
# Flags:
#   --dry-run          Run only the read-only checks and print every command
#                      that would run. Nothing is modified, ever.
#   --skip-reflectors  Skip step 4 (the snapshot needs the network).
#   --bump-only        Do step 3's file edits and stop, without cargo, the
#                      manifest check, the gates or git. This is what
#                      ci/test_release_sh.sh drives; it is not a release mode.
#
# Bash 3.2 (what macOS ships) — no associative arrays, no `mapfile`, and no
# `timeout` command anywhere.
set -euo pipefail

ROOT="${ASTAR_RELEASE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

PENDING_NOTES="docs/superpowers/notes/2026-09-07-pending-changelog.md"
DMG_PATH="apps/macos/build/astar.dmg"

DRY_RUN=0
SKIP_REFLECTORS=0
BUMP_ONLY=0
VERSION=""

die() {
  echo "FAIL: $*" >&2
  exit 1
}

usage() {
  echo "usage: ci/release.sh <version> [--dry-run] [--skip-reflectors]"
  echo "       e.g. ci/release.sh 0.1.13-beta --dry-run"
}

# Escape a version string for the left-hand side of a sed s/// — only `.` can
# mean anything else, since a version is [0-9A-Za-z.-] by the grammar below.
re_escape() {
  printf '%s' "$1" | sed 's/\./\\./g'
}

# Rewrite a file through sed without relying on `sed -i`, whose argument shape
# differs between BSD (macOS) and GNU sed.
rewrite() {
  local file="$1"
  local expr="$2"
  local tmp="$1.release.$$"
  sed "$expr" "$file" >"$tmp"
  mv "$tmp" "$file"
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --skip-reflectors) SKIP_REFLECTORS=1 ;;
    --bump-only) BUMP_ONLY=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    -*) die "unknown option: $1" ;;
    *)
      [ -z "$VERSION" ] || die "one version at a time (already have $VERSION, then got $1)"
      VERSION="$1"
      ;;
  esac
  shift
done

[ -n "$VERSION" ] || {
  usage >&2
  die "no version given"
}

# SemVer since 0.1.10-beta, and the release script only ever writes new
# versions — so the glued form (`0.1.9beta`) is deliberately NOT accepted here
# even though version_manifest.py still reads it in published manifests.
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
  die "not SemVer: '$VERSION'. Releases are MAJOR.MINOR.PATCH[-pre], e.g. 0.1.13-beta"

if [ "$DRY_RUN" = 1 ]; then
  echo "== astar release $VERSION — DRY RUN (nothing will be modified)"
else
  echo "== astar release $VERSION"
fi

# ---------------------------------------------------------------------------
# 1. Preconditions
# ---------------------------------------------------------------------------

git rev-parse --git-dir >/dev/null 2>&1 || die "$ROOT is not a git checkout"

branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] ||
  die "releases are cut on main, not '$branch'. Merge your work first."

[ -z "$(git status --porcelain)" ] ||
  die "the tree is dirty. Commit or shelve everything first (\`git status\`)."

git remote get-url origin >/dev/null 2>&1 ||
  die "no 'origin' remote. astar releases are pushed to rcludwick/astar-private."

echo ">> git fetch origin"
git fetch origin --quiet --tags

git rev-parse --verify --quiet origin/main >/dev/null ||
  die "origin has no main branch to compare against"

behind="$(git rev-list --count HEAD..origin/main)"
[ "$behind" = "0" ] ||
  die "main is $behind commit(s) behind origin/main. Pull first."

if git rev-parse --verify --quiet "refs/tags/v$VERSION" >/dev/null; then
  die "tag v$VERSION already exists locally. That release was already cut."
fi

# ---------------------------------------------------------------------------
# 2. The changelog is written by hand, and must already name this release
# ---------------------------------------------------------------------------

[ -f CHANGELOG.md ] || die "no CHANGELOG.md in $ROOT"

heading="$(grep -m1 '^## ' CHANGELOG.md || true)"
[ -n "$heading" ] || die "CHANGELOG.md has no '## <version> — <date>' heading"

# Field-split rather than a regex: the dash in the heading is an em dash, and a
# bracket expression holding multibyte characters is not portable across seds.
heading_version="$(printf '%s\n' "$heading" | awk '{print $2}')"
heading_date="$(printf '%s\n' "$heading" | awk '{print $NF}')"

if [ "$heading_version" != "$VERSION" ]; then
  echo "FAIL: CHANGELOG.md's newest heading is not the release you asked for." >&2
  echo "    newest heading:  $heading" >&2
  echo "    asked to release: $VERSION" >&2
  echo >&2
  echo "      The release notes are written BY HAND, before the release: add a" >&2
  echo "      '## $VERSION — $(date +%Y-%m-%d)' section at the top of CHANGELOG.md," >&2
  echo "      moving the waiting bullets out of" >&2
  echo "        $PENDING_NOTES" >&2
  echo "      (gitignored, and may not exist), then commit it and run this again." >&2
  exit 1
fi

case "$heading_date" in
  [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) ;;
  *) die "CHANGELOG.md's newest heading has no YYYY-MM-DD date: $heading" ;;
esac

echo ">> changelog: $heading"

# ---------------------------------------------------------------------------
# 3. The version, in five homes
# ---------------------------------------------------------------------------

[ -f Cargo.toml ] || die "no Cargo.toml in $ROOT"

# The current version, from the one home that is always strict SemVer. Scoped
# to [workspace.package]: [workspace.dependencies] is full of inline tables
# carrying versions of their own.
CURRENT="$(
  awk '
    /^\[workspace\.package\]/ { in_section = 1; next }
    /^\[/                     { in_section = 0 }
    in_section && /^version[[:space:]]*=/ {
      gsub(/^version[[:space:]]*=[[:space:]]*"/, "")
      gsub(/".*$/, "")
      print
      exit
    }
  ' Cargo.toml
)"
[ -n "$CURRENT" ] || die "no [workspace.package] version in Cargo.toml"

echo ">> current version: $CURRENT  ->  $VERSION"

[ "$CURRENT" != "$VERSION" ] ||
  die "Cargo.toml already says $VERSION. Nothing to bump — is this release already cut?"

OLD_RE="$(re_escape "$CURRENT")"

# The files that carry the old string, discovered rather than listed: a new
# crate joins the workspace without anyone remembering to add it here.
EDITED=""
for manifest in crates/*/Cargo.toml apps/*/Cargo.toml; do
  [ -f "$manifest" ] || continue
  if grep -q "^astar-[a-z0-9-]*[[:space:]]*=.*version[[:space:]]*=[[:space:]]*\"$OLD_RE\"" "$manifest"; then
    EDITED="$EDITED$manifest
"
  fi
done

echo ">> files that change:"
echo "     Cargo.toml                       [workspace.package] version"
printf '%s' "$EDITED" | while IFS= read -r f; do
  [ -n "$f" ] || continue
  n="$(grep -c "^astar-[a-z0-9-]*[[:space:]]*=.*version[[:space:]]*=[[:space:]]*\"$OLD_RE\"" "$f")"
  printf '     %-32s %s astar-* path dep(s)\n' "$f" "$n"
done
echo "     apps/macos/project.yml           MARKETING_VERSION"
echo "     zensical.toml                    the footer version chip"
echo "     Cargo.lock                       via cargo update -w"
if [ "$SKIP_REFLECTORS" = 0 ]; then
  echo "     apps/macos/Resources/reflectors.json  via just reflectors"
fi

if [ "$DRY_RUN" = 1 ]; then
  echo
  echo "== the rest of the release, which a dry run does not do:"
  echo "   would edit: the files listed above, $CURRENT -> $VERSION"
  echo "   would run: cargo update -w"
  echo "   would run: python3 ci/version_manifest.py --check"
  if [ "$SKIP_REFLECTORS" = 0 ]; then
    echo "   would run: just reflectors"
  else
    echo "   would skip: just reflectors (--skip-reflectors)"
  fi
  echo "   would run: just ci"
  echo "   would run: just xcframework"
  echo "   would run: just app-test"
  echo "   would run: just dmg          -> $DMG_PATH"
  echo "   would run: git commit -m 'chore: $VERSION' <the files above>"
  echo "   would run: git tag -a v$VERSION -m 'astar $VERSION'"
  echo "   would run: git push origin main v$VERSION"
  echo
  echo "== dry run OK — nothing was modified. Publishing stays a separate step:"
  echo "   just publish $VERSION"
  exit 0
fi

# Every command from here can leave edits behind, so failures say what to undo.
CHANGED_LIST="Cargo.toml Cargo.lock apps/macos/project.yml zensical.toml $(printf '%s' "$EDITED" | tr '\n' ' ')"

fail_after_edits() {
  echo "FAIL: $1" >&2
  echo >&2
  echo "      The version edits are ALREADY MADE and are still in your tree." >&2
  echo "      Nothing was committed, tagged or pushed. To undo them:" >&2
  echo >&2
  echo "        git checkout -- $CHANGED_LIST" >&2
  echo >&2
  echo "      Or fix what failed and run \`ci/release.sh $VERSION\` again — it" >&2
  echo "      refuses a dirty tree, so undo first either way." >&2
  exit 1
}

echo ">> bumping $CURRENT -> $VERSION"

# The root workspace version: a bare `version = "…"` at the start of a line,
# which inside [workspace.package] is the only line of that shape.
rewrite Cargo.toml "s/^version = \"$OLD_RE\"\$/version = \"$VERSION\"/"

# The astar-* path-dependency requirements. Anchored to a line that starts with
# the crate name so a third-party dependency that happens to share the version
# string cannot be caught up in it.
printf '%s' "$EDITED" | while IFS= read -r manifest; do
  [ -n "$manifest" ] || continue
  rewrite "$manifest" "/^astar-[a-z0-9-]*[[:space:]]*=/ s/\"$OLD_RE\"/\"$VERSION\"/g"
done

rewrite apps/macos/project.yml \
  "s/^\\([[:space:]]*MARKETING_VERSION:[[:space:]]*\\)\"$OLD_RE\"/\\1\"$VERSION\"/"

# The chip is `…<span class=\"astar-version\">v0.1.12-beta</span>` inside a TOML
# string, so the quotes around the class are backslash-escaped in the file
# itself. Matching from `>v` to `</span>` sidesteps that entirely, and the file
# holds exactly one `</span>`.
rewrite zensical.toml "s|>v$OLD_RE</span>|>v$VERSION</span>|"

if [ "$BUMP_ONLY" = 1 ]; then
  echo ">> --bump-only: the version strings are rewritten, nothing else ran."
  exit 0
fi

echo ">> cargo update -w"
cargo update -w || fail_after_edits "cargo update -w failed"

echo ">> python3 ci/version_manifest.py --check"
python3 ci/version_manifest.py --check ||
  fail_after_edits "the version does not agree across its five homes"

# ---------------------------------------------------------------------------
# 4. The bundled reflector snapshot — refreshed at release time, on purpose
# ---------------------------------------------------------------------------

if [ "$SKIP_REFLECTORS" = 1 ]; then
  echo ">> skipping just reflectors (--skip-reflectors)"
else
  echo ">> just reflectors"
  just reflectors || fail_after_edits "just reflectors failed (network?)"
fi

# ---------------------------------------------------------------------------
# 5. The gates. All four, in the order they get cheaper to fail.
# ---------------------------------------------------------------------------

for gate in ci xcframework app-test dmg; do
  echo ">> just $gate"
  just "$gate" || fail_after_edits "\`just $gate\` failed"
done

[ -f "$DMG_PATH" ] || fail_after_edits "just dmg produced no $DMG_PATH"

# ---------------------------------------------------------------------------
# 6. Commit, tag, push to origin (the PRIVATE repo)
# ---------------------------------------------------------------------------

echo ">> committing chore: $VERSION"
# Named paths, never `git add -A`: the tree also holds gitignored build output
# and a DMG nobody wants in history.
git add Cargo.toml Cargo.lock apps/macos/project.yml zensical.toml
printf '%s' "$EDITED" | while IFS= read -r manifest; do
  [ -n "$manifest" ] || continue
  git add "$manifest"
done
if [ "$SKIP_REFLECTORS" = 0 ] && [ -f apps/macos/Resources/reflectors.json ]; then
  git add apps/macos/Resources/reflectors.json
fi

git commit -m "chore: $VERSION"
git tag -a "v$VERSION" -m "astar $VERSION"
git push origin main "v$VERSION"

echo
echo "== released $VERSION to origin (rcludwick/astar-private)."
echo "   tag:  v$VERSION"
echo "   dmg:  $DMG_PATH"
echo
echo "   PUBLISHING IS A SEPARATE, DELIBERATE STEP. When Rob says to publish:"
echo "     just publish $VERSION"
