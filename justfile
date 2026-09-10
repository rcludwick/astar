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

# Optimised build of the whole workspace. (`just release` cuts a RELEASE — see
# the Release section at the bottom of this file.)
build-release:
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

# System Fusion (iax-e8a4 link, astar-e7b3 §2 audio). Unlike `dstar-test`
# this needs no hardware at all: `astar-ysf` ships its own loopback
# reflector, so the link tests bind 127.0.0.1 and link to it, and the decode
# tests drive a fake vocoder rather than a dongle.
#
# YSF is ALREADY covered by `just ci` — `astar-sys` has `ysf` in its default
# features and Cargo unifies features across a workspace build, so
# `cargo test --workspace` compiles and runs all of this. This recipe is the
# way to run it alone, and the way to catch a break in the per-crate feature
# combinations that the unified build would hide.
ysf-test:
    cargo test -p astar-ysf
    cargo test -p astar-console --features ysf
    cargo test -p astar-station --features ysf
    cargo test -p astar-cli --features ysf

# Run a self-hosted YSF parrot reflector on [::]:<port>, dual-stack (so both
# 127.0.0.1 and localhost reach it). The System Fusion twin of `m17-parrot`.
#
# Key into it from astar and hear yourself, the way `m17-parrot` works for
# M17: astar transmits YSF in DN mode through the ThumbDV, and the parrot
# replays your own transmission back once you unkey. One dongle is enough —
# YSF here is half-duplex, so the replay decodes after the key-up ends. Any
# other transmitter works too: a radio through a hotspot, Pi-Star, DroidStar.
# Frames are relayed VERBATIM, so it hides no framing bug — what you hear is
# what astar actually put on the wire. Nothing goes on the air.
#
#     just ysf-parrot 42000            # terminal 1
#     just ysf-listen 127.0.0.1:42000 AJ7HR   # terminal 2, to watch
#     ...then key up, from astar or from the hotspot.
ysf-parrot port:
    cargo run -p astar-ysf --example ysf_parrot -- --port {{port}}

# The YSF hardware checkpoint — ROB RUNS THIS, not an agent.
#
# Needs a ThumbDV attached and a live reflector, so it is the one thing the
# rest of the YSF suites cannot answer: they prove bytes in and bytes out,
# this proves the result is speech. Receive only — `ysf-listen` has no PTT,
# so nothing here can go on the air.
#
# KC-Wide's two, from the design doc:
#     just ysf-listen ysf.kcwide.net:42000 AJ7HR      # US-KCWIDE
# Add --wav /tmp/ysf.wav to capture instead of play.
#
# What to watch for: "linked … (backend: thumbdv)", then a "▶ <callsign>"
# line per transmission with audible speech. A "!!" line means the reflector
# is sending VW or data, which astar does not decode — that is the refusal
# working, not a fault. Silence with a climbing frame count and no "!!" line
# would be the interesting failure: most likely V/D mode 1, where MMDVMHost
# and DroidStar disagree about the layout and astar follows MMDVMHost.
ysf-listen host callsign *args:
    cargo run --release -p astar-cli --features ysf -- ysf-listen {{host}} --callsign {{callsign}} {{args}}

# NXDN's hardware-free half: the protocol crate, the codec layer, the session
# and the facade. `astar-nxdn` and `astar-codec::nxdn` are already covered by
# `just ci`; the feature-gated halves are not.
nxdn-test:
    cargo test -p astar-nxdn
    cargo test -p astar-console --features nxdn
    cargo test -p astar-station --features nxdn
    cargo test -p astar-cli --features nxdn

# NXDN — hardware-free bench loop, then the hardware checkpoint.
#
#     just nxdn-parrot 41400 31313          # terminal 1
#     just nxdn-listen 127.0.0.1:41400 KC0ABC 4242 31313   # terminal 2
#
# astar does not transmit NXDN yet, so this is not the round-trip bench
# `ysf-parrot`/`m17-parrot` are. Its use today is a known stream on
# 127.0.0.1 from a real transmitter through a hotspot, or from the loopback
# tests — when transmit lands it becomes the round-trip bench.
nxdn-parrot port tg="31313":
    cargo run -p astar-nxdn --example nxdn_parrot -- --port {{port}} --tg {{tg}}

# Link an NXDNReflector talkgroup and decode the voice on it. RECEIVE ONLY —
# this command has no PTT, because astar has no NXDN transmit path yet.
# Add --wav /tmp/nxdn.wav to capture instead of play.
nxdn-listen host callsign radio_id tg *args:
    cargo run --release -p astar-cli --features nxdn -- nxdn-listen {{host}} --callsign {{callsign}} --radio-id {{radio_id}} --tg {{tg}} {{args}}

# DMR's hardware-free half: the protocol crate, the block codes, the codec
# layer, the session and the facade. `astar-dmr` and `astar-codec::dmr` are
# already covered by `just ci`; the feature-gated halves are not.
dmr-test:
    cargo test -p astar-dmr
    cargo test -p astar-console --features dmr
    cargo test -p astar-station --features dmr
    cargo test -p astar-cli --features dmr

# DMR — hardware-free bench loop, then the hardware checkpoint.
#
#     just dmr-parrot 62031                                        # terminal 1
#     ASTAR_DMR_PASSWORD=passw0rd just dmr-listen 127.0.0.1:62031 tgif KC0ABC 3153591 31313
#
# The parrot is a MASTER: it binds and waits, and performs the real
# RPTL/RPTK/RPTC handshake, so a client that gets the digest wrong fails here
# rather than against somebody's network.
#
# Neither half takes a password on the command line: a secret in argv is
# readable by every process on the machine and lands in shell history. The
# parrot reads ASTAR_DMR_PARROT_PASSWORD and falls back to its own built-in
# loopback default (which is what the ASTAR_DMR_PASSWORD above matches); the
# listener reads ASTAR_DMR_PASSWORD and has no default at all.
dmr-parrot port="62031":
    cargo run -p astar-dmr --example dmr_parrot -- --port {{port}}

# Log in to a DMR master, join a talkgroup on a timeslot, and decode the voice
# on it. RECEIVE ONLY — this command has no PTT, because astar has no DMR
# transmit path yet. The password comes from ASTAR_DMR_PASSWORD, never argv.
# Add --ts 1 for timeslot 1 (the default is TS2, the hotspot convention), and
# --wav /tmp/dmr.wav to capture instead of play.
dmr-listen host system callsign radio_id tg *args:
    cargo run --release -p astar-cli --features dmr -- dmr-listen {{host}} --system {{system}} --callsign {{callsign}} --radio-id {{radio_id}} --tg {{tg}} {{args}}

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

# Refresh the bundled reflector directory snapshot
# (apps/macos/Resources/reflectors.json → astar.app/Contents/Resources/).
# RUN THIS AT RELEASE TIME and commit the result: the snapshot is what a first
# launch with no network reads, and nothing else ever refreshes it.
reflectors:
    apps/macos/Tools/fetch-reflectors.sh

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
    # Generate the release manifest first: it is gitignored, so a fresh clone
    # has no docs/site/api/v1/releases.json for `serve` to pick up.
    python3 ci/version_manifest.py --write docs/site
    uvx zensical serve

# Build the documentation site (docs/site -> docs/.site), exactly as CI does —
# through ci/build-docs.sh, which is literally the script the Pages workflow
# runs, so a local build and a published build cannot diverge.
docs-build:
    ./ci/build-docs.sh

# NOT PUBLISHED, by two independent mechanisms: docs/superpowers/ is gitignored,
# and ci/build-docs.sh reads only zensical.toml. Read zensical.design.toml's
# header before changing either. A separate project and port from `just docs`,
# so both can run at once.
#
# Serve the internal design docs (specs, plans, notes) on localhost:8001.
design: design-stop
    #!/usr/bin/env bash
    set -euo pipefail
    python3 ci/design_index.py
    # pkill returns before the kernel releases the socket, so a immediate
    # bind loses to TIME_WAIT and reports "Address already in use" -- which
    # is the confusing error this recipe exists to stop producing.
    for _ in $(seq 1 40); do
      curl -sS -o /dev/null --max-time 1 http://localhost:8001/ 2>/dev/null || break
      sleep 0.25
    done
    uvx zensical serve -f zensical.design.toml -a localhost:8001 --open

# Stop a running design-docs server. `just design` runs this first.
design-stop:
    @pkill -f "zensical serve -f zensical.design.toml" 2>/dev/null && echo "stopped previous design server" || true

# Output is gitignored and never shipped; useful to check a page renders.
#
# Build the internal design-docs site once (docs/superpowers -> docs/.design-site).
design-build:
    python3 ci/design_index.py
    uvx zensical build -f zensical.design.toml

# The version is spelled in three places (apps/macos/project.yml's
# MARKETING_VERSION, zensical.toml's footer chip, CHANGELOG.md's newest
# heading). This fails if they disagree, and is part of `just ci`.
version-check:
    python3 ci/version_manifest.py --check
    python3 ci/test_version_manifest.py

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
ci: fmt-check clippy test cbindgen version-check release-test
    @echo "✓ ci: fmt + clippy + test + cbindgen + version-check + release-test passed"

# Everything, including the Swift side (needs a full Xcode).
ci-full: fmt-check clippy test cbindgen version-check ffi-example python swift-fmt-check app-test
    @echo "✓ ci-full: rust + swift gates passed locally"

# ── Release ─────────────────────────────────────────────────────────────────
#
# Two commands, and the split between them is deliberate: `release` goes as far
# as the PRIVATE repo and stops. Pushing to the public repo and creating the
# GitHub release is `publish`, which nobody runs on Rob's behalf — CLAUDE.md:
# publishing is his call, never a step in a task.
#
# Before either, by hand: write the `## <version> — <date>` section at the top
# of CHANGELOG.md (moving in whatever is waiting in
# docs/superpowers/notes/2026-09-07-pending-changelog.md) and commit it. The
# release refuses to run until that heading names the version being cut.
# The whole flow, with the recovery steps: docs/RELEASING.md.

# Cut a release: preconditions, version bump in all five homes, the reflector
# snapshot, ci + xcframework + app-test + dmg, then commit, tag and push to
# ORIGIN (the private repo). `just release 0.1.13-beta --dry-run` first — it
# prints every command and modifies nothing. `--skip-reflectors` skips the
# snapshot refresh (it needs the network).
release version *args:
    ci/release.sh {{version}} {{args}}

# THE DELIBERATE SECOND STEP, after `just release` and only when Rob says so:
# push main + the tag to PUBLIC and create the GitHub release with the signed,
# notarized astar.dmg attached, notes lifted from CHANGELOG.md, marked --latest.
# `just publish 0.1.13-beta --dry-run` shows the notes and pushes nothing.
publish version *args:
    ci/publish.sh {{version}} {{args}}

# Tests for ci/release.sh: a throwaway repo in a temp dir, no cargo/just/gh.
# Part of `just ci` — it is hermetic and takes about a second.
release-test:
    ./ci/test_release_sh.sh
