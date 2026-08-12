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

        Ok(SerialTransport { port })
    }

    /// Get available serial ports that look like ThumbDV devices.
    ///
    /// Filters for USB devices with VID 0x0403 (FTDI) and PID 0x6015 (FT230X),
    /// or product string containing "ThumbDV" (§6 of ambe3000-protocol.md).
    /// On macOS, deduplicates the `/dev/cu.*` and `/dev/tty.*` twins and keeps
    /// only the `/dev/cu.*` form to avoid blocking on carrier detect (per plan brief).
    ///
    /// Returns an empty vector if no suitable ports are found.
    pub fn candidate_ports() -> Vec<String> {
        let ports = match serialport::available_ports() {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        let mut candidates = Vec::new();

        for port_info in ports {
            let port_name = port_info.port_name.clone();
            let is_thumbdv = match &port_info.port_type {
                serialport::SerialPortType::UsbPort(usb) => {
                    (usb.vid == 0x0403 && usb.pid == 0x6015)
                        || usb
                            .product
                            .as_ref()
                            .map(|p| p.contains("ThumbDV"))
                            .unwrap_or(false)
                }
                _ => false,
            };

            if is_thumbdv {
                candidates.push(port_name);
            }
        }

        // On macOS, deduplicate cu/tty twins.
        #[cfg(target_os = "macos")]
        {
            dedup_cu_tty_twins(candidates)
        }

        #[cfg(not(target_os = "macos"))]
        candidates
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

    #[test]
    fn candidate_ports_does_not_panic() {
        // Should return without panicking, even if no ports are present.
        let _ = SerialTransport::candidate_ports();
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
