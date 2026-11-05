// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tone detection for the D-STAR and AMBE+2 encoders: recognizes a DTMF digit or a single sustained tone in one
//! 20 ms (160-sample) frame, the way the real chip's `TD_ENABLE` detector does, so an encoder can emit a tone frame
//! instead of a speech frame. Behaviour was matched to the chip with `examples/dstar_tone_detect_probe.rs`:
//! - DTMF digits are detected in the very first frame, index `128 + row + 4*col`;
//! - a single tone is detected from 400 Hz to 3800 Hz (and at 200 Hz), but not at 100 Hz, 300 Hz or from 3900 Hz,
//!   with index `round(f / 31.25)`;
//! - the level field is `186 + 17*log2(A / 4000)` for each tone's amplitude `A` (same for single and dual tones).

use super::tone_synthesis::{DTMF_COL_HZ, DTMF_ROW_HZ};
use std::f64::consts::PI;

pub const FRAME: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetectedTone {
    Dtmf {
        row: u8,
        col: u8,
    },
    /// `index = round(f / 31.25)`, `hz` the measured frequency.
    Single {
        index: u32,
        hz: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    pub tone: DetectedTone,
    /// Per-tone peak amplitude in PCM units.
    pub amplitude: f64,
}

/// Amplitude of the best-fit sinusoid of frequency `hz` in `x` (least squares over the frame).
fn fit_sinusoid(x: &[f64], hz: f64) -> f64 {
    let w = 2.0 * PI * hz / 8000.0;
    let (mut sc, mut cc, mut sn, mut xc, mut xs) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = (w * i as f64).sin_cos();
        cc += c * c;
        sn += s * s;
        sc += s * c;
        xc += v * c;
        xs += v * s;
    }
    let det = cc * sn - sc * sc;
    if det.abs() < 1e-9 {
        return 0.0;
    }
    let a = (xc * sn - xs * sc) / det;
    let b = (xs * cc - xc * sc) / det;
    (a * a + b * b).sqrt()
}

/// Fraction of the frame's energy explained by sinusoids at the given frequencies (fitted independently; adequate for
/// well-separated tones).
fn explained_fraction(x: &[f64], hzs: &[f64]) -> (f64, Vec<f64>) {
    let total: f64 = x.iter().map(|v| v * v).sum();
    let amps: Vec<f64> = hzs.iter().map(|&hz| fit_sinusoid(x, hz)).collect();
    let explained: f64 = amps.iter().map(|a| a * a / 2.0 * x.len() as f64).sum();
    (
        if total > 0.0 {
            (explained / total).min(1.5)
        } else {
            0.0
        },
        amps,
    )
}

/// Fraction of a frame's energy one sinusoid must explain to count as a single tone. `0.9` mistook voiced speech whose
/// fundamental dominates a frame for a tone (5 of 150 frames of the OSR test speech, each then synthesized at tone
/// level); real tones sit at essentially 1.0.
pub const SINGLE_TONE_MIN_EXPLAINED_FRACTION: f64 = 0.99;

/// Purity required of the 200 Hz single tone, the one tone below 400 Hz the chip reports.
pub const LOW_TONE_MIN_EXPLAINED_FRACTION: f64 = 0.999;

pub fn volume_for_amplitude(amplitude: f64) -> u32 {
    (186.0 + 17.0 * (amplitude.max(1.0) / 4000.0).log2())
        .round()
        .clamp(0.0, 255.0) as u32
}

/// Detects a tone in one 160-sample frame, or `None` for anything else (speech, silence, noise).
pub fn detect_tone(frame: &[f64]) -> Option<Detection> {
    if frame.len() != FRAME {
        return None;
    }
    let energy: f64 = frame.iter().map(|v| v * v).sum::<f64>() / FRAME as f64;
    if energy < 100.0 * 100.0 / 2.0 {
        return None; // quieter than a 100-amplitude sine: nothing worth calling a tone
    }

    // DTMF: strongest row and column.
    let row_amps: Vec<f64> = DTMF_ROW_HZ
        .iter()
        .map(|&hz| fit_sinusoid(frame, hz))
        .collect();
    let col_amps: Vec<f64> = DTMF_COL_HZ
        .iter()
        .map(|&hz| fit_sinusoid(frame, hz))
        .collect();
    let arg_max = |v: &[f64]| {
        v.iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap()
    };
    let (r, c) = (arg_max(&row_amps), arg_max(&col_amps));
    let (ra, ca) = (row_amps[r], col_amps[c]);
    if ra > 100.0 && ca > 100.0 && (ra / ca).max(ca / ra) < 3.0 {
        let (frac, _) = explained_fraction(frame, &[DTMF_ROW_HZ[r], DTMF_COL_HZ[c]]);
        if frac > 0.85 {
            return Some(Detection {
                tone: DetectedTone::Dtmf {
                    row: r as u8,
                    col: c as u8,
                },
                amplitude: (ra + ca) / 2.0,
            });
        }
    }

    // Single tone: coarse scan for the strongest sinusoid, then refine.
    let mut best = (0.0f64, 0.0f64);
    let mut hz = 150.0;
    while hz <= 3900.0 {
        let a = fit_sinusoid(frame, hz);
        if a > best.0 {
            best = (a, hz);
        }
        hz += 12.5;
    }
    let mut refined = best;
    let mut f = best.1 - 12.5;
    while f <= best.1 + 12.5 {
        let a = fit_sinusoid(frame, f);
        if a > refined.0 {
            refined = (a, f);
        }
        f += 1.0;
    }
    let (amp, hz) = refined;
    let (frac, _) = explained_fraction(frame, &[hz]);
    if amp > 100.0 && frac > SINGLE_TONE_MIN_EXPLAINED_FRACTION {
        let index = (hz / 31.25).round() as u32;
        // The chip reports single tones from 400 Hz up, plus 200 Hz. Below 400 Hz voiced speech is itself nearly a pure
        // sinusoid (a vowel's fundamental at 150-390 Hz explains 90-100% of a frame), so only a very pure, exactly-200 Hz
        // component counts there.
        let in_range = (13..=122).contains(&index)
            || (index == 6 && (hz - 200.0).abs() < 3.0 && frac > LOW_TONE_MIN_EXPLAINED_FRACTION);
        if in_range {
            return Some(Detection {
                tone: DetectedTone::Single { index, hz },
                amplitude: amp,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sines(freqs: &[f64], amp: f64, offset: usize) -> Vec<f64> {
        (0..FRAME)
            .map(|i| {
                freqs
                    .iter()
                    .map(|&hz| amp * (2.0 * PI * hz * (offset + i) as f64 / 8000.0).sin())
                    .sum()
            })
            .collect()
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn detects_every_dtmf_digit_with_the_chip_index_and_level() {
        for r in 0..4usize {
            for c in 0..4usize {
                let x = sines(&[DTMF_ROW_HZ[r], DTMF_COL_HZ[c]], 4000.0, 37);
                let d = detect_tone(&x).expect("digit detected");
                assert_eq!(
                    d.tone,
                    DetectedTone::Dtmf {
                        row: r as u8,
                        col: c as u8
                    }
                );
                assert!(
                    (d.amplitude / 4000.0 - 1.0).abs() < 0.05,
                    "amplitude {}",
                    d.amplitude
                );
                assert!((volume_for_amplitude(d.amplitude) as i32 - 186).abs() <= 1);
            }
        }
    }

    #[test]
    fn single_tone_index_and_levels_match_the_chip() {
        for (hz, index) in [
            (500.0, 16u32),
            (1000.0, 32),
            (2000.0, 64),
            (3800.0, 122),
            (400.0, 13),
        ] {
            let d = detect_tone(&sines(&[hz], 4000.0, 11)).expect("tone detected");
            assert!(
                matches!(d.tone, DetectedTone::Single { index: i, .. } if i == index),
                "{hz} Hz -> {:?}",
                d.tone
            );
        }
        assert_eq!(volume_for_amplitude(500.0), 135);
        assert_eq!(volume_for_amplitude(1000.0), 152);
        assert_eq!(volume_for_amplitude(12000.0), 213);
    }

    #[test]
    fn chip_non_detections_are_reproduced_and_speech_like_frames_are_rejected() {
        assert!(detect_tone(&sines(&[100.0], 4000.0, 0)).is_none());
        assert!(detect_tone(&sines(&[300.0], 4000.0, 0)).is_none());
        assert!(detect_tone(&sines(&[3900.0], 4000.0, 0)).is_none());
        assert!(detect_tone(&vec![0.0; FRAME]).is_none());
        // A harmonic-rich voiced-like frame is not a single tone.
        let voiced = sines(&[130.0, 260.0, 390.0, 520.0, 650.0, 780.0], 800.0, 0);
        assert!(detect_tone(&voiced).is_none());
    }
}
