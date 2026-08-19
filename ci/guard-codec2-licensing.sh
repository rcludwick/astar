#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# Merge invariant: no LGPL code in a DEFAULT build (astar-8c4d).
#
# Codec 2 is `LGPL-2.1-only AND MIT`. astar ships it deliberately, linked into
# the macOS app so M17 works on a Mac with no Homebrew libcodec2, and carries
# the notices and written offer that go with that in LICENSE-EXCEPTIONS.md.
#
# What must stay true is the OPT-IN. `codec2-static` / `codec2-runtime` must
# never reach a default feature set, so that a plain `cargo build` — here, or in
# anything that depends on these crates — links no LGPL code and needs no
# notices. That property is one careless `default = [...]` edit away from
# vanishing silently, and nothing else in CI would notice: the build would still
# be green, the tests would still pass, and the licensing obligation would just
# quietly attach to every downstream consumer.
#
# Checked against `cargo tree`'s resolved graph rather than by grepping
# Cargo.toml, so feature unification through any path is caught.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Crates a downstream consumer builds with defaults. astar-station and
# astar-console are the two that route M17, and astar-sys is what the Swift
# binding compiles.
DEFAULT_CLEAN=(astar-codec astar-console astar-station astar-sys)

fail=0
for crate in "${DEFAULT_CLEAN[@]}"; do
  if cargo tree -p "$crate" 2>/dev/null | grep -qE '^\|?[[:space:]|`+-]*codec2 v'; then
    echo "FAIL: a default build of $crate pulls in codec2 (LGPL)." >&2
    echo "      codec2-static/codec2-runtime must stay opt-in — see" >&2
    echo "      crates/astar-codec/Cargo.toml and LICENSE-EXCEPTIONS.md." >&2
    fail=1
  fi
done

# And the opt-in must actually still work, or the guard above would pass
# vacuously after someone deleted the feature.
if ! cargo tree -p astar-sys --features codec2-static 2>/dev/null | grep -qE 'codec2 v'; then
  echo "FAIL: astar-sys --features codec2-static does NOT pull in codec2." >&2
  echo "      The shipped app builds with this feature (see" >&2
  echo "      bindings/swift/build-xcframework.sh); M17 would have no codec." >&2
  fail=1
fi

[ "$fail" -eq 0 ] || exit 1
echo "codec2 licensing guard: defaults are LGPL-free, opt-in works."
