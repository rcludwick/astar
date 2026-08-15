// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Post-parrot signal analysis (iax-feab): level stats, clipping density,
//! and fake-ICOM 588 Hz tone-spur detection over a complete recording.

// Sample counts/rates and dB/Hz values here never approach f32's precision
// limits or i16/i32/u32 range edges (same rationale as the DSP casts in
// `spectrum.rs`).
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use astar_iax_core::VoiceFormat;

/// Overall signal grade, spoken first in the English report (Rob 2026-07-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grade {
    /// Healthy level, clean spectrum, good SNR.
    Green,
    /// Usable but marginal: hot (above [`YELLOW_HOT_PEAK_DB`]), quiet (below
    /// [`YELLOW_QUIET_PEAK_DB`]), noisy (SNR under [`YELLOW_MIN_SNR_DB`]),
    /// or a moderate tone spur (20-30 dB over the spectral floor).
    Yellow,
    /// Overdriven, underdriven, or a severe tone spur (at or above
    /// [`RED_TONE_OVER_FLOOR_DB`] over the spectral floor).
    Red,
}

/// Post-parrot signal analysis (Rob's spec 2026-07-12).
#[derive(Clone, Debug, PartialEq)]
pub struct SignalReport {
    /// Overall grade; spoken first in the report.
    pub grade: Grade,
    /// Max |sample| in dBFS (0 dBFS = full scale i16).
    pub peak_dbfs: f32,
    /// Quietest decile of 20 ms frame RMS values, in dBFS ("min peaks").
    pub noise_floor_dbfs: f32,
    /// > 0.5% of samples at or above -0.5 dBFS.
    pub overdriven: bool,
    /// Peak below -30 dBFS.
    pub underdriven: bool,
    /// Detected tone spurs: (frequency Hz, dB above the spectral floor —
    /// see `spectral_floor_dbfs`, not the same measure as `noise_floor_dbfs`).
    /// 588 Hz and integer multiples below Nyquist, threshold >= 20 dB.
    pub tones: Vec<(f32, f32)>,
}

const CLIP_LEVEL: f32 = 0.944; // -0.5 dBFS
const CLIP_RATIO: f32 = 0.005; // 0.5% of samples
const UNDERDRIVEN_PEAK_DB: f32 = -30.0;
const TONE_BASE_HZ: f32 = 588.0;
/// A spur this far over the spectral floor is reported (yellow band).
const TONE_OVER_FLOOR_DB: f32 = 20.0;
/// A spur this far over the spectral floor grades red.
const RED_TONE_OVER_FLOOR_DB: f32 = 30.0;
/// Voice-channel HPF line: repeater PL/CTCSS tones (~85-100 Hz) live below
/// it and are legitimate, so nothing under this frequency is probed as a
/// spur or used as a spectral-floor control point.
const IGNORE_BELOW_HZ: f32 = 300.0;
/// Peak hotter than this (but not clipping) grades yellow.
const YELLOW_HOT_PEAK_DB: f32 = -3.0;
/// Peak quieter than this (but above the underdriven line) grades yellow.
const YELLOW_QUIET_PEAK_DB: f32 = -20.0;
/// Peak-over-noise-floor below this grades yellow.
const YELLOW_MIN_SNR_DB: f32 = 20.0;

/// Analyze a complete recording (whole-call PCM buffer, not streaming).
#[must_use]
pub fn analyze_signal(pcm: &[i16], sample_rate: u32) -> SignalReport {
    if pcm.is_empty() {
        return SignalReport {
            grade: Grade::Red, // underdriven
            peak_dbfs: -120.0,
            noise_floor_dbfs: -120.0,
            overdriven: false,
            underdriven: true,
            tones: Vec::new(),
        };
    }
    let f: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
    let peak = f.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
    let peak_dbfs = dbfs(peak);
    let clipped = f.iter().filter(|s| s.abs() >= CLIP_LEVEL).count();
    let overdriven = clipped as f32 / f.len() as f32 > CLIP_RATIO;
    let underdriven = peak_dbfs < UNDERDRIVEN_PEAK_DB;

    // Noise floor: quietest decile of 20 ms frame RMS, measured on a copy
    // high-passed at IGNORE_BELOW_HZ so legitimate sub-audible PL/CTCSS
    // energy doesn't inflate it. Peak and clipping stay on the RAW samples:
    // overdrive/level are raw-domain facts.
    let hp = highpass_300(&f, sample_rate);
    let frame_len = (sample_rate as usize / 50).max(1);
    let mut frame_rms: Vec<f32> = hp
        .chunks(frame_len)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    frame_rms.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in RMS"));
    let decile = &frame_rms[..(frame_rms.len() / 10).max(1)];
    let noise_floor_dbfs = dbfs(decile.iter().sum::<f32>() / decile.len() as f32);

    // Tone spurs: Goertzel at 588 Hz multiples below Nyquist, measured as the
    // tone's RMS level vs a spectral floor (see `spectral_floor_dbfs` for why
    // this isn't just `noise_floor_dbfs`).
    let nyquist = sample_rate as f32 / 2.0;
    let floor_for_tones = spectral_floor_dbfs(&f, sample_rate, nyquist, noise_floor_dbfs);
    let mut tones = Vec::new();
    let mut k = 1.0_f32;
    while TONE_BASE_HZ * k < nyquist {
        let freq = TONE_BASE_HZ * k;
        // Sub-300 Hz is PL/CTCSS territory, never a spur (explicit guard:
        // today's 588 Hz grid already starts above the line).
        if freq >= IGNORE_BELOW_HZ {
            let level = dbfs(goertzel_rms(&f, freq, sample_rate));
            if level - floor_for_tones >= TONE_OVER_FLOOR_DB && !underdriven {
                tones.push((freq, level - floor_for_tones));
            }
        }
        k += 1.0;
    }
    // Red on hard faults or a severe spur; yellow on any reported spur or a
    // marginal level/SNR; green otherwise.
    let severe_spur = tones.iter().any(|t| t.1 >= RED_TONE_OVER_FLOOR_DB);
    let grade = if overdriven || underdriven || severe_spur {
        Grade::Red
    } else if !tones.is_empty()
        || !(YELLOW_QUIET_PEAK_DB..=YELLOW_HOT_PEAK_DB).contains(&peak_dbfs)
        || (peak_dbfs - noise_floor_dbfs) < YELLOW_MIN_SNR_DB
    {
        Grade::Yellow
    } else {
        Grade::Green
    };
    SignalReport {
        grade,
        peak_dbfs,
        noise_floor_dbfs,
        overdriven,
        underdriven,
        tones,
    }
}

/// Spectral floor used specifically as the comparison baseline for tone-spur
/// detection.
///
/// `noise_floor_dbfs` (quietest decile of frame RMS, a *time-domain* level
/// measure) works for the headline "how loud is the background" report, but
/// it under-serves spur detection: a recording that is *entirely* one or two
/// steady tones (no pauses at all) has a frame RMS that sits near the tone's
/// own level in every frame, so "level above `noise_floor_dbfs`" collapses
/// toward zero even though the tone obviously stands out from everything
/// else in the spectrum. Instead, sample several off-grid control
/// frequencies — the odd half-multiples of the 588 Hz grid (294, 882, 1470,
/// …) — that are never candidate spur frequencies themselves and, for a
/// stationary sinusoid, receive negligible Goertzel leakage. Their median
/// Goertzel level is a much better estimate of "everything that isn't a
/// spur." Control points below [`IGNORE_BELOW_HZ`] are skipped (294 Hz sits
/// in PL/CTCSS-adjacent territory; a strong sub-audible tone must not skew
/// the floor). Falls back to `fallback` (the time-domain noise floor) if the
/// Nyquist range is too narrow to hold a single control point.
fn spectral_floor_dbfs(f: &[f32], sample_rate: u32, nyquist: f32, fallback: f32) -> f32 {
    let mut levels = Vec::new();
    let mut k = 0.0_f32;
    loop {
        let freq = TONE_BASE_HZ / 2.0 + TONE_BASE_HZ * k;
        if freq >= nyquist {
            break;
        }
        if freq >= IGNORE_BELOW_HZ {
            levels.push(goertzel_rms(f, freq, sample_rate));
        }
        k += 1.0;
    }
    if levels.is_empty() {
        return fallback;
    }
    levels.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in Goertzel level"));
    dbfs(levels[levels.len() / 2])
}

fn dbfs(v: f32) -> f32 {
    20.0 * v.max(1e-9).log10()
}

/// 2nd-order Butterworth high-pass at [`IGNORE_BELOW_HZ`] (RBJ audio-EQ
/// cookbook coefficients, Q = 1/sqrt(2)), direct form I, zero initial state.
/// Used only for the noise-floor measurement so sub-audible PL/CTCSS energy
/// doesn't inflate it (~22 dB down at 85 Hz).
fn highpass_300(f: &[f32], sample_rate: u32) -> Vec<f32> {
    let w = std::f32::consts::TAU * IGNORE_BELOW_HZ / sample_rate as f32;
    let (sin_w, cos_w) = w.sin_cos();
    let alpha = sin_w / std::f32::consts::SQRT_2; // sin(w) / (2 * Q), Q = 1/sqrt(2)
    let a0 = 1.0 + alpha;
    let b0 = f32::midpoint(1.0, cos_w) / a0;
    let b1 = -(1.0 + cos_w) / a0;
    let b2 = b0;
    let a1 = -2.0 * cos_w / a0;
    let a2 = (1.0 - alpha) / a0;
    let (mut x1, mut x2, mut y1, mut y2) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
    f.iter()
        .map(|&x| {
            let y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            x2 = x1;
            x1 = x;
            y2 = y1;
            y1 = y;
            y
        })
        .collect()
}

/// RMS amplitude of the DFT bin nearest `freq` over the whole buffer
/// (block-averaged Goertzel, 4096-sample blocks — same technique as the DTMF
/// decoder in astar-codec).
fn goertzel_rms(f: &[f32], freq: f32, sample_rate: u32) -> f32 {
    const BLOCK: usize = 4096;
    let mut acc = 0.0_f32;
    let mut blocks = 0usize;
    for chunk in f.chunks(BLOCK) {
        if chunk.len() < BLOCK / 4 {
            continue; // ignore a short tail
        }
        let n = chunk.len();
        let k = (freq * n as f32 / sample_rate as f32).round();
        let w = std::f32::consts::TAU * k / n as f32;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0_f32, 0.0_f32);
        for &x in chunk {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (n as f32 * n as f32 / 4.0);
        acc += power.max(0.0).sqrt() / std::f32::consts::SQRT_2; // RMS of the tone
        blocks += 1;
    }
    if blocks == 0 {
        0.0
    } else {
        acc / blocks as f32
    }
}

/// Round to the nearest whole dB and render TTS-friendly ("minus 6", not "-6").
fn spoken_db(v: f32) -> String {
    let n = v.round() as i32;
    if n < 0 {
        format!("minus {}", -n)
    } else {
        format!("{n}")
    }
}

/// Spoken codec name, bit depth, and sample rate (Hz) for the report's
/// closing sentence (iax-9722). Depth is 8 for G.711 (companded), 16 for
/// signed-linear; the rate disambiguates slin (8 kHz) from slin16 (16 kHz).
fn spoken_codec(f: VoiceFormat) -> (&'static str, u8, u32) {
    let rate = f.sample_rate().unwrap_or(8000);
    match f {
        VoiceFormat::G711U => ("G 711 u-law", 8, rate),
        VoiceFormat::G711A => ("G 711 a-law", 8, rate),
        VoiceFormat::Slin | VoiceFormat::Slin16 => ("slin", 16, rate),
        _ => ("unknown", 16, rate),
    }
}

/// The English renderer: [`SignalReport`] -> a TTS-friendly sentence.
///
/// Opens with the grade, then whole-dB numbers with "minus" spelled out;
/// the over/too-quiet sentence is omitted when neither applies and tone
/// sentences are omitted when none were detected. `codec`, when given,
/// appends a closing sentence naming the negotiated codec/depth/rate
/// (iax-9722). Example:
/// "Signal report red. Peak minus 6 d B. Noise floor minus 52 d B. Audio is
///  overdriven. Tone detected at 588 hertz. Codec slin, 16 bit, 8 kilohertz."
#[must_use]
pub fn render_report(r: &SignalReport, codec: Option<VoiceFormat>) -> String {
    use std::fmt::Write as _;

    let grade_word = match r.grade {
        Grade::Green => "green",
        Grade::Yellow => "yellow",
        Grade::Red => "red",
    };
    let mut s = format!(
        "Signal report {grade_word}. Peak {} d B. Noise floor {} d B.",
        spoken_db(r.peak_dbfs),
        spoken_db(r.noise_floor_dbfs)
    );
    if r.overdriven {
        s.push_str(" Audio is overdriven.");
    } else if r.underdriven {
        s.push_str(" Audio is too quiet.");
    }
    for &(freq, _) in &r.tones {
        let _ = write!(s, " Tone detected at {} hertz.", freq.round() as i32);
    }
    if let Some(f) = codec {
        let (name, bits, rate) = spoken_codec(f);
        let _ = write!(s, " Codec {name}, {bits} bit, {} kilohertz.", rate / 1000);
    }
    s
}

/// Sound in, English out — the whole report as one pure function so it is
/// unit-testable end to end.
#[must_use]
pub fn signal_report_text(pcm: &[i16], sample_rate: u32, codec: Option<VoiceFormat>) -> String {
    render_report(&analyze_signal(pcm, sample_rate), codec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_sentence_appended_per_format() {
        use astar_iax_core::VoiceFormat::{G711A, G711U, Slin, Slin16};
        let clean = SignalReport {
            grade: Grade::Green,
            peak_dbfs: -10.0,
            noise_floor_dbfs: -50.0,
            overdriven: false,
            underdriven: false,
            tones: vec![],
        };
        assert!(
            render_report(&clean, Some(G711U)).ends_with(" Codec G 711 u-law, 8 bit, 8 kilohertz.")
        );
        assert!(
            render_report(&clean, Some(G711A)).ends_with(" Codec G 711 a-law, 8 bit, 8 kilohertz.")
        );
        assert!(render_report(&clean, Some(Slin)).ends_with(" Codec slin, 16 bit, 8 kilohertz."));
        assert!(
            render_report(&clean, Some(Slin16)).ends_with(" Codec slin, 16 bit, 16 kilohertz.")
        );
        // None → no codec sentence, report otherwise unchanged.
        let none = render_report(&clean, None);
        assert!(!none.contains("Codec"));
        assert_eq!(
            none,
            "Signal report green. Peak minus 10 d B. Noise floor minus 50 d B."
        );
    }

    fn sine(freq: f32, rate: u32, secs: f32, amp: f32) -> Vec<i16> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin() * amp * 32767.0)
            .map(|s| s.clamp(-32767.0, 32767.0) as i16)
            .collect()
    }

    #[test]
    fn english_report_for_clipped_signal_with_icom_spur() {
        let mut pcm = sine(588.0, 16000, 1.0, 1.4); // clips AND carries the spur
        let quiet: Vec<i16> = sine(1000.0, 16000, 1.0, 0.001);
        pcm.extend(quiet); // a quiet tail gives a real noise floor
        let text = signal_report_text(&pcm, 16000, None);
        assert!(text.starts_with("Signal report red. Peak "), "{text}");
        assert!(text.contains("Audio is overdriven."), "{text}");
        assert!(text.contains("Tone detected at 588 hertz."), "{text}");
    }

    #[test]
    fn english_report_for_clean_audio_has_no_extra_sentences() {
        let pcm = sine(1000.0, 8000, 1.0, 0.3);
        let text = signal_report_text(&pcm, 8000, None);
        assert!(text.contains("Peak minus "), "{text}");
        assert!(text.contains("Noise floor minus "), "{text}");
        assert!(
            !text.contains("overdriven") && !text.contains("too quiet"),
            "{text}"
        );
        assert!(!text.contains("Tone detected"), "{text}");
    }

    #[test]
    fn english_report_for_quiet_audio_says_too_quiet() {
        let pcm = sine(440.0, 8000, 1.0, 0.01);
        let text = signal_report_text(&pcm, 8000, None);
        assert!(text.starts_with("Signal report red."), "{text}");
        assert!(text.contains("Audio is too quiet."), "{text}");
    }

    #[test]
    fn grade_green_for_clean_audio_with_real_noise_floor() {
        // A healthy level (~-10.5 dBFS peak) plus a near-silent tail so the
        // quietest-decile noise floor is genuinely low -> high SNR -> green.
        // (A pause-free continuous tone grades yellow instead: its noise
        // floor is its own RMS, so peak-over-floor is only ~3 dB.)
        let mut pcm = sine(1000.0, 8000, 1.0, 0.3);
        let quiet: Vec<i16> = sine(1000.0, 8000, 1.0, 0.001);
        pcm.extend(quiet);
        let r = analyze_signal(&pcm, 8000);
        assert_eq!(r.grade, Grade::Green, "{r:?}");
        let text = render_report(&r, None);
        assert!(text.starts_with("Signal report green."), "{text}");
    }

    #[test]
    fn grade_red_for_clipped_signal() {
        let pcm = sine(440.0, 8000, 1.0, 1.6);
        let r = analyze_signal(&pcm, 8000);
        assert_eq!(r.grade, Grade::Red, "{r:?}");
        let text = render_report(&r, None);
        assert!(text.starts_with("Signal report red."), "{text}");
        assert!(text.contains("Audio is overdriven."), "{text}");
    }

    #[test]
    fn grade_yellow_for_marginal_level() {
        // ~-26 dBFS peak: quieter than the -20 dB yellow line but louder
        // than the -30 dB underdriven (red) line.
        let pcm = sine(1000.0, 8000, 1.0, 0.05);
        let r = analyze_signal(&pcm, 8000);
        assert_eq!(r.grade, Grade::Yellow, "{r:?}");
        let text = render_report(&r, None);
        assert!(text.starts_with("Signal report yellow."), "{text}");
    }

    #[test]
    fn grade_red_for_588_spur() {
        let mut pcm = sine(588.0, 16000, 1.0, 0.25);
        let h: Vec<i16> = sine(1764.0, 16000, 1.0, 0.15);
        for (a, b) in pcm.iter_mut().zip(h) {
            *a = a.saturating_add(b);
        }
        let r = analyze_signal(&pcm, 16000);
        assert_eq!(r.grade, Grade::Red, "{r:?}");
    }

    /// Mix `extra` into `base` in place with saturating adds.
    fn mix(base: &mut [i16], extra: &[i16]) {
        for (a, &b) in base.iter_mut().zip(extra) {
            *a = a.saturating_add(b);
        }
    }

    #[test]
    fn spur_in_20_to_30_db_band_grades_yellow() {
        // Pin the spectral floor with a comb of tones AT the off-grid control
        // frequencies (882, 1470, ... Hz), so the spur's dB-over-floor is the
        // amplitude ratio: 20*log10(0.178/0.01) ~= 25 dB -> the yellow band.
        let mut pcm = sine(588.0, 8000, 1.0, 0.178);
        for f in [882.0, 1470.0, 2058.0, 2646.0, 3234.0, 3822.0] {
            mix(&mut pcm, &sine(f, 8000, 1.0, 0.01));
        }
        let r = analyze_signal(&pcm, 8000);
        let spur = r
            .tones
            .iter()
            .find(|t| (t.0 - 588.0).abs() < 1.0)
            .unwrap_or_else(|| panic!("588 spur reported: {r:?}"));
        assert!(
            spur.1 >= TONE_OVER_FLOOR_DB && spur.1 < RED_TONE_OVER_FLOOR_DB,
            "spur in the yellow band, got {} dB: {r:?}",
            spur.1
        );
        assert_eq!(r.grade, Grade::Yellow, "{r:?}");
        let text = render_report(&r, None);
        assert!(text.starts_with("Signal report yellow."), "{text}");
        assert!(text.contains("Tone detected at 588 hertz."), "{text}");
    }

    #[test]
    fn spur_at_or_above_30_db_over_floor_grades_red() {
        // Same comb floor, spur 40 dB over it: 20*log10(0.4/0.004) = 40.
        let mut pcm = sine(588.0, 8000, 1.0, 0.4);
        for f in [882.0, 1470.0, 2058.0, 2646.0, 3234.0, 3822.0] {
            mix(&mut pcm, &sine(f, 8000, 1.0, 0.004));
        }
        let r = analyze_signal(&pcm, 8000);
        assert_eq!(r.grade, Grade::Red, "{r:?}");
        let text = render_report(&r, None);
        assert!(text.starts_with("Signal report red."), "{text}");
        assert!(text.contains("Tone detected at 588 hertz."), "{text}");
    }

    #[test]
    fn loud_pl_tone_does_not_inflate_noise_floor() {
        // Green fixture with a realistic channel floor (tail amp 0.035,
        // ~-32 dBFS) -- against the artificial -63 dB floor of the 0.001
        // tail, a 2nd-order HPF's ~22 dB of attenuation at 85 Hz could
        // never land the residual within 3 dB. PL amp 0.35 (~-9 dBFS, 10x
        // the tail tone) keeps the raw summed peak (0.65 -> -3.7 dBFS)
        // under the -3 dBFS yellow-hot line; 0.4 would leave 0.1 dB margin.
        let mut clean = sine(1000.0, 8000, 1.0, 0.3);
        clean.extend(sine(1000.0, 8000, 1.0, 0.035));
        let r_clean = analyze_signal(&clean, 8000);

        let mut pcm = clean.clone();
        mix(&mut pcm, &sine(85.0, 8000, 2.0, 0.35));
        let r = analyze_signal(&pcm, 8000);
        assert_eq!(r.grade, Grade::Green, "{r:?}");
        assert!(
            (r.noise_floor_dbfs - r_clean.noise_floor_dbfs).abs() < 3.0,
            "floor {} dB should be within 3 dB of clean floor {} dB",
            r.noise_floor_dbfs,
            r_clean.noise_floor_dbfs
        );
    }

    #[test]
    fn pl_tone_below_300_hz_does_not_affect_report() {
        // Repeater PL/CTCSS (~85-100 Hz, sub-audible) is legitimate: an 85 Hz
        // tone riding under otherwise-green audio must not flip the grade or
        // produce a tone sentence. 0.02 amplitude is ~29 dB above the quiet
        // tail's clean floor -- a solid "tone" if it were in-band.
        let mut pcm = sine(1000.0, 8000, 1.0, 0.3);
        pcm.extend(sine(1000.0, 8000, 1.0, 0.001));
        let pl = sine(85.0, 8000, 2.0, 0.02);
        mix(&mut pcm, &pl);
        let r = analyze_signal(&pcm, 8000);
        assert_eq!(r.grade, Grade::Green, "{r:?}");
        let text = render_report(&r, None);
        assert!(text.starts_with("Signal report green."), "{text}");
        assert!(!text.contains("Tone detected"), "{text}");
    }

    #[test]
    fn clean_tone_is_neither_over_nor_underdriven_and_has_no_588_spurs() {
        let pcm = sine(1000.0, 8000, 1.0, 0.3); // ~-10 dBFS
        let r = analyze_signal(&pcm, 8000);
        assert!(!r.overdriven && !r.underdriven);
        assert!(
            r.tones.is_empty(),
            "1 kHz is not a 588 multiple: {:?}",
            r.tones
        );
        assert!(
            (r.peak_dbfs - -10.5).abs() < 1.5,
            "peak ~-10.5 dBFS, got {}",
            r.peak_dbfs
        );
    }

    #[test]
    fn clipped_signal_is_overdriven() {
        let pcm = sine(440.0, 8000, 1.0, 1.6); // drives past full scale -> clamps
        let r = analyze_signal(&pcm, 8000);
        assert!(r.overdriven);
        assert!(!r.underdriven);
    }

    #[test]
    fn quiet_signal_is_underdriven() {
        let pcm = sine(440.0, 8000, 1.0, 0.01); // ~-40 dBFS
        let r = analyze_signal(&pcm, 8000);
        assert!(r.underdriven);
        assert!(!r.overdriven);
    }

    #[test]
    fn fake_icom_tone_at_588_and_harmonic_detected() {
        // Speechy noise floor + strong 588 Hz and 1764 Hz spurs.
        let mut pcm = sine(588.0, 16000, 1.0, 0.25);
        let h: Vec<i16> = sine(1764.0, 16000, 1.0, 0.15);
        for (a, b) in pcm.iter_mut().zip(h) {
            *a = a.saturating_add(b);
        }
        let r = analyze_signal(&pcm, 16000);
        let freqs: Vec<u32> = r.tones.iter().map(|t| t.0.round() as u32).collect();
        assert!(freqs.contains(&588), "588 detected: {freqs:?}");
        assert!(freqs.contains(&1764), "1764 detected: {freqs:?}");
    }

    #[test]
    fn empty_recording_reports_silence() {
        let r = analyze_signal(&[], 8000);
        assert!(r.underdriven);
        assert!(r.tones.is_empty());
    }
}
