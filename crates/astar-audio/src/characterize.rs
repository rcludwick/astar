// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Mic noise characterization (iax-fb8d): analyze a recording of silence and
//! produce a [`MicProfile`] — the narrowband noise tones to notch, the noise
//! floor, and a derived gate threshold — that configures a per-mic noise
//! reducer. Cheap mics have device-specific narrowband whine (the test mic
//! whines around 240–600 Hz — ~588 Hz at 48 kHz, a 240–400 Hz cluster once
//! decimated to the 8 kHz pipeline rate), so a measured profile beats a fixed
//! mains-hum comb.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::f32::consts::TAU;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// A single notch: centre frequency (Hz) and quality factor.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NotchSpec {
    pub freq_hz: f32,
    pub q: f32,
}

/// A measured noise profile for one mic.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MicProfile {
    /// High-pass corner (Hz) for DC/rumble.
    pub highpass_hz: f32,
    /// Narrowband tones to notch out (strongest first).
    pub notches: Vec<NotchSpec>,
    /// Measured broadband noise floor (dBFS RMS).
    pub noise_floor_dbfs: f32,
    /// Suggested gate threshold (dBFS), the floor plus a margin.
    pub gate_threshold_db: f32,
}

/// Largest analysis window (samples). Longer silence is truncated to this.
const MAX_WINDOW: usize = 16_384;
/// A peak must exceed the spectral-median floor by this many dB (power).
const PEAK_OVER_FLOOR_DB: f32 = 12.0;
/// Most notches to emit. Whines are harmonic-rich (the test mic runs to its
/// 5th/6th harmonic, ~2940/3528 Hz); notch enough of them that no audible tone
/// survives — important for digital modes, where a residual tone corrupts the
/// vocoder/data even when an analog FM ear wouldn't notice.
const MAX_NOTCHES: usize = 6;
/// Notch quality for detected tones (narrow, to preserve voice).
const NOTCH_Q: f32 = 20.0;
/// Only look for tones in this band (Hz) — skip sub-audio rumble; the upper
/// bound reaches the high harmonics while staying under the 4 kHz Nyquist.
const SCAN_LO_HZ: f32 = 100.0;
const SCAN_HI_HZ: f32 = 3800.0;
/// Margin (dB) added to the noise floor for the gate threshold.
const GATE_MARGIN_DB: f32 = 6.0;

/// Runtime options for [`characterize_with`] (iax-5fb6). `Default` reproduces
/// today's flat peak detection exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CharacterizeOpts {
    /// Enable harmonic-aware notch detection: find the strongest fundamental
    /// `f0`, then scan its integer multiples `k·f0` at a RELAXED threshold and
    /// emit a notch comb (catches rolled-off upper harmonics a flat threshold
    /// misses). **Default off** to limit blast radius until validated against
    /// the real fake-Icom recording; when off, falls back to the flat detector.
    pub harmonic_comb: bool,
}

/// The strongest detected fundamental must beat the spectral-median floor by
/// this many dB (power) to anchor a harmonic comb (a stricter gate than the
/// per-harmonic scan: we only build a comb around a clearly-present tone).
const FUNDAMENTAL_OVER_FLOOR_DB: f32 = 12.0;
/// Once a fundamental is found, its harmonics are accepted at this RELAXED
/// threshold over the floor — low enough to catch upper harmonics that have
/// rolled off well below the fundamental.
const HARMONIC_OVER_FLOOR_DB: f32 = 3.0;
/// A harmonic peak is searched within ±this many bins of the ideal `k·f0` (the
/// real tone drifts a little off an exact integer multiple).
const HARMONIC_SEARCH_BINS: usize = 2;

/// Analyze `silence` (mic capture with no speech) at `sample_rate` and return a
/// noise profile, using today's flat peak detection.
#[must_use]
pub fn characterize(silence: &[f32], sample_rate: u32) -> MicProfile {
    characterize_with(silence, sample_rate, CharacterizeOpts::default())
}

/// Analyze `silence` at `sample_rate` with explicit [`CharacterizeOpts`]. With
/// `harmonic_comb` off this is identical to [`characterize`]; with it on, the
/// notch set is a learned-fundamental harmonic comb (iax-5fb6).
#[must_use]
pub fn characterize_with(silence: &[f32], sample_rate: u32, opts: CharacterizeOpts) -> MicProfile {
    let fs = sample_rate as f32;
    let rms = (silence.iter().map(|&s| s * s).sum::<f32>() / silence.len().max(1) as f32).sqrt();
    let floor_dbfs = if rms > 1e-9 {
        20.0 * rms.log10()
    } else {
        -120.0
    };

    let notches = if opts.harmonic_comb {
        detect_harmonic_comb(silence, fs)
    } else {
        detect_notches(silence, fs)
    };

    MicProfile {
        highpass_hz: 90.0,
        notches,
        noise_floor_dbfs: floor_dbfs,
        gate_threshold_db: floor_dbfs + GATE_MARGIN_DB,
    }
}

/// Compute the Hann-windowed power spectrum over (up to) [`MAX_WINDOW`] samples
/// plus the bin width and scan-band edges, shared by the flat and harmonic
/// detectors. Returns `None` if the input is too short to analyze.
fn power_spectrum(silence: &[f32], fs: f32) -> Option<(Vec<f32>, f32, usize, usize)> {
    let n = silence.len().min(MAX_WINDOW);
    if n < 64 {
        return None;
    }
    let mut buf: Vec<Complex<f32>> = (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos(); // Hann
            Complex::new(silence[i] * w, 0.0)
        })
        .collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let half = n / 2;
    let power: Vec<f32> = buf[..half].iter().map(Complex::norm_sqr).collect();
    let bin_hz = fs / n as f32;
    let k_lo = ((SCAN_LO_HZ / bin_hz).ceil() as usize).max(1);
    let k_hi = ((SCAN_HI_HZ / bin_hz).floor() as usize).min(half - 2);
    if k_hi <= k_lo {
        return None;
    }
    Some((power, bin_hz, k_lo, k_hi))
}

/// Harmonic-aware detection (iax-5fb6): find the strongest fundamental `f0` in
/// the scan band, then walk `k·f0` (k = 1, 2, 3, …) accepting each harmonic at a
/// RELAXED threshold, emitting a notch comb. Mirrors the `HumFilter` 60/120 Hz
/// comb, but with a learned fundamental, so rolled-off upper harmonics a flat
/// threshold would miss are still notched.
fn detect_harmonic_comb(silence: &[f32], fs: f32) -> Vec<NotchSpec> {
    let Some((power, bin_hz, k_lo, k_hi)) = power_spectrum(silence, fs) else {
        return Vec::new();
    };

    // Robust floor over the scan band (the same median basis the flat detector
    // uses), then the fundamental and harmonic acceptance thresholds.
    let mut band: Vec<f32> = power[k_lo..=k_hi].to_vec();
    band.sort_by(f32::total_cmp);
    let median = band[band.len() / 2].max(1e-20);
    let f0_threshold = median * 10f32.powf(FUNDAMENTAL_OVER_FLOOR_DB / 10.0);
    let harmonic_threshold = median * 10f32.powf(HARMONIC_OVER_FLOOR_DB / 10.0);

    // Candidate fundamentals: every clear peak (local max above the strict gate).
    let candidates: Vec<usize> = (k_lo..=k_hi)
        .filter(|&k| {
            power[k] > f0_threshold && power[k] >= power[k - 1] && power[k] >= power[k + 1]
        })
        .collect();
    if candidates.is_empty() {
        // No clear fundamental → fall back to the flat detector so we never do
        // worse than today.
        return detect_notches(silence, fs);
    }

    // The fundamental is the candidate that explains the MOST peaks as its
    // harmonics — not the strongest peak. A whine whose 2nd or 3rd harmonic is
    // louder than its fundamental (e.g. the fake-Icom, strongest at 1764 = 3·588)
    // still resolves to 588 because 588 explains the whole 588/1176/1764/… series
    // while 441 (= 1764/4) or 1176 explain only a couple. Tie-break toward the
    // lowest frequency (the true fundamental, not a harmonic of it).
    let tol = HARMONIC_SEARCH_BINS as f32;
    let coverage = |f0: usize| -> usize {
        candidates
            .iter()
            .filter(|&&b| {
                let ratio = (b as f32 / f0 as f32).round();
                ratio >= 1.0 && (ratio * f0 as f32 - b as f32).abs() <= tol
            })
            .count()
    };
    let k_fund = *candidates
        .iter()
        .max_by(|&&a, &&b| coverage(a).cmp(&coverage(b)).then(b.cmp(&a)))
        .expect("candidates non-empty");

    // Walk integer multiples of the fundamental, snapping each to the strongest
    // bin within a small search window and accepting it at the relaxed
    // threshold. Stop at MAX_NOTCHES or once a harmonic leaves the scan band.
    let mut notches: Vec<NotchSpec> = Vec::new();
    let mut k = 1;
    while notches.len() < MAX_NOTCHES {
        let target = k_fund * k;
        if target > k_hi {
            break;
        }
        let lo = target.saturating_sub(HARMONIC_SEARCH_BINS).max(k_lo);
        let hi = (target + HARMONIC_SEARCH_BINS).min(k_hi);
        // Strongest bin in the search window.
        if let Some((peak_k, peak_pw)) = (lo..=hi)
            .map(|kk| (kk, power[kk]))
            .max_by(|a, b| a.1.total_cmp(&b.1))
        {
            // k == 1 is the fundamental (always kept); higher harmonics must
            // clear the relaxed threshold.
            if k == 1 || peak_pw > harmonic_threshold {
                let freq_hz = peak_k as f32 * bin_hz;
                if !notches.iter().any(|ns| (ns.freq_hz - freq_hz).abs() < 15.0) {
                    notches.push(NotchSpec {
                        freq_hz,
                        q: NOTCH_Q,
                    });
                }
            }
        }
        k += 1;
    }
    notches
}

/// Hann-windowed FFT of (up to) the first [`MAX_WINDOW`] samples, then pick the
/// strongest narrowband peaks that stand [`PEAK_OVER_FLOOR_DB`] above the
/// spectral-median floor.
fn detect_notches(silence: &[f32], fs: f32) -> Vec<NotchSpec> {
    let n = silence.len().min(MAX_WINDOW);
    if n < 64 {
        return Vec::new();
    }

    let mut buf: Vec<Complex<f32>> = (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos(); // Hann
            Complex::new(silence[i] * w, 0.0)
        })
        .collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);

    let half = n / 2;
    let power: Vec<f32> = buf[..half].iter().map(Complex::norm_sqr).collect();
    let bin_hz = fs / n as f32;
    let k_lo = ((SCAN_LO_HZ / bin_hz).ceil() as usize).max(1);
    let k_hi = ((SCAN_HI_HZ / bin_hz).floor() as usize).min(half - 2);
    if k_hi <= k_lo {
        return Vec::new();
    }

    // Spectral-median floor over the scan band (robust to the few peaks).
    let mut band: Vec<f32> = power[k_lo..=k_hi].to_vec();
    band.sort_by(f32::total_cmp);
    let median = band[band.len() / 2].max(1e-20);
    let threshold = median * 10f32.powf(PEAK_OVER_FLOOR_DB / 10.0);

    // Local maxima above threshold, strongest first, deduped within 15 Hz.
    let mut peaks: Vec<(usize, f32)> = (k_lo..=k_hi)
        .filter(|&k| power[k] > threshold && power[k] >= power[k - 1] && power[k] >= power[k + 1])
        .map(|k| (k, power[k]))
        .collect();
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut notches: Vec<NotchSpec> = Vec::new();
    for (k, _) in peaks {
        if notches.len() >= MAX_NOTCHES {
            break;
        }
        let freq_hz = k as f32 * bin_hz;
        if notches.iter().any(|ns| (ns.freq_hz - freq_hz).abs() < 15.0) {
            continue;
        }
        notches.push(NotchSpec {
            freq_hz,
            q: NOTCH_Q,
        });
    }
    notches
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic low-level white noise (LCG).
    fn noise(n: usize, amp: f32) -> Vec<f32> {
        let mut state: u32 = 0x2545_f491;
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let r = (state >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
                r * amp
            })
            .collect()
    }

    fn add_tone(sig: &mut [f32], freq: f32, fs: u32, amp: f32) {
        for (i, s) in sig.iter_mut().enumerate() {
            *s += amp * (TAU * freq * i as f32 / fs as f32).sin();
        }
    }

    fn has_notch_near(p: &MicProfile, freq: f32, tol: f32) -> bool {
        p.notches.iter().any(|n| (n.freq_hz - freq).abs() <= tol)
    }

    #[test]
    fn detects_the_two_strong_tones() {
        let mut sig = noise(16_384, 0.01);
        add_tone(&mut sig, 600.0, 8000, 0.1);
        add_tone(&mut sig, 1200.0, 8000, 0.1);
        let profile = characterize(&sig, 8000);
        assert!(
            has_notch_near(&profile, 600.0, 10.0),
            "missing 600 Hz notch: {:?}",
            profile.notches
        );
        assert!(
            has_notch_near(&profile, 1200.0, 10.0),
            "missing 1200 Hz notch: {:?}",
            profile.notches
        );
    }

    #[test]
    fn pure_noise_yields_no_notches() {
        let sig = noise(16_384, 0.02);
        let profile = characterize(&sig, 8000);
        assert!(
            profile.notches.is_empty(),
            "noise should not notch: {:?}",
            profile.notches
        );
    }

    /// Minimal mono 16-bit PCM WAV reader (scans chunks for `data`).
    fn read_wav_mono_f32(path: &str) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("read wav fixture");
        let mut i = 12; // past "RIFF"<size>"WAVE"
        while i + 8 <= bytes.len() {
            let id = &bytes[i..i + 4];
            let sz = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
                as usize;
            let body = i + 8;
            if id == b"data" {
                return bytes[body..body + sz]
                    .chunks_exact(2)
                    .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                    .collect();
            }
            i = body + sz + (sz & 1); // word-aligned
        }
        panic!("no data chunk in {path}");
    }

    #[test]
    fn detects_the_real_mic_whine_in_the_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake-icom-hum.wav"
        );
        let wav_48k = read_wav_mono_f32(path);
        // Leading ~2 s of silence (hum only), decimated 48k→8k by a 6-sample
        // box average (crude anti-alias; the whine is well below 4 kHz).
        let lead = &wav_48k[..(48_000 * 2).min(wav_48k.len())];
        let s8k: Vec<f32> = lead
            .chunks(6)
            .map(|c| c.iter().sum::<f32>() / c.len() as f32)
            .collect();
        let profile = characterize(&s8k, 8000);
        // The mic's narrowband whine lives in the ~240–600 Hz cluster at 8 kHz
        // (588 Hz dominates at 48 kHz, but the pipeline runs at 8 kHz). Assert
        // the detector locks onto several real tones in that band — not voice,
        // not nothing.
        assert!(
            profile.notches.len() >= 3,
            "expected several whine notches, got {:?}",
            profile.notches
        );
        assert!(
            profile
                .notches
                .iter()
                .all(|n| (150.0..=3800.0).contains(&n.freq_hz)),
            "all notches should fall in the whine band, got {:?}",
            profile.notches
        );
    }

    #[test]
    fn measures_a_reasonable_noise_floor() {
        // RMS of a 0.02-amplitude white-ish signal is well below 0 dBFS.
        let sig = noise(8000, 0.02);
        let profile = characterize(&sig, 8000);
        assert!(
            profile.noise_floor_dbfs < -20.0 && profile.noise_floor_dbfs > -90.0,
            "floor out of range: {}",
            profile.noise_floor_dbfs
        );
        assert!(profile.gate_threshold_db > profile.noise_floor_dbfs);
    }

    // --- iax-5fb6: harmonic-aware notch comb (optional, default OFF) ---

    /// The fake-Icom acceptance fixture, SYNTHESIZED: a 588 Hz whine plus three
    /// harmonics (~1176/1764/2352 Hz) with the upper ones ROLLED OFF in level —
    /// the weak 4th especially. All four are within the 8 kHz voice band.
    fn synth_fake_icom(n: usize, fs: u32) -> Vec<f32> {
        let mut sig = noise(n, 0.05);
        add_tone(&mut sig, 588.0, fs, 0.20); // fundamental
        add_tone(&mut sig, 1176.0, fs, 0.08); // 2nd, -8 dB-ish
        add_tone(&mut sig, 1764.0, fs, 0.03); // 3rd, rolled off
        add_tone(&mut sig, 2352.0, fs, 0.006); // 4th, buried near the noise floor — the flat threshold misses it
        sig
    }

    #[test]
    fn comb_off_by_default_keeps_flat_detection() {
        // Default opts == flat detector == today's `characterize`.
        let sig = synth_fake_icom(16_384, 8000);
        let flat = characterize(&sig, 8000);
        let defaulted = characterize_with(&sig, 8000, CharacterizeOpts::default());
        assert_eq!(
            flat.notches, defaulted.notches,
            "default (comb off) must match today's flat detection exactly"
        );
    }

    /// The REAL fake-Icom profile (observed on-device): the 2nd harmonic (1176)
    /// is LOUDER than the 588 fundamental. The synthesized acceptance fixture
    /// above has the fundamental loudest, so it never exercised this case.
    fn synth_fake_icom_loud_2nd(n: usize, fs: u32) -> Vec<f32> {
        let mut sig = noise(n, 0.05);
        add_tone(&mut sig, 588.0, fs, 0.10); // fundamental
        add_tone(&mut sig, 1176.0, fs, 0.25); // 2nd — LOUDER than the fundamental
        add_tone(&mut sig, 1764.0, fs, 0.06); // 3rd
        add_tone(&mut sig, 2352.0, fs, 0.02); // 4th
        sig
    }

    #[test]
    fn harmonic_comb_finds_fundamental_when_a_harmonic_is_loudest() {
        // Regression for the on-device fake-Icom: with the 2nd harmonic loudest,
        // the comb must still anchor on the 588 fundamental (not 1176) and notch
        // the odd harmonics (1764) the wrong anchor would skip.
        let sig = synth_fake_icom_loud_2nd(16_384, 8000);
        let comb = characterize_with(
            &sig,
            8000,
            CharacterizeOpts {
                harmonic_comb: true,
            },
        );
        for f in [588.0, 1176.0, 1764.0] {
            assert!(
                has_notch_near(&comb, f, 12.0),
                "comb must anchor on the 588 fundamental, missing {f} Hz: {:?}",
                comb.notches
            );
        }
    }

    #[test]
    fn harmonic_comb_notches_all_four_including_the_weak_fourth() {
        // With the comb ENABLED, the learned-fundamental walk must notch all
        // four 588 Hz harmonics — including the rolled-off 4th a flat 12 dB
        // threshold misses.
        let sig = synth_fake_icom(16_384, 8000);
        let profile = characterize_with(
            &sig,
            8000,
            CharacterizeOpts {
                harmonic_comb: true,
            },
        );
        for f in [588.0, 1176.0, 1764.0, 2352.0] {
            assert!(
                has_notch_near(&profile, f, 12.0),
                "harmonic comb missing {f} Hz notch: {:?}",
                profile.notches
            );
        }
    }

    #[test]
    fn harmonic_comb_locks_the_series_past_loud_spurious_nonharmonics() {
        // The comb's value over the flat (top-N-by-strength) detector: it targets
        // the harmonic SERIES, so it still notches a weak harmonic even when
        // louder non-harmonic spurs are present that would crowd the flat
        // detector's MAX_NOTCHES budget. Add several loud non-harmonic tones plus
        // the weak 4th harmonic; the comb must still notch all four harmonics.
        let mut sig = synth_fake_icom(16_384, 8000);
        for f in [700.0, 950.0, 1500.0, 2100.0, 3000.0, 3400.0] {
            add_tone(&mut sig, f, 8000, 0.15); // loud non-harmonic spurs
        }
        let comb = characterize_with(
            &sig,
            8000,
            CharacterizeOpts {
                harmonic_comb: true,
            },
        );
        for f in [588.0, 1176.0, 1764.0, 2352.0] {
            assert!(
                has_notch_near(&comb, f, 12.0),
                "comb should lock the harmonic series despite loud spurs, missing {f}: {:?}",
                comb.notches
            );
        }
    }

    #[test]
    fn harmonic_comb_falls_back_to_flat_when_no_fundamental() {
        // Pure noise has no clear fundamental → the comb path must fall back to
        // the flat detector (which emits nothing), never panic or invent tones.
        let sig = noise(16_384, 0.02);
        let comb = characterize_with(
            &sig,
            8000,
            CharacterizeOpts {
                harmonic_comb: true,
            },
        );
        assert!(
            comb.notches.is_empty(),
            "no fundamental → no comb: {:?}",
            comb.notches
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn mic_profile_json_round_trips() {
        let profile = MicProfile {
            highpass_hz: 90.0,
            notches: vec![
                NotchSpec {
                    freq_hz: 588.0,
                    q: 20.0,
                },
                NotchSpec {
                    freq_hz: 1176.0,
                    q: 20.0,
                },
            ],
            noise_floor_dbfs: -52.0,
            gate_threshold_db: -46.0,
        };
        let json = serde_json::to_string(&profile).expect("serialize");
        let back: MicProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(profile, back, "MicProfile JSON must round-trip");
    }

    /// DEFERRED real-recording acceptance (iax-5fb6): the comb must notch all
    /// four 588 Hz harmonics of the REAL fake-Icom handset mic. The WAV is not
    /// captured yet; drop it at the path below and remove `#[ignore]` to enable.
    /// Until then the synthesized `harmonic_comb_notches_all_four_*` test stands
    /// in as the interim sanity check.
    #[test]
    fn harmonic_comb_notches_real_fake_icom_recording() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake-icom-588hz.wav"
        );
        // The fixture is captured at the 8 kHz pipeline rate via ffmpeg's
        // anti-aliased resampler (Tools/record-mic-fixture.sh + a -ar 8000 pass).
        // A crude in-test 6:1 box-average would alias the whine's high harmonics
        // down into the band and fabricate a spurious lower fundamental, so we
        // feed the 8 kHz capture straight in.
        let s8k = read_wav_mono_f32(path);
        let profile = characterize_with(
            &s8k,
            8000,
            CharacterizeOpts {
                harmonic_comb: true,
            },
        );
        for f in [588.0, 1176.0, 1764.0, 2352.0] {
            assert!(
                has_notch_near(&profile, f, 20.0),
                "real fake-Icom comb missing {f} Hz notch: {:?}",
                profile.notches
            );
        }
    }
}
