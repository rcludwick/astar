#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# fetch-allstar.sh — refresh the bundled AllStar node directory snapshot at
# apps/macos/Resources/allstar.json, which ships in the app bundle as
# Contents/Resources/allstar.json and is what a first launch with no network
# reads (AllStarDirectory falls back to it when the Application Support cache
# is missing or unreadable).
#
# **Run this at release time**, in the same pass as bumping MARKETING_VERSION,
# and commit the result. The snapshot is the floor a new install starts from,
# not the live data: the app syncs on its own cadence afterwards. A snapshot
# nobody refreshes still works — it just starts everyone further behind, and
# `generated` in the file says how far, which is why the date is printed here
# and kept in the file rather than inferred from git.
#
# Source: hamcall-db's static API. No token, no account, the licence the feed states — the
# attribution and licence travel inside the file and the app displays them.
# Not dvref.com: astar reads the aggregate, and hitting the upstream directly
# is both rate-limited and the wrong layer.
#
# Usage: apps/macos/Tools/fetch-allstar.sh [url]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
URL="${1:-https://rcludwick.github.io/hamcall-db/api/v1/allstar.json}"
DEST="$HERE/Resources/allstar.json"
TMP="$(mktemp -t astar-allstar)"
trap 'rm -f "$TMP"' EXIT

echo "fetching $URL"
# Identify the client: hamcall-db's upstreams ask for a contactable User-Agent
# so a misbehaving fetcher can be mailed rather than blocked, and astar extends
# the same courtesy to hamcall-db.
curl --fail --silent --show-error --location \
    --user-agent "astar-snapshot-refresh (+https://github.com/rcludwick/astar)" \
    --output "$TMP" "$URL"

# Validate before overwriting. A 200 carrying an error page, a truncated
# transfer or a CDN's HTML would otherwise replace a working directory with
# something that decodes to nothing — and the failure would not show up until
# someone launched the app offline, which is the one case the snapshot exists
# for. The same rule the app applies to a synced payload (parse, then write).
python3 - "$TMP" <<'PY'
import json, sys

path = sys.argv[1]
with open(path, "rb") as handle:
    feed = json.load(handle)

if feed.get("api_version") != "v1":
    sys.exit(f"refusing: api_version is {feed.get('api_version')!r}, expected 'v1'")
rows = feed.get("nodes")
if not isinstance(rows, list) or len(rows) < 30000:
    sys.exit(f"refusing: 'nodes' holds {type(rows).__name__} with "
             f"{len(rows) if isinstance(rows, list) else 'no'} rows")

print(f"generated {feed.get('generated')} · {len(rows)} nodes")
PY

mv "$TMP" "$DEST"
# mktemp is 0600; a file that ships inside the bundle wants 0644.
chmod 644 "$DEST"
trap - EXIT
echo "wrote ${DEST#"$(cd "$HERE/../.." && pwd)/"} ($(wc -c <"$DEST" | tr -d ' ') bytes)"
echo "commit it — the app bundles this file verbatim."
