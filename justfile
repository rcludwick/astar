# astar — task runner. Run `just` (or `just --list`) to see every recipe.
#
# One repo, three deliverables:
#   * the engine crates under crates/   (Rust; "astar-lib" in prose)
#   * the node daemon crates/astar-server
#   * the clients: apps/macos (SwiftUI) and apps/gui (Iced, Windows/Linux)
#
# Cargo and xcodebuild are the build systems; this file is the command palette.

# Real stable-toolchain bin dir, prepended to PATH so cargo bypasses Homebrew's
# rustup shim (which loses argv[0]) on the maintainer's mac. On any other host /
# CI this directory doesn't exist, so it's an inert PATH entry — fully portable.
toolchain_bin := "/Users/rob/.rustup/toolchains/stable-aarch64-apple-darwin/bin"
export PATH := toolchain_bin + ":" + env_var("PATH")

# Show the recipe list (default when you just run `just`).
default:
    @just --list

# ── Rust: core build / test / lint ──────────────────────────────────────────

# Build. `just build` = whole workspace; `just build -p astar-audio` scopes it.
build *args="--workspace --all-targets":
    cargo build {{args}}

# Test. `just test` = whole workspace; `just test -p astar-server` scopes it.
test *args="--workspace --all-targets":
    cargo test {{args}}

# Clippy with warnings-as-errors (matches CI). Args scope it like build/test.
clippy *args="--workspace --all-targets":
    cargo clippy {{args}} -- -D warnings

# Rewrite all files with rustfmt.
fmt:
    cargo fmt --all

# Check formatting without rewriting (CI-style).
fmt-check:
    cargo fmt --all -- --check

# Release build of the whole workspace.
release:
    cargo build --workspace --release

# ── D-Star / ThumbDV ────────────────────────────────────────────────────────

# D-Star is HARDWARE-ONLY: the vocoder is a ThumbDV / DV3000 USB dongle driven
# by the vendored `ambe-thumbdv` crate (vendor/ambe-thumbdv). There is no
# software AMBE backend. The `dstar` feature is therefore not part of `just ci`.
#
# Without IAX_THUMBDV_TESTS=1 every hardware-touching test SKIPS (printing why
# on stderr) and the suites still run green — that gate is what keeps other
# machines and CI honest. The hardware-free coverage of the same D-Star
# run-loop logic (priming, burst absorption, the drains) runs either way, via
# astar-console's tests/dstar_session_pipeline.rs.
#
# `just dstar-test` = the hardware-free half. `just dstar-test-hw` = with the
# dongle attached; only ONE process may hold it, so nothing else (dstar-listen,
# another test run) may be running at the same time.
#
# IAX_THUMBDV_PORT=/dev/cu.usbserial-XXXX pins WHICH ThumbDV to use when
# several are attached. It can only SELECT among ports the FTDI 0x0403:0x6015
# scan already matched — it can never point at a USB radio interface's serial
# port, where opening the tty would assert RTS and key a transmitter.
dstar-test:
    cargo test -p astar-codec --features ambe-hw
    cargo test -p astar-console --features dstar
    cargo test -p astar-station --features dstar
    cargo test -p astar-cli --features dstar

dstar-test-hw:
    IAX_THUMBDV_TESTS=1 cargo test -p astar-codec --features ambe-hw
    IAX_THUMBDV_TESTS=1 cargo test -p astar-console --features dstar
    IAX_THUMBDV_TESTS=1 cargo test -p astar-station --features dstar

# ── repo-specific checks ────────────────────────────────────────────────────

# Regenerate the astar-sys / astar-serial-sys C headers and fail on drift +
# secret leaks. Requires: cargo install cbindgen --version 0.29.4 --locked
cbindgen:
    ./scripts/check-cbindgen.sh
    ./scripts/check-cbindgen-serial.sh

# The five merge invariants CI's `guard` stage enforces: no armed hardware /
# live-network opt-in, no git dependencies (ambe-thumbdv stays vendored, with
# its licences), no invented install channel, an AGPL SPDX header on every
# first-party rs/swift/sh/py file, and no LGPL code in a default build (Codec 2
# stays opt-in). Each prints one line; run them before a push.
guards:
    ./ci/guard-safety-env.sh
    ./ci/guard-no-git-deps.sh
    ./ci/guard-distribution-claims.sh
    ./ci/guard-spdx-headers.sh
    ./ci/guard-codec2-licensing.sh

# RFC audit verifier (tracker-free: ticket refs are format-checked, plus
# archive membership when docs/issues-archive.jsonl is present locally).
rfc-audit:
    ./scripts/verify-rfc-audit.sh

# Build the cdylib/staticlib and compile+link the C parrot example.
ffi-example:
    cargo build --release -p astar-sys
    cd crates/astar-sys && ./examples/build.sh

# Offline Python ctypes smoke + compile-check. Needs python3.
python:
    cargo build -p astar-sys
    cd bindings/python && python3 astarstation.py && python3 test_smoke.py && python3 examples/parrot.py --dry-run
    cd bindings/python && python3 -m py_compile astarstation.py test_smoke.py examples/parrot.py

# ── Swift bindings + the macOS app ──────────────────────────────────────────

# Build BOTH Swift xcframeworks (host slice; iOS slices if rustup has the
# targets). Always both — a stale cache lies. Requires a full Xcode.
xcframework:
    ./bindings/swift/build-xcframework.sh
    ./bindings/swift-serial/build-xcframework.sh

# (Re)generate apps/macos/astar.xcodeproj from project.yml.
generate:
    cd apps/macos && xcodegen generate

# Build the macOS app. Pass flags through, e.g. `just app --release` / `--clean`.
# Run `just xcframework` first on a fresh checkout.
app *args:
    apps/macos/Tools/build.sh {{args}}

# Build (if needed) and launch the macOS app. `just run --no-build` relaunches.
run *args:
    apps/macos/Tools/run.sh {{args}}

# Run the AstarCore unit tests (plain SwiftPM — no Xcode project needed).
app-test:
    cd apps/macos/Packages/AstarCore && swift test

# Build a double-clickable Release astar.dmg (→ apps/macos/build/astar.dmg).
# This is a LOCAL artifact: astar has no published release, tap, or cask.
dmg:
    apps/macos/Tools/make-dmg.sh

# Regenerate app icons + the menu-bar template from the art/ SVG masters.
icons:
    apps/macos/Tools/render-icons.sh

# Reformat the hand-written Swift sources in place (swift-format, bundled with
# Xcode; uses .swift-format).
swift-fmt:
    swift format --in-place --recursive apps/macos/Sources apps/macos/Packages/AstarCore/Sources apps/macos/Packages/AstarCore/Tests

# Report swift-format issues without modifying. `--strict` fails on any finding.
swift-fmt-check:
    swift format lint --strict --recursive apps/macos/Sources apps/macos/Packages/AstarCore/Sources apps/macos/Packages/AstarCore/Tests

# ── apps/gui (the Iced Windows/Linux client) ────────────────────────────────

# Build + test + headless-run astar-gui on Linux, in a podman container.
# Proof PNG: apps/gui/.shots/linux-idle.png.
gui-linux:
    apps/gui/check-linux.sh

# Cross-compile + link a Windows astar-gui.exe from this Mac (cargo-xwin).
gui-windows:
    apps/gui/check-windows.sh

# Regenerate the Iced client's demo screenshots into apps/gui/.shots/.
gui-shots *args:
    apps/gui/shots.sh {{args}}

# ── node daemon (astar-server) ──────────────────────────────────────────────

# Launch the node daemon (sources .env for ALLSTAR_NODE/ALLSTAR_SECRET).
node config="node.toml":
    #!/usr/bin/env bash
    set -euo pipefail
    [ -f .env ] && { set -a; source .env; set +a; }
    cargo run -p astar-server -- serve --config "{{config}}"

# Open the node daemon's TUI against a config.
node-tui config="node.toml":
    #!/usr/bin/env bash
    set -euo pipefail
    [ -f .env ] && { set -a; source .env; set +a; }
    cargo run -p astar-server -- tui --config "{{config}}"

# ── M17 ─────────────────────────────────────────────────────────────────────

# Run a self-hosted M17 parrot (echo test) reflector on [::]:<port>, dual-stack
# so both 127.0.0.1 and localhost reach it.
m17-parrot port module="A":
    cargo run -p astar-m17 --example m17_parrot -- --port {{port}} --module {{module}}

# ── Docs ────────────────────────────────────────────────────────────────────

# Serve the documentation site locally with live reload (http://localhost:8000).
docs:
    uvx zensical serve

# Build the documentation site (docs/site -> docs/.site), exactly as CI does.
docs-build:
    uvx zensical build --clean --strict

# ── CI mirrors ──────────────────────────────────────────────────────────────

# Deliberately NOT part of `just ci`: it needs the network to fetch the
# advisory database, and the everyday gate should stay runnable offline and
# answer only for code in this tree. CI runs it as its own job.
#
# RustSec advisory scan over Cargo.lock.
audit:
    @command -v cargo-audit >/dev/null 2>&1 || cargo install cargo-audit --locked
    cargo audit --deny warnings

# The everyday Rust gate: format, lint, test, header-drift.
ci: fmt-check clippy test cbindgen
    @echo "✓ ci: fmt + clippy + test + cbindgen passed"

# Everything, including the Swift side (needs a full Xcode).
ci-full: fmt-check clippy test cbindgen ffi-example python swift-fmt-check app-test
    @echo "✓ ci-full: rust + swift gates passed locally"
