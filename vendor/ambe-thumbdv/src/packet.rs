//! DVSI packet protocol implementation.
//!
//! Implements the DVSI packet codec for the AMBE-3000 vocoder as specified
//! in the AMBE-3000R™ Users Manual and `docs/research/ambe3000-protocol.md`.

use std::fmt;

/// Number of bytes in a compressed AMBE frame.
pub const FRAME_BYTES: usize = 9;

/// Number of PCM samples per frame (20 ms at 8 kHz).
pub const FRAME_SAMPLES: usize = 160;

/// A raw DVSI packet (header parsed, payload intact).
#[derive(Debug, Clone, PartialEq)]
pub struct RawPacket {
    /// Packet type: 0 = control, 1 = channel, 2 = speech.
    pub ptype: u8,
    /// Raw payload bytes (after the 4-byte header).
    pub payload: Vec<u8>,
}

/// Protocol response from the AMBE-3000 chip.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Response {
    /// Chip is ready (after reset).
    Ready,
    /// Status response: generic field + status byte.
    Status {
        /// Field identifier.
        field: u8,
        /// Status byte value.
        status: u8,
    },
    /// Product ID string (NUL-terminated, lossy UTF-8).
    ProdId(String),
    /// Version string (NUL-terminated, lossy UTF-8).
    Version(String),
    /// Configuration: 3 bytes from GET/READCFG.
    Config([u8; 3]),
    /// Channel frame: compressed audio, and the number of BITS the device
    /// said it contains.
    ///
    /// The bit count is carried rather than assumed because it is a property
    /// of the rate the chip was configured for, and this parser does not know
    /// what that was. D-Star and DMR run at 72 bits (nine bytes); AMBE+2
    /// half-rate — YSF's DN modes, NXDN — runs at 49 (seven). Both are
    /// legitimate, and which one is CORRECT is a question only the caller who
    /// set the rate can answer.
    ///
    /// `data` holds `bits.div_ceil(8)` meaningful bytes; the remainder up to
    /// [`FRAME_BYTES`] is zero.
    Channel {
        /// Bits of compressed audio the device declared, 1..=72.
        bits: u8,
        /// The frame, low bytes first, zero-padded to [`FRAME_BYTES`].
        data: [u8; FRAME_BYTES],
    },
    /// Speech frame (160 samples of PCM, big-endian i16).
    Speech([i16; FRAME_SAMPLES]),
}

/// Protocol parsing error.
#[derive(Debug, Clone)]
pub enum ProtoError {
    /// Malformed packet or response.
    Malformed(String),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Malformed(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ProtoError {}

/// Generic packet builder: start byte, big-endian length, type, fields.
fn packet(ptype: u8, fields: &[u8]) -> Vec<u8> {
    let len = fields.len() as u16;
    let mut result = vec![0x61]; // START_BYTE
    result.extend_from_slice(&len.to_be_bytes());
    result.push(ptype);
    result.extend_from_slice(fields);
    result
}

/// Reset packet (§3.2). Chip responds with READY.
pub fn reset() -> Vec<u8> {
    packet(0, &[0x33])
}

/// Ready query (§3.2). Emitted by chip after reset.
pub fn ready() -> Vec<u8> {
    packet(0, &[0x39])
}

/// Product ID query (§3.2).
pub fn prodid_query() -> Vec<u8> {
    packet(0, &[0x30])
}

/// Version string query (§3.2).
pub fn verstring_query() -> Vec<u8> {
    packet(0, &[0x31])
}

/// Rate parameters (D-STAR mode) (§3.4, §8.1).
/// 12-byte RATEP field with rate-control words for D-STAR full-rate.
pub fn ratep_dstar() -> Vec<u8> {
    packet(
        0,
        &[
            0x0A, // RATEP field ID
            0x01, 0x30, // e (1st rate word)
            0x07, 0x63, // u (2nd)
            0x40, 0x00, // v (3rd)
            0x00, 0x00, // w (4th)
            0x00, 0x00, // x (5th)
            0x00, 0x48, // y (6th)
        ],
    )
}

/// Initialize encoder/decoder (§3.5).
pub fn init_encdec() -> Vec<u8> {
    packet(0, &[0x0B, 0x03])
}

/// Encoder mode: clear error correction (§8.1).
pub fn ecmode_off() -> Vec<u8> {
    packet(0, &[0x05, 0x00, 0x00])
}

/// Decoder mode: clear error concealment (§8.1).
pub fn dcmode_off() -> Vec<u8> {
    packet(0, &[0x06, 0x00, 0x00])
}

/// Gain: set to zero (§3.2).
pub fn gain_zero() -> Vec<u8> {
    packet(0, &[0x4B, 0x00, 0x00])
}

/// Get configuration at reset (§3.1).
pub fn getcfg_query() -> Vec<u8> {
    packet(0, &[0x36])
}

/// Read configuration now (§3.1).
pub fn readcfg_query() -> Vec<u8> {
    packet(0, &[0x37])
}

/// Speech input: 160 PCM samples (§4.1).
/// Packs big-endian i16 samples into a speech packet.
pub fn speech_in(pcm: &[i16; FRAME_SAMPLES]) -> Vec<u8> {
    let mut fields = vec![0x00, 0xA0]; // field ID, count (160)
    for sample in pcm {
        fields.extend_from_slice(&sample.to_be_bytes());
    }
    packet(2, &fields)
}

/// Channel input: 9 compressed bytes (§4.3).
/// Packs a single AMBE frame into a channel packet.
pub fn channel_in(frame: &[u8; FRAME_BYTES]) -> Vec<u8> {
    let mut fields = vec![0x01, 0x48]; // field ID, bit count (72)
    fields.extend_from_slice(frame);
    packet(1, &fields)
}

/// Packet reassembly and resync state machine (§6 resync rule).
#[derive(Debug)]
pub struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    /// Create a new deframer.
    pub fn new() -> Self {
        Deframer { buf: Vec::new() }
    }

    /// Push more bytes into the buffer.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to extract the next complete packet.
    /// Returns None if incomplete; scans for `0x61`, validates type and length,
    /// resyncs by discarding one byte if invalid.
    pub fn next_packet(&mut self) -> Option<RawPacket> {
        loop {
            // Look for START_BYTE. A buffer with no 0x61 at all is pure
            // garbage — drop it so noise cannot accumulate unboundedly.
            let Some(start_idx) = self.buf.iter().position(|&b| b == 0x61) else {
                self.buf.clear();
                return None;
            };

            // Discard any leading garbage.
            self.buf.drain(..start_idx);

            // Need at least 4 bytes (header) to parse.
            if self.buf.len() < 4 {
                return None;
            }

            // Parse length (big-endian).
            let len = u16::from_be_bytes([self.buf[1], self.buf[2]]) as usize;

            // Validate length (§6): ≤ 1024 bytes.
            if len > 1024 {
                self.buf.remove(0); // discard invalid start
                continue;
            }

            // Parse type.
            let ptype = self.buf[3];

            // Validate type (§6): must be 0, 1, or 2.
            if ptype > 2 {
                self.buf.remove(0); // discard invalid start
                continue;
            }

            // Check if we have the full packet.
            let total_len = 4 + len;
            if self.buf.len() < total_len {
                return None;
            }

            // Extract packet.
            let packet_bytes = self.buf.drain(..total_len).collect::<Vec<_>>();
            let payload = packet_bytes[4..].to_vec();

            return Some(RawPacket { ptype, payload });
        }
    }
}

impl Default for Deframer {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a raw packet into a Response.
///
/// # Errors
/// Returns `ProtoError` if the packet is malformed (wrong field IDs,
/// lengths, or types).
pub fn parse_response(pkt: &RawPacket) -> Result<Response, ProtoError> {
    match pkt.ptype {
        0 => parse_control(pkt),
        1 => parse_channel(pkt),
        2 => parse_speech(pkt),
        _ => Err(ProtoError::Malformed(format!(
            "invalid packet type: {}",
            pkt.ptype
        ))),
    }
}

/// Parse control packet (§2–§3).
fn parse_control(pkt: &RawPacket) -> Result<Response, ProtoError> {
    let mut payload = pkt.payload.as_slice();

    // Skip optional 0x40 (channel field) if present at the start (§2.4).
    if !payload.is_empty() && payload[0] == 0x40 {
        payload = &payload[1..];
    }

    if payload.is_empty() {
        return Err(ProtoError::Malformed("empty control payload".into()));
    }

    let field = payload[0];

    match field {
        0x39 => Ok(Response::Ready), // PKT_READY
        0x30 => {
            // ProdId: truncate at first NUL, lossy UTF-8.
            let data = &payload[1..];
            let bytes = data.split(|&b| b == 0).next().unwrap_or(&[]);
            let s = String::from_utf8_lossy(bytes);
            Ok(Response::ProdId(s.to_string()))
        }
        0x31 => {
            // Version: truncate at first NUL, lossy UTF-8.
            let data = &payload[1..];
            let bytes = data.split(|&b| b == 0).next().unwrap_or(&[]);
            let s = String::from_utf8_lossy(bytes);
            Ok(Response::Version(s.to_string()))
        }
        0x36 | 0x37 => {
            // Config: 3 bytes.
            if payload.len() < 4 {
                return Err(ProtoError::Malformed("config response too short".into()));
            }
            let mut cfg = [0u8; 3];
            cfg.copy_from_slice(&payload[1..4]);
            Ok(Response::Config(cfg))
        }
        _ => {
            // Status: field + 0x00 (at minimum).
            if payload.len() < 2 {
                return Err(ProtoError::Malformed("status response too short".into()));
            }
            Ok(Response::Status {
                field,
                status: payload[1],
            })
        }
    }
}

/// Parse channel packet (§4.1 output, §4.3 input).
fn parse_channel(pkt: &RawPacket) -> Result<Response, ProtoError> {
    let mut payload = pkt.payload.as_slice();

    // Skip optional 0x40 0x00 (channel field) if present (§2.4).
    if payload.len() >= 2 && payload[0] == 0x40 && payload[1] == 0x00 {
        payload = &payload[2..];
    }

    if payload.len() < 2 {
        return Err(ProtoError::Malformed("channel payload too short".into()));
    }

    let field = payload[0];
    let bits = payload[1];

    // Field 0x01 is CHAND — the only channel field this driver reads.
    if field != 0x01 {
        return Err(ProtoError::Malformed(format!(
            "expected channel field 0x01, got 0x{:02x}",
            field
        )));
    }

    // The bit count is REPORTED, not validated against a rate this function
    // cannot know. It used to require 0x48 and call anything else "rate lost"
    // (§8.4), which is a real condition — but it is the caller who configured
    // the rate and therefore the only party who can tell a lost rate from a
    // correctly-decoded half-rate frame. Hard-coding 72 here made every
    // AMBE+2 half-rate mode unreachable through this crate: YSF's DN modes and
    // NXDN both answer at 0x31, and both were rejected as corruption.
    //
    // `ThumbDv::encode_frame` still enforces 72, so the high-level D-Star API
    // is unchanged; see its own "rate lost" check.
    if bits == 0 || bits as usize > FRAME_BYTES * 8 {
        return Err(ProtoError::Malformed(format!(
            "channel bit count {bits} outside 1..={}",
            FRAME_BYTES * 8
        )));
    }

    let need = (bits as usize).div_ceil(8);
    if payload.len() < 2 + need {
        return Err(ProtoError::Malformed(format!(
            "channel data too short: {bits} bits need {need} bytes, got {}",
            payload.len().saturating_sub(2)
        )));
    }

    let mut data = [0u8; FRAME_BYTES];
    data[..need].copy_from_slice(&payload[2..2 + need]);
    Ok(Response::Channel { bits, data })
}

/// Parse speech packet (§4.1).
fn parse_speech(pkt: &RawPacket) -> Result<Response, ProtoError> {
    let mut payload = pkt.payload.as_slice();

    // Skip optional 0x40 0x00 (channel field) if present (§2.4).
    if payload.len() >= 2 && payload[0] == 0x40 && payload[1] == 0x00 {
        payload = &payload[2..];
    }

    if payload.len() < 2 {
        return Err(ProtoError::Malformed("speech payload too short".into()));
    }

    let field = payload[0];
    let count = payload[1];

    // Expect field 0x00 (SPEECHD) and count 0xA0 (160 samples).
    if field != 0x00 {
        return Err(ProtoError::Malformed(format!(
            "expected speech field 0x00, got 0x{:02x}",
            field
        )));
    }

    if count != 0xA0 {
        return Err(ProtoError::Malformed(format!(
            "expected 160 samples, got {}",
            count
        )));
    }

    if payload.len() < 2 + FRAME_SAMPLES * 2 {
        return Err(ProtoError::Malformed("speech data too short".into()));
    }

    let mut pcm = [0i16; FRAME_SAMPLES];
    for (i, pcm_sample) in pcm.iter_mut().enumerate() {
        let offset = 2 + i * 2;
        let bytes = [payload[offset], payload[offset + 1]];
        *pcm_sample = i16::from_be_bytes(bytes);
    }
    Ok(Response::Speech(pcm))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn control_builders_match_the_documented_wire_bytes() {
        // ambe3000-protocol.md §3.2, §3.4, §3.5, §3.9, §8.1.
        assert_eq!(reset(), hex("61 00 01 00 33"));
        assert_eq!(ready(), hex("61 00 01 00 39"));
        assert_eq!(prodid_query(), hex("61 00 01 00 30"));
        assert_eq!(verstring_query(), hex("61 00 01 00 31"));
        assert_eq!(
            ratep_dstar(),
            hex("61 00 0D 00 0A 01 30 07 63 40 00 00 00 00 00 00 48")
        );
        assert_eq!(init_encdec(), hex("61 00 02 00 0B 03"));
        assert_eq!(ecmode_off(), hex("61 00 03 00 05 00 00"));
        assert_eq!(dcmode_off(), hex("61 00 03 00 06 00 00"));
        assert_eq!(gain_zero(), hex("61 00 03 00 4B 00 00"));
        assert_eq!(getcfg_query(), hex("61 00 01 00 36"));
        assert_eq!(readcfg_query(), hex("61 00 01 00 37"));
    }

    #[test]
    fn speech_in_packs_160_samples_big_endian() {
        let mut pcm = [0i16; FRAME_SAMPLES];
        pcm[0] = 0x1234;
        pcm[159] = -2; // 0xFFFE
        let p = speech_in(&pcm);
        assert_eq!(p.len(), 326); // §4.1: header 4 + 1 + 1 + 320
        assert_eq!(&p[..6], &hex("61 01 42 02 00 A0")[..]);
        assert_eq!(&p[6..8], &[0x12, 0x34]);
        assert_eq!(&p[324..], &[0xFF, 0xFE]);
    }

    #[test]
    fn channel_in_wraps_9_bytes() {
        let f = [0xAB; FRAME_BYTES];
        let p = channel_in(&f);
        assert_eq!(p.len(), 15);
        assert_eq!(&p[..6], &hex("61 00 0B 01 01 48")[..]);
        assert_eq!(&p[6..], &[0xAB; 9][..]);
    }

    #[test]
    fn deframer_reassembles_and_resyncs() {
        let mut d = Deframer::new();
        // Garbage, then READY split across two pushes, then a full
        // channel response.
        d.push(&[0x00, 0xFF, 0x61, 0x00]);
        assert!(d.next_packet().is_none());
        d.push(&hex("01 00 39"));
        let ready = d.next_packet().unwrap();
        assert_eq!(ready.ptype, 0);
        assert_eq!(ready.payload, vec![0x39]);
        let mut chan = hex("61 00 0B 01 01 48");
        chan.extend_from_slice(&[7u8; 9]);
        d.push(&chan);
        let pkt = d.next_packet().unwrap();
        assert_eq!(pkt.ptype, 1);
        assert!(d.next_packet().is_none());
    }

    #[test]
    fn deframer_skips_invalid_type_bytes() {
        let mut d = Deframer::new();
        // 0x61 with TYPE 7 is not a packet start — resync must discard
        // and still find the real READY behind it (§6 resync rule).
        let mut bytes = hex("61 00 01 07");
        bytes.extend_from_slice(&hex("61 00 01 00 39"));
        d.push(&bytes);
        let pkt = d.next_packet().unwrap();
        assert_eq!(parse_response(&pkt).unwrap(), Response::Ready);
    }

    #[test]
    fn responses_parse_per_the_manual() {
        // ProdId: 61 00 0n 00 30 + NUL-terminated string (§3.2).
        let mut payload = vec![0x30];
        payload.extend_from_slice(b"AMBE3000R\0");
        let p = RawPacket { ptype: 0, payload };
        assert_eq!(
            parse_response(&p).unwrap(),
            Response::ProdId("AMBE3000R".into())
        );
        // Status: field + 0x00 (§3.1).
        let p = RawPacket {
            ptype: 0,
            payload: vec![0x0A, 0x00],
        };
        assert_eq!(
            parse_response(&p).unwrap(),
            Response::Status {
                field: 0x0A,
                status: 0
            }
        );
        // A 49-bit (0x31) channel response is AMBE+2 half rate — YSF DN and
        // NXDN — and is perfectly valid. This assertion used to be
        // `is_err()`, which is what made every half-rate mode unreachable
        // through this crate: the parser called a correctly-decoded frame
        // corruption because it assumed one caller's rate.
        let mut payload = vec![0x01, 0x31];
        payload.extend_from_slice(&[0xAB; 7]);
        let p = RawPacket { ptype: 1, payload };
        assert_eq!(
            parse_response(&p).unwrap(),
            Response::Channel {
                bits: 0x31,
                data: [0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0xAB, 0x00, 0x00],
            },
            "49 bits occupy seven bytes; the rest is zero, not garbage"
        );
        // Full rate still parses, and still reports its own bit count.
        let mut payload = vec![0x01, 0x48];
        payload.extend_from_slice(&[0x11; 9]);
        let p = RawPacket { ptype: 1, payload };
        assert_eq!(
            parse_response(&p).unwrap(),
            Response::Channel {
                bits: 0x48,
                data: [0x11; 9]
            }
        );

        // A count of zero, or one wider than the frame buffer, is still
        // refused — reporting what arrived is not the same as trusting it.
        for bad in [0x00u8, 0x49, 0xFF] {
            let mut payload = vec![0x01, bad];
            payload.extend_from_slice(&[0u8; 9]);
            let p = RawPacket { ptype: 1, payload };
            assert!(
                parse_response(&p).is_err(),
                "bit count 0x{bad:02x} must be refused"
            );
        }

        // A frame shorter than its declared bit count is refused rather than
        // read past the end of the payload.
        let p = RawPacket {
            ptype: 1,
            payload: vec![0x01, 0x48, 0x11, 0x22],
        };
        assert!(
            parse_response(&p).is_err(),
            "truncated frame must be refused"
        );

        // Speech response round-trips PCM.
        let mut payload = vec![0x00, 0xA0];
        for i in 0..160u16 {
            let v = (i as i16 - 80) * 100;
            payload.extend_from_slice(&v.to_be_bytes());
        }
        let p = RawPacket { ptype: 2, payload };
        match parse_response(&p).unwrap() {
            Response::Speech(pcm) => {
                assert_eq!(pcm[0], -8000);
                assert_eq!(pcm[159], 7900);
            }
            other => panic!("expected speech, got {other:?}"),
        }
    }
}
