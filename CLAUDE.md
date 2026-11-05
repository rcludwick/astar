# astar

Native multi-network ham-radio client and node: AllStarLink (IAX2), M17, and
D-Star. One cargo workspace, one git history. AGPL-3.0-only throughout, except
the vendored licence islands: `vendor/ambe-thumbdv` (MIT/Apache-2.0),
`vendor/codec2` (LGPL-2.1-only AND MIT), `vendor/ham-digital-modes-ambe`
(Bruce Perens' software D-Star vocoder, LGPL-3.0-or-later, no AMBE+2 code), and
`vendor/mp3lame-sys` + `vendor/mp3lame-encoder` (LAME, LGPL; astar-server's
opt-in `stream-mp3` only).

Design polish is a first-class requirement: astar should be pretty and easy to
use, not just functional.

## Two remotes — check before you push

| remote | repository | |
| --- | --- | --- |
| `origin` | `rcludwick/astar-private` | **private.** Development happens here. Push here. CI runs here. |
| `public` | `rcludwick/astar` | **public.** The release target — Rob merges to it deliberately. |

Everyday work goes to `origin`. **Publishing is Rob's call, not a step in a
task**: never `git push public` unless he asks for it in those words.

**The two repos do not share history** (Rob, 2026-11-01: "Private should stay
private always"). Public `main` is one squashed commit per release, carrying
the release tag's files and dated at a random evening time (18:00–22:59 MST)
on the tag's day; `just publish` builds and pushes it. It refuses to push Monday–Friday,
7 AM–5 PM MST. Never push a private
commit, branch or tag to `public` by any other route, and never fetch
public's tags into this clone: they name different commits from the private
tags of the same names (`remote.public.tagOpt=--no-tags`).

Consequences worth knowing:

* The self-hosted IONOS runner is attached to the **private** repo only. A
  self-hosted runner on a public repo would hand any fork's pull request
  arbitrary code execution on that box. Both workflow files exist in both
  repos and are guarded on `github.repository`, so each is inert in the wrong
  one — `ci.yml` works only in private, `docs-pages.yml` only in public.
* Anything committed to `origin` is still *going* to be public eventually.
  Secrets, node numbers and personal addresses do not belong in either.

## Layout

```
crates/          astar-lib — the engine (protocol, codecs, audio, PTT, station
                 facade, C ABI). No UI. Includes crates/astar-server, the node
                 daemon.
bindings/        The engine's own bindings: swift (AstarStation),
                 swift-serial (AstarSerial), python.
apps/macos/      The SwiftUI app (macOS menu-bar; iOS targeted).
apps/gui/        The Iced client for Windows/Linux (crate astar-gui).
vendor/          ambe-thumbdv — the ThumbDV vocoder driver, MIT/Apache island;
                 codec2 — the M17 voice codec (pure Rust), LGPL-2.1/MIT island;
                 ham-digital-modes-ambe — Bruce Perens' D-Star AMBE vocoder, LGPL-3.0 island;
                 mp3lame-sys / mp3lame-encoder — LAME for /stream.mp3, LGPL island.
docs/            Engine docs + docs/app/ for the app's own backlog/design.
```

**Fat core, thin views.** All protocol/audio/PTT logic lives in `crates/`. The
macOS and Iced clients are views over the same engine; every feature ships on
all platforms with per-platform native UI, same look and feel.

**Iced is being replaced** (decided 2026-10-08, Rob): its look is not good
enough, and the goal is a complete look-and-feel clone of the macOS app on
Linux, Windows, iOS and Android (`astar-uiclone` in `docs/app/BACKLOG.md`).
Until that lands, `apps/gui` still builds and ships, so keep it compiling and
green.

**The Windows/Linux client is paused** (Rob, 2026-10-08): it is not being
tested or published. New features skip it, and their `docs/FEATURES.md` row
marks Windows/Linux "paused" rather than leaving a parity defect. Its parity
debt is owed to the `astar-uiclone` replacement, not to Iced. Keep the shared
logic in the engine, so that replacement inherits it.

## Build / test / run

```
just ci          # fmt + clippy + test + C-header drift.  Must be green.
just xcframework # builds BOTH Swift xcframeworks — always both; a stale cache
                 # lies. Required once on a fresh checkout/worktree.
just app         # build the macOS app        just run   # build + launch
just app-test    # AstarCore SwiftPM tests    just dmg   # local .dmg
just gui-linux / just gui-windows             # Iced client cross-checks
just dstar-test  # hardware-free D-Star half
just nodebench   # astar-server vs a real ASL3 node in podman (not in `just ci`;
                 # `harness/nodebench/rig.sh build` first; see its README.md)
just nodebench-up / just nodebench-down       # leave the rig up / tear it down
just nodebench-soak 24 / just nodebench-serve # hold one link for hours / browse
                 # runs on 127.0.0.1:8740
```

Build output can live on another drive: `just link-builds <root>` (or
`ASTAR_BUILD_ROOT`) moves `target/` and `apps/macos/build/` to
`<root>/astar/<checkout>/` behind symlinks. Rob's Mac keeps them under
`/Volumes/data/builds`, so run `just link-builds /Volumes/data/builds` in each
new worktree before its first build. `just clean-builds` removes a merged
worktree's linked folders with it and sweeps folders whose worktree is gone.

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
* **nodebench links astar-server to stock AllStarLink software, and it
  stays sealed** (`harness/nodebench/`). Its ASL3 node and
  astar-server run in podman, with no radio, sound card or serial device
  anywhere in the rig. The quick tier stays on 127.0.0.1 inside one pod.
  The exception, which Rob approved on 2026-10-11, is the soak tier's `nat`
  topology: it runs on podman `--internal` networks, which have no route
  out, rather than on 127.0.0.1. The rig never queries AllStar DNS and never
  registers with the portal, and its nodes are the test numbers 1999 and
  1998.

## D-Star / AMBE

The ThumbDV / DV3000 dongle via `vendor/ambe-thumbdv` is the default vocoder
everywhere, and `astar-codec`'s `ambe-hw` feature is off by default.

**Software vocoders are permitted where the patent term has expired and the
licence is AGPL-compatible — which today means D-Star's AMBE and nothing
else.** Decided 2026-09-19 (Rob), reversing the previous hardware-only rule.

**Built in since 2026-11-01** (Rob: "We want to build it in by default. But we
want a switch for d-star to use the dongle (dvstick) or bruce perens' codec in
the config."). Bruce Perens' D-Star vocoder (`vendor/ham-digital-modes-ambe`,
feature `dstar-soft-ambe`) is in every shipped build: `just xcframework`
builds it in (`ASTAR_DSTAR_SOFT_AMBE=0` leaves it out) and astar-server's
`release` set includes it. The cargo feature itself stays explicit, never a
cargo default, like `codec2-static`. The **dongle stays the default choice**:
Settings → Vocoders (`vocoder.dstar`) and `node.toml` `[vocoder] dstar =
"dongle" | "software"`, and nothing switches by itself. D-Star only:
`ci/guard-dstar-soft-ambe.sh` enforces no AMBE+2 source, D-Star-only
permission, the hard-coded licence and the shipped-build opt-in.

| Generation | Modes | Status |
|---|---|---|
| AMBE 3600×2400 | D-Star | Patent term expired. A software backend is allowed. |
| AMBE+2 | DMR, NXDN, YSF | Reads on **US8359197 until 2028-05-20**. Hardware only. Do not add a software backend, and do not let a "software vocoder" config option reach these modes. |

So the software/hardware selector is **per-mode, never global**: offering it
for DMR, NXDN or YSF would ship an encumbered codec. A UI that greys those out
needs to say why.

Any software backend is an **opt-in cargo feature, off by default** (a shipped
build may turn it on by name, as `dstar-soft-ambe` is), with its
licence and the patent reasoning recorded in `LICENSE-EXCEPTIONS.md` — the
`codec2` entry is the pattern. The ThumbDV path stays the default even where a
software backend exists.

**Unchanged: never reference or vendor the software AMBE work from
`rcludwick/ambe`.** That prohibition is about that specific effort and is not
lifted by any of the above.

`vendor/ambe-thumbdv/Cargo.toml`, `vendor/codec2/Cargo.toml` and
`vendor/ham-digital-modes-ambe/Cargo.toml` hard-code
`license`/`version`/`edition`/`repository` (and `codec2` its `authors`) on
purpose. Do not convert them to `.workspace = true` — that would relicense
someone else's MIT/Apache or LGPL code as AGPL. See each one's `VENDORED.md`.
`vendor/codec2` is also in the workspace `exclude`, so fmt/clippy never touch
it: it stays byte-for-byte upstream. The same holds for `vendor/mp3lame-sys`
and `vendor/mp3lame-encoder` (LGPL-3.0), whose `VENDORED.md` files list every
patch; `stream-mp3` must never become a default feature
(`ci/guard-codec2-licensing.sh`).

## Distribution reality

Beta releases are published (latest: `gh release list -R rcludwick/astar`, or
the top of `CHANGELOG.md`): each is a Developer ID-signed, notarized, stapled
`astar.dmg` on the public repo's releases page. It is **arm64 only** (a single slice, macOS
13+) — there is no Intel and no universal build, so an Intel Mac still builds
from source. There is still **no** Homebrew tap or cask and no App Store
listing. Do not write install instructions that imply otherwise.

`just dmg` reproduces that image. It discovers a "Developer ID Application"
identity in the keychain and notarizes through the `astar-notary` keychain
profile (`ASTAR_SIGN_IDENTITY` / `ASTAR_NOTARY_PROFILE` override both;
`ASTAR_SKIP_NOTARIZE=1` skips the wait). With neither configured it falls back
to an ad-hoc image and says so, so a fresh clone still builds. No signing
identity, team id, or notary credential is stored in this repository.

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

## Versions — SemVer, since `0.1.10-beta`

**Decided 2026-08-29 (Rob), and done in `0.1.10-beta`: every version string is
real SemVer.** That includes the Rust crates. Releases up to and including
`0.1.9beta` are NOT retrofitted — they are shipped, tagged and published, and
rewriting a released version is worse than the inconsistency it fixes.

The glued form (`0.1.9beta`, pre-release stuck to the patch number) is not
SemVer and orders wrongly: `0.1.10beta` is newer than `0.1.9beta` and string
comparison says the opposite. `0.1.10-beta` is the hyphenated pre-release form
and sorts correctly against everything before it.

**The two spellings have converged.** Cargo could never parse the glued form,
so the Rust side spelled `0.1.9beta` as `0.1.9-beta` while the app spelled it
`0.1.9beta`; from `0.1.10-beta` all five homes carry one identical string.
`normalize()` in `ci/version_manifest.py` still bridges the old shape and must
keep doing so — published manifests contain `0.1.9beta`-shaped strings forever.

**One number, five homes, enforced.** `just ci` runs
`ci/version_manifest.py --check`, which fails when they disagree:

| Where | Spelling |
|---|---|
| `apps/macos/project.yml` | `MARKETING_VERSION` — Apple's free-form string |
| `zensical.toml` | the version chip inside `copyright` |
| `CHANGELOG.md` | the newest `## <version> — <date>` heading |
| root `Cargo.toml` | `workspace.package.version` — strict SemVer, always |
| `crates/*/Cargo.toml`, `apps/*/Cargo.toml` | every `astar-*` path dep's `version =` |

The path-dep requirements are the ones to watch: Cargo reads a stale
`version = "0.1.3-beta"` as a caret range the new crate still satisfies, so it
never complains — which is how the workspace drifted six releases behind before
`--check` started reading it.

`vendor/ambe-thumbdv` and `vendor/codec2` are outside all of this and keep
their own upstream versions (`0.1.0`, `0.3.1`).

Bumping all five is `just release <version>` (`--dry-run` first); publishing is
the separate `just publish <version>` — see `docs/RELEASING.md`.

## Config version — bump it for translation, not for change

`ConfigVersion.current` (AstarCore) is **1**. One number, two homes: stamped
into the preferences domain under `config.version` at launch, and written into
the envelope of every exported `.astarconfig`. `ConfigArchive.currentVersion`
is that same constant — a file and the Mac that wrote it must never claim
different versions.

* **Adding a field or a whole section does NOT bump it.** Both directions
  already cope: an older reader ignores what it does not recognise, a newer
  reader treats what is absent as unset. That tolerance is what keeps saved
  configs portable, and spending a version number on an addition throws it
  away for nothing.
* **Bump it only when existing data would be misread unless something rewrites
  it** — a key renamed, a unit changed, a meaning inverted, a type swapped.
* **A bump owes a translation** from the previous version, in both places. Old
  files and old preference domains do not stop existing because a new version
  shipped; `Tests/AstarCoreTests/Fixtures/astar-config-v1.astarconfig` is a
  real `0.1.4beta` export kept precisely so a version-1 file must keep
  importing forever.

An unstamped preferences domain reads as version 1, not 0 — everything written
before the version existed *is* version 1.

## Feature list — `docs/FEATURES.md`

`docs/FEATURES.md` is the complete list of what astar does **today**: every
user-visible feature of the macOS app, the Windows/Linux client and
`astar-server`, plus the engine capabilities they stand on. One row per
feature, with which platforms have it, so a gap between clients is visible at a
glance. It lives outside `docs/site/`, so it is not published.

**Every feature ships on every platform.** A feature is not done until every
client has it: today that's macOS and the Windows/Linux client, and later the
`astar-uiclone` clients for iOS and Android. Any platform marked "no" in
`docs/FEATURES.md` is a defect and gets a backlog item naming that row. A
feature that cannot reach a platform, because of hardware or an OS limit,
says why in its row. That is the only acceptable gap.

**Keep it true.** A change that adds, removes or changes what a user can do
updates `docs/FEATURES.md` in the same commit. Shipped-only: designs and backlog
items do not go in it until the code lands. When it and the code disagree, the
code wins; fix the list.

**Every feature is documented.** Adding a feature is not done until it has
user documentation on the public site (`docs/site/`, wired into the `nav` in
`zensical.toml`): what it does, how to turn it on, and what each setting means.
Its `docs/FEATURES.md` row links to that page. A plan for a feature carries the
docs as a task, not an afterthought.

## Work tracking

Use the Claude Code task tracker for the current session. The durable backlog is
`docs/BACKLOG.md` (engine) and `docs/app/BACKLOG.md` (clients) — anything that
outlives the session goes there.

**Always use a per-item git worktree for implementation work** — never work
directly on the default branch in the main checkout, which is reserved for
merging and doc commits. Check `git worktree list` first.

### Design docs — and keeping them honest

Designs and implementation plans live in `docs/superpowers/`, which is
**gitignored on purpose**: this is internal working material, it is not
published, and it is not in either remote. Browse it with `just design`
(localhost:8001, live reload); `zensical.design.toml` is a second Zensical
project kept deliberately separate from the public site, because
`ci/build-docs.sh` reads only `zensical.toml`.

| Directory | Means |
|---|---|
| `specs/` | a design agreed but **not yet shipped** |
| `plans/` | a task-by-task plan for work **still in progress** |
| `notes/` | a finding that belongs to no single spec |
| `implemented/` | shipped. Kept for the reasoning, not as a to-do list |

**A file's directory is a claim about the state of the code, and that claim
rots.** Work lands, the backlog moves on, and nobody moves the file — so
`specs/` silently fills with things that were finished months ago, and the
next session reads them as outstanding work.

So: **when you pick up design work, check the open designs against reality
first.** For each one that looks related to what you are about to do, confirm
against the code and `docs/BACKLOG.md` whether it actually shipped. If it did,
`git mv`-style move it to `implemented/` — plans especially, which are pure
procedure and worthless once the steps are ticked. A spec whose reasoning
exists nowhere else belongs in `docs/design/` first; move it only after that
reasoning has a permanent home.

Nothing here is in version control, so a deletion is unrecoverable except from
Time Machine. **Move to `implemented/`; do not delete.**

## Conventions

* No Claude/AI attribution anywhere — not in commits, merges, PRs, code
  comments, docs, or CI files. Never write "generated by".
* macOS shell quirks: no `timeout` command; zsh chokes on bare `==`/`===`
  arguments; the Bash tool's cwd resets between calls — use absolute paths.
* Apple HIG, SF Symbols, full light/dark, accessibility, instant-feeling
  interactions. Hold all UI work to that bar.
