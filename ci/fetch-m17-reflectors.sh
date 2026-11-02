#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# fetch-m17-reflectors.sh — regenerate the M17 reflector snapshot that
# astar-server compiles in (crates/astar-server/data/m17-reflectors.json) and
# uses only when an operator writes `[m17] directory = "bundled"`.
#
# The node never fetches this at run time: a reflector name becomes an address
# only through data the operator put on the node, and this dated file is part
# of that data. `generated` travels in the file and /status shows it, so a
# stale snapshot is visible rather than inferred from git.
#
# Source: hamcall-db's static API, CC BY 4.0. The licence, its URL and the
# attribution are copied verbatim into the output, because the data may only
# be redistributed with them. So is the feed's own `modifications` notice,
# with a sentence appended saying what this script removes (CC BY 4.0
# 3(a)(1)(B): earlier modifications stay indicated and ours are added).
#
# Usage: ci/fetch-m17-reflectors.sh [SOURCE]
#   SOURCE  a URL (default: the hamcall-db v1 feed below) or a path to an
#           already-downloaded hamcall-db reflectors.json. A path touches no
#           network at all.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FEED_URL="https://rcludwick.github.io/hamcall-db/api/v1/reflectors.json"
SOURCE="${1:-$FEED_URL}"
DEST="$ROOT/crates/astar-server/data/m17-reflectors.json"
FEED="$(mktemp "${TMPDIR:-/tmp}/astar-m17-feed.XXXXXX")"
OUT="$(mktemp "${TMPDIR:-/tmp}/astar-m17-out.XXXXXX")"
trap 'rm -f "$FEED" "$OUT"' EXIT

case "$SOURCE" in
  http://* | https://*)
    ORIGIN="$SOURCE"
    echo "fetching $SOURCE"
    # A contactable User-Agent, the same courtesy fetch-reflectors.sh extends.
    curl --fail --silent --show-error --location \
      --user-agent "astar-snapshot-refresh (+https://github.com/rcludwick/astar)" \
      --output "$FEED" "$SOURCE"
    ;;
  *)
    if [ ! -f "$SOURCE" ]; then
      echo "fetch-m17-reflectors: no such file: $SOURCE" >&2
      exit 1
    fi
    # A local copy is the same feed; the attribution names where it lives.
    ORIGIN="$FEED_URL"
    cp "$SOURCE" "$FEED"
    ;;
esac

# Validate everything before writing anything: a CDN error page or a
# truncated transfer must never replace a working snapshot, and astar-server's
# own parser (Directory::from_json) refuses the whole file over one bad row.
python3 - "$FEED" "$OUT" "$ORIGIN" <<'PY'
import json
import re
import sys
import unicodedata

feed_path, out_path, origin = sys.argv[1:4]
with open(feed_path, "rb") as handle:
    feed = json.load(handle)

for field in ("generated", "license", "license_url", "attribution", "modifications"):
    if not feed.get(field):
        sys.exit(f"refusing: envelope is missing '{field}'")
rows = feed.get("reflectors")
if not isinstance(rows, list):
    sys.exit("refusing: 'reflectors' is not a list")

out = []
for row in rows:
    if row.get("network") != "m17":
        continue
    dial = row.get("dial") or {}
    rid = row.get("id")
    host = dial.get("host")
    port = dial.get("port")
    modules = dial.get("modules")
    if not isinstance(rid, str) or not re.fullmatch(r"[A-Z0-9-]{3,8}", rid):
        sys.exit(f"refusing: bad M17 id {rid!r}")
    # Same rule as astar-server's row_entry: trimmed, no whitespace, no
    # control character (Python's isspace() alone lets NUL, DEL and C1 by).
    if (not isinstance(host, str) or not host or host != host.strip()
            or any(c.isspace() or unicodedata.category(c) == "Cc" for c in host)):
        sys.exit(f"refusing: {rid}: bad host {host!r}")
    if type(port) is not int or not 1 <= port <= 65535:
        sys.exit(f"refusing: {rid}: bad port {port!r}")
    if (not isinstance(modules, list) or not modules
            or not all(isinstance(m, str) and re.fullmatch(r"[A-Z]", m) for m in modules)):
        sys.exit(f"refusing: {rid}: bad modules {modules!r}")
    out.append({"id": rid, "host": host, "port": port, "modules": sorted(set(modules))})

if not out:
    sys.exit("refusing: the feed holds no M17 reflectors")
ids = [r["id"] for r in out]
dups = sorted({i for i in ids if ids.count(i) > 1})
if dups:
    sys.exit(f"refusing: duplicate ids {', '.join(dups)}")
out.sort(key=lambda r: r["id"])

# One row per line so a refresh diffs row by row.
lines = [
    "{",
    f'  "generated": {json.dumps(feed["generated"])},',
    f'  "license": {json.dumps(feed["license"])},',
    f'  "license_url": {json.dumps(feed["license_url"])},',
    f'  "attribution": {json.dumps(feed["attribution"], ensure_ascii=False)},',
    '  "modifications": ' + json.dumps(
        feed["modifications"]
        + " astar-server keeps only the M17 rows, and of each only id, dial.host,"
        + " dial.port and dial.modules.",
        ensure_ascii=False,
    ) + ",",
    f'  "source": {json.dumps(origin)},',
    '  "reflectors": [',
]
for i, r in enumerate(out):
    comma = "," if i < len(out) - 1 else ""
    lines.append("    " + json.dumps(r, ensure_ascii=False) + comma)
lines += ["  ]", "}"]
with open(out_path, "w", encoding="utf-8") as handle:
    handle.write("\n".join(lines) + "\n")
print(f"generated {feed['generated']} · {len(out)} M17 reflectors")
PY

mkdir -p "$(dirname "$DEST")"
mv "$OUT" "$DEST"
chmod 644 "$DEST"
trap 'rm -f "$FEED"' EXIT
echo "wrote ${DEST#"$ROOT/"} ($(wc -c <"$DEST" | tr -d ' ') bytes)"
