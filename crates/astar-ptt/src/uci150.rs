// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `AllScan` UCI150 backend over its WCH CH343 USB-serial port. By default the
//! handset PTT arrives on CTS and RTS keys the radio, but the input/output lines
//! are selectable (other interfaces route COS/PTT on DCD etc.). On macOS the WCH
//! `CH34xVCPDriver` dext is REQUIRED; with it the port appears as
//! `/dev/cu.wchusbserial*`.

use std::io;
use std::time::Duration;

use serialport::SerialPort;

use crate::lines::{KeyLine, ModemPort, RadioLine};
use crate::{PttBackend, PttError};

impl ModemPort for Box<dyn SerialPort> {
    fn read_line(&mut self, line: KeyLine) -> io::Result<bool> {
        let r = match line {
            KeyLine::Cts => self.read_clear_to_send(),
            KeyLine::Dcd => self.read_carrier_detect(),
            KeyLine::Dsr => self.read_data_set_ready(),
            KeyLine::Ri => self.read_ring_indicator(),
        };
        r.map_err(|e| io::Error::other(e.to_string()))
    }
    fn write_line(&mut self, line: RadioLine, level: bool) -> io::Result<()> {
        let r = match line {
            RadioLine::Rts => self.write_request_to_send(level),
            RadioLine::Dtr => self.write_data_terminal_ready(level),
        };
        r.map_err(|e| io::Error::other(e.to_string()))
    }
}

/// Serial PTT lines: a selectable input line (operator key) and output line
/// (radio key). Default profile (UCI150) is CTS-in / RTS-out.
pub struct Uci150Serial {
    port: Box<dyn SerialPort>,
    key: KeyLine,
    radio: RadioLine,
}

/// WCH (Jiangsu Qinheng) USB vendor id — the CH343 used by the UCI150.
pub const WCH_VID: u16 = 0x1a86;

/// Lowest-named USB serial port whose vendor id is WCH. Pure; the OS query is
/// in [`Uci150Serial::autodetect`].
fn pick_wch_port(ports: &[serialport::SerialPortInfo]) -> Option<String> {
    let mut names: Vec<String> = ports
        .iter()
        .filter_map(|p| match &p.port_type {
            serialport::SerialPortType::UsbPort(info) if info.vid == WCH_VID => {
                Some(p.port_name.clone())
            }
            _ => None,
        })
        .collect();
    names.sort();
    names.into_iter().next()
}

impl Uci150Serial {
    /// First WCH USB serial port (the UCI150's CH343), if any. Cross-platform
    /// (macOS `/dev/cu.*`, Linux `/dev/ttyUSB*`, Windows `COM*`). The caller may
    /// prefer an explicit path from its own config.
    #[must_use]
    pub fn autodetect() -> Option<String> {
        pick_wch_port(&serialport::available_ports().unwrap_or_default())
    }

    /// Open with the default UCI150 line profile (CTS in, RTS out).
    ///
    /// # Errors
    /// [`PttError::Io`] when the port cannot be opened or the lines cleared.
    pub fn open(path: &str) -> Result<Self, PttError> {
        Self::open_with(path, KeyLine::Cts, RadioLine::Rts)
    }

    /// Open with explicit input/output lines. Clears the radio line + DTR
    /// immediately (opening a port asserts them by default, which would key the
    /// radio).
    ///
    /// # Errors
    /// [`PttError::Io`] when the port cannot be opened or the lines cleared.
    pub fn open_with(path: &str, key: KeyLine, radio: RadioLine) -> Result<Self, PttError> {
        let port = serialport::new(path, 9600)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|e| PttError::Io(io::Error::other(e.to_string())))?;
        let mut me = Self { port, key, radio };
        me.write_line(me.radio, false)?;
        me.port
            .write_data_terminal_ready(false)
            .map_err(|e| PttError::Io(io::Error::other(e.to_string())))?;
        Ok(me)
    }

    fn write_line(&mut self, line: RadioLine, level: bool) -> Result<(), PttError> {
        ModemPort::write_line(&mut self.port, line, level).map_err(PttError::Io)
    }
}

impl PttBackend for Uci150Serial {
    fn read_key(&mut self) -> Result<bool, PttError> {
        self.port.read_line(self.key).map_err(PttError::Io)
    }
    fn set_radio_key(&mut self, level: bool) -> Result<(), PttError> {
        self.write_line(self.radio, level)
    }
    fn fail_safe(&mut self) {
        let _ = ModemPort::write_line(&mut self.port, self.radio, false);
        let _ = self.port.write_data_terminal_ready(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};

    fn usb(name: &str, vid: u16) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid: 0,
                serial_number: None,
                manufacturer: None,
                product: None,
            }),
        }
    }

    #[test]
    fn picks_lowest_named_wch_usb_port_and_ignores_others() {
        let ports = vec![
            SerialPortInfo {
                port_name: "/dev/cu.Bluetooth".to_string(),
                port_type: SerialPortType::BluetoothPort,
            },
            usb("/dev/ttyUSB9", WCH_VID),
            usb("/dev/ttyUSB2", WCH_VID),
            usb("/dev/ttyUSB0", 0x2341), // not WCH
        ];
        assert_eq!(pick_wch_port(&ports).as_deref(), Some("/dev/ttyUSB2"));
    }

    #[test]
    fn no_wch_port_returns_none() {
        let ports = vec![usb("COM3", 0x2341)];
        assert_eq!(pick_wch_port(&ports), None);
    }
}
