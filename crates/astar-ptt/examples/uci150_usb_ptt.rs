// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Hardware check for the production raw-USB backend (iax-d937): open
//! `Uci150Usb`, poll the operator-key line, and print every transition. Unlike
//! the iax-ceba spike (which pokes USB directly), this exercises the real
//! `PttBackend` path: `open_wch` -> `read_key` (req 0x95 + decode) and, with
//! `--key`, `set_radio_key` (CDC req 0x22) + `fail_safe`.
//!
//! Run: `cargo run -p astar-ptt --features serial-usb --example uci150_usb_ptt`
//! Read-only by default; pass `--key` to pulse RTS (key the radio) once at start.
//! Requires the WCH dext to be REMOVED (generic CDC-ACM driver is fine).

use std::time::Duration;

use astar_ptt::{PttBackend, Uci150Usb};

fn main() {
    let key = std::env::args().any(|a| a == "--key");
    println!("UCI150 raw-USB PTT backend check (iax-d937)");

    let mut backend = match Uci150Usb::open() {
        Ok(b) => {
            println!("opened Uci150Usb (default DCD-in / RTS-out)");
            b
        }
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(1);
        }
    };

    if key {
        println!("pulsing RTS (keying radio) for 500ms...");
        if let Err(e) = backend.set_radio_key(true) {
            eprintln!("set_radio_key failed: {e}");
        }
        std::thread::sleep(Duration::from_millis(500));
        backend.fail_safe();
        println!("radio unkeyed (fail_safe).");
    }

    println!("polling operator key — press/release the handset PTT (Ctrl-C to stop)");
    let mut last = None;
    loop {
        match backend.read_key() {
            Ok(down) => {
                if last != Some(down) {
                    println!("  PTT {}", if down { "DOWN (keyed)" } else { "up" });
                    last = Some(down);
                }
            }
            Err(e) => {
                eprintln!("read_key failed: {e}");
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
