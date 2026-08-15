// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Selectable serial modem lines: which input pin carries the operator key and
//! which output pin keys the radio. Isolated behind `ModemPort` so line
//! dispatch is testable without a full `serialport::SerialPort` fake.

use std::io;

/// Operator-key INPUT line read from the radio interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyLine {
    Cts,
    Dcd,
    Dsr,
    Ri,
}

/// Radio-key OUTPUT line asserted to key the transmitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioLine {
    Rts,
    Dtr,
}

/// The four readable status lines + two writable control lines of a UART.
pub trait ModemPort {
    /// Read the level of one input status line.
    fn read_line(&mut self, line: KeyLine) -> io::Result<bool>;
    /// Drive one output control line.
    fn write_line(&mut self, line: RadioLine, level: bool) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Each bool mirrors an inherent hardware modem control line (CTS/DCD/DSR/RI);
    // they are independent signal states, not a flag-set to fold into an enum.
    #[allow(clippy::struct_excessive_bools)]
    #[derive(Default)]
    struct FakePort {
        cts: bool,
        dcd: bool,
        dsr: bool,
        ri: bool,
        writes: HashMap<&'static str, bool>,
    }
    impl ModemPort for FakePort {
        fn read_line(&mut self, line: KeyLine) -> io::Result<bool> {
            Ok(match line {
                KeyLine::Cts => self.cts,
                KeyLine::Dcd => self.dcd,
                KeyLine::Dsr => self.dsr,
                KeyLine::Ri => self.ri,
            })
        }
        fn write_line(&mut self, line: RadioLine, level: bool) -> io::Result<()> {
            let k = match line {
                RadioLine::Rts => "rts",
                RadioLine::Dtr => "dtr",
            };
            self.writes.insert(k, level);
            Ok(())
        }
    }

    #[test]
    fn read_line_selects_the_named_status_line() {
        let mut p = FakePort {
            dcd: true,
            ..FakePort::default()
        };
        assert!(!p.read_line(KeyLine::Cts).unwrap());
        assert!(p.read_line(KeyLine::Dcd).unwrap());
    }

    #[test]
    fn write_line_drives_the_named_control_line() {
        let mut p = FakePort::default();
        p.write_line(RadioLine::Dtr, true).unwrap();
        assert_eq!(p.writes.get("dtr"), Some(&true));
        assert_eq!(p.writes.get("rts"), None);
    }
}
