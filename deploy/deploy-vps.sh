#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Rapid redeploy (iax-4703): incremental compile → ship the binary →
# thin image rebuild on the VPS → service restart → deploy/verify.sh →
# prune old images. ~1-2 min warm. ASTAR_VPS must be the service user
# (allstar@host): podman here is rootless and belongs to it. A failed verify
# prints the rollback command and leaves the previous image in place.
set -euo pipefail
cd "$(dirname "$0")/.."
VPS="${ASTAR_VPS:?set ASTAR_VPS=user@host for your own server}"

IMAGE=localhost/iaxclient-node:latest

deploy/build.sh
# The image the tag points at BEFORE this deploy: the rollback target. The
# build below leaves it untagged, and nothing prunes it until verify passes.
PREV="$(ssh "$VPS" "podman image inspect -f '{{.Id}}' $IMAGE 2>/dev/null" || true)"
# Box clock, so the restart check is immune to Mac/box clock skew.
DEPLOY_T0="$(ssh "$VPS" 'date +%s')"

rollback_hint() {
  echo >&2
  echo "deploy-vps: FAILED. The previous image has not been pruned." >&2
  if [ -n "$PREV" ]; then
    echo "  Roll back (re-tag the previous image, recreate the container):" >&2
    echo "    ssh $VPS 'podman tag $PREV $IMAGE && /home/allstar/bin/iaxnode-run'" >&2
  else
    echo "  No previous image was recorded, so there is nothing to roll back to." >&2
  fi
  echo "  Then re-check:  ASTAR_VPS=$VPS deploy/verify.sh" >&2
}

ssh "$VPS" 'mkdir -p /tmp/iaxnode-deploy'
scp deploy/out/astar-server deploy/Containerfile.app "$VPS:/tmp/iaxnode-deploy/"
# The tag MUST match what run-iaxnode.sh starts (gh-runners:
# vps/run-iaxnode.sh). It did not between the astar-server rename and
# 2026-09-02: this built `astar-server:latest` while the helper ran
# `iaxclient-node:latest`, so a deploy built a fresh image under a tag
# nothing used and then restarted the old one. It reported success and
# shipped nothing. verify.sh's container-image check now catches that.
if ! ssh "$VPS" "podman build -t $IMAGE \
                   -f /tmp/iaxnode-deploy/Containerfile.app /tmp/iaxnode-deploy \
                 && /home/allstar/bin/iaxnode-run"; then
  rollback_hint
  exit 1
fi

# The gate: container, image tag, restart, registration, secrets in the log,
# disk, UDP 4569 (and the public checks when ASTAR_PUBLIC_HOST is set). It
# polls /status until registered, so there is no fixed sleep here.
if ! ASTAR_VPS="$VPS" ASTAR_DEPLOYED_AFTER="$DEPLOY_T0" deploy/verify.sh; then
  rollback_hint
  exit 1
fi

# Each build leaves the previous image untagged. Nothing removed them, and
# about sixty (7 GB) filled the 9.7 GB disk by 2026-09-27. Prune only after
# verify passes, so a failed deploy keeps its fallback image.
ssh "$VPS" 'podman image prune -f >/dev/null'
echo "deploy-vps: deployed, verified, old images pruned"
