// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Live example: drive a call's PTT from a UCI150. Requires hardware; a no-op
//! (prints guidance) without a device. Run: `cargo run -p astar-serial-sys
//! --example serial_parrot`.

use std::ffi::CString;
use std::{thread, time::Duration};

use astar_serial_sys::{
    IaxKeyLine, IaxRadioLine, IaxRxKeyMode, IaxSerialConfig, IaxSerialTransport, iax_serial_close,
    iax_serial_open, iax_serial_ptt_tick,
};

fn main() {
    let mut buf = [0i8; 256];
    let rc = unsafe { astar_serial_sys::iax_serial_autodetect(buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        println!("no UCI150 serial device found; attach one and retry");
        return;
    }
    let path = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_owned();
    let path_c = CString::new(path.to_bytes()).unwrap();
    let cfg = IaxSerialConfig {
        port_path: path_c.as_ptr(),
        transport: IaxSerialTransport::Tty,
        key_line: IaxKeyLine::Cts,
        key_active_high: true,
        radio_line: IaxRadioLine::Rts,
        radio_active_high: true,
        cts_debounce_ms: 30,
        rx_mode: IaxRxKeyMode::RemotePtt,
        rx_floor_db: -45.0,
        rx_hang_ms: 250,
    };
    let h = unsafe { iax_serial_open(&raw const cfg) };
    assert!(!h.is_null(), "open failed");
    println!("serial PTT open on {path:?}. Key the handset (Ctrl-C to quit).");
    loop {
        let (mut on, mut changed) = (false, false);
        let rc = unsafe { iax_serial_ptt_tick(h, false, -60.0, &raw mut on, &raw mut changed) };
        if rc < 0 {
            eprintln!("tick error {rc}");
            break;
        }
        if changed {
            println!("PTT -> {on}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    unsafe { iax_serial_close(h) };
}
