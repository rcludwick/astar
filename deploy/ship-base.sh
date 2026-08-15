#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Ship the iaxnode-base image to the VPS (iax-4703). Run on first deploy and
# whenever Containerfile.base changes — NOT per code change (deploy-vps.sh is that).
set -euo pipefail
cd "$(dirname "$0")/.."
VPS="${ASTAR_VPS:?set ASTAR_VPS=user@host for your own server}"

podman build --platform linux/amd64 -t iaxnode-base -f deploy/Containerfile.base deploy
podman save localhost/iaxnode-base | ssh "$VPS" 'podman load'
