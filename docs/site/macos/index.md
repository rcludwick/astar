---
icon: lucide/monitor
---

# The macOS app

astar on macOS is a **menu-bar app** that also, by default, shows up in the Dock
and in Cmd-Tab. After launch, look for the **rainbow asterisk** in the menu bar —
the product mark, a six-spoke asterisk, not a five-pointed star. Left-click opens
the dial popover; everything the client does happens there or in Settings.

To run it without a Dock icon, right-click the asterisk and turn off **Show in
Dock**. It takes effect immediately and persists across launches.

<figure markdown="span">
  ![The astar window on macOS: connected to a node, with live TX and RX meters, the levels and spectrum view, and the DTMF dialpad open](../images/macos-app.png){ width="333" }
  <figcaption>Connected, receiving, with the spectrum and dialpad open.</figcaption>
</figure>

[Download astar.dmg](https://github.com/rcludwick/astar/releases/latest){ .md-button .md-button--primary }
[First run walkthrough](first-run.md){ .md-button }

!!! info "Requirements"

    macOS **13 (Ventura) or later**. The deployment target is set in
    `apps/macos/project.yml` and in AstarCore's `Package.swift`; the SwiftUI
    APIs the app uses that need it include `ViewThatFits`, in the status row.
    Building it also needs a full Xcode; see
    [Build it from source](../build/macos-app.md).

## What is in the popover

The popover is the whole client. In broad strokes:

* **Network picker** — AllStarLink, M17, EchoLink, and the digital-voice
  networks when their dongle is attached.
* **Dial field** — a node number, an address, a reflector or a callsign, plus
  [favorites and recents](favorites.md) from every network.
* **Connect / Disconnect**, with call state and round-trip time.
* **Push to talk** — press-and-hold in the UI, a hardware operator key on a USB
  radio interface, the button on an Inrico B02PTT Bluetooth speaker-mic, or
  VOX. They combine, and a transmit time-out (2 minutes unless you change it)
  ends any over that runs too long. Also a *TX disabled — listening only* mode
  when you want to monitor without any chance of keying.
* **Dialpad** for DTMF commands to the far end.
* **Quick settings** — the config's PTT hardware, input and output device,
  gains, VOX threshold and hang time, codec, talk timer.
* **Level meters and spectrum** for what you are sending and receiving.
* **Record** — saves the call to an audio file on your Mac. See
  [Recording](recording.md).
* **SSTV picture card** — when someone sends a slow-scan TV picture on an
  AllStarLink or EchoLink call, a card under the call card shows it coming
  in and then the finished picture, which is saved to a folder and kept in a
  gallery window. See [SSTV pictures](sstv.md).

Settings adds the rest: your AllStarLink and EchoLink accounts, audio configs
("setups"), microphone profiles and characterization, serial PTT wiring, the
Bluetooth PTT button, the transmit time-out, SSTV pictures, and device
selection.

## Finding your way around Settings

Open Settings with the sliders button in the popover footer, or with ⌘,
from the menu bar. It is one long list of sections: Audio configs, Mic Profiles,
Transmit, Operator, AllStarLink, EchoLink, DMR, NXDN, Vocoders, Favorites,
Reflector directory, Spectrum, Recording, SSTV and Backup.

That is a lot to scroll through, so each section folds away. Every section
header has a small triangle at its left edge, the same kind of disclosure
triangle as Quick settings and the Dialpad on the call screen. Click the
header, triangle or title, to open the section, and click it again to fold it
shut. A folded section shows only its header, so you can see every section's
name at once and open just the one you came for.

A few things make this quicker:

* **Expand All / Collapse All.** The button at the right of the Settings
  title opens every section at once. Once everything is open it turns into
  Collapse All, which folds them all again.
* **Option-click a triangle** to open or fold every section in one go, the
  way Finder's list view works. The section you clicked decides the
  direction: Option-clicking a folded one opens them all.
* **The "+" stays in the header.** Vocoders, Audio configs and Mic Profiles
  keep their "+" visible even while folded. Pressing it opens the section
  for you, so a new saved config appears in view, ready to be named.
* **Links into Settings open the right section.** Buttons elsewhere in
  astar that take you to one part of Settings, such as *Open Vocoder
  Settings…* after a refused D-Star connect or *Open EchoLink Settings…*
  when EchoLink has no login yet, open that section before scrolling to it.

astar remembers which sections you left open and which you folded, and
Settings comes back the same way the next time, even after a restart. The
first time you open it, only Operator and AllStarLink are open, so it fits on
about one screen. This is a choice about your own Mac's window, so it is not
part of an exported config: importing someone's `.astarconfig` never
rearranges your Settings.

With VoiceOver, each section header reads as a button with its name and
whether it is expanded or collapsed. With keyboard navigation turned on in
System Settings → Keyboard, Tab reaches each header and Space opens or folds
it.

The running version is shown in the popover footer, so you can always tell what
you are looking at.

## Networks

| Network | Notes |
|---|---|
| **AllStarLink (IAX2)** | The primary target. Dials nodes; speaks the `app_rpt` link-layer dialect on top of IAX2. |
| **M17** | Native support, including reflectors. Transmits your callsign, so set it before you key. The app links Codec 2 in, so there is nothing to install; a system `libcodec2` is used in preference when one is present — see [Codec 2](../build/prerequisites.md#codec-2-only-for-m17). |
| **EchoLink** | Log in with your validated callsign, browse and search the station list, call a station, talk and chat. Voice is GSM 06.10, built in. See [EchoLink](echolink.md). |
| **D-Star** *(dongle or software vocoder)* | XLX/XRF reflectors over DExtra. Voice is AMBE, which runs on a **hardware vocoder dongle**, so the network appears in the picker only while one you have added in [Settings → Vocoders](vocoders.md) is attached — or always, when you choose D-Star's built-in software vocoder there. See [Digital voice](hardware.md#digital-voice). |
| **System Fusion** *(needs a dongle)* | YSF reflectors, receive and transmit. Voice is AMBE+2, which only the dongle can run. |
| **NXDN** *(needs a dongle; receive only)* | A talkgroup on an NXDNReflector. Set your NXDN ID in Settings → NXDN. |
| **DMR** *(needs a dongle; receive only)* | A talkgroup and timeslot on one network's master. Set your DMR radio ID in Settings → DMR. |

## Audio and PTT hardware

astar targets the generic class of USB radio interfaces — serial PTT plus USB
audio. The AllScan UCI150 is the reference device, not a special case. The
[Hardware](hardware.md) page covers the macOS driver situation and how the PTT
lines are wired.

The push-to-talk button on an **Inrico B02PTT** Bluetooth speaker-mic can key
astar too; a config uses either it or a USB radio interface. See
[Bluetooth PTT button](bluetooth-ptt.md).

## What these docs cover

The pages in this section, in the order you are likely to need them:

* [First run](first-run.md) — from the download to a first contact.
* [Audio configs](configs.md), [Favorites and recents](favorites.md) and
  [the reflector directory](reflectors.md) — saving setups and finding
  something to dial.
* [Hardware](hardware.md), [Bluetooth PTT button](bluetooth-ptt.md) and
  [Vocoders](vocoders.md) — radio interfaces, push-to-talk and the D-Star
  vocoder dongle.
* [EchoLink](echolink.md) — logging in and calling a station.
* [Recording](recording.md) and [SSTV pictures](sstv.md) — keeping what you
  hear.
* [Packet log](packet-log.md) — capturing a call's packets for a bug report.

A full walkthrough of day-to-day operating has not been written yet; see
[Using astar](usage.md).
