#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Snapshot the libiax2 source from astar into this harness directory.
#
# We capture astar HEAD pristine (via git archive, NOT the working tree) so
# the snapshot is reproducible from a single SHA. Local modifications to
# astar's libiax2 are out of scope here — record them as patches under
# ./patches/ instead. The Dockerfile applies vendored/ + patches/*.patch in
# order before building.
#
# Refresh by re-running this script and committing the result.

set -euo pipefail

ASTAR_REPO="${ASTAR_REPO:-$HOME/dev/astar}"
SUBPATH="vendor/iaxclient/lib/libiax2"
HERE="$(cd "$(dirname "$0")" && pwd)"
DST="$HERE/vendored/libiax2"

if [[ ! -d "$ASTAR_REPO/.git" ]]; then
  echo "snapshot: $ASTAR_REPO is not a git repo (set ASTAR_REPO if astar lives elsewhere)" >&2
  exit 2
fi

if ! git -C "$ASTAR_REPO" cat-file -e "HEAD:$SUBPATH" 2>/dev/null; then
  echo "snapshot: $SUBPATH not present at astar HEAD" >&2
  exit 2
fi

ASTAR_SHA="$(git -C "$ASTAR_REPO" rev-parse HEAD)"
IAXCLIENT_SHA="$(git -C "$ASTAR_REPO" log -1 --pretty=format:%H -- "$SUBPATH")"

# Warn (but don't fail) if astar's working tree has uncommitted changes to
# libiax2 — they're being skipped intentionally.
WIP_NOTE=""
if ! git -C "$ASTAR_REPO" diff --quiet HEAD -- "$SUBPATH"; then
  WIP_NOTE=$'\n> **Note:** astar working tree had uncommitted changes under `'"$SUBPATH"$'` at snapshot time.\n> Those are intentionally NOT vendored. Record them as `patches/NNNN-*.patch` if they\n> are needed for the captures.'
fi

rm -rf "$DST"
mkdir -p "$DST"
# Strip 4 leading path components (vendor/iaxclient/lib/libiax2/) and skip
# stale build artifacts that are committed in astar's tree. They're not
# source — they're macOS arm64 binaries from someone's earlier build that
# slipped past .gitignore upstream. We rebuild fresh inside the container.
git -C "$ASTAR_REPO" archive --format=tar HEAD -- "$SUBPATH" \
  | tar -x -C "$DST" --strip-components=4 \
        --exclude='*.o' --exclude='*.lo' --exclude='*.la' \
        --exclude='*.a' --exclude='.libs' --exclude='.deps'

cat > "$HERE/SNAPSHOT.md" <<EOF
# libiax2 source snapshot

This directory mirrors \`$SUBPATH\` from astar at a pinned SHA, captured via
\`git archive\` (always pristine — never reflects astar's working tree).
Refresh with \`./snapshot.sh\` and commit the result.

| Field | Value |
|-------|-------|
| Astar repo HEAD | \`$ASTAR_SHA\` |
| Last commit touching libiax2 | \`$IAXCLIENT_SHA\` |
| Snapshot source | \`$ASTAR_REPO/$SUBPATH\` at HEAD |
| Snapshot taken by | \`$(whoami)@$(hostname)\` |

Patches applied on top of this snapshot (in order) live under
\`./patches/\`. The Dockerfile unpacks \`vendored/\`, then \`git apply\` each
\`.patch\` file in lexical order, then builds. Reproducibility binding:
**\`$ASTAR_SHA\` + \`patches/*.patch\` = the binary that produced the
fixtures**.
$WIP_NOTE
EOF

echo "snapshot: wrote $(find "$DST" -type f | wc -l | tr -d ' ') files to $DST"
echo "snapshot: astar=$ASTAR_SHA libiax2=$IAXCLIENT_SHA"
