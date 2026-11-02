// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The level and the voice-band spectrum the page draws for viewers who are
//! not listening. One 20 ms frame in, numbers out; no FFT crate — 32
//! Goertzel bins over 160 or 320 samples are cheap.

/// Bins across 300 Hz–3.4 kHz, log-spaced.
pub const SPECTRUM_BINS: usize = 32;
/// The bottom of every scale, in dBFS.
pub const SPECTRUM_FLOOR_DB: f32 = -80.0;
const F_LO: f32 = 300.0;
const F_HI: f32 = 3400.0;

/// Centre of bin `i`: `300·(3400/300)^(i/31)`. The page uses the same
/// formula for its own analyser, so both draw the same axis.
#[must_use]
pub fn bin_centre_hz(i: usize) -> f32 {
    #[allow(clippy::cast_precision_loss, reason = "i < 32")]
    let t = i as f32 / (SPECTRUM_BINS - 1) as f32;
    F_LO * (F_HI / F_LO).powf(t)
}

fn to_db(power: f32) -> f32 {
    if power <= 0.0 {
        SPECTRUM_FLOOR_DB
    } else {
        (10.0 * power.log10()).max(SPECTRUM_FLOOR_DB)
    }
}

/// RMS level of `pcm` in dBFS (a full-scale square wave is 0), floored.
#[must_use]
pub fn rms_dbfs(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return SPECTRUM_FLOOR_DB;
    }
    let sum: f64 = pcm
        .iter()
        .map(|&s| {
            let x = f64::from(s) / f64::from(i16::MAX);
            x * x
        })
        .sum();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a level"
    )]
    let mean = (sum / pcm.len() as f64) as f32;
    let db = to_db(mean);
    if db <= SPECTRUM_FLOOR_DB {
        SPECTRUM_FLOOR_DB
    } else {
        db.min(0.0)
    }
}

/// The voice-band spectrum of one frame, in dBFS per bin: a Hann window,
/// then a Goertzel filter at each bin centre. A full-scale sine at a bin
/// centre reads close to 0 dBFS.
#[must_use]
pub fn voice_spectrum(pcm: &[i16], sample_rate: u32) -> [f32; SPECTRUM_BINS] {
    let mut out = [SPECTRUM_FLOOR_DB; SPECTRUM_BINS];
    let n = pcm.len();
    if n < 2 || sample_rate == 0 {
        return out;
    }
    #[allow(clippy::cast_precision_loss, reason = "frame lengths are small")]
    let nf = n as f32;
    let window: Vec<f32> = (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss, reason = "i < n")]
            let x = i as f32 / (nf - 1.0);
            0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
        })
        .collect();
    let xs: Vec<f32> = pcm
        .iter()
        .zip(&window)
        .map(|(&s, w)| f32::from(s) / f32::from(i16::MAX) * w)
        .collect();
    // A Hann window's coherent gain is 0.5: amplitude A at a bin centre
    // gives |X| = A·N/4, so power is normalised by (N/4)² and doubled back
    // to a sine's RMS-referenced scale.
    let norm = (nf / 4.0) * (nf / 4.0);
    #[allow(clippy::cast_precision_loss, reason = "a sample rate")]
    let rate = sample_rate as f32;
    for (i, slot) in out.iter_mut().enumerate() {
        let f = bin_centre_hz(i);
        if f >= rate / 2.0 {
            continue; // above Nyquist at 8 kHz: nothing there
        }
        let coeff = 2.0 * (std::f32::consts::TAU * f / rate).cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in &xs {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2) / norm / 2.0;
        *slot = to_db(power).min(0.0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, ms: u32, amp: f32) -> Vec<i16> {
        (0..(rate * ms / 1000) as usize)
            .map(|i| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let v = ((i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * amp) as i16;
                v
            })
            .collect()
    }

    #[test]
    fn silence_is_at_the_floor() {
        assert!((rms_dbfs(&[0; 320]) - SPECTRUM_FLOOR_DB).abs() < f32::EPSILON);
        assert!(
            voice_spectrum(&[0; 320], 16_000)
                .iter()
                .all(|v| (*v - SPECTRUM_FLOOR_DB).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn full_scale_square_is_zero_dbfs() {
        assert!(rms_dbfs(&[i16::MAX; 320]).abs() < 0.1);
        // A full-scale sine is about -3 dBFS RMS.
        assert!((rms_dbfs(&tone(16_000, 1000.0, 20, 32767.0)) + 3.0).abs() < 0.3);
    }

    #[test]
    fn a_1khz_tone_peaks_in_the_1khz_bin() {
        for rate in [8_000, 16_000] {
            let s = voice_spectrum(&tone(rate, 1000.0, 20, 8000.0), rate);
            let peak = s
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0;
            let centre = bin_centre_hz(peak);
            assert!(
                (850.0..1180.0).contains(&centre),
                "rate {rate}: peak bin {peak} at {centre} Hz"
            );
            assert!(s[peak] > -20.0 && s[0] < s[peak] - 20.0, "{s:?}");
        }
    }

    #[test]
    fn the_axis_runs_300_to_3400() {
        assert!((bin_centre_hz(0) - 300.0).abs() < 0.01);
        assert!((bin_centre_hz(SPECTRUM_BINS - 1) - 3400.0).abs() < 0.1);
    }
}
