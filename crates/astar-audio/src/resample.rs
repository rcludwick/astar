// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Single-channel streaming resampler bridging a cpal device rate to/from
//! IAX2's 8 kHz voice rate.
//!
//! # Why `rubato` (not `dasp_signal`)
//!
//! `rubato`'s `FastFixedIn` resampler with a polynomial interpolator is
//! good enough for voice: G.711 telephony spec is 8 kHz × 8 bit µ-law,
//! roughly 3.4 kHz of usable bandwidth. Linear/cubic polynomial is well
//! below the noise floor at that band-limit. `rubato` also has a stable
//! 0.16 API, has been in async-audio production use, and exposes a clean
//! "push chunks of input, drain chunks of output" model that matches the
//! cpal callback pattern.
//!
//! `dasp_signal` is excellent for offline DSP but its `from_hz_to_hz`
//! iterator is awkward to drive from a `&mut [f32]` callback because it
//! consumes a `Signal`. The bookkeeping to bridge that into a streaming
//! callback would be more code than wrapping `rubato` directly.
//!
//! # Streaming wrapper
//!
//! `rubato::FastFixedIn` requires fixed-size input chunks. cpal callbacks
//! are not fixed-size. We buffer input until we have enough, run the
//! resampler, append the output to an output buffer, and drain on demand.
//! Allocation happens on construction; the steady-state path does no
//! heap traffic.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless
)]

use rubato::{
    FastFixedIn, PolynomialDegree, Resampler, SincFixedIn, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use crate::AudioError;

/// Chunk size (in input frames) passed to `FastFixedIn`. 256 is a
/// reasonable balance for voice: ~5 ms at 48 kHz input or ~32 ms at
/// 8 kHz input. Small enough to not introduce noticeable latency, large
/// enough that per-chunk overhead is negligible.
const CHUNK: usize = 256;

/// Single-channel streaming resampler.
///
/// Use [`Resampler1::push`] to feed input samples and
/// [`Resampler1::drain`] to take whatever output is currently available.
pub struct Resampler1 {
    inner: FastFixedIn<f32>,
    /// Input samples per resampler pass (see [`Self::with_chunk`]).
    chunk: usize,
    /// Accumulates input frames until we reach `chunk`.
    in_buf: Vec<f32>,
    /// Holds output frames waiting for the caller to drain.
    out_buf: Vec<f32>,
    /// Scratch buffers reused on each `process_into_buffer` call.
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
}

impl Resampler1 {
    /// Construct a new resampler converting `from_rate` → `to_rate` with the
    /// default device-bridge chunk ([`CHUNK`]).
    pub fn new(from_rate: u32, to_rate: u32) -> Result<Self, AudioError> {
        Self::with_chunk(from_rate, to_rate, CHUNK)
    }

    /// Construct with an explicit processing chunk (input samples per
    /// resampler pass). Callers with fixed-size framing (the codec edge's
    /// 20 ms frames, iax-62ac) pass their frame size so one frame in yields
    /// exactly one rate-converted frame out; the default [`CHUNK`] would
    /// straddle frame boundaries and emit short/long/empty frames.
    pub fn with_chunk(from_rate: u32, to_rate: u32, chunk: usize) -> Result<Self, AudioError> {
        if from_rate == 0 || to_rate == 0 || chunk == 0 {
            return Err(AudioError::Resampler(format!(
                "invalid rates {from_rate} -> {to_rate}"
            )));
        }
        let ratio = f64::from(to_rate) / f64::from(from_rate);
        let inner = FastFixedIn::new(
            ratio,
            1.0, // no dynamic ratio changes
            PolynomialDegree::Linear,
            chunk,
            1, // single channel
        )
        .map_err(|e| AudioError::Resampler(e.to_string()))?;

        // FastFixedIn output size depends on ratio. Pre-size scratch to
        // the worst case (ratio*chunk + a few samples of slack).
        let max_out = ((chunk as f64) * ratio).ceil() as usize + 16;
        Ok(Self {
            inner,
            chunk,
            in_buf: Vec::with_capacity(chunk * 2),
            out_buf: Vec::with_capacity(max_out * 4),
            scratch_in: vec![vec![0.0; chunk]],
            scratch_out: vec![vec![0.0; max_out]],
        })
    }

    /// Push input samples. May internally run the resampler one or more
    /// times if enough input has accumulated. Output samples become
    /// available via [`Resampler1::drain`].
    pub fn push(&mut self, input: &[f32]) -> Result<(), AudioError> {
        self.in_buf.extend_from_slice(input);
        while self.in_buf.len() >= self.chunk {
            // Copy one chunk into scratch input.
            self.scratch_in[0].clear();
            self.scratch_in[0].extend_from_slice(&self.in_buf[..self.chunk]);

            // Run the resampler. The output channel buffer must have
            // capacity for the produced frames; rubato will resize if
            // needed when using `process_into_buffer` with a Vec.
            let (consumed, produced) = self
                .inner
                .process_into_buffer(&self.scratch_in, &mut self.scratch_out, None)
                .map_err(|e| AudioError::Resampler(e.to_string()))?;

            // FastFixedIn always consumes the entire chunk.
            debug_assert_eq!(consumed, self.chunk);

            self.out_buf
                .extend_from_slice(&self.scratch_out[0][..produced]);

            // Drop consumed frames.
            self.in_buf.drain(..self.chunk);
        }
        Ok(())
    }

    /// Drain up to `out.len()` samples into `out`. Returns the number of
    /// samples actually written; trailing entries are left untouched.
    pub fn drain(&mut self, out: &mut [f32]) -> usize {
        let take = self.out_buf.len().min(out.len());
        out[..take].copy_from_slice(&self.out_buf[..take]);
        self.out_buf.drain(..take);
        take
    }

    /// Drain all currently buffered output, returning it as a `Vec`. Handy
    /// for tests; not used on the hot path.
    #[must_use]
    pub fn drain_all(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.out_buf)
    }
}

/// Input frames per resampler pass for the anti-aliased device resampler.
const AA_CHUNK: usize = 256;

/// Anti-aliased single-channel streaming resampler (iax-6945). Same push/drain
/// shell as [`Resampler1`], but wraps rubato `SincFixedIn` (windowed-sinc) so
/// downsampling filters out-of-band content instead of folding it into the
/// band. Used on the cpal device capture/playback paths (48k↔16k wideband),
/// where the sinc group delay is acceptable as constant latency. NOT for the
/// codec edge, which needs the zero-delay one-frame-in/one-frame-out of
/// [`Resampler1`].
pub struct AntiAliasResampler {
    inner: SincFixedIn<f32>,
    chunk: usize,
    /// `from_rate == to_rate`: skip the sinc filter entirely (a 1:1
    /// `SincFixedIn` still filters/delays, which would fail a true
    /// passthrough — see `antialias_passthrough_when_rates_match`).
    passthrough: bool,
    in_buf: Vec<f32>,
    out_buf: Vec<f32>,
    scratch_in: Vec<Vec<f32>>,
    scratch_out: Vec<Vec<f32>>,
}

impl AntiAliasResampler {
    /// Construct a resampler converting `from_rate` → `to_rate`.
    pub fn new(from_rate: u32, to_rate: u32) -> Result<Self, AudioError> {
        if from_rate == 0 || to_rate == 0 {
            return Err(AudioError::Resampler(format!(
                "invalid rates {from_rate} -> {to_rate}"
            )));
        }
        let chunk = AA_CHUNK;
        let ratio = f64::from(to_rate) / f64::from(from_rate);
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let inner = SincFixedIn::<f32>::new(ratio, 1.0, params, chunk, 1)
            .map_err(|e| AudioError::Resampler(e.to_string()))?;
        let max_out = ((chunk as f64) * ratio).ceil() as usize + 16;
        Ok(Self {
            inner,
            chunk,
            passthrough: from_rate == to_rate,
            in_buf: Vec::with_capacity(chunk * 2),
            out_buf: Vec::with_capacity(max_out * 4),
            scratch_in: vec![vec![0.0; chunk]],
            scratch_out: vec![vec![0.0; max_out]],
        })
    }

    /// Push input samples; runs the resampler once per accumulated `chunk`.
    pub fn push(&mut self, input: &[f32]) -> Result<(), AudioError> {
        if self.passthrough {
            self.out_buf.extend_from_slice(input);
            return Ok(());
        }
        self.in_buf.extend_from_slice(input);
        while self.in_buf.len() >= self.chunk {
            self.scratch_in[0].clear();
            self.scratch_in[0].extend_from_slice(&self.in_buf[..self.chunk]);
            // SincFixedIn output size can vary by ±1 per call; ensure scratch
            // is large enough (grows once, then stable).
            let need = self.inner.output_frames_next();
            if self.scratch_out[0].len() < need {
                self.scratch_out[0].resize(need, 0.0);
            }
            let (consumed, produced) = self
                .inner
                .process_into_buffer(&self.scratch_in, &mut self.scratch_out, None)
                .map_err(|e| AudioError::Resampler(e.to_string()))?;
            debug_assert_eq!(consumed, self.chunk);
            self.out_buf
                .extend_from_slice(&self.scratch_out[0][..produced]);
            self.in_buf.drain(..self.chunk);
        }
        Ok(())
    }

    /// Drain up to `out.len()` samples; returns the count written.
    pub fn drain(&mut self, out: &mut [f32]) -> usize {
        let take = self.out_buf.len().min(out.len());
        out[..take].copy_from_slice(&self.out_buf[..take]);
        self.out_buf.drain(..take);
        take
    }

    /// Drain all buffered output.
    #[must_use]
    pub fn drain_all(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.out_buf)
    }
}

/// One-shot anti-aliased resample of a complete buffer (iax-e6f1).
///
/// Windowed-sinc (rubato `SincFixedIn`), so DOWNSAMPLING filters content
/// above the target Nyquist instead of folding it back into the band —
/// which is exactly what [`Resampler1`]'s filterless linear interpolation
/// does (piper's 22.05 kHz voice → 8 kHz aliased audibly: the "overdriven"
/// greeting). Use this for offline material (TTS renders, WAV samples);
/// [`AntiAliasResampler`] covers the realtime device path (iax-6945) with
/// the same sinc filtering in a streaming push/drain shell; keep
/// [`Resampler1`] for the codec edge, where its near-zero latency and
/// zero-delay one-frame-in/one-frame-out matter.
///
/// The sinc filter's group delay is trimmed from the front and the output
/// is capped at `floor(len·ratio)`, so the result is time-aligned with the
/// input.
///
/// # Errors
/// [`AudioError::Resampler`] on zero rates or an internal resampler error.
pub fn resample_offline(
    input: &[f32],
    from_rate: u32,
    to_rate: u32,
) -> Result<Vec<f32>, AudioError> {
    /// Input frames per resampler pass (offline: latency is irrelevant).
    const CHUNK_IN: usize = 1024;

    if from_rate == 0 || to_rate == 0 {
        return Err(AudioError::Resampler(format!(
            "invalid rates {from_rate} -> {to_rate}"
        )));
    }
    if from_rate == to_rate {
        return Ok(input.to_vec());
    }

    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    let ratio = f64::from(to_rate) / f64::from(from_rate);
    let mut rs = SincFixedIn::<f32>::new(ratio, 1.0, params, CHUNK_IN, 1)
        .map_err(|e| AudioError::Resampler(e.to_string()))?;
    let delay = rs.output_delay();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        // audio buffer lengths: well within f64/usize precision
    )]
    let expected = (input.len() as f64 * ratio).floor() as usize;

    let mut out: Vec<f32> = Vec::with_capacity(expected + CHUNK_IN);
    let mut pos = 0usize;
    while pos + CHUNK_IN <= input.len() {
        let chunk = [&input[pos..pos + CHUNK_IN]];
        let o = rs
            .process(&chunk, None)
            .map_err(|e| AudioError::Resampler(e.to_string()))?;
        out.extend_from_slice(&o[0]);
        pos += CHUNK_IN;
    }
    // Tail, then empty passes to flush the filter's delay line.
    let rem = [&input[pos..]];
    let o = rs
        .process_partial(Some(&rem), None)
        .map_err(|e| AudioError::Resampler(e.to_string()))?;
    out.extend_from_slice(&o[0]);
    while out.len() < expected + delay {
        let flush: Option<&[&[f32]]> = None;
        let o = rs
            .process_partial(flush, None)
            .map_err(|e| AudioError::Resampler(e.to_string()))?;
        if o[0].is_empty() {
            break;
        }
        out.extend_from_slice(&o[0]);
    }
    Ok(out.into_iter().skip(delay).take(expected).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Generate a sine wave at `freq_hz` for `n` samples at `rate_hz`.
    fn sine(freq_hz: f32, rate_hz: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (TAU * freq_hz * i as f32 / rate_hz as f32).sin())
            .collect()
    }

    fn rms(buf: &[f32]) -> f32 {
        if buf.is_empty() {
            return 0.0;
        }
        let s: f32 = buf.iter().map(|v| v * v).sum();
        (s / buf.len() as f32).sqrt()
    }

    #[test]
    fn downsample_48k_to_8k_preserves_signal() {
        // 1 second of 1 kHz tone — well within Nyquist for both rates.
        let input = sine(1000.0, 48_000, 48_000);
        let mut r = Resampler1::new(48_000, 8_000).unwrap();
        r.push(&input).unwrap();
        let out = r.drain_all();

        // Allow some lag from the resampler's internal pipeline; we just
        // need a non-trivial chunk of output.
        assert!(out.len() > 7_000, "got only {} output samples", out.len());
        assert!(out.iter().all(|v| v.is_finite()), "non-finite sample");
        let r_rms = rms(&out);
        // A pure 1 kHz sine has RMS = 1/sqrt(2) ~= 0.707. Allow generous
        // tolerance for resampler attenuation / startup transient.
        assert!(r_rms > 0.5, "rms too low: {r_rms}");
        assert!(r_rms < 0.8, "rms too high: {r_rms}");
    }

    #[test]
    fn upsample_8k_to_48k_preserves_signal() {
        let input = sine(500.0, 8_000, 8_000);
        let mut r = Resampler1::new(8_000, 48_000).unwrap();
        r.push(&input).unwrap();
        let out = r.drain_all();
        assert!(out.len() > 40_000, "got only {} output samples", out.len());
        assert!(out.iter().all(|v| v.is_finite()));
        let r_rms = rms(&out);
        assert!(r_rms > 0.5 && r_rms < 0.8, "rms out of range: {r_rms}");
    }

    #[test]
    fn invalid_rate_returns_error() {
        assert!(Resampler1::new(0, 8_000).is_err());
        assert!(Resampler1::new(8_000, 0).is_err());
    }

    #[test]
    fn drain_into_small_buffer_returns_partial() {
        let input = sine(1000.0, 48_000, 4_096);
        let mut r = Resampler1::new(48_000, 8_000).unwrap();
        r.push(&input).unwrap();
        let mut out = [0.0_f32; 16];
        let n = r.drain(&mut out);
        assert_eq!(n, 16);
        assert!(out.iter().all(|v| v.is_finite()));
    }

    fn goertzel_mag(samples: &[f32], freq: f32, rate: f32) -> f32 {
        let n = samples.len();
        if n == 0 {
            return 0.0;
        }
        let k = (freq * n as f32 / rate).round();
        let w = std::f32::consts::TAU * k / n as f32;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in samples {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / n as f32
    }

    #[test]
    fn antialias_suppresses_out_of_band_vs_linear() {
        // A 10 kHz tone at 48 kHz, downsampled to 16 kHz (Nyquist 8 kHz). Linear
        // interpolation folds 10 kHz to 6 kHz (16k - 10k); the sinc resampler
        // filters it out.
        let input: Vec<f32> = (0..48_000)
            .map(|i| (TAU * 10_000.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();

        let mut aa = AntiAliasResampler::new(48_000, 16_000).unwrap();
        aa.push(&input).unwrap();
        let aa_out = aa.drain_all();

        let mut lin = Resampler1::new(48_000, 16_000).unwrap();
        lin.push(&input).unwrap();
        let lin_out = lin.drain_all();

        // Goertzel magnitude at the aliased image (6 kHz) in the 16 kHz output.
        let mag6 = |v: &[f32]| goertzel_mag(v, 6_000.0, 16_000.0);
        let aa6 = mag6(&aa_out);
        let lin6 = mag6(&lin_out);
        let atten_db = 20.0 * (aa6 / lin6.max(1e-9)).log10();
        assert!(
            atten_db < -30.0,
            "sinc must suppress the aliased 6 kHz by >=30 dB vs linear (got {atten_db:.1} dB; aa6={aa6:.5} lin6={lin6:.5})"
        );
    }

    #[test]
    fn antialias_passes_in_band_tone() {
        // 1 kHz at 48 kHz -> 16 kHz: in-band, amplitude preserved within ~1 dB.
        let input: Vec<f32> = (0..48_000)
            .map(|i| (TAU * 1_000.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();
        let mut aa = AntiAliasResampler::new(48_000, 16_000).unwrap();
        aa.push(&input).unwrap();
        let out = aa.drain_all();
        // Measure on the settled middle (skip the filter warm-up).
        let mid = &out[out.len() / 4..out.len() * 3 / 4];
        let mag1 = goertzel_mag(mid, 1_000.0, 16_000.0);
        // A 0.5-amplitude sine has Goertzel mag ~0.25 with this helper; require
        // it within ~1 dB (>= 0.223).
        assert!(mag1 > 0.223, "in-band 1 kHz preserved (got {mag1:.4})");
    }

    #[test]
    fn antialias_passthrough_when_rates_match() {
        let input: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut aa = AntiAliasResampler::new(16_000, 16_000).unwrap();
        aa.push(&input).unwrap();
        assert_eq!(aa.drain_all(), input);
    }

    #[test]
    fn antialias_chunked_equals_single_push() {
        let input: Vec<f32> = (0..9600).map(|i| (i as f32 * 0.03).sin() * 0.4).collect();
        let mut a = AntiAliasResampler::new(48_000, 16_000).unwrap();
        a.push(&input).unwrap();
        let one = a.drain_all();
        let mut b = AntiAliasResampler::new(48_000, 16_000).unwrap();
        for c in input.chunks(97) {
            b.push(c).unwrap();
        }
        let many = b.drain_all();
        assert_eq!(
            one.len(),
            many.len(),
            "same total output regardless of push sizing"
        );
        for (x, y) in one.iter().zip(&many) {
            assert!((x - y).abs() < 1e-6, "chunked output matches single push");
        }
    }
}
