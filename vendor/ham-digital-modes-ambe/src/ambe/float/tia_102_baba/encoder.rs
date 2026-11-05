// SPDX-License-Identifier: LGPL-3.0-or-later
//! Streaming PCM-to-frame encoder for TIA-102.BABA: composes the spec's full pitch estimation (section 5.1:
//! per-frame error function `E(P)`, look-back and look-ahead tracking, the initial-estimate decision, and
//! quarter-sample refinement) with [`super::encode_frame`]'s own analysis/quantization/FEC pipeline.
//!
//! Before this module every caller of [`super::encode_frame`] either passed a known pitch or used a
//! per-frame grid search that ignored frame-to-frame continuity (`tests/ambe_real_speech_round_trip.rs`).
//! Here the two-frame lookahead is real, so the encoder has an algorithmic delay of two frames plus the
//! analysis margin; [`Encoder::finish`] pads with silence to flush the tail.
//!
//! Frame `k` is analysed at sample index `k*160` of the input (the window is centred on the start of the
//! frame's own 20 ms slot), plus a settable offset ([`Encoder::set_center_offset`]) so the alignment against
//! a hardware encoder can be tuned.

use super::pitch::{
    candidate_pitches, choose_initial_pitch_estimate, look_ahead_pitch_tracking,
    look_back_pitch_tracking, PitchAnalysisFrame,
};
use super::pitch_refinement::{refine_pitch, RefinementFrame};
use super::{encode_frame, encode_frame_chip_wire, FrameState};
use std::collections::VecDeque;

const FRAME_SAMPLES: usize = 160;
/// Zero samples conceptually preceding the input, so frame 0 has its full analysis margin.
const LEAD: usize = 200;
/// Analysis margin needed on each side of a frame centre (pitch analysis: 150 + 10 filter taps).
const MARGIN: usize = 160;
const CANDIDATES: usize = 203;

/// One frame's error function `E(P)` sampled at every candidate pitch (`21, 21.5, ..., 122`).
struct ErrorTable(Vec<f64>);

impl ErrorTable {
    fn compute(raw: &[f64], center: usize) -> Self {
        let frame = PitchAnalysisFrame::new(raw, center);
        Self(
            candidate_pitches()
                .map(|p| frame.error_function(p))
                .collect(),
        )
    }

    fn at(&self, p: f64) -> f64 {
        let idx = ((p - 21.0) / 0.5).round() as usize;
        self.0[idx.min(CANDIDATES - 1)]
    }
}

/// One frame's pitch analysis result: the refined fundamental, the initial estimate's error `E(P_hat_I)`, and the
/// frame's own windowed spectrum ready for voicing/amplitude analysis.
pub struct FrameAnalysis {
    pub omega0_hat: f64,
    /// The initial pitch estimate `P_hat_I` (samples, before refinement).
    pub initial_pitch: f64,
    pub initial_pitch_error: f64,
    pub refinement: RefinementFrame,
    /// The 160 input samples of this frame's own 20 ms slot (`k*160..(k+1)*160`, zero padded past the input's end), for
    /// tone detection.
    pub slot_samples: Vec<f64>,
}

/// Streaming pitch analysis shared by every mode's encoder: buffers input, runs the spec's two-frame lookahead
/// pitch tracking and half/quarter-sample refinement, and yields one [`FrameAnalysis`] per 20 ms frame.
pub struct FrameAnalyzer {
    /// `LEAD` zeros followed by every input sample received (older samples are trimmed, see `trimmed`).
    raw: Vec<f64>,
    /// Number of samples already dropped from the front of `raw`.
    trimmed: usize,
    /// Real (non-padding) samples pushed so far.
    real_samples: usize,
    next_frame: usize,
    center_offset: i32,
    prev1: (f64, f64),
    prev2: (f64, f64),
    tables: VecDeque<(usize, ErrorTable)>,
    finished: bool,
}

impl FrameAnalyzer {
    pub fn new() -> Self {
        Self {
            raw: vec![0.0; LEAD],
            trimmed: 0,
            real_samples: 0,
            next_frame: 0,
            center_offset: 0,
            prev1: (100.0, 0.0),
            prev2: (100.0, 0.0),
            tables: VecDeque::new(),
            finished: false,
        }
    }

    /// Shifts every frame's analysis centre by `samples` (may be negative) relative to `k*160`.
    pub fn set_center_offset(&mut self, samples: i32) {
        // Bounded so `center()` can neither underflow after trimming nor push the tail frames past the input.
        self.center_offset = samples.clamp(-160, 160);
    }

    fn center(&self, k: usize) -> usize {
        (LEAD as i64 + (k * FRAME_SAMPLES) as i64 + self.center_offset as i64).max(MARGIN as i64)
            as usize
    }

    pub fn push_samples(&mut self, samples: &[f64]) {
        self.raw.extend_from_slice(samples);
        self.real_samples += samples.len();
    }

    /// Pads with silence so every pushed sample's frame can be analysed; call once, then drain `next_analysis`.
    pub fn finish_input(&mut self) {
        if !self.finished {
            self.raw
                .extend(std::iter::repeat_n(0.0, 3 * FRAME_SAMPLES + 2 * MARGIN));
            self.finished = true;
        }
    }

    fn available(&self, k: usize) -> bool {
        self.center(k + 2) + MARGIN < self.trimmed + self.raw.len()
    }

    fn has_real_frame(&self) -> bool {
        !self.finished || self.center(self.next_frame) < LEAD + self.real_samples
    }

    fn table(&mut self, k: usize) {
        if !self.tables.iter().any(|(i, _)| *i == k) {
            let center = self.center(k);
            let table = ErrorTable::compute(&self.raw, center - self.trimmed);
            self.tables.push_back((k, table));
        }
    }

    /// The next frame's analysis if enough lookahead has been pushed (and, after [`Self::finish_input`], while
    /// frames still cover real input), else `None`.
    pub fn next_analysis(&mut self) -> Option<FrameAnalysis> {
        let k = self.next_frame;
        if !self.has_real_frame() || !self.available(k) {
            return None;
        }
        self.table(k);
        self.table(k + 1);
        self.table(k + 2);
        let find = |tables: &VecDeque<(usize, ErrorTable)>, idx: usize| -> usize {
            tables.iter().position(|(i, _)| *i == idx).unwrap()
        };
        let (i0, i1, i2) = (
            find(&self.tables, k),
            find(&self.tables, k + 1),
            find(&self.tables, k + 2),
        );
        let (t0, t1, t2) = (&self.tables[i0].1, &self.tables[i1].1, &self.tables[i2].1);

        let (p_b, ce_b) = look_back_pitch_tracking(|p| t0.at(p), self.prev1, self.prev2);
        let (p_f, ce_f) = look_ahead_pitch_tracking(|p| t0.at(p), |p| t1.at(p), |p| t2.at(p));
        let p_initial = choose_initial_pitch_estimate(p_b, ce_b, p_f, ce_f);
        let e_initial = t0.at(p_initial);

        let center = self.center(k) - self.trimmed;
        let refinement = RefinementFrame::new(&self.raw, center);
        let omega0_hat = refine_pitch(&refinement, p_initial);
        let slot_start = LEAD + k * FRAME_SAMPLES - self.trimmed;
        let slot_samples = self.raw[slot_start..slot_start + FRAME_SAMPLES].to_vec();

        self.prev2 = self.prev1;
        self.prev1 = (p_initial, e_initial);
        self.next_frame += 1;

        while self.tables.front().is_some_and(|(i, _)| *i < k + 1) {
            self.tables.pop_front();
        }
        let keep_from = self.center(k + 1).saturating_sub(MARGIN + 10);
        if keep_from > self.trimmed + 4096 {
            let drop = keep_from - self.trimmed;
            self.raw.drain(..drop);
            self.trimmed += drop;
        }
        Some(FrameAnalysis {
            omega0_hat,
            initial_pitch: p_initial,
            initial_pitch_error: e_initial,
            refinement,
            slot_samples,
        })
    }
}

impl Default for FrameAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// The standard's input high-pass filter (Eq. 3, section 5): `H(z) = (1 - z^-1) / (1 - 0.99 z^-1)`, removing any
/// residual D.C. before analysis. Without it a constant offset in the input (common in recordings) makes quiet frames
/// look strongly periodic at every lag, corrupting the pitch error function `E(P)` (found by comparing against the
/// OP25 `imbe_vocoder` encoder, which applies it; see `docs/references/tia_102_baba_cross_validation.md`).
#[derive(Clone, Copy, Debug, Default)]
pub struct HighPassFilter {
    prev_input: i64,
    prev_output_q16: i64,
}

/// `0.99` in Q30 (`round(0.99 * 2^30)`), the same constant as the fixed-point sibling, whose integer arithmetic this
/// filter reproduces exactly so that the float and fixed encoders see bit-identical filtered input.
const HIGH_PASS_POLE_Q30: i64 = ((99i64 << 30) + 50) / 100;

impl HighPassFilter {
    /// One sample of the filter; `x` is a whole-number PCM value and the result is the rounded whole-number output.
    pub fn step(&mut self, x: f64) -> f64 {
        let x = x.round() as i64;
        let feedback = (self.prev_output_q16 * HIGH_PASS_POLE_Q30 + (1 << 29)) >> 30;
        let y_q16 = ((x - self.prev_input) << 16) + feedback;
        self.prev_input = x;
        self.prev_output_q16 = y_q16;
        ((y_q16 + (1 << 15)) >> 16) as f64
    }
}

pub struct Encoder {
    high_pass: HighPassFilter,
    analyzer: FrameAnalyzer,
    state: FrameState,
    last_frame: Option<[u32; 8]>,
    /// Frames for which analysis failed (degenerate pitch/`L_hat`) and the previous frame was repeated.
    pub failed_frames: usize,
    /// The last frame's initial pitch estimate `P_hat_I`, refined fundamental `omega0_hat` and `E(P_hat_I)`, for
    /// diagnostics (`examples/tia_102_baba_oracle_encode_dump.rs`).
    pub last_analysis: Option<(f64, f64, f64)>,
    /// Emit the DVSI chip's framing instead of the standard's (see [`super::encode_code_vectors_chip`]).
    chip_wire: bool,
}

impl Encoder {
    /// An encoder whose frames use the standard's wire layer (FEC plus modulation, Eq. 81-94).
    pub fn new() -> Self {
        Self {
            high_pass: HighPassFilter::default(),
            analyzer: FrameAnalyzer::new(),
            state: FrameState::initial(),
            last_frame: None,
            failed_frames: 0,
            last_analysis: None,
            chip_wire: false,
        }
    }

    /// An encoder whose frames use the DVSI chip's framing (no modulation, chip Hamming labelling), for the
    /// chip-comparison tools. The analysis and quantization are identical to [`Self::new`]'s.
    pub fn new_chip_wire() -> Self {
        Self {
            chip_wire: true,
            ..Self::new()
        }
    }

    /// Shifts every frame's analysis centre by `samples` (may be negative) relative to `k*160`.
    pub fn set_center_offset(&mut self, samples: i32) {
        self.analyzer.set_center_offset(samples);
    }

    /// Feeds PCM to the encoder, applying the standard's input high-pass filter (Eq. 3) first.
    pub fn push_samples(&mut self, samples: &[f64]) {
        // Rounded to whole sample values, as PCM hardware (and the fixed-point sibling) would output.
        let filtered: Vec<f64> = samples
            .iter()
            .map(|&x| (self.high_pass.step(x) + 0.5).floor())
            .collect();
        self.analyzer.push_samples(&filtered);
    }

    /// Encodes the next frame if enough lookahead has been pushed, else `None`.
    pub fn next_frame(&mut self) -> Option<[u32; 8]> {
        let a = self.analyzer.next_analysis()?;
        self.last_analysis = Some((a.initial_pitch, a.omega0_hat, a.initial_pitch_error));
        let encoded = if self.chip_wire {
            encode_frame_chip_wire(
                &a.refinement,
                a.omega0_hat,
                a.initial_pitch_error,
                &self.state,
                false,
            )
        } else {
            encode_frame(
                &a.refinement,
                a.omega0_hat,
                a.initial_pitch_error,
                &self.state,
                false,
            )
        };
        match encoded {
            Some((c, next_state)) => {
                self.state = next_state;
                self.last_frame = Some(c);
                Some(c)
            }
            None => {
                self.failed_frames += 1;
                self.last_frame.or(Some([0; 8]))
            }
        }
    }

    /// Pads with silence so every pushed sample's frame can be emitted, and returns those remaining frames.
    pub fn finish(&mut self) -> Vec<[u32; 8]> {
        self.analyzer.finish_input();
        let mut out = Vec::new();
        while let Some(f) = self.next_frame() {
            out.push(f);
        }
        out
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambe::float::tia_102_baba::decode::DecoderState;

    /// A harmonic-rich periodic signal at a known pitch.
    fn tone(period: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| {
                (1..=8)
                    .map(|h| {
                        1500.0 / h as f64
                            * (2.0 * std::f64::consts::PI * h as f64 * i as f64 / period).sin()
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn streaming_encoder_recovers_a_steady_pitch_and_decodes() {
        let period = 60.0; // 133.3 Hz -> b0 for omega0 = 2*pi/60
        let mut enc = Encoder::new();
        enc.push_samples(&tone(period, 160 * 30));
        let mut frames = Vec::new();
        while let Some(f) = enc.next_frame() {
            frames.push(f);
        }
        frames.extend(enc.finish());
        assert!(
            frames.len() >= 25,
            "expected roughly one frame per 160 input samples, got {}",
            frames.len()
        );

        let mut dec = DecoderState::new();
        let mut checked = 0;
        for c in frames.iter().skip(5).take(15) {
            if let Some(crate::ambe::float::tia_102_baba::decode::FrameOutcome::Decoded(p)) =
                DecoderState::new().decode_parameters(*c)
            {
                let period_est = 2.0 * std::f64::consts::PI / p.omega0_tilde;
                // A perfectly periodic signal fits an integer multiple of its period equally well (E(P) ~ 0 for
                // both), and the sub-multiple tests are ill-conditioned against such a near-zero reference error,
                // so accept the period or its double.
                let ok = [1.0, 2.0]
                    .iter()
                    .any(|m| (period_est / (period * m) - 1.0).abs() < 0.03);
                assert!(ok, "decoded period {period_est} vs true {period}");
                checked += 1;
            }
            assert!(dec.decode_frame(*c).is_some());
        }
        assert!(
            checked >= 10,
            "only {checked} steady-state frames decoded to parameters"
        );
        assert_eq!(enc.failed_frames, 0);
    }
}
