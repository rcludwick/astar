// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! P25 Phase 1 voice: the software IMBE vocoder.
//!
//! P25 Phase 1 carries speech as full-rate IMBE: every 20 ms of 8 kHz audio
//! is one 88-bit frame, which the reflector protocol sends as 11 bytes. This
//! module turns those 11 bytes back into 160 samples with
//! `vendor/imbe-vocoder`, Pavel Yazev's fixed-point `imbe_vocoder` from OP25
//! ported to Rust bit for bit (GPL-3.0-or-later, a licence island; see
//! `LICENSE-EXCEPTIONS.md` section 15).
//!
//! **Software only, P25 Phase 1 only.** Rob decided on 2026-11-21 that P25
//! uses this software vocoder and nothing else: the DVSI sticks astar drives
//! cannot do P25 full rate (DVSI lists it as "NO" for every AMBE-3000 and
//! AMBE-3003 part), and every patent that reads on full-rate IMBE has
//! expired. There is no dongle path to choose, so there is no switch.
//!
//! **No AMBE+2, now or later.** P25 Phase 2 is AMBE+2 half rate, which reads
//! on US 8,359,197 until 2028-05-20. It is not in this module or in the
//! vendored crate, and it never will be: like DMR, NXDN and System Fusion it
//! stays hardware-only (`ci/guard-p25-imbe.sh`).
//!
//! **No encryption.** Nothing here decrypts anything. An encrypted call is
//! muted a layer up, from its ALGID, and its frames never reach this decoder.

pub use imbe_vocoder::ImbeEncoder;

/// Bytes in one IMBE frame on the wire (88 bits).
pub const FRAME_BYTES: usize = imbe_vocoder::FRAME_BYTES;

/// Samples in one decoded frame: 20 ms at 8 kHz.
pub const FRAME_SAMPLES: usize = imbe_vocoder::FRAME_SAMPLES;

/// The IMBE silence frame `MMDVMHost` and `P25Gateway` send when they have no
/// voice. It decodes to near silence.
pub const NULL_FRAME: [u8; FRAME_BYTES] = [
    0x04, 0x0C, 0xFD, 0x7B, 0xFB, 0x7D, 0xF2, 0x7B, 0x3D, 0x9E, 0x45,
];

/// Which backend decodes P25. There is only one.
pub const BACKEND: &str = "software";

/// Gain, in dB, applied to every decoded sample (with saturation). Zero for
/// now.
///
/// Measured on 2026-11-21: a 1 kHz sine at -20 dBFS RMS, through
/// [`ImbeEncoder`] and the untrimmed decoder, came back at -24.9 dBFS (100
/// frames, the first 10 skipped while the decoder settles). That suggests a
/// trim of about +5 dB, so P25 would play as loud as the DVSI decode makes
/// NXDN. But the loop runs through Yazev's own encoder, so some or all of the
/// 4.9 dB may be the encoder's loss, not the decoder's; real radios encode
/// with DVSI's IMBE, and a +5 dB trim could play that traffic 5 dB hot and
/// hard-clip its peaks. The trim waits for a check against real P25 traffic
/// (a reflector or off-air capture).
pub const RX_TRIM_DB: f32 = 0.0;

/// `10^(RX_TRIM_DB / 20)`, the linear factor [`RX_TRIM_DB`] means, written
/// out because `powf` is not `const`. A test pins the two together.
const RX_TRIM_GAIN: f32 = 1.0;

/// The level `a_tone_keeps_its_level_through_encode_and_soft_decode`
/// measured through the untrimmed encode/decode loop, in dBFS, for a
/// -20 dBFS tone.
#[cfg(test)]
const MEASURED_LOOP_DBFS: f32 = -24.9;

/// A P25 voice decoder: 11-byte IMBE frames in, 160 samples out.
pub trait ImbeVoice: Send {
    /// Decodes one frame into 20 ms of 8 kHz audio.
    fn decode(&mut self, frame: &[u8; FRAME_BYTES]) -> [i16; FRAME_SAMPLES];

    /// Forgets everything the decoder carried over from the last frames, so
    /// a new over starts as a fresh decoder would.
    fn reset(&mut self);
}

/// The software IMBE decoder, with [`RX_TRIM_DB`] applied.
#[derive(Clone, Debug, Default)]
pub struct SoftImbe {
    decoder: imbe_vocoder::ImbeDecoder,
}

impl SoftImbe {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> SoftImbe {
        SoftImbe::default()
    }
}

impl ImbeVoice for SoftImbe {
    fn decode(&mut self, frame: &[u8; FRAME_BYTES]) -> [i16; FRAME_SAMPLES] {
        self.decoder.decode(frame).map(trim)
    }

    fn reset(&mut self) {
        self.decoder = imbe_vocoder::ImbeDecoder::new();
    }
}

/// One sample with [`RX_TRIM_DB`] applied, saturating at the `i16` range.
fn trim(s: i16) -> i16 {
    // `as` saturates a float to the integer's range rather than wrapping.
    #[allow(clippy::cast_possible_truncation)]
    let out = (f32::from(s) * RX_TRIM_GAIN).round() as i16;
    out
}

/// Opens the P25 decoder. Always the software one: P25 has no dongle path.
#[must_use]
pub fn open_imbe() -> Box<dyn ImbeVoice> {
    Box::new(SoftImbe::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `frames` frames of a sine at `hz`, at `dbfs` RMS.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn tone_frames(hz: f32, dbfs: f32, frames: usize) -> Vec<[i16; FRAME_SAMPLES]> {
        let amp = 32768.0 * 10f32.powf(dbfs / 20.0) * std::f32::consts::SQRT_2;
        let step = std::f32::consts::TAU * hz / 8000.0;
        (0..frames)
            .map(|f| {
                std::array::from_fn(|i| {
                    let n = (f * FRAME_SAMPLES + i) as f32;
                    (amp * (step * n).sin()).round() as i16
                })
            })
            .collect()
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn rms_dbfs(pcm: &[i16]) -> f32 {
        let sum: f64 = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
        let rms = (sum / pcm.len() as f64).sqrt();
        (20.0 * (rms.max(1e-9) / 32768.0).log10()) as f32
    }

    #[test]
    fn a_tone_keeps_its_level_through_encode_and_soft_decode() {
        let mut enc = ImbeEncoder::new();
        let mut dec = SoftImbe::new();
        let tone = tone_frames(1000.0, -20.0, 100);
        let out: Vec<i16> = tone
            .iter()
            .flat_map(|f| dec.decode(&enc.encode(f)))
            .collect();
        let got = rms_dbfs(&out[FRAME_SAMPLES * 10..]);
        // The loop loses 4.9 dB on its own (see RX_TRIM_DB); the trim adds
        // back what it adds back.
        let want = MEASURED_LOOP_DBFS + RX_TRIM_DB;
        assert!(
            (got - want).abs() <= 0.5,
            "decoded at {got} dBFS, want {want}"
        );
    }

    #[test]
    fn the_trim_saturates_rather_than_wrapping() {
        assert_eq!(trim(i16::MAX), i16::MAX);
        assert_eq!(trim(i16::MIN), i16::MIN);
        assert_eq!(trim(0), 0);
        let up = f32::from(trim(1000)) / 1000.0;
        assert!((20.0 * up.log10() - RX_TRIM_DB).abs() < 0.01, "{up}");
    }

    #[test]
    fn the_trim_gain_is_the_trim_in_db() {
        let gain = 10f32.powf(RX_TRIM_DB / 20.0);
        assert!(
            (RX_TRIM_GAIN - gain).abs() < 1e-4,
            "{RX_TRIM_GAIN} vs {gain}"
        );
    }

    #[test]
    fn open_imbe_is_the_software_decoder() {
        let mut opened = open_imbe();
        let mut soft = SoftImbe::new();
        assert_eq!(opened.decode(&NULL_FRAME), soft.decode(&NULL_FRAME));
        assert_eq!(BACKEND, "software");
    }

    #[test]
    fn the_null_frame_decodes_to_near_silence() {
        let mut dec = SoftImbe::new();
        let out: Vec<i16> = (0..20).flat_map(|_| dec.decode(&NULL_FRAME)).collect();
        assert!(rms_dbfs(&out) < -30.0, "{} dBFS", rms_dbfs(&out));
    }

    #[test]
    fn reset_forgets_the_previous_over() {
        let mut enc = ImbeEncoder::new();
        let mut used = SoftImbe::new();
        for f in tone_frames(1000.0, -20.0, 20) {
            let _ = used.decode(&enc.encode(&f));
        }
        used.reset();
        let mut fresh = SoftImbe::new();
        for _ in 0..5 {
            assert_eq!(used.decode(&NULL_FRAME), fresh.decode(&NULL_FRAME));
        }
    }
}
