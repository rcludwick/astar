// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::tone_detect`: recognizes a DTMF digit or a single sustained tone in one 20 ms
//! (160-sample) frame of 16-bit PCM, with the chip-matched rules of the float sibling (DTMF index
//! `128 + row + 4*col`; single tones at 200 Hz and 400-3800 Hz with index `round(f/31.25)`; none at 100/300/3900 Hz;
//! level field `186 + 17*log2(A/4000)`).
//!
//! Numerics: the least-squares sinusoid fit accumulates `sin`/`cos` products from the quarter-wave table
//! ([`super::trig`]) in `i64` (Q32 sums) and solves the 2x2 normal equations in `i128`; amplitudes come out as Q16.16
//! (`i64`, since two full-scale tones can exceed the `i32` Q16.16 range). All threshold tests are done on squared
//! quantities by cross-multiplication, so no division or square root enters a decision except the amplitude itself.
//! The frame's phase per sample is a wrapping `u32` accumulator step (`hz/8000` turns).
//!
//! Agreement with the float detector, measured in `tests/ambe_fixed_tone_detect.rs` on 449 synthetic frames (all 16
//! DTMF digits at four levels and three phases, unequal-level pairs, 200 Hz and 400-3800 Hz single tones plus
//! off-grid frequencies at five levels, the 100/300/3900 Hz non-detections, quiet tones, noise, silence, voiced-like
//! harmonics, a tone in noise): identical decisions on all 449 (402 detections), worst amplitude error 0.0018%,
//! volume field identical, frequency identical.

use super::explog::log2_q16_i64;
use super::fixed_ops::sqrt_wide_q16;
use super::trig::{cos_q16, sin_q16};

pub const FRAME: usize = 160;

const HZ_Q16: i32 = 65536;
/// DTMF row and column frequencies in Q16.16 Hz (697/770/852/941 and 1209/1336/1477/1633).
const DTMF_ROW_HZ_Q16: [i32; 4] = [697 * HZ_Q16, 770 * HZ_Q16, 852 * HZ_Q16, 941 * HZ_Q16];
const DTMF_COL_HZ_Q16: [i32; 4] = [1209 * HZ_Q16, 1336 * HZ_Q16, 1477 * HZ_Q16, 1633 * HZ_Q16];
/// `12.5 Hz` and `31.25 Hz` in Q16.16 (both exact).
const SCAN_STEP_Q16: i32 = 819_200;
const HZ_PER_INDEX_Q16: i32 = 2_048_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedTone {
    Dtmf {
        row: u8,
        col: u8,
    },
    /// `index = round(f / 31.25)`, `hz_q16` the measured frequency (Q16.16 Hz).
    Single {
        index: u32,
        hz_q16: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub tone: DetectedTone,
    /// Per-tone peak amplitude in PCM units, Q16.16 (`i64`).
    pub amplitude_q16: i64,
}

/// Best-fit sinusoid of frequency `hz_q16` in `x`: returns `(amplitude_q16, amplitude_squared_q16)`.
fn fit_sinusoid(x: &[i16], hz_q16: i32) -> (i64, i64) {
    let step = (((hz_q16 as u64) << 16) / 8000) as u32; // turns per sample, 1u32<<32 = one turn
    let (mut sc, mut cc, mut sn, mut xc, mut xs) = (0i64, 0i64, 0i64, 0i64, 0i64);
    for (i, &v) in x.iter().enumerate() {
        let phase = step.wrapping_mul(i as u32);
        let (s, c) = (sin_q16(phase) as i64, cos_q16(phase) as i64);
        cc += c * c;
        sn += s * s;
        sc += s * c;
        xc += v as i64 * c;
        xs += v as i64 * s;
    }
    let det = (cc as i128) * (sn as i128) - (sc as i128) * (sc as i128);
    if det < (1i128 << 35) {
        return (0, 0); // the float's `|det| < 1e-9` degenerate case (`det` here is scaled by 2^64)
    }
    let num_a = (xc as i128) * (sn as i128) - (xs as i128) * (sc as i128);
    let num_b = (xs as i128) * (cc as i128) - (xc as i128) * (sc as i128);
    let a = (num_a << 32) / det; // Q16.16
    let b = (num_b << 32) / det;
    let p = ((a * a + b * b) >> 16) as i64; // amplitude squared, Q16.16
    (sqrt_wide_q16(p), p)
}

/// Energy (in Q16.16 sample-squared units, times the frame length / 2 per the float's `a*a/2*N`) explained by the
/// given fitted amplitudes-squared, compared with the frame's total energy: returns whether
/// `explained / total > num / den`.
fn explained_exceeds(total: i64, p_sum_q16: i64, num: i64, den: i64) -> bool {
    // explained = sum(a^2)/2 * N = sum(a^2) * 80 (Q16.16)
    let explained_q16 = (p_sum_q16 as i128) * (FRAME as i128 / 2);
    total > 0 && explained_q16 * den as i128 > ((total as i128) << 16) * num as i128
}

/// The level field for a per-tone amplitude (Q16.16): `round(186 + 17*log2(A / 4000))` clamped to `0..=255`.
pub fn volume_for_amplitude_q16(amplitude_q16: i64) -> u32 {
    let log2_4000 = log2_q16_i64(4000i64 << 16) as i64;
    let lg = log2_q16_i64(amplitude_q16.max(65536)) as i64;
    let v = 186 * 65536 + 17 * (lg - log2_4000);
    ((v + 32768) >> 16).clamp(0, 255) as u32
}

/// Detects a tone in one 160-sample frame, or `None` for anything else (speech, silence, noise).
pub fn detect_tone(frame: &[i16]) -> Option<Detection> {
    if frame.len() != FRAME {
        return None;
    }
    let total: i64 = frame.iter().map(|&v| v as i64 * v as i64).sum();
    // energy = total / 160 < 100^2 / 2  <=>  total < 800_000
    if total < 800_000 {
        return None;
    }

    // DTMF: strongest row and column (ties resolve to the last, like the float's `max_by`).
    let rows: Vec<(i64, i64)> = DTMF_ROW_HZ_Q16
        .iter()
        .map(|&hz| fit_sinusoid(frame, hz))
        .collect();
    let cols: Vec<(i64, i64)> = DTMF_COL_HZ_Q16
        .iter()
        .map(|&hz| fit_sinusoid(frame, hz))
        .collect();
    let arg_max = |v: &[(i64, i64)]| {
        let mut best = 0;
        for (i, e) in v.iter().enumerate() {
            if e.0 >= v[best].0 {
                best = i;
            }
        }
        best
    };
    let (r, c) = (arg_max(&rows), arg_max(&cols));
    let (ra, ca) = (rows[r].0, cols[c].0);
    let hundred = 100i64 << 16;
    if ra > hundred
        && ca > hundred
        && ra.max(ca) < 3 * ra.min(ca)
        && explained_exceeds(total, rows[r].1 + cols[c].1, 17, 20)
    {
        return Some(Detection {
            tone: DetectedTone::Dtmf {
                row: r as u8,
                col: c as u8,
            },
            amplitude_q16: (ra + ca) >> 1,
        });
    }

    // Single tone: coarse scan for the strongest sinusoid (150..=3900 Hz in 12.5 Hz steps), then refine by 1 Hz.
    let mut best = (0i64, 0i32, 0i64); // (amplitude, hz, amplitude squared)
    let mut hz = 150 * HZ_Q16;
    while hz <= 3900 * HZ_Q16 {
        let (a, p) = fit_sinusoid(frame, hz);
        if a > best.0 {
            best = (a, hz, p);
        }
        hz += SCAN_STEP_Q16;
    }
    let mut refined = best;
    let mut f = best.1 - SCAN_STEP_Q16;
    while f <= best.1 + SCAN_STEP_Q16 {
        let (a, p) = fit_sinusoid(frame, f);
        if a > refined.0 {
            refined = (a, f, p);
        }
        f += HZ_Q16;
    }
    let (amp, hz, p) = refined;
    if amp > hundred && explained_exceeds(total, p, 99, 100) {
        let index = ((hz as i64 + (HZ_PER_INDEX_Q16 as i64 >> 1)) / HZ_PER_INDEX_Q16 as i64) as u32;
        let in_range = (13..=122).contains(&index)
            || (index == 6
                && (hz - 200 * HZ_Q16).abs() < 3 * HZ_Q16
                && explained_exceeds(total, p, 999, 1000));
        if in_range {
            return Some(Detection {
                tone: DetectedTone::Single { index, hz_q16: hz },
                amplitude_q16: amp,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integer test tone: `amp * sin(2*pi*hz*(offset+i)/8000)` from the fixed sine table itself.
    fn sines(freqs: &[i32], amp: i64, offset: usize) -> Vec<i16> {
        (0..FRAME)
            .map(|i| {
                freqs
                    .iter()
                    .map(|&hz| {
                        let step = (((hz as u64) << 32) / 8000) as u32;
                        let phase = step.wrapping_mul((offset + i) as u32);
                        (amp * sin_q16(phase) as i64) >> 16
                    })
                    .sum::<i64>() as i16
            })
            .collect()
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn detects_every_dtmf_digit_with_the_chip_index_and_level() {
        for r in 0..4usize {
            for c in 0..4usize {
                let x = sines(
                    &[DTMF_ROW_HZ_Q16[r] >> 16, DTMF_COL_HZ_Q16[c] >> 16],
                    4000,
                    37,
                );
                let d = detect_tone(&x).expect("digit detected");
                assert_eq!(
                    d.tone,
                    DetectedTone::Dtmf {
                        row: r as u8,
                        col: c as u8
                    }
                );
                assert!(
                    (d.amplitude_q16 - (4000 << 16)).abs() < (200 << 16),
                    "amplitude {}",
                    d.amplitude_q16
                );
                assert!((volume_for_amplitude_q16(d.amplitude_q16) as i32 - 186).abs() <= 1);
            }
        }
    }

    #[test]
    fn single_tone_index_and_levels_match_the_chip() {
        for (hz, index) in [(500, 16u32), (1000, 32), (2000, 64), (3800, 122), (400, 13)] {
            let d = detect_tone(&sines(&[hz], 4000, 11)).expect("tone detected");
            assert!(
                matches!(d.tone, DetectedTone::Single { index: i, .. } if i == index),
                "{hz} Hz -> {:?}",
                d.tone
            );
        }
        assert_eq!(volume_for_amplitude_q16(500 << 16), 135);
        assert_eq!(volume_for_amplitude_q16(1000 << 16), 152);
        assert_eq!(volume_for_amplitude_q16(12000 << 16), 213);
    }

    #[test]
    fn chip_non_detections_are_reproduced_and_speech_like_frames_are_rejected() {
        assert!(detect_tone(&sines(&[100], 4000, 0)).is_none());
        assert!(detect_tone(&sines(&[300], 4000, 0)).is_none());
        assert!(detect_tone(&sines(&[3900], 4000, 0)).is_none());
        assert!(detect_tone(&[0i16; FRAME]).is_none());
        let voiced = sines(&[130, 260, 390, 520, 650, 780], 800, 0);
        assert!(detect_tone(&voiced).is_none());
    }
}
