#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Every HTTP server in the workspace runs on crates/astar-http. tiny_http
# drained unread request bodies through a buffer the size the client
# announced (iax-bodydrain) and had no deadline; it must not come back,
# as a dependency or a dev-dependency of anything.
set -euo pipefail
if ! tree=$(cargo tree --workspace --all-features -e normal,dev,build --prefix none 2>&1); then
  echo "FAIL: 'cargo tree --workspace' errored"; echo "$tree"; exit 1
fi
if grep -qE '^tiny_http v' <<<"$tree"; then
  echo "FAIL: tiny_http is in the workspace; use astar-http (astar_http::testing for test stubs)"
  exit 1
fi
if grep -q 'name = "tiny_http"' Cargo.lock; then
  echo "FAIL: tiny_http is still in Cargo.lock"; exit 1
fi
echo "OK: no tiny_http anywhere in the workspace"
