//! Transport abstraction for DVSI packet I/O.
//!
//! Provides the `Transport` trait for sending and receiving bytes,
//! and a `MockTransport` implementation for testing.

use std::io;
use std::time::Duration;

/// Trait for sending and receiving raw bytes over a transport medium.
pub trait Transport: Send {
    /// Send bytes to the device.
    ///
    /// # Errors
    /// Returns an `io::Error` if the send fails.
    fn send(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Receive some bytes with a timeout.
    ///
    /// Blocks up to `timeout` waiting for data. Returns the number of bytes read.
    /// Returns 0 if the timeout expires without receiving any data.
    ///
    /// # Errors
    /// Returns an `io::Error` if a receive error occurs.
    fn recv_some(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;

    /// Drive DTR and RTS low.
    ///
    /// Opening a tty on macOS (and on most POSIX systems) raises DTR and RTS.
    /// On a USB radio interface or CAT/PTT cable either line can key a
    /// transmitter, so every opener calls this straight after open and before
    /// the first byte is written. [`SerialTransport`] does it inside
    /// [`SerialTransport::open`] as well. Transports without modem-control
    /// lines (mocks, sockets) keep the default no-op.
    ///
    /// # Errors
    /// Returns an `io::Error` if the lines could not be set; callers treat
    /// that as a failed open.
    fn deassert_control_lines(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Mock transport for testing: scripted request/response pairs.
///
/// Use `expect()` to queue expected requests and their corresponding response chunks.
/// `send()` asserts the request matches the next expectation (panics with a hex diff on mismatch).
/// `recv_some()` drains queued response chunks.
pub struct MockTransport {
    expectations: Vec<(Vec<u8>, Vec<Vec<u8>>)>,
    current_expectation: usize,
    current_chunk: usize,
}

impl MockTransport {
    /// Create a new mock transport with no expectations.
    pub fn new() -> Self {
        MockTransport {
            expectations: Vec::new(),
            current_expectation: 0,
            current_chunk: 0,
        }
    }

    /// Queue an expected request and the response chunks to feed back.
    pub fn expect(&mut self, request: Vec<u8>, responses: Vec<Vec<u8>>) {
        self.expectations.push((request, responses));
    }

    /// Check if all expectations have been consumed.
    pub fn done(&self) -> bool {
        self.current_expectation >= self.expectations.len()
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for MockTransport {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.current_expectation >= self.expectations.len() {
            panic!("MockTransport: unexpected send() after all expectations consumed");
        }

        let (expected, _responses) = &self.expectations[self.current_expectation];

        if bytes != expected.as_slice() {
            // Find the first differing byte offset.
            let mut first_diff = bytes.len().min(expected.len());
            for (i, (e, g)) in expected.iter().zip(bytes.iter()).enumerate() {
                if e != g {
                    first_diff = i;
                    break;
                }
            }
            // Handle length mismatch.
            if bytes.len() != expected.len() && first_diff == bytes.len().min(expected.len()) {
                first_diff = bytes.len().min(expected.len());
            }

            // Format hex diff for debugging.
            let got_hex = hex_encode(bytes);
            let exp_hex = hex_encode(expected);
            panic!(
                "MockTransport: send mismatch at byte offset {}\n  expected: {}\n  got:      {}",
                first_diff, exp_hex, got_hex
            );
        }

        // Successfully matched; reset chunk index to start reading responses,
        // and increment current_expectation to point to the next send.
        self.current_chunk = 0;
        // Note: current_expectation - 1 will be used for recv_some.
        // We increment it now so the next send knows which expectation to check.
        self.current_expectation += 1;

        Ok(())
    }

    fn recv_some(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
        // current_expectation points to the next send; we're reading from the previous one.
        if self.current_expectation == 0 {
            // No send has been done yet.
            return Ok(0);
        }

        let prev_expectation = self.current_expectation - 1;

        let (_request, responses) = &self.expectations[prev_expectation];

        // If we've drained all chunks for this expectation, return 0.
        if self.current_chunk >= responses.len() {
            return Ok(0);
        }

        let chunk = &responses[self.current_chunk];

        // Assert chunk fits in the buffer.
        if chunk.len() > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MockTransport: response chunk does not fit in buffer",
            ));
        }

        buf[..chunk.len()].copy_from_slice(chunk);
        self.current_chunk += 1;

        Ok(chunk.len())
    }
}

/// Encode bytes as hex string for display.
fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Serial transport over a USB or UART connection.
///
/// Opens a serial port at a specified baud rate and implements the Transport trait.
/// Per-call timeouts are set via `set_timeout` on the underlying serial port.
/// Timeout errors are mapped to `Ok(0)` per §6 of the protocol documentation.
pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialTransport {
    /// Open a serial port at the specified baud rate (8 data bits, no parity, 1 stop bit).
    ///
    /// Per §6 of ambe3000-protocol.md: 8 data bits, no parity, 1 stop bit.
    ///
    /// # Errors
    /// Returns an `io::Error` if the port cannot be opened or configured.
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        let port = serialport::new(path, baud)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .timeout(std::time::Duration::from_millis(100))
            .open()
            .map_err(map_serialport_error)?;

        let mut transport = SerialTransport { port };
        // The OS raised DTR/RTS as part of open(). Drop them before anything
        // else happens; a port whose lines cannot be driven is not used.
        transport.deassert_control_lines()?;
        Ok(transport)
    }

    /// Every FT-X-class port present, by USB serial number and path, for
    /// the enrolment picker. **Enumeration only: nothing is opened.**
    ///
    /// Ports without a USB serial number are left out, because they can
    /// never be trusted (see [`trusted_ports_from`]).
    pub fn list_candidates() -> Vec<VocoderCandidate> {
        candidates_from(&enumerate())
    }

    /// The port paths of attached FT-X-class devices whose USB serial number
    /// is in `trusted`. **Enumeration only: nothing is opened.**
    ///
    /// This is the only list any opener outside enrolment may walk. An empty
    /// `trusted` yields an empty list.
    pub fn candidate_ports_trusted(trusted: &[String]) -> Vec<String> {
        if trusted.iter().all(|t| t.trim().is_empty()) {
            return Vec::new();
        }
        trusted_ports_from(&enumerate(), trusted)
    }
}

/// FTDI's USB vendor id.
pub const FTDI_VID: u16 = 0x0403;
/// The FT-X family (FT230X / FT231X) product id: DVsticks, ThumbDVs, and a
/// great many CAT/PTT cables. USB identity alone cannot tell them apart.
pub const FT_X_PID: u16 = 0x6015;

/// One USB serial port as enumeration reports it. A plain mirror of the
/// fields of `serialport::UsbPortInfo` this crate reads, so the trust policy
/// is testable with synthetic listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbListing {
    /// The OS path (`/dev/cu.usbserial-…`, `COM7`, …).
    pub port_name: String,
    /// USB vendor id.
    pub vid: u16,
    /// USB product id.
    pub pid: u16,
    /// USB serial number, when the device reports one.
    pub serial_number: Option<String>,
    /// USB product string, when the device reports one.
    pub product: Option<String>,
}

/// An FT-X-class port offered for enrolment. Never opened by listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VocoderCandidate {
    /// USB serial number: the identity trust is recorded against.
    pub serial: String,
    /// The OS port path it enumerates as right now.
    pub port: String,
    /// USB product string, for display.
    pub product: Option<String>,
}

fn enumerate() -> Vec<UsbListing> {
    let Ok(ports) = serialport::available_ports() else {
        return Vec::new();
    };
    ports
        .into_iter()
        .filter_map(|info| match info.port_type {
            serialport::SerialPortType::UsbPort(usb) => Some(UsbListing {
                port_name: info.port_name,
                vid: usb.vid,
                pid: usb.pid,
                serial_number: usb.serial_number,
                product: usb.product,
            }),
            _ => None,
        })
        .collect()
}

/// FT-X VID/PID, or a product string naming a ThumbDV.
fn is_vocoder_class(l: &UsbListing) -> bool {
    (l.vid == FTDI_VID && l.pid == FT_X_PID)
        || l.product.as_deref().is_some_and(|p| p.contains("ThumbDV"))
}

fn usable_serial(l: &UsbListing) -> Option<&str> {
    l.serial_number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The trust policy: paths of vocoder-class ports whose serial number is in
/// `trusted` (exact match after trimming). Pure; the seam for tests.
///
/// A port with no serial number never matches, and neither does a trusted
/// serial on a device of another USB class.
pub fn trusted_ports_from(ports: &[UsbListing], trusted: &[String]) -> Vec<String> {
    let paths: Vec<String> = ports
        .iter()
        .filter(|l| is_vocoder_class(l))
        .filter(|l| usable_serial(l).is_some_and(|s| trusted.iter().any(|t| t.trim() == s)))
        .map(|l| l.port_name.clone())
        .collect();
    dedup_paths(paths)
}

/// Every vocoder-class port with a serial number, one entry per port after
/// cu/tty de-duplication. Pure; the seam for tests.
pub fn candidates_from(ports: &[UsbListing]) -> Vec<VocoderCandidate> {
    let keep = dedup_paths(ports.iter().map(|l| l.port_name.clone()).collect());
    ports
        .iter()
        .filter(|l| is_vocoder_class(l) && keep.contains(&l.port_name))
        .filter_map(|l| {
            usable_serial(l).map(|s| VocoderCandidate {
                serial: s.to_string(),
                port: l.port_name.clone(),
                product: l.product.clone(),
            })
        })
        .collect()
}

fn dedup_paths(paths: Vec<String>) -> Vec<String> {
    // On macOS, deduplicate cu/tty twins.
    #[cfg(target_os = "macos")]
    {
        dedup_cu_tty_twins(paths)
    }
    #[cfg(not(target_os = "macos"))]
    {
        paths
    }
}

/// Map serialport::Error to io::Error, preserving ErrorKind where applicable.
///
/// Maps `serialport::ErrorKind::Io(k)` to `io::Error::new(k, msg)` to preserve
/// the underlying io::ErrorKind. Other error kinds are mapped to `io::Error::other`.
fn map_serialport_error(e: serialport::Error) -> io::Error {
    match e.kind() {
        serialport::ErrorKind::Io(io_kind) => io::Error::new(io_kind, e.to_string()),
        _ => io::Error::other(e.to_string()),
    }
}

/// Deduplicate `/dev/cu.*` and `/dev/tty.*` twins, keeping only `/dev/cu.*`.
///
/// On macOS, `serialport::available_ports()` returns both `/dev/cu.usbserial-X`
/// (callout device, no carrier-detect blocking) and `/dev/tty.usbserial-X` (dialin device,
/// blocks on carrier detect) for each physical device. This function removes the tty.* form
/// whenever a cu.* counterpart exists.
#[cfg(target_os = "macos")]
fn dedup_cu_tty_twins(ports: Vec<String>) -> Vec<String> {
    // Collect all cu.* suffixes (everything after "/dev/cu.").
    let cu_suffixes: std::collections::HashSet<String> = ports
        .iter()
        .filter_map(|port| port.strip_prefix("/dev/cu.").map(|s| s.to_string()))
        .collect();

    // Filter out tty.* ports that have a cu.* counterpart.
    ports
        .into_iter()
        .filter(|port| {
            if port.starts_with("/dev/tty.") {
                // Drop tty.* if we have a cu.* with the same suffix.
                if let Some(suffix) = port.strip_prefix("/dev/tty.") {
                    !cu_suffixes.contains(suffix)
                } else {
                    true
                }
            } else {
                // Keep all cu.* and non-standard paths.
                true
            }
        })
        .collect()
}

impl Transport for SerialTransport {
    fn deassert_control_lines(&mut self) -> io::Result<()> {
        self.port
            .write_data_terminal_ready(false)
            .map_err(map_serialport_error)?;
        self.port
            .write_request_to_send(false)
            .map_err(map_serialport_error)
    }

    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        self.port.write_all(bytes)?;
        Ok(())
    }

    fn recv_some(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.port
            .set_timeout(timeout)
            .map_err(map_serialport_error)?;

        use std::io::Read;
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                // Map timeout error to Ok(0) per §6 of ambe3000-protocol.md.
                if e.kind() == io::ErrorKind::TimedOut {
                    Ok(0)
                } else {
                    Err(e)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_transport_queues_and_drains() {
        let mut m = MockTransport::new();
        m.expect(vec![0x01, 0x02], vec![vec![0x03, 0x04]]);

        m.send(&[0x01, 0x02]).unwrap();

        let mut buf = [0u8; 10];
        let n = m.recv_some(&mut buf, Duration::from_millis(100)).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], &[0x03, 0x04]);

        assert!(m.done());
    }

    #[test]
    #[should_panic]
    fn mock_transport_panics_on_mismatch() {
        let mut m = MockTransport::new();
        m.expect(vec![0x01, 0x02], vec![]);
        m.send(&[0x99, 0x99]).unwrap();
    }

    #[test]
    fn serial_transport_open_nonexistent_path_errors() {
        let result = SerialTransport::open("/dev/nonexistent-port-xyz", 460800);
        assert!(
            result.is_err(),
            "Expected open() to fail on nonexistent port"
        );
    }

    // ---- Trust filter (vocoder trust, on-air safety) ----------------------
    //
    // Every listing below is synthetic. Nothing here enumerates, let alone
    // opens, a real serial port.

    fn ftx(port: &str, serial: Option<&str>, product: &str) -> UsbListing {
        UsbListing {
            port_name: port.to_string(),
            vid: FTDI_VID,
            pid: FT_X_PID,
            serial_number: serial.map(str::to_string),
            product: Some(product.to_string()),
        }
    }

    fn trust(serials: &[&str]) -> Vec<String> {
        serials.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn untrusted_cat_cable_is_never_a_candidate_even_when_alone() {
        // An FT231X CAT/PTT cable: same VID/PID as a DVstick. It is the only
        // FT-X device present, and it is still not a candidate.
        let ports = [ftx(
            "/dev/cu.usbserial-CATCABLE",
            Some("CATCABLE"),
            "FT231X USB UART",
        )];
        assert!(trusted_ports_from(&ports, &trust(&["TESTSER1"])).is_empty());
    }

    #[test]
    fn empty_trust_list_yields_no_candidates() {
        let ports = [
            ftx(
                "/dev/cu.usbserial-TESTSER1",
                Some("TESTSER1"),
                "FT230X Basic UART",
            ),
            ftx(
                "/dev/cu.usbserial-TESTSER2",
                Some("TESTSER2"),
                "FT230X Basic UART",
            ),
        ];
        assert!(trusted_ports_from(&ports, &[]).is_empty());
    }

    #[test]
    fn only_trusted_serials_are_candidates() {
        let ports = [
            ftx(
                "/dev/cu.usbserial-TESTSER1",
                Some("TESTSER1"),
                "FT230X Basic UART",
            ),
            ftx(
                "/dev/cu.usbserial-CATCABLE",
                Some("CATCABLE"),
                "FT231X USB UART",
            ),
            ftx(
                "/dev/cu.usbserial-TESTSER2",
                Some("TESTSER2"),
                "FT230X Basic UART",
            ),
        ];
        assert_eq!(
            trusted_ports_from(&ports, &trust(&["TESTSER2", "TESTSER1"])),
            vec![
                "/dev/cu.usbserial-TESTSER1".to_string(),
                "/dev/cu.usbserial-TESTSER2".to_string()
            ]
        );
    }

    #[test]
    fn a_trusted_serial_on_a_non_ftx_device_is_refused() {
        // The serial matches, the USB identity does not: a WCH radio
        // interface that happens to report the same serial string.
        let ports = [UsbListing {
            port_name: "/dev/cu.wchusbserialTESTSER1".to_string(),
            vid: 0x1a86,
            pid: 0x55d3,
            serial_number: Some("TESTSER1".to_string()),
            product: Some("USB Single Serial".to_string()),
        }];
        assert!(trusted_ports_from(&ports, &trust(&["TESTSER1"])).is_empty());
    }

    #[test]
    fn a_port_with_no_serial_number_is_never_trusted() {
        let ports = [ftx("/dev/cu.usbserial-NOSERIAL", None, "FT230X Basic UART")];
        assert!(trusted_ports_from(&ports, &trust(&[""])).is_empty());
        assert!(candidates_from(&ports).is_empty());
    }

    #[test]
    fn candidates_list_every_ftx_port_by_serial_and_path() {
        let ports = [
            ftx(
                "/dev/cu.usbserial-TESTSER1",
                Some("TESTSER1"),
                "FT230X Basic UART",
            ),
            UsbListing {
                port_name: "/dev/cu.wchusbserialRADIO".to_string(),
                vid: 0x1a86,
                pid: 0x55d3,
                serial_number: Some("RADIO".to_string()),
                product: None,
            },
            ftx(
                "/dev/cu.usbserial-CATCABLE",
                Some("CATCABLE"),
                "FT231X USB UART",
            ),
        ];
        let got = candidates_from(&ports);
        assert_eq!(
            got,
            vec![
                VocoderCandidate {
                    serial: "TESTSER1".into(),
                    port: "/dev/cu.usbserial-TESTSER1".into(),
                    product: Some("FT230X Basic UART".into()),
                },
                VocoderCandidate {
                    serial: "CATCABLE".into(),
                    port: "/dev/cu.usbserial-CATCABLE".into(),
                    product: Some("FT231X USB UART".into()),
                },
            ],
            "every FT-X port is listed for enrolment, the WCH radio interface is not"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn cu_tty_twins_collapse_to_one_cu_port() {
        let ports = [
            ftx(
                "/dev/tty.usbserial-TESTSER1",
                Some("TESTSER1"),
                "FT230X Basic UART",
            ),
            ftx(
                "/dev/cu.usbserial-TESTSER1",
                Some("TESTSER1"),
                "FT230X Basic UART",
            ),
        ];
        assert_eq!(
            trusted_ports_from(&ports, &trust(&["TESTSER1"])),
            vec!["/dev/cu.usbserial-TESTSER1".to_string()]
        );
        let c = candidates_from(&ports);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].port, "/dev/cu.usbserial-TESTSER1");
    }

    #[test]
    fn mock_transport_deassert_is_a_no_op_by_default() {
        let mut m = MockTransport::new();
        m.deassert_control_lines().unwrap();
        assert!(m.done());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn dedup_cu_tty_twins_removes_duplicates() {
        let ports = vec![
            "/dev/cu.usbserial-A".to_string(),
            "/dev/tty.usbserial-A".to_string(),
            "/dev/cu.usbserial-B".to_string(),
            "/dev/tty.usbserial-C".to_string(), // No cu.* twin; keep it
            "/dev/other".to_string(),
        ];

        let result = dedup_cu_tty_twins(ports);

        // Should keep cu.usbserial-A, cu.usbserial-B, tty.usbserial-C, /dev/other.
        // Should remove tty.usbserial-A (has cu.* twin).
        assert_eq!(result.len(), 4);
        assert!(result.contains(&"/dev/cu.usbserial-A".to_string()));
        assert!(result.contains(&"/dev/cu.usbserial-B".to_string()));
        assert!(result.contains(&"/dev/tty.usbserial-C".to_string()));
        assert!(result.contains(&"/dev/other".to_string()));
        assert!(!result.contains(&"/dev/tty.usbserial-A".to_string()));
    }
}
