// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! In-band DTMF (Dual-Tone Multi-Frequency) generator and decoder.
//!
//! This is the *audio* side of DTMF — synthesizing the dual-sine tone pair and
//! detecting tones in a PCM stream — as opposed to the out-of-band IAX2 DTMF
//! control frames (which carry the digit in the frame subclass). Radios and
//! `AllStarLink` nodes routinely signal `*`-commands as in-band tones, so a UI
//! client needs both directions.
//!
//! Everything operates on 16-bit linear PCM at 8 kHz (the G.711 wire rate), so
//! callers tap the decoded stream before any resample to the device rate.

// DSP math mixes f32 with usize/i16/u32 on nearly every line; these casts are
// intentional and bounds-checked by construction, so allow them module-wide
// rather than scattering local `#[allow]`s (same pattern as `g711`).
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

/// Native sample rate for IAX2 audio (and this module's default).
pub const DTMF_SAMPLE_RATE: u32 = 8000;

/// Default per-tone level (dBFS) for the generator. Each of the two tones sits
/// at this level, so the summed peak stays well clear of full-scale clipping.
pub const DEFAULT_TONE_DBFS: f32 = -12.0;

/// The standard DTMF keypad, row (low group) by column (high group).
const KEYPAD: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// Low-group (row) frequencies in Hz.
const LOW_FREQS: [u16; 4] = [697, 770, 852, 941];
/// High-group (column) frequencies in Hz.
const HIGH_FREQS: [u16; 4] = [1209, 1336, 1477, 1633];

/// The `(low, high)` frequency pair (Hz) for a DTMF digit, or `None` if `digit`
/// is not one of the 16 valid keys (`0-9`, `A-D`, `*`, `#`).
#[must_use]
pub fn digit_frequencies(digit: char) -> Option<(u16, u16)> {
    for (row, keys) in KEYPAD.iter().enumerate() {
        for (col, &key) in keys.iter().enumerate() {
            if key == digit {
                return Some((LOW_FREQS[row], HIGH_FREQS[col]));
            }
        }
    }
    None
}

/// Whether `c` is one of the 16 valid DTMF keys.
#[must_use]
pub fn is_dtmf_digit(c: char) -> bool {
    digit_frequencies(c).is_some()
}

/// A streaming DTMF tone source: emits a phase-continuous dual-tone for one
/// digit, a fixed number of samples, then goes silent (`next_block` returns 0).
///
/// Use [`DtmfGenerator::next_block`] to mix the tone into a live stream (TX
/// inject), or [`synthesize`] for a one-shot buffer (local sidetone).
#[derive(Debug, Clone)]
pub struct DtmfGenerator {
    /// Phase increment per sample (radians) for the low and high tones.
    step_low: f32,
    step_high: f32,
    /// Running phase accumulators, kept continuous across `next_block` calls.
    phase_low: f32,
    phase_high: f32,
    /// Per-tone peak amplitude in i16 units.
    amplitude: f32,
    /// Samples still to emit.
    remaining: usize,
}

impl DtmfGenerator {
    /// Build a generator for `digit` at `sample_rate`, lasting `duration_ms`,
    /// with each tone at `level_dbfs`. Returns `None` for a non-DTMF digit.
    #[must_use]
    pub fn new(digit: char, sample_rate: u32, duration_ms: u32, level_dbfs: f32) -> Option<Self> {
        let (low, high) = digit_frequencies(digit)?;
        let sr = sample_rate as f32;
        let step = |freq: u16| std::f32::consts::TAU * f32::from(freq) / sr;
        let amplitude = 10.0_f32.powf(level_dbfs / 20.0) * f32::from(i16::MAX);
        let remaining = (u64::from(sample_rate) * u64::from(duration_ms) / 1000) as usize;
        Some(Self {
            step_low: step(low),
            step_high: step(high),
            phase_low: 0.0,
            phase_high: 0.0,
            amplitude,
            remaining,
        })
    }

    /// Fill `out` with up to its length of tone samples; returns how many were
    /// written (0 once the tone is exhausted). Phase-continuous across calls.
    pub fn next_block(&mut self, out: &mut [i16]) -> usize {
        let n = out.len().min(self.remaining);
        for slot in &mut out[..n] {
            let s = self.amplitude * (self.phase_low.sin() + self.phase_high.sin());
            *slot = s.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
            // Advance and wrap the phases so f32 precision holds over long tones.
            self.phase_low = (self.phase_low + self.step_low) % std::f32::consts::TAU;
            self.phase_high = (self.phase_high + self.step_high) % std::f32::consts::TAU;
        }
        self.remaining -= n;
        n
    }

    /// Whether the full tone has been emitted.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.remaining == 0
    }
}

/// Synthesize the whole tone for `digit` as a single buffer (e.g. for local
/// sidetone). Returns `None` for a non-DTMF digit. See [`DtmfGenerator`] for
/// the streaming form.
#[must_use]
pub fn synthesize(
    digit: char,
    sample_rate: u32,
    duration_ms: u32,
    level_dbfs: f32,
) -> Option<Vec<i16>> {
    let mut tonegen = DtmfGenerator::new(digit, sample_rate, duration_ms, level_dbfs)?;
    let mut buf = vec![0_i16; tonegen.remaining];
    let n = tonegen.next_block(&mut buf);
    buf.truncate(n);
    Some(buf)
}

/// The Goertzel coefficient `2·cos(2πk/N)` for the DFT bin `k` nearest `freq`.
/// Snapping to the nearest bin (the canonical DTMF-detector trick) keeps the
/// tone energy concentrated in one bin and minimizes spectral leakage.
fn goertzel_coeff(freq: u16, sample_rate: u32, block: usize) -> f32 {
    let k = (block as f32 * f32::from(freq) / sample_rate as f32).round();
    2.0 * (std::f32::consts::TAU * k / block as f32).cos()
}

/// Streaming in-band DTMF detector.
///
/// Feed it PCM with [`DtmfDecoder::process`]; it buffers into fixed analysis
/// blocks, runs a Goertzel filter per DTMF frequency, and emits a digit on the
/// rising edge of each detected tone (debounced, so one keypress yields one
/// digit no matter how long it's held).
#[derive(Debug, Clone)]
pub struct DtmfDecoder {
    /// Analysis block length in samples (~25 ms).
    block_size: usize,
    /// Goertzel coefficients: indices 0..4 low group, 4..8 high group.
    coeffs: [f32; 8],
    /// Samples not yet consumed into a full block.
    buf: Vec<i16>,
    /// Debounce: the candidate decision and how many consecutive blocks agreed.
    pending: Option<char>,
    pending_count: u32,
    /// The currently-confirmed tone (`None` = silence/no tone).
    confirmed: Option<char>,
}

/// Consecutive equal block decisions required to flip the confirmed state.
const DETECT_BLOCKS: u32 = 2;

impl DtmfDecoder {
    /// A decoder for `sample_rate` (use [`DTMF_SAMPLE_RATE`] for IAX2 audio).
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let block_size = (sample_rate as usize * 205 / 8000).max(64);
        let mut coeffs = [0.0_f32; 8];
        for (c, &f) in coeffs[..4].iter_mut().zip(&LOW_FREQS) {
            *c = goertzel_coeff(f, sample_rate, block_size);
        }
        for (c, &f) in coeffs[4..].iter_mut().zip(&HIGH_FREQS) {
            *c = goertzel_coeff(f, sample_rate, block_size);
        }
        Self {
            block_size,
            coeffs,
            buf: Vec::new(),
            pending: None,
            pending_count: 0,
            confirmed: None,
        }
    }

    /// Feed `samples`; returns any digits detected (rising edges) this call.
    pub fn process(&mut self, samples: &[i16]) -> Vec<char> {
        self.buf.extend_from_slice(samples);
        let mut detected = Vec::new();
        while self.buf.len() >= self.block_size {
            let decision = self.decode_block(&self.buf[..self.block_size]);
            self.buf.drain(..self.block_size);

            // Debounce: a state change must persist DETECT_BLOCKS blocks; emit
            // only on a transition INTO a tone (rising edge), so one keypress
            // yields one digit however long it is held.
            if decision == self.pending {
                self.pending_count += 1;
            } else {
                self.pending = decision;
                self.pending_count = 1;
            }
            if self.pending_count == DETECT_BLOCKS && self.confirmed != self.pending {
                self.confirmed = self.pending;
                if let Some(digit) = self.confirmed {
                    detected.push(digit);
                }
            }
        }
        detected
    }

    /// Classify one analysis block as a DTMF digit, or `None` for
    /// silence/noise/voice/single-tone. Goertzel power per frequency, then a
    /// dominant tone in each group carrying a real share of the block energy,
    /// within twist limits.
    fn decode_block(&self, block: &[i16]) -> Option<char> {
        let energy: f32 = block.iter().map(|&x| f32::from(x) * f32::from(x)).sum();
        let n = block.len() as f32;
        if energy / n < MIN_MEAN_ENERGY {
            return None; // silence
        }

        let mut powers = [0.0_f32; 8];
        for (p, &coeff) in powers.iter_mut().zip(&self.coeffs) {
            let (mut s1, mut s2) = (0.0_f32, 0.0_f32);
            for &x in block {
                let s = f32::from(x) + coeff * s1 - s2;
                s2 = s1;
                s1 = s;
            }
            *p = s1.mul_add(s1, s2 * s2) - coeff * s1 * s2;
        }

        let (li, lp) = argmax(&powers[..4]);
        let (hi, hp) = argmax(&powers[4..]);

        // Parseval: sum over all N bins of |X_k|^2 == N * energy. So each
        // tone's share of the total spectral energy is power / (N * energy).
        let spectral = n * energy;
        if lp / spectral < MIN_TONE_SHARE || hp / spectral < MIN_TONE_SHARE {
            return None; // a group is missing its tone (single tone / noise / twist)
        }

        Some(KEYPAD[li][hi])
    }
}

/// Index and value of the largest element in a non-empty slice.
fn argmax(p: &[f32]) -> (usize, f32) {
    let mut best = (0, p[0]);
    for (i, &v) in p.iter().enumerate().skip(1) {
        if v > best.1 {
            best = (i, v);
        }
    }
    best
}

/// Reject a block whose mean per-sample energy is below this (silence floor).
const MIN_MEAN_ENERGY: f32 = 100.0;
/// Each selected tone must carry at least this share of the block's spectral
/// energy — rejects noise, voice, lone single tones, and pairs with one tone
/// too weak (excessive twist).
const MIN_TONE_SHARE: f32 = 0.05;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_5_maps_to_770_and_1336() {
        assert_eq!(digit_frequencies('5'), Some((770, 1336)));
    }

    #[test]
    fn keypad_corners_and_symbols_map() {
        assert_eq!(digit_frequencies('1'), Some((697, 1209)));
        assert_eq!(digit_frequencies('A'), Some((697, 1633)));
        assert_eq!(digit_frequencies('*'), Some((941, 1209)));
        assert_eq!(digit_frequencies('0'), Some((941, 1336)));
        assert_eq!(digit_frequencies('#'), Some((941, 1477)));
        assert_eq!(digit_frequencies('D'), Some((941, 1633)));
    }

    #[test]
    fn non_dtmf_chars_have_no_frequencies() {
        assert_eq!(digit_frequencies('E'), None);
        assert_eq!(digit_frequencies('x'), None);
        assert_eq!(digit_frequencies(' '), None);
    }

    #[test]
    fn is_dtmf_digit_accepts_keys_and_rejects_others() {
        assert!(is_dtmf_digit('7'));
        assert!(is_dtmf_digit('#'));
        assert!(is_dtmf_digit('B'));
        assert!(!is_dtmf_digit('E'));
        assert!(!is_dtmf_digit('z'));
    }

    // ---------- generator ----------

    #[test]
    fn synthesize_yields_duration_in_samples() {
        // 100 ms at 8 kHz = 800 samples.
        let tone = synthesize('5', 8000, 100, DEFAULT_TONE_DBFS).expect("valid digit");
        assert_eq!(tone.len(), 800);
    }

    #[test]
    fn synthesize_rejects_non_dtmf_digit() {
        assert!(synthesize('E', 8000, 100, DEFAULT_TONE_DBFS).is_none());
    }

    #[test]
    fn synthesized_tone_is_audible_but_never_clips() {
        let tone = synthesize('9', 8000, 50, DEFAULT_TONE_DBFS).expect("valid digit");
        let peak = tone.iter().map(|s| s.unsigned_abs()).max().unwrap();
        // Two -12 dBFS tones sum to ~ -6 dBFS peak: loud, but short of full scale.
        assert!(peak > 8_000, "tone too quiet: peak {peak}");
        assert!(peak < 30_000, "tone clipping risk: peak {peak}");
    }

    #[test]
    fn next_block_is_phase_continuous_and_matches_one_shot() {
        let one_shot = synthesize('*', 8000, 40, DEFAULT_TONE_DBFS).expect("valid digit");

        let mut tonegen =
            DtmfGenerator::new('*', 8000, 40, DEFAULT_TONE_DBFS).expect("valid digit");
        let mut streamed = Vec::new();
        let mut chunk = [0_i16; 70]; // awkward size to cross block boundaries
        loop {
            let n = tonegen.next_block(&mut chunk);
            if n == 0 {
                break;
            }
            streamed.extend_from_slice(&chunk[..n]);
        }

        assert_eq!(
            streamed, one_shot,
            "streamed tone must equal the one-shot tone"
        );
        assert!(tonegen.is_done());
    }

    #[test]
    fn generator_exhausts_after_its_duration() {
        let mut tonegen =
            DtmfGenerator::new('1', 8000, 10, DEFAULT_TONE_DBFS).expect("valid digit");
        let mut buf = [0_i16; 80]; // exactly 10 ms
        assert_eq!(tonegen.next_block(&mut buf), 80);
        assert!(tonegen.is_done());
        assert_eq!(
            tonegen.next_block(&mut buf),
            0,
            "exhausted generator emits nothing"
        );
    }

    // ---------- decoder ----------

    const ALL_DIGITS: [char; 16] = [
        '1', '2', '3', 'A', '4', '5', '6', 'B', '7', '8', '9', 'C', '*', '0', '#', 'D',
    ];

    /// A single pure sine (NOT a valid DTMF pair) for talk-off testing.
    fn single_tone(freq: f32, n: usize, peak: f32) -> Vec<i16> {
        let step = std::f32::consts::TAU * freq / 8000.0;
        (0..n)
            .map(|i| (peak * (step * i as f32).sin()).round() as i16)
            .collect()
    }

    /// Deterministic pseudo-random noise (LCG) at a given peak amplitude.
    fn white_noise(n: usize, peak: i16) -> Vec<i16> {
        let mut state: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let r = (state >> 16) as i16; // -32768..32767-ish
                ((i32::from(r) * i32::from(peak)) / 32768) as i16
            })
            .collect()
    }

    #[test]
    fn decodes_each_of_the_sixteen_digits() {
        for digit in ALL_DIGITS {
            let tone = synthesize(digit, 8000, 100, DEFAULT_TONE_DBFS).expect("valid digit");
            let mut decoder = DtmfDecoder::new(8000);
            let detected = decoder.process(&tone);
            assert_eq!(detected, vec![digit], "round-trip failed for {digit}");
        }
    }

    #[test]
    fn silence_decodes_to_nothing() {
        let mut decoder = DtmfDecoder::new(8000);
        assert!(decoder.process(&[0_i16; 2000]).is_empty());
    }

    #[test]
    fn white_noise_does_not_trigger_a_digit() {
        let mut decoder = DtmfDecoder::new(8000);
        let noise = white_noise(4000, 12_000);
        assert!(
            decoder.process(&noise).is_empty(),
            "white noise produced a false digit"
        );
    }

    /// Two valid DTMF frequencies, but at independent amplitudes.
    fn two_tone(low: f32, high: f32, low_peak: f32, high_peak: f32, n: usize) -> Vec<i16> {
        let lstep = std::f32::consts::TAU * low / 8000.0;
        let hstep = std::f32::consts::TAU * high / 8000.0;
        (0..n)
            .map(|i| {
                let t = i as f32;
                (low_peak * (lstep * t).sin() + high_peak * (hstep * t).sin()).round() as i16
            })
            .collect()
    }

    #[test]
    fn an_imbalanced_tone_pair_is_rejected() {
        // '5' = 770 + 1336, but the high tone is ~30 dB down: not a clean key.
        let mut decoder = DtmfDecoder::new(8000);
        let sig = two_tone(770.0, 1336.0, 9_000.0, 300.0, 2000);
        assert!(
            decoder.process(&sig).is_empty(),
            "a pair with one near-absent tone must not decode"
        );
    }

    #[test]
    fn a_single_tone_is_not_a_digit() {
        // Only one of the two required groups is present -> not a valid DTMF key.
        let mut decoder = DtmfDecoder::new(8000);
        let tone = single_tone(941.0, 2000, 9_000.0);
        assert!(
            decoder.process(&tone).is_empty(),
            "lone tone misread as a digit"
        );
    }

    #[test]
    fn a_held_tone_emits_the_digit_only_once() {
        // 300 ms is many analysis blocks; debounce must still emit a single '5'.
        let tone = synthesize('5', 8000, 300, DEFAULT_TONE_DBFS).expect("valid digit");
        let mut decoder = DtmfDecoder::new(8000);
        assert_eq!(decoder.process(&tone), vec!['5']);
    }

    #[test]
    fn detection_survives_arbitrary_chunk_boundaries() {
        let tone = synthesize('7', 8000, 120, DEFAULT_TONE_DBFS).expect("valid digit");
        let mut decoder = DtmfDecoder::new(8000);
        let mut detected = Vec::new();
        for chunk in tone.chunks(37) {
            detected.extend(decoder.process(chunk));
        }
        assert_eq!(detected, vec!['7']);
    }

    #[test]
    fn two_digits_separated_by_silence_both_emit() {
        let mut stream = synthesize('3', 8000, 100, DEFAULT_TONE_DBFS).expect("valid digit");
        stream.extend(std::iter::repeat_n(0_i16, 400)); // 50 ms gap
        stream.extend(synthesize('9', 8000, 100, DEFAULT_TONE_DBFS).expect("valid digit"));

        let mut decoder = DtmfDecoder::new(8000);
        assert_eq!(decoder.process(&stream), vec!['3', '9']);
    }
}
