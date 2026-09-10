#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# test_release_sh.sh — tests for ci/release.sh, against a throwaway repo.
#
# Run::
#
#     ci/test_release_sh.sh        (or `just release-test`)
#
# Hermetic and offline: it builds a miniature astar in a temp directory — a
# workspace Cargo.toml, one crate manifest with an astar-* path dep, the two
# non-Rust homes of the version, a CHANGELOG — plus a bare repo standing in for
# `origin`, so the real `git fetch origin` precondition runs against a file
# path. Nothing here calls cargo, just or gh; `--bump-only` is exactly the
# seam that makes that possible, because it stops after the file edits.
#
# What is pinned:
#   * the refusal when CHANGELOG.md's newest heading is not the release asked
#     for — the one that stops a release going out with the wrong notes;
#   * that a dry run reports the right files and modifies nothing;
#   * that the bump rewrites the five homes and NOTHING ELSE. A third-party
#     dependency that happens to carry the same version string is the trap
#     here, and there is a decoy in the fixture for exactly that.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELEASE_SH="$HERE/release.sh"
OLD="0.1.12-beta"
NEW="0.1.13-beta"

# `mktemp -d -t NAME` is a BSD spelling GNU coreutils rejects (it wants the
# X's). An explicit template works on both.
TMP="$(mktemp -d "${TMPDIR:-/tmp}/astar-release-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
failures=0

ok() { printf '  ok   %s\n' "$1"; }
bad() {
  printf '  FAIL %s\n' "$1" >&2
  failures=$((failures + 1))
}

check() { # description, expected, actual
  if [ "$2" = "$3" ]; then ok "$1"; else
    bad "$1"
    printf '       expected: %s\n       actual:   %s\n' "$2" "$3" >&2
  fi
}

contains() { # description, needle, haystack
  case "$3" in
    *"$2"*) ok "$1" ;;
    *)
      bad "$1"
      printf '       output did not contain: %s\n' "$2" >&2
      ;;
  esac
}

git_q() { git -C "$REPO" -c user.name=test -c user.email=test@example.com -c commit.gpgsign=false "$@"; }

release() { ASTAR_RELEASE_ROOT="$REPO" "$RELEASE_SH" "$@"; }

# ---------------------------------------------------------------------------
# The fixture: a miniature astar, at $OLD.
# ---------------------------------------------------------------------------

mkdir -p "$REPO/crates/astar-x" "$REPO/apps/macos"

cat >"$REPO/Cargo.toml" <<EOF
[workspace]
members = ["crates/astar-x"]

[workspace.package]
version = "$OLD"
edition = "2024"

[workspace.dependencies]
# The decoy: a third-party crate that happens to be at the same version. A
# blanket search-and-replace would eat this, and nothing downstream would
# notice until a build broke.
notastar = { version = "$OLD" }
EOF

cat >"$REPO/crates/astar-x/Cargo.toml" <<EOF
[package]
name = "astar-x"
version.workspace = true

[dependencies]
astar-y = { path = "../astar-y", version = "$OLD" }
notastar = { workspace = true }
EOF

cat >"$REPO/apps/macos/project.yml" <<EOF
settings:
    MARKETING_VERSION: "$OLD"
    OTHER_VERSION: "$OLD"
EOF

cat >"$REPO/zensical.toml" <<EOF
[project]
copyright = "Copyright &copy; 2026 Rob Ludwick &nbsp;<span class=\\"astar-version\\">v$OLD</span>"
EOF

cat >"$REPO/CHANGELOG.md" <<EOF
# Changelog

## $OLD — 2026-09-07

- the release that already shipped.
EOF

git init -q "$REPO"
git -C "$REPO" symbolic-ref HEAD refs/heads/main
git_q add -A
git_q commit -qm "fixture"

git init -q --bare "$TMP/origin.git"
git_q remote add origin "$TMP/origin.git"
git_q push -q origin main

# ---------------------------------------------------------------------------
# 1. A release whose notes are not written yet is refused.
# ---------------------------------------------------------------------------

echo "changelog gate"
set +e
out="$(release "$NEW" --dry-run 2>&1)"
rc=$?
set -e
check "refuses when the newest heading is not the version asked for" "1" "$rc"
contains "names the heading it found" "## $OLD" "$out"
contains "points at the pending-notes file" "2026-09-07-pending-changelog.md" "$out"

set +e
out="$(release "$OLD" --dry-run 2>&1)"
rc=$?
set -e
check "refuses a version the tree already carries" "1" "$rc"
contains "says the bump would be a no-op" "already says $OLD" "$out"

# Write the section by hand, the way a release day does.
{
  echo "# Changelog"
  echo
  echo "## $NEW — 2026-09-09"
  echo
  echo "- the release being cut."
  echo
  sed -n '3,$p' "$REPO/CHANGELOG.md"
} >"$REPO/CHANGELOG.md.new"
mv "$REPO/CHANGELOG.md.new" "$REPO/CHANGELOG.md"
git_q commit -qam "docs: $NEW notes"
git_q push -q origin main

# ---------------------------------------------------------------------------
# 2. Bad input and a dirty tree are refused before anything else.
# ---------------------------------------------------------------------------

echo "preconditions"
set +e
out="$(release "0.1.13beta" --dry-run 2>&1)"
rc=$?
set -e
check "refuses the glued (non-SemVer) spelling" "1" "$rc"
contains "says why" "not SemVer" "$out"

echo "scratch" >"$REPO/dirty.txt"
set +e
out="$(release "$NEW" --dry-run 2>&1)"
rc=$?
set -e
check "refuses a dirty tree" "1" "$rc"
contains "says the tree is dirty" "tree is dirty" "$out"
rm "$REPO/dirty.txt"

# ---------------------------------------------------------------------------
# 3. The dry run reports the right files and touches nothing.
# ---------------------------------------------------------------------------

echo "dry run"
out="$(release "$NEW" --dry-run 2>&1)"
contains "reports the current version" "$OLD  ->  $NEW" "$out"
contains "names the crate manifest" "crates/astar-x/Cargo.toml" "$out"
contains "names the app project" "apps/macos/project.yml" "$out"
contains "names the site config" "zensical.toml" "$out"
contains "would build the DMG" "would run: just dmg" "$out"
contains "would tag" "git tag -a v$NEW" "$out"
contains "leaves publishing to the second step" "just publish $NEW" "$out"
check "modified nothing" "" "$(git -C "$REPO" status --porcelain)"

# ---------------------------------------------------------------------------
# 4. The bump rewrites the five homes, and only those.
# ---------------------------------------------------------------------------

echo "bump"
release "$NEW" --bump-only >/dev/null

check "workspace version" \
  "version = \"$NEW\"" \
  "$(grep -m1 '^version = ' "$REPO/Cargo.toml")"
check "astar-* path dep" \
  "astar-y = { path = \"../astar-y\", version = \"$NEW\" }" \
  "$(grep '^astar-y' "$REPO/crates/astar-x/Cargo.toml")"
check "MARKETING_VERSION" \
  "    MARKETING_VERSION: \"$NEW\"" \
  "$(grep 'MARKETING_VERSION' "$REPO/apps/macos/project.yml")"
contains "the site's version chip" \
  ">v$NEW</span>" \
  "$(cat "$REPO/zensical.toml")"

# ...and the things that must NOT move.
check "the third-party decoy in the workspace deps is untouched" \
  "notastar = { version = \"$OLD\" }" \
  "$(grep '^notastar' "$REPO/Cargo.toml")"
check "an unrelated Xcode setting is untouched" \
  "    OTHER_VERSION: \"$OLD\"" \
  "$(grep 'OTHER_VERSION' "$REPO/apps/macos/project.yml")"
check "the changelog is never rewritten" \
  "2" \
  "$(grep -c '^## ' "$REPO/CHANGELOG.md")"

# Exactly the four files, and no stray sed temp file left behind.
check "the files that changed" \
  "Cargo.toml apps/macos/project.yml crates/astar-x/Cargo.toml zensical.toml" \
  "$(git -C "$REPO" status --porcelain | awk '{print $2}' | LC_ALL=C sort | tr '\n' ' ' | sed 's/ $//')"

echo
if [ "$failures" -ne 0 ]; then
  echo "FAIL: $failures release.sh check(s) failed" >&2
  exit 1
fi
echo "release.sh: all checks passed"
