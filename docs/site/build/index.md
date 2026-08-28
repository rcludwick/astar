---
icon: lucide/hammer
---

# Building astar

There is one published binary: a Developer ID-signed, notarized `astar.dmg` for
the macOS app, on the [releases page](https://github.com/rcludwick/astar/releases/latest).
It is a single `arm64` slice for macOS 13 or later. Everything else — the
engine, `astar-server`, the Iced client, and the macOS app on an Intel Mac — is
built from source.

There is no Homebrew tap, no cask, and no App Store listing.

`just` is the command palette; run it with no arguments to list every recipe.

## Pick what you are building

One repository, three deliverables, and they have very different requirements.
Build only the one you want.

<div class="grid cards" markdown>

-   __[The engine](engine.md)__

    **astar-lib** — the Rust crates under `crates/`. Protocol, codecs, audio,
    PTT, the C ABI. Needs nothing but a Rust toolchain, builds on macOS, Linux
    and Windows.

    `cargo build --workspace`

-   __[Building the macOS app](macos-app.md)__

    The SwiftUI menu-bar client. The heaviest set of prerequisites — a full
    Xcode, XcodeGen, and one mandatory extra step (`just xcframework`) that a
    fresh checkout must not skip.

    `just xcframework && just run`

-   __[The Windows / Linux client](clients.md)__

    The Iced client, `apps/gui`. Builds and runs natively on both, and is
    **not finished** — treat it as a work in progress rather than something to
    install.

    `cargo run -p astar-gui`

-   __[astar-server](server.md)__

    The headless node daemon. A plain cargo binary plus a config file; the
    easiest thing here to build and the only one meant to run unattended.

    `just node`

</div>

## The 60-second version

If you only want to know whether the tree is healthy on your machine:

```bash
git clone https://github.com/rcludwick/astar
cd astar
cargo build --workspace
just ci                 # fmt + clippy + test + C-header drift
```

That needs a Rust toolchain and nothing else (plus two dev packages on Linux —
see [Prerequisites](prerequisites.md)). No Xcode, no Swift, no Python.

## Toolchain at a glance

| You want | Rust | Xcode | Other |
|---|:--:|:--:|---|
| The engine crates | ✅ | — | Linux: `libasound2-dev`, `libudev-dev` |
| `astar-server` | ✅ | — | same |
| The Iced client | ✅ | — | Linux: X11/Wayland runtime libraries |
| The macOS app | ✅ | ✅ full | XcodeGen, `just` |
| The Swift bindings | ✅ | ✅ full | — |
| The Python binding | ✅ | — | `python3` |
| These docs | — | — | `uv` (or any `pip`) |

**MSRV is 1.89**, edition 2024. That floor comes from the Iced client's
dependency graph; the engine crates on their own build on 1.86. The repository
pins a toolchain in `rust-toolchain.toml`, so `rustup` will fetch the right one
without being asked.

## Where things end up

| Artifact | Path |
|---|---|
| Rust binaries and libraries | `target/debug/`, `target/release/` |
| The macOS app | `apps/macos/build/DD/Build/Products/Debug/astar.app` |
| A local disk image | `apps/macos/build/astar.dmg` |
| The Swift xcframeworks | `bindings/swift/astar.xcframework`, `bindings/swift-serial/astarserial.xcframework` |
| The documentation site | `docs/.site/` |

All of those are gitignored and fully regenerable. Deleting `target/` or an
xcframework costs you a rebuild and nothing else.

## Next steps

* [Prerequisites](prerequisites.md) — the toolchains, per operating system.
* [Verifying a build](verifying.md) — the gates, and what to do when one is red.
