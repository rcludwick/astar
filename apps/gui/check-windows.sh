#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# check-windows.sh — cross-compile + link a real Windows astar-gui.exe from the
# Mac (astar-feee). cargo-xwin fetches the MSVC CRT + Windows SDK and drives
# clang/lld-link, so this catches unix-only code and link errors in the whole
# dep tree (cpal/WASAPI, serialport, ring's C sources) without a Windows box.
#
# What it CANNOT prove is that the window opens — that's the gui.yml Windows
# CI job (`--shot` on a windows-latest runner). Treat this as the fast local
# pre-flight for that job.
#
# Requires: `cargo install cargo-xwin` and `brew install llvm` (for llvm-lib
# and llvm-rc, the MSVC-flavored archiver/resource tools cc-rs invokes).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

export PATH="/opt/homebrew/opt/llvm/bin:$PATH"
command -v cargo-xwin >/dev/null || { echo "!! cargo-xwin missing: cargo install cargo-xwin" >&2; exit 1; }
command -v llvm-lib   >/dev/null || { echo "!! llvm-lib missing: brew install llvm" >&2; exit 1; }

rustup target add x86_64-pc-windows-msvc >/dev/null 2>&1 || true
echo ">> cargo xwin build --release (x86_64-pc-windows-msvc)"
cargo xwin build --release --target x86_64-pc-windows-msvc

# One cargo workspace: the shared target/ dir lives at the repo root.
EXE="$(cd ../.. && pwd)/target/x86_64-pc-windows-msvc/release/astar-gui.exe"
ls -la "$EXE"
file "$EXE"
echo ">> Windows cross-build ok — $EXE"
