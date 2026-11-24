#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Merge invariant for the software P25 IMBE vocoder.
#
# vendor/imbe-vocoder is a Rust port of Pavel Yazev's fixed-point IMBE vocoder
# from OP25, GPL-3.0-or-later. Rob, 2026-11-21: P25 Phase 1 uses it and nothing
# else (no DVSI stick astar supports can do P25 full rate), the macOS app and
# the iPhone build carry it, and astar-server and astar-netrec do not yet.
# Each rule below is one careless edit away from vanishing with the build
# still green:
#
#   1. The cargo feature stays EXPLICIT, never a cargo default: a plain
#      `cargo build` of any astar crate links no GPL code.
#   2. The opt-in works: `p25-soft-imbe` pulls the vocoder at every level, and
#      the network feature `p25` brings it along.
#   3. Shipped where intended, and only there: build-xcframework.sh turns `p25`
#      on unless ASTAR_P25_SOFT_IMBE=0; astar-server's and astar-netrec's
#      `release` sets do not carry it (backlog `astar-server-p25` flips this
#      when it lands).
#   4. Its licence is hard-coded, never inherited from the AGPL workspace, and
#      the GPL text ships beside it.
#   5. No AMBE+2. P25 Phase 2 (and DMR, NXDN, System Fusion) is AMBE+2, which
#      reads on US 8,359,197 until 2028-05-20: nothing in the island is named
#      for it or mentions it.
#   6. Only P25 Phase 1 reaches it: the island is named in one engine file,
#      and only P25 code uses that file.
#   7. No upstream C++ is committed; the port's oracle is the golden vectors.
#   8. No encryption (Rob, 2026-11-21): neither the island nor astar-p25 links
#      a crypto crate, and no P25 code decrypts anything. Encrypted calls are
#      muted from the ALGID, never decrypted.
#
# Checks 1-3 and 8 read `cargo tree`'s resolved graph (normal edges only), not
# Cargo.toml text, so feature unification through any path is caught.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VENDOR=vendor/imbe-vocoder
DEFAULT_CLEAN=(astar-codec astar-console astar-station astar-sys astar-server astar-netrec astar-cli astar-gui)
LINE='^imbe-vocoder v'

fail=0

# 1. Off by default.
for crate in "${DEFAULT_CLEAN[@]}"; do
  if ! output=$(cargo tree -p "$crate" -e normal --prefix none 2>&1); then
    echo "FAIL: 'cargo tree -p $crate' errored — crate name wrong or missing?" >&2
    echo "$output" >&2
    fail=1
    continue
  fi
  if grep -qE "$LINE" <<<"$output"; then
    echo "FAIL: a plain cargo build of $crate pulls in the software IMBE vocoder (GPL)." >&2
    echo "      p25-soft-imbe stays an explicit cargo feature that shipped builds name;" >&2
    echo "      see crates/astar-codec/Cargo.toml and LICENSE-EXCEPTIONS.md section 15." >&2
    fail=1
  fi
done

# 2. The opt-in works.
for crate in astar-codec astar-console astar-station astar-sys; do
  if ! cargo tree -p "$crate" -e normal --prefix none --features p25-soft-imbe 2>/dev/null |
    grep -qE "$LINE"; then
    echo "FAIL: $crate --features p25-soft-imbe does NOT pull in the IMBE vocoder." >&2
    fail=1
  fi
done
if ! cargo tree -p astar-sys -e normal --prefix none --features p25 2>/dev/null | grep -qE "$LINE"; then
  echo "FAIL: astar-sys --features p25 does NOT pull in the IMBE vocoder; p25 must require p25-soft-imbe." >&2
  fail=1
fi

# 3. Shipped where intended, and only there.
if ! grep -qE 'ASTAR_P25_SOFT_IMBE:-1\}' bindings/swift/build-xcframework.sh ||
  ! grep -qE -- '--features p25\)' bindings/swift/build-xcframework.sh; then
  echo "FAIL: bindings/swift/build-xcframework.sh no longer builds P25 in by default" >&2
  echo "      (ASTAR_P25_SOFT_IMBE defaults to 1 and adds --features p25)." >&2
  fail=1
fi
if cargo tree -p astar-server -e normal --prefix none --no-default-features --features release 2>/dev/null |
  grep -qE "$LINE"; then
  echo "FAIL: astar-server's release set pulls in the IMBE vocoder. P25 does not reach the" >&2
  echo "      server this round (backlog astar-server-p25); update this guard when it does." >&2
  fail=1
fi
if cargo tree -p astar-netrec -e normal --prefix none --features release 2>/dev/null |
  grep -qE "$LINE"; then
  echo "FAIL: astar-netrec's release set pulls in the IMBE vocoder; P25 does not reach it." >&2
  fail=1
fi

# 4. Licence hard-coded.
if ! grep -qE '^license = "GPL-3.0-or-later"$' "$VENDOR/Cargo.toml"; then
  echo "FAIL: $VENDOR/Cargo.toml must hard-code license = \"GPL-3.0-or-later\"." >&2
  fail=1
fi
if grep -vE '^[[:space:]]*#' "$VENDOR/Cargo.toml" | grep -nE 'workspace[[:space:]]*=[[:space:]]*true' >&2; then
  echo "FAIL: $VENDOR/Cargo.toml inherits workspace metadata; that would relicense it as AGPL." >&2
  fail=1
fi
if [ ! -f "$VENDOR/COPYING" ]; then
  echo "FAIL: $VENDOR/COPYING (the GPL-3.0 text) is missing." >&2
  fail=1
fi

# 5. No AMBE+2.
if [ -n "$(find "$VENDOR" -iname '*ambe*' -print -quit)" ]; then
  echo "FAIL: $VENDOR has a file named for AMBE:" >&2
  find "$VENDOR" -iname '*ambe*' >&2
  echo "      AMBE+2 (P25 Phase 2) is hardware-only (US 8,359,197 until 2028-05-20)." >&2
  fail=1
fi
if grep -rniE 'ambe|2450|3600x2450' "$VENDOR/src" >&2; then
  echo "FAIL: $VENDOR/src mentions AMBE or a half-rate (2450) mode. The island is" >&2
  echo "      P25 Phase 1 full-rate IMBE only." >&2
  fail=1
fi

# 6. Only P25 Phase 1 reaches it. --untracked so a new, not-yet-added file
#    counts too; ignored paths (target/) stay out.
users=$(git grep --untracked -l 'imbe_vocoder' -- 'crates/*.rs' 'apps/*.rs' || true)
if [ "$users" != "crates/astar-codec/src/p25.rs" ]; then
  echo "FAIL: the IMBE vocoder must be named only in crates/astar-codec/src/p25.rs; found:" >&2
  echo "${users:-<nowhere>}" >&2
  fail=1
fi
while IFS= read -r f; do
  [ -z "$f" ] && continue
  case "$f" in
    *p25*) ;;
    *)
      echo "FAIL: $f uses astar_codec::p25 but is not P25 code (no 'p25' in its path)." >&2
      fail=1
      ;;
  esac
done <<<"$(git grep --untracked -l 'astar_codec::p25' -- 'crates/*.rs' || true)"

# 7. No upstream C++ committed.
if git ls-files "$VENDOR" | grep -E '\.(cc|cpp|h)$' >&2; then
  echo "FAIL: C++ source is committed under $VENDOR. Only the golden vectors the" >&2
  echo "      tool writes belong here (see $VENDOR/VENDORED.md)." >&2
  fail=1
fi

# 8. No encryption.
CRYPTO='^(aes|des|rc4|cipher|ring|rustls|openssl|hmac|sha[0-9]|sha2|sha3|chacha|chacha20|salsa20|poly1305|rsa|blowfish|twofish|camellia|md5|aead|block-modes|ctr|ofb|cfb|cbc)[ -]'
# --all-features: a crypto crate behind any feature of either crate could be
# switched on by a dependent's feature and ship, so it counts too.
if ! island=$(cargo tree --manifest-path "$VENDOR/Cargo.toml" -p imbe-vocoder -e normal --prefix none --all-features 2>&1); then
  echo "FAIL: 'cargo tree' on $VENDOR errored:" >&2
  echo "$island" >&2
  fail=1
elif grep -E "$CRYPTO" <<<"$island" >&2; then
  echo "FAIL: the IMBE vocoder links a crypto crate. P25 in astar carries no cipher." >&2
  fail=1
fi
if ! proto=$(cargo tree -p astar-p25 -e normal --prefix none --all-features 2>&1); then
  echo "FAIL: 'cargo tree -p astar-p25' errored:" >&2
  echo "$proto" >&2
  fail=1
elif grep -E "$CRYPTO" <<<"$proto" >&2; then
  echo "FAIL: astar-p25 links a crypto crate. P25 in astar carries no cipher." >&2
  fail=1
fi
# Code only: a comment saying calls are "never decrypted" is fine, so a line
# loses any trailing `//` comment before it is matched, and a line that is all
# comment (`//`, `*`, `#`) is dropped. A pathspec that matches nothing is
# fine. `astar-console/src/session.rs` holds the P25 route and snapshot
# alongside every other network's; it is checked whole, since no network in
# astar decrypts anything. `astar-cli/src/p25_listen.rs` is the CLI listener.
decrypt_code=$(git grep --untracked -niE 'algid.*(decrypt|key)|decrypt' -- \
  crates/astar-p25 crates/astar-codec/src/p25.rs crates/astar-console/src/p25.rs \
  crates/astar-console/src/session.rs crates/astar-cli/src/p25_listen.rs \
  'crates/astar-station/*p25*' 'crates/astar-sys/*p25*' |
  awk '{ match($0, /^[^:]+:[0-9]+:/); loc = substr($0, 1, RLENGTH)
         code = substr($0, RLENGTH + 1); sub(/\/\/.*/, "", code)
         if (code ~ /^[[:space:]]*(\*|#)/) next
         if (tolower(code) ~ /algid.*(decrypt|key)|decrypt/) print loc code }' || true)
if [ -n "$decrypt_code" ]; then
  echo "$decrypt_code" >&2
  echo "FAIL: P25 code mentions decryption outside a comment. Encrypted calls are muted" >&2
  echo "      from the ALGID, never decrypted (LICENSE-EXCEPTIONS.md section 15)." >&2
  fail=1
fi

[ "$fail" -eq 0 ] || exit 1
echo "p25-imbe guard: explicit cargo feature, app-only shipping, GPL licence not inherited, no AMBE+2, P25 Phase 1 only, no C++, no crypto."
