#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# On-air safety gate. Runs before every CI job that compiles or runs tests.
#
# The Rust suites contain hardware- and network-opt-in tests that are gated on
# environment variables, NOT on #[ignore]. If any of these leak into a CI shell
# (a runner profile, a project-level variable, a stray `export`), a plain
# `cargo test --workspace` will open real serial ports or dial real
# infrastructure. On a machine with a USB radio interface attached, opening the
# wrong tty asserts RTS and keys a transmitter.
#
#   IAX_THUMBDV_TESTS=1  arms every ThumbDV hardware test (astar-codec,
#                        astar-console, astar-station). Note that astar-sys
#                        defaults to the `dstar` feature, so those tests are
#                        compiled into the DEFAULT `cargo test --workspace`
#                        run — this gate is load-bearing on the everyday
#                        command, not just on the scoped D-Star recipes.
#   IAX_THUMBDV_PORT     pins WHICH dongle to use. It can only narrow the
#                        FTDI 0x0403:0x6015 scan, never replace it, but it has
#                        no business being set in CI at all.
#   IAX_PORTAL_LIVE=1    astar-station/tests/mint_token.rs hits the real
#                        AllStarLink portal, and wants ASL_USER / ASL_PASS /
#                        ASL_NODE — credentials that must never enter a CI env.
#   IAX_PARROT_LIVE=1    dials live AllStar node 55553.
#
# Nothing in CI transmits. Nothing in CI leaves 127.0.0.1. Rob is the only one
# who keys on air.
set -euo pipefail

FORBIDDEN=(
  IAX_THUMBDV_TESTS
  IAX_THUMBDV_PORT
  IAX_PORTAL_LIVE
  IAX_PARROT_LIVE
  ASL_USER
  ASL_PASS
  ASL_NODE
)

rc=0
for var in "${FORBIDDEN[@]}"; do
  value="${!var-}"
  if [ -n "$value" ]; then
    # Never echo the value: ASL_PASS is a credential.
    echo "REFUSING TO BUILD: $var is set in this CI environment." >&2
    rc=1
  fi
done

if [ "$rc" -ne 0 ]; then
  cat >&2 <<'EOF'

These variables arm hardware / live-network tests. No CI runner has a ThumbDV
attached, and no CI job may touch a transmitter or a live node. Unset them on
the runner (check the runner's config.toml `environment`, the project's CI/CD
variables, and the service user's shell profile) and re-run.
EOF
  exit 1
fi

echo "safety-env: clean (no hardware or live-network opt-in is armed)"
