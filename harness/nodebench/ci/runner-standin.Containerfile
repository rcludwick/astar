# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# A stand-in for the IONOS gh-runner image (Ubuntu 24.04, user `runner`,
# uid 1001) with exactly what nodebench needs to run nested rootless podman.
# nested-trial.sh runs the rig inside it to prove the recipe before anything
# changes on the runner box. The real image is built from
# rcludwick/gh-runners runner/Containerfile; README.md "CI" lists the same
# lines for it.
FROM docker.io/library/ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        podman uidmap fuse-overlayfs slirp4netns passt catatonit libcap2-bin \
        ca-certificates curl \
        # nodebench's own runtime libraries (astar-audio links ALSA); the
        # real runner already has the -dev packages.
        libasound2t64 libudev1 \
    && rm -rf /var/lib/apt/lists/*

# newuidmap/newgidmap with file capabilities, not setuid: the setuid bit
# alone fails inside a rootless container ("write to uid_map failed:
# Operation not permitted"). This is what quay.io/podman/stable does.
RUN chmod 0755 /usr/bin/newuidmap /usr/bin/newgidmap \
    && setcap cap_setuid=ep /usr/bin/newuidmap \
    && setcap cap_setgid=ep /usr/bin/newgidmap

# The inner user namespace: every id the outer container has (0-65536)
# except runner's own 1001, which maps to itself.
RUN useradd -m -u 1001 runner \
    && printf 'runner:1:1000\nrunner:1002:64534\n' > /etc/subuid \
    && printf 'runner:1:1000\nrunner:1002:64534\n' > /etc/subgid

# fuse-overlayfs, because the container's own root is already overlay.
# allow_host_loopback lets a pod reach the runner's 127.0.0.1 at 10.0.2.2,
# which is how the node's USRP transmit audio gets back to the bench
# (nodebench run --var USRP_HOST=10.0.2.2).
RUN mkdir -p /home/runner/.config/containers \
    && printf '[storage]\ndriver = "overlay"\n[storage.options.overlay]\nmount_program = "/usr/bin/fuse-overlayfs"\n' \
        > /home/runner/.config/containers/storage.conf \
    && printf '[engine]\ncgroup_manager = "cgroupfs"\nevents_logger = "file"\nnetwork_cmd_options = ["allow_host_loopback=true"]\n' \
        > /home/runner/.config/containers/containers.conf \
    && chown -R runner:runner /home/runner

USER runner
WORKDIR /home/runner
