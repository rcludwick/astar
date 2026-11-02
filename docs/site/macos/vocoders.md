---
icon: lucide/cpu
---

# Vocoders (DVstick / ThumbDV)

D-Star, System Fusion, NXDN and DMR carry their voice as AMBE, which astar
decodes on a USB **vocoder stick**: a DVMEGA DVstick 30, or a ThumbDV / DV3000.
Before astar will use a stick, you add it once in **Settings → Vocoders**.
This page explains why, how to add and remove one, and which sticks work.

## Why you have to add the stick

A vocoder stick and many radio **CAT/PTT cables** use the same FTDI USB chip.
Over USB they look identical: the same `0403:6015` id and the same
"FT230X Basic UART" name. Nothing on the USB side tells a stick from a
cable.

Opening a serial port on macOS raises its DTR and RTS lines. On a cable wired
to a radio, that can **key the transmitter**. Earlier builds of astar found a
stick by opening every port with that id, so a D-Star connect could briefly
key a radio plugged into such a cable.

So astar now opens a vocoder port only when its **USB serial number** is on
the list you build here. With nothing on the list, astar opens no vocoder
port at all and never looks for one. D-Star, Fusion, NXDN and DMR then stay
out of the network picker. While a vocoder-class device is plugged in and
nothing is on the list, the call card says so, with an **Add Vocoder…**
button; dismiss it with **×** if the device is a cable. A connect refused for
this reason reads *"No trusted vocoder — add one in Settings → Vocoders"* and
offers an **Open Vocoder Settings…** button.

## Adding a stick

1. Plug the stick in.
2. Open **Settings** (the gear in the window's footer, or ⌘,) and find
   **Vocoders**. Click **+** in the section header, or **Add Vocoder…**.
3. The sheet lists the vocoder-class USB devices attached right now, by
   serial number and port. Building this list only reads USB descriptors and
   **opens nothing**. Sticks you have already added are left out.
4. Choose your stick. Read the warning, then tick **This is a vocoder stick,
   not a radio cable**. The box clears whenever you choose a different
   device, so it always refers to the device you are about to check.
5. Click **Check and Add**. astar opens that one port, holds DTR and RTS low
   before writing anything, asks the chip for its product ID with a short
   timeout, and closes the port again. This takes a couple of seconds.

The sheet then says what it found:

| Result | What happens |
|---|---|
| **Vocoder added** (an `AMBE3000` chip: DVstick 30, ThumbDV, DV3000) | The stick is added to the list. D-Star, Fusion, NXDN and DMR appear in the network picker while it is plugged in. |
| **DVstick 33 isn't supported yet** (an `AMBE3003` chip) | Recognised, but **not** added, because astar cannot drive it yet. See below. |
| **Not a supported vocoder** | Something answered with another product ID. Not added. |
| **Not a vocoder** | Nothing answered as a vocoder. It may be a radio cable. Not added. |
| **Couldn't open the device** | The port would not open, for example because another app holds it. Nothing was written to it. |

!!! danger "Never add a radio cable"

    Add only a device you know is a vocoder stick. If you are not sure which
    `usbserial` port is which, unplug the radio cable before you add the
    stick. Once a serial is on the list, astar will open it whenever you
    connect to D-Star, Fusion, NXDN or DMR.

If the sheet shows no devices, check the stick is plugged in and click
**Refresh**. A device with no USB serial number cannot be added, because
there is nothing stable to recognise it by.

## The list

Each stick you have added shows:

* a **dot**: green while the stick is plugged in, grey while it is not;
* its **model** and **serial number**, and the chip's product ID;
* where it is now (**Connected · /dev/cu.usbserial-…**) or, when unplugged,
  when this Mac **last saw** it.

The list refreshes every couple of seconds while Settings is open. Like the
sheet, this only lists USB devices and opens nothing.

**Removing a stick.** Click the trash button on its row, or right-click the
row and choose **Remove**. astar stops opening it straight away. A connection
already using it keeps going until you hang up. To use the stick again, add
it again.

## It stays on this Mac

The list is stored in this Mac's preferences under `vocoder.trusted`. It is
**never included in an exported config**, whichever sections you tick, and an
imported config can never add to it. A serial number names one physical
device, and on another Mac the same serial could belong to something else.
Add your sticks again on each Mac.

## DVstick 30 and DVstick 33

| Stick | Chip | Status |
|---|---|---|
| **DVMEGA DVstick 30** | AMBE-3000 | Supported. |
| **ThumbDV / DV3000** | AMBE-3000 | Supported. |
| **DVMEGA DVstick 33** | AMBE-3003 (three channels) | Recognised, not supported yet. |

The AMBE-3003 is a three-channel part with a different control protocol.
Support for it is planned. Until then, add a DVstick 30 or a ThumbDV.

## The `IAX_THUMBDV_PORT` pin

`IAX_THUMBDV_PORT` is an environment variable for developers. It narrows the
choice to one port **among the trusted sticks**. It cannot add a port: a pin
that names a port whose serial is not on the list is refused. See
[On-air safety](../about/safety.md).

## On astar-server

The node daemon keeps the same kind of list in `node.toml`, as
`[vocoder] trusted_serials`, and adds sticks with `astar-server vocoder add`.
See [Configuration](../server/configuration.md#vocoder-trusted-ambe-vocoder-sticks).
