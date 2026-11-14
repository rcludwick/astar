---
icon: lucide/rocket
---

# First run: from download to your first contact

This is the short path from a freshly downloaded `astar.dmg` to hearing a
repeater and being heard back. It should take about ten minutes, most of it
spent on audio levels.

!!! danger "You must be a licensed amateur"

    Everything below ends with a real transmitter keying on a real band.
    **An appropriate amateur radio licence for your jurisdiction is
    required**, and as control operator you are responsible for every
    transmission your station makes — including the ones a computer
    initiated on your behalf.

    astar does not know your licence class, your band and mode privileges,
    or your local identification rules. Those stay yours.
    [On-air safety](../about/safety.md) is the full version and is not
    optional reading.

!!! info "What you need"

    * **An amateur radio licence**, and a callsign you are entitled to use.
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
| **Callsign** | Yours, e.g. `N0CALL`. |
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

Levels are where most of the setup time goes. Open **Quick settings** in the
popover.

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

### Jitter buffer — leave it on

**Jitter buffer** is on by default, and for almost everyone that is the end of
it. Audio arrives from the network in 20 ms pieces that do not arrive evenly
spaced; the buffer holds a small cushion of them back so the speaker always has
the next piece ready. Without it, every gap in the network's timing is an
audible hole — the choppiness people describe as "it was stuttering until I
disconnected and reconnected".

The cushion costs latency, which is what the two sliders under the switch
control. They are the depth the buffer may adapt between, in milliseconds:

* **Min** (default 40) is the slack it keeps over the jitter it has measured —
  the floor it will not go below.
* **Max** (default 200) is the ceiling. It will not hold back more than this no
  matter how bad the path gets.

Those defaults are Asterisk's, deliberately: the node at the other end of an
AllStarLink call is Asterisk, so matching its buffer makes astar sound the way
operators already expect. The same buffer, with the same Min and Max, plays an
[EchoLink](echolink.md#voice-quality) call's audio. **Raise Max** — 300, 400 — when audio is still choppy
on a path you cannot fix: a satellite link, hotel wifi, a distant node. **Lower
Min** toward 0 when the path is clean and you would rather have the
conversation feel immediate; you will hear the difference in how quickly
somebody's first syllable arrives. Turning the buffer off entirely is a
diagnostic, not a setting to live on.

While you are connected, the line under the meters tells you what it is
actually doing — `jitter 12 ms · buffer 60 ms · lost 0 · late 0`. A growing
`underruns` count appears on the end when the speaker ran dry mid-transmission,
which is the number to watch if you are raising **Max** to chase choppy audio.

### Full duplex — headphones only

**Full duplex** lets you hear the channel while you are transmitting, which is
how you find out that someone is doubling with you.

On a laptop with the **built-in speaker and built-in mic**, turn it off. The
speaker feeds straight back into the mic and you get a feedback loop. Full
duplex is a headphones feature.

With headphones on and full duplex enabled, watch for the green **RX** meter
spiking while you are transmitting. That means someone else is talking over
you — back off and let them finish.

## 4. Choose how you key up

astar gives you five ways to transmit. Any of them can be your main one, and
they combine: astar stays on air while any of them holds, so letting go of one
never cuts off another.

* **The TX button** in the popover — press and hold.
* **The Spacebar** — hold to talk. The astar window has to have keyboard focus;
  astar deliberately does not grab the Spacebar system-wide. If you are typing
  in a text field, Space types a space and does not key.
* **VOX** — voice-activated. Turn on *VOX* in Quick settings. Two knobs:
  **VOX** threshold (default −50 dB; drag left, down to −58, for more
  sensitivity) and
  **Hang Timeout** (default 500 ms — how long transmit stays up through the
  natural gaps between words). Press **Test** and the **Audio Level** bar
  shows the same loudness VOX compares with the threshold: it turns green
  when it reaches the orange mark, which is exactly when VOX would key. Stay
  quiet for a moment first and set the threshold well above where the bar
  sits; a threshold at your room's noise floor means the room keys your
  transmitter. [How VOX hears you](#how-vox-hears-you) explains what the
  number measures.
* **A hardware PTT switch** on a USB radio interface — see
  [Hardware](hardware.md).
* **A Bluetooth PTT button** — the button on an Inrico B02PTT speaker-mic.
  Pick it in Quick settings → Station → **PTT**, then press it once to confirm
  it is yours. See [Bluetooth PTT button](bluetooth-ptt.md).

A config has one PTT device: a USB radio interface or the Bluetooth button,
never both. The TX button, the Spacebar and VOX work with either.

**The TX button and the Spacebar are also the stop.** Press either while
something else holds the key — the Bluetooth button, the radio's PTT switch,
VOX — and astar stops transmitting. Whatever was holding has to be released
before it can key again. With VOX on, the TX button is replaced by the VOX indicator,
which is not a button: press Space (with the astar window in front) or turn on
**TX disabled — listening only** instead.

### How VOX hears you

VOX has to tell your voice from everything else the mic picks up: the fan, a
keyboard click, a pop from a Bluetooth headset's radio link. It does that with
two rules.

**It listens to loudness, not to the loudest instant.** Every 10 ms or so the
Mac hands astar a small batch of mic samples. The loudest single sample in a
batch is its *peak*. A click lasts a few milliseconds but its peak can be as
high as a spoken word's, so a VOX that went by peaks would key on a click.
astar's VOX instead takes the average power of the last 50 ms of sound (its
*RMS* loudness, in dB below full scale). A 5 ms click lands in that window as
a tenth of its energy, about 10 dB quieter than its peak, and 50 ms later it is
out of the window entirely. Your voice keeps the window full for as long as you
talk.

**The sound has to last.** The loudness has to stay at or above the threshold
for 120 ms before VOX keys, and if it dips below in that time the count starts
again. Together with the 50 ms window, that means any sound shorter than about
70 ms can never key the radio, however loud it is, while even a short "yes"
easily lasts long enough.

Waiting 120 ms would normally cost you the start of your first word. It
doesn't, because while VOX is on astar keeps the last quarter of a second of
mic audio in a *pre-roll* buffer, and when VOX keys it sends that buffer first.
The far end hears you from the beginning of the word; your transmission just
starts a fraction of a second later than you began talking. The buffer belongs
to the microphone, not to VOX, so while VOX is on every key-up sends it: press
Space, the TX button, a radio's PTT switch or the Bluetooth button, and the
quarter second before the press goes out first too. Turn VOX off and keying
goes back to sending only what comes after the press.

The bottom of the threshold slider is −58, not −60. astar's meters read −60
for silence and anything quieter, so a threshold of −60 would be met by a
silent room and VOX would key and never let go. A reading of −60 never counts
as loud, whatever the threshold.

**If you set a VOX threshold in astar 0.1.20-beta or earlier.** Those versions compared
the threshold with the peak, and the same sound reads about 10 dB lower as RMS
loudness (speech, and room noise, both peak about 10 dB above their average).
So the first time a newer astar starts, it moves your saved threshold down by
10 dB, once: −40 becomes −50, and anything that would land below −58 lands on
−58. Thresholds inside saved configs and in imported
`.astarconfig` files from older versions are converted the same way. If VOX
now feels a little more or less eager than before, nudge the slider and press
**Test** to check.

**Transmit time-out.** However you key, astar stops transmitting after a set
time on air: 2 minutes unless you change it. It is one clock for the whole
over, so handing over from Space to the Bluetooth button does not restart it.
Set it in Settings → **Transmit**, from 1 to 10 minutes; it cannot be turned
off. It is one value for every config, and it travels in config exports. It is
not the VOX **Hang Timeout** above: Hang Timeout is how long VOX waits after
you stop talking, and the transmit time-out is how long you may stay on air.
The setting's caption puts it in the app's words:

> Stops transmitting after this long on air, whatever is keying: on-screen button, Space, serial, Bluetooth or VOX. Release, or stop talking, to transmit again.

See [How keying is limited](../about/safety.md#how-keying-is-limited) for the
whole set of rules.

And one way to *not* transmit: **TX disabled — listening only** hard-mutes
transmit no matter what else is set, the Bluetooth button included. Turning it
on mid-over stops every source at once, and turning it off again keys nothing
until you press again. It is the right setting for monitoring a net, and it
pairs well with VOX while you are still tuning the threshold.

## 5. Make a contact

Back in the popover, type a node number into the dial field and press
**Connect**. A local repeater or hub node is the usual first target; the
AllStarLink [node list](https://www.allstarlink.org/nodelist) is where to find
one near you.

Nodes you connect to are saved automatically as **recents**, and you can star
one to make it a **favorite** and give it your own name — "Local 2m", not
`51234`. Favorites are unlimited; the recents list shows the ten most recent.
Both live in the list button beside the dial field, for every network at once;
see [Favorites and recents](favorites.md).

Listen first, then key up and identify.

## Things worth knowing early

**Where your settings live.** Preferences — devices, gains, VOX, favorites,
audio configs — are in `~/Library/Preferences/com.aj7hr.astar.plist`. Your
portal password is not in there; it is in the login Keychain under
`com.astar.app`, which is what you want.

Settings can be exported to a file and imported back, in whole or in part, so
a second Mac does not mean re-entering everything — see
[Audio configs, backup and transfer](configs.md). Your portal password is never
in an export; it stays in the Keychain, so the account is the one thing you
enter again. There is still no "put my config on a network drive and sync it"
option.

**Mic profiles.** The **+** next to Mic Profile in Quick settings characterizes
a microphone — measuring its noise floor and notching out whine and hum — and
saves the result as a named profile you can pick per config. Worth doing once
for each mic you use regularly.

**Configs.** A config bundles a device pair, gains, and a mic profile under a
name. **System Default** is the built-in one you start on. Save your headset
setup as its own config and switching rigs becomes one menu pick.

**Other networks.** The network picker appears once astar has more than one
network available. **M17** needs nothing installed and dials reflectors as
`host[:port]/module`. It transmits your callsign, so set it before you key;
AllStarLink identifies by node number instead.

**D-Star** uses a hardware vocoder dongle by default, so it appears in the
network picker only while one is attached, and only once you have added it in
[Settings → Vocoders](vocoders.md). No dongle? Choose **Software** for D-Star
on the same page and it appears without one. See
[Digital voice](hardware.md#digital-voice).

**Bluetooth and AirPods.** They work — they appear in the Mic and Speaker menus
like any other device. Two caveats. First, the moment macOS starts using a
Bluetooth headset's *microphone*, it drops the link into a voice mode and
playback quality falls off noticeably; this is a macOS/Bluetooth behaviour, not
an astar one, and it affects every app. If you care about receive audio, a
wired or USB headset is better. Second, Bluetooth adds latency, so if you use
VOX give yourself a longer **Hang Timeout** than you would on USB.

AirPods stem presses are not available to astar as a PTT button — macOS does not
hand third-party apps those events. On AirPods, use **VOX**.

A Bluetooth speaker-mic with its own PTT button is different: astar supports
the **Inrico B02PTT**, whose button reaches astar over Bluetooth LE while its
mic and speaker pair like any Bluetooth headset. See
[Bluetooth PTT button](bluetooth-ptt.md).

**Getting help.** Bugs and feature requests are welcome at
[github.com/rcludwick/astar/issues](https://github.com/rcludwick/astar/issues).

---

*73 — [AJ7HR](https://www.qrz.com/db/AJ7HR)*
