// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! GSM 06.10 full-rate speech codec: EchoLink's voice, and IAX2's GSM format.
//!
//! One frame is 20 ms: 160 samples of 8 kHz 16-bit PCM in, 33 bytes out. The
//! 33 bytes are the usual `.gsm` byte frame that libgsm, Asterisk and
//! EchoLink all exchange — a `0xD` signature nibble, then the 76 parameters
//! of EN 300 961 Table 1.1, each most-significant bit first.
//!
//! The arithmetic is `oxideav-gsm` (pure Rust, MIT; licence and patent
//! reasoning in `LICENSE-EXCEPTIONS.md` §8). This module is a thin wrapper
//! that fixes the frame shapes in the types and turns the one decode error
//! — a frame without the signature — into silence, so callers on a live
//! audio path never have to handle it. `tests/gsm_etsi.rs` holds the wrapper
//! and the crate to the ETSI test sequences bit for bit.

use oxideav_gsm::{DecoderState, EncoderState, UnpackedFrame};

/// PCM samples in one 20 ms frame at 8 kHz.
pub const FRAME_SAMPLES: usize = 160;
/// Bytes in one encoded frame.
pub const FRAME_BYTES: usize = 33;

/// The signature every GSM 06.10 byte frame carries in its top nibble.
const SIGNATURE: u8 = 0xD;

/// GSM 06.10 encoder. Keeps the filter and long-term-predictor state that
/// runs from one frame to the next, so use one encoder per audio stream.
#[derive(Default)]
pub struct GsmEncoder {
    state: EncoderState,
}

impl GsmEncoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode one 20 ms frame.
    pub fn encode(&mut self, pcm: &[i16; FRAME_SAMPLES]) -> [u8; FRAME_BYTES] {
        self.state.encode_frame(pcm).to_gsm_byte_frame()
    }
}

/// GSM 06.10 decoder. Keeps synthesis state between frames, so use one
/// decoder per audio stream.
#[derive(Default)]
pub struct GsmDecoder {
    state: DecoderState,
}

impl GsmDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one 20 ms frame. A frame whose top nibble is not `0xD` is not
    /// GSM: it decodes as 160 zeros and leaves the decoder state untouched,
    /// so one corrupt packet does not disturb the frames after it.
    pub fn decode(&mut self, frame: &[u8; FRAME_BYTES]) -> [i16; FRAME_SAMPLES] {
        if frame[0] >> 4 != SIGNATURE {
            return [0; FRAME_SAMPLES];
        }
        match UnpackedFrame::from_gsm_byte_frame(frame) {
            Ok(unpacked) => self.state.decode_frame(&unpacked),
            // Unreachable for a 33-byte frame with the signature, which is
            // all `from_gsm_byte_frame` checks; silence keeps it total.
            Err(_) => [0; FRAME_SAMPLES],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes_match_the_crate() {
        assert_eq!(FRAME_SAMPLES, oxideav_gsm::bitstream::FRAME_SAMPLES);
        assert_eq!(
            GsmEncoder::new().encode(&[0; FRAME_SAMPLES]).len(),
            FRAME_BYTES
        );
    }

    #[test]
    fn a_bad_frame_does_not_touch_decoder_state() {
        const CYCLE: [i16; 8] = [-4000, -3000, -2000, -1000, 0, 1000, 2000, 3000];
        let mut enc = GsmEncoder::new();
        let pcm: [i16; FRAME_SAMPLES] = std::array::from_fn(|i| CYCLE[i % 8]);
        let frames: Vec<_> = (0..4).map(|_| enc.encode(&pcm)).collect();

        let mut clean = GsmDecoder::new();
        let mut hit = GsmDecoder::new();
        for frame in &frames {
            assert_eq!(hit.decode(&[0x55; FRAME_BYTES]), [0; FRAME_SAMPLES]);
            assert_eq!(hit.decode(frame), clean.decode(frame));
        }
    }
}
