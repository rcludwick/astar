// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// Live example: requires a UCI150 attached. Builds offline; the loop is a no-op
// without a device (autodetect → open fails → prints guidance).
import Foundation
import AstarSerial

var cfg = SerialConfig()
cfg.portPath = SerialClient.autodetect()
guard cfg.portPath != nil else {
    print("no UCI150 serial device found; attach one and retry"); exit(0)
}
let serial = try SerialClient(cfg)
print("serial PTT open on \(cfg.portPath!). Key the handset (Ctrl-C to quit).")
// In a real app, also open an AstarStation, dial 55553, and feed snapshot values.
while true {
    let (changed, on) = try serial.pttTick(remoteKeyed: false, rxDb: -60.0)
    if changed { print("PTT -> \(on)") }
    Thread.sleep(forTimeInterval: 0.02)
}
