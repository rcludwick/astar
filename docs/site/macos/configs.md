---
icon: lucide/archive
---

# Saved configs, backup and transfer

astar keeps its whole setup on the Mac it runs on — no account, no sync, no
server. This page covers what that setup consists of, how to move it to another
Mac, and how to share part of it with someone else.

## Saved configs

A **saved config** is one rig you switch to in a single click: a hardware
profile, the input and output devices, the mic and speaker gains, compression,
noise reduction, VOX, full duplex, and — for a serial interface like the
UCI150 — the whole serial/PTT wiring.

The point is that you flip between rigs instead of re-picking devices by hand.
A desk setup with a USB radio interface and a portable headset setup are two
configs, and switching restores both halves at once.

Settings → **Saved configs**. **System Default** always sits at the top: it is
a real config, not a hidden fallback, so there is always a known-good plain-audio
state one click away. ★ marks the config applied at launch.

!!! tip "Mic profiles follow the device, not the config"

    A mic profile is characterized against one input device and is re-applied
    whenever that device is selected — so switching configs also restores the
    right mic profile, without the config having to carry it.

## Where it is stored

Everything lives in one macOS preferences domain:

```bash
~/Library/Preferences/com.aj7hr.astar.plist
```

**Your AllStarLink account is not in there.** The portal username, node and
password are kept in the **login Keychain**, which is why they survive a reset
of the preferences and why they are never part of an export.

To take a raw backup of everything, or put one back:

```bash
defaults export com.aj7hr.astar ~/astar-backup.plist    # save
defaults import com.aj7hr.astar ~/astar-backup.plist    # restore
```

Quit astar first for the restore — a running app rewrites its preferences on
exit and would overwrite what you just put back.

That raw plist is exact and total, which makes it the right tool for *this Mac,
later* and the wrong one for anything you intend to share. For that, use export.

## Export

Settings → **Backup** → **Export…**. Choose what to include:

| Section | What travels |
|---|---|
| **Saved configs** | Your rigs and the mic profiles they reference |
| **Node directory** | Favorites and recents |
| **Audio and serial settings** | Devices, gains, VOX, compression, serial PTT |
| **Callsign** | Your M17 callsign |
| **Window and panel state** | Which panels are open, Dock icon, selected network |

The result is a plain-text JSON file with an `.astarconfig` extension. It is
meant to be opened and read — you can check what is in one before trusting it.

!!! warning "Callsign is what makes a file personal"

    It is a separate tick box for exactly that reason. Leave it off and an
    export is a rig you can hand to anyone; leave it on and the file identifies
    you. Your node directory is worth a thought too — it records which nodes you
    have been dialing.

Credentials are **never** exported, whatever you tick. There is no option to
include them.

## Import

Settings → **Backup** → **Import…**, pick a file, then choose which of its
sections to bring in. Only the sections the file actually contains are offered.

**Importing adds and updates. It never deletes.** Your existing configs and
nodes stay; anything matching is updated in place. Importing the same file twice
is a no-op rather than a way to get everything twice — the second run reports
*"Nothing to change — already up to date."*

That is what makes a partial import useful: you can take somebody's node list
and leave their microphone, gains and serial wiring alone.

| Section | How a match is decided |
|---|---|
| Saved configs, mic profiles | By id — a config edited elsewhere updates in place and keeps its position |
| Node directory | By **node number** — the same node is never stored twice, your label and ★ are kept |

Afterwards astar reports what actually changed, e.g. *"2 configs added, 15 nodes
added, 31 settings applied, callsign set."* Nothing is reported for a section
you left unticked.

!!! note "Devices are matched by name"

    A config records its input and output device by the name macOS reports.
    Import onto a Mac without that device and the config keeps the name and
    falls back to the system default until the device appears. See
    [Hardware](hardware.md) for what happens when two devices report the *same*
    name.

## Config version

Every exported file, and the preferences domain itself, records a **config
version** — currently `1`.

It exists so a future astar can translate an older file rather than reject it.
The rule it follows is worth knowing if you keep old exports around:

* Adding new fields or whole new sections **does not** change the version. An
  older astar ignores what it does not recognise, and a newer one treats what is
  missing as unset, so files stay portable across releases.
* The version only moves when existing data would be *misread* without being
  rewritten — a setting renamed, a unit changed, a meaning reversed.
* Anything that moves it ships the translation with it. **A version 1 file will
  keep importing.**

A file from a *newer* astar than the one you are running is refused with a clear
message rather than half-read; update astar and try again.

## Starting over

To reset astar to factory state, quit it and remove the domain:

```bash
defaults delete com.aj7hr.astar
```

Your AllStarLink account survives, because it is in the Keychain. Take a
`defaults export` first if there is any chance you want it back.
