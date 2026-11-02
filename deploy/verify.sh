#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# verify.sh — post-deploy verification of astar-server on the node VPS.
#
#     ASTAR_VPS=allstar@<host> deploy/verify.sh            # box checks
#     ASTAR_PUBLIC_HOST=<name> ...  deploy/verify.sh       # + public checks
#     deploy/verify.sh --json                              # machine output
#
# OBSERVATION ONLY. Over ssh it runs read-only commands (podman inspect /
# images / logs, df, ss, and a GET of the loopback /status). Publicly it sends
# GETs, plus one POST to `/` that the public router must refuse with 405. It
# never keys, links, unlinks, restarts, prunes, writes a file, or POSTs to any
# control route. Keep it that way: deploy-vps.sh runs it against a live node.
#
# Why bash: it sits beside build.sh / deploy-vps.sh / ship-base.sh, runs from
# the Mac or a runner with nothing to compile, and every check is an ssh or
# curl call. python3 (already needed by `just ci`) parses JSON and dates;
# openssl reads the certificate's expiry. Written for bash 3.2 (stock macOS):
# no associative arrays, no mapfile.
#
# Secret hygiene: the log scan runs ON THE BOX and sends back only line
# numbers. The node secret and other credential values are read from the
# config directory on the box and matched there with `grep -F`; no value ever
# leaves the box or reaches this script's output.
#
# One line per check: PASS / FAIL / SKIP. Exit 0 when nothing failed, 1 on any
# FAIL, 2 on a usage error.
#
# Knobs (all optional except ASTAR_VPS):
#   ASTAR_VPS            user@host of the node box (root@ or allstar@ both work)
#   ASTAR_PUBLIC_HOST    public hostname; unset = public checks are SKIPped
#   ASTAR_DEPLOYED_AFTER epoch seconds (box clock) the container must have
#                        started after; deploy-vps.sh sets it
#   ASTAR_CONTAINER      container name                  [iaxnode]
#   ASTAR_EXPECT_IMAGE   image the container must run    [localhost/iaxclient-node:latest]
#   ASTAR_NODE_USER      user owning the rootless podman [allstar]
#   ASTAR_NODE_CONF      node config dir on the box      [/home/allstar/iaxnode]
#   ASTAR_CONTROL_PORT   loopback control port           [8730]
#   ASTAR_MAX_IMAGES     image-count ceiling             [6]
#   ASTAR_DISK_MAX       disk-use ceiling, percent       [80]
#   ASTAR_REGISTER_WAIT  seconds to wait for registered  [60]
#   ASTAR_POLL_INTERVAL  seconds between /status polls   [4]
#   ASTAR_CERT_MIN_DAYS  certificate days-left floor     [14]
#   ASTAR_VPS_IP         the box's public IPv4, when ASTAR_VPS is not an IP
# Test seams (deploy/test/test_verify.sh):
#   ASTAR_SSH            ssh command                     [ssh -o BatchMode=yes -o ConnectTimeout=10]
#   ASTAR_HTTPS_CONNECT  ip:port to dial for the host's :443 (curl --connect-to)
#   ASTAR_HTTP_CONNECT   ip:port to dial for the host's :80
#   ASTAR_CA_BUNDLE      CA file to trust instead of the system roots
set -uo pipefail

JSON=0
for arg in "$@"; do
  case "$arg" in
    --json) JSON=1 ;;
    -h | --help)
      sed -n '6,52p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "verify.sh: unknown argument '$arg' (try --help)" >&2
      exit 2
      ;;
  esac
done

VPS="${ASTAR_VPS:-}"
if [ -z "$VPS" ]; then
  echo "verify.sh: set ASTAR_VPS=user@host for your own server" >&2
  exit 2
fi
PUBLIC_HOST="${ASTAR_PUBLIC_HOST:-}"
CONTAINER="${ASTAR_CONTAINER:-iaxnode}"
IMAGE="${ASTAR_EXPECT_IMAGE:-localhost/iaxclient-node:latest}"
NODE_USER="${ASTAR_NODE_USER:-allstar}"
NODE_CONF="${ASTAR_NODE_CONF:-/home/allstar/iaxnode}"
CONTROL_PORT="${ASTAR_CONTROL_PORT:-8730}"
MAX_IMAGES="${ASTAR_MAX_IMAGES:-6}"
DISK_MAX="${ASTAR_DISK_MAX:-80}"
REGISTER_WAIT="${ASTAR_REGISTER_WAIT:-60}"
POLL="${ASTAR_POLL_INTERVAL:-4}"
DEPLOYED_AFTER="${ASTAR_DEPLOYED_AFTER:-}"
CERT_MIN_DAYS="${ASTAR_CERT_MIN_DAYS:-14}"
read -r -a SSH <<<"${ASTAR_SSH:-ssh -o BatchMode=yes -o ConnectTimeout=10}"

case "$NODE_USER" in
  *[!a-z0-9_-]* | "") echo "verify.sh: bad ASTAR_NODE_USER" >&2; exit 2 ;;
esac
for n in "$CONTROL_PORT" "$MAX_IMAGES" "$DISK_MAX" "$REGISTER_WAIT" "$POLL" "$CERT_MIN_DAYS" ${DEPLOYED_AFTER:+"$DEPLOYED_AFTER"}; do
  case "$n" in
    *[!0-9]* | "") echo "verify.sh: '$n' is not a whole number" >&2; exit 2 ;;
  esac
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/astar-verify.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
RESULTS="$WORK/results.tsv"
: >"$RESULTS"
FAILED=0

# record <check> <PASS|FAIL|SKIP> <detail>
record() {
  local detail
  detail="$(printf '%s' "$3" | tr '\t\n\r' '   ')"
  printf '%s\t%s\t%s\n' "$1" "$2" "$detail" >>"$RESULTS"
  [ "$2" = FAIL ] && FAILED=1
  [ "$JSON" = 1 ] || printf '%-4s  %-22s %s\n' "$2" "$1" "$detail"
}

py() { python3 -I -c "$@"; }

# ── box checks (over ssh) ───────────────────────────────────────────────────

# Run the script on stdin on the box as $NODE_USER: a root login drops to the
# service user (rootless podman lives there), a service-user login runs as is.
box_script() {
  "${SSH[@]}" "$VPS" \
    "if [ \"\$(id -u)\" = 0 ]; then exec su - $NODE_USER -s /bin/bash -c 'bash -s'; else exec bash -s; fi"
}

FACTS="$WORK/facts"
{
  printf 'C=%q\nIMG=%q\nCONF=%q\n' "$CONTAINER" "$IMAGE" "$NODE_CONF"
  cat <<'REMOTE'
# Read-only fact collector. Prints key=value lines; never prints log content
# or any credential value.
fact() { printf '%s=%s\n' "$1" "$2"; }
fact now "$(date +%s)"
insp="$(podman inspect -f '{{.State.Running}}|{{.State.StartedAt.Unix}}|{{.ImageName}}|{{.Image}}' "$C" 2>/dev/null)" || insp=missing
fact container "$insp"
fact tag_id "$(podman image inspect -f '{{.Id}}' "$IMG" 2>/dev/null || echo missing)"
fact images "$(podman images -q 2>/dev/null | wc -l | tr -d ' ')"
fact dangling "$(podman images -q -f dangling=true 2>/dev/null | wc -l | tr -d ' ')"
fact disk "$(df -P / | awk 'NR==2 { sub("%", "", $5); print $5 }')"
fact udp4569 "$(ss -Huln 2>/dev/null | awk '$4 ~ /:4569$/' | wc -l | tr -d ' ')"
logs="$(podman logs --tail 500 "$C" 2>&1 || true)"
fact log_lines "$(printf '%s\n' "$logs" | grep -c .)"
# Keyword shapes: Porkbun's apikey/secretapikey, tokens, and a `secret =` /
# `"secret":` assignment (the node-secret shape in a config dump).
pat='apikey|porkbun|token|secret[^[:alnum:]]{0,3}[=:]'
fact log_pattern_lines "$(printf '%s\n' "$logs" | grep -n -i -E "$pat" | cut -d: -f1 | paste -sd, -)"
# The actual credential values from the config dir, matched literally. Values
# shorter than 8 characters are skipped: too likely to match by accident.
vals=0
hits=""
for f in "$CONF/node.toml" "$CONF/credentials.toml" "$CONF/portal.env"; do
  [ -r "$f" ] || continue
  while IFS= read -r v; do
    [ "${#v}" -ge 8 ] || continue
    vals=$((vals + 1))
    h="$(printf '%s\n' "$logs" | grep -n -F -- "$v" | cut -d: -f1 | paste -sd, -)"
    [ -n "$h" ] && hits="${hits:+$hits,}$h"
  done <<EOF
$(grep -i -E '^[[:space:]]*(export[[:space:]]+)?[A-Za-z0-9_.]*(secret|pass|apikey|token)[A-Za-z0-9_.]*[[:space:]]*=' "$f" 2>/dev/null |
    sed -E 's/^[^=]*=[[:space:]]*//; s/[[:space:]]+#.*$//; s/^"(.*)"$/\1/; s/^'"'"'(.*)'"'"'$/\1/')
EOF
done
fact config_values "$vals"
fact log_value_lines "$hits"
fact end ok
REMOTE
} | box_script >"$FACTS" 2>"$WORK/facts.err"
ssh_rc=$?

fact() { sed -n "s/^$1=//p" "$FACTS" | head -n 1; }

ago() {
  local s=$1
  if [ "$s" -lt 120 ]; then echo "${s}s ago"
  elif [ "$s" -lt 7200 ]; then echo "$((s / 60)) min ago"
  elif [ "$s" -lt 172800 ]; then echo "$((s / 3600)) h ago"
  else echo "$((s / 86400)) days ago"; fi
}

BOX_CHECKS="container-running container-image container-restarted image-count disk status-registered status-listening log-secrets udp-4569"

if [ "$ssh_rc" -ne 0 ] || [ "$(fact end)" != ok ]; then
  err="$(head -n 1 "$WORK/facts.err" 2>/dev/null)"
  record box-ssh FAIL "could not collect facts from $VPS (ssh exit $ssh_rc${err:+: $err})"
  for c in $BOX_CHECKS; do record "$c" SKIP "box unreachable"; done
else
  record box-ssh PASS "$VPS"
  now="$(fact now)"

  # container running + image
  IFS='|' read -r running started img_name img_id <<<"$(fact container)"
  if [ "$running" = true ]; then
    record container-running PASS "$CONTAINER is running"
  elif [ "$running" = missing ]; then
    record container-running FAIL "no container named $CONTAINER"
  else
    record container-running FAIL "$CONTAINER exists but is not running"
  fi

  # The 2026-09-02 failure: a deploy built a fresh image under a tag nothing
  # used, restarted the OLD container, and reported success. Check both the
  # name the container was started from and that it runs the image the tag
  # points at NOW.
  tag_id="$(fact tag_id)"
  if [ "$running" = missing ]; then
    record container-image SKIP "no container"
  elif [ "$tag_id" = missing ]; then
    record container-image FAIL "$IMAGE does not exist on the box"
  elif [ "$img_name" != "$IMAGE" ]; then
    record container-image FAIL "container started from '$img_name', expected '$IMAGE'"
  elif [ "$img_id" != "$tag_id" ]; then
    record container-image FAIL "container runs image ${img_id:0:12} but $IMAGE is ${tag_id:0:12} (deploy built an image the container is not running)"
  else
    record container-image PASS "$IMAGE (${tag_id:0:12})"
  fi

  if [ "$running" = missing ] || [ -z "$started" ]; then
    record container-restarted SKIP "no container"
  elif [ -z "$DEPLOYED_AFTER" ]; then
    record container-restarted SKIP "no deploy timestamp (ASTAR_DEPLOYED_AFTER); started $(ago $((now - started)))"
  elif [ "$started" -ge "$DEPLOYED_AFTER" ]; then
    record container-restarted PASS "started $((started - DEPLOYED_AFTER))s after the deploy began"
  else
    record container-restarted FAIL "started $(ago $((now - started))), before the deploy began $(ago $((now - DEPLOYED_AFTER))): the restart did not happen"
  fi

  # Image count: about sixty untagged images filled the disk by 2026-09-27.
  images="$(fact images)"
  dangling="$(fact dangling)"
  if [ -z "$images" ]; then
    record image-count FAIL "podman images gave no answer"
  elif [ "$images" -le "$MAX_IMAGES" ]; then
    record image-count PASS "$images images ($dangling untagged), ceiling $MAX_IMAGES"
  else
    record image-count FAIL "$images images ($dangling untagged), ceiling $MAX_IMAGES: prune them (podman image prune -f)"
  fi

  disk="$(fact disk)"
  if [ -z "$disk" ]; then
    record disk FAIL "df gave no answer"
  elif [ "$disk" -lt "$DISK_MAX" ]; then
    record disk PASS "/ is ${disk}% used (ceiling ${DISK_MAX}%)"
  else
    record disk FAIL "/ is ${disk}% used (ceiling ${DISK_MAX}%)"
  fi

  # /status: registration is a round trip to AllStarLink and takes a few
  # seconds after a restart, so poll. `curl -sf` alone was never a gate:
  # /status answers 200 whatever the link state (2026-08-22..30).
  t0=$SECONDS
  reg=none
  lis=none
  while :; do
    body="$("${SSH[@]}" "$VPS" "curl -s --max-time 5 http://127.0.0.1:$CONTROL_PORT/status" </dev/null 2>/dev/null)"
    parsed="$(printf '%s' "$body" | py 'import json,sys
try:
    d = json.load(sys.stdin)
    print(str(d.get("registered") is True).lower(), str(d.get("listening") is True).lower())
except Exception:
    print("none none")' 2>/dev/null)"
    read -r reg lis <<<"${parsed:-none none}"
    if [ "$reg" = true ] && [ "$lis" = true ]; then break; fi
    if [ $((SECONDS - t0 + POLL)) -gt "$REGISTER_WAIT" ]; then break; fi
    sleep "$POLL"
  done
  waited=$((SECONDS - t0))
  case "$reg" in
    true) record status-registered PASS "registered: true (after ${waited}s)" ;;
    false) record status-registered FAIL "registered: false after ${waited}s: see the [register] comment in gh-runners' node.toml.j2" ;;
    *) record status-registered FAIL "no JSON from 127.0.0.1:$CONTROL_PORT/status after ${waited}s" ;;
  esac
  case "$lis" in
    true) record status-listening PASS "listening: true" ;;
    false) record status-listening FAIL "listening: false" ;;
    *) record status-listening FAIL "no JSON from 127.0.0.1:$CONTROL_PORT/status" ;;
  esac

  pat_lines="$(fact log_pattern_lines)"
  val_lines="$(fact log_value_lines)"
  scanned="$(fact log_lines) log lines, $(fact config_values) credential values checked"
  if [ -z "$pat_lines" ] && [ -z "$val_lines" ]; then
    record log-secrets PASS "$scanned"
  else
    msg="content withheld;"
    [ -n "$pat_lines" ] && msg="$msg keyword at lines $pat_lines"
    [ -n "$val_lines" ] && msg="$msg credential value at lines $val_lines"
    record log-secrets FAIL "$msg (of podman logs --tail 500 $CONTAINER; $scanned)"
  fi

  if [ "$(fact udp4569)" -ge 1 ] 2>/dev/null; then
    record udp-4569 PASS "UDP 4569 bound"
  else
    record udp-4569 FAIL "nothing bound to UDP 4569"
  fi
fi

# ── public checks ───────────────────────────────────────────────────────────

PUBLIC_CHECKS="dns tls-chain tls-expiry http-root public-status hidden:/key hidden:/link hidden:/secrets hidden:/settings hidden:/status post-405 wrong-host stream-mp3 http-80"

if [ -z "$PUBLIC_HOST" ]; then
  # One SKIP row per check (so --json always has the same shape), but one
  # line on the terminal.
  n=0
  for c in $PUBLIC_CHECKS; do
    printf '%s\tSKIP\tASTAR_PUBLIC_HOST not set\n' "$c" >>"$RESULTS"
    n=$((n + 1))
  done
  [ "$JSON" = 1 ] || printf '%-4s  %-22s %s\n' SKIP public "ASTAR_PUBLIC_HOST not set: $n public checks skipped"
else
  H="$PUBLIC_HOST"
  CURL=(curl -sS --max-time 10)
  [ -n "${ASTAR_HTTPS_CONNECT:-}" ] && CURL+=(--connect-to "$H:443:$ASTAR_HTTPS_CONNECT")
  [ -n "${ASTAR_CA_BUNDLE:-}" ] && CURL+=(--cacert "$ASTAR_CA_BUNDLE")
  BASE="https://$H"

  # get <method> <path> [curl args...] → "code|content-type"; body in $WORK/body
  get() {
    local m=$1 p=$2
    shift 2
    "${CURL[@]}" -X "$m" -o "$WORK/body" -w '%{http_code}|%{content_type}' "$@" "$BASE$p" 2>"$WORK/curl.err"
  }

  # DNS: A records for the host must be exactly the box's address.
  vps_host="${VPS#*@}"
  vps_ip="${ASTAR_VPS_IP:-$vps_host}"
  resolve() {
    py 'import socket,sys
try:
    print(" ".join(sorted({a[4][0] for a in socket.getaddrinfo(sys.argv[1], None, socket.AF_INET)})))
except Exception:
    pass' "$1"
  }
  a_records="$(resolve "$H")"
  want_ip="$(resolve "$vps_ip")"
  if [ -z "$a_records" ]; then
    record dns FAIL "$H has no A record"
  elif [ "$a_records" = "$want_ip" ]; then
    record dns PASS "$H A $a_records"
  else
    record dns FAIL "$H A ${a_records}, box is ${want_ip:-$vps_ip}"
  fi

  # TLS chain + name: curl verifies both by default; exit 0 means it held.
  if "${CURL[@]}" -o /dev/null "$BASE/" 2>"$WORK/tls.err"; then
    record tls-chain PASS "certificate chains to a trusted root and covers $H"
  else
    record tls-chain FAIL "$(head -n 1 "$WORK/tls.err")"
  fi
  enddate="$(openssl s_client -connect "${ASTAR_HTTPS_CONNECT:-$H:443}" -servername "$H" </dev/null 2>/dev/null |
    openssl x509 -noout -enddate 2>/dev/null | sed -n 's/^notAfter=//p')"
  days="$(py 'import sys,datetime
try:
    t = datetime.datetime.strptime(sys.argv[1], "%b %d %H:%M:%S %Y %Z")
    print(int((t - datetime.datetime.utcnow()).total_seconds() // 86400))
except Exception:
    pass' "$(printf '%s' "$enddate" | tr -s ' ')" 2>/dev/null)"
  if [ -z "$days" ]; then
    record tls-expiry FAIL "could not read the certificate's expiry"
  elif [ "$days" -gt "$CERT_MIN_DAYS" ]; then
    record tls-expiry PASS "$days days left (floor $CERT_MIN_DAYS)"
  else
    record tls-expiry FAIL "$days days left (floor $CERT_MIN_DAYS): renewal is not happening"
  fi

  r="$(get GET /)"
  if [ "${r%%|*}" = 200 ]; then
    record http-root PASS "GET / 200"
  else
    record http-root FAIL "GET / ${r%%|*} $(head -n 1 "$WORK/curl.err")"
  fi

  # /public/status: 200 JSON, and nothing in it that names an address. The
  # walk refuses address-ish keys and any value shaped like an IPv4 or
  # host:port, wherever it sits.
  r="$(get GET /public/status)"
  code="${r%%|*}"
  ctype="${r#*|}"
  if [ "$code" != 200 ]; then
    record public-status FAIL "GET /public/status $code"
  else
    leak="$(py 'import json,re,sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print("not JSON"); sys.exit()
bad_key = re.compile(r"^(addr|address|ip|ipv4|ipv6|port|host|peer|remote|endpoint)$|_(addr|address|ip|port|host)$|^(addr|ip|port|host)_", re.I)
ipv4 = re.compile(r"(^|[^0-9.])(\d{1,3}\.){3}\d{1,3}($|[^0-9.])")
hostport = re.compile(r"^[\w.\-\[\]:]+:\d{2,5}$")
found = []
def walk(v, path):
    if isinstance(v, dict):
        for k, x in v.items():
            if bad_key.search(str(k)):
                found.append(path + "." + str(k))
            walk(x, path + "." + str(k))
    elif isinstance(v, list):
        for i, x in enumerate(v):
            walk(x, "%s[%d]" % (path, i))
    elif isinstance(v, str) and (ipv4.search(v) or hostport.match(v)):
        found.append(path + " (address-shaped value)")
walk(d, "$")
print(", ".join(found[:5]))' "$WORK/body" 2>/dev/null)"
    case "$ctype" in
      application/json*) ;;
      *) leak="${leak:+$leak; }content-type '$ctype'" ;;
    esac
    if [ -z "$leak" ]; then
      record public-status PASS "200 JSON, no address fields"
    else
      record public-status FAIL "$leak"
    fi
  fi

  # Admin routes must not exist publicly. GET only: never send a control verb.
  for p in /key /link /secrets /settings /status; do
    r="$(get GET "$p")"
    if [ "${r%%|*}" = 404 ]; then
      record "hidden:$p" PASS "GET $p 404"
    else
      record "hidden:$p" FAIL "GET $p ${r%%|*} (must be 404 on the public bind)"
    fi
  done

  r="$(get POST / --data '')"
  if [ "${r%%|*}" = 405 ]; then
    record post-405 PASS "POST / 405"
  else
    record post-405 FAIL "POST / ${r%%|*} (expected 405)"
  fi

  r="$(get GET / -H 'Host: verify-wrong-host.invalid')"
  code="${r%%|*}"
  case "$code" in
    4??) record wrong-host PASS "foreign Host refused with $code" ;;
    000) record wrong-host PASS "foreign Host refused at the connection" ;;
    *) record wrong-host FAIL "foreign Host answered $code (DNS-rebinding guard missing)" ;;
  esac

  # /stream.mp3: read at most 64 KB, then hang up.
  "${CURL[@]}" -N --max-time 8 -D "$WORK/stream.hdr" -o - "$BASE/stream.mp3" 2>/dev/null |
    head -c 65536 >"$WORK/stream.body"
  scode="$(head -n 1 "$WORK/stream.hdr" 2>/dev/null | awk '{ print $2 }')"
  sctype="$(grep -i '^content-type:' "$WORK/stream.hdr" 2>/dev/null | tail -n 1 | cut -d: -f2- | tr -d ' \r')"
  sbytes="$(wc -c <"$WORK/stream.body" | tr -d ' ')"
  case "$scode" in
    200)
      case "$sctype" in
        audio/mpeg*)
          if [ "$sbytes" -gt 0 ]; then
            record stream-mp3 PASS "200 audio/mpeg, read $sbytes bytes"
          else
            record stream-mp3 FAIL "200 audio/mpeg but no bytes in 8s"
          fi
          ;;
        *) record stream-mp3 FAIL "200 with content-type '$sctype', expected audio/mpeg" ;;
      esac
      ;;
    503)
      reason="$(head -n 1 "$WORK/stream.body" | tr -cd '[:print:]' | cut -c1-100)"
      if [ -n "$reason" ]; then
        record stream-mp3 PASS "503: $reason"
      else
        record stream-mp3 FAIL "503 with no reason"
      fi
      ;;
    *) record stream-mp3 FAIL "GET /stream.mp3 ${scode:-no answer}" ;;
  esac

  # Port 80: a redirect to https, or nothing listening. Never plain content.
  CURL80=(curl -sS --max-time 5 -o /dev/null -w '%{http_code}|%{redirect_url}')
  [ -n "${ASTAR_HTTP_CONNECT:-}" ] && CURL80+=(--connect-to "$H:80:$ASTAR_HTTP_CONNECT")
  r="$("${CURL80[@]}" "http://$H/" 2>/dev/null)"
  rc=$?
  code="${r%%|*}"
  loc="${r#*|}"
  if [ "$rc" = 7 ]; then
    record http-80 PASS "port 80 not listening"
  elif [ "$rc" = 28 ]; then
    record http-80 PASS "port 80 does not answer (filtered)"
  else
    case "$code" in
      301 | 302 | 307 | 308)
        case "$loc" in
          "https://$H"*) record http-80 PASS "$code to $loc" ;;
          *) record http-80 FAIL "$code to '$loc', expected https://$H/" ;;
        esac
        ;;
      *) record http-80 FAIL "port 80 answered ${code:-?} (expected a redirect to https)" ;;
    esac
  fi
fi

# ── output ──────────────────────────────────────────────────────────────────

if [ "$JSON" = 1 ]; then
  py 'import json,sys
rows = [l.rstrip("\n").split("\t", 2) for l in open(sys.argv[1]) if l.strip()]
checks = [{"check": c, "status": s, "detail": d} for c, s, d in rows]
print(json.dumps({"ok": not any(c["status"] == "FAIL" for c in checks),
                  "vps": sys.argv[2], "public_host": sys.argv[3] or None,
                  "checks": checks}, indent=2))' "$RESULTS" "$VPS" "$PUBLIC_HOST"
else
  pass=$(grep -c "	PASS	" "$RESULTS")
  fail=$(grep -c "	FAIL	" "$RESULTS")
  skip=$(grep -c "	SKIP	" "$RESULTS")
  echo "verify: $pass passed, $fail failed, $skip skipped"
fi
exit "$FAILED"
