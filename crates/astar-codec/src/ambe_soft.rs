// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The software D-Star vocoder (astar-dstar-perens, feature `dstar-soft-ambe`).
//!
//! Bruce Perens' AMBE 3600x2400 implementation, vendored at
//! `vendor/ham-digital-modes-ambe` (LGPL-3.0-or-later; see its `VENDORED.md`
//! and `LICENSE-EXCEPTIONS.md` §9), behind the same [`AmbeStream`] and
//! [`AmbeVoice`] traits the `ThumbDV` dongle implements.
//!
//! # D-Star only
//!
//! [`open_stream`] returns `None` for every [`VocoderMode`] but
//! [`VocoderMode::Dstar`], and a stream that is somehow handed another mode's
//! frame drops it rather than decode it. DMR, NXDN and System Fusion are
//! AMBE+2 (US 8,359,197 until 2028-05-20) and stay hardware-only; the
//! vendored crate does not contain their code at all.
//!
//! # Shape
//!
//! Everything runs in-process and synchronously: no thread, no port, no
//! vocoder trust. A submitted frame is decoded on the spot and is ready at
//! the next poll, so `in_flight()` counts decoded frames not yet taken —
//! normally zero or one — and the [`AMBE_STREAM_MAX_IN_FLIGHT`] bound and its
//! drop-newest rule hold exactly as they do for the dongle.
//!
//! Transmit keeps D-Star's one-frame-in, one-frame-out rhythm. The encoder
//! analyses a few frames ahead, so for the first frames of a transmission it
//! has nothing to emit yet; those slots carry D-Star's standard silence frame
//! ([`DSTAR_SILENCE`]), and from then on every 20 ms of PCM in returns one
//! frame out, a constant few frames late.
//!
//! Float, not fixed point: the two agree to the fourth decimal upstream, and
//! the float decoder is the faster of the two (about 0.27 ms per frame).

use std::collections::VecDeque;

use ham_digital_modes_ambe::ambe::float::dstar::encoder::Encoder;
use ham_digital_modes_ambe::ambe::float::dstar::interleave::{
    frame_to_wire_bytes, wire_bytes_to_frame,
};
use ham_digital_modes_ambe::ambe::float::dstar::synthesis::DStarSynthesisDecoder;

use crate::ambe::{AMBE_STREAM_MAX_IN_FLIGHT, AmbeStream, AmbeVoice, ChannelFrame, VocoderMode};

/// D-Star's standard AMBE silence frame, as every D-Star implementation sends
/// it (the same nine bytes as `astar_dstar::NULL_AMBE`; that crate sits above
/// this one, so the constant is repeated and a test in `astar-console` keeps
/// the two equal).
pub(crate) const DSTAR_SILENCE: [u8; 9] = [0x9E, 0x8D, 0x32, 0x88, 0x26, 0x1A, 0x3F, 0x61, 0xE8];

/// One 20 ms frame at 8 kHz.
const FRAME: usize = 160;

/// The decoder and encoder pair both traits share.
struct Codec {
    decoder: DStarSynthesisDecoder,
    encoder: Encoder,
    /// Frames the encoder produced beyond the one-per-call rhythm (normally
    /// empty; holds the odd extra so none is lost).
    lagged: VecDeque<[u8; 9]>,
}

impl Codec {
    fn new() -> Self {
        Self {
            decoder: DStarSynthesisDecoder::new(),
            encoder: Encoder::new(),
            lagged: VecDeque::new(),
        }
    }

    fn decode(&mut self, bytes: &[u8; 9]) -> [i16; FRAME] {
        // `None` means the decoder had nothing to repeat yet; silence keeps
        // the one-frame-in, one-frame-out contract.
        self.decoder
            .decode_frame(wire_bytes_to_frame(bytes))
            .map_or([0; FRAME], |pcm| to_i16(&pcm))
    }

    fn encode(&mut self, pcm: &[i16; FRAME]) -> [u8; 9] {
        let samples: Vec<f64> = pcm.iter().map(|&s| f64::from(s)).collect();
        self.encoder.push_samples(&samples);
        while let Some(frame) = self.encoder.next_frame() {
            self.lagged.push_back(frame_to_wire_bytes(frame));
        }
        self.lagged.pop_front().unwrap_or(DSTAR_SILENCE)
    }
}

/// Round and clamp the decoder's 16-bit-scaled float PCM.
fn to_i16(pcm: &[f64; FRAME]) -> [i16; FRAME] {
    let mut out = [0i16; FRAME];
    for (o, &s) in out.iter_mut().zip(pcm) {
        // `as` saturates for floats, and NaN becomes 0.
        #[allow(clippy::cast_possible_truncation)]
        {
            *o = s.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        }
    }
    out
}

/// The software vocoder as a streaming [`AmbeStream`]. D-Star frames only.
pub(crate) struct SoftDstarStream {
    codec: Codec,
    decoded: VecDeque<[i16; FRAME]>,
    encoded: VecDeque<ChannelFrame>,
}

impl SoftDstarStream {
    fn new() -> Self {
        Self {
            codec: Codec::new(),
            decoded: VecDeque::new(),
            encoded: VecDeque::new(),
        }
    }
}

impl AmbeStream for SoftDstarStream {
    fn submit_decode(&mut self, frame: ChannelFrame) {
        if self.decoded.len() >= AMBE_STREAM_MAX_IN_FLIGHT {
            tracing::warn!("ambe-soft: decode queue full, dropping the incoming frame");
            return;
        }
        let Some(bytes) = frame.as_dstar() else {
            // Never decode another mode's frame in software: DMR, NXDN and
            // System Fusion are AMBE+2 and hardware-only.
            tracing::warn!(
                mode = frame.mode().as_str(),
                "ambe-soft: refusing a non-D-Star frame (the software vocoder is D-Star only)"
            );
            return;
        };
        self.decoded.push_back(self.codec.decode(&bytes));
    }

    fn poll_decoded(&mut self) -> Option<[i16; FRAME]> {
        self.decoded.pop_front()
    }

    fn in_flight(&self) -> usize {
        self.decoded.len()
    }

    fn submit_encode(&mut self, pcm: [i16; FRAME]) {
        if self.encoded.len() >= AMBE_STREAM_MAX_IN_FLIGHT {
            tracing::warn!("ambe-soft: encode queue full, dropping the incoming PCM");
            return;
        }
        let frame = self.codec.encode(&pcm);
        self.encoded.push_back(ChannelFrame::Dstar(frame));
    }

    fn poll_encoded(&mut self) -> Option<ChannelFrame> {
        self.encoded.pop_front()
    }

    fn in_flight_encoded(&self) -> usize {
        self.encoded.len()
    }
}

/// The software vocoder as a stop-and-wait [`AmbeVoice`].
pub(crate) struct SoftDstarVoice {
    codec: Codec,
}

impl AmbeVoice for SoftDstarVoice {
    fn decode(&mut self, frame: &[u8; 9]) -> [i16; FRAME] {
        self.codec.decode(frame)
    }

    fn encode(&mut self, pcm: &[i16; FRAME]) -> [u8; 9] {
        self.codec.encode(pcm)
    }
}

/// A software stream for `mode`, or `None` unless `mode` is D-Star.
pub(crate) fn open_stream(mode: VocoderMode) -> Option<Box<dyn AmbeStream>> {
    crate::ambe::software_backend_permitted(mode)
        .then(|| Box::new(SoftDstarStream::new()) as Box<dyn AmbeStream>)
}

/// A software stop-and-wait voice. [`AmbeVoice`] is D-Star's full-rate frame
/// by definition, so there is no mode to check.
pub(crate) fn open_voice() -> Box<dyn AmbeVoice> {
    Box::new(SoftDstarVoice {
        codec: Codec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(pcm: &[i16]) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let n = pcm.len() as f64;
        (pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / n).sqrt()
    }

    /// A synthetic voiced signal: a 140 Hz fundamental and its harmonics, with
    /// a slow amplitude wobble. Made up here, so it is nobody's voice.
    fn voiced(frames: usize) -> Vec<[i16; FRAME]> {
        let mut out = Vec::new();
        for f in 0..frames {
            let mut pcm = [0i16; FRAME];
            for (i, s) in pcm.iter_mut().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let t = (f * FRAME + i) as f64 / 8000.0;
                let env = 0.7 + 0.3 * (2.0 * std::f64::consts::PI * 3.0 * t).sin();
                let mut v = 0.0;
                for h in 1..=12 {
                    let hf = f64::from(h);
                    v += (2.0 * std::f64::consts::PI * 140.0 * hf * t).sin() / hf;
                }
                #[allow(clippy::cast_possible_truncation)]
                {
                    *s = (v * env * 5000.0) as i16;
                }
            }
            out.push(pcm);
        }
        out
    }

    #[test]
    fn only_dstar_opens() {
        assert!(open_stream(VocoderMode::Dstar).is_some());
        assert!(open_stream(VocoderMode::Dmr).is_none());
        assert!(open_stream(VocoderMode::YsfDn).is_none());
    }

    #[test]
    fn the_silence_frame_decodes_quiet() {
        let mut s = SoftDstarStream::new();
        let mut loudest = 0.0f64;
        for _ in 0..25 {
            s.submit_decode(ChannelFrame::Dstar(DSTAR_SILENCE));
            let pcm = s.poll_decoded().expect("one frame out per frame in");
            loudest = loudest.max(rms(&pcm));
        }
        assert!(loudest < 50.0, "silence decoded at rms {loudest}");
    }

    #[test]
    fn non_dstar_frames_are_refused_not_decoded() {
        let mut s = SoftDstarStream::new();
        s.submit_decode(ChannelFrame::Dmr(DSTAR_SILENCE));
        assert_eq!(s.in_flight(), 0);
        assert!(s.poll_decoded().is_none());
    }

    #[test]
    fn decode_keeps_the_drop_newest_bound() {
        let mut s = SoftDstarStream::new();
        for _ in 0..AMBE_STREAM_MAX_IN_FLIGHT + 3 {
            s.submit_decode(ChannelFrame::Dstar(DSTAR_SILENCE));
        }
        assert_eq!(s.in_flight(), AMBE_STREAM_MAX_IN_FLIGHT);
    }

    #[test]
    fn encode_is_one_frame_in_one_frame_out_starting_with_silence() {
        let mut s = SoftDstarStream::new();
        let input = voiced(50);
        let mut out = Vec::new();
        for pcm in &input {
            s.submit_encode(*pcm);
            assert_eq!(s.in_flight_encoded(), 1);
            out.push(s.poll_encoded().and_then(ChannelFrame::as_dstar).unwrap());
        }
        assert_eq!(out.len(), input.len());
        assert_eq!(out[0], DSTAR_SILENCE, "look-ahead slots carry silence");
        assert!(
            out.iter().skip(10).any(|f| *f != DSTAR_SILENCE),
            "real frames follow the look-ahead"
        );
    }

    #[test]
    fn voiced_input_survives_a_round_trip() {
        let mut enc = SoftDstarStream::new();
        let mut dec = SoftDstarStream::new();
        let input = voiced(100);
        let mut decoded = Vec::new();
        for pcm in &input {
            enc.submit_encode(*pcm);
            let frame = enc.poll_encoded().unwrap();
            dec.submit_decode(frame);
            decoded.extend_from_slice(&dec.poll_decoded().unwrap());
        }
        let in_rms = rms(&input.concat());
        // Skip the start (look-ahead silence, decoder warm-up). Measured at
        // about -3.7 dB; upstream reports a -4 dB level offset for this codec
        // family on real speech, the chip included.
        let out_rms = rms(&decoded[30 * FRAME..]);
        assert!(
            out_rms > in_rms * 0.4 && out_rms < in_rms * 1.5,
            "in rms {in_rms}, out rms {out_rms}"
        );
    }

    #[test]
    fn the_voice_interface_round_trips_too() {
        let mut v = open_voice();
        let input = voiced(60);
        let mut last = [0i16; FRAME];
        for pcm in &input {
            let frame = v.encode(pcm);
            last = v.decode(&frame);
        }
        assert!(rms(&last) > 100.0, "rms {}", rms(&last));
    }
}
