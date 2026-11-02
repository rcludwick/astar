#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Fast linux/amd64 build of astar-server via cargo-zigbuild (iax-4703).
# Contract: drop a linux/amd64 binary at deploy/out/astar-server.
# Primary path since 2026-07-04: the podman machine lacks Rosetta and
# qemu-user segfaults on amd64 rustc (spec R2) — so we cross-compile on the
# host and use containers only for image assembly.
set -euo pipefail
cd "$(dirname "$0")/.."

# Homebrew's rustup shim loses argv[0] (its `cargo` symlink resolves to the
# rustup binary itself, which then misbehaves as bare `rustup` instead of
# dispatching to cargo). Put the real toolchain bin dir ahead of ~/.cargo/bin
# so the genuine cargo/rustc binaries win the PATH lookup.
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"

# Since the single static release (Task 14): `just server-release`, a static
# musl binary with everything inside. No alsa sysroot and no glibc floor any
# more: the release has no sound-device stack (a node runs backend = "none")
# and links no shared library at all.
command -v zig >/dev/null            || brew install zig
command -v cargo-zigbuild >/dev/null || brew install cargo-zigbuild
command -v cmake >/dev/null          || brew install cmake

TARGET=x86_64-unknown-linux-musl
just server-release "$TARGET"

mkdir -p deploy/out
cp "target/server-release/$TARGET/astar-server" deploy/out/astar-server
echo "deploy/out/astar-server:"
file deploy/out/astar-server
