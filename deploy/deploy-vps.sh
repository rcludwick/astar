#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Rapid redeploy (iax-4703): incremental compile → ship the binary →
# thin image rebuild on the VPS → service restart. ~1-2 min warm.
set -euo pipefail
cd "$(dirname "$0")/.."
VPS="${ASTAR_VPS:?set ASTAR_VPS=user@host for your own server}"

deploy/build.sh
ssh "$VPS" 'mkdir -p /tmp/iaxnode-deploy'
scp deploy/out/astar-server deploy/Containerfile.app "$VPS:/tmp/iaxnode-deploy/"
ssh "$VPS" 'podman build -t astar-server:latest \
              -f /tmp/iaxnode-deploy/Containerfile.app /tmp/iaxnode-deploy \
            && /home/allstar/bin/iaxnode-run'
sleep 3
ssh "$VPS" 'podman ps --filter name=iaxnode --format "{{.Names}} {{.Status}}" | grep " Up " && curl -sf http://127.0.0.1:8730/status'
echo
