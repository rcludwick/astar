#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# publish.sh — push a release that already exists on `origin` to the PUBLIC
# repo, and create the GitHub release with the signed, notarized DMG on it.
#
#     ci/publish.sh <version> [--dry-run]
#
# THIS IS THE DELIBERATE SECOND STEP. `ci/release.sh` stops at `origin`
# (rcludwick/astar-private) on purpose: CLAUDE.md says publishing is Rob's
# call and never a step in a task. Nothing may run this script on his behalf.
#
# Both repositories share one history, so the push to `public` is a
# fast-forward and never a merge.
#
# What it does:
#   1. Preconditions — HEAD is `main` (it is main that gets pushed), the tag
#      exists locally, is reachable from main AND is on origin, the DMG is on
#      disk and BOTH Gatekeeper-clean and stapled, `gh` is authenticated.
#   2. Extracts this version's section out of CHANGELOG.md as the release
#      notes, with the "Full changelog" link every earlier release carried.
#      CHANGELOG.md's NEWEST heading must be this version: publishing an older
#      section as --latest would put stale notes on the release the docs link
#      to, and the changelog is in descending order, so anything below the top
#      is already out.
#   3. `git push public main v<version>`
#   4. `gh release create` with astar.dmg attached, marked --latest (NOT a
#      prerelease: the docs link to /releases/latest and it must resolve).
#   5. Reads the published tag back from the API and checks it.
#
# Bash 3.2 (what macOS ships) — no associative arrays, no `mapfile`, and no
# `timeout` command anywhere.
set -euo pipefail

ROOT="${ASTAR_RELEASE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

PUBLIC_REPO="rcludwick/astar"
DMG_PATH="apps/macos/build/astar.dmg"
CHANGELOG_URL="https://rcludwick.github.io/astar/changelog/"

DRY_RUN=0
VERSION=""

die() {
  echo "FAIL: $*" >&2
  exit 1
}

usage() {
  echo "usage: ci/publish.sh <version> [--dry-run]"
  echo "       e.g. ci/publish.sh 0.1.13-beta --dry-run"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
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

TAG="v$VERSION"

if [ "$DRY_RUN" = 1 ]; then
  echo "== astar publish $VERSION — DRY RUN (reads only; touches nothing but remote-tracking refs)"
else
  echo "== astar publish $VERSION -> $PUBLIC_REPO"
fi

# ---------------------------------------------------------------------------
# 1. Preconditions
# ---------------------------------------------------------------------------

git rev-parse --git-dir >/dev/null 2>&1 || die "$ROOT is not a git checkout"

# What gets pushed is the branch `main`, not HEAD — so `main` is what every
# check below has to be about. Publishing from a detached HEAD or a work branch
# would check one commit and push a different one.
branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] ||
  die "publishing pushes main, and you are on '$branch'. Check out main first."

git rev-parse --verify --quiet "refs/tags/$TAG" >/dev/null ||
  die "no local tag $TAG. Cut the release first: just release $VERSION"

git merge-base --is-ancestor "$TAG^{commit}" main ||
  die "$TAG is not reachable from main — the tag names a commit main does not contain."

git remote get-url public >/dev/null 2>&1 ||
  die "no 'public' remote. It should be git@github.com:$PUBLIC_REPO.git"

echo ">> checking $TAG is on origin"
if [ -z "$(git ls-remote --tags origin "refs/tags/$TAG")" ]; then
  die "$TAG is not on origin. \`just release $VERSION\` pushes it; publish never skips that."
fi

# The public repo must be a fast-forward of what is being published: both
# repos share one history, and a divergence means something was committed
# straight to public.
echo ">> git fetch public"
git fetch public --quiet || die "could not fetch the public remote"
if git rev-parse --verify --quiet public/main >/dev/null; then
  if ! git merge-base --is-ancestor public/main main; then
    die "public/main is not an ancestor of main — the public repo has diverged."
  fi
fi

[ -f "$DMG_PATH" ] ||
  die "no $DMG_PATH. \`just release $VERSION\` builds it via \`just dmg\`."

# The DMG must be the distributable article, not an ad-hoc local build. These
# are the same two assertions make-dmg.sh ends a good run with, and BOTH must
# hold — they answer different questions and neither implies the other:
#
#   spctl    the container passes the assessment a downloader's Mac makes when
#            they open the download. A signed-but-unnotarized DMG fails here.
#   stapler  the ticket is attached to the image, so a first launch works with
#            the network unplugged. Apple can notarize an image whose ticket
#            never got stapled, and that image passes spctl on a machine that
#            can reach Apple — and fails on one that cannot.
#
# make-dmg.sh exits 3 rather than ship a DMG that misses either, so publishing
# one would be shipping an artifact that script already refused.
echo ">> verifying $DMG_PATH the way a downloader's Mac will"
gate_ok=1

# Captured, not piped: `spctl … | sed` reports SED's exit status, so a piped
# assessment silently "passes" no matter what Gatekeeper said.
spctl_out="$(spctl --assess --type open --context context:primary-signature -v "$DMG_PATH" 2>&1)" &&
  spctl_rc=0 || spctl_rc=$?
printf '%s\n' "$spctl_out" | sed 's/^/   spctl:   /'
if [ "$spctl_rc" -ne 0 ]; then
  echo "   spctl:   REJECTED (exit $spctl_rc)" >&2
  gate_ok=0
fi

if xcrun stapler validate "$DMG_PATH" >/dev/null 2>&1; then
  echo "   stapler: stapled ticket present (works offline)"
else
  echo "   stapler: NO stapled ticket" >&2
  gate_ok=0
fi

[ "$gate_ok" = 1 ] ||
  die "$DMG_PATH is not both Gatekeeper-clean AND stapled. Do not ship it — rebuild with \`just dmg\` on a machine with the Developer ID identity and the notary profile."

echo ">> gh auth status"
gh auth status >/dev/null 2>&1 ||
  die "gh is not authenticated (\`gh auth login\`)"

# ---------------------------------------------------------------------------
# 2. Release notes — this version's CHANGELOG.md section, verbatim
# ---------------------------------------------------------------------------

[ -f CHANGELOG.md ] || die "no CHANGELOG.md in $ROOT"

# The NEWEST heading must be this release — the same rule release.sh applies,
# and for a sharper reason here. Publishing an older section as `--latest`
# would put stale notes on the release everything links to, and the changelog
# is in descending order, so anything but the top entry is a version that has
# already been published.
heading="$(grep -m1 '^## ' CHANGELOG.md || true)"
[ -n "$heading" ] || die "CHANGELOG.md has no '## <version> — <date>' heading"
heading_version="$(printf '%s\n' "$heading" | awk '{print $2}')"
if [ "$heading_version" != "$VERSION" ]; then
  echo "FAIL: CHANGELOG.md's newest heading is not the release you asked to publish." >&2
  echo "    newest heading:  $heading" >&2
  echo "    asked to publish: $VERSION" >&2
  echo >&2
  echo "      Publishing $VERSION now would attach the notes of a release that" >&2
  echo "      is already out, and mark it --latest. Publish the newest release," >&2
  echo "      or check out the commit whose changelog tops out at $VERSION." >&2
  exit 1
fi

# An explicit template: `mktemp -t NAME` is a BSD spelling GNU rejects.
NOTES="$(mktemp "${TMPDIR:-/tmp}/astar-release-notes.XXXXXX")"
trap 'rm -f "$NOTES"' EXIT

# From the `## <version> — <date>` heading to the next `## `, heading dropped:
# the GitHub release already carries the version in its title.
awk -v want="$VERSION" '
  /^## / {
    if (in_section) { exit }
    if ($2 == want) { in_section = 1; next }
  }
  # Skip the blank line that always follows the heading, so the release body
  # starts with prose rather than whitespace.
  in_section && !started && /^[[:space:]]*$/ { next }
  in_section { started = 1; print }
' CHANGELOG.md >"$NOTES"

[ -s "$NOTES" ] ||
  die "CHANGELOG.md has no '## $VERSION — <date>' section to publish as notes"

# Trailing blank lines are noise in the release body; strip them (command
# substitution eats them), then add the link every earlier release carried.
printf '%s\n' "$(cat "$NOTES")" >"$NOTES.trimmed"
mv "$NOTES.trimmed" "$NOTES"

{
  echo
  echo "Full changelog: $CHANGELOG_URL"
} >>"$NOTES"

echo ">> release notes ($(wc -l <"$NOTES" | tr -d ' ') lines):"
sed 's/^/   | /' "$NOTES"

# ---------------------------------------------------------------------------
# 3-5. Push, release, verify
# ---------------------------------------------------------------------------

if [ "$DRY_RUN" = 1 ]; then
  echo
  echo "== what publishing would do:"
  echo "   would run: git push public main $TAG"
  echo "   would run: gh release create $TAG $DMG_PATH -R $PUBLIC_REPO \\"
  echo "                --title \"astar $VERSION\" --notes-file <the notes above> --latest"
  echo "   would run: gh api repos/$PUBLIC_REPO/releases/latest --jq .tag_name"
  echo
  echo "== dry run OK — nothing was pushed or created."
  exit 0
fi

echo ">> git push public main $TAG"
git push public main "$TAG"

# --latest, never --prerelease: the docs and the in-app update check both read
# /releases/latest, and a prerelease does not answer there.
# The asset must arrive named exactly astar.dmg — the download link in the docs
# is by name, so a renamed upload breaks it.
echo ">> gh release create $TAG"
gh release create "$TAG" "$DMG_PATH" \
  -R "$PUBLIC_REPO" \
  --title "astar $VERSION" \
  --notes-file "$NOTES" \
  --latest

echo ">> verifying what the API now calls latest"
latest="$(gh api "repos/$PUBLIC_REPO/releases/latest" --jq .tag_name)"
[ "$latest" = "$TAG" ] ||
  die "the release was created but /releases/latest still reports '$latest', not $TAG"

echo
echo "== published $VERSION."
echo "   https://github.com/$PUBLIC_REPO/releases/tag/$TAG"
