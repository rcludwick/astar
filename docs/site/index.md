---
icon: lucide/radio-tower
---

# astar

A native ham-radio digital-voice client and node — **AllStarLink (IAX2)** and
**M17** — built on one Rust engine with native front-ends.

astar dials nodes and reflectors as a client: audio, push-to-talk, DTMF, live
meters, and support for the generic class of USB radio interfaces (serial PTT
plus USB audio). The same engine also runs headless as an always-on node daemon.

The two networks are **not equally far along**. AllStarLink is the primary
target. M17 is in the apps but capability-gated on a system `libcodec2`. See
[The macOS app](macos/index.md).

Other networks are in the tree at various stages and are **not** claimed as
working. Treat any crate you find for one as work in progress, not a feature.

[Build it from source](build/index.md){ .md-button .md-button--primary }

## Three surfaces, one engine

<div class="grid cards" markdown>

-   __[The macOS app](macos/index.md)__

    A SwiftUI **menu-bar** client: a **rainbow asterisk** in the menu bar that
    opens a dial popover, plus a Dock icon you can turn off. macOS 13 or later.
    [Build it from source](build/macos-app.md).

-   __[astar-server](server/index.md)__

    The headless **node daemon** — an inbound IAX2 listener, registration with
    the AllStarLink registrar, a conference bridge, and a loopback HTTP + SSE
    control channel.

-   __astar-lib__

    The engine every front-end sits on: IAX2 framing and session state, codecs,
    audio I/O, PTT backends, and the multi-network station facade. Pure Rust,
    no UI, exposed to Swift and Python through a C ABI.

-   __[Protocol notes](reference/index.md)__

    What was learned reverse-engineering `app_rpt`'s IAX2 link layer and the
    Web Transceiver call flow, cited to source, wire captures, or RFC 5456.

</div>

## Architecture: fat core, thin views

All protocol, audio and PTT logic lives in the Rust crates. The front-ends are
views over that engine, so a feature lands once and every client gets it, with
per-platform native UI rather than a shared web shell.

## Platform status

| Platform | Status |
|---|---|
| **macOS 13+** | The supported client. Menu-bar app, built from source — [how to build it](build/macos-app.md). |
| **Windows / Linux** | **In progress.** The engine is cross-platform and an [Iced](https://iced.rs) client (`apps/gui`) builds and runs on both, but it is not finished and should not be treated as ready. [Building it](build/clients.md) is documented; using it is not. |
| **Server** | `astar-server` runs headless anywhere the engine builds. [Building it](build/server.md). |
| **iOS** | Targeted. The Xcode project already builds a multiplatform target; there is no shipping iOS client. |

**Everything is built from source** — [Building astar](build/index.md) covers
all of it. The day-to-day *usage* pages are macOS-only for now; when the Iced
client is ready, usage pages for Windows and Linux will join them.

## Licence

**AGPL-3.0-only** for everything in the repository, except the vendored
`ambe-thumbdv` ThumbDV driver, which keeps its own MIT/Apache-2.0 terms. See
[Licence](about/license.md).
