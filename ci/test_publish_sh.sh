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
#   * a real publish pushes one squashed commit with the tag's files, dated in
#     the evening MST, and no private commit object;
#   * a release already on public is refused;
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
    ASTAR_PUBLISH_NOW="${ASTAR_PUBLISH_NOW:-1789268400}" \
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
case "$*" in *releases/latest*) echo "${STUB_LATEST:-}" ;; esac
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
# Public has its own history: one squashed root commit that shares no object
# with the private one, as the real public repo does.
old_root="$(git -C "$REPO" -c user.name=t -c user.email=t@e commit-tree "$(git -C "$REPO" rev-parse "HEAD^{tree}")" -m "astar $OLD")"
git_q push -q public "$old_root:refs/heads/main"
# The private work after it: a second commit, then the release tag.
echo "more private work" >"$REPO/private.txt"
git_q add private.txt
git_q commit -qm "a private commit that must never reach public"
git_q tag -d "v$NEW" >/dev/null
git_q tag -a "v$NEW" -m "astar $NEW"
git_q push -q -f origin main "v$NEW"

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
contains "would build one squashed commit" "would build: one commit with v$NEW's files on top of public main" "$out"
contains "would push it and its tag atomically" "would run: git push --atomic public <that commit>:main <that tag>:refs/tags/v$NEW" "$out"
contains "dates it in the evening, MST" " MST," "$out"
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
# 5. A real publish pushes ONE squashed commit, and nothing private.
#    (Last: it changes public for good.)
# ---------------------------------------------------------------------------

echo "push hours"
set +e
out="$(ASTAR_PUBLISH_NOW=1788975000 publish "$NEW" 2>&1)"
rc=$?
set -e
check "refuses a real publish on a weekday at 10:30 AM MST" "1" "$rc"
contains "says when pushes are allowed" "no pushes to the public repo Monday-Friday, 7 AM to 5 PM MST" "$out"
check "pushed nothing during work hours" "" "$(git -C "$TMP/public.git" tag -l "v$NEW")"
out="$(ASTAR_PUBLISH_NOW=1788975000 publish "$NEW" --dry-run 2>&1)"
contains "a dry run in work hours says NOT NOW" "NOT NOW: it is Wed 2026-09-09 10:30 MST" "$out"
out="$(ASTAR_PUBLISH_NOW=1788998400 publish "$NEW" --dry-run 2>&1)"
contains "5 PM on a weekday is allowed" "allowed now: it is Wed 2026-09-09 17:00 MST" "$out"

echo "real publish"
priv="$(git -C "$REPO" rev-parse "v$NEW^{commit}")"
out="$(STUB_LATEST="v$NEW" ASTAR_CLEAN_IGNORE_BUSY=1 publish "$NEW" 2>&1)" || {
  bad "a real publish succeeds"
  printf '%s\n' "$out" >&2
}
pub="$(git -C "$TMP/public.git" rev-parse main)"
check "public main gained exactly one commit" "2" "$(git -C "$TMP/public.git" rev-list --count main)"
check "it sits on the previous public release" "$old_root" "$(git -C "$TMP/public.git" rev-parse main^)"
check "it carries exactly the tag's files" "$(git -C "$REPO" rev-parse "v$NEW^{tree}")" \
  "$(git -C "$TMP/public.git" rev-parse "main^{tree}")"
check "its message names the release" "astar $NEW" "$(git -C "$TMP/public.git" log -1 --format=%s main)"
if git -C "$TMP/public.git" cat-file -e "$priv" 2>/dev/null; then
  bad "no private commit reaches public"
else
  ok "no private commit reaches public"
fi
check "the public tag names the squashed commit" "$pub" "$(git -C "$TMP/public.git" rev-parse "v$NEW^{commit}")"
hour="$(TZ=America/Phoenix git -C "$TMP/public.git" log -1 --format=%ad --date=format-local:%H main)"
if [ "$hour" -ge 18 ] && [ "$hour" -le 22 ]; then ok "dated in the evening, MST ($hour h)"; else bad "dated in the evening, MST (got $hour h)"; fi
check "the private tag is untouched" "$priv" "$(git -C "$REPO" rev-parse "v$NEW^{commit}")"

set +e
out="$(publish "$NEW" --dry-run 2>&1)"
rc=$?
set -e
check "refuses to publish the same release twice" "1" "$rc"
contains "says it is already public" "already on the public repo" "$out"

echo
if [ "$failures" -ne 0 ]; then
  echo "FAIL: $failures publish.sh check(s) failed" >&2
  exit 1
fi
echo "publish.sh: all checks passed"
