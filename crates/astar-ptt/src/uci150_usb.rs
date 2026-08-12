// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Raw-USB UCI150 PTT backend (iax-d937): drive the WCH CH343's modem-control
//! lines over IOKit/nusb instead of the `/dev/cu.*` tty, so no `CH34x` dext is
//! required and the build stays sandbox/MAS- and iOS-eligible. Proven by the
//! iax-ceba spike.
//!
//! Only the TRANSPORT differs from [`crate::Uci150Serial`]; debounce, polarity,
//! and edge logic still live in [`crate::PttBridge`]. This module supplies a
//! [`ModemPort`] over the USB control pipe:
//!   - read a status line via the WCH vendor status register (req `0x95`),
//!   - drive RTS/DTR via the CDC `SET_CONTROL_LINE_STATE` request (req `0x22`).
//!
//! Control transfers ride the device's default control pipe and coexist with
//! macOS's generic CDC-ACM driver; only the WCH *dext* must be absent.

use std::io;
use std::time::Duration;

use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};
use nusb::{Device, MaybeFuture};

use crate::lines::{KeyLine, ModemPort, RadioLine};
use crate::{PttBackend, PttError};

/// WCH (Jiangsu Qinheng) USB vendor id — the CH343 in the UCI150.
pub const WCH_VID: u16 = 0x1a86;

/// USB base class 0x09 = hub. The UCI150 puts its CH343 behind a WCH hub; skip
/// the hub when picking the serial device.
const USB_CLASS_HUB: u8 = 0x09;
/// USB base class 0x02 = CDC communication. `SET_CONTROL_LINE_STATE` is
/// addressed to this interface.
const USB_CLASS_CDC_COMM: u8 = 0x02;

/// WCH vendor request: read the modem-status register pair (0x06,0x07).
const CH34X_REQ_READ_REG: u8 = 0x95;
const CH34X_STATUS_REG: u16 = 0x0706;
/// CDC-ACM class request that drives the RTS/DTR output lines.
const CDC_SET_CONTROL_LINE_STATE: u8 = 0x22;

const TIMEOUT: Duration = Duration::from_millis(200);

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Decode a CH343 modem-status byte (req `0x95`) into one line's level.
///
/// Signals are active-low on the wire: a line is asserted when its bit is 0.
///
/// Bit map (**CH343 0x95 register**): `CTS=bit3, DSR=bit2, RI=bit1, DCD=bit0`.
/// This is the CH341 nibble **reversed**: the CH343's legacy 0x95 status register
/// returns the status nibble bit-reversed versus the interrupt-IN notification
/// path that WCH's own driver (and the macOS CDC-ACM driver) uses. CTS=bit3 and
/// DCD=bit0 were measured on a real UCI150 (2026-06-23); DSR=bit2 and RI=bit1 are
/// inferred from the full-nibble reversal and remain unverified. Only this
/// raw-USB backend reads 0x95 — the tty backend reads the canonical order via the
/// kernel driver, so it stays unchanged. See
/// `docs/research/ch34x-usb-ptt-chip-mapping.md`.
fn decode_status(raw: u8, line: KeyLine) -> bool {
    let bit = match line {
        KeyLine::Cts => 1 << 3,
        KeyLine::Dsr => 1 << 2,
        KeyLine::Ri => 1 << 1,
        KeyLine::Dcd => 1 << 0,
    };
    // Active-low: the line is asserted when its bit is 0 in the raw byte.
    (!raw) & bit != 0
}

/// CDC `SET_CONTROL_LINE_STATE` wValue bitmap: bit0=DTR, bit1=RTS.
fn control_line_bitmap(dtr: bool, rts: bool) -> u16 {
    u16::from(dtr) | (u16::from(rts) << 1)
}

/// A WCH `CH34x` modem-control port over the raw-USB control pipe. Reads status
/// lines via the vendor register and drives RTS/DTR via the CDC request. The
/// CDC request sets BOTH output lines at once, so we remember their levels.
struct Ch34xUsbPort {
    device: Device,
    /// Interface index for `SET_CONTROL_LINE_STATE` (the CDC comm interface).
    comm_iface: u16,
    rts: bool,
    dtr: bool,
}

impl Ch34xUsbPort {
    /// Push the remembered RTS/DTR levels to the device in one CDC request.
    fn send_control_lines(&self) -> io::Result<()> {
        self.device
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: CDC_SET_CONTROL_LINE_STATE,
                    value: control_line_bitmap(self.dtr, self.rts),
                    index: self.comm_iface,
                    data: &[],
                },
                TIMEOUT,
            )
            .wait()
            .map_err(io_err)
    }
}

impl ModemPort for Ch34xUsbPort {
    fn read_line(&mut self, line: KeyLine) -> io::Result<bool> {
        let buf = self
            .device
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: CH34X_REQ_READ_REG,
                    value: CH34X_STATUS_REG,
                    index: 0x0000,
                    length: 2,
                },
                TIMEOUT,
            )
            .wait()
            .map_err(io_err)?;
        let raw = *buf
            .first()
            .ok_or_else(|| io::Error::other("empty modem-status read"))?;
        Ok(decode_status(raw, line))
    }

    fn write_line(&mut self, line: RadioLine, level: bool) -> io::Result<()> {
        match line {
            RadioLine::Rts => self.rts = level,
            RadioLine::Dtr => self.dtr = level,
        }
        self.send_control_lines()
    }
}

/// `AllScan` UCI150 PTT over raw USB (no tty, no `CH34x` dext). Mirror of
/// [`crate::Uci150Serial`] but on the IOKit/nusb transport. Default profile is
/// **CTS-in / RTS-out**: on the tested UCI150 `MicPTT Dest` switch position the
/// handset PTT asserts CTS (idle status `0xff`, keyed `0xf7` = bit3 low, which is
/// CTS on the CH343 0x95 register); the input/output lines stay selectable.
pub struct Uci150Usb {
    port: Ch34xUsbPort,
    key: KeyLine,
    radio: RadioLine,
}

/// Enumerate USB and open the UCI150's WCH CH343, returning the opened device
/// and its CDC comm-interface number. Requires that no WCH *dext* holds the
/// device (macOS's generic CDC-ACM driver is fine — it only claims iface #0).
fn open_wch() -> Result<(Device, u8), PttError> {
    let devices = nusb::list_devices()
        .wait()
        .map_err(|e| PttError::Io(io_err(e)))?;
    let info = devices
        .filter(|d| d.vendor_id() == WCH_VID)
        .find(|d| d.interfaces().all(|i| i.class() != USB_CLASS_HUB))
        .ok_or(PttError::Unsupported(
            "no raw-USB WCH serial device — is the UCI150 plugged in and the WCH dext removed?",
        ))?;
    let comm = info
        .interfaces()
        .find(|i| i.class() == USB_CLASS_CDC_COMM)
        .map_or(0, nusb::InterfaceInfo::interface_number);
    let device = info.open().wait().map_err(|e| PttError::Io(io_err(e)))?;
    Ok((device, comm))
}

impl Uci150Usb {
    /// Open the UCI150 with the default line profile (CTS in, RTS out).
    ///
    /// # Errors
    /// [`PttError::Unsupported`] if no raw-USB WCH device is present (e.g. the
    /// dext still holds it); [`PttError::Io`] on a USB transport failure.
    pub fn open() -> Result<Self, PttError> {
        Self::open_with(KeyLine::Cts, RadioLine::Rts)
    }

    /// Open with explicit input/output lines. Drives both output lines low
    /// immediately so the radio cannot come up keyed.
    ///
    /// # Errors
    /// [`PttError::Unsupported`] / [`PttError::Io`] as in [`Self::open`].
    pub fn open_with(key: KeyLine, radio: RadioLine) -> Result<Self, PttError> {
        let (device, comm) = open_wch()?;
        let me = Self {
            port: Ch34xUsbPort {
                device,
                comm_iface: u16::from(comm),
                rts: false,
                dtr: false,
            },
            key,
            radio,
        };
        me.port.send_control_lines().map_err(PttError::Io)?;
        Ok(me)
    }
}

impl PttBackend for Uci150Usb {
    fn read_key(&mut self) -> Result<bool, PttError> {
        self.port.read_line(self.key).map_err(PttError::Io)
    }
    fn set_radio_key(&mut self, level: bool) -> Result<(), PttError> {
        self.port
            .write_line(self.radio, level)
            .map_err(PttError::Io)
    }
    fn fail_safe(&mut self) {
        // Drop both output lines (radio + DTR) in one safe request.
        self.port.rts = false;
        self.port.dtr = false;
        let _ = self.port.send_control_lines();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_status_byte_reads_all_lines_deasserted() {
        // Spike: idle raw=0xff -> nothing asserted.
        assert!(!decode_status(0xff, KeyLine::Dcd));
        assert!(!decode_status(0xff, KeyLine::Cts));
        assert!(!decode_status(0xff, KeyLine::Dsr));
        assert!(!decode_status(0xff, KeyLine::Ri));
    }

    #[test]
    fn keyed_uci150_cts_position_asserts_cts() {
        // Measured on a real UCI150 CH343 over the 0x95 register (2026-06-23):
        // handset PTT in the "CTS" switch position -> raw=0xf7 (bit3 low). On the
        // CH343's 0x95 register bit3 IS the CTS line (nibble reversed vs CH341).
        assert!(decode_status(0xf7, KeyLine::Cts), "CTS asserted when keyed");
        assert!(
            !decode_status(0xf7, KeyLine::Dcd),
            "DCD not the keyed line here"
        );
        assert!(!decode_status(0xf7, KeyLine::Dsr));
        assert!(!decode_status(0xf7, KeyLine::Ri));
    }

    #[test]
    fn keyed_uci150_dcd_position_asserts_dcd() {
        // Measured: handset PTT in the "DCD" switch position -> raw=0xfe (bit0 low).
        // On the CH343's 0x95 register bit0 IS the DCD line.
        assert!(decode_status(0xfe, KeyLine::Dcd), "DCD asserted when keyed");
        assert!(
            !decode_status(0xfe, KeyLine::Cts),
            "CTS not the keyed line here"
        );
    }

    #[test]
    fn each_status_bit_maps_to_its_ch343_line() {
        // CH343 0x95 register nibble is bit-reversed vs CH341/the notification path.
        // CTS=bit3 and DCD=bit0 are measured; DSR=bit2 and RI=bit1 are inferred
        // from the full-nibble reversal and remain unverified on hardware.
        assert!(decode_status(0xf7, KeyLine::Cts), "bit3 low = CTS"); // measured
        assert!(decode_status(0xfb, KeyLine::Dsr), "bit2 low = DSR"); // inferred
        assert!(decode_status(0xfd, KeyLine::Ri), "bit1 low = RI"); // inferred
        assert!(decode_status(0xfe, KeyLine::Dcd), "bit0 low = DCD"); // measured
    }

    #[test]
    fn control_line_bitmap_packs_dtr_bit0_rts_bit1() {
        assert_eq!(control_line_bitmap(false, false), 0x0000, "both off");
        assert_eq!(control_line_bitmap(true, false), 0x0001, "DTR only");
        assert_eq!(control_line_bitmap(false, true), 0x0002, "RTS only");
        assert_eq!(control_line_bitmap(true, true), 0x0003, "both on");
    }
}
