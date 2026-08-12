# astar — Copyright (c) 2026 Rob Ludwick.
# SPDX-License-Identifier: AGPL-3.0-only
# Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
"""Live example: requires a UCI150 attached. No-op without a device."""
import time

from astarserial import SerialClient, SerialConfig

path = SerialClient.autodetect()
if path is None:
    print("no UCI150 serial device found; attach one and retry")
    raise SystemExit(0)

with SerialClient(SerialConfig(port_path=path)) as serial:
    print(f"serial PTT open on {path}. Key the handset (Ctrl-C to quit).")
    while True:
        changed, on = serial.ptt_tick(remote_keyed=False, rx_db=-60.0)
        if changed:
            print(f"PTT -> {on}")
        time.sleep(0.02)
