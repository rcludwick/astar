#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Run nodebench the way the IONOS runner would: inside a rootless container
# (runner-standin.Containerfile) that runs its own rootless podman. This is
# the evidence behind the CI job's runner requirements (README.md "CI"), and
# the way to re-check them on the Mac before changing the box.
#
#   harness/nodebench/ci/nested-trial.sh [scenario...]
#
# It builds the stand-in, builds `nodebench` for linux in rust:1-bookworm,
# copies the rig images (rig.sh build first) into the nested store, and runs
# the scenarios with 10 s holds. Output goes to target/nodebench-nested/.
#
# Everything stays inside the stand-in container's own network namespace; no
# port is published from it at all.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
work="$repo/target/nodebench-nested"
mkdir -p "$work/out" "$work/target" "$work/cargo"

# The flags the runner container needs (README.md "CI"). Remove any one and
# the nested pod fails to start; each failure is listed there.
nest_flags=(
    --device /dev/fuse --device /dev/net/tun
    --security-opt label=disable
    --security-opt unmask=ALL
    --security-opt seccomp=unconfined
)

podman build -q -t localhost/nodebench-runner-standin -f "$here/runner-standin.Containerfile" "$here" >/dev/null

# A linux build of the bench from a clean copy of HEAD, so a half-edited
# working tree cannot break it.
rm -rf "$work/src" && mkdir -p "$work/src"
git -C "$repo" archive HEAD | tar -x -C "$work/src"
podman run --rm -e CARGO_TARGET_DIR=/target \
    -v "$work/src:/src:ro" -v "$work/target:/target" -v "$work/cargo:/usr/local/cargo/registry" \
    -w /src docker.io/library/rust:1-bookworm bash -c \
    'apt-get update -qq >/dev/null && apt-get install -y -qq libasound2-dev libudev-dev >/dev/null 2>&1
     cargo build -q --locked -p astar-nodebench'

podman save -q -o "$work/rig-images.tar" \
    localhost/nodebench-asl3:latest localhost/nodebench-astar-server:latest \
    docker.io/nicolaka/netshoot:v0.13

if [ $# -eq 0 ]; then
    set -- rig_ready node_links_to_server ulaw_only_node audio_node_to_server \
        audio_server_to_node keepalive_exchange
fi

podman run --rm "${nest_flags[@]}" \
    -v "$work/rig-images.tar:/rig-images.tar:ro" \
    -v "$work/src:/src:ro" \
    -v "$work/target/debug:/nb:ro" \
    -v "$work/out:/out:rw,U" \
    localhost/nodebench-runner-standin bash -c '
        podman load -q -i /rig-images.tar >/dev/null
        /nb/nodebench run --rig-dir /src/harness/nodebench --json-out /out \
            --hold-secs 10 --var USRP_HOST=10.0.2.2 "$@"' nested "$@"
