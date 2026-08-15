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

## Vocoder dongles

The tree contains support for a USB vocoder dongle, used by work that has not
reached the clients. Nothing in the macOS app uses it today, and you do not need
one to run astar.

If you do have one attached, the rule that governs it is a safety rule rather
than a convenience: `IAX_THUMBDV_PORT` only ever **narrows** the USB VID/PID
scan and can never point the opener at an arbitrary serial port. Opening a USB
radio interface's tty asserts RTS, which keys a transmitter. See
[On-air safety](../about/safety.md).

## Audio devices

Input and output devices are chosen in Quick settings or in the full device
settings; the UCI150 and similar interfaces show up as ordinary USB audio
devices. astar also carries microphone characterization and per-profile gain,
so a headset and a radio interface can each keep their own levels.

## Next steps

* [Using astar](usage.md) — day-to-day operating.
