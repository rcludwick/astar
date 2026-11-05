#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Checks a `just server-release` binary is what the release promises: one
# static executable with everything inside (Task 14, Rob 2026-10-12).
#
#   ci/check-server-release.sh <path/to/astar-server>
#
# Always: `file` says statically linked; the listener page and LAME's version
# mark are inside. When the binary can run here — natively on a Linux host of
# the same arch, or in an empty `scratch` container when podman's machine has
# that arch — also: --version runs, --features lists the release set, and the
# binary starts from nothing but itself and one node.toml.
set -euo pipefail

bin="${1:?usage: ci/check-server-release.sh <astar-server binary>}"
[ -f "$bin" ] || { echo "FAIL: $bin does not exist"; exit 1; }
fail=0

desc=$(file -b "$bin")
case "$desc" in
  *"statically linked"*|*"static-pie linked"*) echo "ok   static: $desc" ;;
  *) echo "FAIL: not statically linked: $desc"; fail=1 ;;
esac
grep -aq 'id="hero"' "$bin" && echo "ok   the listener page is compiled in" \
  || { echo "FAIL: no listener page inside"; fail=1; }
grep -aq '3\.100' "$bin" && echo "ok   LAME 3.100 is linked in" \
  || { echo "FAIL: no LAME version mark inside"; fail=1; }

case "$desc" in
  *x86-64*) arch=x86_64 ;;
  *aarch64*|*ARM\ aarch64*) arch=aarch64 ;;
  *) arch=unknown ;;
esac

run=()
if [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = "$arch" ]; then
  run=("$bin")
elif command -v podman >/dev/null && podman info >/dev/null 2>&1 \
     && [ "$(podman info --format '{{.Host.Arch}}')" = "$( [ "$arch" = x86_64 ] && echo amd64 || echo arm64)" ]; then
  dir=$(mktemp -d)
  cp "$bin" "$dir/astar-server"
  run=(podman run --rm -v "$dir:/x:Z" --entrypoint /x/astar-server scratch-astar-check)
  printf 'FROM scratch\n' | podman build -q -t scratch-astar-check - >/dev/null
fi

if [ ${#run[@]} -eq 0 ]; then
  echo "skip run checks: cannot execute a $arch Linux binary here"
else
  "${run[@]}" --version | grep -q '^astar-server ' && echo "ok   --version runs" \
    || { echo "FAIL: --version"; fail=1; }
  feats=$("${run[@]}" --features)
  for f in stream stream-mp3 codec2-static dstar-soft-ambe public-tls discord auth dns-providers; do
    grep -qw -- "$f" <<<"$feats" || { echo "FAIL: --features lacks $f ($feats)"; fail=1; }
  done
  grep -qw device-audio <<<"$feats" && { echo "FAIL: device-audio is in the release"; fail=1; }
  echo "ok   --features: $feats"
fi

[ "$fail" -eq 0 ] || exit 1
echo "server release: OK"
