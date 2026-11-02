#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Proves the discord feature is genuinely optional: a default-features build
# of the workspace must link neither tokio nor songbird. astar-server ships
# to operators who never configure a Discord channel, and this is what keeps
# that build free of an async runtime and a reverse-engineered voice stack.
set -euo pipefail

fail=0

# 1. The node daemon and the engine crates stay free of an async runtime.
#    (astar-gui legitimately has tokio via iced_futures; it is not a node.)
echo "==> tokio must not reach the daemon or the engine"
for crate in astar-server astar-iax astar-audio astar-station astar-discord; do
  if ! output=$(cargo tree -p "$crate" --edges normal --prefix none 2>&1); then
    echo "FAIL: 'cargo tree -p ${crate}' errored — crate name wrong or missing?"
    echo "$output"
    fail=1
    continue
  fi
  if grep -qE "^tokio v" <<<"$output"; then
    echo "FAIL: tokio is in a default build of ${crate}"
    fail=1
  fi
done

# 2. songbird and serenity must appear NOWHERE by default.
echo "==> songbird/serenity must be absent workspace-wide"
if ! tree=$(cargo tree --workspace --edges normal --prefix none 2>&1); then
  echo "FAIL: 'cargo tree --workspace' errored"
  echo "$tree"
  fail=1
else
  for forbidden in songbird serenity; do
    if grep -qE "^${forbidden} v" <<<"$tree"; then
      echo "FAIL: '${forbidden}' is in a default-features build"
      grep -E "^${forbidden} v" <<<"$tree" | head -3
      fail=1
    fi
  done
fi

# 3. The public page's ACME stack is opt-in (public-tls; rustls and ureq are
#    already here through astar-asl3), and synchronous even
#    when it is on: the daemon gains certificates without gaining a runtime.
echo "==> public-tls: no TLS stack by default, and no tokio with it"
if ! output=$(cargo tree -p astar-server --edges normal --prefix none 2>&1); then
  echo "FAIL: 'cargo tree -p astar-server' errored"
  fail=1
else
  for forbidden in rcgen instant-acme; do
    if grep -qE "^${forbidden} v" <<<"$output"; then
      echo "FAIL: '${forbidden}' is in a default build of astar-server (public-tls must stay opt-in)"
      fail=1
    fi
  done
fi
if ! output=$(cargo tree -p astar-server --edges normal --prefix none --features public-tls 2>&1); then
  echo "FAIL: 'cargo tree -p astar-server --features public-tls' errored"
  fail=1
elif grep -qE "^(tokio|instant-acme) v" <<<"$output"; then
  echo "FAIL: public-tls pulls an async runtime into astar-server"
  fail=1
fi

# 4. auth (passkey sign-in) is opt-in too, and as synchronous as public-tls:
#    webauthn-rs and OpenSSL, no runtime.
echo "==> auth: opt-in, and no tokio with it"
if ! output=$(cargo tree -p astar-server --edges normal --prefix none 2>&1); then
  echo "FAIL: 'cargo tree -p astar-server' errored"
  fail=1
elif grep -qE "^(webauthn-rs|openssl-sys) v" <<<"$output"; then
  echo "FAIL: a default build of astar-server pulls in webauthn-rs/OpenSSL (auth must stay opt-in)"
  fail=1
fi
if ! output=$(cargo tree -p astar-server --edges normal --prefix none --features auth 2>&1); then
  echo "FAIL: 'cargo tree -p astar-server --features auth' errored"
  fail=1
elif grep -qE "^tokio v" <<<"$output"; then
  echo "FAIL: auth pulls an async runtime into astar-server"
  fail=1
elif ! grep -qE "^webauthn-rs v" <<<"$output"; then
  echo "FAIL: astar-server --features auth does not pull in webauthn-rs"
  fail=1
fi

# 5. dns-providers (dns-update, on one contained tokio thread) is opt-in, and
#    in a release build tokio arrives only through it and discord.
echo "==> dns-providers: off by default; tokio in release only via discord/dns-providers"
if ! output=$(cargo tree -p astar-server --edges normal --prefix none 2>&1); then
  echo "FAIL: 'cargo tree -p astar-server' errored"; fail=1
elif grep -qE "^dns-update v" <<<"$output"; then
  echo "FAIL: dns-update is in a default build of astar-server (dns-providers must stay opt-in)"; fail=1
fi
if ! output=$(cargo tree -p astar-server --edges normal --prefix none --no-default-features \
      --features stream,stream-mp3,codec2-static,public-tls,auth 2>&1); then
  echo "FAIL: 'cargo tree' of the release set minus discord and dns-providers errored"; fail=1
elif grep -qE "^tokio v" <<<"$output"; then
  echo "FAIL: tokio reaches a release build by a path other than discord or dns-providers"; fail=1
fi
if ! output=$(cargo tree -p astar-server --edges normal --prefix none --features dns-providers 2>&1); then
  echo "FAIL: 'cargo tree -p astar-server --features dns-providers' errored"; fail=1
elif ! grep -qE "^dns-update v" <<<"$output"; then
  echo "FAIL: --features dns-providers does not pull in dns-update"; fail=1
elif grep -qE "^(aws-lc-rs|aws-lc-sys) v" <<<"$output"; then
  echo "FAIL: dns-update pulls aws-lc-rs; it must be built with features = [\"ring\"] only"; fail=1
fi
if ! grep -qE '^release = \[.*"dns-providers"' crates/astar-server/Cargo.toml; then
  echo "FAIL: release does not list dns-providers"; fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "The discord, public-tls, auth and dns-providers features must stay off by default."
  exit 1
fi

echo "OK: daemon and engine are tokio-free; no songbird or serenity anywhere; public-tls and auth opt-in and sync; dns-providers opt-in"
