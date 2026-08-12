#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Licence-header gate.
#
# README.md and docs/site/about/license.md both promise that every first-party
# Rust, Swift, shell and Python source file carries
#
#     SPDX-License-Identifier: AGPL-3.0-only
#
# A promise nothing enforces rots the first time a file is added, and a false
# licensing claim in a published doc is worse than no claim at all. So this
# guard makes the promise checkable.
#
# Two directories are DELIBERATELY exempt because they are not ours to relabel:
#   vendor/                                       ambe-thumbdv, MIT OR Apache-2.0
#   harness/asterisk_parity/c_iaxclient/vendored/ the historical C libiax2, GPL/LGPL
# Stamping an AGPL header on either would misstate someone else's terms — even
# though Rob wrote ambe-thumbdv, it is permissively licensed on purpose.
#
# Docs (*.md) are covered by the root LICENSE, not by per-file headers, and are
# not checked here. Neither doc claims otherwise.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

missing=""
checked=0

while IFS= read -r f; do
  case "$f" in
    vendor/* | harness/asterisk_parity/c_iaxclient/vendored/*) continue ;;
  esac
  checked=$((checked + 1))
  # Only the first 10 lines: the header belongs at the top of the file, not
  # buried in a string literal or a test fixture halfway down.
  if ! head -n 10 "$f" | grep -q 'SPDX-License-Identifier: AGPL-3.0-only'; then
    missing="${missing}${f}"$'\n'
  fi
done < <(git ls-files '*.rs' '*.swift' '*.sh' '*.py')

if [ -n "$missing" ]; then
  echo "FAIL: source file(s) with no AGPL-3.0-only SPDX header in the first 10 lines:" >&2
  printf '%s' "$missing" >&2
  echo "      Add these three lines at the top (after a shebang or a" >&2
  echo "      swift-tools-version line, if present):" >&2
  echo >&2
  echo "        // astar — Copyright (c) 2026 Rob Ludwick." >&2
  echo "        // SPDX-License-Identifier: AGPL-3.0-only" >&2
  echo "        // Licensed under the GNU Affero General Public License v3.0 only. See LICENSE." >&2
  exit 1
fi

echo "spdx-headers: clean ($checked first-party rs/swift/sh/py files carry AGPL-3.0-only)"
