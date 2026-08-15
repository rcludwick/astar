// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Live voice-band spectrum analyzer (iax-e73e): a windowed `rustfft` over the
//! post-resample mic signal (8 kHz or 16 kHz, per the station's configured
//! rate), folded into a fixed ring of LOG-binned dBFS magnitudes with
//! PEAK-HOLD.
//!
//! The monitor tap (iax-2377) feeds [`SpectrumAnalyzer::push`] every capture
//! callback; the analyzer buffers until it has [`FFT_SIZE`] samples, runs a
//! Hann-windowed FFT, maps each linear FFT bin onto a logarithmic frequency
//! axis (`SPECTRUM_LO_HZ`..[`spectrum_hi_hz`]), and keeps the peak magnitude
//! per log bin. Windows OVERLAP — a fresh FFT is recomputed every [`HOP`]
//! samples (~31 Hz @ 8 kHz), decoupled from the [`FFT_SIZE`] resolution
//! window — so the live display stays smooth for a 20 Hz front-end poll
//! (iax-871d). Peak-hold means a transient (or a steady whine during
//! silence) stays visible across polls; the hold decays slowly so the
//! display tracks a falling level instead of latching forever. The
//! front-end polls the snapshot ~20 Hz.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::f32::consts::TAU;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// FFT window length (samples). At 8 kHz this is a 256 ms window — fine
/// resolution for narrowband whine while still refreshing several times a
/// second. This sets the frequency *resolution* only; the *update rate* is
/// decoupled from it via [`HOP`] (overlapping windows).
pub const FFT_SIZE: usize = 2_048;

/// FFT hop (samples between successive windows). Windows OVERLAP by
/// `FFT_SIZE - HOP`, so the spectrum recomputes every `HOP` samples instead of
/// every `FFT_SIZE`. At 8 kHz, `HOP = 256` is 32 ms → ~31.25 FFTs/s (vs the
/// ~3.9 Hz of non-overlapping `FFT_SIZE` hops), giving a smooth live display
/// for front-ends polling at 20 Hz. The frequency resolution is unchanged
/// (still set by [`FFT_SIZE`]).
const HOP: usize = 256;

/// Number of logarithmic display bins. Chosen so the array is small enough to
/// poll cheaply over FFI yet dense enough to draw a smooth curve.
pub const SPECTRUM_BINS: usize = 256;

/// Low edge of the analyzed/displayed band (Hz). Below this is sub-audio rumble.
pub const SPECTRUM_LO_HZ: f32 = 100.0;

/// Upper edge of the displayed band: just under Nyquist, capped at 7.9 kHz.
/// At 8 kHz this is 3 900 Hz (matching the old `SPECTRUM_HI_HZ` const); at
/// 16 kHz (or above) it caps at 7 900 Hz rather than growing unbounded.
#[must_use]
pub fn spectrum_hi_hz(sample_rate: u32) -> f32 {
    (sample_rate as f32 / 2.0 - 100.0).min(7_900.0)
}

/// The dBFS floor reported for an empty/silent bin (and the value bins decay
/// toward). Matches the level-meter floor used elsewhere.
pub const SPECTRUM_FLOOR_DBFS: f32 = -120.0;

/// Default peak-hold decay in dB/SECOND (iax-8616). Stored per-analyzer and
/// converted to a per-FFT step internally (see [`SpectrumAnalyzer::decay_per_fft`]).
/// At [`HOP`] = 256 @ 8 kHz (~31.25 FFTs/s) 800 dB/s ≈ 25.6 dB/FFT — a snappy
/// fall (iax-fbea; was 100 dB/s): a full-scale bin reaches the floor in ~0.15 s,
/// so the hold reads as a brief afterglow instead of a lingering ceiling.
/// Runtime-settable via [`SpectrumAnalyzer::set_decay_db_per_sec`] so a
/// front-end can scrub it live.
pub const DEFAULT_DECAY_DB_PER_SEC: f32 = 800.0;

/// Minimum runtime-settable peak-hold decay (dB/s) — a very slow, near-latching
/// fall.
pub const MIN_DECAY_DB_PER_SEC: f32 = 1.0;
/// Maximum runtime-settable peak-hold decay (dB/s) — an aggressive fall
/// (headroom above the 800 dB/s default, iax-fbea).
pub const MAX_DECAY_DB_PER_SEC: f32 = 1600.0;

/// A windowed-FFT spectrum analyzer with log binning + peak-hold.
pub struct SpectrumAnalyzer {
    sample_rate: u32,
    /// Accumulates input samples until a full [`FFT_SIZE`] window is ready.
    acc: Vec<f32>,
    /// Precomputed Hann window.
    window: Vec<f32>,
    /// Reusable FFT scratch (complex input/output, in place).
    scratch: Vec<Complex<f32>>,
    /// The planned forward FFT.
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    /// Per-log-bin peak-held magnitude in dBFS.
    bins: [f32; SPECTRUM_BINS],
    /// Peak-hold decay in dB/SECOND (iax-8616), runtime-settable via
    /// [`Self::set_decay_db_per_sec`]. Stored in dB/s (front-end-facing units);
    /// converted to a per-FFT step on demand by [`Self::decay_per_fft`].
    decay_db_per_sec: f32,
    /// Total number of FFTs run so far — the spectrum recompute count. Used by
    /// tests to assert the overlapping-window update rate.
    fft_count: u64,
    /// Upper edge of the displayed/analyzed band (Hz), derived from
    /// `sample_rate` at construction via [`spectrum_hi_hz`].
    hi_hz: f32,
}

impl SpectrumAnalyzer {
    /// Build an analyzer for `sample_rate`-Hz mono input.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        Self {
            sample_rate,
            acc: Vec::with_capacity(FFT_SIZE * 2),
            window,
            scratch: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            fft,
            bins: [SPECTRUM_FLOOR_DBFS; SPECTRUM_BINS],
            decay_db_per_sec: DEFAULT_DECAY_DB_PER_SEC,
            fft_count: 0,
            hi_hz: spectrum_hi_hz(sample_rate),
        }
    }

    /// Set the peak-hold decay in dB/SECOND (iax-8616), clamped to
    /// `MIN_DECAY_DB_PER_SEC..=MAX_DECAY_DB_PER_SEC`. Takes effect on the next
    /// folded FFT — a larger value makes held peaks fall faster (track downward
    /// changes more closely), a smaller value holds them longer. The stored
    /// dB/s is converted to a per-FFT step internally so the *time* behavior is
    /// independent of [`HOP`] / `sample_rate`.
    pub fn set_decay_db_per_sec(&mut self, v: f32) {
        self.decay_db_per_sec = v.clamp(MIN_DECAY_DB_PER_SEC, MAX_DECAY_DB_PER_SEC);
    }

    /// Current peak-hold decay in dB/SECOND (front-end-facing units).
    #[must_use]
    pub fn decay_db_per_sec(&self) -> f32 {
        self.decay_db_per_sec
    }

    /// The per-FFT decay step (dB) for the current dB/s setting: each folded FFT
    /// advances `HOP` samples, so there are `sample_rate / HOP` FFTs per second
    /// (~31.25 @ 8 kHz). `db_per_fft = db_per_sec * HOP / sample_rate`.
    fn decay_per_fft(&self) -> f32 {
        self.decay_db_per_sec * HOP as f32 / self.sample_rate as f32
    }

    /// Feed captured samples. Runs an FFT (folding into the peak-held log bins)
    /// for every [`HOP`] samples once a full [`FFT_SIZE`] window is available;
    /// windows OVERLAP by `FFT_SIZE - HOP`, so the spectrum recomputes ~8× more
    /// often than non-overlapping hops would. Partial tails (< `FFT_SIZE`) are
    /// kept for the next call. `acc` stays bounded: each iteration drains `HOP`
    /// while requiring `FFT_SIZE`, so the loop always drives `acc.len()` below
    /// `FFT_SIZE` (the leftover is just the overlap tail).
    pub fn push(&mut self, samples: &[f32]) {
        self.acc.extend_from_slice(samples);
        while self.acc.len() >= FFT_SIZE {
            // Window the leading FFT_SIZE samples into the complex scratch, FFT
            // in place, fold into bins.
            for (dst, (&s, &w)) in self
                .scratch
                .iter_mut()
                .zip(self.acc.iter().zip(self.window.iter()))
            {
                *dst = Complex::new(s * w, 0.0);
            }
            self.fft.process(&mut self.scratch);
            self.fold_into_bins();
            self.fft_count += 1;
            // Advance by one HOP, leaving an FFT_SIZE - HOP overlap tail.
            self.acc.drain(..HOP);
        }
    }

    /// Map the linear FFT magnitudes onto the log frequency axis and update the
    /// peak-held dBFS bins (decay then max).
    fn fold_into_bins(&mut self) {
        let fs = self.sample_rate as f32;
        let half = FFT_SIZE / 2;
        let bin_hz = fs / FFT_SIZE as f32;
        // Normalize FFT magnitude to a 0 dBFS full-scale reference: a full-scale
        // sine through a Hann window produces a bin magnitude of ~FFT_SIZE/4.
        let norm = 4.0 / FFT_SIZE as f32;

        // Accumulate the max linear magnitude landing in each log bin.
        let mut bin_mag = [0.0_f32; SPECTRUM_BINS];
        let mut got = [false; SPECTRUM_BINS];
        let log_lo = SPECTRUM_LO_HZ.ln();
        let log_hi = self.hi_hz.ln();
        for k in 1..half {
            let freq = k as f32 * bin_hz;
            if !(SPECTRUM_LO_HZ..=self.hi_hz).contains(&freq) {
                continue;
            }
            let pos = (freq.ln() - log_lo) / (log_hi - log_lo);
            let idx = ((pos * SPECTRUM_BINS as f32) as usize).min(SPECTRUM_BINS - 1);
            let mag = self.scratch[k].norm() * norm;
            if mag > bin_mag[idx] {
                bin_mag[idx] = mag;
            }
            got[idx] = true;
        }

        // Fill empty log bins from the nearest filled neighbour. At fine bin
        // counts a narrow low-frequency bin can span less than one linear FFT
        // bin (bin_hz) and so catch nothing — without this it would draw as a
        // floor "comb". Forward fill carries the curve rightward; the backward
        // pass covers any leading empties before the first filled bin.
        let mut carry: Option<f32> = None;
        for i in 0..SPECTRUM_BINS {
            if got[i] {
                carry = Some(bin_mag[i]);
            } else if let Some(v) = carry {
                bin_mag[i] = v;
                got[i] = true;
            }
        }
        let mut carry: Option<f32> = None;
        for i in (0..SPECTRUM_BINS).rev() {
            if got[i] {
                carry = Some(bin_mag[i]);
            } else if let Some(v) = carry {
                bin_mag[i] = v;
            }
        }

        // Decay each held peak, then raise it to the new measurement. The
        // per-FFT step is derived from the runtime dB/s setting (iax-8616).
        let decay_db = self.decay_per_fft();
        for (held, &mag) in self.bins.iter_mut().zip(bin_mag.iter()) {
            let new_db = if mag > 1e-9 {
                20.0 * mag.log10()
            } else {
                SPECTRUM_FLOOR_DBFS
            };
            let decayed = (*held - decay_db).max(SPECTRUM_FLOOR_DBFS);
            *held = decayed.max(new_db);
        }
    }

    /// Copy the current peak-held spectrum (dBFS) into `out` (up to `out.len()`
    /// bins) and return the number of bins written.
    pub fn copy_into(&self, out: &mut [f32]) -> usize {
        let n = out.len().min(SPECTRUM_BINS);
        out[..n].copy_from_slice(&self.bins[..n]);
        n
    }

    /// The number of log bins this analyzer produces.
    #[must_use]
    pub fn bin_count(&self) -> usize {
        SPECTRUM_BINS
    }

    /// Total number of FFTs run so far — the spectrum recompute count. With
    /// overlapping windows this grows ~`(N - FFT_SIZE) / HOP + 1` per `N`
    /// pushed samples (once `N >= FFT_SIZE`). Mainly for tests/diagnostics.
    #[must_use]
    pub fn fft_count(&self) -> u64 {
        self.fft_count
    }

    /// Centre frequency (Hz) of log bin `idx` — for tests / labelling. Uses
    /// this analyzer's own upper edge (set from its construction rate), not a
    /// fixed const, so it stays correct at any sample rate.
    #[must_use]
    pub fn bin_center_hz(&self, idx: usize) -> f32 {
        let log_lo = SPECTRUM_LO_HZ.ln();
        let log_hi = self.hi_hz.ln();
        let pos = (idx as f32 + 0.5) / SPECTRUM_BINS as f32;
        (log_lo + pos * (log_hi - log_lo)).exp()
    }

    /// The log bin a given frequency falls into — for tests.
    #[must_use]
    pub fn bin_of_hz(&self, freq: f32) -> usize {
        let log_lo = SPECTRUM_LO_HZ.ln();
        let log_hi = self.hi_hz.ln();
        let pos = (freq.ln() - log_lo) / (log_hi - log_lo);
        ((pos * SPECTRUM_BINS as f32) as usize).min(SPECTRUM_BINS - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, fs: u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (TAU * freq * i as f32 / fs as f32).sin())
            .collect()
    }

    #[test]
    fn spectrum_ceiling_tracks_nyquist() {
        assert!((spectrum_hi_hz(8_000) - 3_900.0).abs() < f32::EPSILON);
        assert!((spectrum_hi_hz(16_000) - 7_900.0).abs() < f32::EPSILON);
    }

    #[test]
    fn synthetic_tone_lands_in_the_expected_log_bin() {
        let mut sa = SpectrumAnalyzer::new(8_000);
        // Drive a few full windows of a 1 kHz tone.
        sa.push(&tone(1_000.0, 8_000, FFT_SIZE * 4, 0.8));
        let mut out = [0.0_f32; SPECTRUM_BINS];
        let n = sa.copy_into(&mut out);
        assert_eq!(n, SPECTRUM_BINS);

        let expect = sa.bin_of_hz(1_000.0);
        // The loudest bin must be the one containing 1 kHz (±1 bin for log
        // quantization / spectral leakage).
        let (loudest, _) = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        assert!(
            loudest.abs_diff(expect) <= 1,
            "1 kHz tone should peak near log bin {expect}, peaked at {loudest}: {out:?}"
        );
        // And it should be well above the floor.
        assert!(
            out[expect] > -30.0,
            "tone bin level too low: {}",
            out[expect]
        );
    }

    #[test]
    fn bins_are_stable_across_polls() {
        let mut sa = SpectrumAnalyzer::new(8_000);
        sa.push(&tone(1_000.0, 8_000, FFT_SIZE * 4, 0.8));
        let mut a = [0.0_f32; SPECTRUM_BINS];
        let mut b = [0.0_f32; SPECTRUM_BINS];
        sa.copy_into(&mut a);
        // A second poll WITHOUT new audio returns the same snapshot (copy is
        // non-destructive; no new FFT ran).
        sa.copy_into(&mut b);
        assert!(
            a.iter()
                .zip(b.iter())
                .all(|(x, y)| x.to_bits() == y.to_bits()),
            "polling must not mutate the held spectrum"
        );
    }

    #[test]
    fn overlapping_windows_recompute_every_hop() {
        // Pushing N >= FFT_SIZE samples yields one FFT per HOP advance:
        // (N - FFT_SIZE) / HOP + 1 FFTs (windows overlap). For N = FFT_SIZE * 4
        // this is far more than the 4 a non-overlapping (FFT_SIZE hop) analyzer
        // would have produced.
        let mut sa = SpectrumAnalyzer::new(8_000);
        let n = FFT_SIZE * 4;
        sa.push(&tone(1_000.0, 8_000, n, 0.8));

        let expected = (n - FFT_SIZE) / HOP + 1;
        assert_eq!(
            sa.fft_count(),
            expected as u64,
            "overlapping windows should recompute every HOP samples"
        );

        // Sanity: many times more than the old non-overlapping cadence (which
        // gave just N / FFT_SIZE FFTs). The ratio approaches FFT_SIZE / HOP = 8
        // for large N; here (N = 4*FFT_SIZE) it is 25 vs 4 ≈ 6.25×.
        let non_overlapping = n / FFT_SIZE;
        assert!(
            sa.fft_count() >= (non_overlapping as u64) * 6,
            "expected several× more FFTs than non-overlapping ({non_overlapping}), got {}",
            sa.fft_count()
        );
    }

    #[test]
    fn accumulator_stays_bounded_across_pushes() {
        // Feeding many small chunks must never let `acc` grow without bound:
        // after each push it should hold strictly less than FFT_SIZE samples
        // (only the overlap tail / partial remainder is retained).
        let mut sa = SpectrumAnalyzer::new(8_000);
        for _ in 0..200 {
            sa.push(&tone(1_000.0, 8_000, 160, 0.5)); // 20 ms chunks
            assert!(
                sa.acc.len() < FFT_SIZE,
                "accumulator grew unbounded: {} >= {FFT_SIZE}",
                sa.acc.len()
            );
        }
    }

    #[test]
    fn transient_is_reflected_within_one_hop() {
        // A loud tone appearing after FFT_SIZE-1 samples of silence should be
        // picked up after only ~HOP further samples (the next overlapping
        // window slides over it), not a whole FFT_SIZE later.
        let mut sa = SpectrumAnalyzer::new(8_000);
        let bin = sa.bin_of_hz(1_000.0);

        // Prime a full window of silence so the loop is "armed".
        sa.push(&vec![0.0_f32; FFT_SIZE]);
        let baseline = sa.fft_count();

        // Now push just over one HOP of a loud tone.
        sa.push(&tone(1_000.0, 8_000, HOP + 1, 0.9));
        assert!(
            sa.fft_count() > baseline,
            "a new FFT should run within one HOP of fresh audio"
        );
        let mut out = [0.0_f32; SPECTRUM_BINS];
        sa.copy_into(&mut out);
        assert!(
            out[bin] > -40.0,
            "transient should be visible within one HOP: bin level {}",
            out[bin]
        );
    }

    /// Drive a tone, then feed `silence_ffts` worth of silence and return how
    /// many dB the 1 kHz bin fell. Shared by the default-rate and
    /// runtime-settable-rate decay tests.
    fn measure_fall(sa: &mut SpectrumAnalyzer, silence_ffts: usize) -> (f32, u64) {
        sa.push(&tone(1_000.0, 8_000, FFT_SIZE * 4, 0.8));
        let bin = sa.bin_of_hz(1_000.0);
        let mut before = [0.0_f32; SPECTRUM_BINS];
        sa.copy_into(&mut before);
        let start = before[bin];
        let start_fft = sa.fft_count();
        sa.push(&vec![0.0_f32; FFT_SIZE + HOP * silence_ffts]);
        let ran = sa.fft_count() - start_fft;
        let mut after = [0.0_f32; SPECTRUM_BINS];
        sa.copy_into(&mut after);
        (start - after[bin], ran)
    }

    #[test]
    fn peak_hold_decay_rate_matches_default_db_per_sec() {
        // With the DEFAULT decay (dB/s), each FFT decays a held peak by
        // decay_db_per_sec * HOP / sample_rate before raising it to the new
        // (silent → floor) measurement. After K silent FFTs a peak should have
        // fallen ~K * that per-FFT step (clamped at the floor).
        let mut sa = SpectrumAnalyzer::new(8_000);
        assert!((sa.decay_db_per_sec() - DEFAULT_DECAY_DB_PER_SEC).abs() < f32::EPSILON);
        let per_fft = DEFAULT_DECAY_DB_PER_SEC * HOP as f32 / 8_000.0;

        let silence_ffts = 5;
        let (fell, ran) = measure_fall(&mut sa, silence_ffts);
        assert!(ran >= silence_ffts as u64);
        // Should have decayed about ran * per_fft (allow slack since the first
        // few overlapping windows still straddle the tone tail).
        assert!(
            fell >= per_fft * silence_ffts as f32 * 0.5,
            "decay too slow: fell {fell} dB over {ran} FFTs (per_fft={per_fft})"
        );
    }

    #[test]
    fn set_decay_db_per_sec_changes_the_observed_fall() {
        // A faster decay (dB/s) must make the held peak fall MORE over the same
        // number of silent FFTs than a slower one (iax-8616). Drive identical
        // input through two analyzers differing only in the runtime decay.
        let silence_ffts = 4;

        let mut slow = SpectrumAnalyzer::new(8_000);
        slow.set_decay_db_per_sec(30.0);
        assert!((slow.decay_db_per_sec() - 30.0).abs() < f32::EPSILON);
        let (slow_fell, _) = measure_fall(&mut slow, silence_ffts);

        let mut fast = SpectrumAnalyzer::new(8_000);
        fast.set_decay_db_per_sec(300.0);
        let (fast_fell, _) = measure_fall(&mut fast, silence_ffts);

        assert!(
            fast_fell > slow_fell + 1.0,
            "faster decay should fall more: fast={fast_fell} dB vs slow={slow_fell} dB"
        );
    }

    #[test]
    fn set_decay_db_per_sec_clamps_to_range() {
        let mut sa = SpectrumAnalyzer::new(8_000);
        sa.set_decay_db_per_sec(-100.0);
        assert!((sa.decay_db_per_sec() - MIN_DECAY_DB_PER_SEC).abs() < f32::EPSILON);
        sa.set_decay_db_per_sec(99_999.0);
        assert!((sa.decay_db_per_sec() - MAX_DECAY_DB_PER_SEC).abs() < f32::EPSILON);
    }

    #[test]
    fn peak_hold_decays_when_the_tone_stops() {
        let mut sa = SpectrumAnalyzer::new(8_000);
        sa.push(&tone(1_000.0, 8_000, FFT_SIZE * 4, 0.8));
        let bin = sa.bin_of_hz(1_000.0);
        let mut peak = [0.0_f32; SPECTRUM_BINS];
        sa.copy_into(&mut peak);
        let loud = peak[bin];

        // Now feed silence: the held peak must decay (fall) over subsequent FFTs.
        sa.push(&vec![0.0_f32; FFT_SIZE * 8]);
        let mut quiet = [0.0_f32; SPECTRUM_BINS];
        sa.copy_into(&mut quiet);
        assert!(
            quiet[bin] < loud,
            "peak-hold must decay during silence: was {loud}, now {}",
            quiet[bin]
        );
    }
}
