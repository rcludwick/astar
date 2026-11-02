// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The two stream encoders. Each takes one 20 ms frame of mono PCM at the
//! pipeline rate (8 or 16 kHz) and returns whatever bytes it produced.
//!
//! * **Opus** through `mousiki`, a pure-Rust port of the Xiph reference
//!   codec, MIT-licensed: no C library and no cmake to build. (Not through
//!   `opus2`: its mousiki and libopus backends are mutually exclusive, and
//!   the discord feature already pulls `opus2` with libopus.) VBR, no DTX (a silent VBR frame is a few bytes anyway), one
//!   packet per frame.
//! * **MP3** through vendored LAME (`stream-mp3`, LGPL, never default):
//!   MPEG-2 (16 kHz) or MPEG-2.5 (8 kHz) Layer III at a constant bitrate.
//!   LAME buffers internally, so a frame may produce nothing.

/// One format's encoder.
pub trait Encoder: Send {
    /// One 20 ms frame in; zero or more bytes out.
    fn encode(&mut self, pcm: &[i16]) -> Vec<u8>;
}

/// The page player's encoder.
pub struct OpusEnc {
    inner: opus::OpusEncoder<'static>,
}

use mousiki::c_style_api::opus_encoder as opus;

/// `OPUS_APPLICATION_VOIP`.
const APPLICATION_VOIP: i32 = 2048;

impl OpusEnc {
    /// An Opus encoder at `sample_rate` (8000 or 16000) and `kbps`.
    ///
    /// # Errors
    /// The codec refused the rate or the bitrate.
    pub fn new(sample_rate: u32, kbps: u32) -> Result<Self, String> {
        let rate = i32::try_from(sample_rate).map_err(|_| "opus: sample rate".to_string())?;
        let mut inner = opus::opus_encoder_create(rate, 1, APPLICATION_VOIP)
            .map_err(|e| format!("opus: {e:?}"))?;
        let bits = i32::try_from(kbps * 1000).map_err(|_| "opus: bitrate".to_string())?;
        opus::opus_encoder_ctl(&mut inner, opus::OpusEncoderCtlRequest::SetBitrate(bits))
            .map_err(|e| format!("opus: {e:?}"))?;
        opus::opus_encoder_ctl(&mut inner, opus::OpusEncoderCtlRequest::SetVbr(true))
            .map_err(|e| format!("opus: {e:?}"))?;
        Ok(Self { inner })
    }
}

impl Encoder for OpusEnc {
    fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        let mut out = vec![0u8; 1500];
        match opus::opus_encode(&mut self.inner, pcm, pcm.len(), &mut out) {
            Ok(n) => {
                out.truncate(n);
                out
            }
            Err(_) => Vec::new(),
        }
    }
}

/// The `/stream.mp3` encoder.
#[cfg(feature = "stream-mp3")]
pub struct Mp3Enc {
    inner: mp3lame_encoder::Encoder,
    /// LAME's output not yet cut into whole frames.
    pending: Vec<u8>,
}

/// Layer III bitrates by header index: MPEG-1, then MPEG-2 and 2.5.
const MP3_KBPS_V1: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const MP3_KBPS_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// Length of the MPEG audio Layer III frame whose header starts `h`, or
/// `None` when `h` is not a frame header. MPEG-1 is 144·bitrate/rate
/// bytes, MPEG-2 and 2.5 are 72·bitrate/rate, plus one padding byte.
#[must_use]
pub fn mp3_frame_len(h: &[u8]) -> Option<usize> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0b11; // 0 = 2.5, 2 = 2, 3 = 1
    let layer = (h[1] >> 1) & 0b11; // 1 = Layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let bitrate_idx = usize::from(h[2] >> 4);
    let rate_idx = usize::from((h[2] >> 2) & 0b11);
    let padding = usize::from((h[2] >> 1) & 1);
    if bitrate_idx == 0 || bitrate_idx == 15 || rate_idx == 3 {
        return None;
    }
    let (kbps, rate, coeff) = match version {
        3 => (
            MP3_KBPS_V1[bitrate_idx],
            [44_100, 48_000, 32_000][rate_idx],
            144,
        ),
        2 => (
            MP3_KBPS_V2[bitrate_idx],
            [22_050, 24_000, 16_000][rate_idx],
            72,
        ),
        _ => (
            MP3_KBPS_V2[bitrate_idx],
            [11_025, 12_000, 8_000][rate_idx],
            72,
        ),
    };
    usize::try_from(coeff * kbps * 1000 / rate)
        .ok()
        .map(|n| n + padding)
}

#[cfg(feature = "stream-mp3")]
impl Mp3Enc {
    /// An MP3 encoder at `sample_rate` and a constant `kbps`.
    ///
    /// # Errors
    /// LAME refused the settings.
    pub fn new(sample_rate: u32, kbps: u32) -> Result<Self, String> {
        use mp3lame_encoder::{Bitrate, Builder, Quality};
        let brate = match kbps {
            8 => Bitrate::Kbps8,
            16 => Bitrate::Kbps16,
            24 => Bitrate::Kbps24,
            32 => Bitrate::Kbps32,
            40 => Bitrate::Kbps40,
            48 => Bitrate::Kbps48,
            64 => Bitrate::Kbps64,
            other => return Err(format!("mp3: {other} kbit/s is not offered")),
        };
        let mut b = Builder::new().ok_or("mp3: LAME did not initialise")?;
        b.set_num_channels(1).map_err(|e| format!("mp3: {e}"))?;
        b.set_sample_rate(sample_rate)
            .map_err(|e| format!("mp3: {e}"))?;
        b.set_brate(brate).map_err(|e| format!("mp3: {e}"))?;
        b.set_quality(Quality::Good)
            .map_err(|e| format!("mp3: {e}"))?;
        b.set_to_write_vbr_tag(false)
            .map_err(|e| format!("mp3: {e}"))?;
        Ok(Self {
            inner: b.build().map_err(|e| format!("mp3: {e}"))?,
            pending: Vec::new(),
        })
    }
}

#[cfg(feature = "stream-mp3")]
impl Encoder for Mp3Enc {
    /// Whole frames only: every chunk a listener can start on begins with a
    /// frame header, so a player joining mid-stream needs no resync.
    fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()));
        if let Ok(n) = self
            .inner
            .encode(mp3lame_encoder::MonoPcm(pcm), out.spare_capacity_mut())
        {
            // SAFETY: LAME wrote exactly `n` initialised bytes into the
            // spare capacity it was handed (mp3lame-encoder's contract).
            unsafe { out.set_len(n) };
            self.pending.extend_from_slice(&out);
        }
        let mut cut = 0;
        while let Some(len) = mp3_frame_len(&self.pending[cut..]) {
            if cut + len > self.pending.len() {
                break;
            }
            cut += len;
        }
        if cut == 0 && self.pending.len() > 8192 {
            // Not a frame boundary after 8 KiB: start over rather than grow.
            self.pending.clear();
        }
        self.pending.drain(..cut).collect()
    }
}

#[cfg(test)]
pub(crate) fn tone(rate: u32, hz: f32, ms: u32) -> Vec<i16> {
    (0..(rate * ms / 1000) as usize)
        .map(|i| {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let v = ((i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16;
            v
        })
        .collect()
}

/// Goertzel power of `hz` in `pcm`, in dB (relative; for comparing bins).
#[cfg(test)]
pub(crate) fn goertzel_db(pcm: &[i16], rate: u32, hz: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let w = 2.0 * (std::f32::consts::TAU * hz / rate as f32).cos();
    let (mut a, mut b) = (0.0f32, 0.0f32);
    for &x in pcm {
        let c = f32::from(x) + w * a - b;
        b = a;
        a = c;
    }
    10.0 * (a * a + b * b - w * a * b).max(1e-9).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_round_trips_a_1khz_tone_at_both_rates() {
        for rate in [8_000u32, 16_000] {
            let mut enc = OpusEnc::new(rate, 24).unwrap();
            let mut dec = mousiki::c_style_api::opus_decoder::opus_decoder_create(
                i32::try_from(rate).unwrap(),
                1,
            )
            .unwrap();
            let pcm = tone(rate, 1000.0, 1000);
            let mut out = Vec::new();
            for f in pcm.chunks(rate as usize / 50) {
                let pkt = enc.encode(f);
                assert_ne!(pkt, [] as [u8; 0]);
                let mut buf = vec![0i16; f.len()];
                let n = mousiki::c_style_api::opus_decoder::opus_decode(
                    &mut dec,
                    Some(&pkt),
                    pkt.len(),
                    &mut buf,
                    f.len(),
                    false,
                )
                .unwrap();
                out.extend_from_slice(&buf[..n]);
            }
            let tail = &out[out.len() / 2..];
            assert!(
                goertzel_db(tail, rate, 1000.0) - goertzel_db(tail, rate, 2300.0) > 20.0,
                "rate {rate}"
            );
        }
    }

    #[test]
    fn opus_silence_stays_small() {
        let mut enc = OpusEnc::new(16_000, 24).unwrap();
        let total: usize = (0..50).map(|_| enc.encode(&[0; 320]).len()).sum();
        assert!(total < 50 * 40, "VBR silence is cheap: {total} bytes/s");
    }

    #[cfg(feature = "stream-mp3")]
    #[test]
    fn mp3_emits_valid_frames_for_a_second_of_tone() {
        for rate in [8_000u32, 16_000] {
            let mut enc = Mp3Enc::new(rate, 32).unwrap();
            let pcm = tone(rate, 1000.0, 1000);
            let mut out = Vec::new();
            for f in pcm.chunks(rate as usize / 50) {
                let chunk = enc.encode(f);
                assert!(
                    chunk.is_empty() || mp3_frame_len(&chunk).is_some(),
                    "chunks start on a frame"
                );
                out.extend(chunk);
            }
            assert!(out.len() > 1000, "rate {rate}: {} bytes", out.len());
            let i = out
                .windows(2)
                .position(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)
                .expect("frame sync");
            assert_ne!(out[i + 1] & 0x18, 0x18, "never MPEG-1");
            // Whole frames back to back from the first byte.
            assert_eq!(i, 0, "the stream starts on a frame header");
            let mut at = 0;
            while at < out.len() {
                at += mp3_frame_len(&out[at..]).expect("a header at every frame start");
            }
            assert_eq!(at, out.len());
        }
        assert!(Mp3Enc::new(16_000, 56).is_err());
    }
}
