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
# Only TRUSTED sticks are ever opened: the hardware-gated tests read the USB
# serials from ASTAR_VOCODER_SERIALS=serial1,serial2 and open nothing without
# it. IAX_THUMBDV_PORT=/dev/cu.usbserial-XXXX then pins WHICH trusted stick to
# use when several are attached. It can only SELECT among trusted FTDI
# 0x0403:0x6015 ports — a pin outside them is refused, so it can never point at
# a USB radio interface's serial port, where opening the tty would assert RTS
# and key a transmitter.
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

# The eleven merge invariants CI's `guard` stage enforces: no armed hardware /
# live-network opt-in, no git dependencies (ambe-thumbdv stays vendored, with
# its licences), no invented install channel, an AGPL SPDX header on every
# first-party rs/swift/sh/py file, no LGPL code in a default build (Codec 2
# stays opt-in; the software D-Star vocoder stays opt-in and D-Star only), no async runtime / voice stack in a default build (the
# discord feature stays opt-in), a dependency-free Bluetooth PTT core that the
# node never links, with CoreBluetooth confined to one app file and no
# characteristic writes, no tiny_http anywhere (every HTTP server
# runs on astar-http), the EchoLink test fences (no EchoLink host name in
# the engine; the loopback-only features never reach a real build), no packet
# tap on a path that carries secrets, and the net recorder staying
# receive-only. Each prints one line; run them before a push.
guards:
    ./ci/guard-safety-env.sh
    ./ci/guard-no-git-deps.sh
    ./ci/guard-distribution-claims.sh
    ./ci/guard-spdx-headers.sh
    ./ci/guard-codec2-licensing.sh
    ./ci/guard-dstar-soft-ambe.sh
    ./ci/guard-discord-feature.sh
    ./ci/guard-ble-ptt.sh
    ./ci/guard-no-tiny-http.sh
    ./ci/guard-echolink.sh
    ./ci/guard-packet-log.sh
    ./ci/guard-netrec-receive-only.sh

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

# Refresh the bundled reflector and AllStar node directory snapshots
# (apps/macos/Resources/{reflectors,allstar}.json → astar.app/Contents/Resources/).
# RUN THIS AT RELEASE TIME and commit the result: the snapshot is what a first
# launch with no network reads, and nothing else ever refreshes it.
reflectors:
    apps/macos/Tools/fetch-reflectors.sh
    apps/macos/Tools/fetch-allstar.sh

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

# Offscreen screenshots of the /nets/ pages into crates/astar-netarchive/.shots/
# (headless Chrome, fixture archive in a temp dir). `--check` also tests the DOM.
nets-shots *args:
    crates/astar-netarchive/shots.sh {{args}}

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

# Checks: container, image tag, restart, image count, disk, /status
# registration, secrets in the log, UDP 4569 — and, with ASTAR_PUBLIC_HOST
# set, DNS, TLS and the public routes. Observation only: ssh read-only
# commands and HTTPS GETs. `--json` for machine output. deploy/README.md#verify.
# Verify the node VPS deploy (read-only; needs ASTAR_VPS=user@host).
deploy-verify *args:
    deploy/verify.sh {{args}}

# A fake box (fake-ssh) and a fake public bind on 127.0.0.1 behind a
# throwaway CA. No network; part of `just ci`.
# Tests for deploy/verify.sh (hermetic).
deploy-verify-test:
    deploy/test/test_verify.sh

# ── M17 ─────────────────────────────────────────────────────────────────────

# Run a self-hosted M17 parrot (echo test) reflector on [::]:<port>, dual-stack
# so both 127.0.0.1 and localhost reach it.
m17-parrot port module="A":
    cargo run -p astar-m17 --example m17_parrot -- --port {{port}} --module {{module}}

# ── nodebench ───────────────────────────────────────────────────────────────
#
# astar-server against a real AllStarLink 3 node in a podman pod (no radio;
# everything on 127.0.0.1). NOT part of `just ci`: it needs podman and the rig
# images (`harness/nodebench/rig.sh build`). On the Mac, set
# CONTAINER_CONNECTION=podman-machine-default. See harness/nodebench/RIG.md.

# Flags pass through, e.g. `just nodebench --keep --hold-secs 10 node_links_to_server`.
# Bring the rig up, run scenarios (default: all), report, tear down.
nodebench *args:
    cargo run -q -p astar-nodebench -- run {{args}}

# Bring the nodebench pod up and leave it running (rig.sh up options pass through).
nodebench-up *args:
    harness/nodebench/rig.sh up {{args}}

# Tear the nodebench pod down.
nodebench-down:
    harness/nodebench/rig.sh down

# Flags pass through, e.g. `just nodebench-soak 2 --topology nat --udp-timeout 20`;
# Ctrl-C stops it and still writes summary.json; `--resume DIR` carries on.
# Hold one link for `hours` and log every drop (soak.jsonl + summary.json).
nodebench-soak hours="24" *args:
    cargo run -q -p astar-nodebench -- soak --hours {{hours}} {{args}}

# Browse runs and soaks at http://127.0.0.1:8740 (loopback only).
nodebench-serve *args:
    cargo run -q -p astar-nodebench -- serve {{args}}

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

# The Discord bridge, which `just ci` cannot reach: `discord` is off by
# default, so a default-features clippy and test compile none of
# songbird_link.rs, none of astar-server's `#[cfg(feature = "discord")]`
# items, and none of the feature-gated tests. This is the local twin of
# ci.yml's `discord` job — deliberately NOT in `just ci`, because the feature
# resolves ~200 extra packages (songbird, serenity, tokio) and the everyday
# gate should stay fast. Run it when you touch the bridge.
discord-check:
    cargo clippy -p astar-server -p astar-discord --all-targets --features discord -- -D warnings
    cargo test -p astar-discord --features discord
    cargo test -p astar-server  --features discord
    @echo "✓ discord-check: clippy + tests with --features discord passed"

# Mutation testing for the PTT safety core: crates/astar-ble-ptt and
# crates/astar-station/src/ptt_arbiter.rs. The bar is zero survivors: every
# mutant cargo-mutants makes must turn a test red. Too slow for `just ci`;
# run it before merging any change to either. Output lands in
# target/mutants.out (astar-ble-ptt) and target/mutants-ptt-arbiter/mutants.out
# (the arbiter). Requires cargo-mutants 27.1.0.
mutants-ble-ptt:
    cargo mutants -p astar-ble-ptt --output target
    cargo mutants -p astar-station --file ptt_arbiter.rs --output target/mutants-ptt-arbiter

# The everyday Rust gate: format, lint, test, header-drift.
# `guard-codec2-licensing.sh` and the rest of `just guards` are NOT in this
# chain (they run as a separate pre-push step) — guard-discord-feature.sh IS,
# deliberately, so a default-features build staying tokio/songbird-free is
# checked on every `just ci`, not just before a push. The feature's own code
# is gated by `just discord-check` / ci.yml's `discord` job, not here.
# guard-ble-ptt.sh is in the chain for the same reason: the Bluetooth PTT core
# staying dependency-free and out of the node is a safety property.
ci: fmt-check clippy test cbindgen version-check release-test deploy-verify-test public-check dns-check auth-check http-check gsm-check soft-ambe-check
    ./ci/guard-discord-feature.sh
    ./ci/guard-no-tiny-http.sh
    ./ci/guard-echolink.sh
    ./ci/guard-packet-log.sh
    ./ci/guard-netrec-receive-only.sh
    ./ci/guard-ble-ptt.sh
    @echo "✓ ci: fmt + clippy + test + cbindgen + version-check + release-test + deploy-verify-test + public-check + dns-check + auth-check + http-check + gsm-check + soft-ambe-check + discord-guard + tiny-http-guard + echolink-guard + packet-log-guard + netrec-guard + ble-ptt-guard passed"

# astar-server's opt-in public-tls (HTTPS, ACME, Porkbun) and stream-mp3 (LAME)
# features, which a default-features `just clippy`/`just test` never compiles. Unlike discord it
# adds no runtime and resolves few packages, so it rides in `just ci`. The
# Pebble tests inside skip unless podman answers and `just acme-test-images`
# has pulled the images; with CONTAINER_CONNECTION set they run for real.
public-check:
    cargo clippy -p astar-server --all-targets --features public-tls,stream-mp3 -- -D warnings
    cargo test -p astar-server --features public-tls,stream-mp3 --lib -- public:: stream::
    @echo "✓ public-check: clippy + public::/stream:: tests with --features public-tls,stream-mp3 passed"

# astar-codec's opt-in `gsm` feature (GSM 06.10 over oxideav-gsm), which a
# default `just clippy`/`just test` never compiles. Two small pure-Rust crates,
# so it rides in `just ci`. tests/gsm_etsi.rs skips unless ASTAR_GSM_ETSI_DIR
# points at the ETSI EN 300 961 vectors; tests/gsm_golden.rs always runs.
gsm-check:
    cargo clippy -p astar-codec --all-targets --features gsm -- -D warnings
    cargo test -p astar-codec --features gsm
    @echo "✓ gsm-check: clippy + tests with --features gsm passed"

# The software D-Star vocoder (astar-dstar-perens, feature `dstar-soft-ambe`,
# off by default), which a default `just clippy`/`just test` never compiles.
# One small pure-Rust crate with no dependencies, so it rides in `just ci`:
# the selection tests, the encode/decode round trip, and the console session
# opening a software D-Star link to 127.0.0.1 with nothing trusted.
soft-ambe-check:
    cargo clippy -p astar-codec -p astar-console -p astar-station -p astar-sys -p astar-server --all-targets --features astar-codec/dstar-soft-ambe,astar-console/dstar,astar-console/dstar-soft-ambe,astar-station/dstar-soft-ambe,astar-sys/dstar-soft-ambe,astar-server/dstar-soft-ambe -- -D warnings
    cargo test -p astar-codec --features dstar-soft-ambe --lib
    cargo test -p astar-console --features dstar,dstar-soft-ambe --lib -- software_ dstar_vocoder
    cargo test -p astar-sys --features dstar-soft-ambe --lib -- dstar_vocoder
    cargo test -p astar-server --features dstar-soft-ambe --lib -- vocoder
    cargo clippy -p astar-netrec --all-targets --features release -- -D warnings
    cargo test -p astar-netrec --features release
    ./ci/guard-dstar-soft-ambe.sh
    @echo "✓ soft-ambe-check: clippy + tests with --features dstar-soft-ambe passed"

# The feature combinations `just ci` never builds: clippy for every crate with
# --all-features and with each feature alone (no defaults), full tests for
# astar-cli, astar-console and astar-server with --all-features and for
# astar-server without `stream`, and the server's `release` set. Not part of
# `just ci`: it takes several times as long (see ci/feature-matrix.sh for the
# steps, the exclusions and why). `just feature-matrix --list` prints the
# steps; `just feature-matrix <regex>` runs the matching ones.
feature-matrix *args:
    ./ci/feature-matrix.sh {{args}}

# astar-server's opt-in dns-providers feature (dns-update on a contained
# tokio thread). Not in default clippy/test; its tests use only dns-update's
# in-memory provider and challtestsrv on 127.0.0.1.
dns-check:
    cargo clippy -p astar-server --all-targets --features dns-providers -- -D warnings
    cargo test -p astar-server --features dns-providers --lib -- public::
    cargo test -p astar-server --features dns-providers --lib -- config::
    @echo "✓ dns-check: clippy + public:: and config:: tests with --features dns-providers passed"

# astar-http's opt-in features (tls, ws), which a default `just clippy`/
# `just test` never compiles: clippy for each alone and together, tests with
# both. (`testing` joins in, as --all-features, once it exists.)
http-check:
    cargo clippy -p astar-http --all-targets --features tls -- -D warnings
    cargo clippy -p astar-http --all-targets --features ws -- -D warnings
    cargo clippy -p astar-http --all-targets --features tls,ws -- -D warnings
    cargo clippy -p astar-http --all-targets --all-features -- -D warnings
    cargo test -p astar-http --all-features
    @echo "✓ http-check: astar-http clippy (tls, ws, tls+ws, all features) + tests with all features passed"

# astar-server's opt-in `auth` feature (passkey, security-key and TOTP
# sign-in), which a default-features `just clippy`/`just test` never
# compiles. Its WebAuthn tests run against software authenticators only.
auth-check:
    cargo clippy -p astar-server --all-targets --features auth -- -D warnings
    cargo test -p astar-server --features auth
    @echo "✓ auth-check: clippy + astar-server tests with --features auth passed"

# The single static release executable (Task 14): `--no-default-features
# --features release`, a static musl binary with LAME, Codec 2, the Opus
# codec, rustls and the pages inside, and no shared-library dependency.
# Cross-built on the host with cargo-zigbuild (the podman machine cannot run
# amd64 rustc), so it needs zig, cargo-zigbuild and cmake (the discord
# feature's libopus). Writes target/server-release/<target>/astar-server and
# runs ci/check-server-release.sh on it. Targets: x86_64-unknown-linux-musl
# (the IONOS node) and aarch64-unknown-linux-musl.
server-release target="x86_64-unknown-linux-musl":
    rustup target list --installed | grep -qx {{target}} || rustup target add {{target}}
    cargo zigbuild --release -p astar-server --no-default-features --features release --target {{target}}
    mkdir -p target/server-release/{{target}}
    cp target/{{target}}/release/astar-server target/server-release/{{target}}/astar-server
    ci/check-server-release.sh target/server-release/{{target}}/astar-server

# Pull the two images the ACME tests run (Pebble and its fake DNS server).
# The tests never pull on their own: this is the one step that downloads.
acme-test-images:
    podman pull ghcr.io/letsencrypt/pebble:latest
    podman pull ghcr.io/letsencrypt/pebble-challtestsrv:latest

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

# Remove build output a published release no longer needs (`just publish`
# runs this itself afterwards). Keeps the Debug app, the xcframeworks and any
# unmerged or dirty worktree. `--dry-run` lists what it would remove.
clean-builds *args:
    ci/clean-builds.sh {{args}}

# Keep this checkout's target/ and apps/macos/build/ on another drive, behind
# symlinks: `just link-builds /Volumes/data/builds` (or set ASTAR_BUILD_ROOT).
# Run it once in each new worktree; it moves anything already built.
link-builds root="":
    ci/link-builds.sh {{root}}

# Tests for both release scripts: a throwaway repo in a temp dir, with gh /
# spctl / stapler stubbed on PATH for the publish half. No cargo, no network,
# nothing real is ever pushed. Part of `just ci` — hermetic, about two seconds.
release-test:
    ./ci/test_release_sh.sh
    ./ci/test_publish_sh.sh
    ./ci/test_clean_builds_sh.sh
