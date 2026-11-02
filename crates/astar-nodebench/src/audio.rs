// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Tone in, analysis out: the bench's audio measurements.
//!
//! * [`tone`] generates a test sine as i16 PCM.
//! * [`detect_tone`] finds the dominant tone (a Goertzel scan plus a peak
//!   refine) and reports its frequency, level and SNR.
//! * [`onset`] / [`onset_of_tone`] find the first 20 ms frame above a
//!   threshold, for latency.
//! * [`loss_from_sequence`] / [`loss_from_timestamps`] / [`arrival_gaps`]
//!   count lost frames and stalls over a received-frame timeline.
//! * [`write_wav`] saves a capture as a 16-bit mono WAV artifact.
//!
//! **Level convention.** dBFS here is sine-referenced (AES17): a sine whose
//! peak is full scale (32768) is 0 dBFS. A tone made with
//! `tone(f, -10.0, ..)` is therefore measured back as -10 dBFS.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

/// Duration of one voice frame on both USRP and IAX2.
pub const FRAME_MS: u32 = 20;

/// Full scale for i16 PCM, as used by the dBFS convention above.
const FULL_SCALE: f64 = 32768.0;

/// Samples in one 20 ms frame at `rate`.
#[must_use]
pub fn frame_len(rate: u32) -> usize {
    (rate * FRAME_MS / 1000) as usize
}

/// Convert a sine-referenced dBFS level to a peak amplitude in i16 units.
#[must_use]
pub fn dbfs_to_amplitude(level_dbfs: f64) -> f64 {
    FULL_SCALE * 10f64.powf(level_dbfs / 20.0)
}

/// Convert a peak amplitude in i16 units to sine-referenced dBFS.
#[must_use]
pub fn amplitude_to_dbfs(amplitude: f64) -> f64 {
    if amplitude <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * (amplitude / FULL_SCALE).log10()
    }
}

/// Generate a sine tone: `freq_hz` at `level_dbfs` for `duration`, sampled at
/// `rate` (8000 or 16000 in practice). The level is clamped so the peak never
/// exceeds i16 range.
#[must_use]
pub fn tone(freq_hz: f64, level_dbfs: f64, duration: Duration, rate: u32) -> Vec<i16> {
    let n = samples_for(duration, rate);
    let amp = dbfs_to_amplitude(level_dbfs).min(32767.0);
    let w = 2.0 * std::f64::consts::PI * freq_hz / f64::from(rate);
    (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let v = amp * (w * i as f64).sin();
            #[allow(clippy::cast_possible_truncation)]
            let s = v.round().clamp(-32768.0, 32767.0) as i16;
            s
        })
        .collect()
}

/// Number of samples spanning `duration` at `rate`.
#[must_use]
pub fn samples_for(duration: Duration, rate: u32) -> usize {
    let n = duration.as_nanos() * u128::from(rate) / 1_000_000_000;
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// RMS of `samples` in sine-referenced dBFS (so a full-scale sine reads 0).
#[must_use]
pub fn rms_dbfs(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return f64::NEG_INFINITY;
    }
    let rms = mean_square(samples).sqrt();
    amplitude_to_dbfs(rms * std::f64::consts::SQRT_2)
}

fn mean_square(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = samples.len() as f64;
    samples
        .iter()
        .map(|&s| {
            let v = f64::from(s);
            v * v
        })
        .sum::<f64>()
        / n
}

/// Hann-windowed Goertzel: the estimated peak amplitude (i16 units) of a
/// sinusoid at `freq_hz` in `samples`. The window keeps leakage from a strong
/// neighbouring tone (or a non-integer number of cycles) out of the estimate,
/// and its coherent gain of 0.5 is corrected for.
#[must_use]
pub fn goertzel_amplitude(samples: &[i16], rate: u32, freq_hz: f64) -> f64 {
    let n = samples.len();
    if n < 2 {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * freq_hz / f64::from(rate);
    let coeff = 2.0 * w.cos();
    #[allow(clippy::cast_precision_loss)]
    let denom = (n - 1) as f64;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for (i, &x) in samples.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let win = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / denom).cos();
        let s0 = f64::from(x) * win + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    #[allow(clippy::cast_precision_loss)]
    let half_n = n as f64 * 0.5;
    // |X| = sqrt(power); a sine of amplitude A gives |X| = A·N·cg/2, cg = 0.5.
    2.0 * power.max(0.0).sqrt() / half_n
}

/// The dominant tone in a capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneReport {
    /// Refined frequency of the strongest tone, Hz.
    pub freq_hz: f64,
    /// Its level, sine-referenced dBFS.
    pub level_dbfs: f64,
    /// Tone power over everything else (noise, distortion, other tones), dB.
    /// Capped at 120 dB for a numerically clean synthetic tone.
    pub snr_db: f64,
}

/// Search band for [`detect_tone`]: (lowest, highest) Hz.
pub type Band = (f64, f64);

/// Find the dominant tone in `samples` at `rate`.
///
/// A Goertzel scan over `band` (default 100 Hz up to 200 Hz below Nyquist) in
/// 5 Hz steps finds the strongest bin, then a golden-section search over
/// ±5 Hz refines it to well under 1 Hz on a clean tone. At most the first
/// 2 s of `samples` is analysed — enough resolution for ±2 Hz, and it keeps a
/// long capture cheap. Returns `None` for silence (or fewer than 20 ms).
#[must_use]
pub fn detect_tone(samples: &[i16], rate: u32, band: Option<Band>) -> Option<ToneReport> {
    let max_n = (rate as usize) * 2;
    let x = &samples[..samples.len().min(max_n)];
    if x.len() < frame_len(rate) || x.iter().all(|&s| s == 0) {
        return None;
    }
    let nyquist = f64::from(rate) / 2.0;
    let (lo, hi) = band.unwrap_or((100.0, nyquist - 200.0));
    let (lo, hi) = (lo.max(1.0), hi.min(nyquist - 1.0));
    if hi <= lo {
        return None;
    }

    let step = 5.0;
    let mut best = (lo, 0.0f64);
    let mut freq = lo;
    while freq <= hi {
        let amp = goertzel_amplitude(x, rate, freq);
        if amp > best.1 {
            best = (freq, amp);
        }
        freq += step;
    }

    // The Hann main lobe is only ±2·rate/N wide (±1.25 Hz for 1.6 s at
    // 8 kHz), far narrower than the 5 Hz coarse grid, and outside it the
    // response is a train of side lobes. A golden-section search bracketed
    // by the coarse grid could therefore settle on a side lobe (998 Hz for a
    // clean 1000 Hz tone), and the sinusoid fit at that frequency drifts out
    // of phase over the capture: a clean tone read as -31 dBFS, SNR -21 dB.
    // So scan around the coarse peak at rate/N spacing, which always puts a
    // grid point inside the main lobe, and refine only within that lobe.
    #[allow(clippy::cast_precision_loss)]
    let fine = (f64::from(rate) / x.len() as f64).min(step);
    let (f_lo, f_hi) = ((best.0 - 1.5 * step).max(lo), (best.0 + 1.5 * step).min(hi));
    let mut freq = f_lo;
    while freq <= f_hi {
        let amp = goertzel_amplitude(x, rate, freq);
        if amp > best.1 {
            best = (freq, amp);
        }
        freq += fine;
    }
    let freq_hz = golden_peak(
        |freq| goertzel_amplitude(x, rate, freq),
        (best.0 - fine).max(lo),
        (best.0 + fine).min(hi),
    );
    // Level and SNR from a least-squares sinusoid fit at the refined
    // frequency: exact for a clean tone, and the residual is everything that
    // is not the tone.
    let (amp, residual) = fit_sinusoid(x, rate, freq_hz);
    let tone_power = amp * amp / 2.0;
    let noise = residual.max(tone_power * 1e-12);
    let snr_db = (10.0 * (tone_power / noise).log10()).min(120.0);

    Some(ToneReport {
        freq_hz,
        level_dbfs: amplitude_to_dbfs(amp),
        snr_db,
    })
}

/// Per-frame quality of a tone already found at `freq_hz`: each `frame`
/// samples is fitted on its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameSnr {
    /// Median of the per-frame SNRs, dB.
    pub median_db: f64,
    /// Frames whose fitted tone is under half the median amplitude: a
    /// dropout (a silent or near-silent frame inside the tone).
    pub dropouts: usize,
    pub frames: usize,
}

/// SNR of a tone measured frame by frame, so one dropout does not read as
/// broadband noise. A single silent 20 ms frame in a 1.7 s capture costs a
/// whole-capture sinusoid fit about 19 dB of "SNR" (its residual is 1/84 of
/// the tone power) while every other frame is clean; the dropouts are
/// counted separately. `None` for fewer than one whole frame.
#[must_use]
pub fn frame_snr(samples: &[i16], rate: u32, freq_hz: f64, frame: usize) -> Option<FrameSnr> {
    if frame == 0 || samples.len() < frame {
        return None;
    }
    let fits: Vec<(f64, f64)> = samples
        .chunks_exact(frame)
        .map(|c| {
            let (amp, residual) = fit_sinusoid(c, rate, freq_hz);
            let tone_power = amp * amp / 2.0;
            let noise = residual.max(tone_power * 1e-12).max(1e-12);
            (amp, (10.0 * (tone_power / noise).log10()).min(120.0))
        })
        .collect();
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let amp_med = median(fits.iter().map(|f| f.0).collect());
    Some(FrameSnr {
        median_db: median(fits.iter().map(|f| f.1).collect()),
        dropouts: fits.iter().filter(|f| f.0 < amp_med / 2.0).count(),
        frames: fits.len(),
    })
}

/// Least-squares fit of `a·cos(ωn) + b·sin(ωn)` to `x`: returns the fitted
/// peak amplitude and the mean-square residual (both in i16 units).
fn fit_sinusoid(x: &[i16], rate: u32, freq_hz: f64) -> (f64, f64) {
    let w = 2.0 * std::f64::consts::PI * freq_hz / f64::from(rate);
    let (mut cc, mut ss, mut cs, mut xc, mut xs) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let (sin, cos) = (w * i as f64).sin_cos();
        let v = f64::from(v);
        cc += cos * cos;
        ss += sin * sin;
        cs += cos * sin;
        xc += v * cos;
        xs += v * sin;
    }
    let det = cc * ss - cs * cs;
    if det.abs() < 1e-9 {
        return (0.0, mean_square(x));
    }
    let cos_k = (xc * ss - xs * cs) / det;
    let sin_k = (xs * cc - xc * cs) / det;
    let residual = x
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            #[allow(clippy::cast_precision_loss)]
            let (sin, cos) = (w * i as f64).sin_cos();
            let err = f64::from(v) - (cos_k * cos + sin_k * sin);
            err * err
        })
        .sum::<f64>();
    #[allow(clippy::cast_precision_loss)]
    let n = x.len() as f64;
    (cos_k.hypot(sin_k), residual / n)
}

/// Golden-section search for the maximum of a unimodal `level` on
/// `[lo, hi]`, to 0.01 Hz.
fn golden_peak<F: Fn(f64) -> f64>(level: F, lo: f64, hi: f64) -> f64 {
    let ratio = (5f64.sqrt() - 1.0) / 2.0;
    let (mut left, mut right) = (lo, hi);
    let mut inner_l = right - ratio * (right - left);
    let mut inner_r = left + ratio * (right - left);
    let mut level_l = level(inner_l);
    let mut level_r = level(inner_r);
    while right - left > 0.01 {
        if level_l > level_r {
            right = inner_r;
            inner_r = inner_l;
            level_r = level_l;
            inner_l = right - ratio * (right - left);
            level_l = level(inner_l);
        } else {
            left = inner_l;
            inner_l = inner_r;
            level_l = level_r;
            inner_r = left + ratio * (right - left);
            level_r = level(inner_r);
        }
    }
    f64::midpoint(left, right)
}

/// Where a signal starts in a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Onset {
    /// Index of the first 20 ms frame above threshold.
    pub frame: usize,
    /// Its start, as an offset into the capture.
    pub offset: Duration,
}

fn onset_by<F: Fn(&[i16]) -> f64>(
    samples: &[i16],
    rate: u32,
    threshold_dbfs: f64,
    level: F,
) -> Option<Onset> {
    let len = frame_len(rate);
    if len == 0 {
        return None;
    }
    samples
        .chunks_exact(len)
        .position(|fr| level(fr) >= threshold_dbfs)
        .map(|frame| Onset {
            frame,
            offset: frame_offset(frame),
        })
}

fn frame_offset(frame: usize) -> Duration {
    let ms = u64::try_from(frame).unwrap_or(u64::MAX) * u64::from(FRAME_MS);
    Duration::from_millis(ms)
}

/// First 20 ms frame whose broadband RMS level reaches `threshold_dbfs`.
#[must_use]
pub fn onset(samples: &[i16], rate: u32, threshold_dbfs: f64) -> Option<Onset> {
    onset_by(samples, rate, threshold_dbfs, rms_dbfs)
}

/// First 20 ms frame in which the tone at `freq_hz` (Goertzel, not broadband)
/// reaches `threshold_dbfs` — robust to broadband noise ahead of the tone.
#[must_use]
pub fn onset_of_tone(
    samples: &[i16],
    rate: u32,
    freq_hz: f64,
    threshold_dbfs: f64,
) -> Option<Onset> {
    onset_by(samples, rate, threshold_dbfs, |fr| {
        amplitude_to_dbfs(goertzel_amplitude(fr, rate, freq_hz))
    })
}

/// Frame loss over a received timeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LossReport {
    /// Frames the sender's numbering says were sent between the first and the
    /// last received frame, inclusive.
    pub expected: u64,
    /// Distinct frames received.
    pub received: u64,
    /// `expected - received`.
    pub lost: u64,
    /// Number of separate runs of missing frames.
    pub gaps: u64,
    /// Longest run of missing frames.
    pub max_gap_frames: u64,
    /// Frames received more than once.
    pub duplicates: u64,
    /// Frames that arrived after a later-numbered one.
    pub reordered: u64,
}

impl LossReport {
    /// Fraction of expected frames that were lost, 0.0..=1.0.
    #[must_use]
    pub fn loss_ratio(&self) -> f64 {
        if self.expected == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let r = self.lost as f64 / self.expected as f64;
            r
        }
    }
}

/// Count loss from 32-bit sender sequence numbers (USRP `seq`), in arrival
/// order. Wrap-around is unwrapped relative to the previous frame, so a run
/// spanning `u32::MAX → 0` counts correctly.
#[must_use]
pub fn loss_from_sequence(seqs: &[u32]) -> LossReport {
    let mut ext = Vec::with_capacity(seqs.len());
    let mut last: Option<(u32, i64)> = None;
    for &s in seqs {
        let e = match last {
            None => i64::from(s),
            Some((prev_raw, prev_ext)) => {
                // Signed distance in the 32-bit ring.
                #[allow(clippy::cast_possible_wrap)]
                let d = s.wrapping_sub(prev_raw) as i32;
                prev_ext + i64::from(d)
            }
        };
        last = Some((s, e));
        ext.push(e);
    }
    loss_from_indices(&ext)
}

/// Count loss from sender media timestamps in ms (IAX2), in arrival order:
/// each timestamp is rounded to its 20 ms slot.
#[must_use]
pub fn loss_from_timestamps(ts_ms: &[i64]) -> LossReport {
    let half = i64::from(FRAME_MS) / 2;
    let slots: Vec<i64> = ts_ms
        .iter()
        .map(|t| (t + half).div_euclid(i64::from(FRAME_MS)))
        .collect();
    loss_from_indices(&slots)
}

/// The core counter over already-unwrapped frame indices.
#[must_use]
pub fn loss_from_indices(idx: &[i64]) -> LossReport {
    if idx.is_empty() {
        return LossReport::default();
    }
    let mut r = LossReport::default();
    let mut max_seen = idx[0];
    for &i in &idx[1..] {
        if i < max_seen {
            r.reordered += 1;
        }
        max_seen = max_seen.max(i);
    }
    let mut sorted = idx.to_vec();
    sorted.sort_unstable();
    let before = sorted.len();
    sorted.dedup();
    r.duplicates = (before - sorted.len()) as u64;
    r.received = sorted.len() as u64;
    let span = sorted[sorted.len() - 1] - sorted[0] + 1;
    r.expected = u64::try_from(span).unwrap_or(0);
    r.lost = r.expected.saturating_sub(r.received);
    for w in sorted.windows(2) {
        let missing = u64::try_from(w[1] - w[0] - 1).unwrap_or(0);
        if missing > 0 {
            r.gaps += 1;
            r.max_gap_frames = r.max_gap_frames.max(missing);
        }
    }
    r
}

/// Stalls in an arrival timeline (offsets from any common origin): every
/// inter-arrival interval longer than `threshold`, as (index of the frame that
/// ended the stall, the interval). Use when the sender carries no numbering,
/// or to see jitter the numbering hides.
#[must_use]
pub fn arrival_gaps(arrivals: &[Duration], threshold: Duration) -> Vec<(usize, Duration)> {
    arrivals
        .windows(2)
        .enumerate()
        .filter_map(|(i, w)| {
            let d = w[1].saturating_sub(w[0]);
            (d > threshold).then_some((i + 1, d))
        })
        .collect()
}

/// Encode `samples` as a 16-bit PCM mono WAV.
#[must_use]
pub fn wav_bytes(rate: u32, samples: &[i16]) -> Vec<u8> {
    let data_len = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Write `samples` to `path` as a 16-bit PCM mono WAV (an artifact for a
/// failed run). Creates parent directories.
///
/// # Errors
/// Any I/O error creating or writing the file.
pub fn write_wav(path: &Path, rate: u32, samples: &[i16]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(&wav_bytes(rate, samples))?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift noise generator (no rand dependency).
    struct Noise(u64);
    impl Noise {
        fn next_f64(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            #[allow(clippy::cast_precision_loss)]
            let u = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
            u
        }
        /// Roughly Gaussian (sum of 12 uniforms), unit variance.
        fn gauss(&mut self) -> f64 {
            (0..12).map(|_| self.next_f64()).sum::<f64>() - 6.0
        }
    }

    fn add_noise(x: &[i16], rms: f64, seed: u64) -> Vec<i16> {
        let mut n = Noise(seed);
        x.iter()
            .map(|&s| {
                #[allow(clippy::cast_possible_truncation)]
                let v = (f64::from(s) + rms * n.gauss())
                    .round()
                    .clamp(-32768.0, 32767.0) as i16;
                v
            })
            .collect()
    }

    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn tone_has_the_requested_length_and_peak() {
        let t = tone(1000.0, -6.0, SEC, 8000);
        assert_eq!(t.len(), 8000);
        let peak = t.iter().map(|s| i32::from(*s).abs()).max().unwrap();
        let want = dbfs_to_amplitude(-6.0);
        assert!((f64::from(peak) - want).abs() < want * 0.01, "peak {peak}");
        assert_eq!(
            tone(700.0, 0.0, Duration::from_millis(20), 16000).len(),
            320
        );
    }

    #[test]
    fn rms_of_a_sine_reads_its_sine_referenced_level() {
        let t = tone(1000.0, -12.0, SEC, 8000);
        assert!((rms_dbfs(&t) + 12.0).abs() < 0.1, "{}", rms_dbfs(&t));
    }

    /// A clean tone a little longer than a second used to be read as 998 Hz
    /// at -31 dBFS: the refine step settled on a Hann side lobe, and the
    /// sinusoid fit at that frequency drifted out of phase. Every length the
    /// bench can hand the detector must measure the tone it was given.
    #[test]
    fn long_clean_tones_are_not_read_off_a_side_lobe() {
        for frames in [60usize, 67, 70, 75, 84, 90, 96, 100] {
            let n = frames * 160;
            let t = tone(
                1000.0,
                -10.0,
                Duration::from_millis(20 * frames as u64),
                8000,
            );
            assert_eq!(t.len(), n);
            let r = detect_tone(&t, 8000, Some((800.0, 1200.0))).expect("tone found");
            assert!(
                (r.freq_hz - 1000.0).abs() < 0.2,
                "{frames} frames: {} Hz",
                r.freq_hz
            );
            assert!(
                (r.level_dbfs + 10.0).abs() < 0.5,
                "{frames} frames: {} dBFS",
                r.level_dbfs
            );
            assert!(r.snr_db > 40.0, "{frames} frames: SNR {}", r.snr_db);
        }
    }

    #[test]
    fn one_dropout_is_counted_not_read_as_noise() {
        let mut t = tone(1000.0, -10.0, Duration::from_millis(1680), 8000);
        t[800..960].iter_mut().for_each(|s| *s = 0);
        let whole = detect_tone(&t, 8000, Some((800.0, 1200.0))).unwrap();
        assert!(whole.snr_db < 21.0, "whole-capture fit: {}", whole.snr_db);
        let f = frame_snr(&t, 8000, whole.freq_hz, 160).unwrap();
        assert_eq!(f.frames, 84);
        assert_eq!(f.dropouts, 1);
        assert!(f.median_db > 40.0, "per-frame median {}", f.median_db);
    }

    #[test]
    fn detects_clean_tones_within_two_hz_at_both_rates() {
        for &(rate, f) in &[
            (8000, 1000.0),
            (8000, 700.0),
            (8000, 1234.5),
            (16000, 700.0),
            (16000, 3100.0),
        ] {
            let t = tone(f, -10.0, SEC, rate);
            let r = detect_tone(&t, rate, None).expect("tone found");
            assert!(
                (r.freq_hz - f).abs() < 2.0,
                "rate {rate}: want {f}, got {}",
                r.freq_hz
            );
            assert!((r.level_dbfs + 10.0).abs() < 0.5, "level {}", r.level_dbfs);
            assert!(r.snr_db > 40.0, "snr {}", r.snr_db);
        }
    }

    #[test]
    fn short_captures_still_resolve_frequency() {
        // 200 ms = ten frames; still ±2 Hz on a clean tone.
        let t = tone(1000.0, -10.0, Duration::from_millis(200), 8000);
        let r = detect_tone(&t, 8000, None).unwrap();
        assert!((r.freq_hz - 1000.0).abs() < 2.0, "{}", r.freq_hz);
    }

    #[test]
    fn detection_is_robust_to_noise() {
        // Tone at -10 dBFS, white noise ~20 dB below it.
        let t = tone(1000.0, -10.0, SEC, 8000);
        let tone_rms = dbfs_to_amplitude(-10.0) / std::f64::consts::SQRT_2;
        let noisy = add_noise(&t, tone_rms / 10.0, 0x5eed);
        let r = detect_tone(&noisy, 8000, None).unwrap();
        assert!((r.freq_hz - 1000.0).abs() < 2.0, "{}", r.freq_hz);
        assert!((r.level_dbfs + 10.0).abs() < 1.0, "{}", r.level_dbfs);
        assert!((r.snr_db - 20.0).abs() < 2.0, "snr {}", r.snr_db);

        // Down to 0 dB SNR the frequency still holds.
        let noisier = add_noise(&t, tone_rms, 0xbeef);
        let r = detect_tone(&noisier, 8000, None).unwrap();
        assert!((r.freq_hz - 1000.0).abs() < 2.0, "{}", r.freq_hz);
        assert!(r.snr_db.abs() < 2.0, "snr {}", r.snr_db);
    }

    #[test]
    fn the_strongest_of_two_tones_wins_and_the_band_selects() {
        let a = tone(700.0, -10.0, SEC, 8000);
        let b = tone(1500.0, -20.0, SEC, 8000);
        let mix: Vec<i16> = a
            .iter()
            .zip(&b)
            .map(|(x, y)| x.saturating_add(*y))
            .collect();
        let r = detect_tone(&mix, 8000, None).unwrap();
        assert!((r.freq_hz - 700.0).abs() < 2.0);
        assert!((r.snr_db - 10.0).abs() < 1.0, "snr {}", r.snr_db);
        let r = detect_tone(&mix, 8000, Some((1200.0, 2000.0))).unwrap();
        assert!((r.freq_hz - 1500.0).abs() < 2.0);
        assert!((r.level_dbfs + 20.0).abs() < 0.5);
    }

    #[test]
    fn silence_and_tiny_captures_have_no_tone() {
        assert!(detect_tone(&[0; 8000], 8000, None).is_none());
        assert!(detect_tone(&[100; 10], 8000, None).is_none());
    }

    #[test]
    fn onset_finds_the_first_loud_frame() {
        let mut x = vec![0i16; 160 * 7];
        x.extend(tone(1000.0, -10.0, Duration::from_millis(200), 8000));
        let o = onset(&x, 8000, -40.0).unwrap();
        assert_eq!(o.frame, 7);
        assert_eq!(o.offset, Duration::from_millis(140));
        assert!(onset(&[0; 1600], 8000, -40.0).is_none());
    }

    #[test]
    fn tone_onset_ignores_broadband_noise_ahead_of_the_tone() {
        // 300 ms of -30 dBFS noise, then the tone. Broadband onset fires on the
        // noise; the tone-selective onset waits for the tone.
        let noise_rms = dbfs_to_amplitude(-30.0) / std::f64::consts::SQRT_2;
        let mut x = add_noise(&vec![0i16; 2400], noise_rms, 7);
        x.extend(add_noise(
            &tone(1000.0, -10.0, Duration::from_millis(400), 8000),
            noise_rms,
            9,
        ));
        assert_eq!(onset(&x, 8000, -40.0).unwrap().frame, 0);
        let o = onset_of_tone(&x, 8000, 1000.0, -25.0).unwrap();
        assert_eq!(o.frame, 15, "{o:?}");
        assert_eq!(o.offset, Duration::from_millis(300));
    }

    #[test]
    fn sequence_loss_counts_gaps_dups_and_reorders() {
        let r = loss_from_sequence(&[10, 11, 12, 15, 16, 16, 18, 17, 19]);
        assert_eq!(r.expected, 10);
        assert_eq!(r.received, 8);
        assert_eq!(r.lost, 2);
        assert_eq!(r.gaps, 1);
        assert_eq!(r.max_gap_frames, 2);
        assert_eq!(r.duplicates, 1);
        assert_eq!(r.reordered, 1);
        assert!((r.loss_ratio() - 0.2).abs() < 1e-9);
        assert_eq!(loss_from_sequence(&[]), LossReport::default());
    }

    #[test]
    fn sequence_loss_survives_wraparound() {
        let r = loss_from_sequence(&[u32::MAX - 1, u32::MAX, 1, 2]);
        assert_eq!(r.expected, 5);
        assert_eq!(r.lost, 1);
        assert_eq!(r.gaps, 1);
    }

    #[test]
    fn timestamp_loss_rounds_to_slots() {
        // 20 ms cadence with jitter, slots 3 and 6–7 missing.
        let r = loss_from_timestamps(&[0, 21, 39, 81, 99, 161, 180]);
        assert_eq!(r.expected, 10);
        assert_eq!(r.received, 7);
        assert_eq!(r.lost, 3);
        assert_eq!(r.gaps, 2);
        assert_eq!(r.max_gap_frames, 2);
    }

    #[test]
    fn arrival_gaps_report_stalls() {
        let a: Vec<Duration> = [0u64, 20, 40, 150, 170, 171, 300]
            .iter()
            .map(|&m| Duration::from_millis(m))
            .collect();
        let g = arrival_gaps(&a, Duration::from_millis(60));
        assert_eq!(
            g,
            vec![
                (3, Duration::from_millis(110)),
                (6, Duration::from_millis(129))
            ]
        );
    }

    #[test]
    fn wav_has_a_valid_header_and_payload() {
        let b = wav_bytes(8000, &[1, -2, 3]);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&b[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 8000);
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 16000);
        assert_eq!(&b[36..40], b"data");
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 6);
        assert_eq!(&b[44..], &[1, 0, 0xfe, 0xff, 3, 0]);
        assert_eq!(b.len(), 50);
    }

    #[test]
    fn write_wav_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("nodebench-wav-{}", std::process::id()));
        let p = dir.join("sub/tone.wav");
        write_wav(
            &p,
            16000,
            &tone(700.0, -10.0, Duration::from_millis(20), 16000),
        )
        .unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 44 + 640);
        let _ = std::fs::remove_dir_all(dir);
    }
}
