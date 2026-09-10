#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# test_publish_sh.sh — tests for ci/publish.sh, against a throwaway repo.
#
# Run::
#
#     ci/test_publish_sh.sh        (or `just release-test`, which runs both)
#
# Same harness as ci/test_release_sh.sh, plus three PATH shims. `publish.sh`
# talks to Apple's tools and to GitHub, so `spctl`, `xcrun` and `gh` are stubbed
# in a directory prepended to PATH: each records its argv in a log the checks
# read back, and each returns whatever the STUB_*_RC variables say. That is what
# makes "would this have shipped an unstapled DMG?" answerable offline.
#
# `origin` and `public` are bare repos in the temp directory, so the ref
# plumbing — the tag on origin, `public/main` being an ancestor — is exercised
# for real rather than mocked.
#
# What is pinned, all of it a refusal that protects a published release:
#   * the notes come from the NEWEST changelog section, or not at all;
#   * the DMG gate is spctl AND stapler, not either;
#   * publishing happens from `main`, because `main` is what gets pushed;
#   * a diverged public repo is refused rather than force-published;
#   * a dry run prints the three commands and runs none of them.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PUBLISH_SH="$HERE/publish.sh"
OLD="0.1.12-beta"
NEW="0.1.13-beta"

# `mktemp -d -t NAME` is a BSD spelling GNU coreutils rejects (it wants the
# X's). An explicit template works on both.
TMP="$(mktemp -d "${TMPDIR:-/tmp}/astar-publish-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
STUB_LOG="$TMP/stub.log"
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

lacks() { # description, needle, haystack
  case "$3" in
    *"$2"*)
      bad "$1"
      printf '       output SHOULD NOT have contained: %s\n' "$2" >&2
      ;;
    *) ok "$1" ;;
  esac
}

git_q() { git -C "$REPO" -c user.name=test -c user.email=test@example.com -c commit.gpgsign=false "$@"; }

# The stubs read these, so a check can say "Apple says the ticket is missing".
publish() {
  : >"$STUB_LOG"
  ASTAR_RELEASE_ROOT="$REPO" \
    PATH="$TMP/bin:$PATH" \
    STUB_LOG="$STUB_LOG" \
    STUB_SPCTL_RC="${STUB_SPCTL_RC:-0}" \
    STUB_STAPLER_RC="${STUB_STAPLER_RC:-0}" \
    STUB_GH_RC="${STUB_GH_RC:-0}" \
    "$PUBLISH_SH" "$@"
}

# ---------------------------------------------------------------------------
# The stubs. Nothing here reaches Apple, GitHub or the network.
# ---------------------------------------------------------------------------

mkdir -p "$TMP/bin"

cat >"$TMP/bin/spctl" <<'EOF'
#!/usr/bin/env bash
echo "spctl $*" >>"$STUB_LOG"
if [ "${STUB_SPCTL_RC:-0}" -eq 0 ]; then
  echo "the dmg: accepted"
  echo "source=Notarized Developer ID"
else
  echo "the dmg: rejected"
  echo "source=no usable signature"
fi
exit "${STUB_SPCTL_RC:-0}"
EOF

cat >"$TMP/bin/xcrun" <<'EOF'
#!/usr/bin/env bash
echo "xcrun $*" >>"$STUB_LOG"
if [ "${1:-}" = "stapler" ]; then exit "${STUB_STAPLER_RC:-0}"; fi
exit 0
EOF

cat >"$TMP/bin/gh" <<'EOF'
#!/usr/bin/env bash
echo "gh $*" >>"$STUB_LOG"
if [ "${1:-}" = "auth" ]; then exit "${STUB_GH_RC:-0}"; fi
exit 0
EOF

chmod +x "$TMP/bin/spctl" "$TMP/bin/xcrun" "$TMP/bin/gh"

# ---------------------------------------------------------------------------
# The fixture: a miniature astar with $NEW released to origin, not yet public.
# ---------------------------------------------------------------------------

mkdir -p "$REPO/apps/macos/build"

cat >"$REPO/CHANGELOG.md" <<EOF
# Changelog

## $NEW — 2026-09-09

- the release being published.

## $OLD — 2026-09-07

- the release that already shipped.
EOF

echo "not really a disk image" >"$REPO/apps/macos/build/astar.dmg"

git init -q "$REPO"
git -C "$REPO" symbolic-ref HEAD refs/heads/main
git_q add CHANGELOG.md
git_q commit -qm "fixture"

git init -q --bare "$TMP/origin.git"
git init -q --bare "$TMP/public.git"
# Both bare repos default HEAD to refs/heads/master on older git; point them at
# main so a clone of one checks out.
git -C "$TMP/origin.git" symbolic-ref HEAD refs/heads/main
git -C "$TMP/public.git" symbolic-ref HEAD refs/heads/main
git_q remote add origin "$TMP/origin.git"
git_q remote add public "$TMP/public.git"
git_q tag -a "v$NEW" -m "astar $NEW"
git_q push -q origin main "v$NEW"
git_q push -q public main # public has main, but not the tag: publish pushes it

# ---------------------------------------------------------------------------
# 1. The notes come from the NEWEST section, or not at all.
# ---------------------------------------------------------------------------

echo "changelog gate"
git_q tag -a "v$OLD" -m "astar $OLD"
git_q push -q origin "v$OLD"
set +e
out="$(publish "$OLD" --dry-run 2>&1)"
rc=$?
set -e
check "refuses to publish a version that is not the newest section" "1" "$rc"
contains "names the newest heading" "## $NEW" "$out"
contains "says what it would have done" "already out" "$out"
lacks "never reached gh release create" "release create" "$(cat "$STUB_LOG")"

# ---------------------------------------------------------------------------
# 2. The DMG gate is spctl AND stapler.
# ---------------------------------------------------------------------------

echo "notarization gate"
set +e
out="$(STUB_SPCTL_RC=3 publish "$NEW" --dry-run 2>&1)"
rc=$?
set -e
check "refuses when Gatekeeper rejects the container" "1" "$rc"
contains "says the assessment failed" "REJECTED" "$out"
contains "says both are required" "not both Gatekeeper-clean AND stapled" "$out"

set +e
out="$(STUB_STAPLER_RC=1 publish "$NEW" --dry-run 2>&1)"
rc=$?
set -e
check "refuses when the ticket is not stapled, even though spctl passes" "1" "$rc"
contains "says the ticket is missing" "NO stapled ticket" "$out"
contains "says both are required" "not both Gatekeeper-clean AND stapled" "$out"

# A good DMG says so on stdout and complains on neither stream. (Dry run: this
# suite never runs a real publish, so nothing can reach a remote.)
err="$(publish "$NEW" --dry-run 2>&1 >/dev/null)"
check "a good DMG produces no gate complaint" "" "$err"

# ---------------------------------------------------------------------------
# 3. It is `main` that gets pushed, so `main` is what is checked.
# ---------------------------------------------------------------------------

echo "branch and remote state"
git_q checkout -q -b work/elsewhere
set +e
out="$(publish "$NEW" --dry-run 2>&1)"
rc=$?
set -e
check "refuses to publish from a work branch" "1" "$rc"
contains "names the branch it found" "you are on 'work/elsewhere'" "$out"
git_q checkout -q main
git_q branch -q -D work/elsewhere

set +e
out="$(publish "0.1.99-beta" --dry-run 2>&1)"
rc=$?
set -e
check "refuses a version with no local tag" "1" "$rc"
contains "says to cut the release first" "no local tag v0.1.99-beta" "$out"

# ---------------------------------------------------------------------------
# 4. A dry run prints the three commands and runs none of them.
# ---------------------------------------------------------------------------

echo "dry run"
out="$(publish "$NEW" --dry-run 2>&1)"
log="$(cat "$STUB_LOG")"
contains "would push to public" "would run: git push public main v$NEW" "$out"
contains "would create the release with the DMG" \
  "would run: gh release create v$NEW apps/macos/build/astar.dmg" "$out"
contains "marks it --latest" "--latest" "$out"
contains "would verify what the API calls latest" \
  "would run: gh api repos/rcludwick/astar/releases/latest" "$out"
contains "lifts this version's notes" "the release being published." "$out"
contains "appends the changelog link" \
  "Full changelog: https://rcludwick.github.io/astar/changelog/" "$out"
lacks "does not publish the older section" "the release that already shipped" "$out"

contains "asked Gatekeeper" "spctl --assess" "$log"
contains "asked for the ticket" "xcrun stapler validate" "$log"
contains "checked gh auth" "gh auth status" "$log"
lacks "created no release" "release create" "$log"
lacks "called no API" "gh api" "$log"
check "pushed nothing to public" "" \
  "$(git -C "$TMP/public.git" tag -l "v$NEW")"

# ---------------------------------------------------------------------------
# 5. A diverged public repo is refused. (Last: it changes public for good.)
# ---------------------------------------------------------------------------

echo "diverged public"
git clone -q "$TMP/public.git" "$TMP/other"
git -C "$TMP/other" -c user.name=other -c user.email=other@example.com \
  -c commit.gpgsign=false commit -q --allow-empty -m "straight to public"
git -C "$TMP/other" push -q origin main

set +e
out="$(publish "$NEW" --dry-run 2>&1)"
rc=$?
set -e
check "refuses when public/main is not an ancestor of main" "1" "$rc"
contains "says the public repo diverged" "public repo has diverged" "$out"
lacks "created no release" "release create" "$(cat "$STUB_LOG")"

echo
if [ "$failures" -ne 0 ]; then
  echo "FAIL: $failures publish.sh check(s) failed" >&2
  exit 1
fi
echo "publish.sh: all checks passed"
