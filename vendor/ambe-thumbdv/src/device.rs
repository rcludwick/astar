//! ThumbDV device driver for AMBE-3000 vocoder.
//!
//! Implements the initialization sequence, encoding, and decoding operations
//! as specified in the AMBE-3000R™ Users Manual and ambe3000-protocol.md.

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use crate::packet::{
    channel_in, dcmode_off, ecmode_off, gain_zero, init_encdec, prodid_query, ratep_dstar, reset,
    speech_in, verstring_query, Deframer, Response, FRAME_BYTES, FRAME_SAMPLES,
};
use crate::transport::Transport;

/// Error type for device operations.
#[derive(Debug)]
pub enum DeviceError {
    /// I/O error.
    Io(io::Error),
    /// Operation timed out waiting for a response.
    Timeout(&'static str),
    /// Device is not an AMBE-3000.
    WrongDevice(String),
    /// Protocol parsing error.
    Protocol(String),
    /// Device returned a non-zero status.
    Status {
        /// Field identifier.
        field: u8,
        /// Status value.
        status: u8,
    },
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::Io(e) => write!(f, "I/O error: {}", e),
            DeviceError::Timeout(msg) => write!(f, "Timeout: {}", msg),
            DeviceError::WrongDevice(msg) => write!(f, "Wrong device: {}", msg),
            DeviceError::Protocol(msg) => write!(f, "Protocol error: {}", msg),
            DeviceError::Status { field, status } => {
                write!(
                    f,
                    "Device status error: field 0x{:02X} = 0x{:02X}",
                    field, status
                )
            }
        }
    }
}

impl std::error::Error for DeviceError {}

impl From<io::Error> for DeviceError {
    fn from(e: io::Error) -> Self {
        DeviceError::Io(e)
    }
}

/// ThumbDV device driver for AMBE-3000 vocoder.
pub struct ThumbDv<T: Transport> {
    transport: T,
    prodid: String,
    version: String,
}

impl<T: Transport> fmt::Debug for ThumbDv<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThumbDv")
            .field("prodid", &self.prodid)
            .field("version", &self.version)
            .field("transport", &"<T: Transport>")
            .finish()
    }
}

impl<T: Transport> ThumbDv<T> {
    /// Initialize the device with the given transport.
    ///
    /// Follows the initialization cookbook (§8.1):
    /// 1. Drain initial bytes with a 50 ms timeout.
    /// 2. Reset and await Ready.
    /// 3. Query product ID (must start with "AMBE3000").
    /// 4. Query version string.
    /// 5. Set D-STAR rate parameters (RATEP).
    /// 6. Initialize encoder/decoder.
    /// 7. Disable encoder mode (EC).
    /// 8. Disable decoder mode (DC).
    /// 9. Set gain to zero.
    ///
    /// # Errors
    /// Returns `DeviceError` if initialization fails at any step.
    pub fn init_with(mut transport: T) -> Result<Self, DeviceError> {
        // Step 1: Drain initial bytes (50 ms timeout, discard result).
        let mut _buf = [0u8; 1024];
        let _ = transport.recv_some(&mut _buf, Duration::from_millis(50));

        // Step 2: Reset and await Ready (§8.1).
        let resp = Self::transact_internal(&mut transport, &reset())?;
        match resp {
            Response::Ready => {}
            _ => return Err(DeviceError::Protocol("expected Ready after reset".into())),
        }

        // Step 3: Query product ID.
        let resp = Self::transact_internal(&mut transport, &prodid_query())?;
        let prodid = match resp {
            Response::ProdId(s) => s,
            _ => return Err(DeviceError::Protocol("expected ProdId".into())),
        };

        if !prodid.starts_with("AMBE3000") {
            return Err(DeviceError::WrongDevice(format!(
                "expected AMBE3000*, got {}",
                prodid
            )));
        }

        // Step 4: Query version string.
        let resp = Self::transact_internal(&mut transport, &verstring_query())?;
        let version = match resp {
            Response::Version(s) => s,
            _ => return Err(DeviceError::Protocol("expected Version".into())),
        };

        // Step 5: Set D-STAR rate parameters.
        let resp = Self::transact_internal(&mut transport, &ratep_dstar())?;
        Self::check_status(&resp, 0x0A)?;

        // Step 6: Initialize encoder/decoder.
        let resp = Self::transact_internal(&mut transport, &init_encdec())?;
        Self::check_status(&resp, 0x0B)?;

        // Step 7: Disable encoder mode.
        let resp = Self::transact_internal(&mut transport, &ecmode_off())?;
        Self::check_status(&resp, 0x05)?;

        // Step 8: Disable decoder mode.
        let resp = Self::transact_internal(&mut transport, &dcmode_off())?;
        Self::check_status(&resp, 0x06)?;

        // Step 9: Set gain to zero.
        let resp = Self::transact_internal(&mut transport, &gain_zero())?;
        Self::check_status(&resp, 0x4B)?;

        Ok(ThumbDv {
            transport,
            prodid,
            version,
        })
    }

    /// Re-initialize the device (RATEP + INIT + mode words again, §8.1).
    ///
    /// # Errors
    /// Returns `DeviceError` if re-initialization fails.
    pub fn reinit(&mut self) -> Result<(), DeviceError> {
        // Steps 5-9 from init_with.
        let resp = Self::transact_internal(&mut self.transport, &ratep_dstar())?;
        Self::check_status(&resp, 0x0A)?;

        let resp = Self::transact_internal(&mut self.transport, &init_encdec())?;
        Self::check_status(&resp, 0x0B)?;

        let resp = Self::transact_internal(&mut self.transport, &ecmode_off())?;
        Self::check_status(&resp, 0x05)?;

        let resp = Self::transact_internal(&mut self.transport, &dcmode_off())?;
        Self::check_status(&resp, 0x06)?;

        let resp = Self::transact_internal(&mut self.transport, &gain_zero())?;
        Self::check_status(&resp, 0x4B)?;

        Ok(())
    }

    /// Encode PCM audio to AMBE frame.
    ///
    /// # Errors
    /// Returns `DeviceError` if encoding fails.
    pub fn encode_frame(
        &mut self,
        pcm: &[i16; FRAME_SAMPLES],
    ) -> Result<[u8; FRAME_BYTES], DeviceError> {
        let resp = Self::transact_internal(&mut self.transport, &speech_in(pcm))?;
        match resp {
            Response::Channel(frame) => Ok(frame),
            _ => Err(DeviceError::Protocol("expected Channel response".into())),
        }
    }

    /// Decode AMBE frame to PCM audio.
    ///
    /// # Errors
    /// Returns `DeviceError` if decoding fails.
    pub fn decode_frame(
        &mut self,
        frame: &[u8; FRAME_BYTES],
    ) -> Result<[i16; FRAME_SAMPLES], DeviceError> {
        let resp = Self::transact_internal(&mut self.transport, &channel_in(frame))?;
        match resp {
            Response::Speech(pcm) => Ok(pcm),
            _ => Err(DeviceError::Protocol("expected Speech response".into())),
        }
    }

    /// Get product ID string.
    pub fn prodid(&self) -> &str {
        &self.prodid
    }

    /// Get version string.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Get configuration (at reset).
    ///
    /// # Errors
    /// Returns `DeviceError` if the operation fails.
    pub fn getcfg(&mut self) -> Result<[u8; 3], DeviceError> {
        let resp = Self::transact_internal(&mut self.transport, &crate::packet::getcfg_query())?;
        match resp {
            Response::Config(cfg) => Ok(cfg),
            _ => Err(DeviceError::Protocol("expected Config response".into())),
        }
    }

    /// Read configuration (now).
    ///
    /// # Errors
    /// Returns `DeviceError` if the operation fails.
    pub fn readcfg(&mut self) -> Result<[u8; 3], DeviceError> {
        let resp = Self::transact_internal(&mut self.transport, &crate::packet::readcfg_query())?;
        match resp {
            Response::Config(cfg) => Ok(cfg),
            _ => Err(DeviceError::Protocol("expected Config response".into())),
        }
    }

    /// Send a request and wait for a response with a 300 ms deadline (§6 timeouts).
    fn transact_internal(transport: &mut T, req: &[u8]) -> Result<Response, DeviceError> {
        transport.send(req)?;

        let deadline = Instant::now() + Duration::from_millis(300);
        let mut deframer = Deframer::new();

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(DeviceError::Timeout("300 ms deadline expired"));
            }

            let mut buf = [0u8; 1024];
            match transport.recv_some(&mut buf, remaining) {
                Ok(0) => {
                    // Timeout or no more data.
                    continue;
                }
                Ok(n) => {
                    deframer.push(&buf[..n]);
                }
                Err(e) => return Err(DeviceError::Io(e)),
            }

            if let Some(pkt) = deframer.next_packet() {
                let resp = crate::packet::parse_response(&pkt)
                    .map_err(|e| DeviceError::Protocol(e.to_string()))?;
                return Ok(resp);
            }
        }
    }

    /// Check if a response is a Status with status ≠ 0, return an error if so.
    fn check_status(resp: &Response, expected_field: u8) -> Result<(), DeviceError> {
        match resp {
            Response::Status { field, status } => {
                if *status != 0 {
                    return Err(DeviceError::Status {
                        field: *field,
                        status: *status,
                    });
                }
                if *field != expected_field {
                    return Err(DeviceError::Protocol(format!(
                        "expected field 0x{:02X}, got 0x{:02X}",
                        expected_field, field
                    )));
                }
                Ok(())
            }
            _ => Err(DeviceError::Protocol("expected Status response".into())),
        }
    }
}

/// Detect and initialize a ThumbDV device on any available serial port.
///
/// Tries candidate serial ports at 460800 then 230400 baud (§6 fallback).
/// Returns the first successfully initialized device.
///
/// # Errors
/// - `DeviceError::Protocol("no ThumbDV-like serial ports present")` if no candidate ports exist.
/// - `DeviceError::Timeout("no ThumbDV found")` if candidates exist but none initialize.
pub fn detect() -> Result<ThumbDv<crate::transport::SerialTransport>, DeviceError> {
    use crate::transport::SerialTransport;

    let candidates = SerialTransport::candidate_ports();
    if candidates.is_empty() {
        return Err(DeviceError::Protocol(
            "no ThumbDV-like serial ports present".into(),
        ));
    }

    for port_path in candidates {
        for &baud in &[460800u32, 230400u32] {
            match SerialTransport::open(&port_path, baud) {
                Ok(transport) => match ThumbDv::init_with(transport) {
                    Ok(dev) => return Ok(dev),
                    Err(_) => continue, // Try next baud or port.
                },
                Err(_) => continue, // Try next baud or port.
            }
        }
    }

    Err(DeviceError::Timeout("no ThumbDV found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::hex;
    use crate::MockTransport;

    fn scripted_init() -> MockTransport {
        let mut m = MockTransport::new();
        m.expect(reset(), vec![hex("61 00 01 00 39")]);
        let mut prod = hex("61 00 0B 00 30");
        prod.extend_from_slice(b"AMBE3000R\0");
        m.expect(prodid_query(), vec![prod]);
        let mut ver = hex("61 00 07 00 31");
        ver.extend_from_slice(b"V120A\0");
        m.expect(verstring_query(), vec![ver]);
        m.expect(ratep_dstar(), vec![hex("61 00 02 00 0A 00")]);
        m.expect(init_encdec(), vec![hex("61 00 02 00 0B 00")]);
        m.expect(ecmode_off(), vec![hex("61 00 02 00 05 00")]);
        m.expect(dcmode_off(), vec![hex("61 00 02 00 06 00")]);
        m.expect(gain_zero(), vec![hex("61 00 02 00 4B 00")]);
        m
    }

    #[test]
    fn init_sequence_follows_the_cookbook() {
        let dev = ThumbDv::init_with(scripted_init()).unwrap();
        assert_eq!(dev.prodid(), "AMBE3000R");
        assert_eq!(dev.version(), "V120A");
    }

    #[test]
    fn wrong_product_id_is_rejected() {
        let mut m = MockTransport::new();
        m.expect(reset(), vec![hex("61 00 01 00 39")]);
        let mut prod = hex("61 00 0B 00 30");
        prod.extend_from_slice(b"AMBE3003\0\0"); // wrong chip
        m.expect(prodid_query(), vec![prod]);
        match ThumbDv::init_with(m) {
            Err(DeviceError::WrongDevice(s)) => assert!(s.contains("AMBE3003")),
            other => panic!("expected WrongDevice, got {other:?}"),
        }
    }

    #[test]
    fn encode_frame_round_trips_through_the_mock() {
        let mut m = scripted_init();
        let pcm = [100i16; FRAME_SAMPLES];
        let mut resp = hex("61 00 0B 01 01 48");
        resp.extend_from_slice(&[0x5A; 9]);
        // Response arrives split across two reads (deframer handles it).
        m.expect(
            speech_in(&pcm),
            vec![resp[..7].to_vec(), resp[7..].to_vec()],
        );
        let mut dev = ThumbDv::init_with(m).unwrap();
        assert_eq!(dev.encode_frame(&pcm).unwrap(), [0x5A; 9]);
    }

    #[test]
    fn decode_frame_returns_pcm() {
        let mut m = scripted_init();
        let frame = [0x11u8; FRAME_BYTES];
        let mut resp = hex("61 01 42 02 00 A0");
        for _ in 0..160 {
            resp.extend_from_slice(&500i16.to_be_bytes());
        }
        m.expect(channel_in(&frame), vec![resp]);
        let mut dev = ThumbDv::init_with(m).unwrap();
        assert_eq!(dev.decode_frame(&frame).unwrap(), [500i16; FRAME_SAMPLES]);
    }

    #[test]
    fn error_status_surfaces_as_status_error() {
        let mut m = MockTransport::new();
        m.expect(reset(), vec![hex("61 00 01 00 39")]);
        let mut prod = hex("61 00 0B 00 30");
        prod.extend_from_slice(b"AMBE3000R\0");
        m.expect(prodid_query(), vec![prod]);
        let mut ver = hex("61 00 07 00 31");
        ver.extend_from_slice(b"V120A\0");
        m.expect(verstring_query(), vec![ver]);
        // RATEP rejected by the chip (status 0x05).
        m.expect(ratep_dstar(), vec![hex("61 00 02 00 0A 05")]);
        match ThumbDv::init_with(m) {
            Err(DeviceError::Status {
                field: 0x0A,
                status: 5,
            }) => {}
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn timeout_is_reported_not_hung() {
        let mut m = MockTransport::new();
        m.expect(reset(), vec![]); // no READY ever arrives
        let t0 = std::time::Instant::now();
        match ThumbDv::init_with(m) {
            Err(DeviceError::Timeout(_)) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn detect_without_hardware_errors_not_hangs() {
        let t0 = std::time::Instant::now();
        match crate::detect() {
            Err(DeviceError::Protocol(_)) | Err(DeviceError::Timeout(_)) => {
                // Expected without hardware: either no ports or ports
                // but no device found.
            }
            // A real ThumbDV is attached and answered the probe —
            // valid on dev machines with the stick plugged in; the
            // test still bounds detection time below.
            Ok(_) => {}
            Err(e) => panic!("expected detect() to error or find a device, got {e:?}"),
        }
        // Ensure we didn't hang; detection should be fast.
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "detect() took longer than 5 seconds"
        );
    }
}
