---
icon: lucide/rocket
---

# First run: from download to your first contact

This is the short path from a freshly downloaded `astar.dmg` to hearing a
repeater and being heard back. It should take about ten minutes, most of it
spent on audio levels.

!!! info "What you need"

    * A Mac running **macOS 13 (Ventura) or later**, Apple silicon.
    * An **AllStarLink account** on [allstarlink.org](https://www.allstarlink.org/)
      with at least one node number assigned to your callsign.
    * A **headset** — USB or Bluetooth. Not required, but it makes everything
      below easier, for reasons the [Audio](#3-set-your-audio-levels) section
      explains.

## 1. Install

Download **astar.dmg** from the
[releases page](https://github.com/rcludwick/astar/releases/latest), open it,
and drag `astar.app` to Applications. The app is signed and notarized, so it
opens on a double-click — no right-click-Open dance, no Gatekeeper override.

On first launch astar asks for **microphone access**. Say yes; without it the
mic reads digital silence and you will transmit dead air. If you dismissed the
prompt by accident, grant it in *System Settings → Privacy & Security →
Microphone*.

Look for the **rainbow asterisk** in the menu bar. Left-click it to open the
client. That popover *is* the whole app.

## 2. Enter your AllStarLink account

Settings opens by itself the first time, because nothing works until this is
filled in. If it did not, click the gear in the lower left of the popover.

At the **top** of the Settings page is the *AllStarLink account* box. Three
fields:

| Field | What goes in it |
| --- | --- |
| **Callsign** | Yours, e.g. `KF8EBV`. |
| **Node number** | A node number registered to you. |
| **Account password** | Your **allstarlink.org website password**. |

!!! warning "This is the website password, not the node password"

    This trips up nearly everyone. astar logs in to the AllStarLink *portal* to
    mint a connection token — the same password you type into the website, not
    the per-node secret out of your node's config. If you have a hardware node
    (a ClearNode, a Pi running app_rpt), its node password is **not** what goes
    here.

**Which node number?** Any node registered to your callsign. If you already
have a hardware node on the air, request a **second** node number for astar and
use that. Two things logging in as the same node will fight over the
registration.

Press **Test**. You want *Token minted ✓ — credentials valid*. Until that
succeeds, the dial field stays locked and Settings will keep telling you that
AllStarLink is unavailable — that is the app refusing to let you dial into a
failure, not a bug.

If Test fails, it is almost always the password. Log in to allstarlink.org in a
browser to confirm it, and check the callsign and node number for typos.

## 3. Set your audio levels

This is the part worth slowing down for. Open **Quick settings** in the popover.

### Pick the devices

**Mic** and **Speaker** each default to *System Default*, which follows whatever
macOS is using. That is a fine starting point. Choosing your headset explicitly
is better once you have one you like, because then astar keeps using it even if
macOS switches the system default out from under you.

### Turn voice compression OFF while you set levels

Leave **Voice compression** off for now.

Compression is an automatic leveller: it pushes quiet audio up and holds loud
audio down until everything fits the transmit range. That is useful on the air
and confusing on the bench — you turn a knob, and compression quietly undoes it.
Set your raw levels first, then turn compression back on if you want it.

### Watch the meters, not the numbers

Talk normally into the mic and watch the **Audio Level** bar.

* **Mic Level** sets the input gain. Aim for the bar living in the upper-middle
  of its range on normal speech, with the loudest peaks short of the top. The
  default is 90%, deliberately backed off to leave headroom.
* **TX Gain** is the final stage after compression — think of it as *TX boost*.
  If compression alone will not get you loud enough, this is the control that
  will. It is also the one to pull down if a hot mic is making you too loud.
* **Vol** is astar's own speaker gain, from 100% up to 400%. It only ever
  boosts; it will not attenuate below unity. Use it when a station comes in weak.

!!! tip "astar never touches your Mac's volume"

    Every control above is astar's own software gain. astar does not read or
    change the macOS output volume, the menu-bar slider, or anything in *Audio
    MIDI Setup*. If your headset's master volume slider is greyed out in Audio
    MIDI Setup, that is the headset's own USB descriptor not exposing a master
    control — normal, harmless, and unrelated to astar. Set levels in astar.

### Full duplex — headphones only

**Full duplex** lets you hear the channel while you are transmitting, which is
how you find out that someone is doubling with you. It is genuinely useful.

On a laptop with the **built-in speaker and built-in mic**, turn it off. The
speaker feeds straight back into the mic and you get a feedback loop. Full
duplex is a headphones feature.

With headphones on and full duplex enabled, watch for the green **RX** meter
spiking while you are transmitting. That means someone else is talking over
you — back off and let them finish.

## 4. Choose how you key up

astar gives you four ways to transmit. Any of them can be your main one.

* **The TX button** in the popover — press and hold.
* **The Spacebar** — hold to talk. The astar window has to have keyboard focus;
  astar deliberately does not grab the Spacebar system-wide. If you are typing
  in a text field, Space types a space and does not key.
* **VOX** — voice-activated. Turn on *VOX* in Quick settings. Two knobs:
  **VOX** threshold (default −40 dB; drag toward −60 for more sensitivity) and
  **Hang Timeout** (default 500 ms — how long transmit stays up through the
  natural gaps between words). Use **Test** to sample your room's noise floor
  while you stay quiet; astar will tell you if your threshold sits too close to
  it and suggest a safer one. A threshold below the noise floor means the room
  keys your transmitter.
* **A hardware PTT switch** on a USB radio interface — see
  [Hardware](hardware.md).

And one way to *not* transmit: **TX disabled — listening only** hard-mutes
transmit no matter what else is set. It is the right setting for monitoring a
net, and it pairs well with VOX while you are still tuning the threshold.

## 5. Make a contact

Back in the popover, type a node number into the dial field and press
**Connect**. `69586` is AJ7HR's personal node and a reasonable first target.

Nodes you connect to are saved automatically as **recents**, and you can star
one to make it a **favorite** and give it your own name — "Local 2m", not
`51234`. Favorites are unlimited; the recents list shows the ten most recent.

When you get there, listen first, then key up and identify. Congratulations —
that is astar on the air.

## Things worth knowing early

**Where your settings live.** Preferences — devices, gains, VOX, favorites,
saved configs — are in `~/Library/Preferences/com.aj7hr.astar.plist`. Your
portal password is not in there; it is in the login Keychain under
`com.aj7hr.astar`, which is what you want.

There is no settings export/import yet, and no "put my config on a network
drive and sync it" option. It is on the list. For now, a second Mac means
entering the account once more and setting levels again — which you would
mostly want to do anyway, since a different mic on a different machine wants
different gain.

**Mic profiles.** *Analyze…* in Quick settings characterizes a microphone —
measuring its noise floor and notching out whine and hum — and saves the result
as a named profile you can pick per config. Worth doing once for each mic you
use regularly.

**Configs.** A config bundles a device pair, gains, and a mic profile under a
name. **System Default** is the built-in one you start on. Save your headset
setup as its own config and switching rigs becomes one menu pick.

**Bluetooth and AirPods.** They work — they appear in the Mic and Speaker menus
like any other device. Two caveats. First, the moment macOS starts using a
Bluetooth headset's *microphone*, it drops the link into a voice mode and
playback quality falls off noticeably; this is a macOS/Bluetooth behaviour, not
an astar one, and it affects every app. If you care about receive audio, a
wired or USB headset is better. Second, Bluetooth adds latency, so if you use
VOX give yourself a longer **Hang Timeout** than you would on USB.

AirPods stem presses are not available to astar as a PTT button — macOS does not
hand third-party apps those events. On AirPods, use **VOX**.

**Getting help.** Bugs and feature requests are welcome at
[github.com/rcludwick/astar/issues](https://github.com/rcludwick/astar/issues).

---

*73 — [AJ7HR](https://www.qrz.com/db/AJ7HR)*
