// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Cheap peak-amplitude level metering.
//!
//! Computed inside the cpal audio callback once per buffer, then handed to
//! the sink/source via [`InputSink::write`](crate::InputSink::write) /
//! [`OutputSource::meter`](crate::OutputSource::meter). The meter is the
//! maximum absolute sample value, normalised to `0.0..=1.0` (assuming
//! standard f32 PCM clamped at ±1.0).
//!
//! We use peak rather than RMS because:
//! - it's a single branchless `max` in the hot loop;
//! - VU-style mic meters in voice apps want responsiveness to peaks
//!   (the use case here is astar's PTT mic indicator);
//! - downstream UIs can apply their own smoothing.
//!
//! VOX is the exception: it judges [`VoxLevel`], a 50 ms windowed power,
//! because a peak lets a single click key the transmitter.

/// Compute the peak amplitude across the slice. Returns `0.0` for an empty
/// slice. Treats NaN as zero (NaN doesn't compare with `max`).
#[must_use]
pub fn peak(samples: &[f32]) -> f32 {
    let mut p = 0.0_f32;
    for &s in samples {
        let a = s.abs();
        if a > p {
            p = a;
        }
    }
    p
}

/// Convert a peak amplitude in `0.0..=1.0` to dBFS, clamped to `[-60, 0]`.
/// Silence (`<= 1e-6`) and `NaN` floor at `-60`; overdriven input (`> 1.0`)
/// reports `0` dBFS.
#[must_use]
pub fn peak_to_dbfs(peak: f32) -> f32 {
    if peak.is_nan() || peak <= 1e-6 {
        -60.0
    } else {
        (20.0 * peak.log10()).clamp(-60.0, 0.0)
    }
}

/// Length of the VOX loudness window, in milliseconds.
pub const VOX_WINDOW_MS: u32 = 50;
/// The window is kept as this many equal blocks, so it slides in
/// `VOX_WINDOW_MS / VOX_WINDOW_BLOCKS` = 5 ms steps.
const VOX_WINDOW_BLOCKS: usize = 10;

/// The loudness VOX judges: the mean power (mean square) of the input over
/// the last [`VOX_WINDOW_MS`] milliseconds.
///
/// [`peak`] answers "what was the single loudest sample in this capture
/// callback?", which is right for a meter but wrong for a voice gate: one
/// click, pop or Bluetooth codec glitch is as loud as a word, so a gate that
/// keys from it keys on nothing. Averaging the power over 50 ms shrinks a
/// 5 ms click to a tenth of its power (10 dB down) and, more importantly,
/// forgets it entirely 50 ms later. That bound is why this is a sliding
/// window rather than an exponential average: an exponential average's tail
/// lasts longer the louder the click was, so a loud enough click could still
/// hold the level over the threshold for the whole VOX attack time. With a
/// window, a click shorter than `attack − 50 ms` can never key VOX, however
/// loud it is.
///
/// The window is measured in time, not samples: the block length is derived
/// from the sample rate, so the same sound reads the same at 16 kHz and
/// 48 kHz. It holds no heap memory, so it is safe on the audio thread.
#[derive(Debug, Clone)]
pub struct VoxLevel {
    /// Sample rate the block length was derived for (`0` = not yet set).
    rate: u32,
    /// Samples per block at `rate`.
    block_len: u32,
    /// Sum of squares of each completed block, oldest overwritten first.
    blocks: [f32; VOX_WINDOW_BLOCKS],
    /// Next slot in `blocks` to overwrite.
    next: usize,
    /// The block being filled: running sum of squares and sample count.
    acc: f32,
    count: u32,
    /// Mean square over the window as of the last completed block.
    power: f32,
}

impl Default for VoxLevel {
    fn default() -> Self {
        Self::new()
    }
}

impl VoxLevel {
    /// A meter reading silence.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rate: 0,
            block_len: 1,
            blocks: [0.0; VOX_WINDOW_BLOCKS],
            next: 0,
            acc: 0.0,
            count: 0,
            power: 0.0,
        }
    }

    /// Feed one buffer captured at `rate` Hz, each sample scaled by `gain`
    /// (and clipped to ±1, as the peak meter clips). Returns the mean square
    /// over the last 50 ms. A change of rate starts the window afresh.
    pub fn update(&mut self, samples: &[f32], gain: f32, rate: u32) -> f32 {
        if rate != self.rate {
            *self = Self::new();
            self.rate = rate;
            let per_block =
                u64::from(rate) * u64::from(VOX_WINDOW_MS) / (1000 * VOX_WINDOW_BLOCKS as u64);
            self.block_len = u32::try_from(per_block.max(1)).unwrap_or(u32::MAX);
        }
        for &s in samples {
            let v = if s.is_nan() {
                0.0
            } else {
                (gain * s).clamp(-1.0, 1.0)
            };
            self.acc += v * v;
            self.count += 1;
            if self.count == self.block_len {
                self.blocks[self.next] = self.acc;
                self.next = (self.next + 1) % VOX_WINDOW_BLOCKS;
                self.acc = 0.0;
                self.count = 0;
                #[allow(clippy::cast_precision_loss)] // block_len is a few hundred
                let n = (self.block_len as usize * VOX_WINDOW_BLOCKS) as f32;
                self.power = self.blocks.iter().sum::<f32>() / n;
            }
        }
        self.power
    }

    /// The current mean square (`0.0..=1.0`).
    #[must_use]
    pub fn power(&self) -> f32 {
        self.power
    }
}

/// Convert a mean square (power, `0.0..=1.0`) to dBFS, floored at -60 like
/// [`peak_to_dbfs`]. A full-scale sine reads about -3 dBFS.
#[must_use]
pub fn power_to_dbfs(power: f32) -> f32 {
    if power.is_nan() || power <= 1e-12 {
        -60.0
    } else {
        (10.0 * power.log10()).clamp(-60.0, 0.0)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn peak_empty_is_zero() {
        assert_eq!(peak(&[]), 0.0);
    }

    #[test]
    fn peak_picks_largest_absolute() {
        assert!((peak(&[0.1, -0.7, 0.3, -0.5]) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn peak_handles_clamped_signal() {
        assert!((peak(&[1.0, -1.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn peak_ignores_nan() {
        // NaN.abs() is NaN; comparisons against NaN are false, so p stays
        // at the previously-seen real peak.
        let p = peak(&[0.4, f32::NAN, -0.2]);
        assert!((p - 0.4).abs() < 1e-6);
    }

    /// A sine of amplitude `amp` at `freq` Hz, `ms` long, at `rate`.
    #[allow(clippy::cast_precision_loss)]
    fn tone(amp: f32, freq: f32, ms: u32, rate: u32) -> Vec<f32> {
        (0..rate * ms / 1000)
            .map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// Feed `signal` in 10 ms callbacks and return the dBFS after each.
    fn trace(signal: &[f32], rate: u32) -> Vec<f32> {
        let mut m = VoxLevel::new();
        signal
            .chunks((rate / 100) as usize)
            .map(|c| power_to_dbfs(m.update(c, 1.0, rate)))
            .collect()
    }

    #[test]
    fn a_five_ms_click_in_silence_stays_well_below_its_peak_and_is_forgotten() {
        for rate in [16_000, 48_000] {
            let mut sig = vec![0.0_f32; (rate / 10) as usize]; // 100 ms silence
            sig.extend(std::iter::repeat_n(0.5_f32, (rate / 200) as usize)); // 5 ms click
            sig.extend(vec![0.0_f32; (rate / 5) as usize]); // 200 ms silence
            let t = trace(&sig, rate);
            let click_peak_db = peak_to_dbfs(0.5); // ≈ -6 dBFS
            let worst = t.iter().copied().fold(-60.0_f32, f32::max);
            assert!(
                worst <= click_peak_db - 9.0,
                "{rate} Hz: a 5 ms click read {worst} dBFS, its peak is {click_peak_db}"
            );
            // 60 ms after the click ends it has left the window entirely.
            let after = t[(100 + 5 + 60) / 10 + 1];
            assert!(
                (after + 60.0).abs() < 1e-3,
                "{rate} Hz: click still in the window: {after}"
            );
        }
    }

    #[test]
    fn a_sustained_tone_reaches_its_rms_within_150_ms_at_both_rates() {
        let amp = 0.1_f32;
        let rms_db = 20.0 * (amp / std::f32::consts::SQRT_2).log10(); // ≈ -23 dBFS
        let mut answers = Vec::new();
        for rate in [16_000, 48_000] {
            let t = trace(&tone(amp, 440.0, 300, rate), rate);
            let at_150 = t[14];
            assert!(
                (at_150 - rms_db).abs() < 0.5,
                "{rate} Hz: {at_150} dBFS at 150 ms, want ≈ {rms_db}"
            );
            answers.push(at_150);
        }
        assert!(
            (answers[0] - answers[1]).abs() < 0.2,
            "16 kHz and 48 kHz disagree: {answers:?}"
        );
    }

    #[test]
    fn the_gain_scales_the_level_and_clips_like_the_peak_meter() {
        let mut m = VoxLevel::new();
        let p = m.update(&vec![0.25_f32; 800], 2.0, 16_000);
        assert!((power_to_dbfs(p) - peak_to_dbfs(0.5)).abs() < 1e-3);
        let mut m = VoxLevel::new();
        let p = m.update(&vec![0.9_f32; 800], 4.0, 16_000);
        assert!((p - 1.0).abs() < 1e-6, "clipped to full scale, got {p}");
    }

    #[test]
    fn power_to_dbfs_floors_and_caps() {
        assert_eq!(power_to_dbfs(0.0), -60.0);
        assert_eq!(power_to_dbfs(f32::NAN), -60.0);
        assert!((power_to_dbfs(0.01) + 20.0).abs() < 1e-4);
        assert_eq!(power_to_dbfs(2.0), 0.0);
    }
}
