#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Keeps the EchoLink test fences where they belong.
#
# 1. No EchoLink host name in the engine. The default directory servers live
#    in AstarCore's defaults and astar-server's config default only, so no
#    engine code or test can name one.
# 2. Every EchoLink socket goes through astar-console's `echolink/net.rs`,
#    the one file whose functions refuse non-loopback addresses in a test
#    build. No other EchoLink source binds, connects, sends or resolves:
#    not astar-console's or astar-echolink's EchoLink code, not any
#    `echolink*` file in astar-station or astar-server, and not the EchoLink
#    functions of astar-server's controller.rs (every `fn` whose name has
#    `echolink` in it, and its EchoLink test helpers, `fn el_*`).
# 3. The two test-only features never reach a real build. `loopback-fakes`
#    (astar-echolink's fake directory and station, which keep passwords) and
#    `echolink-loopback-only` (astar-console's loopback guard) may be turned
#    on only from test wiring: a [dev-dependencies] entry, or a feature that
#    is itself test-only (`echolink-loopback-only`, `test-support`). The
#    authority is cargo's own resolution: no workspace member's normal build,
#    with its default features, may enable either feature.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0

# --- 1. No EchoLink host name in the engine -------------------------------
host_dirs=(crates/astar-echolink crates/astar-console crates/astar-station crates/astar-sys)
for d in "${host_dirs[@]}"; do
  if [ ! -d "$d" ]; then
    echo "FAIL: $d is missing, so the host-name check cannot run"
    fail=1
  fi
done
set +e
hits=$(grep -rni "echolink.org" "${host_dirs[@]}" 2>&1)
status=$?
set -e
if [ "$status" -eq 0 ]; then
  echo "FAIL: an EchoLink host name is in the engine (it belongs in AstarCore / astar-server defaults):"
  echo "$hits"
  fail=1
elif [ "$status" -ge 2 ]; then
  echo "FAIL: the host-name grep could not run:"
  echo "$hits"
  fail=1
fi

# --- 2. Sockets only through echolink/net.rs ------------------------------
# Required paths must exist; the station and server paths join the scan as
# soon as they do.
socket_re='UdpSocket::bind|TcpStream::connect|TcpListener::bind|to_socket_addrs|\.send_to\(|\.connect\('
socket_required=(
  crates/astar-console/src/echolink crates/astar-echolink/src
  crates/astar-server/src/echolink_node.rs
)
socket_paths=()
for p in "${socket_required[@]}"; do
  if [ -e "$p" ]; then socket_paths+=("$p"); else
    echo "FAIL: $p is missing, so the socket check cannot run"; fail=1
  fi
done
# Any other `echolink*` file or directory in the station and the server
# joins the scan as soon as it exists.
for p in crates/astar-station/src/echolink* crates/astar-server/src/echolink*; do
  if [ -e "$p" ] && [ "$p" != crates/astar-server/src/echolink_node.rs ]; then
    socket_paths+=("$p")
  fi
done
# The exclusions are exact paths, not bare file names, so a `net.rs` or
# `fake.rs` anywhere else is still scanned.
excluded_re='^(crates/astar-console/src/echolink/net\.rs|crates/astar-echolink/src/fake\.rs):'
if [ "${#socket_paths[@]}" -gt 0 ]; then
  set +e
  hits=$(grep -rnE "$socket_re" "${socket_paths[@]}" 2>&1 | grep -vE "$excluded_re")
  status=$?
  set -e
  # grep -v exits 1 when it filtered everything (nothing to report).
  if [ "$status" -eq 0 ]; then
    echo "FAIL: an EchoLink socket is opened outside echolink/net.rs (route it through net.rs so the loopback guard applies):"
    echo "$hits"
    fail=1
  fi
fi

# The EchoLink functions of astar-server's controller.rs. The rest of the
# file (and its non-EchoLink tests, which bind loopback UDP on purpose) is
# not EchoLink code, so only the bodies of `fn *echolink*` and `fn el_*`
# are read: from the `fn` line to the `}` at that line's own indent.
controller=crates/astar-server/src/controller.rs
if [ ! -f "$controller" ]; then
  echo "FAIL: $controller is missing, so its EchoLink socket check cannot run"; fail=1
else
  set +e
  # The pattern goes in through the environment: `awk -v` would eat its
  # backslashes.
  hits=$(SOCKET_RE="$socket_re" awk -v file="$controller" '
    BEGIN { re = ENVIRON["SOCKET_RE"] }
    !inside && match($0, /^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?fn[[:space:]]+([a-z0-9_]*echolink[a-z0-9_]*|el_[a-z0-9_]+)[<(]/) {
      inside = 1; seen = 1
      match($0, /^[[:space:]]*/); indent = substr($0, 1, RLENGTH)
    }
    inside && $0 ~ re { printf "%s:%d:%s\n", file, NR, $0; bad = 1 }
    inside && $0 == indent "}" { inside = 0 }
    END { if (!seen) { print "no EchoLink fn found in " file; exit 2 } exit bad }
  ' "$controller" 2>&1)
  status=$?
  set -e
  if [ "$status" -eq 1 ]; then
    echo "FAIL: an EchoLink function in controller.rs opens a socket (route it through net.rs so the loopback guard applies):"
    echo "$hits"
    fail=1
  elif [ "$status" -ge 2 ]; then
    echo "FAIL: the controller.rs socket scan could not run:"
    echo "$hits"
    fail=1
  fi
fi

# --- 3a. Fast line check on every Cargo.toml ------------------------------
# Comments are stripped first. Within [features], a feature's own key is not
# a use, and a chain is allowed when its key is itself test-only.
check_manifest() {
  awk -v file="$1" '
    function report(what) { printf "%s:%d: %s: %s\n", file, NR, what, raw; bad = 1 }
    {
      raw = $0
      line = $0
      sub(/#.*/, "", line)
    }
    line ~ /^[[:space:]]*\[/ {
      section = line
      gsub(/[[:space:]]/, "", section)
      key = ""
      next
    }
    {
      is_dev = (section ~ /dev-dependencies\]$/)
      is_deps = (section ~ /dependencies\]$/ || section ~ /dependencies\./) && !is_dev
      is_features = (section == "[features]")
      value = line
      if (is_features && line ~ /^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=/) {
        key = line
        sub(/^[[:space:]]*/, "", key)
        sub(/[[:space:]]*=.*/, "", key)
        sub(/^[^=]*=/, "", value)
      }
      if (value ~ /loopback-fakes|echolink-loopback-only/) {
        if (is_features && key == "default") report("test-only feature in a default list")
        else if (is_features && key != "echolink-loopback-only" && key != "test-support" && key != "loopback-fakes")
          report("test-only feature switched on by a feature that is not test-only")
        else if (is_deps) report("test-only feature on a non-dev dependency")
      }
    }
    END { exit bad }
  ' "$1"
}

while IFS= read -r manifest; do
  if ! check_manifest "$manifest"; then
    fail=1
  fi
done < <(git ls-files -- 'Cargo.toml' '*/Cargo.toml')

# --- 3b. Reachability: what cargo actually enables -------------------------
# No dev edges, so dev-dependencies (and the features they unify in) are left
# out; default features; every target, so a platform-only dependency counts.
members=$(cargo metadata --no-deps --format-version 1 --offline 2>/dev/null |
  python3 -c 'import json,sys; print("\n".join(p["name"] for p in json.load(sys.stdin)["packages"]))') || {
  echo "FAIL: cargo metadata errored"; exit 1
}
for m in $members; do
  # `{f}` lists each package's enabled features. (A feature-edge tree, `-e
  # features`, leaves out features the root's own features switch on.)
  if ! tree=$(cargo tree -p "$m" -e normal,build --target all --prefix none -f '{p} [{f}]' 2>&1); then
    echo "FAIL: cargo tree -p $m errored"; echo "$tree"; fail=1; continue
  fi
  if hits=$(grep -E '\[([^]]*,)?(loopback-fakes|echolink-loopback-only)(,[^]]*)?\]$' <<<"$tree" | sort -u); then
    echo "FAIL: a normal build of $m enables a test-only EchoLink feature:"
    echo "$hits"
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "FAIL: echolink guard"
  exit 1
fi
echo "echolink guard: ok"
