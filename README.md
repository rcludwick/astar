<p align="center">
  <img src="apps/gui/assets/icon/astar-256.png" alt="astar" width="128" height="128">
</p>

<h1 align="center">astar</h1>

<p align="center">
  A native ham-radio digital-voice client and node —
  <strong>AllStarLink (IAX2)</strong> and <strong>M17</strong>,
  in one Rust engine with native front-ends.
</p>

<p align="center">
  <strong>0.1.0beta</strong> · AGPL-3.0-only · macOS today, Windows and Linux in progress
</p>

<p align="center">
  <a href="https://rcludwick.github.io/astar/"><strong>Documentation</strong></a> ·
  <a href="https://rcludwick.github.io/astar/build/">Build from source</a> ·
  <a href="https://rcludwick.github.io/astar/macos/hardware/">Hardware</a> ·
  <a href="https://rcludwick.github.io/astar/server/">astar-server</a>
</p>

---

astar dials nodes and reflectors as a client — audio, push-to-talk, DTMF, live
meters — with support for generic USB radio interfaces (serial PTT + USB audio;
the AllScan UCI150 is the reference device). It also runs as an always-on node
daemon.

> ### Nothing here has been released yet
>
> There is no Homebrew tap, no cask, no App Store listing, no notarized
> download, and no published binary of any kind. **The only way to run astar
> today is to build it from this repository.** See [Building](#building).
>
> This is a `0.1.0beta` of a project that has never shipped. Expect rough
> edges, expect things to move.

---

## What works today

Be aware that "the engine supports it" and "you can click it in the app" are
two different things right now. This is the honest state:

| | AllStar (IAX2) | M17 |
|---|---|---|
| **Engine** (`crates/`) | yes | yes |
| **macOS app** (`apps/macos`) | yes | yes¹ |
| **Iced client** (`apps/gui`) | yes | yes¹ |
| **CLI** (`astar-cli`) | yes | **no** — IAX2 only |

¹ M17 is capability-gated: the client shows it only when the running build can
actually place the call. On macOS that currently means a system `libcodec2` —
see [M17 and Codec 2](#m17-and-codec-2).

M17 is **compiled in by default** everywhere it is implemented — the engine,
the C ABI the macOS app links, the Iced client and the node daemon all get it
without a feature flag, and a test pins that so it cannot regress silently.
`astar-cli` is the exception: it sits directly on `astar-iax` and has no M17
subcommand at all.

Other networks are in the tree at various stages and are **not** claimed as
working yet. You will find crates for them; treat their presence as work in
progress rather than a feature list.

### Platform state

* **macOS** — the primary front-end. A menu-bar app (`MenuBarExtra`), no Dock
  icon, no main window. This is the one that gets the design attention.
* **Linux** — the Iced client builds, tests and runs headless in CI-style
  container runs (`just gui-linux`), and its rendering is verified from
  screenshots. It has not had a sit-down-and-use-it pass on real hardware.
* **Windows** — the Iced client cross-compiles and links (`just gui-windows`).
  Runtime verification on an actual Windows machine is still an open backlog
  item. Treat it as unproven.
* **iOS** — the app target exists and builds multiplatform from day one, but
  no iOS UI work has happened.

---

## What is in this repo

One cargo workspace, one git history, three deliverables:

| Path | What it is |
|---|---|
| `crates/` | **astar-lib** — the engine. Protocol, codecs, audio, PTT, the multi-network station facade, and the C ABI. Pure Rust, no UI. |
| `crates/astar-server` | **astar-server** — the always-on node daemon (inbound IAX2 + local handset bridge), with an HTTP control plane and a TUI. |
| `apps/macos` | **astar** — the SwiftUI app (macOS menu-bar today, iOS targeted). |
| `apps/gui` | **astar** — the Iced client for Windows and Linux, same intended feature set. |
| `bindings/` | The engine's own bindings: Swift (`AstarStation`, `AstarSerial`) and Python. |
| `vendor/ambe-thumbdv` | The ThumbDV / DV3000 AMBE vocoder driver. An MIT/Apache-2.0 island — see [Licence](#licence). |

**Architecture rule: fat core, thin views.** All protocol / audio / PTT logic
lives in `crates/`. The macOS and Iced clients are views over the same engine,
so a feature written once is meant to reach every platform, with per-platform
native UI rather than a shared web shell.

### Crate map

| Crate | Role |
|---|---|
| `astar-iax-core` | IAX2 wire framing + session FSM. No I/O. |
| `astar-iax` | The high-level IAX2 client stack over `astar-iax-core`. |
| `astar-codec` | G.711 / GSM / Speex / iLBC, Codec 2, plus the AMBE (ThumbDV) backend. |
| `astar-audio` | cpal device I/O, network-agnostic. |
| `astar-station` | The multi-network station facade the clients drive. |
| `astar-console` | Front-end-agnostic operator-console core. |
| `astar-asl3` / `astar-m17` / `astar-dstar` | Per-network service layers. |
| `astar-ptt` | Pluggable PTT backends (serial, HID, VOX, UI). |
| `astar-wireguard` | Userspace WireGuard link transport. |
| `astar-sys` / `astar-serial-sys` | The C ABI (`astar.h`, `astarserial.h`) the Swift/Python bindings consume. |
| `astar-cli` | Command-line front-end. |
| `astar-inspect` | Web console harness. |
| `astar-conformance` | Asterisk-parity / pcap conformance harness. |
| `astar-server` | The node daemon. |
| `astar-gui` (`apps/gui`) | The Iced client. |

---

## Building

Everything is built from source. `just` is the command palette — run `just` on
its own to list every recipe.

> **Full build documentation:**
> [rcludwick.github.io/astar/build/](https://rcludwick.github.io/astar/build/)
> — prerequisites per operating system, each of the three deliverables, the
> verification gates, and what to do when one goes red. What follows here is
> the short version.

### The Rust engine (all platforms)

```bash
cargo build --workspace
cargo test  --workspace
just ci                 # fmt + clippy + test + C-header drift
```

Needs a stable Rust toolchain (`rustup`; MSRV **1.89**, edition 2024) and, on
Linux, `libasound2-dev` + `libudev-dev`. The 1.89 floor comes from the Iced
client's dependency graph; the engine crates on their own build on 1.86.

### The macOS app

macOS 13+ (`MenuBarExtra` is the floor), plus:

| Requirement | Why | Install |
|---|---|---|
| Full **Xcode** | `xcodebuild` and the SwiftUI/AppKit SDKs. Command Line Tools alone are not enough. | App Store, then `sudo xcode-select -s /Applications/Xcode.app` |
| **XcodeGen** | Generates the (gitignored) `astar.xcodeproj` from `apps/macos/project.yml`. | `brew install xcodegen` |
| **just** | Task runner. | `brew install just` |
| **Rust toolchain** | The app links the engine through the Swift binding. | `curl https://sh.rustup.rs -sSf \| sh` |

```bash
git clone <this repo> astar && cd astar

just xcframework    # builds BOTH Swift xcframeworks — always both
just run            # xcodegen → build → launch
```

`just xcframework` is the one step a fresh checkout must not skip. The two
xcframeworks are large and fully regenerable, so they are gitignored; `just
app` and `just run` hard-fail with instructions when they are missing, rather
than silently linking some host-only fallback.

After launch, look for the **rainbow asterisk** in the menu bar — astar has no
Dock icon and no main window. Left-click opens the dial popover. The running
version (`0.1.0beta`) is shown in the popover footer, so you can always tell
what you are actually running.

### A local .dmg

```bash
just dmg            # → apps/macos/build/astar.dmg
```

This is a **local, unsigned, un-notarized** disk image for your own machine.
It is not a release, and it is not something to hand to anyone else: macOS
Gatekeeper will refuse it on any other Mac. astar publishes no binaries
anywhere.

### The Windows / Linux client

```bash
cargo run -p astar-gui          # native, on the host
just gui-linux                  # Linux build+test+headless run in podman
just gui-windows                # cross-compile a Windows .exe (cargo-xwin)
```

### The node daemon

```bash
cp deploy/node.toml.example node.toml   # then edit
just node                               # astar-server serve --config node.toml
```

`node.toml` is **secret-free by design**: the registration secret comes from
the environment (`ALLSTAR_SECRET`) or is POSTed to the loopback control port
at runtime. Never put a password in it.

---

## Hardware and codecs

### USB radio interfaces

astar targets the generic class of USB radio interfaces (serial PTT + USB
audio). The AllScan UCI150 (WCH CH343) is the reference device, not a special
case. Two ways in:

* **Raw USB backend — the default.** No driver, no system extension. A fresh
  install keys a radio without installing anything.
* **tty backend — opt-in.** A `/dev/cu.*` port, chosen deliberately in the
  serial settings. On macOS a CH34x-class adapter then needs WCH's driver:

  ```bash
  brew install --cask wch-ch34x-usb-serial-driver
  ```

  A third-party cask, and the only one astar has anything to do with — there
  is no cask for astar itself.

On the UCI150, the PTT DEST switch selects CTS.

All serial I/O runs on a Rust-side worker thread, so a wedged USB transfer
surfaces as a "serial device error", never a UI hang.

### M17 and Codec 2

M17 needs Codec 2. The engine looks for a system `libcodec2` at runtime
(`/opt/homebrew/lib`, `/usr/local/lib`, `/usr/lib`, or a path in
`IAX_CODEC2_PATH`) and reports M17 as unavailable when it finds none — which
is why the network picker can hide M17 on an otherwise working build. On macOS
that means a Homebrew `codec2` today. Bundling the library so M17 works out of
the box is an open backlog item.

---

## Transmitting

**Never transmit on the air autonomously.**

Connecting to live nodes and keying PTT are deliberate human actions. No
automated process in this repository may key a transmitter, connect to a live
reflector or node, or reach anything outside `127.0.0.1`; the
hardware-touching test suites skip unless `IAX_THUMBDV_TESTS=1` is set
explicitly.

`IAX_THUMBDV_PORT`, which selects among attached vocoder dongles, only ever
**narrows** the USB VID/PID scan — it can never point the opener at an
arbitrary serial port. That restriction is load-bearing rather than tidy:
opening a USB radio interface's tty asserts RTS, and RTS keys a transmitter.

Operating astar on the air requires an appropriate amateur radio licence for
your jurisdiction, and you are responsible for what your station transmits.

---

## Secrets

Node secrets and portal passwords are **connect/init arguments only**. They
are never stored on a station, never present in snapshots, events or errors,
and never logged. The C-header check (`just cbindgen`) fails the build if a
secret-shaped field appears in an out-struct.

---

## Licence

**AGPL-3.0-only.** Copyright (c) 2026 Rob Ludwick. The full text is in
[`LICENSE`](LICENSE).

The engine and the applications are now under one licence. The apps were
previously proprietary; as sole copyright holder Rob has relicensed them under
the AGPL, and no file in this repository carries proprietary terms any more.

Because it is the *Affero* GPL: if you modify astar and let other people use it
over a network — a hosted node, a bridge, a web front-end — those users are
entitled to the source of your modified version.

### Third-party components

These keep their own terms and are **not** covered by the AGPL:

| Path | Component | Licence |
|---|---|---|
| `vendor/ambe-thumbdv` | ThumbDV / DV3000 AMBE driver (Rob's own, vendored in) | MIT **OR** Apache-2.0 — see its `LICENSE-MIT`, `LICENSE-APACHE`, and `VENDORED.md` |
| `apps/gui/assets/fonts` | Inter typeface | SIL Open Font License 1.1 |
| `harness/asterisk_parity/c_iaxclient/vendored/libiax2` | The historical C libiax2, used only as a parity reference by the test harness — never linked into a shipped binary | GPL / LGPL, see its `COPYING` and `COPYING.LIB` |

Everything else — every `astar-*` crate, both applications, the bindings, the
scripts and the docs — is AGPL-3.0-only. Every first-party `.rs`, `.swift`,
`.sh` and `.py` file carries the header, and `ci/guard-spdx-headers.sh` fails the pipeline
if one goes missing:

```
SPDX-License-Identifier: AGPL-3.0-only
```

The docs and other prose are covered by the root `LICENSE` rather than by
per-file headers.

---

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). The short version: AGPL-3.0-only,
`just ci` green before anything lands, and nothing goes on the air without a
human deciding to key it.
