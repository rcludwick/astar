#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# nodebench rig: the loopback pod (ASL3 node 1999 + astar-server 1998 +
# tshark), driven by podman. See RIG.md.
#
#   rig.sh build                  build both images (native arch)
#   rig.sh up [--ulaw] [--debug] [--register] [--fill-callerid]
#                                 start the pod, wait until both nodes answer
#                                 (options: RIG.md "Variants")
#   rig.sh down                   stop and remove the pod
#   rig.sh status                 pod, app_rpt nodes, IAX channels, /status
#   rig.sh logs [asl3|astar-server|capture]
#   rig.sh cli "<asterisk cmd>"   asterisk -rx inside the node
#   rig.sh full-log               the node's /var/log/asterisk/full
#
# Environment:
#   CONTAINER_CONNECTION  podman connection (the Mac: podman-machine-default)
#   NODEBENCH_CODECS      node codec list, offer order (default slin16,slin,ulaw)
#   NODEBENCH_DEBUG       1 = iax2 debug + rpt debug level 7 from the start
#   RUST_LOG              astar-server log filter (default debug)
#   NODEBENCH_CAPTURES    pcap directory (default harness/nodebench/captures)
#   NODEBENCH_USRP_HOST   where usrp-relay sends the node's USRP TX audio
#                         (default host.containers.internal, port 34001)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
pod=nodebench

die() { echo "rig: $*" >&2; exit 1; }

render() {
    local codecs="$1" debug="$2" captures="$3"
    sed -e "s|@CODECS@|$codecs|" -e "s|@DEBUG@|$debug|" \
        -e "s|@RUST_LOG@|${RUST_LOG:-debug}|" -e "s|@CAPTURES@|$captures|" -e "s|@CONFIG@|${4:-node.toml}|" \
        -e "s|@FILLCID@|${5:-0}|" \
        -e "s|@USRP_HOST@|${NODEBENCH_USRP_HOST:-host.containers.internal}|" \
        "$here/pod.yaml"
}

wait_ready() {
    local t0=$SECONDS
    local node=0 server=0
    while ((SECONDS - t0 < 90)); do
        if ((node == 0)) && podman exec "$pod-asl3" asterisk -rx 'rpt localnodes' 2>/dev/null |
            grep -qx '1999'; then
            node=1
            echo "rig: app_rpt has node 1999 ($((SECONDS - t0)) s)"
        fi
        if ((server == 0)) && curl -fsS --max-time 2 http://127.0.0.1:28730/status >/dev/null 2>&1; then
            server=1
            echo "rig: astar-server /status answers ($((SECONDS - t0)) s)"
        fi
        if ((node == 1 && server == 1)); then return 0; fi
        sleep 0.5
    done
    die "not ready after 90 s (node=$node server=$server); try: rig.sh logs"
}

cmd="${1:-}"
shift || true
case "$cmd" in
    build)
        podman build -t localhost/nodebench-asl3 "$here/images/asl3"
        podman build -t localhost/nodebench-astar-server \
            -f "$here/images/astar-server/Containerfile" \
            --ignorefile "$here/images/astar-server/Containerfile.ignore" "$repo"
        ;;
    up)
        codecs="${NODEBENCH_CODECS:-slin16,slin,ulaw}"
        debug="${NODEBENCH_DEBUG:-0}"
        config=node.toml
        fillcid=0
        for a in "$@"; do
            case "$a" in
                --ulaw) codecs=ulaw ;;
                --debug) debug=1 ;;
                --register) config="node-register.toml" ;;
                --fill-callerid) fillcid=1 ;;
                *) die "up: unknown option $a" ;;
            esac
        done
        captures="${NODEBENCH_CAPTURES:-$here/captures}"
        mkdir -p "$captures"
        if podman pod exists "$pod"; then die "pod $pod already exists; rig.sh down first"; fi
        render "$codecs" "$debug" "$captures" "$config" "$fillcid" | podman kube play - >/dev/null
        echo "rig: pod up (codecs=$codecs debug=$debug config=$config fill-callerid=$fillcid captures=$captures)"
        wait_ready
        ;;
    down)
        if podman pod exists "$pod"; then
            render x 0 /tmp | podman kube down - >/dev/null 2>&1 || podman pod rm -f "$pod" >/dev/null
        fi
        echo "rig: down"
        ;;
    status)
        podman pod ps --filter "name=^$pod\$"
        podman ps --filter "pod=$pod" --format '{{.Names}}\t{{.Status}}'
        echo "--- rpt nodes 1999"
        podman exec "$pod-asl3" asterisk -rx 'rpt nodes 1999' || true
        echo "--- iax2 show channels"
        podman exec "$pod-asl3" asterisk -rx 'iax2 show channels' || true
        echo "--- iax2 show registry / peers"
        podman exec "$pod-asl3" asterisk -rx 'iax2 show peers' || true
        echo "--- astar-server /status"
        curl -fsS --max-time 2 http://127.0.0.1:28730/status || echo "(no answer)"
        echo
        ;;
    logs)
        podman logs "$pod-${1:-asl3}"
        ;;
    cli)
        [ $# -ge 1 ] || die 'cli "<asterisk command>"'
        podman exec "$pod-asl3" asterisk -rx "$*"
        ;;
    full-log)
        podman exec "$pod-asl3" cat /var/log/asterisk/full
        ;;
    *)
        sed -n '7,/^set -euo/p' "$0" | sed '$d'
        exit 2
        ;;
esac
