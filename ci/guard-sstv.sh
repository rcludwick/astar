#!/usr/bin/env bash
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#
# Keeps SSTV sending out of every shipped build, and astar-sstv a leaf.
#
# The `sstv` crate can encode (send) SSTV as well as decode it. astar carries
# a patched fork of it at vendor/sstv (MIT), and only astar-station's
# `sstv-tx` feature, which no shipped build turns on, may use its encoder.
# Each rule prints a tag, so a failure says which rule it broke:
#
# [published]  The published `sstv` crate is named nowhere. Every dependency
#              on `sstv` (any table, renamed or not, dev-dependencies too) is
#              a path to vendor/sstv or `workspace = true`, the workspace
#              entry is that path, and Cargo.lock has no registry `sstv`.
# [licence]    vendor/sstv/Cargo.toml hard-codes `license = "MIT"`, and its
#              version, edition and repository: none `.workspace = true`.
# [encode]     The fork's `encode` feature is asked for only by
#              astar-station's optional dependency, reached only through its
#              `sstv-tx` feature, and by dev-dependencies. Every other
#              normal dependency on the fork (the workspace entry included,
#              and astar-sstv's, which uses the fork's de-jitter to decode)
#              turns default features off, since they include `encode`, and
#              no `[features]` table enables `sstv/encode` or `sstv/default`.
# [sstv-tx]    `sstv-tx` is in no default feature set, and only
#              astar-nodebench (the bench) may turn it on: no other manifest
#              names it, which keeps it out of astar-sys (the app's engine),
#              astar-server's `release` and astar-netrec. The one exception
#              is astar-sys declaring its own non-default
#              `sstv-tx = ["astar-station/sstv-tx"]` (the bodies of its
#              picture functions) and a `[[test]]` requiring it; anything
#              else there naming it, `default` included, fails. Neither the
#              justfile's xcframework recipe nor
#              bindings/swift/build-xcframework.sh names it.
# [recipes]    A feature can also be asked for on a cargo command line, which
#              no manifest shows. So `sstv-tx` appears, outside `#` comments,
#              in no build recipe: the justfile, .gitlab-ci.yml, and the
#              scripts, workflows and container files under .github/,
#              deploy/, scripts/, bindings/ and apps/macos/Tools/. The
#              allowed lines are one that names astar-nodebench (the bench),
#              a `cargo test` / `cargo clippy` of `-p astar-station` (its own
#              sstv-tx tests), and exactly `cargo test -p astar-sys
#              --features sstv-tx --target-dir target/picture-ffi` (the
#              picture C interface's tests), which may add test-name filters
#              and arguments after `--` but no other package, feature or
#              flag, nothing chained after it, no line continuation and no
#              just `{{...}}` interpolation. The `--target-dir` is required:
#              without it the test build leaves a static library with
#              working picture functions in target/debug, which the Swift
#              package's fallback path links.
#              Lines are read one at a time.
# [workspace]  A recipe that feeds a shipped artifact never builds with
#              `--workspace` (or `--all`): a workspace build unifies features
#              across members, so the bench's `sstv-tx` would reach the
#              shipped crate. Those recipes are the justfile's xcframework,
#              server-release, app and dmg, .gitlab-ci.yml's `server` job,
#              both build-xcframework.sh scripts, apps/macos/Tools/build.sh
#              and make-dmg.sh, and every recipe file under deploy/. A
#              workspace-wide test, clippy or check (`just ci`) ships
#              nothing and stays allowed. The flag is matched as a token
#              anywhere on a line outside a comment, so a bash array
#              (`flags=("--workspace")`) counts.
# [all-features] Those same shipped recipes never pass `--all-features`
#              either: it names no feature, so [recipes] cannot see it, yet
#              on astar-sys it turns `sstv-tx` and the fork's encoder on.
#              It is matched as a token anywhere on a line outside a comment,
#              so a bash array (`codec_features=("--all-features")`) counts.
#              Lint and test runs (`ci/feature-matrix.sh`'s clippy per crate,
#              `just ci`) are not on that list, so they may keep using it.
# [leaf]      crates/astar-sstv's normal dependencies name no astar-* crate
#              except astar-record, so the decoder never reaches anything
#              that could key a radio.
# [headerless] slowrx's headerless starts stay off: neither of its two
#              setters for them appears anywhere under crates/, so a picture
#              starts only on a VIS header.
# [tree]       The check that cannot be spelled around: in the normal
#              dependency tree of every workspace member (read from
#              `cargo metadata`, so a new crate is checked without anyone
#              adding it here), each resolved on its own with all features
#              on, the `sstv` crate (which astar-sstv's de-jitter brings in)
#              is built without its `encode` or `default` feature. Three
#              members differ: astar-station is checked with its default
#              features, since `sstv-tx` is its own feature and all
#              features would turn it on; astar-sys is checked with every
#              feature but its own `sstv-tx` (its defaults and everything
#              the xcframework script adds); astar-nodebench is not checked,
#              since it is the bench, turns `sstv-tx` on by design and
#              ships nothing. When the check cannot run (the self-test
#              trees under SSTV_ROOT, no cargo, no Cargo.lock) it prints a
#              SKIP line saying why.
# [manifest]   A Cargo.toml the guard cannot parse fails, so nothing hides
#              behind a syntax the guard does not read.
#
# SSTV_ROOT points the guard at another tree; `--self-test` builds temporary
# trees that each break exactly one rule and checks the guard catches every
# one with the right tag.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

if [ "${1:-}" = "--self-test" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  cases=0
  failures=0

  # A clean tree that obeys every rule: the workspace entry for the fork,
  # the fork itself, astar-station's optional dependency behind `sstv-tx`,
  # the bench turning `sstv-tx` on, dev-dependencies asking for `encode`,
  # astar-sstv with its allowed dependencies, and the build scripts.
  clean() {
    # A fresh directory per tree: `clean` runs in a $(...) subshell, so a
    # counter kept here would never advance and every case would reuse (and
    # inherit files from) the same tree.
    local t
    t="$(mktemp -d "$tmp/t.XXXXXX")"
    mkdir -p "$t/vendor/sstv" "$t/crates/astar-sstv/src" "$t/crates/astar-station" \
      "$t/crates/astar-netrec" "$t/crates/astar-nodebench" "$t/crates/astar-server" \
      "$t/crates/astar-sys" "$t/bindings/swift"
    cat > "$t/Cargo.toml" <<'TOML'
[workspace]
members = ["crates/*"]

[workspace.dependencies]
sstv = { path = "vendor/sstv", version = "0.3.1", default-features = false }
TOML
    cat > "$t/vendor/sstv/Cargo.toml" <<'TOML'
[package]
name = "sstv"
version = "0.3.1"
edition = "2024"
repository = "https://github.com/unexcellent/sstv"
license = "MIT"

[features]
default = ["std", "encode"]
std = []
encode = []
TOML
    cat > "$t/crates/astar-sstv/Cargo.toml" <<'TOML'
[package]
name = "astar-sstv"

[dependencies]
slowrx = { workspace = true }
astar-record = { path = "../astar-record" }
sstv = { workspace = true, features = ["std", "dejitter"] }

[dev-dependencies]
sstv = { workspace = true, features = ["std", "encode"] }
astar-station = { path = "../astar-station" }
TOML
    cat > "$t/crates/astar-station/Cargo.toml" <<'TOML'
[package]
name = "astar-station"

[dependencies]
sstv = { workspace = true, optional = true, features = ["std", "encode"] }

[features]
default = ["m17"]
m17 = []
sstv = []
sstv-tx = ["dep:sstv"]
TOML
    cat > "$t/crates/astar-netrec/Cargo.toml" <<'TOML'
[package]
name = "astar-netrec"

[dependencies]
astar-sstv = { path = "../astar-sstv" }
astar-station = { path = "../astar-station", features = ["sstv"] }

[features]
release = ["astar-station/m17"]

[target.'cfg(unix)'.dev-dependencies]
sstv = { path = "../../vendor/sstv", version = "0.3.1", default-features = false, features = ["std", "encode"] }
TOML
    cat > "$t/crates/astar-nodebench/Cargo.toml" <<'TOML'
[package]
name = "astar-nodebench"

[dependencies]
astar-station = { path = "../astar-station", features = ["sstv-tx"] }

[dev-dependencies]
sstv = { workspace = true, features = ["std", "encode"] }
TOML
    cat > "$t/crates/astar-server/Cargo.toml" <<'TOML'
[package]
name = "astar-server"

[dependencies]
astar-station = { path = "../astar-station", default-features = false }

[features]
release = ["stream"]
stream = []
TOML
    cat > "$t/crates/astar-sys/Cargo.toml" <<'TOML'
[package]
name = "astar-sys"

[dependencies]
astar-station = { path = "../astar-station" }

[features]
default = ["dstar", "sstv"]
dstar = []
sstv = ["astar-station/sstv"]
codec2-static = []
sstv-tx = ["astar-station/sstv-tx"]

[[test]]
name = "picture_ffi"
required-features = ["sstv-tx"]
TOML
    cat > "$t/justfile" <<'JUST'
# The sstv-tx feature is the bench's alone (a comment may name it).
xcframework:
    ./bindings/swift/build-xcframework.sh
    ./bindings/swift-serial/build-xcframework.sh

bench:
    cargo test -p astar-nodebench
    cargo run -p astar-nodebench --features astar-station/sstv-tx -- run

ci:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace --all-targets
    cargo test -p astar-station --features sstv-tx

ffi-example:
    cargo build --release -p astar-sys

server-release target="x86_64-unknown-linux-musl":
    cargo zigbuild --release -p astar-server --no-default-features --features release --target {{target}}
JUST
    printf '#!/usr/bin/env bash\ncodec_features=(--features codec2-static)\ncargo build -p astar-sys "${codec_features[@]}"\n' \
      > "$t/bindings/swift/build-xcframework.sh"
    mkdir -p "$t/bindings/swift-serial" "$t/deploy" "$t/.github/workflows" "$t/apps/macos/Tools"
    printf '#!/usr/bin/env bash\ncargo build -p astar-serial-sys\n' \
      > "$t/bindings/swift-serial/build-xcframework.sh"
    printf '#!/usr/bin/env bash\n# never sstv-tx here\njust server-release "$TARGET"\n' > "$t/deploy/build.sh"
    printf '#!/usr/bin/env bash\nxcodebuild -scheme astar\n' > "$t/apps/macos/Tools/make-dmg.sh"
    cat > "$t/.gitlab-ci.yml" <<'YML'
rust:
  script:
    - cargo test --workspace --all-targets --locked

server:
  script:
    - cargo build --release -p astar-server --locked
YML
    cat > "$t/.github/workflows/ci.yml" <<'YML'
jobs:
  rust:
    steps:
      # sstv-tx is the bench's alone (a comment may name it).
      - run: cargo clippy --workspace --all-targets -- -D warnings
YML
    printf '[[package]]\nname = "sstv"\nversion = "0.3.1"\n' > "$t/Cargo.lock"
    echo 'fn main() {}' > "$t/crates/astar-sstv/src/lib.rs"
    echo "$t"
  }

  # expect pass|fail NAME TREE [TAG]: a failing case must print TAG, so it
  # cannot pass by breaking some other rule (or the TOML) by accident.
  expect() {
    local want="$1" name="$2" tree="$3" tag="${4:-}" got=0
    cases=$((cases + 1))
    SSTV_ROOT="$tree" "$here/guard-sstv.sh" > "$tree.out" 2>&1 || got=1
    if [ -n "${SSTV_SELFTEST_VERBOSE:-}" ]; then echo "== $name"; cat "$tree.out"; fi
    if [ "$want" = pass ] && [ "$got" -ne 0 ]; then
      echo "self-test FAIL: $name should pass:"; cat "$tree.out"; failures=$((failures + 1))
    elif [ "$want" = fail ] && [ "$got" -eq 0 ]; then
      echo "self-test FAIL: $name should fail"; failures=$((failures + 1))
    elif [ "$want" = fail ] && ! grep -qF -- "$tag" "$tree.out"; then
      echo "self-test FAIL: $name failed without $tag:"; cat "$tree.out"; failures=$((failures + 1))
    elif [ "$want" = fail ] && [ "$tag" != "[manifest]" ] && grep -qF "[manifest]" "$tree.out"; then
      echo "self-test FAIL: $name broke a manifest:"; cat "$tree.out"; failures=$((failures + 1))
    fi
  }

  # Replace line $2 of file $1 (matched exactly) with $3.
  swap() {
    python3 -I -c '
import sys
path, old, new = sys.argv[1:4]
text = open(path).read()
assert old in text, (path, old)
open(path, "w").write(text.replace(old, new, 1))
' "$1" "$2" "$3"
  }

  t=$(clean); expect pass "a clean tree" "$t"

  # [published]
  t=$(clean)
  swap "$t/crates/astar-nodebench/Cargo.toml" 'sstv = { workspace = true, features = ["std", "encode"] }' 'sstv = "=0.3.1"'
  expect fail "the published crate as a dev-dependency" "$t" "[published]"
  t=$(clean)
  swap "$t/crates/astar-netrec/Cargo.toml" 'astar-sstv = { path = "../astar-sstv" }' 'astar-sstv = { path = "../astar-sstv" }
enc = { package = "sstv", version = "0.3", default-features = false }'
  expect fail "the published crate renamed" "$t" "[published]"
  t=$(clean)
  swap "$t/Cargo.toml" 'sstv = { path = "vendor/sstv", version = "0.3.1", default-features = false }' 'sstv = { version = "0.3.1", default-features = false }'
  expect fail "the published crate in workspace.dependencies" "$t" "[published]"
  t=$(clean)
  swap "$t/crates/astar-netrec/Cargo.toml" 'sstv = { path = "../../vendor/sstv",' 'sstv = { path = "../../elsewhere/sstv",'
  expect fail "a path that is not vendor/sstv" "$t" "[published]"
  t=$(clean)
  printf 'source = "registry+https://github.com/rust-lang/crates.io-index"\n' >> "$t/Cargo.lock"
  expect fail "a registry sstv in Cargo.lock" "$t" "[published]"

  # [licence]
  t=$(clean)
  swap "$t/vendor/sstv/Cargo.toml" 'license = "MIT"' 'license.workspace = true'
  expect fail "the fork's licence inherited" "$t" "[licence]"
  t=$(clean)
  swap "$t/vendor/sstv/Cargo.toml" 'license = "MIT"' 'license = "AGPL-3.0-only"'
  expect fail "the fork relicensed" "$t" "[licence]"
  t=$(clean)
  swap "$t/vendor/sstv/Cargo.toml" 'edition = "2024"' 'edition.workspace = true'
  expect fail "the fork's edition inherited" "$t" "[licence]"

  # [encode]
  t=$(clean)
  swap "$t/Cargo.toml" ', default-features = false }' ' }'
  expect fail "the workspace entry keeps default features" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-netrec/Cargo.toml" 'astar-sstv = { path = "../astar-sstv" }' 'astar-sstv = { path = "../astar-sstv" }
sstv = { workspace = true, features = ["encode"] }'
  expect fail "another crate asking for encode" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-netrec/Cargo.toml" 'astar-sstv = { path = "../astar-sstv" }' 'astar-sstv = { path = "../astar-sstv" }
sstv = { path = "../../vendor/sstv", version = "0.3.1" }'
  expect fail "another crate with the fork's default features" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-netrec/Cargo.toml" 'astar-sstv = { path = "../astar-sstv" }' 'astar-sstv = { path = "../astar-sstv" }
sstv = { workspace = true, optional = true }'
  swap "$t/crates/astar-netrec/Cargo.toml" 'release = ["astar-station/m17"]' 'release = ["astar-station/m17", "sstv/encode"]'
  expect fail "a feature turning sstv/encode on" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-station/Cargo.toml" 'sstv = { workspace = true, optional = true, features' 'sstv = { workspace = true, features'
  expect fail "astar-station's encoder not optional" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-station/Cargo.toml" 'sstv = []' 'sstv = ["dep:sstv"]'
  expect fail "astar-station reaching the encoder from another feature" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-netrec/Cargo.toml" 'astar-sstv = { path = "../astar-sstv" }' 'astar-sstv = { path = "../astar-sstv" }
sstv = { workspace = true }'
  expect pass "another crate decoding with the fork" "$t"
  t=$(clean)
  swap "$t/crates/astar-sstv/Cargo.toml" 'sstv = { workspace = true, features = ["std", "dejitter"] }' 'sstv = { workspace = true, features = ["std", "dejitter", "encode"] }'
  expect fail "astar-sstv's de-jitter dependency asking for encode" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-sstv/Cargo.toml" 'sstv = { workspace = true, features = ["std", "dejitter"] }' 'sstv = { path = "../../vendor/sstv", version = "0.3.1", features = ["dejitter"] }'
  expect fail "astar-sstv's de-jitter dependency with default features" "$t" "[encode]"
  t=$(clean)
  swap "$t/crates/astar-sstv/Cargo.toml" 'sstv = { workspace = true, features = ["std", "dejitter"] }' 'sstv = { workspace = true, default-features = true, features = ["dejitter"] }'
  expect fail "astar-sstv's de-jitter dependency turning default features back on" "$t" "[encode]"

  # [sstv-tx]
  t=$(clean)
  swap "$t/crates/astar-station/Cargo.toml" 'default = ["m17"]' 'default = ["m17", "sstv-tx"]'
  expect fail "sstv-tx in astar-station's default" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/crates/astar-station/Cargo.toml" 'm17 = []' 'm17 = ["sstv-tx"]'
  expect fail "sstv-tx reached from astar-station's default" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/crates/astar-server/Cargo.toml" 'release = ["stream"]' 'release = ["stream", "astar-station/sstv-tx"]'
  expect fail "sstv-tx in astar-server's release" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/crates/astar-netrec/Cargo.toml" 'features = ["sstv"] }' 'features = ["sstv", "sstv-tx"] }'
  expect fail "astar-netrec turning sstv-tx on" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/justfile" '    ./bindings/swift/build-xcframework.sh' '    ASTAR_FEATURES=sstv-tx ./bindings/swift/build-xcframework.sh'
  expect fail "sstv-tx in the xcframework recipe" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" '--features codec2-static' '--features codec2-static,astar-station/sstv-tx'
  expect fail "sstv-tx in build-xcframework.sh" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/crates/astar-sys/Cargo.toml" 'default = ["dstar", "sstv"]' 'default = ["dstar", "sstv", "sstv-tx"]'
  expect fail "sstv-tx in astar-sys's default" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/crates/astar-sys/Cargo.toml" 'codec2-static = []' 'codec2-static = ["sstv-tx"]'
  expect fail "astar-sys reaching sstv-tx through another feature" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/crates/astar-sys/Cargo.toml" '[[test]]' '[dev-dependencies]
astar-station = { path = "../astar-station", features = ["sstv-tx"] }

[[test]]'
  expect fail "astar-sys naming sstv-tx in a dev-dependency" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/crates/astar-sys/Cargo.toml" 'required-features = ["sstv-tx"]' 'required-features = ["sstv-tx", "dstar"]'
  expect fail "an astar-sys test requiring more than sstv-tx" "$t" "[sstv-tx]"

  # [recipes]
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo build --release -p astar-sys --features astar-station/sstv-tx'
  expect fail "sstv-tx on another justfile recipe's command line" "$t" "[recipes]"
  t=$(clean)
  swap "$t/.gitlab-ci.yml" '    - cargo build --release -p astar-server --locked' '    - cargo build --release -p astar-server --locked --features astar-station/sstv-tx'
  expect fail "sstv-tx in .gitlab-ci.yml" "$t" "[recipes]"
  t=$(clean)
  swap "$t/.github/workflows/ci.yml" '      - run: cargo clippy' '      - run: cargo build -p astar-netrec --features astar-station/sstv-tx
      - run: cargo clippy'
  expect fail "sstv-tx in a GitHub workflow" "$t" "[recipes]"
  t=$(clean)
  swap "$t/deploy/build.sh" 'just server-release' 'ASTAR_FEATURES=sstv-tx just server-release'
  expect fail "sstv-tx in a deploy script" "$t" "[recipes]"
  t=$(clean)
  swap "$t/bindings/swift-serial/build-xcframework.sh" '-p astar-serial-sys' '-p astar-serial-sys --features astar-station/sstv-tx'
  expect fail "sstv-tx in a bindings script" "$t" "[recipes]"
  t=$(clean)
  mkdir -p "$t/scripts"
  printf '#!/usr/bin/env bash\ncargo run -p astar-cli --features astar-station/sstv-tx\n' > "$t/scripts/send.sh"
  expect fail "sstv-tx in a script under scripts/" "$t" "[recipes]"
  t=$(clean)
  swap "$t/deploy/build.sh" 'just server-release "$TARGET"' 'just server-release "$TARGET"  # not sstv-tx'
  expect pass "sstv-tx in a trailing comment" "$t"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys --features sstv-tx --target-dir target/picture-ffi'
  expect pass "astar-sys's own picture tests" "$t"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys --features sstv-tx --target-dir=target/picture-ffi picture -- --nocapture'
  expect pass "astar-sys's own picture tests with a filter and arguments" "$t"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys --features sstv-tx'
  expect fail "astar-sys's picture tests into the default target directory" "$t" "[recipes]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys --features sstv-tx --target-dir target'
  expect fail "astar-sys's picture tests into another target directory" "$t" "[recipes]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys --features sstv-tx --target-dir target/picture-ffi \
    --release'
  expect fail "astar-sys's picture tests with a line continuation" "$t" "[recipes]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys --features sstv-tx --target-dir target/picture-ffi {{flags}}'
  expect fail "astar-sys's picture tests with just interpolation" "$t" "[recipes]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo build -p astar-sys --features sstv-tx'
  expect fail "a build of astar-sys with sstv-tx" "$t" "[recipes]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys -p astar-server --features sstv-tx'
  expect fail "astar-sys's picture tests with another package" "$t" "[recipes]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test --release -p astar-sys --features sstv-tx'
  expect fail "astar-sys's picture tests with another flag" "$t" "[recipes]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo test -p astar-sys --features sstv-tx && cargo build -p astar-sys'
  expect fail "astar-sys's picture tests with a command chained on" "$t" "[recipes]"

  # [workspace]
  t=$(clean)
  swap "$t/justfile" 'cargo zigbuild --release -p astar-server' 'cargo zigbuild --release --workspace'
  expect fail "server-release building the workspace" "$t" "[workspace]"
  t=$(clean)
  swap "$t/justfile" '    ./bindings/swift-serial/build-xcframework.sh' '    ./bindings/swift-serial/build-xcframework.sh
    cargo build --all --release'
  expect fail "the xcframework recipe building the workspace" "$t" "[workspace]"
  t=$(clean)
  swap "$t/.gitlab-ci.yml" '    - cargo build --release -p astar-server --locked' '    - cargo build --release --workspace --locked'
  expect fail "the gitlab server job building the workspace" "$t" "[workspace]"
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'cargo build -p astar-sys' 'cargo build --workspace'
  expect fail "build-xcframework.sh building the workspace" "$t" "[workspace]"
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'codec_features=(--features codec2-static)' 'codec_features=("--workspace" --features codec2-static)'
  expect fail "build-xcframework.sh's feature array with --workspace" "$t" "[workspace]"
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'codec_features=(--features codec2-static)' 'codec_features=(--all --features codec2-static)'
  expect fail "build-xcframework.sh's feature array with --all" "$t" "[workspace]"
  t=$(clean)
  swap "$t/apps/macos/Tools/make-dmg.sh" 'xcodebuild -scheme astar' 'cargo build --workspace --release'
  expect fail "make-dmg.sh building the workspace" "$t" "[workspace]"
  t=$(clean)
  swap "$t/deploy/build.sh" 'just server-release "$TARGET"' 'cargo build --workspace --release'
  expect fail "a deploy script building the workspace" "$t" "[workspace]"
  t=$(clean)
  swap "$t/justfile" '    cargo build --release -p astar-sys' '    cargo build --release --workspace'
  expect pass "a recipe that ships nothing building the workspace" "$t"

  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'codec_features=(--features codec2-static)' 'codec_features=(--features codec2-static)
codec_features+=(--features sstv-tx)'
  expect fail "build-xcframework.sh adding sstv-tx to its feature array" "$t" "[sstv-tx]"
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'codec_features=(--features codec2-static)' 'codec_features=(--features astar-station/sstv-tx)'
  expect fail "build-xcframework.sh's feature array naming astar-station/sstv-tx" "$t" "[recipes]"

  # [all-features]
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'codec_features=(--features codec2-static)' 'codec_features=(--all-features)'
  expect fail "build-xcframework.sh's feature array with --all-features" "$t" "[all-features]"
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'codec_features=(--features codec2-static)' 'codec_features=("--all-features")'
  expect fail "build-xcframework.sh's feature array with a quoted --all-features" "$t" "[all-features]"
  t=$(clean)
  swap "$t/bindings/swift/build-xcframework.sh" 'codec_features=(--features codec2-static)' 'codec_features=(--features codec2-static)  # never --all-features'
  expect pass "--all-features in a comment" "$t"
  t=$(clean)
  swap "$t/justfile" 'cargo zigbuild --release -p astar-server --no-default-features --features release' 'cargo zigbuild --release -p astar-server --all-features'
  expect fail "server-release with --all-features" "$t" "[all-features]"
  t=$(clean)
  swap "$t/justfile" '    cargo clippy --workspace --all-targets -- -D warnings' '    cargo clippy -p astar-sys --all-targets --all-features -- -D warnings'
  expect pass "a lint run with --all-features" "$t"

  # [leaf]
  t=$(clean)
  swap "$t/crates/astar-sstv/Cargo.toml" 'slowrx = { workspace = true }' 'slowrx = { workspace = true }
astar-station = { path = "../astar-station" }'
  expect fail "astar-sstv depending on astar-station" "$t" "[leaf]"
  t=$(clean)
  printf '\n[target.'"'"'cfg(unix)'"'"'.dependencies]\nastar-ptt = { path = "../astar-ptt" }\n' >> "$t/crates/astar-sstv/Cargo.toml"
  expect fail "astar-sstv depending on astar-ptt for one target" "$t" "[leaf]"
  t=$(clean)
  swap "$t/crates/astar-sstv/Cargo.toml" 'slowrx = { workspace = true }' 'slowrx = { workspace = true }
keyer = { package = "astar-ptt", path = "../astar-ptt" }'
  expect fail "astar-sstv depending on a renamed astar-ptt" "$t" "[leaf]"

  # [headerless]
  t=$(clean)
  echo "fn f(d: &mut D) { d.set_forced_mode(None); }" >> "$t/crates/astar-sstv/src/lib.rs"
  expect fail "set_forced_mode called" "$t" "[headerless]"
  t=$(clean)
  mkdir -p "$t/crates/astar-station/src"
  echo "// d.set_fallback_mode(Some(m))" > "$t/crates/astar-station/src/sstv.rs"
  expect fail "set_fallback_mode under another crate" "$t" "[headerless]"

  # [manifest]
  t=$(clean)
  printf '\n[dependencies]\nsstv = "=0.3.1"\n' >> "$t/crates/astar-netrec/Cargo.toml"
  expect fail "a manifest that does not parse" "$t" "[manifest]"

  if [ "$failures" -ne 0 ]; then
    echo "FAIL: sstv guard self-test ($failures)"
    exit 1
  fi
  echo "sstv guard self-test: ok ($cases cases)"
  exit 0
fi

root="${SSTV_ROOT:-$(cd "$here/.." && pwd)}"
cd "$root"
fail=0

# The manifest rules, read with a real TOML parser.
if ! python3 -I - "$root" <<'PY'
import os
import re
import sys
import tomllib

root = os.path.realpath(sys.argv[1])
fork = os.path.join(root, "vendor", "sstv")
problems = []


def bad(tag, where, what):
    problems.append(f"FAIL {tag} {os.path.relpath(where, root)}: {what}")


manifests = []
for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = sorted(
        d for d in dirnames if d not in {"target", ".worktrees", ".git", "node_modules", ".build"}
    )
    if "Cargo.toml" in filenames:
        manifests.append(os.path.join(dirpath, "Cargo.toml"))

parsed = {}
for m in manifests:
    try:
        with open(m, "rb") as f:
            parsed[m] = tomllib.load(f)
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as e:
        bad("[manifest]", m, f"cannot be parsed, so it cannot be checked: {e}")


def dep_tables(doc):
    """(kind, table) for every dependency table: kind is normal, build or dev."""
    out = []
    for kind, key in (("normal", "dependencies"), ("build", "build-dependencies"),
                      ("dev", "dev-dependencies"), ("dev", "dev_dependencies"),
                      ("build", "build_dependencies")):
        if isinstance(doc.get(key), dict):
            out.append((kind, doc[key]))
    for target in (doc.get("target") or {}).values():
        if isinstance(target, dict):
            out.extend(dep_tables(target))
    return out


def is_sstv(name, spec):
    return name == "sstv" or (isinstance(spec, dict) and spec.get("package") == "sstv")


def flag(spec, key, default):
    if not isinstance(spec, dict):
        return default
    return spec.get(key, spec.get(key.replace("-", "_"), default))


root_manifest = os.path.join(root, "Cargo.toml")
ws_deps = ((parsed.get(root_manifest) or {}).get("workspace") or {}).get("dependencies") or {}
ws_entry = next((spec for name, spec in ws_deps.items() if is_sstv(name, spec)), None)


def points_at_fork(manifest, spec):
    if not isinstance(spec, dict) or "path" not in spec:
        return False
    target = os.path.realpath(os.path.join(os.path.dirname(manifest), spec["path"]))
    return target == fork and not any(k in spec for k in ("git", "registry"))


# [published] and [encode] on the workspace entry.
for name, spec in ws_deps.items():
    if is_sstv(name, spec):
        if not points_at_fork(root_manifest, spec):
            bad("[published]", root_manifest,
                f"workspace dependency {name!r} must be a path to vendor/sstv")
        if flag(spec, "default-features", True):
            bad("[encode]", root_manifest,
                "the workspace entry for the fork must set default-features = false")
        if {"encode", "default"} & set(flag(spec, "features", [])):
            bad("[encode]", root_manifest, "the workspace entry may not ask for encode")

station = os.path.join(root, "crates", "astar-station", "Cargo.toml")
bench = os.path.join(root, "crates", "astar-nodebench", "Cargo.toml")

for m, doc in parsed.items():
    sstv_keys = set()
    for kind, table in dep_tables(doc):
        for name, spec in table.items():
            if not is_sstv(name, spec):
                continue
            sstv_keys.add(name)
            inherited = flag(spec, "workspace", False) is True
            # [published]
            if inherited:
                if ws_entry is None:
                    bad("[published]", m, f"{name!r} inherits a workspace entry that does not exist")
            elif not points_at_fork(m, spec):
                bad("[published]", m, f"{name!r} must be a path to vendor/sstv or workspace = true")
            if kind == "dev":
                continue
            # [encode]
            features = set(flag(spec, "features", []))
            if inherited and isinstance(ws_entry, dict):
                features |= set(flag(ws_entry, "features", []))
                defaults = flag(ws_entry, "default-features", True) or flag(spec, "default-features", False) is True
            else:
                defaults = flag(spec, "default-features", True)
            if m == station and kind == "normal":
                if flag(spec, "optional", False) is not True:
                    bad("[encode]", m, "astar-station's encoder dependency must be optional")
                continue
            if defaults or {"encode", "default"} & features:
                bad("[encode]", m,
                    f"{name!r} may not ask for the encoder: set default-features = false "
                    "and leave out encode (only astar-station's sstv-tx and dev-dependencies may)")
    features = doc.get("features") or {}
    for fname, values in features.items():
        for v in values if isinstance(values, list) else []:
            # [encode]: no feature turns the fork's encoder on.
            for key in sstv_keys:
                if re.fullmatch(rf"{re.escape(key)}\??/(encode|default)", v):
                    bad("[encode]", m, f"feature {fname!r} turns {v!r} on")
            # [encode]: in astar-station only `sstv-tx` reaches the dependency.
            if m == station and fname != "sstv-tx" and any(
                v == f"dep:{k}" or v.startswith((f"{k}/", f"{k}?/")) for k in sstv_keys
            ):
                bad("[encode]", m, f"only sstv-tx may reach the encoder, not {fname!r}")

# [licence]
fork_manifest = os.path.join(fork, "Cargo.toml")
pkg = (parsed.get(fork_manifest) or {}).get("package")
if pkg is None:
    if fork_manifest not in [p.split(": ")[0] for p in problems]:
        bad("[licence]", fork_manifest, "missing, so its licence cannot be checked")
else:
    if pkg.get("license") != "MIT":
        bad("[licence]", fork_manifest, 'must hard-code license = "MIT"')
    for key in ("version", "edition", "repository"):
        if not isinstance(pkg.get(key), str):
            bad("[licence]", fork_manifest, f"must hard-code {key}, never .workspace = true")

# [published]: Cargo.lock has the fork only, never a registry copy.
lock = os.path.join(root, "Cargo.lock")
if os.path.exists(lock):
    with open(lock, "rb") as f:
        try:
            packages = tomllib.load(f).get("package", [])
        except tomllib.TOMLDecodeError as e:
            packages = []
            bad("[manifest]", lock, f"cannot be parsed: {e}")
    for p in packages:
        if p.get("name") == "sstv" and "source" in p:
            bad("[published]", lock, f"locks a published sstv ({p['source']})")

# [sstv-tx]: never a default, and named only by astar-station and the bench.
if station in parsed:
    feats = parsed[station].get("features") or {}
    seen, todo = set(), list(feats.get("default", []))
    while todo:
        f = todo.pop()
        if f in seen:
            continue
        seen.add(f)
        if f == "sstv-tx":
            bad("[sstv-tx]", station, "sstv-tx is reachable from the default features")
            break
        todo.extend(feats.get(f, []))


def names_sstv_tx(node):
    if isinstance(node, str):
        return "sstv-tx" in node
    if isinstance(node, list):
        return any(names_sstv_tx(x) for x in node)
    if isinstance(node, dict):
        return any(names_sstv_tx(k) or names_sstv_tx(v) for k, v in node.items())
    return False


sys_manifest = os.path.join(root, "crates", "astar-sys", "Cargo.toml")


def without_sys_declaration(doc):
    """astar-sys's manifest less the two mentions it may make: its own
    non-default `sstv-tx = ["astar-station/sstv-tx"]`, and a `[[test]]`
    requiring exactly that feature."""
    doc = dict(doc)
    feats = dict(doc.get("features") or {})
    if feats.get("sstv-tx") == ["astar-station/sstv-tx"]:
        del feats["sstv-tx"]
    doc["features"] = feats
    tests = doc.get("test")
    if isinstance(tests, list):
        doc["test"] = [
            {k: v for k, v in t.items() if not (k == "required-features" and v == ["sstv-tx"])}
            if isinstance(t, dict) else t
            for t in tests
        ]
    return doc


for m, doc in parsed.items():
    if m in (station, bench):
        continue
    rest = {k: v for k, v in doc.items() if k != "package"}
    if m == sys_manifest:
        rest = without_sys_declaration(rest)
    if names_sstv_tx(rest):
        bad("[sstv-tx]", m, "names sstv-tx; only astar-nodebench may turn it on "
            "(astar-sys may only declare its own non-default sstv-tx)")

justfile = os.path.join(root, "justfile")
if os.path.exists(justfile):
    recipe, inside = [], False
    for line in open(justfile, encoding="utf-8"):
        if re.match(r"^xcframework\s*:", line):
            inside = True
            continue
        if inside:
            if line.strip() and not line[0].isspace():
                break
            recipe.append(line)
    if any("sstv-tx" in l for l in recipe):
        bad("[sstv-tx]", justfile, "the xcframework recipe names sstv-tx")
script = os.path.join(root, "bindings", "swift", "build-xcframework.sh")
if os.path.exists(script) and "sstv-tx" in open(script, encoding="utf-8").read():
    bad("[sstv-tx]", script, "names sstv-tx; the app's engine never sends pictures")


# [recipes] and [workspace]: what the cargo command lines ask for.
def is_recipe(path):
    base = os.path.basename(path)
    if base in ("justfile", "Justfile", "Makefile") or base.startswith(("Containerfile", "Dockerfile")):
        return True
    if os.path.splitext(base)[1] in (".sh", ".bash", ".yml", ".yaml", ".py"):
        return True
    try:
        with open(path, "rb") as f:
            return f.read(2) == b"#!"
    except OSError:
        return False


def code_lines(path):
    """(number, text) for each line of a recipe with its `#` comment removed."""
    out = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for i, line in enumerate(f, 1):
            text = line.rstrip("\n")
            if text.lstrip().startswith("#"):
                continue
            out.append((i, re.split(r"\s#", text, maxsplit=1)[0]))
    return out


def block(path, header):
    """The (number, text) lines of a justfile recipe or a top-level YAML job."""
    lines, inside = [], False
    if not os.path.exists(path):
        return lines
    for i, text in code_lines(path):
        if re.match(rf"^{re.escape(header)}(\s[^:]*)?:", text):
            inside = True
            continue
        if inside:
            if text.strip() and not text[0].isspace():
                break
            lines.append((i, text))
    return lines


recipes = [p for p in (os.path.join(root, "justfile"), os.path.join(root, ".gitlab-ci.yml"))
           if os.path.exists(p)]
for sub in (".github", "deploy", "scripts", "bindings", os.path.join("apps", "macos", "Tools")):
    for dirpath, dirnames, filenames in os.walk(os.path.join(root, sub)):
        dirnames[:] = sorted(
            d for d in dirnames
            if d not in {"target", ".build", "build", "node_modules"} and not d.endswith(".xcframework")
        )
        recipes.extend(os.path.join(dirpath, f) for f in sorted(filenames)
                       if is_recipe(os.path.join(dirpath, f)))

STATION_TESTS = re.compile(r"\bcargo\s+(?:\+\S+\s+)?(?:test|clippy|nextest\s+run)\b.*-p\s+astar-station\b")


def sys_picture_tests(text):
    """True only for `cargo test -p astar-sys --features sstv-tx`, with
    optional test-name filters and `--` arguments, alone on its line."""
    t = re.sub(r"^\s*(?:-\s+)?(?:run:\s*)?@?", "", text).strip()
    # No chaining or substitution, no line continuation (the next line would
    # go unchecked), and no just `{{...}}` interpolation.
    if re.search(r"[;&|`$()<>\\{}]", t):
        return False
    toks = t.split()
    if toks[:1] != ["cargo"]:
        return False
    toks = toks[1:]
    if toks and toks[0].startswith("+"):
        toks = toks[1:]
    if toks[:1] != ["test"]:
        return False
    toks = toks[1:]
    if "--" in toks:
        toks = toks[: toks.index("--")]
    values = {"-p": [], "--features": [], "--target-dir": []}
    alias = {"-p": "-p", "--package": "-p", "-F": "--features", "--features": "--features",
             "--target-dir": "--target-dir"}
    i = 0
    while i < len(toks):
        tok = toks[i]
        if tok in alias:
            if i + 1 >= len(toks):
                return False
            values[alias[tok]].append(toks[i + 1])
            i += 2
            continue
        key, eq, value = tok.partition("=")
        if eq and key in alias:
            values[alias[key]].append(value)
        elif tok.startswith("-"):
            return False
        i += 1
    return (values["-p"] == ["astar-sys"] and values["--features"] == ["sstv-tx"]
            and values["--target-dir"] == ["target/picture-ffi"])


for path in recipes:
    for i, text in code_lines(path):
        if ("sstv-tx" in text and "astar-nodebench" not in text
                and not STATION_TESTS.search(text) and not sys_picture_tests(text)):
            bad("[recipes]", path,
                f"line {i} names sstv-tx; only the bench and astar-station's own tests may: {text.strip()}")

WORKSPACE = re.compile(r"(?<![\w-])--(workspace|all)(?![\w-])")
ALL_FEATURES = re.compile(r"(?<![\w-])--all-features(?![\w-])")
shipped = [(os.path.join(root, "justfile"), name)
           for name in ("xcframework", "server-release", "app", "dmg")]
shipped.append((os.path.join(root, ".gitlab-ci.yml"), "server"))
for path, name in shipped:
    for i, text in block(path, name):
        if WORKSPACE.search(text):
            bad("[workspace]", path,
                f"line {i}: {name!r} feeds a shipped artifact and may not build the whole workspace")
        if ALL_FEATURES.search(text):
            bad("[all-features]", path,
                f"line {i}: {name!r} feeds a shipped artifact and may not use --all-features")
shipped_files = [os.path.join(root, *p) for p in (
    ("bindings", "swift", "build-xcframework.sh"),
    ("bindings", "swift-serial", "build-xcframework.sh"),
    ("apps", "macos", "Tools", "build.sh"),
    ("apps", "macos", "Tools", "make-dmg.sh"),
)]
shipped_files += [p for p in recipes if p.startswith(os.path.join(root, "deploy") + os.sep)]
for path in shipped_files:
    if not os.path.exists(path):
        continue
    for i, text in code_lines(path):
        if WORKSPACE.search(text):
            bad("[workspace]", path,
                f"line {i} builds the whole workspace; a shipped artifact is built with -p")
        if ALL_FEATURES.search(text):
            bad("[all-features]", path,
                f"line {i} uses --all-features, which turns sstv-tx on; name a shipped build's features")

# [leaf]
leaf = os.path.join(root, "crates", "astar-sstv", "Cargo.toml")
if leaf not in parsed:
    if not any(p.startswith("FAIL [manifest]") for p in problems):
        bad("[leaf]", leaf, "missing, so the guard cannot run")
else:
    for kind, table in dep_tables(parsed[leaf]):
        if kind == "dev":
            continue
        for name, spec in table.items():
            crate = flag(spec, "package", name) if isinstance(spec, dict) else name
            if crate.startswith("astar-") and crate != "astar-record":
                bad("[leaf]", leaf, f"astar-sstv may depend on no astar crate but astar-record, not {crate}")

for p in problems:
    print(p)
sys.exit(1 if problems else 0)
PY
then
  fail=1
fi

# [headerless]. The names are split so this script does not match itself if
# it is ever scanned.
fallback="set_fallback""_mode"
forced="set_forced""_mode"
if [ -d crates ]; then
  status=0
  hits=$(grep -rnE --include='*.rs' -e "($fallback|$forced)" crates) || status=$?
  if [ "$status" -ge 2 ]; then
    echo "FAIL [headerless] the scan of crates/ could not run"
    exit 1
  fi
  if [ -n "$hits" ]; then
    echo "FAIL [headerless] slowrx's headerless starts must stay off; a picture starts only on a VIS header:"
    echo "$hits"
    fail=1
  fi
fi

# [tree]
if [ -n "${SSTV_ROOT:-}" ]; then
  echo "SKIP [tree] SSTV_ROOT names another tree (the self-test's), which cargo cannot resolve; the rules above still ran"
elif ! command -v cargo >/dev/null 2>&1; then
  echo "SKIP [tree] cargo is not on PATH, so no dependency tree can be read; the rules above still ran"
elif [ ! -f Cargo.lock ]; then
  echo "SKIP [tree] there is no Cargo.lock, so the trees cannot be resolved offline; the rules above still ran"
else
  check_tree() { # crate, then cargo tree feature flags
    local crate="$1" tree
    shift
    if tree=$(cargo tree -e normal,features --offline --locked "$@" -p "$crate" 2>&1); then
      if printf '%s\n' "$tree" | grep -Eq '(^|[^A-Za-z0-9_-])sstv feature "(encode|default)"'; then
        echo "FAIL [tree] $crate ($*) builds the sstv crate with its encoder"
        fail=1
      fi
    else
      echo "FAIL [tree] cargo tree could not run for $crate:"
      echo "$tree"
      fail=1
    fi
  }
  # Every workspace member, so a new crate is covered without being listed.
  if members=$(cargo metadata --no-deps --format-version 1 --offline --locked 2>&1 |
    python3 -I -c 'import json, sys
d = json.load(sys.stdin)
ids = set(d["workspace_members"])
print("\n".join(sorted(p["name"] for p in d["packages"] if p["id"] in ids)))'); then
    for crate in $members; do
      case "$crate" in
        # The bench: it turns sstv-tx on by design and ships nothing.
        astar-nodebench) ;;
        # sstv-tx is astar-station's own feature, so --all-features would
        # turn it on; its default features are what everything else gets.
        astar-station) check_tree "$crate" ;;
        # astar-sys declares its own non-default sstv-tx: check it with
        # every other feature, which covers its defaults and the
        # xcframework script's additions.
        astar-sys)
          if sys_features=$(python3 -I -c 'import sys, tomllib
f = tomllib.load(open(sys.argv[1], "rb")).get("features", {})
print(",".join(sorted(k for k in f if k not in ("default", "sstv-tx"))))' crates/astar-sys/Cargo.toml); then
            check_tree "$crate" --features "$sys_features"
          else
            echo "FAIL [tree] could not read astar-sys's features"
            fail=1
          fi
          ;;
        *) check_tree "$crate" --all-features ;;
      esac
    done
  else
    echo "FAIL [tree] cargo metadata could not list the workspace members:"
    echo "$members"
    fail=1
  fi
fi

if [ "$fail" -ne 0 ]; then
  echo "FAIL: sstv guard"
  exit 1
fi
echo "sstv guard: ok"
