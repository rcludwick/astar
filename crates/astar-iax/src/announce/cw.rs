// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! In-DSP Morse code generation for CW station ID (iax-6c5d).

/// Morse table for the characters a callsign/node id needs (A–Z, 0–9).
fn morse(c: char) -> Option<&'static str> {
    Some(match c.to_ascii_uppercase() {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        _ => return None,
    })
}

/// Generate PCM at `sample_rate` for `text` at `wpm` words/min and `tone_hz`
/// sidetone. Standard timing: dot = 1200/wpm ms; dash = 3 dots; intra-char
/// gap = 1 dot; inter-char gap = 3 dots; word gap = 7 dots.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // audio-sample conversions: intentional float→int casts
pub(crate) fn cw_pcm(text: &str, wpm: u32, tone_hz: f32, sample_rate: u32) -> Vec<i16> {
    let wpm = wpm.max(1);
    let fs = sample_rate as f32;
    #[allow(clippy::cast_precision_loss)] // wpm≤u32, fits f32 without loss at any realistic WPM
    let dot_samples = ((1_200.0 / wpm as f32) / 1_000.0 * fs) as usize;
    let mut out = Vec::new();
    let mut phase = 0.0_f32;
    let step = std::f32::consts::TAU * tone_hz / fs;
    let tone = |n: usize, out: &mut Vec<i16>, phase: &mut f32| {
        for _ in 0..n {
            let s = (phase.sin() * 0.5 * 32767.0) as i16;
            out.push(s);
            *phase += step;
        }
    };
    let gap = |n: usize, out: &mut Vec<i16>| out.extend(std::iter::repeat_n(0_i16, n));
    for (wi, word) in text.split_whitespace().enumerate() {
        if wi > 0 {
            gap(dot_samples * 7, &mut out);
        }
        for (ci, ch) in word.chars().enumerate() {
            let Some(code) = morse(ch) else { continue };
            if ci > 0 {
                gap(dot_samples * 3, &mut out);
            }
            for (si, sym) in code.chars().enumerate() {
                if si > 0 {
                    gap(dot_samples, &mut out);
                }
                let n = if sym == '-' {
                    dot_samples * 3
                } else {
                    dot_samples
                };
                tone(n, &mut out, &mut phase);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cw_e_is_one_dot_of_tone() {
        // 'E' = a single dot. At 20 wpm, dot = 60 ms = 480 samples, all tone.
        let pcm = cw_pcm("E", 20, 800.0, 8_000);
        assert_eq!(pcm.len(), 480, "one dot at 20 wpm = 480 samples");
        assert!(pcm.iter().any(|&s| s.abs() > 1_000), "dot carries tone");
    }

    #[test]
    fn unknown_chars_are_skipped_not_panicked() {
        let _ = cw_pcm("A!@#1", 20, 800.0, 8_000); // must not panic
    }

    #[test]
    #[allow(clippy::cast_possible_wrap)] // test-only: PCM lengths are tiny; cast is safe
    fn cw_pcm_scales_with_sample_rate() {
        let n8 = cw_pcm("E", 20, 700.0, 8_000).len();
        let n16 = cw_pcm("E", 20, 700.0, 16_000).len();
        // Same wall-clock duration → twice the samples at 16 kHz (±1 for rounding).
        assert!((n16 as i64 - 2 * n8 as i64).abs() <= 2, "n8={n8} n16={n16}");
    }
}
