---
icon: lucide/bluetooth
---

# Bluetooth PTT button

astar on macOS can take push-to-talk from the button on an **Inrico B02PTT**
Bluetooth speaker-mic. Press it and astar transmits; let go and it stops,
exactly as holding the Spacebar does. The on-screen button, the Spacebar and
VOX keep working beside it.

!!! danger "The button keys a real transmitter"

    During a call you have answered, pressing the button puts you on the air.
    Read [On-air safety](../about/safety.md) first, in particular
    [How keying is limited](../about/safety.md#how-keying-is-limited).

!!! info "macOS only"

    The Windows/Linux client is paused and has no Bluetooth PTT. astar-server
    has none either: the node has no Bluetooth adapter, and a speaker-mic's
    audio belongs to the computer you operate from.

## Two links: the button and the audio

The B02PTT talks to the Mac over two separate Bluetooth links.

| Link | Carries | Pairing |
|---|---|---|
| **Bluetooth LE** | The PTT button: one message when it goes down, one when it comes up | None. astar subscribes to the button and nothing else |
| **Classic Bluetooth (HFP)** | The mic and the speaker | In System Settings → Bluetooth, like any headset |

They are independent. The button works with the audio unpaired, and the audio
works whether or not astar uses the button. astar never sends anything to the
device: it listens to the button and reads the link's signal strength, and that
is all.

## Set it up

1. Turn the B02PTT on and keep it near the Mac.
2. Open **Quick settings**, and in the **Station** card set **PTT** to
   **Inrico B02PTT**. The choice is saved to the current config straight away;
   there is no Save button for it.
3. macOS now asks whether astar may use Bluetooth. It asks at this point and
   not before, and never asks anyone who does not choose a Bluetooth button.
   The request reads:

    > astar reads the push-to-talk button on your Bluetooth speaker-mic. It never sends commands to the device.

4. astar searches ("Searching…"), finds the button, and asks you to prove it is
   yours:

    > Press the button once to confirm it's yours.

    Press it once. The line changes to "Confirmed on this Mac", then to the
    signal reading, for example "B02PTT · strong".

Until that confirm press, the button cannot key anything.

**System Default** has no PTT device. Its PTT row shows **None**, greyed out,
with the caption "Add a config in Settings → Audio configs to choose a PTT
device." To use the B02PTT, add a config of your own with the **+** on the
Audio configs header in Settings, switch to it, and choose there.

### One PTT device per config

| PTT | What keys |
|---|---|
| **None** | On-screen button and Space only |
| **AllScan UCI150** | The UCI150's PTT line, with its serial wiring |
| **Custom serial** | Another USB radio interface, with the serial wiring you set |
| **Inrico B02PTT** | The B02PTT's button |

VOX is not on this list because it is not hardware: the VOX switch works with
every choice, and so do the on-screen button and Space.

A config has one PTT device because you use one rig at a time. Choosing the
B02PTT closes that config's USB radio interface, so a handset switch and the
button can never key together. Choosing AllScan UCI150 or Custom serial again
brings the serial wiring back as you left it and stops using the button. To
move between a desk rig and the speaker-mic, make them two configs and switch
configs; switching away from a B02PTT config mid-over stops the over.

The same choice is in Settings → **Audio configs**, in each config's card,
together with everything below.

### The confirm press is per Mac

The confirmation is tied to the identifier macOS gives that one unit on this
Mac, never to its name. Another B02PTT, even one advertising the same name, is
a new device and needs its own confirm press. astar never confirms anything by
itself.

Confirmations live in this Mac's preferences and are never exported. To undo
one, open the config's card in Settings, expand **Advanced**, and click
**Forget this button**. The config still says Inrico B02PTT, but nothing keys
until you confirm again.

### Imported configs

A config that uses the B02PTT travels in an export like any other — see
[Audio configs, backup and transfer](configs.md) — but its confirmation does
not. On the receiving Mac it arrives unconfirmed: its PTT row shows
**Set up the B02PTT on this Mac**, and astar does not touch Bluetooth (no
search, no permission request) until you click it.

## Mic and speaker

To use the B02PTT's mic and speaker, pair it in System Settings → Bluetooth.
Once macOS lists its audio devices, Quick settings offers
**Use B02PTT-6522 for mic and speaker** (with your unit's own number), which
sets both pickers and saves them to the config. Until it is paired, astar says
so instead:

> Pair B02PTT-6522 in Bluetooth settings to use its mic and speaker. The button already works.

Its audio is an ordinary Bluetooth headset link, with the caveats in
[First run](first-run.md#things-worth-knowing-early): playback quality drops
while the mic is in use, and VOX wants a longer Hang Timeout than on USB.

## Self-test and status

The config's card in Settings has a **PTT self-test**. It reads
"Press and hold the PTT button." and, while you hold it, shows
"Button press detected." with a checkmark. The self-test never transmits; its caption is the reminder that the
real thing does: "During a call, pressing it transmits."

The card also shows the connection in full:

| Status | Means |
|---|---|
| Ready · signal strong | Connected and confirmed (also "fair" and "weak") |
| Searching… / Connecting… | On its way |
| Press the button once to confirm it's yours. | Connected, not yet confirmed on this Mac |
| Reconnecting… | The link dropped; astar reconnects by itself, and if the button was up the next press keys |
| Bluetooth is off | Turn it on: **Bluetooth Settings…** opens the pane |
| Bluetooth access is off for astar | Allow it: **Open Privacy & Security…** |
| This Mac has no Bluetooth LE | Nothing to set up |
| Not a supported PTT button | A device named like a B02PTT that does not carry the button service. astar disconnects and leaves it alone |
| Press ignored: release and press again | The button was down when the link dropped or as it came back, or when a forced stop or a refused press caught it. Let go, then press again |

**Advanced** shows the model, the button's Bluetooth service and
characteristic, this Mac's identifier for the unit, the last signal reading in
dBm, and **Forget this button**.

## The status chip

While the config's PTT is the B02PTT, a chip on the dial page and on the call
card shows the button's state. Its glyph fills while the button is down but
astar is not transmitting, so a refused press is visible.

| Chip | Means |
|---|---|
| B02PTT · strong | Connected, signal −65 dBm or better |
| B02PTT · fair | Signal −65 to −80 dBm |
| B02PTT · weak | Signal below −80 dBm: move closer before an over |
| B02PTT · connected | Connected, no signal reading yet |
| B02PTT · searching… / B02PTT · connecting… | Not connected yet |
| B02PTT · not confirmed | Confirm it in setup first |
| B02PTT · reconnecting… | The link is down (orange) |
| B02PTT · Bluetooth off / B02PTT · no Bluetooth access / B02PTT · Bluetooth unavailable | See the status table above (orange) |
| B02PTT · not supported | Not a button astar can use (orange) |
| B02PTT · release to arm | Let go, then press again (orange) |
| B02PTT · won't transmit | Listen-only is on, or this network is receive-only |

There is no battery reading. The B02PTT's battery level cannot be read without
pairing the button link, so the chip shows signal strength instead.

## On a call

Press to transmit, release to stop. The PTT button reads **ON AIR**, the
menu-bar icon turns red, and VoiceOver says "Transmitting", the same as for
Space. With VOX on, the call card shows the VOX indicator in place of the PTT
button. It reads "ON AIR (VOX)" during any over, the B02PTT's included, and it
is not a button.

The button keys only during a call you have answered. A press while idle or
dialing does nothing, and a button held down through the answer does not key
when the call connects: release it and press again. It never keys while
listen-only is on, or on a receive-only network.

### With Space, the on-screen button and VOX

Every way of keying combines: astar stays on air while any of them holds. VOX
going quiet does not cut off a held B02PTT, and letting go of Space does not
cut it off either while the button is still down.

The on-screen PTT button and Space are also the **stop**. Click the button, or
press Space, while the B02PTT (or VOX) holds the key, and astar stops
transmitting. While that applies, VoiceOver gives the PTT button the hint
"Stop transmitting". The B02PTT then needs releasing and a fresh press.

With VOX on there is no PTT button to click: the VOX indicator takes its place
and has no click and no VoiceOver action.
Space is the stop then, with the astar window in front, and so is turning on
**TX disabled — listening only**.

## Forced stops and release to re-arm

When astar stops an over you did not end, an orange banner says why and
VoiceOver reads it out. The banner clears after 8 seconds or at your next clean
press. There is no macOS notification.

| Cause | How fast | Banner |
|---|---|---|
| The link dropped (out of range, battery out) | Within about 2.5 seconds | Transmit stopped: B02PTT disconnected. Release the button, then press again. |
| Bluetooth switched off | At once | Transmit stopped: Bluetooth turned off. Release the button, then press again. |
| The Mac went to sleep | At once | Transmit stopped: the Mac went to sleep. Release the button, then press again. |
| astar stopped responding | Within 1.5 seconds | Transmit stopped: astar stopped responding. Release the button, then press again. |
| The button sent something unexpected | At once | Transmit stopped: B02PTT sent a value astar doesn't recognise. Release the button, then press again. |
| The transmit time-out | 2 minutes on air by default | Transmit stopped after 2 minutes. Release to talk again. |

**Release to re-arm.** After any of these the button has to come up before it
can key again. A reconnect never keys by itself: after the Mac wakes, the chip
shows "B02PTT · reconnecting…" and then the signal again, and the next over
takes a fresh press.

If the button was **up** when the link went (the Mac slept, Bluetooth was
switched off, you walked out of range between overs), there is nothing to
release: astar reconnects, the status returns to Ready, and your first press
keys. If it was **down**, or you are holding it as the link comes back, the
chip shows "B02PTT · release to arm" until you let go.

"astar stopped responding" is caught because the engine, not the app, holds
the key. While the button is down the app renews a 1.5-second lease with the
engine every 250 ms; if the app hangs, the renewals stop and the engine unkeys
on its own. The [transmit time-out](../about/safety.md#the-transmit-time-out)
is enforced the same way, and it applies to every way of keying, not only this
button.

## The button link is not authenticated

The button link is Bluetooth LE with no pairing and no encryption: the B02PTT
needs neither, and macOS gives an app no way to insist on them. In principle,
someone within Bluetooth range who imitates your confirmed button could send a
press. What that can do is bounded. It can key only during a call you have
answered, and only when listen-only is off and the network can transmit.
Pressing the on-screen button or Space (only Space when VOX is on), or the
transmit time-out firing, ends that over — but a device that then sends a release and a new press can key
again. To stop it lasting, turn on **TX disabled — listening only**, set the
config's PTT to **None** (which detaches the button), or click **Forget this
button**. Ending the call stops it only for that call. It opens your own microphone on the
air; it cannot send audio of its own, change what you are connected to, or key
while astar is idle.

An unexpected key is visible: the menu-bar icon turns red, the PTT button reads
**ON AIR** (with VOX on, the VOX indicator reads "ON AIR (VOX)"), and VoiceOver
says "Transmitting". If you see one you did not cause, press the on-screen
button to stop it, or Space when VOX is on (the indicator is not a button), or
turn on **TX disabled — listening only**. Then keep listening only on, or set
PTT to **None**, until you know why.
