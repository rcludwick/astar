// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Network-edge transcode (iax-31f7): internal buses carry bus-rate i16 PCM (8 or 16 kHz);
//! wire formats exist only here. TX encodes PCM into the negotiated format;
//! RX decodes on the received frame's own format.

use astar_codec::g711::{alaw_decode, alaw_encode, ulaw_decode, ulaw_encode};
use astar_codec::slin;
use astar_iax_core::VoiceFormat;

/// Encode one PCM frame for the wire. Only negotiable formats are reachable;
/// anything else is a negotiation bug — `debug_assert`, µ-law in release.
pub(crate) fn encode_voice(format: VoiceFormat, pcm: &[i16]) -> Vec<u8> {
    match format {
        VoiceFormat::G711A => pcm.iter().map(|&s| alaw_encode(s)).collect(),
        // slin is big-endian on the wire; slin16 is LITTLE-endian — an
        // Asterisk chan_iax2 asymmetry, both live-verified (iax-31f7 /
        // iax-4348). See `astar_codec::slin` module docs.
        VoiceFormat::Slin => slin::encode(pcm),
        VoiceFormat::Slin16 => slin::encode_le(pcm),
        VoiceFormat::G711U => pcm.iter().map(|&s| ulaw_encode(s)).collect(),
        other => {
            debug_assert!(false, "negotiated unencodable format {other:?}");
            pcm.iter().map(|&s| ulaw_encode(s)).collect()
        }
    }
}

/// Decode a received voice payload. `None` ⇒ drop the frame (format we can't
/// decode, or a malformed length for the format).
pub(crate) fn decode_voice(format: VoiceFormat, payload: &[u8]) -> Option<Vec<i16>> {
    match format {
        VoiceFormat::G711U => Some(payload.iter().map(|&b| ulaw_decode(b)).collect()),
        VoiceFormat::G711A => Some(payload.iter().map(|&b| alaw_decode(b)).collect()),
        VoiceFormat::Slin if payload.len().is_multiple_of(2) => Some(slin::decode(payload)),
        VoiceFormat::Slin16 if payload.len().is_multiple_of(2) => Some(slin::decode_le(payload)),
        _ => None,
    }
}

/// Stateful per-call edge: bus-rate PCM ↔ wire-format bytes, resampling with
/// the station resampler when the negotiated wire rate differs from the bus
/// rate (iax-4348). On an 8 kHz station no resampler is ever built and both
/// paths reduce to the pure `encode_voice`/`decode_voice`.
pub(crate) struct EdgeAudio {
    bus_rate: u32,
    /// (`wire_rate`, bus→wire resampler), built lazily on first mismatch.
    tx: Option<(u32, astar_audio::Resampler1)>,
    /// (`wire_rate`, anti-alias low-pass cascade) applied at the bus rate before
    /// a DOWNSAMPLING resample (iax-a9b9). Without it, the filterless linear
    /// resampler folds 16 kHz source energy above the wire Nyquist (4 kHz for an
    /// 8 kHz codec) back into the voice band. Built lazily, only when
    /// `wire_rate < bus_rate`; runs in place, so it changes neither the sample
    /// count nor the 20 ms frame cadence (iax-62ac).
    tx_aa: Option<(u32, Vec<astar_audio::Biquad>)>,
    /// (`wire_rate`, wire→bus resampler), rebuilt if the frame rate changes.
    rx: Option<(u32, astar_audio::Resampler1)>,
    /// Resampled-but-not-yet-framed output (iax-62ac). The interpolator holds
    /// back a few warm-up samples, so the first pass runs short; these FIFOs
    /// let encode/decode emit EXACTLY one 20 ms frame per call (left-padding
    /// the very first frame with a few samples of silence) instead of
    /// propagating short frames onto a fixed-cadence wire/bus.
    tx_fifo: Vec<i16>,
    rx_fifo: Vec<i16>,
}

/// Pop exactly `frame` samples from `fifo`, left-padding with silence when the
/// stream has not yet produced enough (start-up warm-up only — steady state is
/// 1:1 in and out).
fn pop_frame(fifo: &mut Vec<i16>, frame: usize) -> Vec<i16> {
    if fifo.len() < frame {
        let missing = frame - fifo.len();
        let mut out = vec![0i16; missing];
        out.append(fifo);
        return out;
    }
    fifo.drain(..frame).collect()
}

/// Anti-alias low-pass cascade for a bus→wire DOWNSAMPLE (iax-a9b9): a 4th-order
/// Butterworth (two cascaded 2nd-order sections with the standard Butterworth Q
/// pair) with its corner at 0.45 × the wire rate — i.e. 0.9 × the wire Nyquist,
/// so the telephony voice band (≤ ~3.4 kHz for an 8 kHz codec) passes intact
/// while fold-band energy above Nyquist is rolled off before decimation. Runs at
/// the bus rate.
#[allow(clippy::cast_precision_loss)]
fn antialias_lowpass(bus_rate: u32, wire_rate: u32) -> Vec<astar_audio::Biquad> {
    let cutoff = 0.45 * wire_rate as f32;
    [0.541_196_1_f32, 1.306_563]
        .iter()
        .map(|&q| astar_audio::Biquad::lowpass(bus_rate, cutoff, q))
        .collect()
}

impl EdgeAudio {
    pub(crate) fn new(bus_rate: u32) -> Self {
        Self {
            bus_rate,
            tx: None,
            tx_aa: None,
            rx: None,
            tx_fifo: Vec::new(),
            rx_fifo: Vec::new(),
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn encode(&mut self, format: VoiceFormat, pcm: &[i16]) -> Vec<u8> {
        let wire_rate = format.sample_rate().unwrap_or(self.bus_rate);
        if wire_rate == self.bus_rate {
            return encode_voice(format, pcm);
        }
        let mut f: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        // Anti-alias BEFORE a downsample (iax-a9b9): the linear resampler has no
        // filter, so any bus-rate energy above the wire Nyquist would fold into
        // the voice band. Filter in place at the bus rate first. Upsampling
        // (wire_rate > bus_rate) has no aliasing to guard against, so it is
        // skipped. Borrow `tx_aa` here and let it end before the `tx` borrow.
        if wire_rate < self.bus_rate {
            let lp = match &mut self.tx_aa {
                Some((r, lp)) if *r == wire_rate => lp,
                slot => {
                    &mut slot
                        .insert((wire_rate, antialias_lowpass(self.bus_rate, wire_rate)))
                        .1
                }
            };
            for bq in lp.iter_mut() {
                bq.process(&mut f);
            }
        }
        let rs = match &mut self.tx {
            Some((r, rs)) if *r == wire_rate => rs,
            slot => {
                // Chunk = one 20 ms bus frame (iax-62ac): with the integer
                // bus↔wire ratios, one frame in yields exactly one frame out,
                // so every SendVoice payload matches its 20 ms timestamp.
                let chunk = (self.bus_rate / 50) as usize;
                let rs = astar_audio::Resampler1::with_chunk(self.bus_rate, wire_rate, chunk)
                    .expect("valid edge rates");
                &mut slot.insert((wire_rate, rs)).1
            }
        };
        // Resampler push only errors on internal rubato failure; drop the
        // frame's contribution rather than poison the call.
        if rs.push(&f).is_err() {
            return encode_voice(format, &[]);
        }
        let out = rs.drain_all();
        self.tx_fifo
            .extend(out.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
        // One 20 ms wire frame per bus frame, matching the caller's fixed
        // timestamp cadence (iax-62ac).
        let frame = (wire_rate / 50) as usize;
        let pcm_wire = pop_frame(&mut self.tx_fifo, frame);
        encode_voice(format, &pcm_wire)
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn decode(&mut self, format: VoiceFormat, payload: &[u8]) -> Option<Vec<i16>> {
        let pcm_wire = decode_voice(format, payload)?;
        let wire_rate = format.sample_rate().unwrap_or(self.bus_rate);
        if wire_rate == self.bus_rate {
            return Some(pcm_wire);
        }
        let rs = match &mut self.rx {
            Some((r, rs)) if *r == wire_rate => rs,
            slot => {
                // Chunk = one 20 ms wire frame (iax-62ac): each received
                // frame becomes exactly one bus frame — no 0-sample outputs
                // underrunning the speaker between chunk boundaries.
                let chunk = (wire_rate / 50) as usize;
                let rs = astar_audio::Resampler1::with_chunk(wire_rate, self.bus_rate, chunk)
                    .expect("valid edge rates");
                &mut slot.insert((wire_rate, rs)).1
            }
        };
        let f: Vec<f32> = pcm_wire.iter().map(|&s| f32::from(s) / 32768.0).collect();
        rs.push(&f).ok()?;
        let out = rs.drain_all();
        self.rx_fifo
            .extend(out.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
        // One 20 ms bus frame per wire frame — no 0-sample underruns at the
        // speaker between resampler chunk boundaries (iax-62ac).
        let frame = (self.bus_rate / 50) as usize;
        Some(pop_frame(&mut self.rx_fifo, frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// iax-62ac: one 20 ms frame in must yield exactly one 20 ms frame out,
    /// every frame, or cross-rate calls stutter (TX frames are stamped on a
    /// 20 ms cadence regardless of payload length).
    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn cross_rate_tx_emits_exactly_one_wire_frame_per_bus_frame() {
        let mut edge = EdgeAudio::new(16000);
        let bus_frame: Vec<i16> = (0..320).map(|i| ((i % 64) * 500 - 16000) as i16).collect();
        for n in 0..50 {
            let wire = edge.encode(VoiceFormat::G711U, &bus_frame);
            assert_eq!(wire.len(), 160, "frame {n}: expected 160 ulaw bytes");
        }
    }

    /// iax-62ac RX half: each 20 ms µ-law wire frame must decode to exactly
    /// one 20 ms bus frame — 0-sample outputs underrun the speaker.
    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn cross_rate_rx_emits_exactly_one_bus_frame_per_wire_frame() {
        let mut edge = EdgeAudio::new(16000);
        let wire_frame: Vec<u8> = (0..160).map(|i| (i % 251) as u8).collect();
        for n in 0..50 {
            let pcm = edge
                .decode(VoiceFormat::G711U, &wire_frame)
                .expect("decodes");
            assert_eq!(pcm.len(), 320, "frame {n}: expected 320 bus samples");
        }
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn slin_tx_rx_is_bit_exact() {
        let pcm: Vec<i16> = (0..160).map(|i| (i * 200 - 16000) as i16).collect();
        let wire = encode_voice(VoiceFormat::Slin, &pcm);
        assert_eq!(wire.len(), 320);
        assert_eq!(decode_voice(VoiceFormat::Slin, &wire), Some(pcm));
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn slin16_tx_rx_is_bit_exact() {
        let pcm: Vec<i16> = (0..320).map(|i| (i * 100 - 16000) as i16).collect();
        let wire = encode_voice(VoiceFormat::Slin16, &pcm);
        assert_eq!(wire.len(), 640);
        assert_eq!(decode_voice(VoiceFormat::Slin16, &wire), Some(pcm));
    }

    #[test]
    fn slin_wire_order_is_big_endian_and_slin16_is_little_endian() {
        // Wire-order pinning: the round-trip tests above are order-blind
        // (encode+decode with the SAME wrong order still round-trips), so
        // they cannot catch a byte-order regression. These can. Orders are
        // live-verified vs Asterisk 20 Milliwatt: slin BE (iax-31f7),
        // slin16 LE (iax-4348) — deliberately opposite.
        assert_eq!(encode_voice(VoiceFormat::Slin, &[0x0102]), vec![0x01, 0x02]);
        assert_eq!(
            encode_voice(VoiceFormat::Slin16, &[0x0102]),
            vec![0x02, 0x01]
        );
        assert_eq!(
            decode_voice(VoiceFormat::Slin, &[0x01, 0x02]),
            Some(vec![0x0102])
        );
        assert_eq!(
            decode_voice(VoiceFormat::Slin16, &[0x02, 0x01]),
            Some(vec![0x0102])
        );
    }

    #[test]
    fn ulaw_rx_still_decodes() {
        let wire = vec![astar_codec::g711::ulaw_encode(8000); 160];
        let pcm = decode_voice(VoiceFormat::G711U, &wire).unwrap();
        assert_eq!(pcm.len(), 160);
        assert!(pcm[0] > 7000 && pcm[0] < 9000); // companded round-trip
    }

    #[test]
    fn odd_length_slin_and_unsupported_formats_drop() {
        assert_eq!(decode_voice(VoiceFormat::Slin, &[0u8; 321]), None);
        assert_eq!(
            decode_voice(VoiceFormat::Slin16, &[0u8; 640]),
            Some(vec![0i16; 320])
        );
        assert_eq!(decode_voice(VoiceFormat::Slin16, &[0u8; 641]), None);
    }

    // --- EdgeAudio (iax-4348) --------------------------------------------

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn edge_audio_passthrough_when_rates_match() {
        let mut edge = EdgeAudio::new(8000);
        let pcm: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        let wire = edge.encode(VoiceFormat::Slin, &pcm);
        assert_eq!(wire.len(), 320);
        assert_eq!(edge.decode(VoiceFormat::Slin, &wire), Some(pcm));
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn edge_audio_downsamples_tx_for_narrowband_wire_on_wideband_bus() {
        let mut edge = EdgeAudio::new(16000);
        // Push several 20 ms bus frames (320 samples of a 1 kHz tone @ 16 kHz);
        // the resampler has ~CHUNK buffering, so sizes converge over frames.
        let mut total_wire = 0usize;
        let frames = 50usize;
        for f in 0..frames {
            let pcm: Vec<i16> = (0..320)
                .map(|i| {
                    let t = (f * 320 + i) as f64 / 16000.0;
                    (8000.0 * (std::f64::consts::TAU * 1000.0 * t).sin()) as i16
                })
                .collect();
            total_wire += edge.encode(VoiceFormat::G711U, &pcm).len();
        }
        // 50 frames * 320 samples @ 2:1 → ~8000 µ-law bytes, minus resampler latency.
        let expected = frames * 160;
        assert!(
            total_wire > expected - 400 && total_wire <= expected,
            "wire bytes {total_wire} not ~{expected}"
        );
    }

    #[test]
    fn edge_audio_upsamples_rx_from_narrowband_wire_on_wideband_bus() {
        let mut edge = EdgeAudio::new(16000);
        let mut total_bus = 0usize;
        for _ in 0..50 {
            let wire = vec![astar_codec::g711::ulaw_encode(8000); 160];
            if let Some(pcm) = edge.decode(VoiceFormat::G711U, &wire) {
                total_bus += pcm.len();
            }
        }
        let expected = 50 * 320;
        assert!(
            total_bus > expected - 800 && total_bus <= expected,
            "bus samples {total_bus} not ~{expected}"
        );
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn edge_audio_wideband_wire_passthrough_on_wideband_bus() {
        let mut edge = EdgeAudio::new(16000);
        let pcm: Vec<i16> = (0..320).map(|i| (i * 50) as i16).collect();
        let wire = edge.encode(VoiceFormat::Slin16, &pcm);
        assert_eq!(wire.len(), 640);
        assert_eq!(edge.decode(VoiceFormat::Slin16, &wire), Some(pcm));
    }

    /// Mean-square power (energy per sample) of a `tone_hz` sine passed through
    /// the 16 kHz→8 kHz downsample edge, measured on the 8 kHz `Slin` wire after
    /// skipping warm-up frames. Returns `(input_ms, wire_ms)`; `Slin` avoids the
    /// companding noise a µ-law wire would add to the measurement.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn downsample_power(tone_hz: f64) -> (f64, f64) {
        let mut edge = EdgeAudio::new(16000);
        let (mut in_ss, mut in_n) = (0.0_f64, 0usize);
        let (mut out_ss, mut out_n) = (0.0_f64, 0usize);
        let frames = 60usize;
        for f in 0..frames {
            let pcm: Vec<i16> = (0..320)
                .map(|i| {
                    let t = (f * 320 + i) as f64 / 16000.0;
                    (0.3 * 32767.0 * (std::f64::consts::TAU * tone_hz * t).sin()) as i16
                })
                .collect();
            let wire = edge.encode(VoiceFormat::Slin, &pcm);
            if f >= 20 {
                // Skip biquad + resampler warm-up.
                for &s in &pcm {
                    in_ss += (f64::from(s) / 32768.0).powi(2);
                    in_n += 1;
                }
                for s in decode_voice(VoiceFormat::Slin, &wire).unwrap() {
                    out_ss += (f64::from(s) / 32768.0).powi(2);
                    out_n += 1;
                }
            }
        }
        (in_ss / in_n as f64, out_ss / out_n as f64)
    }

    /// iax-a9b9: a 7 kHz tone is above the 4 kHz Nyquist of an 8 kHz codec.
    /// Without the pre-filter the linear resampler folds it to 1 kHz — square in
    /// the voice band. Since 7 kHz cannot legitimately survive at 8 kHz, all
    /// wire energy is alias, and the anti-alias low-pass must drop it ≥ 20 dB.
    #[test]
    fn edge_audio_antialiases_downsample_suppresses_fold() {
        let (in_ms, out_ms) = downsample_power(7000.0);
        let atten_db = 10.0 * (out_ms / in_ms).log10();
        assert!(
            atten_db < -20.0,
            "7 kHz fold not suppressed: alias power only {atten_db:.1} dB below input"
        );
    }

    /// The anti-alias low-pass must NOT eat the voice band: a 1 kHz tone through
    /// the same downsample survives within ~2 dB (mean-square power is
    /// rate-agnostic, so half the sample count does not skew it).
    #[test]
    fn edge_audio_downsample_preserves_voice_band() {
        let (in_ms, out_ms) = downsample_power(1000.0);
        let loss_db = 10.0 * (out_ms / in_ms).log10();
        assert!(
            loss_db > -2.0,
            "1 kHz voice tone lost too much through downsample: {loss_db:.1} dB"
        );
    }
}
