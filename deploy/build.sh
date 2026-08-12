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

TARGET=x86_64-unknown-linux-gnu
GLIBC=2.38   # link floor: trixie's libasound imports __isoc23_* (glibc 2.38+);
             # VPS runtime is trixie (glibc 2.41) so 2.38 <= runtime holds.
SYSROOT="$PWD/deploy/sysroot-amd64"

# --- one-time host prereqs (idempotent) -------------------------------------
command -v pkg-config >/dev/null     || brew install pkg-config
command -v zig >/dev/null            || brew install zig
command -v cargo-zigbuild >/dev/null || brew install cargo-zigbuild
rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"

# --- one-time amd64 alsa sysroot from pinned trixie debs --------------------
# alsa-sys is the only native dep; it needs alsa.pc, headers, and libasound.so
# for the *target*. Extracting the runtime + dev debs into a sysroot provides
# all three (the dev deb's libasound.so symlink resolves against the runtime
# deb's libasound.so.2.* in the same tree).
if [ ! -f "$SYSROOT/.ok" ]; then
    rm -rf "$SYSROOT"
    mkdir -p "$SYSROOT/debs"
    (
      cd "$SYSROOT/debs"
      curl -fsSLO https://deb.debian.org/debian/pool/main/a/alsa-lib/libasound2t64_1.2.14-1_amd64.deb
      curl -fsSLO https://deb.debian.org/debian/pool/main/a/alsa-lib/libasound2-dev_1.2.14-1_amd64.deb
      shasum -a 256 -c <<'SUMS'
f03a2bd9d234f4e6d283c36520d66befd0952d6f5ba454badd8fae2305ad70a1  libasound2t64_1.2.14-1_amd64.deb
dd1e577c2984c02d40ef7c352174b7ba24b5cf85aa2b906bf2e88e2b52e7ac65  libasound2-dev_1.2.14-1_amd64.deb
SUMS
      for d in *.deb; do
          ar -x "$d"
          tar -xf data.tar.xz -C "$SYSROOT"
          rm -f data.tar.xz control.tar.xz debian-binary
      done
    )
    touch "$SYSROOT/.ok"
fi

# Route alsa-sys's pkg-config lookups into the sysroot.
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_LIBDIR="$SYSROOT/usr/lib/x86_64-linux-gnu/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"

cargo zigbuild --release --target "$TARGET.$GLIBC" -p astar-server

mkdir -p deploy/out
cp "target/$TARGET/release/astar-server" deploy/out/astar-server
echo "deploy/out/astar-server:"
file deploy/out/astar-server
