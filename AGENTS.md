# astar

Native multi-network ham-radio client and node: AllStarLink (IAX2), M17, and
D-Star. One cargo workspace, one git history. AGPL-3.0-only throughout, except
the vendored `vendor/ambe-thumbdv` (MIT/Apache-2.0).

Design polish is a first-class requirement: astar should be pretty and easy to
use, not just functional.

## Layout

```
crates/          astar-lib — the engine (protocol, codecs, audio, PTT, station
                 facade, C ABI). No UI. Includes crates/astar-server, the node
                 daemon.
bindings/        The engine's own bindings: swift (AstarStation),
                 swift-serial (AstarSerial), python.
apps/macos/      The SwiftUI app (macOS menu-bar; iOS targeted).
apps/gui/        The Iced client for Windows/Linux (crate astar-gui).
vendor/          ambe-thumbdv — the ThumbDV vocoder driver, MIT/Apache island.
docs/            Engine docs + docs/app/ for the app's own backlog/design.
```

**Fat core, thin views.** All protocol/audio/PTT logic lives in `crates/`. The
macOS and Iced clients are views over the same engine; every feature ships on
all platforms with per-platform native UI, same look and feel.

## Build / test / run

```
just ci          # fmt + clippy + test + C-header drift.  Must be green.
just xcframework # builds BOTH Swift xcframeworks — always both; a stale cache
                 # lies. Required once on a fresh checkout/worktree.
just app         # build the macOS app        just run   # build + launch
just app-test    # AstarCore SwiftPM tests    just dmg   # local .dmg
just gui-linux / just gui-windows             # Iced client cross-checks
just dstar-test  # hardware-free D-Star half
```

There is no vendoring step and no sibling repo any more: `apps/macos/project.yml`
points straight at `bindings/swift` and `bindings/swift-serial`. A fresh worktree
just runs `just xcframework` — no symlinks, and the old "never `git add -A`"
landmine is gone.

`bindings/swift/Package.swift` picks its linking path by whether the xcframework
is on disk (binaryTarget if present, systemLibrary + raw `.a` if not). The app's
build scripts hard-fail when it is missing rather than dropping to that fallback
silently.

After changing any `extern "C"` fn / `#[repr(C)]` type, run `just cbindgen`; the
committed `astar.h` / `astarserial.h` must not drift.

## Safety rules — these are load-bearing

* **Never transmit on the air autonomously.** Connecting to live nodes and
  keying PTT are Rob's manual actions. Agents observe and measure only. No test
  may transmit anywhere except `127.0.0.1`.
* **`IAX_THUMBDV_PORT` narrows, never replaces, the FTDI VID/PID scan**
  (`thumbdv_candidate_ports_from` in `crates/astar-codec/src/ambe.rs`). Pointing
  the opener at a USB radio interface's tty asserts RTS and keys a transmitter.
  The tests that pin this behaviour must keep passing.
* **`astar-server` exposes remote keying** (`POST /key`). Its `NodeCommand::Key`
  handler refuses while the snapshot reports `dstar_active`
  (`key_refusal` in `crates/astar-server/src/controller.rs`). That guard stays.
* **Secrets** (node secret, portal pass) are connect/init in-args ONLY: never
  stored on a Station, never in snapshots/events/errors, never logged.
* Hardware-gated suites (`IAX_THUMBDV_TESTS=1`) are never run by an agent.

## D-Star / AMBE

D-Star is hardware-only: the ThumbDV / DV3000 dongle via `vendor/ambe-thumbdv`.
There is **no** software AMBE backend — the old `ambe-soft` feature and its
`SoftAmbe` type were removed, and there is no dependency on the external `ambe`
repo. `astar-codec`'s `ambe-hw` feature is off by default.

`vendor/ambe-thumbdv/Cargo.toml` hard-codes `license`/`version`/`edition`/
`repository` on purpose. Do not convert them to `.workspace = true` — that would
relicense someone else's MIT/Apache code as AGPL. See its `VENDORED.md`.

## Distribution reality

Nothing has ever been released. There is **no** Homebrew tap or cask for astar,
no App Store listing, no download URL, no published binary. The only honest
install path is building from source; `just dmg` makes a local, unsigned image.
Do not write install instructions that imply otherwise.

The only cask astar ever mentions is WCH's CH34x USB serial driver, and it is
now **optional** — see Hardware notes.

## Hardware notes

PTT hardware targets the generic class of USB radio interfaces (serial PTT +
USB audio); the AllScan UCI150 (WCH CH343) is the reference device, not a
special case — don't hard-code it.

**Raw USB is the default transport and needs no driver** (iax-c7e1):
`SerialConfig.transport` defaults to `.usb`, and the three fallback sites — the
binding, `SerialLineSpec+Config`, `SerialSettings` — all inherit that, so a
config with no transport recorded lands on the driver-free path. A config that
names `.tty` still gets `.tty`; nobody is migrated. `testDefaultTransportIsUsb`
pins this, and it matters for more than convenience: opening a USB radio
interface's tty asserts RTS, which is the radio-key line.

The tty path is opt-in, has no UI (only the `serial.transport` default reaches
it), and on macOS needs WCH's driver:
`brew install --cask wch-ch34x-usb-serial-driver`. PTT DEST switch = CTS.

All serial I/O runs on a Rust worker thread: a wedged USB transfer surfaces as
"Serial device error (disabled)", never a UI hang.

Surface audio/PTT config layered: sensible defaults and common controls up
front, advanced device/serial tuning under progressive disclosure. Don't hide
capability the station offers.

## Work tracking

Use the Claude Code task tracker for the current session. The durable backlog is
`docs/BACKLOG.md` (engine) and `docs/app/BACKLOG.md` (clients) — anything that
outlives the session goes there.

**Always use a per-item git worktree for implementation work** — never work
directly on the default branch in the main checkout, which is reserved for
merging and doc commits. Check `git worktree list` first.

## Conventions

* No Claude/AI attribution anywhere — not in commits, merges, PRs, code
  comments, docs, or CI files. Never write "generated by".
* macOS shell quirks: no `timeout` command; zsh chokes on bare `==`/`===`
  arguments; the Bash tool's cwd resets between calls — use absolute paths.
* Apple HIG, SF Symbols, full light/dark, accessibility, instant-feeling
  interactions. Hold all UI work to that bar.
