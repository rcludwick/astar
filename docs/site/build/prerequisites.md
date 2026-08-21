---
icon: lucide/package-check
---

# Prerequisites

Install only what the thing you are building needs — the table in
[Building astar](index.md#toolchain-at-a-glance) says which column is yours.
Everything on this page is a one-time setup.

## Rust — everyone needs this

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**MSRV 1.89**, edition 2024. `rust-toolchain.toml` selects the `stable` channel
and the `rustfmt` + `clippy` components, so `rustup` installs those for you on
the first `cargo` command in the tree. It does **not** pin a version — a stable
toolchain older than 1.89 will fail the build rather than be upgraded for you.
If you already have `rustup`, update it first:

```bash
rustup update stable
rustc --version          # want 1.89 or newer
```

!!! note "Why 1.89 and not 1.86"

    The engine crates alone build on 1.86. The higher floor comes from the Iced
    client's dependency graph (`font-types` 1.89, `iced`/`wgpu`/`image` 1.88).
    It is declared workspace-wide at the higher of the two so that
    `cargo build --workspace` on a minimum toolchain cannot fail halfway.

## macOS

=== "Engine and astar-server only"

    Nothing beyond Rust. Xcode is not involved.

    ```bash
    cargo build --workspace
    ```

=== "The macOS app"

    | Requirement | Why | Install |
    |---|---|---|
    | **macOS 13+** | `MenuBarExtra` is the floor. | — |
    | **Full Xcode** | `xcodebuild` plus the SwiftUI/AppKit SDKs. The Command Line Tools alone are **not** enough. | App Store, then `sudo xcode-select -s /Applications/Xcode.app` |
    | **XcodeGen** | Generates the (gitignored) `astar.xcodeproj` from `apps/macos/project.yml`. | `brew install xcodegen` |
    | **just** | The command palette. | `brew install just` |

    Verify Xcode is really selected — this is the single most common cause of a
    confusing first build:

    ```bash
    xcode-select -p          # want /Applications/Xcode.app/Contents/Developer
    xcodebuild -version      # want a version, not an error
    ```

## Linux

Two development packages, for the audio and serial crates respectively:

=== "Debian / Ubuntu"

    ```bash
    sudo apt install build-essential pkg-config libasound2-dev libudev-dev
    ```

=== "Fedora"

    ```bash
    sudo dnf install gcc pkgconf-pkg-config alsa-lib-devel systemd-devel
    ```

=== "Arch"

    ```bash
    sudo pacman -S base-devel alsa-lib systemd-libs
    ```

`libasound2-dev` is ALSA, used by `cpal` for audio. `libudev-dev` is used by
`serialport` for device enumeration. Without them the build fails at link time
with a missing `-lasound` or `-ludev`.

The Iced client additionally needs the usual X11/Wayland runtime libraries.
`apps/gui/check-linux.sh` runs the client headless in a container and its
package list is the authoritative one — see [The Windows / Linux
client](clients.md).

## Windows

A stable Rust toolchain with the **MSVC** target and the Visual Studio Build
Tools it links against. `rustup` prompts for these on first run. No extra
system packages are needed; `cpal` uses WASAPI and `serialport` uses the Win32
API, both part of the OS.

Cross-compiling a Windows binary from a Mac is a different path and is covered
in [The Windows / Linux client](clients.md).

## Optional extras

These unlock specific features. Skip any you do not want — the build succeeds
without all of them.

### `cbindgen` — only if you touch the C ABI

```bash
cargo install cbindgen --version 0.29.4 --locked
```

`just cbindgen` checks the committed `astar.h` / `astarserial.h` against what
the current Rust source would generate, and fails on drift. It is part of
`just ci`, so you need it to run the full gate. The pinned version matters:
different cbindgen releases format headers differently, and a mismatch reads as
spurious drift.

### Codec 2 — only for M17

M17 needs Codec 2. Where it comes from depends on which of the two things you
are building, and only one of them needs anything installed.

**The macOS app carries its own** (astar-8c4d). `just xcframework` builds
`astar-sys` with `--features codec2-static` unless you say otherwise, so both
the shipped DMG and an app you build yourself have M17 with nothing else on the
machine. `ASTAR_CODEC2=runtime just xcframework` opts out and leaves no LGPL
code linked in.

**Everything else resolves a system `libcodec2` at runtime** — a plain
`cargo build` of the engine, `astar-server` or the Iced client `dlopen`s it,
never links it. It tries `IAX_CODEC2_PATH` first, then any configured search
directories, then `/opt/homebrew/lib`, `/usr/local/lib` and `/usr/lib`. Each
candidate is sanity-checked before use, so a wrong or broken library is
rejected rather than half-loaded.

The runtime path wins wherever both exist: even in the linked app, a healthy
system library is preferred and the linked copy is the fallback. Install one
if you want to control which Codec 2 astar runs.

=== "macOS"

    ```bash
    brew install codec2
    ```

=== "Debian / Ubuntu"

    ```bash
    sudo apt install libcodec2-dev
    ```

If your copy lives somewhere unusual, name the library itself:

```bash
export IAX_CODEC2_PATH=/path/to/libcodec2.dylib
```

!!! warning "No Codec 2 at all looks exactly like no M17 support"

    When a build can find neither a system `libcodec2` nor a linked copy, it
    reports M17 as unavailable and the clients simply do not offer M17 in the
    network picker. There is no error dialog. AllStarLink needs none of this —
    only M17 does.

    Keeping Codec 2 out of every *default* build is deliberate: it is LGPL-2.1
    and MIT, so `codec2-static` and `codec2-runtime` are both opt-in and a
    plain `cargo build` stays free of LGPL code. `ci/guard-codec2-licensing.sh`
    fails the build if that ever stops being true. Linking it into the app is
    the one deliberate exception, covered by the notices and written offer in
    `LICENSE-EXCEPTIONS.md`.

### `uv` — only for the documentation site

```bash
brew install uv          # or: pipx install uv
```

`just docs` and `just docs-build` run Zensical through `uvx`, so there is no
virtualenv to manage. `ci/build-docs.sh` falls back to a throwaway virtualenv
and `pip install zensical` when `uv` is absent. Either way it needs network
access once.

### `podman` / `cargo-xwin` — only for the cross-platform checks

`just gui-linux` needs a running podman machine. `just gui-windows` needs
`cargo install cargo-xwin` and `brew install llvm`. Both are covered in
[The Windows / Linux client](clients.md).

## Next steps

* [The engine](engine.md) — the Rust workspace.
* [The macOS app](macos-app.md) — the SwiftUI client.
