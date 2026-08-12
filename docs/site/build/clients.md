---
icon: lucide/monitor
---

# The Windows / Linux client

`apps/gui` is the [Iced](https://iced.rs) client, crate name **astar-gui**. It
is a view over the same engine the macOS app uses, with the same intended
feature set and a native rather than web UI.

!!! warning "It is not finished"

    This client is a work in progress. It builds, it runs, and its rendering is
    verified from screenshots — but it has not had a sit-down-and-use-it pass
    on real hardware, and Windows runtime behaviour is unproven. Build it if
    you want to work on it, not if you want a radio client today.

## Building and running

Natively, on the machine you are sitting at:

```bash
cargo run -p astar-gui
```

On Linux that needs the two dev packages from
[Prerequisites](prerequisites.md#linux), plus the usual X11/Wayland runtime
libraries your desktop already has. On Windows it needs nothing beyond the MSVC
toolchain.

Nothing here needs `just xcframework` — that is a macOS-app concern. The Iced
client links the engine crates directly as ordinary cargo dependencies.

## Platform state, honestly

| Platform | State |
|---|---|
| **Linux** | Builds, tests and runs headless in container runs; rendering verified from screenshots. No real-hardware pass. |
| **Windows** | Cross-compiles and links. Runtime verification on an actual Windows machine is an open backlog item. Treat it as unproven. |

## Cross-platform checks from a Mac

Two recipes exist so that Linux and Windows breakage is caught without a Linux
or Windows machine in the room. Both are development checks, not a way to
produce something to hand to a user.

### Linux, in a container

```bash
just gui-linux           # → apps/gui/.shots/linux-idle.png
```

Builds, tests and headless-runs `astar-gui` inside podman: the same package set
a Linux CI job would use, a `--shot` run under Xvfb with the software renderer,
and a proof PNG at the end that you can look at. Named volumes keep the cargo
registry and target directory across runs.

Requires a running podman machine:

```bash
podman machine start
```

The repository is mounted at its real path inside the container so that path
dependencies resolve exactly as they do on the host.

### Windows, cross-compiled

```bash
just gui-windows         # links a real astar-gui.exe
```

Uses `cargo-xwin`, which fetches the MSVC CRT and Windows SDK and drives
clang/lld-link. That catches unix-only code and link errors across the whole
dependency tree — `cpal`'s WASAPI backend, `serialport`, `ring`'s C sources —
without a Windows box.

Requires:

```bash
cargo install cargo-xwin
brew install llvm          # llvm-lib and llvm-rc, the MSVC-flavoured tools
```

!!! note "What the Windows check cannot prove"

    It proves the code compiles and links for Windows. It does **not** prove
    the window opens, the audio devices enumerate, or the serial port works.
    Only running it on Windows shows that, and that is exactly the gap the
    backlog item is about.

## Screenshots

The client's demo screenshots are generated, not captured by hand:

```bash
just gui-shots           # → apps/gui/.shots/
```

They exist so a UI change can be reviewed by looking at the rendered result.

## Next steps

* [The engine](engine.md) — what this client is a view over.
* [Verifying a build](verifying.md) — the gates.
