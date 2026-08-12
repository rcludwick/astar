// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! In-band DTMF (touch-tone) detection for the conference relay path
//! (iax-8ca0).
//!
//! A node running in bridge mode must treat DTMF arriving on a member leg as
//! command input for THIS node (the iax-d254 DTMF→link-command mapper), not
//! as audio to rebroadcast to the other linked nodes. This module supplies
//! the per-member detector the [`crate::Conference`] tick runs over each 20 ms
//! decoded block: a bank of eight Goertzel filters (the four DTMF row and four
//! column frequencies) plus the validity checks that keep speech and noise
//! from false-triggering.
//!
//! Design:
//! - Goertzel at the EXACT tone frequencies (`2π·f/fs`, not the nearest DFT
//!   bin), so the 20 ms rectangular window costs no scalloping loss — a pure
//!   tone of amplitude `A` measures `≈ (A·N/2)²` regardless of where it falls
//!   between bins. Eight filters per member per tick; no FFT.
//! - A block reports a digit only if ALL of: the row and column tones clear an
//!   absolute level floor, each dominates the other three candidates in its
//!   group, the row/column level ratio (twist) is within limits, and the tone
//!   pair carries most of the block's total energy (speech fails this).
//! - A digit REGISTERS (one edge-triggered event per key-down) only after
//!   [`CONFIRM_BLOCKS`] consecutive agreeing blocks (~40 ms), per the DTMF
//!   minimum-duration convention.
//! - [`DtmfDetector::squelching`] is the relay-mute gate: `true` from the
//!   FIRST block that looks like a tone (so at most one 20 ms block of tone
//!   leaks before the mute engages) through [`SQUELCH_TAIL_BLOCKS`] blocks
//!   after the tone ends.

use std::f32::consts::PI;

/// The four DTMF row (low-group) frequencies in Hz. Row index selects the
/// keypad row of [`DTMF_DIGITS`].
pub const DTMF_ROW_HZ: [f32; 4] = [697.0, 770.0, 852.0, 941.0];

/// The four DTMF column (high-group) frequencies in Hz. Column index selects
/// the keypad column of [`DTMF_DIGITS`].
pub const DTMF_COL_HZ: [f32; 4] = [1209.0, 1336.0, 1477.0, 1633.0];

/// The 16-key DTMF keypad, indexed `[row][col]`.
pub const DTMF_DIGITS: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// Consecutive agreeing 20 ms blocks required before a digit registers
/// (2 blocks ≈ 40 ms — the conventional DTMF minimum tone duration). Blocks
/// before confirmation are still squelched from the relay.
pub const CONFIRM_BLOCKS: u32 = 2;

/// Blocks the relay squelch holds AFTER the tone disappears (~40 ms tail), so
/// a decaying key-release edge never leaks a chirp onto the relay.
pub const SQUELCH_TAIL_BLOCKS: u32 = 2;

/// Consecutive NON-tone blocks required to release a registered key-down
/// (~40 ms), before the same digit may register again (iax-4d19). Without
/// this hysteresis a single dropped/mis-classified block mid-tone — packet
/// jitter, a momentary level dip — reads as a key-release and the SAME
/// keypress registers a second time. Real inter-digit gaps are far longer
/// than one block, so a legitimate repeated digit still registers twice.
pub const RELEASE_BLOCKS: u32 = 2;

/// Absolute per-tone amplitude floor (normalized full scale = 1.0). Each of
/// the two tones must estimate at least this amplitude, ≈ −34 dBFS — well
/// under any real touch-tone but above line noise, so silence and hiss never
/// engage the squelch.
const MIN_TONE_AMP: f32 = 0.02;

/// How strongly the winning row (resp. column) tone must dominate EACH of the
/// other three candidates in its group, as a power ratio (≈ 8 dB). Speech
/// formants smear energy across neighboring candidates and fail this.
const DOMINANCE_RATIO: f32 = 6.3;

/// Maximum twist: the allowed power ratio between the row and column tones,
/// in either direction (≈ 9 dB — deliberately looser than the spec's 4/8 dB
/// forward/reverse twist, since radio links shape the pair unevenly).
const MAX_TWIST: f32 = 8.0;

/// Minimum fraction of the block's TOTAL energy the two tones must carry.
/// A clean tone pair measures ≈ 1.0; voiced speech spreads energy across many
/// harmonics and lands well below this, so it never reads as a digit.
const TONE_TO_TOTAL_MIN: f32 = 0.6;

/// Goertzel power (squared DTFT magnitude, un-normalized) of `block` at
/// exactly `freq_hz`. For a pure tone of amplitude `A` at that frequency this
/// is `≈ (A·N/2)²` (no bin-snapping, so no scalloping loss).
fn goertzel_power(block: &[f32], freq_hz: f32, sample_rate: f32) -> f32 {
    let w = 2.0 * PI * freq_hz / sample_rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0_f32, 0.0_f32);
    for &x in block {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    coeff.mul_add(-(s1 * s2), s1.mul_add(s1, s2 * s2))
}

/// Classify one 20 ms block: `Some(digit)` iff it passes every DTMF validity
/// check (level floor, in-group dominance, twist, tone-vs-total energy).
/// Stateless — the temporal rules (confirmation, edge trigger, squelch tail)
/// live in [`DtmfDetector`].
#[allow(clippy::cast_precision_loss)] // block lengths are tiny (≤ a few hundred)
fn classify_block(block: &[f32], sample_rate: f32) -> Option<char> {
    let n = block.len() as f32;
    if block.is_empty() {
        return None;
    }
    let total_energy: f32 = block.iter().map(|&s| s * s).sum();

    let row_p: Vec<f32> = DTMF_ROW_HZ
        .iter()
        .map(|&f| goertzel_power(block, f, sample_rate))
        .collect();
    let col_p: Vec<f32> = DTMF_COL_HZ
        .iter()
        .map(|&f| goertzel_power(block, f, sample_rate))
        .collect();

    let best = |p: &[f32]| -> (usize, f32) {
        p.iter().copied().enumerate().fold(
            (0, f32::MIN),
            |acc, (i, v)| if v > acc.1 { (i, v) } else { acc },
        )
    };
    let (ri, rp) = best(&row_p);
    let (ci, cp) = best(&col_p);

    // Absolute level floor: estimated amplitude of each tone (power ≈ (A·N/2)²
    // ⇒ A² ≈ 4·power/N²) must clear MIN_TONE_AMP.
    let floor = (MIN_TONE_AMP * n / 2.0).powi(2);
    if rp < floor || cp < floor {
        return None;
    }

    // In-group dominance: the winner must beat EVERY other candidate in its
    // group by DOMINANCE_RATIO.
    let dominated = |p: &[f32], win: usize, wp: f32| {
        p.iter()
            .enumerate()
            .all(|(i, &v)| i == win || wp >= DOMINANCE_RATIO * v)
    };
    if !dominated(&row_p, ri, rp) || !dominated(&col_p, ci, cp) {
        return None;
    }

    // Twist: row/column power ratio bounded both ways.
    if rp > MAX_TWIST * cp || cp > MAX_TWIST * rp {
        return None;
    }

    // Tone-vs-total dominance: the pair's time-domain energy (power·2/N per
    // tone, since Σ sin² = A²·N/2) must carry most of the block.
    let tone_energy = (rp + cp) * 2.0 / n;
    if tone_energy < TONE_TO_TOTAL_MIN * total_energy {
        return None;
    }

    Some(DTMF_DIGITS[ri][ci])
}

/// Streaming per-leg DTMF detector over consecutive 20 ms f32 blocks (one
/// conference tick each). Feed every decoded block via [`DtmfDetector::feed`];
/// it returns each registered digit exactly once (edge-triggered per
/// key-down), and [`DtmfDetector::squelching`] says whether the CURRENT block
/// (or its [`SQUELCH_TAIL_BLOCKS`] tail) should be muted from the relay sum.
pub struct DtmfDetector {
    sample_rate: f32,
    /// Digit the previous block(s) classified as, `None` after a non-tone
    /// block.
    candidate: Option<char>,
    /// Consecutive blocks agreeing on `candidate`.
    run: u32,
    /// Whether `candidate` already registered (edge trigger: one event per
    /// key-down, however long the key is held).
    registered: bool,
    /// Remaining squelch-tail blocks after the last tone block.
    tail: u32,
    /// Consecutive non-tone blocks seen since the last tone block (iax-4d19);
    /// the key-down releases only at [`RELEASE_BLOCKS`].
    gap: u32,
    /// Whether the block last fed (or its tail) is relay-squelched.
    squelch: bool,
}

impl DtmfDetector {
    /// Build a detector for blocks sampled at `sample_rate` Hz.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // audio rates are far below f32 precision loss
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate as f32,
            candidate: None,
            run: 0,
            registered: false,
            tail: 0,
            gap: 0,
            squelch: false,
        }
    }

    /// Advance the detector by one 20 ms block. Returns `Some(digit)` on the
    /// block that CONFIRMS a key-down ([`CONFIRM_BLOCKS`] agreeing blocks),
    /// exactly once per key-down. Updates [`DtmfDetector::squelching`].
    pub fn feed(&mut self, block: &[f32]) -> Option<char> {
        let cand = classify_block(block, self.sample_rate);

        // Relay squelch: engaged from the first tone-looking block, held for
        // SQUELCH_TAIL_BLOCKS blocks after the tone ends.
        if cand.is_some() {
            self.tail = SQUELCH_TAIL_BLOCKS;
            self.squelch = true;
        } else if self.tail > 0 {
            self.tail -= 1;
            self.squelch = true;
        } else {
            self.squelch = false;
        }

        // Digit registration: CONFIRM_BLOCKS consecutive agreeing blocks,
        // edge-triggered once per key-down.
        match cand {
            Some(d) if self.candidate == Some(d) => {
                // Same tone continuing (possibly across a short dropout —
                // iax-4d19): the key is still down, so never re-register.
                self.gap = 0;
                self.run += 1;
                if self.run >= CONFIRM_BLOCKS && !self.registered {
                    self.registered = true;
                    return Some(d);
                }
            }
            Some(d) => {
                self.candidate = Some(d);
                self.gap = 0;
                self.run = 1;
                self.registered = false;
                if self.run >= CONFIRM_BLOCKS {
                    self.registered = true;
                    return Some(d);
                }
            }
            None => {
                // Release hysteresis: one quiet block is a dropout, not a
                // key-release. Hold the key-down until RELEASE_BLOCKS agree.
                self.gap += 1;
                if self.gap >= RELEASE_BLOCKS {
                    self.candidate = None;
                    self.run = 0;
                    self.registered = false;
                }
            }
        }
        None
    }

    /// Whether the most recently fed block (or the [`SQUELCH_TAIL_BLOCKS`]
    /// tail after a tone) should be EXCLUDED from the conference relay sum.
    /// The local speaker path stays unfiltered regardless.
    #[must_use]
    pub fn squelching(&self) -> bool {
        self.squelch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 8_000;
    const BLOCK: usize = 160; // 20 ms @ 8 kHz

    // -- iax-4d19: release hysteresis ------------------------------------

    #[test]
    fn single_block_dropout_mid_tone_does_not_duplicate_the_digit() {
        // The live failure (hub 2026-08-03): astar sent *355553 and the node
        // decoded *,*,3,5,5,5,5,5,3 — extra tones from momentary dropouts
        // inside a single keypress, dialing node "555553".
        let mut det = DtmfDetector::new(FS);
        let mut got = Vec::new();
        // '5' = 770 + 1336 Hz. Six blocks of tone with ONE silent block in
        // the middle — one keypress, one dropout.
        let head = tone_pair(770.0, 1336.0, 0.3, 3);
        let tail = tone_pair(770.0, 1336.0, 0.3, 3);
        for b in head {
            if let Some(d) = det.feed(&b) {
                got.push(d);
            }
        }
        if let Some(d) = det.feed(&vec![0.0; BLOCK]) {
            got.push(d);
        }
        for b in tail {
            if let Some(d) = det.feed(&b) {
                got.push(d);
            }
        }
        assert_eq!(got, vec!['5'], "one keypress must register exactly once");
    }

    #[test]
    fn a_real_inter_digit_gap_still_registers_a_repeated_digit() {
        // The other half of the contract: 55553 has genuine repeats, so a
        // proper gap (>= RELEASE_BLOCKS of silence) must re-arm the detector.
        let mut det = DtmfDetector::new(FS);
        let mut got = Vec::new();
        for _ in 0..2 {
            for b in tone_pair(770.0, 1336.0, 0.3, 3) {
                if let Some(d) = det.feed(&b) {
                    got.push(d);
                }
            }
            for _ in 0..=RELEASE_BLOCKS {
                if let Some(d) = det.feed(&vec![0.0; BLOCK]) {
                    got.push(d);
                }
            }
        }
        assert_eq!(
            got,
            vec!['5', '5'],
            "two presses with a real gap = two digits"
        );
    }

    /// Synthesize `blocks` consecutive 20 ms blocks of a two-tone pair
    /// (amplitude `amp` per tone), phase-continuous across blocks.
    #[allow(clippy::cast_precision_loss)]
    fn tone_pair(f1: f32, f2: f32, amp: f32, blocks: usize) -> Vec<Vec<f32>> {
        let fs = FS as f32;
        (0..blocks * BLOCK)
            .map(|i| {
                let t = i as f32 / fs;
                amp * (2.0 * PI * f1 * t).sin() + amp * (2.0 * PI * f2 * t).sin()
            })
            .collect::<Vec<f32>>()
            .chunks(BLOCK)
            .map(<[f32]>::to_vec)
            .collect()
    }

    /// Deterministic broadband noise (xorshift-ish LCG), amplitude ~±`amp`.
    fn noise(amp: f32, blocks: usize) -> Vec<Vec<f32>> {
        let mut state: u32 = 0x1234_5678;
        (0..blocks)
            .map(|_| {
                (0..BLOCK)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        #[allow(clippy::cast_precision_loss)]
                        let u = (state >> 8) as f32 / 16_777_216.0; // [0,1)
                        amp * (2.0 * u - 1.0)
                    })
                    .collect()
            })
            .collect()
    }

    /// Feed blocks through a fresh detector; collect registered digits.
    fn digits_of(blocks: &[Vec<f32>]) -> Vec<char> {
        let mut det = DtmfDetector::new(FS);
        blocks.iter().filter_map(|b| det.feed(b)).collect()
    }

    #[test]
    fn all_sixteen_digits_detect_from_clean_tones() {
        for (ri, &rf) in DTMF_ROW_HZ.iter().enumerate() {
            for (ci, &cf) in DTMF_COL_HZ.iter().enumerate() {
                let want = DTMF_DIGITS[ri][ci];
                let got = digits_of(&tone_pair(rf, cf, 0.25, 4));
                assert_eq!(got, vec![want], "digit {want} ({rf}+{cf} Hz)");
            }
        }
    }

    #[test]
    fn digit_registers_once_per_keydown_however_long_held() {
        let mut det = DtmfDetector::new(FS);
        let held = tone_pair(770.0, 1336.0, 0.25, 10); // '5' held 200 ms
        let events: Vec<char> = held.iter().filter_map(|b| det.feed(b)).collect();
        assert_eq!(events, vec!['5'], "edge-triggered: one event per key-down");
        // Release (silence past the tail), then press again: a second event.
        let silence = vec![vec![0.0_f32; BLOCK]; 4];
        assert!(silence.iter().find_map(|b| det.feed(b)).is_none());
        let again: Vec<char> = tone_pair(770.0, 1336.0, 0.25, 4)
            .iter()
            .filter_map(|b| det.feed(b))
            .collect();
        assert_eq!(again, vec!['5'], "a new key-down registers again");
    }

    #[test]
    fn single_tone_is_not_a_digit() {
        // Only the row tone — no column pair ⇒ never a digit.
        let got = digits_of(&tone_pair(770.0, 770.0, 0.25, 5));
        assert!(got.is_empty(), "single tone must not register, got {got:?}");
    }

    #[test]
    fn off_frequency_pair_is_not_a_digit() {
        // A two-tone pair well away from any row+col combination.
        let got = digits_of(&tone_pair(1_000.0, 1_900.0, 0.25, 5));
        assert!(got.is_empty(), "off-frequency pair registered {got:?}");
    }

    #[test]
    fn broadband_noise_is_not_a_digit() {
        let got = digits_of(&noise(0.5, 50));
        assert!(got.is_empty(), "noise registered {got:?}");
    }

    #[test]
    fn silence_and_quiet_tones_are_not_digits() {
        let got = digits_of(&vec![vec![0.0_f32; BLOCK]; 5]);
        assert!(got.is_empty(), "silence registered {got:?}");
        // Pair below the absolute floor (MIN_TONE_AMP).
        let got = digits_of(&tone_pair(770.0, 1336.0, 0.005, 5));
        assert!(got.is_empty(), "sub-floor tones registered {got:?}");
    }

    #[test]
    fn one_block_blip_does_not_register_but_two_do() {
        let mut det = DtmfDetector::new(FS);
        let one = tone_pair(697.0, 1209.0, 0.25, 1);
        assert!(
            det.feed(&one[0]).is_none(),
            "40 ms minimum: 1 block is not enough"
        );
        // A fresh detector fed two agreeing blocks registers on the second.
        let mut det = DtmfDetector::new(FS);
        let two = tone_pair(697.0, 1209.0, 0.25, 2);
        assert!(det.feed(&two[0]).is_none());
        assert_eq!(det.feed(&two[1]), Some('1'));
    }

    #[test]
    fn squelch_covers_tone_and_tail_then_releases() {
        let mut det = DtmfDetector::new(FS);
        assert!(!det.squelching(), "idle detector does not squelch");
        for b in tone_pair(941.0, 1477.0, 0.25, 3) {
            let _ = det.feed(&b);
            assert!(det.squelching(), "every tone block is squelched");
        }
        let silence = vec![0.0_f32; BLOCK];
        for i in 0..SQUELCH_TAIL_BLOCKS {
            let _ = det.feed(&silence);
            assert!(det.squelching(), "tail block {i} still squelched");
        }
        let _ = det.feed(&silence);
        assert!(!det.squelching(), "squelch releases after the tail");
    }

    #[test]
    fn squelch_engages_on_the_first_tone_block_before_confirmation() {
        // At most one block of tone may leak; the mute must be up on block 1
        // even though the digit only registers on block 2.
        let mut det = DtmfDetector::new(FS);
        let blocks = tone_pair(852.0, 1633.0, 0.25, 1);
        assert!(det.feed(&blocks[0]).is_none(), "not yet confirmed");
        assert!(det.squelching(), "squelch precedes confirmation");
    }

    #[test]
    fn speech_like_multitone_is_not_a_digit() {
        // Several strong harmonics (a crude voiced-speech stand-in): fails the
        // in-group dominance and tone-vs-total checks.
        #[allow(clippy::cast_precision_loss)]
        let blocks: Vec<Vec<f32>> = {
            let fs = FS as f32;
            (0..5 * BLOCK)
                .map(|i| {
                    let t = i as f32 / fs;
                    0.2 * (2.0 * PI * 180.0 * t).sin()
                        + 0.2 * (2.0 * PI * 720.0 * t).sin()
                        + 0.2 * (2.0 * PI * 1_260.0 * t).sin()
                        + 0.2 * (2.0 * PI * 2_340.0 * t).sin()
                })
                .collect::<Vec<f32>>()
                .chunks(BLOCK)
                .map(<[f32]>::to_vec)
                .collect()
        };
        let got = digits_of(&blocks);
        assert!(got.is_empty(), "harmonic-rich signal registered {got:?}");
    }

    #[test]
    fn detects_at_16khz_blocks_too() {
        // The station bus can run at 16 kHz (320-sample ticks, iax-4348).
        let fs = 16_000_u32;
        let n = 320_usize;
        #[allow(clippy::cast_precision_loss)]
        let blocks: Vec<Vec<f32>> = (0..4 * n)
            .map(|i| {
                let t = i as f32 / fs as f32;
                0.25 * (2.0 * PI * 770.0 * t).sin() + 0.25 * (2.0 * PI * 1_209.0 * t).sin()
            })
            .collect::<Vec<f32>>()
            .chunks(n)
            .map(<[f32]>::to_vec)
            .collect();
        let mut det = DtmfDetector::new(fs);
        let got: Vec<char> = blocks.iter().filter_map(|b| det.feed(b)).collect();
        assert_eq!(got, vec!['4']);
    }
}
