---
icon: lucide/cable
---

# Hardware

astar targets the **generic class of USB radio interfaces**: a serial line that
carries push-to-talk, plus a USB audio device that carries the audio. The AllScan
UCI150 (WCH CH343) is the reference device used during development — it is not a
special case, and nothing about it is hard-coded.

## USB radio interfaces

There are two ways in. **astar uses the raw-USB backend by default, and it
needs no driver at all** — you should not have to install anything to key a
radio.

=== "Raw USB backend (default)"

    Talks to the device directly over USB. **No driver, no system extension,
    nothing to approve.** This is what a fresh install selects, and it is the
    only path that works inside the App Store sandbox.

=== "tty backend (manual opt-in)"

    Uses a `/dev/cu.*` serial port instead. **The app has no switch for this**
    — nothing in the UI can put you on the tty path. It exists for adapters the
    raw-USB backend cannot claim, and reaching it means setting the preference
    by hand:

    ```bash
    defaults write com.aj7hr.astar serial.transport -int 0   # 0 = tty, 1 = raw USB
    ```

    On macOS a CH34x-class adapter then needs a third-party driver, which
    arrives as a system extension you have to approve:

    ```bash
    brew install --cask wch-ch34x-usb-serial-driver
    ```

    Approve it in **System Settings › Privacy & Security**. Once it loads, the
    port appears as `/dev/cu.wchusbserial*`.

    `wch-ch34x-usb-serial-driver` is published by WCH; it is not an astar
    package. It serves *all* WCH parts — if you also use CH340/CH341
    radio-programming cables, they need it even though astar does not.

!!! tip "You almost certainly do not need any of that"

    Raw USB is the default and the UCI150 works on it. Unless you have an
    adapter the raw-USB path cannot claim, leave this alone.

## PTT wiring

Two independent lines, and it matters which is which:

| Line | Direction | What it is | UCI150 default |
|---|---|---|---|
| **Key line** | *input* — the interface tells astar | The operator has pressed the PTT switch on the radio or mic. | **CTS** |
| **Radio line** | *output* — astar tells the interface | Key the transmitter. | **RTS** |

Both are selectable (key line: CTS, DCD, DSR, RI; radio line: RTS, DTR), both
have a polarity toggle, and there is a debounce control in milliseconds.

On the UCI150 specifically, set the **PTT DEST switch to CTS** so the operator
key reaches astar on the line it expects.

### First-time UCI150 checklist

1. On the UCI150, set the **PTT DEST switch to CTS**.
2. Plug it in.
3. Enable serial PTT in astar's serial settings and leave the transport on
   **raw USB**.

That is the whole list — no driver, no reboot, no system extension.

If astar reports no device, check the cable and the switch first. The message
*no `/dev/cu.wchusbserial*` port found* is specific to the tty backend, so
seeing it means the transport got switched to tty; either switch it back or
install the driver above.

!!! info "Serial I/O never hangs the UI"

    All serial I/O runs on a worker thread inside the engine. A wedged USB
    transfer surfaces as a *serial device error* and the device is disabled —
    it can never freeze the interface.

## Bluetooth PTT buttons

astar on macOS can take push-to-talk from the button on a Bluetooth
speaker-mic. One device is supported:

| Device | Advertises as | Button | Mic and speaker |
|---|---|---|---|
| **Inrico B02PTT** | `B02PTT-` and a unit number | Bluetooth LE, no pairing | Classic Bluetooth (HFP), paired in System Settings |

Setting it up, the status chip and what happens when the link drops are on
[Bluetooth PTT button](bluetooth-ptt.md).

* **One PTT device per config.** A config uses none, a USB radio interface, or
  the Bluetooth button. Choosing the button closes that config's serial
  interface, so the two never key together.
* **Two independent links.** The button works without the audio paired, and
  the audio works without the button.
* **Only devices astar knows.** There is no way to type in a Bluetooth service
  or characteristic. A mistyped one on some other gadget must never become a
  transmit button — the same reasoning as the dongle scan below. Adding a
  device takes a listen-only capture of what its button sends, a row in the
  engine's device table, and tests built from that capture.
* **No battery reading.** The B02PTT's battery level cannot be read without
  pairing the button link; astar shows signal strength instead.

## Digital voice

**System Fusion, NXDN and DMR need a hardware vocoder dongle. D-Star uses one
by default**, or the built-in software vocoder if you choose it. Add the dongle once in **Settings → Vocoders** (see
[Vocoders](vocoders.md)); after that those networks appear in the network
picker while it is attached and disappear when you unplug it.

### Why the hardware is required

These modes carry their voice as AMBE, DVSI's codec. A dongle carries an
implementation on a chip, so the codec runs there instead of in the app.

D-Star uses the older **AMBE 3600×2400**, whose patents have expired. astar can
therefore be built with a **software vocoder for D-Star** (Bruce Perens'
implementation), chosen in [Settings → Vocoders](vocoders.md#which-vocoder-each-mode-uses).
It is built into astar; D-Star uses it only when you choose it.

System Fusion, NXDN and DMR use **AMBE+2**, which is still covered by
US patent 8,359,197 until 2028-05-20. For them the dongle is the only way, and
astar has no software vocoder.

This is specific to AMBE. **M17** uses Codec 2, which is free software, and
**AllStarLink** uses G.711 — neither needs a dongle, and neither is affected by
any of this.

If you want the record rather than the folklore, the patent position is written
up separately in **[The patent landscape](https://rcludwick.github.io/how-ambe-works/16-patents/)**
— part of [How AMBE Works](https://rcludwick.github.io/how-ambe-works/), which
walks through the codec itself from sinusoids to the D-Star frame. It sets out
which patents cover what, which have expired, and which two are still in force.
It is a summary of the public record, not legal advice, and says so.

### Where to get one

| Dongle | |
|---|---|
| **[DVMEGA DVstick 30](https://www.gigaparts.com/dvmega-dvstick-30.html)** | AMBE+2 on a USB stick, sold by GigaParts. Also on the [manufacturer's page](https://www.dvmega.nl/dvstick30/). Used with astar. |
| **ThumbDV / DV3000** | The same idea from NW Digital Radio. Used with astar. |

Both are known to work. astar's D-Star path was first verified live on a
ThumbDV against the KC-Wide XLX458 reflector, and the DVstick 30 is what it is
developed against day to day.

!!! note "Why both work, and what would not"

    astar offers a dongle for adding when it presents the FTDI id
    **`0x0403:0x6015`** — an FT230X — and it opens only dongles whose USB
    serial number you have added in [Settings → Vocoders](vocoders.md). Both
    dongles above present that id, so both can be added. A dongle built
    around a different USB-serial chip could not be, however good the vocoder
    inside it is.

    The id alone is not enough, because many radio CAT/PTT cables use the
    same chip and look identical on USB, and opening one of those can key a
    transmitter. That is why each stick is added by hand.

### Which modes this covers

| Mode | Dongle | Status |
|---|---|---|
| **D-Star** | by default; optional with the software vocoder | Works. XLX/XRF reflectors over DExtra. |
| **DMR** | required | Receive only. Link a talkgroup and timeslot on one network's master and hear it; astar has no DMR transmit path yet. |
| **YSF** | required | Works. YSF reflectors, receive and transmit. |
| **NXDN** | required | Receive only. Link a talkgroup on an NXDNReflector and hear it; astar has no NXDN transmit path yet. |
| **M17** | not needed | Works. Codec 2, built in. |
| **AllStarLink** | not needed | Works. |

Detection is hotplug rather than latched at launch, so the capability follows
the hardware — unplug a trusted dongle mid-session and the D-Star option leaves
the network picker. If you are working from the Rust crates rather than the app, see
[A vocoder dongle](../build/prerequisites.md#a-vocoder-dongle-only-for-d-star)
for which builds include D-Star at all.

!!! danger "astar opens only the dongles you added"

    The rule governing the dongle is a safety rule rather than a convenience:
    astar opens a vocoder port only when its USB serial number is on the list
    in [Settings → Vocoders](vocoders.md). `IAX_THUMBDV_PORT` only ever
    **narrows** the choice among those trusted dongles and can never point the
    opener at another serial port. Opening a USB radio interface's tty
    asserts RTS, which keys a transmitter. See
    [On-air safety](../about/safety.md).

## Audio devices

Input and output devices are chosen in Quick settings or in the full device
settings; the UCI150 and similar interfaces show up as ordinary USB audio
devices. astar also carries microphone characterization and per-profile gain,
so a headset and a radio interface can each keep their own levels.

### "Speaker stopped" and "Microphone stopped"

Some devices change format while a call is up. A Bluetooth headset plays at
44.1 kHz stereo until its mic opens, then switches to hands-free at 16 kHz
mono, and switches back when the mic closes. astar notices and reopens the
speaker or mic on the new format by itself, so the call carries on. If it
cannot reopen it within about a second (usually because the device was
unplugged or turned off), the main window says so under the dial card, for
example **Speaker stopped: B02PTT-6522**. The call stays up but you will not
hear it (or, for the mic, not be heard). Reconnect the device, then hang up
and call again; the next call opens it afresh.

### Monitor speaker

Picture the usual setup: the Mac's output goes to a USB radio interface, so
what you hear in the room is nothing at all. The audio is going down a cable
into the radio. The **monitor** fixes that. It plays a second copy of the
output on another device, such as the Mac's own speakers or your headphones,
so you can listen to what goes to the radio.

**Turn it on in Quick settings.** Open **Quick settings** and find the
**Speaker** card. Under **Out** and **Vol** there is a **Monitor** menu. It
starts at **Off**. Pick a device from the list and the copy starts playing
right away, even in the middle of a call. Choose **Off** to stop it. The
choice is saved with the active config.

**Turn it on in a config.** Each config card in Settings → Configs has a
**Monitor** row directly under **Output**, with the same list. A config
remembers its own monitor, so your radio-interface config can use the Mac
speakers while your headset config uses none. See
[Configs](configs.md).

**Set how loud it is.** Once a device is chosen, a **Volume** slider appears
under the menu. It runs from 0 to 400 %. It is separate from **Vol** for the
main output, so a monitor speaker that is too loud can be turned down on its
own. In Quick settings the volume is saved with the config when you choose
**Save changes**, like the other gains.

**Mute it from the call controls.** While a monitor is set, a speaker button
appears in the call controls, between the packet capture button and
**Disconnect**. Click it to mute the monitor, and click again to unmute. A
small grey **Muted** tag appears beside the Monitor menu while it is muted.
Muting lasts for the session. It is not saved, so a restart starts unmuted.

**What you hear.** The monitor carries what the main output plays: the other
station's audio, plus anything astar plays to the output. Your own microphone
is not included. Because it is a second device playing the same audio, it can
trail the main output by up to about 150 ms.

**Two messages you may see.**

* **Monitor turned off: it was the same device as the output.** A monitor
  cannot play on the device the output already uses. If you change **Out** to
  your monitor's device, astar switches the monitor to **Off** and says so.
  Pick a different device to turn it back on.
* **Monitor unavailable**, with a reason after it. A red triangle appears
  beside the menu when the chosen device cannot be opened, for example
  because it was unplugged or turned off. The main output keeps playing. Plug
  the device back in and the monitor starts again.

!!! note
    On AllStarLink (IAX2) calls the monitor can keep the device open after a
    hangup, playing silence, which stops a Bluetooth or USB speaker from going
    to sleep. Choose **Off** to release it. This is on the backlog as
    `astar-monitor-idle-close`.

## Next steps

* [Using astar](usage.md) — day-to-day operating.
