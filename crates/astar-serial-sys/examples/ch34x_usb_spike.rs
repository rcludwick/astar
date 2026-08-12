// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! De-risk spike (iax-ceba): can we drive the UCI150's CH34x PTT lines over raw
//! USB / IOKit (via `nusb`) instead of the `/dev/cu.*` tty exposed by the CH34x
//! dext? This gates the whole nugget — if we can't claim the interface (or at
//! least reach the device's control endpoint) while the kernel driver is
//! present, the raw-USB / Mac-App-Store path is not viable on this hardware.
//!
//! It walks four gates and prints PASS/FAIL for each:
//!   1. ENUMERATE  — find a WCH (VID 0x1A86) device and identify the exact chip.
//!   2. OPEN       — open the IOUSBHostDevice.
//!   3. CLAIM      — claim the data interface for exclusive use.
//!   4. MODEM CTRL — assert RTS (key) + read CTS (operator key) via vendor
//!                   control requests, using the CH341-family protocol.
//!
//! Run: `cargo run -p astar-serial-sys --example ch34x_usb_spike`
//! Read-only by default; pass `--key` to actually toggle RTS on the radio.
//!
//! `--monitor` enters a live loop (until Ctrl-C) that watches CTS via BOTH the
//! WCH vendor status register (req 0x95) AND the CDC interrupt-IN notification
//! endpoint, printing on every change. Press/release the handset PTT to learn
//! which channel actually carries the operator-key line on this CDC-ACM CH343 —
//! the one open question after the gates pass with the driver removed.

// One-off de-risk scaffolding: loose lints are acceptable here. The production
// backend (clippy-clean) is `crates/astar-ptt/src/uci150_usb.rs`.
#![allow(
    dead_code,
    clippy::doc_markdown,
    clippy::doc_overindented_list_items,
    clippy::identity_op,
    clippy::no_effect,
    clippy::items_after_statements,
    clippy::too_many_lines
)]

use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlOut, ControlType, In, Interrupt, Recipient};

const WCH_VID: u16 = 0x1A86;

// CH341-family vendor requests (Linux drivers/usb/serial/ch341.c).
const CH34X_REQ_READ_REG: u8 = 0x95;
const CH34X_REQ_MODEM_CTRL: u8 = 0xA4;

// CDC-ACM class request (this CH343 enumerates as CDC-ACM 0x02/0x02/0x01).
const CDC_SET_CONTROL_LINE_STATE: u8 = 0x22;

// Modem-control output bits (active-low on the wire: the request sends ~bits).
const CH34X_BIT_RTS: u8 = 1 << 6;
const CH34X_BIT_DTR: u8 = 1 << 5;

// Modem-status input bits returned by the status read (also active-low).
const CH34X_BIT_CTS: u8 = 1 << 0;
const CH34X_BIT_DSR: u8 = 1 << 1;
const CH34X_BIT_RI: u8 = 1 << 2;
const CH34X_BIT_DCD: u8 = 1 << 3;

const TIMEOUT: Duration = Duration::from_millis(200);

fn pass(gate: &str, msg: &str) {
    println!("  [PASS] {gate}: {msg}");
}
fn fail(gate: &str, msg: &str) {
    println!("  [FAIL] {gate}: {msg}");
}

/// Best-effort name for a WCH product id, so we know which protocol applies.
fn chip_name(pid: u16) -> &'static str {
    match pid {
        0x7523 => "CH340 (the chip the spec assumed)",
        0x5523 => "CH341",
        0x55D2 => "CH9102 / CH343 family",
        0x55D3 => "CH343",
        0x55D4 => "CH9102",
        0x55D5 => "CH9103",
        0x55D8 => "CH9101",
        _ => "unknown WCH chip",
    }
}

fn main() {
    let key = std::env::args().any(|a| a == "--key");
    let monitor = std::env::args().any(|a| a == "--monitor");
    println!("CH34x raw-USB PTT de-risk spike (iax-ceba)");
    println!(
        "mode: {}\n",
        if key {
            "ACTIVE (will toggle RTS)"
        } else {
            "read-only (pass --key to toggle RTS)"
        }
    );

    // ---- Gate 1: ENUMERATE -------------------------------------------------
    println!("Gate 1 — ENUMERATE (looking for VID 0x1A86)");
    let devices = match nusb::list_devices().wait() {
        Ok(d) => d,
        Err(e) => {
            fail("enumerate", &format!("list_devices failed: {e}"));
            return;
        }
    };
    let wch: Vec<_> = devices.filter(|d| d.vendor_id() == WCH_VID).collect();
    if wch.is_empty() {
        fail(
            "enumerate",
            "no WCH (0x1A86) device found — is the UCI150 plugged in?",
        );
        return;
    }
    // The UCI150 exposes a WCH USB *hub* (class 0x09) plus the serial chip behind
    // it. Skip the hub and target the actual serial device.
    const USB_CLASS_HUB: u8 = 0x09;
    for d in &wch {
        println!(
            "        seen {:04x}:{:04x} = {} ({:?})",
            d.vendor_id(),
            d.product_id(),
            chip_name(d.product_id()),
            d.product_string(),
        );
    }
    let Some(info) = wch
        .iter()
        .find(|d| d.interfaces().all(|i| i.class() != USB_CLASS_HUB))
    else {
        fail(
            "enumerate",
            "only a WCH hub found, no serial chip behind it",
        );
        return;
    };
    pass(
        "enumerate",
        &format!(
            "VID:PID {:04x}:{:04x} = {} | product={:?} serial={:?}",
            info.vendor_id(),
            info.product_id(),
            chip_name(info.product_id()),
            info.product_string(),
            info.serial_number(),
        ),
    );
    for i in info.interfaces() {
        println!(
            "        interface #{} class={:#04x} subclass={:#04x} protocol={:#04x}",
            i.interface_number(),
            i.class(),
            i.subclass(),
            i.protocol(),
        );
    }

    // ---- Gate 2: OPEN ------------------------------------------------------
    println!("\nGate 2 — OPEN (IOUSBHostDevice)");
    let device = match info.open().wait() {
        Ok(d) => {
            pass("open", "opened the USB device");
            d
        }
        Err(e) => {
            fail("open", &format!("could not open device: {e}"));
            println!("\n=> The kernel CH34x driver almost certainly holds the device exclusively.");
            println!(
                "   Raw-USB control is blocked while the dext/kext is matched. See conclusion."
            );
            return;
        }
    };

    if monitor {
        run_monitor(&device);
        return;
    }

    // Find the data interface + its interrupt-IN endpoint (modem-status reports).
    let mut data_iface: Option<u8> = None;
    let mut interrupt_in: Option<u8> = None;
    if let Ok(cfg) = device.active_configuration() {
        for iface in cfg.interfaces() {
            for alt in iface.alt_settings() {
                let mut has_bulk = false;
                for ep in alt.endpoints() {
                    let dir = ep.direction();
                    let tt = ep.transfer_type();
                    println!(
                        "        ep {:#04x} dir={dir:?} type={tt:?} mps={}",
                        ep.address(),
                        ep.max_packet_size(),
                    );
                    if format!("{tt:?}") == "Interrupt" && format!("{dir:?}") == "In" {
                        interrupt_in = Some(ep.address());
                    }
                    if format!("{tt:?}") == "Bulk" {
                        has_bulk = true;
                    }
                }
                if has_bulk {
                    data_iface = Some(iface.interface_number());
                }
            }
        }
    }
    let iface_num = data_iface.unwrap_or(0);
    println!(
        "        => data interface #{iface_num}, interrupt-IN endpoint {}",
        interrupt_in.map_or("(none found)".to_string(), |e| format!("{e:#04x}")),
    );

    // ---- Gate 3: CLAIM -----------------------------------------------------
    println!("\nGate 3 — CLAIM interface #{iface_num}");
    let claimed = match device.claim_interface(iface_num).wait() {
        Ok(i) => {
            pass("claim", "claimed the interface for exclusive use");
            Some(i)
        }
        Err(e) => {
            fail("claim", &format!("could not claim interface: {e}"));
            println!(
                "        (control transfers via the device endpoint may still work; trying anyway)"
            );
            None
        }
    };

    // ---- Gate 4: MODEM CONTROL --------------------------------------------
    // Control transfers go on the default control endpoint (the Device handle on
    // macОS), so we can attempt them whether or not the interface claim landed.
    println!("\nGate 4 — MODEM CONTROL (vendor control requests)");

    // Read current modem status: REQ_READ_REG of the (0x06,0x07) register pair.
    match device
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: CH34X_REQ_READ_REG,
                value: 0x0706,
                index: 0x0000,
                length: 2,
            },
            TIMEOUT,
        )
        .wait()
    {
        Ok(buf) if !buf.is_empty() => {
            let raw = buf[0];
            let s = !raw; // active-low on the wire
            pass(
                "read CTS",
                &format!(
                    "status raw={raw:#04x} -> CTS={} DSR={} DCD={} RI={}",
                    (s & CH34X_BIT_CTS) != 0,
                    (s & CH34X_BIT_DSR) != 0,
                    (s & CH34X_BIT_DCD) != 0,
                    (s & CH34X_BIT_RI) != 0,
                ),
            );
        }
        Ok(_) => fail("read CTS", "status read returned no bytes"),
        Err(e) => fail("read CTS", &format!("status read failed: {e}")),
    }

    // Assert RTS (key the radio) only when --key is given; restore on exit.
    if key {
        let assert = (!(CH34X_BIT_RTS)) & 0xFF; // assert RTS, DTR low
        match device
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: CH34X_REQ_MODEM_CTRL,
                    value: u16::from(assert),
                    index: 0x0000,
                    data: &[],
                },
                TIMEOUT,
            )
            .wait()
        {
            Ok(()) => pass("set RTS", "asserted RTS (radio keyed) for ~500ms"),
            Err(e) => fail("set RTS", &format!("modem-ctrl write failed: {e}")),
        }
        std::thread::sleep(Duration::from_millis(500));
        // Deassert everything.
        let _ = device
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: CH34X_REQ_MODEM_CTRL,
                    value: 0x00FF,
                    index: 0x0000,
                    data: &[],
                },
                TIMEOUT,
            )
            .wait();
        println!("        RTS deasserted (radio unkeyed)");
    } else {
        println!("  [skip] set RTS: read-only run; pass --key to drive RTS");
    }

    // ---- Gate 5: OUTPUT CONTROL VIA EP0 WITHOUT CLAIM ----------------------
    // Since CLAIM fails (driver holds the interface), the only remaining PTT-out
    // path is a class request through the device's default control endpoint.
    // Probe it harmlessly: SET_CONTROL_LINE_STATE with BOTH lines OFF — this
    // deasserts RTS/DTR, so it cannot key the transmitter.
    println!("\nGate 5 — OUTPUT CONTROL via ep0 (no claim), lines OFF (safe, no TX)");
    match device
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: CDC_SET_CONTROL_LINE_STATE,
                value: 0x0000, // bit0=DTR, bit1=RTS — both off
                index: u16::from(iface_num),
                data: &[],
            },
            TIMEOUT,
        )
        .wait()
    {
        Ok(()) => pass(
            "ep0 output",
            "CDC SET_CONTROL_LINE_STATE accepted via ep0 without claiming — \
             output-only PTT may be feasible despite the driver holding the interface",
        ),
        Err(e) => fail(
            "ep0 output",
            &format!(
                "class request via ep0 rejected ({e}) — output control needs the interface claim"
            ),
        ),
    }

    drop(claimed);
    println!("\nSpike complete.");
}

/// Read the WCH modem-status register (req 0x95, reg pair 0x0706). Returns the
/// raw byte; active-low on the wire, so the caller inverts.
fn read_wch_status(device: &nusb::Device) -> Option<u8> {
    device
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: CH34X_REQ_READ_REG,
                value: 0x0706,
                index: 0x0000,
                length: 2,
            },
            TIMEOUT,
        )
        .wait()
        .ok()
        .and_then(|b| b.first().copied())
}

/// Decode a WCH status byte into a human string (signals are active-low).
fn decode_wch(raw: u8) -> String {
    let s = !raw;
    format!(
        "CTS={} DSR={} DCD={} RI={}",
        u8::from((s & CH34X_BIT_CTS) != 0),
        u8::from((s & CH34X_BIT_DSR) != 0),
        u8::from((s & CH34X_BIT_DCD) != 0),
        u8::from((s & CH34X_BIT_RI) != 0),
    )
}

/// Live PTT monitor: polls the WCH vendor status register and drains the CDC
/// interrupt-IN notification endpoint, printing on every change. Press/release
/// the handset PTT to see which channel carries the operator-key line.
fn run_monitor(device: &nusb::Device) {
    println!("\n=== MONITOR (Ctrl-C to stop) — press/release the handset PTT now ===");

    // The CDC comm interface (#0) owns the interrupt-IN notification endpoint.
    // Claim it so we can also watch standard CDC SERIAL_STATE notifications.
    const COMM_IFACE: u8 = 0;
    const INTERRUPT_IN_EP: u8 = 0x83;
    let mut ep = match device.claim_interface(COMM_IFACE).wait() {
        Ok(iface) => match iface.endpoint::<Interrupt, In>(INTERRUPT_IN_EP) {
            Ok(ep) => {
                println!("  watching: vendor reg 0x95 + CDC interrupt-IN {INTERRUPT_IN_EP:#04x}");
                Some(ep)
            }
            Err(e) => {
                println!("  (no interrupt endpoint {INTERRUPT_IN_EP:#04x}: {e}; vendor-reg only)");
                None
            }
        },
        Err(e) => {
            println!("  (could not claim comm interface #{COMM_IFACE}: {e}; vendor-reg only)");
            None
        }
    };

    // Print the initial vendor-register reading as a baseline.
    let mut last_raw = read_wch_status(device);
    match last_raw {
        Some(raw) => println!("  [vendor] baseline raw={raw:#04x} -> {}", decode_wch(raw)),
        None => println!("  [vendor] baseline read failed"),
    }

    loop {
        // 1) Poll the WCH vendor status register; report on change.
        let raw = read_wch_status(device);
        if raw != last_raw {
            match raw {
                Some(r) => println!("  [vendor] CHANGE raw={r:#04x} -> {}", decode_wch(r)),
                None => println!("  [vendor] read failed"),
            }
            last_raw = raw;
        }

        // 2) Drain any pending CDC interrupt-IN notification (short timeout so we
        //    keep polling the register). A SERIAL_STATE notification is 10 bytes:
        //    [0xA1,0x20, wValue, wIndex, wLen=2, state_lo, state_hi].
        if let Some(ep) = ep.as_mut() {
            let buf = ep.allocate(16);
            let completion = ep.transfer_blocking(buf, Duration::from_millis(50));
            if completion.status.is_ok() {
                let data = &completion.buffer[..];
                if !data.is_empty() {
                    let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
                    let decoded = if data.len() >= 10 && data[1] == 0x20 {
                        let state = data[8];
                        format!(
                            " (SERIAL_STATE DCD={} DSR={} break={} RI={})",
                            u8::from((state & 0x01) != 0),
                            u8::from((state & 0x02) != 0),
                            u8::from((state & 0x04) != 0),
                            u8::from((state & 0x08) != 0),
                        )
                    } else {
                        String::new()
                    };
                    println!("  [cdc-int] {}{decoded}", hex.join(" "));
                }
            }
        } else {
            // No interrupt endpoint: pace the register polling loop instead.
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
