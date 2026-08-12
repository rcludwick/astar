#![warn(missing_docs)]

//! DVSI packet codec for the AMBE-3000 vocoder (ThumbDV / DV3000).
//!
//! This crate implements encoding and decoding of the DVSI packet protocol
//! spoken by AMBE-3000 chips, as specified in the AMBE-3000R™ Vocoder Chip
//! Users Manual and documented in `docs/research/ambe3000-protocol.md`.
//!
//! See [`packet`] for the codec interface.

pub mod device;
pub mod packet;
pub mod transport;

pub use packet::{
    channel_in, dcmode_off, ecmode_off, gain_zero, getcfg_query, init_encdec, parse_response,
    prodid_query, ratep_dstar, readcfg_query, ready, reset, speech_in, Deframer, ProtoError,
    RawPacket, Response, FRAME_BYTES, FRAME_SAMPLES,
};

pub use device::{detect, DeviceError, ThumbDv};
pub use transport::{MockTransport, SerialTransport, Transport};

/// Test utilities (hex helper for tests).
#[cfg(test)]
pub(crate) mod testutil {
    /// Parse hex string "AB CD EF" into bytes.
    pub fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }
}
