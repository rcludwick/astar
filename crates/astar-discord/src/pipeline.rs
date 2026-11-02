// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Mono node audio to one Discord voice frame.
//!
//! Discord wants 20 ms of 48 kHz stereo Opus: 960 sample pairs, 1920
//! interleaved i16. The node's conference runs at 8 kHz (160 samples per
//! 20 ms tick) or 16 kHz under slin16 (320). This resamples and duplicates
//! the mono channel; it does no gain, no filtering and no mixing.

use astar_audio::Resampler1;

/// Discord's voice sample rate.
pub const DISCORD_RATE: u32 = 48_000;
/// Sample pairs in one 20 ms Discord frame.
pub const DISCORD_FRAME_PAIRS: usize = 960;

pub struct FramePipeline {
    frame_in: usize,
    resampler: Resampler1,
    scratch_in: Vec<f32>,
    scratch_out: Vec<f32>,
}

impl FramePipeline {
    /// `input_rate` is the post-filter's OUTPUT rate — 8000 or 16000.
    ///
    /// Uses `astar_audio::Resampler1`, the same resampler the codec edge
    /// uses: one node frame in, one Discord frame out, with no chunk
    /// buffering between them. It is NOT zero-delay — rubato's polynomial
    /// interpolator has a lead-in, and the first pass through a fresh
    /// resampler comes up short (measured 948 of 960 samples at 8 kHz, 954 at
    /// 16 kHz). That is what the warm-up below exists for; the constructor
    /// spends the short pass on silence so the stream itself is
    /// frame-for-frame from the first real frame on.
    ///
    /// `AntiAliasResampler` is deliberately NOT used here: it buffers by its
    /// own chunk and carries sinc group delay, so it would not return one
    /// Discord frame per node frame at all.
    ///
    /// # Panics
    /// If `input_rate` does not divide 48000 into a whole 20 ms frame, or the
    /// resampler cannot be built.
    #[must_use]
    pub fn new(input_rate: u32) -> Self {
        let frame_in = (input_rate as usize) / 50;
        assert!(
            frame_in > 0 && (DISCORD_RATE as usize).is_multiple_of(input_rate as usize),
            "input rate {input_rate} does not divide 48 kHz evenly"
        );
        let resampler = Resampler1::with_chunk(input_rate, DISCORD_RATE, frame_in)
            .expect("resampler for a validated rate pair");
        let mut pipeline = Self {
            frame_in,
            resampler,
            scratch_in: vec![0.0; frame_in],
            scratch_out: vec![0.0; DISCORD_FRAME_PAIRS],
        };

        // rubato's polynomial interpolator has a lead-in: the first pass
        // through a freshly built resampler yields ~948 of 960 samples at
        // 8 kHz (~954 at 16 kHz), settling to exactly 960 from the second
        // pass on. Spend that pass here, on silence, so the first REAL frame
        // is full-length instead of having its tail hard-zeroed into a click
        // at the start of every join. Costs one discarded pass at
        // construction and adds no latency to the stream.
        let warmup = vec![0i16; frame_in];
        let _ = pipeline.process(&warmup);
        pipeline
    }

    /// One 20 ms mono frame in, one Discord frame out: 1920 interleaved i16.
    ///
    /// A short or long input frame is padded or truncated to the expected
    /// length rather than rejected, so a ragged source cannot desynchronise
    /// the stream. A resampler error yields a silent frame rather than a gap,
    /// for the same reason. The same padding-with-silence covers a *partial*
    /// drain (`got < DISCORD_FRAME_PAIRS`): only the samples the resampler
    /// actually produced are used, and the rest of the frame is zero rather
    /// than stale scratch data. In steady state `got` is always exactly
    /// `DISCORD_FRAME_PAIRS`; the one case where it legitimately falls short
    /// is the resampler's first pass ever, which is why [`FramePipeline::new`]
    /// spends that pass on a silent warm-up frame instead of a real one.
    pub fn process(&mut self, mono: &[i16]) -> Vec<i16> {
        self.scratch_in.clear();
        self.scratch_in.extend(
            mono.iter()
                .take(self.frame_in)
                .map(|&s| f32::from(s) / 32768.0),
        );
        self.scratch_in.resize(self.frame_in, 0.0);

        let got = if self.resampler.push(&self.scratch_in).is_ok() {
            self.resampler.drain(&mut self.scratch_out)
        } else {
            0
        };

        let mut stereo = Vec::with_capacity(DISCORD_FRAME_PAIRS * 2);
        for i in 0..DISCORD_FRAME_PAIRS {
            let s = if i < got { self.scratch_out[i] } else { 0.0 };
            #[allow(clippy::cast_possible_truncation)]
            let v = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
            stereo.push(v);
            stereo.push(v);
        }
        stereo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_k_mono_becomes_one_discord_frame() {
        let mut p = FramePipeline::new(8000);
        let out = p.process(&[0i16; 160]);
        assert_eq!(
            out.len(),
            DISCORD_FRAME_PAIRS * 2,
            "960 stereo pairs, interleaved"
        );
    }

    #[test]
    fn sixteen_k_mono_becomes_one_discord_frame() {
        let mut p = FramePipeline::new(16000);
        let out = p.process(&[0i16; 320]);
        assert_eq!(out.len(), DISCORD_FRAME_PAIRS * 2);
    }

    #[test]
    fn first_frame_is_full_length_not_zero_padded() {
        // The direct observable proxy for "drain returned a full 960 on the
        // very first real frame": if the resampler's lead-in shortfall (see
        // FramePipeline::new) ever leaks past the warm-up, this is exactly
        // what goes to zero, since a short `got` only ever hard-zeroes the
        // TAIL of the frame.
        let mut p = FramePipeline::new(8000);
        let out = p.process(&[4000i16; 160]);
        let last_pair = &out[out.len() - 2..];
        assert_ne!(
            last_pair,
            [0i16, 0i16],
            "the last sample pair of the first frame must not be zero-padded"
        );
    }

    #[test]
    fn channels_are_duplicated_not_interleaved_garbage() {
        let mut p = FramePipeline::new(8000);
        // A steady DC level survives resampling as roughly itself, and both
        // channels of every pair must be identical regardless of value.
        let out = p.process(&[4000i16; 160]);
        for pair in out.as_chunks::<2>().0 {
            assert_eq!(pair[0], pair[1], "left and right must be the same sample");
        }
    }

    #[test]
    fn a_steady_level_survives_the_resample() {
        // FramePipeline::new spends the resampler's lead-in pass on a silent
        // warm-up frame internally, so the first real frame here is already
        // settled — no extra priming loop needed.
        let mut p = FramePipeline::new(8000);
        let out = p.process(&[4000i16; 160]);
        let mid = out[out.len() / 2];
        assert!(
            (i32::from(mid) - 4000).abs() < 400,
            "steady 4000 should survive within 10%, got {mid}"
        );
    }
}
