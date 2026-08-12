#!/usr/bin/env -S uv run --script
# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
# /// script
# requires-python = ">=3.11"
# ///
"""Watch ALL serial modem-control lines of a port (via TIOCMGET) and print
whenever any line changes, plus a periodic "steady" heartbeat so a stuck/held
line is visible rather than looking like the monitor stopped.

Used to map how the UCI150 "PTT DEST" switch routes the handset/COS signal
(iax-8e3b): open the UCI150 serial port, then key/unkey the handset and watch
which line(s) move and the polarity. On this Mac, CTS is NOT surfaced by the
built-in CDC driver but DCD is — set PTT DEST to DCD.

Usage:
  uv run scripts/uci150-ptt-monitor.py [/dev/cu.usbmodemXXXX]
Defaults to /dev/cu.usbmodem5B210098241 (Rob's UCI150 / WCH CH343). Ctrl-C stops.
"""

import fcntl
import glob
import os
import struct
import sys
import termios
import time


def default_port() -> str:
    # Prefer the WCH vendor-driver port (cu.wchusbserial*, which tracks the DCD
    # carrier edges the stock CDC driver misses), else the stock CDC port.
    for pattern in ("/dev/cu.wchusbserial*", "/dev/cu.usbmodem*"):
        matches = sorted(glob.glob(pattern))
        if matches:
            return matches[0]
    return "/dev/cu.usbmodem5B210098241"


port = sys.argv[1] if len(sys.argv) > 1 else default_port()

# Every modem-control bit TIOCMGET can report. DTR/RTS are outputs (we assert
# them by opening the port); CTS/DCD/DSR/RI are the inputs that the radio side
# drives. LE/ST/SR are rarely wired but shown if the platform defines them.
BITS: list[tuple[str, int]] = [
    ("DTR", termios.TIOCM_DTR),
    ("RTS", termios.TIOCM_RTS),
    ("CTS", termios.TIOCM_CTS),
    ("DCD", termios.TIOCM_CAR),  # carrier detect
    ("DSR", termios.TIOCM_DSR),
    ("RI", termios.TIOCM_RNG),  # ring indicator
]
for name, attr in (("LE", "TIOCM_LE"), ("ST", "TIOCM_ST"), ("SR", "TIOCM_SR")):
    if hasattr(termios, attr):
        BITS.append((name, getattr(termios, attr)))


def read_bits(fd: int) -> int:
    buf = fcntl.ioctl(fd, termios.TIOCMGET, struct.pack("I", 0))
    return struct.unpack("I", buf)[0]


def fmt(bits: int) -> str:
    return "  ".join(f"{name}={1 if bits & mask else 0}" for name, mask in BITS)


def main() -> None:
    try:
        fd = os.open(port, os.O_RDWR | os.O_NONBLOCK | os.O_NOCTTY)
    except OSError as exc:
        print(f"ERROR opening {port}: {exc}", file=sys.stderr)
        sys.exit(1)

    # IMPORTANT: opening the port asserts RTS+DTR by default. On the UCI150,
    # RTS is the radio PTT (keys the transmitter) — leaving it high would key
    # the radio and pollute the COS/DCD reading. Clear both outputs so we only
    # ever READ here and never key the radio.
    fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("I", termios.TIOCM_RTS | termios.TIOCM_DTR))

    print(f"Watching ALL modem lines on {port} (RTS/DTR cleared — not keying the radio).")
    print("Key/unkey the handset; watch which line(s) move and the 0/1 polarity.")
    print("Lines: DTR RTS are OUTPUTS (we drive); CTS DCD DSR RI are INPUTS.\n")

    last = None
    last_print = 0.0
    while True:
        bits = read_bits(fd)
        now = time.monotonic()
        if bits != last:
            print(
                f"{time.strftime('%H:%M:%S')}  {fmt(bits)}   (raw=0x{bits:04x})",
                flush=True,
            )
            last = bits
            last_print = now
        elif now - last_print > 3.0:
            print(f"{time.strftime('%H:%M:%S')}  …steady   {fmt(bits)}", flush=True)
            last_print = now
        time.sleep(0.02)


if __name__ == "__main__":
    main()
