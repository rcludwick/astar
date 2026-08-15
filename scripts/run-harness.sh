#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Build and run the astar-inspect web operator console (the harness).
#
# Serves a browser UI to place a web-transceiver call (e.g. parrot 55553):
# connect form, live TX/RX meters, PTT, and a Shutdown button. Binds
# 127.0.0.1 only (the secret crosses the wire in plaintext).
#
# Usage:
#   scripts/run-harness.sh            # bind 127.0.0.1:8080, open the browser
#   scripts/run-harness.sh 8090       # bind 127.0.0.1:8090
#   scripts/run-harness.sh 127.0.0.1:8090
#   NO_OPEN=1 scripts/run-harness.sh  # don't auto-open the browser
#
# Stop it from the UI's Shutdown button, or with Ctrl-C.

set -euo pipefail

# Homebrew's rustup shim loses argv[0]; use the real toolchain bin dir.
export PATH="/Users/rob/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

# Run from the repo root regardless of where this is invoked from.
cd "$(dirname "$0")/.."

# Load local harness config (HARNESS_SECRET / HARNESS_NODE / HARNESS_CALLING_NODE /
# HARNESS_NAME). Not committed — see .gitignore. The binary reads these as
# form defaults; HARNESS_SECRET is injected server-side, never sent to the browser.
if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

# Resolve the bind address: bare port -> 127.0.0.1:PORT; full addr passed through.
ARG="${1:-8080}"
if [[ "$ARG" == *:* ]]; then
  ADDR="$ARG"
else
  ADDR="127.0.0.1:$ARG"
fi

echo "Building astar-inspect..."
cargo build -q -p astar-inspect

URL="http://${ADDR}"
if [[ -z "${NO_OPEN:-}" ]]; then
  # Open the browser shortly after the server starts listening.
  ( sleep 1; open "$URL" >/dev/null 2>&1 || true ) &
fi

echo "Starting harness on ${URL}"
exec ./target/debug/astar-inspect "$ADDR"
