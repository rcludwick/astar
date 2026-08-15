#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# check-linux.sh — build, test, and RUN astar-gui on Linux from a Mac, via a
# podman container (astar-feee). This is the local stand-in for the gui.yml
# Linux CI job: same package set, same headless `--shot` run under Xvfb with
# the tiny-skia software renderer. The proof PNG lands in .shots/linux-idle.png
# — Read/eyeball it like any other shots.sh output.
#
# The repo is mounted at its REAL path so the ../../crates/astar-station path
# dep resolves inside the container exactly as it does on the host.
# Named volumes keep the cargo registry + target across runs; debuginfo is off
# to keep the target volume small (the podman VM disk is modest).
#
# Requires: a running podman machine (`podman machine start`).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

# --init: a real PID-1 reaper. Without it bash execs xvfb-run into PID 1,
# where a missed SIGCHLD once left the whole run hung after the app exited.
podman run --rm --init \
  -v "$REPO:$REPO" \
  -v astar-cargo-registry:/usr/local/cargo/registry \
  -v astar-linux-target:/ltarget \
  -e CARGO_TARGET_DIR=/ltarget \
  -e CARGO_PROFILE_DEV_DEBUG=0 \
  -w "$HERE" \
  rust:1 bash -c "
set -e
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq libasound2-dev libudev-dev pkg-config xvfb \
  libxcursor1 libxrandr2 libxi6 libxkbcommon0 libxkbcommon-x11-0 >/dev/null 2>&1
echo '>> cargo build (Linux)'
cargo build
echo '>> cargo test (Linux)'
cargo test
echo '>> headless --shot run (Xvfb + tiny-skia)'
mkdir -p .shots
ICED_BACKEND=tiny-skia timeout 120 xvfb-run -a /ltarget/debug/astar-gui --shot idle .shots/linux-idle.png
"
echo ">> Linux check ok — proof PNG at apps/gui/.shots/linux-idle.png"
