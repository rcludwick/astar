#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Build + link the C parrot example against the astar-sys staticlib.
#
# Run from crates/astar-sys/ (or anywhere; paths are resolved relative to
# this script). Assumes `cargo build --release -p astar-sys` has produced
# target/release/libastar_sys.a.
#
# Output: ./parrot (or ./parrot-dry with IAX_PARROT_DRYRUN=1, a no-network
# offline smoke that CI can run).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE="$(cd "$HERE/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$CRATE/../../target}/release"
LIB="$TARGET_DIR/libastar_sys.a"

if [ ! -f "$LIB" ]; then
  echo "missing $LIB — run: cargo build --release -p astar-sys" >&2
  exit 1
fi

CC="${CC:-cc}"
DEFS=()
OUT="$HERE/parrot"
if [ "${IAX_PARROT_DRYRUN:-0}" = "1" ]; then
  DEFS+=(-DIAX_PARROT_DRYRUN)
  OUT="$HERE/parrot-dry"
fi

# Frameworks the staticlib pulls in (confirmed on arm64 macOS):
#   CoreFoundation/CoreAudio/AudioUnit/AudioToolbox — cpal CoreAudio backend
#   IOKit                                           — serialport (UCI150 PTT)
#   Security                                        — SecRandomCopyBytes, pulled
#                                                     in by ring (via boringtun,
#                                                     the WireGuard transport)
FRAMEWORKS=()
case "$(uname -s)" in
  Darwin)
    FRAMEWORKS=(
      -framework CoreFoundation
      -framework CoreAudio
      -framework AudioUnit
      -framework AudioToolbox
      -framework IOKit
      -framework Security
    )
    ;;
  Linux)
    # The cdylib/staticlib links ALSA via cpal; the system libs resolve through
    # the staticlib's own deps. Add -lasound -lpthread -ldl -lm explicitly.
    FRAMEWORKS=(-lasound -lpthread -ldl -lm)
    ;;
esac

set -x
"$CC" "${DEFS[@]+"${DEFS[@]}"}" "$HERE/parrot.c" -I "$CRATE/include" "$LIB" "${FRAMEWORKS[@]+"${FRAMEWORKS[@]}"}" -o "$OUT"
set +x
echo "built: $OUT"
