// SPDX-License-Identifier: LGPL-3.0-or-later
//! Streaming fixed-point analysis front end: fixed-point sibling of
//! [`crate::ambe::float::tia_102_baba::encoder::FrameAnalyzer`]. It buffers PCM, keeps the per-frame
//! `E(P)` tables and the two-frame look-ahead, runs look-back/look-ahead tracking, the initial-pitch
//! decision and quarter-sample refinement, and yields one [`FrameAnalysis`] per 20 ms frame, ready for
//! [`crate::ambe::fixed::mbe_encode::analyze_at_pitch`].
//!
//! Timing, buffering and trimming are identical to the float analyzer (frame `k` is centred at input
//! sample `k * 160` plus the settable offset; the analyzer has a two-frame algorithmic delay, and
//! [`FrameAnalyzer::finish_input`] pads with silence to flush the tail). Input is 16-bit PCM.
//!
//! Where the float [`FrameAnalysis`] carries `omega0_hat: f64` and `initial_pitch_error: f64`, this one
//! carries the refined period `p8` (eighths of a sample, exact), the [`Pitch`] built from it (which
//! also holds `omega0` in Q30) and `initial_pitch_error_q16` (Q16.16).
//!
//! Verified against the float analyzer in `tests/ambe_fixed_tia_102_baba_encoder.rs`. Measured over all four
//! `osr_speech` files at centre offsets 0 and -37 (15552 frames, input pushed in 97-sample chunks):
//! identical frame counts, the frame's own samples identical on 100%, refined period identical to the
//! eighth of a sample on 99.92%, `E(P_hat_I)` within 0.001 on 99.95%, voicing identical on 99.994% of
//! 359120 harmonics analysed at the float frame's refined pitch, amplitudes within 0.001 dB (worst
//! 0.00074 dB) wherever the period and voicing agree. Each side carried its own tracking history, so a
//! rare half-sample tie in the initial pitch also shifts later frames' look-back window.

use super::encode::{encode_frame, FrameState};
use super::pitch::{
    choose_initial_pitch_estimate, look_ahead_pitch_tracking, look_back_pitch_tracking, ErrorTable,
    PitchAnalysisFrame, DEFAULT_PITCH_INDEX,
};
use super::pitch_refinement::{refine_pitch, Pitch, RefinementFrame};
use std::collections::VecDeque;

const FRAME_SAMPLES: usize = 160;
/// Silence prepended so the first frame's window has real history to read.
const LEAD: usize = 200;
/// Samples the analysis reads beyond a frame's centre (and before it).
const MARGIN: usize = 160;

/// Everything downstream stages need for one frame.
pub struct FrameAnalysis {
    /// Refined pitch period in eighths of a sample (`P = p8 / 8`).
    pub p8: u32,
    /// The refined pitch as an exact fraction plus `omega0` (Q30); see [`Pitch`].
    pub pitch: Pitch,
    /// `E(P_hat_I)` of the initial pitch estimate, Q16.16 (Eq. 37's `E(P_hat_I)`).
    pub initial_pitch_error_q16: i32,
    /// The refinement spectrum `S_w(m)` of the frame.
    pub refinement: RefinementFrame,
    /// The frame's own 160 input samples (for tone detection).
    pub slot_samples: Vec<i32>,
}

/// Streaming analyzer; feed PCM with [`Self::push_samples`], pull frames with [`Self::next_analysis`].
pub struct FrameAnalyzer {
    raw: Vec<i32>,
    trimmed: usize,
    real_samples: usize,
    next_frame: usize,
    center_offset: i32,
    prev_first: (usize, i32),
    prev_second: (usize, i32),
    tables: VecDeque<(usize, ErrorTable)>,
    finished: bool,
}

impl FrameAnalyzer {
    pub fn new() -> Self {
        Self {
            raw: vec![0; LEAD],
            trimmed: 0,
            real_samples: 0,
            next_frame: 0,
            center_offset: 0,
            prev_first: (DEFAULT_PITCH_INDEX, 0),
            prev_second: (DEFAULT_PITCH_INDEX, 0),
            tables: VecDeque::new(),
            finished: false,
        }
    }

    /// Shifts every frame's analysis centre by `samples` (clamped to +-160), as the float analyzer does.
    pub fn set_center_offset(&mut self, samples: i32) {
        self.center_offset = samples.clamp(-160, 160);
    }

    fn center(&self, k: usize) -> usize {
        (LEAD as i64 + (k * FRAME_SAMPLES) as i64 + self.center_offset as i64).max(MARGIN as i64)
            as usize
    }

    pub fn push_samples(&mut self, samples: &[i16]) {
        self.raw.extend(samples.iter().map(|&s| s as i32));
        self.real_samples += samples.len();
    }

    /// [`Self::push_samples`] for samples already widened past 16 bits (the standard's input high-pass filter can
    /// overshoot the 16-bit range).
    pub fn push_samples_wide(&mut self, samples: &[i32]) {
        self.raw.extend_from_slice(samples);
        self.real_samples += samples.len();
    }

    /// Marks the end of input and pads with silence so the last real frames complete.
    pub fn finish_input(&mut self) {
        if !self.finished {
            self.raw
                .extend(std::iter::repeat_n(0, 3 * FRAME_SAMPLES + 2 * MARGIN));
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
            let table = PitchAnalysisFrame::new(&self.raw, center - self.trimmed).error_table();
            self.tables.push_back((k, table));
        }
    }

    /// The next frame's analysis, or `None` if more input is needed (or the stream is exhausted).
    pub fn next_analysis(&mut self) -> Option<FrameAnalysis> {
        let k = self.next_frame;
        if !self.has_real_frame() || !self.available(k) {
            return None;
        }
        self.table(k);
        self.table(k + 1);
        self.table(k + 2);
        let find = |tables: &VecDeque<(usize, ErrorTable)>, idx: usize| -> usize {
            tables
                .iter()
                .position(|(i, _)| *i == idx)
                .expect("table for this frame was just built")
        };
        let (i0, i1, i2) = (
            find(&self.tables, k),
            find(&self.tables, k + 1),
            find(&self.tables, k + 2),
        );
        let (t0, t1, t2) = (&self.tables[i0].1, &self.tables[i1].1, &self.tables[i2].1);

        let (idx_b, ce_b) = look_back_pitch_tracking(t0, self.prev_first, self.prev_second);
        let (idx_f, ce_f) = look_ahead_pitch_tracking(t0, t1, t2);
        let idx_initial = choose_initial_pitch_estimate(idx_b, ce_b, idx_f, ce_f);
        let e_initial = t0.at(idx_initial);

        let center = self.center(k) - self.trimmed;
        let refinement = RefinementFrame::new(&self.raw, center);
        let p8 = refine_pitch(&refinement, idx_initial);
        let slot_start = LEAD + k * FRAME_SAMPLES - self.trimmed;
        let slot_samples = self.raw[slot_start..slot_start + FRAME_SAMPLES].to_vec();

        self.prev_second = self.prev_first;
        self.prev_first = (idx_initial, e_initial);
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
            p8,
            pitch: Pitch::from_p8(p8),
            initial_pitch_error_q16: e_initial,
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

/// Streaming fixed-point TIA-102.BABA encoder: fixed-point sibling of the float `tia_102_baba::encoder::Encoder`
/// (same surface, 16-bit PCM in, `[u32; 8]` code vectors out), composing [`FrameAnalyzer`] with
/// [`super::encode::encode_frame`]. Two frames of look-ahead delay; [`Encoder::finish`] flushes with silence.
/// The standard's input high-pass filter (Eq. 3, `H(z) = (1 - z^-1) / (1 - 0.99 z^-1)`), in integer arithmetic:
/// the recursive state is kept in Q16 with the pole in Q30, so the output is the exact filter response rounded to
/// an integer sample without accumulating rounding error. Fixed-point sibling of the float
/// `tia_102_baba::encoder::HighPassFilter`.
#[derive(Clone, Copy, Debug, Default)]
pub struct HighPassFilter {
    prev_input: i64,
    prev_output_q16: i64,
}

/// `0.99` in Q30 (`round(0.99 * 2^30)`).
const HIGH_PASS_POLE_Q30: i64 = ((99i64 << 30) + 50) / 100;

impl HighPassFilter {
    pub fn step(&mut self, x: i32) -> i32 {
        let x = x as i64;
        let feedback = (self.prev_output_q16 * HIGH_PASS_POLE_Q30 + (1 << 29)) >> 30;
        let y_q16 = ((x - self.prev_input) << 16) + feedback;
        self.prev_input = x;
        self.prev_output_q16 = y_q16;
        ((y_q16 + (1 << 15)) >> 16) as i32
    }
}

pub struct Encoder {
    high_pass: HighPassFilter,
    analyzer: FrameAnalyzer,
    state: FrameState,
    last_frame: Option<[u32; 8]>,
    /// Frames for which analysis failed (degenerate pitch/`L_hat`) and the previous frame was repeated.
    pub failed_frames: usize,
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            high_pass: HighPassFilter::default(),
            analyzer: FrameAnalyzer::new(),
            state: FrameState::initial(),
            last_frame: None,
            failed_frames: 0,
        }
    }

    /// Shifts every frame's analysis centre by `samples` (may be negative) relative to `k*160`.
    pub fn set_center_offset(&mut self, samples: i32) {
        self.analyzer.set_center_offset(samples);
    }

    /// Feeds PCM to the encoder, applying the standard's input high-pass filter (Eq. 3) first.
    pub fn push_samples(&mut self, samples: &[i16]) {
        let filtered: Vec<i32> = samples
            .iter()
            .map(|&s| self.high_pass.step(s as i32))
            .collect();
        self.analyzer.push_samples_wide(&filtered);
    }

    /// Encodes the next frame if enough look-ahead has been pushed, else `None`.
    pub fn next_frame(&mut self) -> Option<[u32; 8]> {
        let a = self.analyzer.next_analysis()?;
        match encode_frame(
            &a.refinement,
            &a.pitch,
            a.initial_pitch_error_q16,
            &self.state,
            false,
        ) {
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
