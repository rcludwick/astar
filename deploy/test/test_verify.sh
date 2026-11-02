#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# test_verify.sh — tests for deploy/verify.sh, with no network.
#
#     deploy/test/test_verify.sh        (or `just deploy-verify-test`)
#
# The box is fake-ssh: it runs verify.sh's remote commands locally, with
# fakebox/ (podman, df, ss, curl, id) answering canned output steered by
# FAKE_* variables. The public bind is fake_server.py on 127.0.0.1, behind a
# throwaway CA made here with openssl, reached through verify.sh's
# ASTAR_HTTPS_CONNECT / ASTAR_HTTP_CONNECT / ASTAR_CA_BUNDLE seams. Nothing
# leaves loopback. Needs bash, python3, openssl and curl.
#
# Each case runs verify.sh once and pins the exit code and the PASS/FAIL/SKIP
# of the checks it is about — every gate has a case that makes it FAIL.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERIFY="$HERE/../verify.sh"
T="$(mktemp -d "${TMPDIR:-/tmp}/astar-verify-test.XXXXXX")"
PIDS=""
cleanup() {
  for p in $PIDS; do kill "$p" 2>/dev/null; done
  rm -rf "$T"
}
trap cleanup EXIT

fails=0
cases=0
OUT=""
RC=0

# ── fixtures ────────────────────────────────────────────────────────────────

CONF="$T/conf"
mkdir -p "$CONF"
cat >"$CONF/node.toml" <<'EOF'
[register]
node = "1999"
[secrets]
source = "config"
secret = "nodesecret-CANARY-1234"   # the node secret
EOF
printf 'ASL_PORTAL_USER=someone\nASL_PORTAL_PASS="portalpass-CANARY-5678"\n' >"$CONF/portal.env"

printf 'INFO astar-server starting\nINFO registered with 1999\nINFO link up 2000\n' >"$T/log-clean"
{
  cat "$T/log-clean"
  echo 'DEBUG porkbun: apikey=pk1_LEAKED-KEYWORD-CANARY'
} >"$T/log-keyword"
{
  cat "$T/log-clean"
  echo 'DEBUG dialing with nodesecret-CANARY-1234'
} >"$T/log-value"

# A throwaway CA and two leaf certificates for "localhost": one healthy, one
# about to expire. Apple's TLS stack wants serverAuth EKU and a short life.
pki() {
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$T/ca.key" -out "$T/ca.pem" \
    -days 30 -subj "/CN=astar verify test CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
  printf 'subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' >"$T/leaf.ext"
  for leaf in good:90 expiring:5; do
    name="${leaf%%:*}"
    days="${leaf#*:}"
    openssl req -newkey rsa:2048 -nodes -keyout "$T/$name.key" -out "$T/$name.csr" -subj "/CN=localhost"
    openssl x509 -req -in "$T/$name.csr" -CA "$T/ca.pem" -CAkey "$T/ca.key" -CAcreateserial \
      -out "$T/$name.pem" -days "$days" -extfile "$T/leaf.ext"
  done
}
if ! pki >"$T/pki.log" 2>&1; then
  cat "$T/pki.log" >&2
  echo "test_verify: openssl could not make the test CA" >&2
  exit 1
fi

# start_server <cert-name> → sets HTTPS_PORT, HTTP_PORT
start_server() {
  local ports="$T/ports.$1"
  python3 "$HERE/fake_server.py" "$T/$1.pem" "$T/$1.key" "$T/mode" "$ports" &
  PIDS="$PIDS $!"
  for _ in $(seq 1 100); do
    [ -s "$ports" ] && break
    sleep 0.1
  done
  [ -s "$ports" ] || { echo "test_verify: fake server did not start" >&2; exit 1; }
  read -r HTTPS_PORT HTTP_PORT <"$ports"
}
: >"$T/mode"
start_server good
GOOD_HTTPS=$HTTPS_PORT
GOOD_HTTP=$HTTP_PORT
start_server expiring
EXP_HTTPS=$HTTPS_PORT
CLOSED_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"

# ── harness ─────────────────────────────────────────────────────────────────

BOX_ENV=(
  ASTAR_VPS=fake@127.0.0.1
  "ASTAR_SSH=$HERE/fake-ssh"
  "ASTAR_NODE_CONF=$CONF"
  ASTAR_REGISTER_WAIT=2
  ASTAR_POLL_INTERVAL=1
  "FAKE_LOG=$T/log-clean"
)
PUB_ENV=(
  ASTAR_PUBLIC_HOST=localhost
  ASTAR_VPS_IP=127.0.0.1
  "ASTAR_HTTPS_CONNECT=127.0.0.1:$GOOD_HTTPS"
  "ASTAR_HTTP_CONNECT=127.0.0.1:$GOOD_HTTP"
  "ASTAR_CA_BUNDLE=$T/ca.pem"
)

CASE=""
# run <case name> <mode> [VAR=value ...] [-- verify args]
run() {
  CASE=$1
  printf '%s\n' "$2" >"$T/mode"
  shift 2
  local envs=() vargs=()
  while [ $# -gt 0 ]; do
    if [ "$1" = -- ]; then shift; vargs=("$@"); break; fi
    envs+=("$1")
    shift
  done
  cases=$((cases + 1))
  rm -f "$T/counter"
  OUT="$(env -i PATH="$PATH" HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}" FAKE_COUNTER="$T/counter" \
    "${BOX_ENV[@]}" ${envs[@]+"${envs[@]}"} "$VERIFY" ${vargs[@]+"${vargs[@]}"} 2>&1)"
  RC=$?
}
bad() {
  fails=$((fails + 1))
  printf 'FAIL [%s] %s\n' "$CASE" "$1"
  printf '%s\n' "$OUT" | sed 's/^/    | /'
}
expect_rc() { [ "$RC" = "$1" ] || bad "exit $RC, expected $1"; }
# expect <check> <PASS|FAIL|SKIP>
expect() {
  printf '%s\n' "$OUT" | awk -v s="$2" -v c="$1" '$1 == s && $2 == c { f = 1 } END { exit !f }' ||
    bad "expected $2 $1"
}
expect_absent() {
  case "$OUT" in *"$1"*) bad "output contains '$1'" ;; esac
}

BOX_CHECKS="box-ssh container-running container-image image-count disk status-registered status-listening log-secrets udp-4569"
PUBLIC_CHECKS="dns tls-chain tls-expiry http-root public-status hidden:/key hidden:/link hidden:/secrets hidden:/settings hidden:/status post-405 wrong-host stream-mp3 http-80"

# ── box cases ───────────────────────────────────────────────────────────────

run "healthy box" ""
expect_rc 0
for c in $BOX_CHECKS; do expect "$c" PASS; done
expect container-restarted SKIP
expect public SKIP

run "restarted after the deploy" "" ASTAR_DEPLOYED_AFTER=$(($(date +%s) - 100))
expect_rc 0
expect container-restarted PASS

run "not restarted (old container still up)" "" \
  ASTAR_DEPLOYED_AFTER=$(($(date +%s) - 100)) FAKE_STARTED=$(($(date +%s) - 86400 * 40))
expect_rc 1
expect container-restarted FAIL

run "image tag mismatch (2026-09-02)" "" FAKE_TAG_ID=ffff9999eeee8888
expect_rc 1
expect container-image FAIL

run "container started from another tag" "" FAKE_IMAGE_NAME=localhost/astar-server:latest
expect_rc 1
expect container-image FAIL

run "expected image missing" "" FAKE_TAG_ID=
expect_rc 1
expect container-image FAIL

run "container stopped" "" FAKE_RUNNING=false
expect_rc 1
expect container-running FAIL

run "no container" "" FAKE_CONTAINER=no
expect_rc 1
expect container-running FAIL
expect container-image SKIP

run "too many images (disk-full incident)" "" FAKE_IMAGES=60 FAKE_DANGLING=57
expect_rc 1
expect image-count FAIL

run "disk over 80%" "" FAKE_DISK=85
expect_rc 1
expect disk FAIL

run "never registers" "" 'FAKE_STATUS={"listening":true,"registered":false}'
expect_rc 1
expect status-registered FAIL
expect status-listening PASS

run "registers on the third poll" "" ASTAR_REGISTER_WAIT=10 FAKE_STATUS_AFTER=2 \
  'FAKE_STATUS_EARLY={"listening":true,"registered":false}'
expect_rc 0
expect status-registered PASS
[ "$(cat "$T/counter" 2>/dev/null)" = 3 ] || bad "expected 3 polls, saw $(cat "$T/counter" 2>/dev/null)"

run "not listening" "" 'FAKE_STATUS={"listening":false,"registered":true}'
expect_rc 1
expect status-listening FAIL

run "control port dead" "" FAKE_STATUS=
expect_rc 1
expect status-registered FAIL
expect status-listening FAIL

run "secret keyword in the log" "" "FAKE_LOG=$T/log-keyword"
expect_rc 1
expect log-secrets FAIL
expect_absent "LEAKED-KEYWORD-CANARY"
case "$OUT" in *"lines 4"*) ;; *) bad "expected the matched line number (4)" ;; esac

run "node secret value in the log" "" "FAKE_LOG=$T/log-value"
expect_rc 1
expect log-secrets FAIL
expect_absent "nodesecret-CANARY"
expect_absent "portalpass-CANARY"

run "credential values are read from the config" ""
case "$OUT" in *"2 credential values checked"*) ;; *) bad "expected 2 credential values checked" ;; esac
expect_absent "CANARY"

run "UDP 4569 not bound" "" FAKE_UDP=no
expect_rc 1
expect udp-4569 FAIL

run "box unreachable" "" FAKE_SSH_FAIL=1
expect_rc 1
expect box-ssh FAIL
expect disk SKIP

run "--json" "" -- --json
expect_rc 0
printf '%s' "$OUT" | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["ok"] is True, d
s = {c["check"]: c["status"] for c in d["checks"]}
assert s["container-image"] == "PASS" and s["dns"] == "SKIP", s
assert len(d["checks"]) == 24, len(d["checks"])' 2>/dev/null || bad "--json output wrong"

run "--json on failure" "" FAKE_DISK=95 -- --json
expect_rc 1
printf '%s' "$OUT" | python3 -c 'import json,sys
d = json.load(sys.stdin)
assert d["ok"] is False and {c["check"]: c["status"] for c in d["checks"]}["disk"] == "FAIL"' 2>/dev/null ||
  bad "--json failure output wrong"

run "bad argument" "" -- --frobnicate
expect_rc 2

# ── public cases ────────────────────────────────────────────────────────────

run "healthy public bind" "" "${PUB_ENV[@]}"
expect_rc 0
for c in $PUBLIC_CHECKS; do expect "$c" PASS; done

run "/key answers 200 publicly" "key200" "${PUB_ENV[@]}"
expect_rc 1
expect hidden:/key FAIL
expect hidden:/link PASS

run "/public/status leaks an address" "leakaddr" "${PUB_ENV[@]}"
expect_rc 1
expect public-status FAIL

run "POST / accepted" "post200" "${PUB_ENV[@]}"
expect_rc 1
expect post-405 FAIL

run "no Host allow-list" "anyhost" "${PUB_ENV[@]}"
expect_rc 1
expect wrong-host FAIL

run "stream full (503 with a reason)" "stream503" "${PUB_ENV[@]}"
expect_rc 0
expect stream-mp3 PASS

run "stream serves html" "streamhtml" "${PUB_ENV[@]}"
expect_rc 1
expect stream-mp3 FAIL

run "port 80 serves plaintext" "http200" "${PUB_ENV[@]}"
expect_rc 1
expect http-80 FAIL

run "port 80 not listening" "" "${PUB_ENV[@]}" "ASTAR_HTTP_CONNECT=127.0.0.1:$CLOSED_PORT"
expect_rc 0
expect http-80 PASS

run "untrusted certificate" "" "${PUB_ENV[@]}" ASTAR_CA_BUNDLE=
expect_rc 1
expect tls-chain FAIL

run "certificate expiring in 5 days" "" "${PUB_ENV[@]}" "ASTAR_HTTPS_CONNECT=127.0.0.1:$EXP_HTTPS"
expect_rc 1
expect tls-chain PASS
expect tls-expiry FAIL

run "DNS points elsewhere" "" "${PUB_ENV[@]}" ASTAR_VPS_IP=192.0.2.10
expect_rc 1
expect dns FAIL

echo
if [ "$fails" -eq 0 ]; then
  echo "✓ test_verify: $cases cases passed"
else
  echo "✗ test_verify: $fails failure(s) across $cases cases"
  exit 1
fi
