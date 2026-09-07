---
icon: lucide/radio-tower
---

<div class="astar-hero" markdown>
<div class="astar-hero__copy" markdown>

# astar

astar is a digital-voice client for **AllStarLink** and **M17**. It connects over
the network as a softclient — no radio, no hotspot, no repeater in the path —
and handles audio, push-to-talk, DTMF and level metering itself.

**D-Star** and **System Fusion** are in the macOS app too — DExtra to XLX/XRF
reflectors, and YSF reflectors — with the AMBE+2 vocoder running on a USB
dongle: a **DVMEGA DVstick 30** or a **ThumbDV / DV3000**. Plug one in and
both networks appear in the picker; there is nothing to configure.

Protocol, codec and audio handling live in one Rust engine; each platform gets a
native front-end over it rather than a shared web shell.

!!! info "Beta"

    astar is beta and moves quickly. AllStarLink is the primary target and takes
    the most testing. M17 needs nothing installed — Codec 2 is linked into the
    app as of `0.1.4beta`. D-Star works, but only with a hardware vocoder
    dongle attached — see [Digital voice](macos/hardware.md#digital-voice).
    Other protocols are in the tree at various stages and are not claimed as
    working.

    The download is Developer ID-signed and notarized, so it opens without a
    Gatekeeper override. It is Apple Silicon only — a single `arm64` slice — and
    needs macOS 13 (Ventura) or later. On an Intel Mac,
    [build from source](build/index.md). There is no Homebrew tap, no cask, and
    no App Store listing.

[Download for macOS](https://github.com/rcludwick/astar/releases/latest){ .md-button .md-button--primary }
[Build it from source](build/index.md){ .md-button }

</div>
<div class="astar-hero__shot" markdown>
![The astar popover on macOS: connected to a node, with live TX and RX meters, the levels and spectrum view, and the DTMF dialpad open](images/macos-app.png)
</div>
</div>

## Networks

| Network | Transport | Voice codec | Dials | Identifies as |
|---|---|---|---|---|
| AllStarLink | IAX2 (RFC 5456), UDP 4569 | µ-law / A-law / signed-linear (8 or 16 kHz), negotiated | Node numbers | Node number, via the allstarlink.org portal |
| M17 | UDP 17000 | Codec 2 3200 | Reflector + module | Callsign |
| D-Star *(needs a dongle)* | DExtra, UDP 30001 | AMBE+2, on the dongle | XLX/XRF reflector + module | Callsign |
| System Fusion *(needs a dongle)* | YSF reflector protocol, UDP 42000 | AMBE+2 DN, on the dongle | YSF reflector | Callsign |
| NXDN *(needs a dongle)* | NXDNReflector, UDP 41400 | AMBE+2, on the dongle | Talkgroup on a reflector | Callsign + NXDN ID |

M17 transmits your callsign on the air; AllStarLink identifies by node number
instead, and authenticates against the portal rather than a per-node secret.

D-Star voice is AMBE+2, which has no software implementation in astar: it runs
on a hardware vocoder dongle, and the network appears in the client's picker
only while one is attached. [Digital voice](macos/hardware.md#digital-voice)
covers why, and what to buy.

## Where to start

<div class="grid cards" markdown>

-   __[The macOS app](macos/index.md)__

    A SwiftUI menu-bar client: an NSStatusItem that opens a dial popover, plus
    an optional Dock icon. macOS 13 or later.
    [Build it from source](build/macos-app.md).

-   __[Building astar](build/index.md)__

    Toolchains, the Rust workspace, the Swift xcframeworks, and the Iced client
    for Windows and Linux.

-   __astar-lib__

    The engine: IAX2 framing and session state, codecs, audio I/O, PTT
    backends, and the multi-network station facade. Pure Rust, no UI, exposed
    to Swift and Python through a C ABI.

-   __[Protocol notes](reference/index.md)__

    What was learned reverse-engineering `app_rpt`'s IAX2 link layer and the
    Web Transceiver call flow, cited to source, wire captures, or RFC 5456.

</div>

## Hardware

For push-to-talk, astar drives the generic class of USB radio interfaces —
serial PTT plus USB audio. The AllScan UCI150 (WCH CH343) is the reference
device, not a special case. Raw USB is the default transport and needs no
driver; the tty path is opt-in and on macOS needs WCH's driver. See
[Hardware](macos/hardware.md).

D-Star and System Fusion are hardware-only. AMBE+2 has no freely licensable
software implementation, so the codec runs on a dongle — a DVMEGA DVstick 30
or a ThumbDV / DV3000, either works — Codec 2 you can install, AMBE you have
to own.

## Architecture

All protocol, audio and PTT logic lives in the Rust crates. The front-ends are
views over that engine, so a feature lands once and every client gets it, with
per-platform native UI.

## Platform support

| Platform | State |
|---|---|
| macOS 13+ | Supported. Menu-bar app; signed, notarized `arm64` [download](https://github.com/rcludwick/astar/releases/latest), or [build it](build/macos-app.md). Intel Macs build from source. |
| Windows / Linux | In progress. The engine is cross-platform and an [Iced](https://iced.rs) client (`apps/gui`) builds and runs on both, but it is unfinished. [Building it](build/clients.md) is documented; using it is not. |
| iOS | Targeted. The Xcode project builds a multiplatform target; there is no shipping iOS client. |

The macOS app is the only published binary; everything else is built from
source. Day-to-day usage documentation is macOS-only for now.

## Community

[**astar on Discord**](https://discord.gg/zDz5R8rVAM) — where questions get
asked, bugs get reported before they are tickets, and on-air testing gets coordinated. astar is a
beta client for live amateur radio networks; hearing what broke on someone
else's setup is how it stops being broken on yours.

## Licence

**AGPL-3.0-only** for everything in the repository, except the vendored
`ambe-thumbdv` ThumbDV driver, which keeps its own MIT/Apache-2.0 terms. See
[Licence](about/license.md).
