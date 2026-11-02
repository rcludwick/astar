#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# nodebench hand gate: bring a 1999 <-> 1998 link up from one side, then poll
# both sides once a second and report how long it holds. Exit 0 if it held
# for the whole window, 1 if it dropped (or never came up).
#
#   hold.sh node   [seconds]   ASL3 dials: rpt cmd 1999 ilink 3 1998
#   hold.sh server [seconds]   astar-server dials: POST /link 1999 @ 127.0.0.1:4569
#
# Needs the pod up (rig.sh up). Prints one line per second:
#   t=<s> node=<up|down: rpt nodes 1999 lists 1998> server=<what /status shows>
# An inbound link from app_rpt shows in /status as an Active call (calls[]),
# not in links[], which holds only links astar-server dialled itself; the
# server side counts either.
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
side="${1:?node|server}"
secs="${2:-90}"
ctl=http://127.0.0.1:28730

node_has_link() {
    "$here/rig.sh" cli 'rpt nodes 1999' 2>/dev/null | grep -Eq '(^|[ ,T])1998([ ,]|$)'
}
server_links() {
    curl -fsS --max-time 2 "$ctl/status" 2>/dev/null |
        python3 -c 'import json,sys
d = json.load(sys.stdin)
up = ["link:" + str(l.get("node", "?")) for l in d.get("links", [])]
up += ["call:" + str(c.get("id")) + "/" + str(c.get("negotiated_format")) for c in d.get("calls", []) if c.get("state") == "Active"]
print(",".join(up) or "-")' 2>/dev/null ||
        echo "?"
}

case "$side" in
    node) "$here/rig.sh" cli 'rpt cmd 1999 ilink 3 1998' ;;
    server) curl -fsS -X POST "$ctl/link" -d '{"action":"connect","node":"1999","addr":"127.0.0.1:4569"}'; echo ;;
    *) echo "hold.sh: side must be node or server" >&2; exit 2 ;;
esac
echo "hold: dialled from $side at $(date -u +%H:%M:%SZ)"

up_seen=0 dropped=0
for ((t = 1; t <= secs; t++)); do
    sleep 1
    if node_has_link; then n=up; else n=down; fi
    s=$(server_links)
    if [ "$s" != "-" ] && [ "$s" != "?" ]; then sv=up; else sv=down; fi
    echo "t=${t}s node=$n server=$s"
    if [ "$n" = up ] && [ "$sv" = up ]; then up_seen=1; fi
    if [ "$up_seen" = 1 ] && { [ "$n" = down ] || [ "$sv" = down ]; }; then
        echo "hold: DROPPED at t=${t}s (node=$n server=$s)"
        dropped=1
        break
    fi
done
if [ "$up_seen" = 0 ]; then echo "hold: link never came up on both sides in ${secs}s"; exit 1; fi
if [ "$dropped" = 1 ]; then exit 1; fi
echo "hold: held ${secs}s"
