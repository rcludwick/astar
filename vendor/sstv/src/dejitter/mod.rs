//! De-jitter: put every line of a picture back on time before a decoder
//! sees it.
//!
//! A decoder that follows the line-sync pulses only near where it expects
//! them, or fits one straight line through all of them, copes with a sender
//! whose clock is a little off, but not with a step: when a 20 ms frame of
//! audio is lost on a network link, every later line arrives 20 ms early and
//! lands in the wrong place.
//!
//! So, once the header has named the mode, this module works on the whole
//! picture's audio at once ([`Plan`]):
//!
//! 1. Runs of exact zeros a few milliseconds long are cut out. Real audio
//!    never holds the same exact zero for that long; these are gaps a far
//!    node stuffed in while bridging two clocks.
//! 2. The audio is demodulated to an instantaneous frequency, and each
//!    line's 1200 Hz sync pulse is searched for near where the previous
//!    pulse says it should be. That window is wide enough to follow a lost
//!    frame (pulses jump early) or stuffed audio (pulses jump late).
//! 3. Each found pulse becomes an anchor: where it is in the audio, and
//!    where it should be on a perfect grid of nominal line lengths. The
//!    audio is resampled piecewise-linearly between anchors, so every pulse
//!    sits exactly on the grid, and the decoder gets a picture whose timing
//!    is clean.
//! 4. A line whose two pulses are further apart or closer together than a
//!    line length (audio was lost or added inside it), or whose audio is
//!    mostly silent, is marked. Its picture rows are later rebuilt from the
//!    rows above and below ([`fill_rows`]): the lost audio cannot be got
//!    back, but the lines after it are no longer harmed. Where one lost
//!    frame can be placed, the gap is filled from the line before to keep
//!    the timing, and those rows are rebuilt from their neighbours too.
//!
//! The work is linear in the picture's length. Above 16 kHz the pulse search
//! runs on a decimated copy, so its cost does not grow with the sample rate.
//!
//! [`Decoder::decode_dejittered`](crate::Decoder::decode_dejittered) runs
//! all of this in front of this crate's own decoder. The parts are public so
//! another decoder can be given the same repair.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    // Signal-processing arithmetic reads best with its usual letters.
    clippy::many_single_char_names
)]

extern crate std;
use std::vec;
use std::vec::Vec;

use crate::timing::{ScanTiming, SyncPosition};

/// A sample whose magnitude is below this counts as silence (about −54 dBFS):
/// where the input's last heard sample is.
const SILENCE_LEVEL: i16 = 64;

/// The sync tone's band. The tone is 1200 Hz; black is 1500 Hz.
const SYNC_LOW_HZ: f64 = 1000.0;
const SYNC_HIGH_HZ: f64 = 1350.0;
/// The demodulator's centre, the middle of the SSTV band.
const MIX_HZ: f64 = 1700.0;
/// The pulse search runs at no more than this rate.
const SEARCH_RATE_MAX: u32 = 16_000;
/// Zero runs at least this long and at most [`ZERO_RUN_MAX_SECS`] are cut.
const ZERO_RUN_MIN_SECS: f64 = 0.000_25;
/// Longer runs of zeros are silence, not stuffing, and stay.
const ZERO_RUN_MAX_SECS: f64 = 0.030;
/// How far from its predicted place a line's pulse is looked for.
const SEARCH_SECS: f64 = 0.045;
/// A line whose length is off by more than this lost or gained audio.
const DAMAGE_SECS: f64 = 0.0015;
/// The phase of the first pulse is taken from this many lines.
const PHASE_LINES: usize = 8;
/// From the line this many after the last pulse found (so once
/// `LOST_LINES - 1` lines in a row have had none), a line whose own pulse
/// is not near its predicted place has the phase looked for again across a
/// whole line: a dropout can come back with the sender's lines shifted.
/// After [`PHASE_LINES`] / 2 such searches in a row find nothing, one is
/// tried only every [`PHASE_LINES`] / 2 lines until lock is regained.
const LOST_LINES: usize = 3;
/// A pulse is accepted when this much of its width is in the sync band...
const PULSE_FILL: f64 = 0.6;
/// ...and it stands this far above the audio either side of it.
const PULSE_CONTRAST: f64 = 0.35;
/// Below this magnitude (as in [`SILENCE_LEVEL`]) a sample is silent.
const SILENT: f32 = 64.0 / 32768.0;
/// A line with more than this share of silent audio is lost.
const SILENT_SHARE: f64 = 0.5;
/// A line holding a silent run longer than this lost part of a channel, and
/// is rebuilt from its neighbours. SSTV is a constant-amplitude FM tone, so
/// even black (1500 Hz) or the sync tone never stays below [`SILENT`] for
/// more than a sample or two around a zero crossing: a run this long is no
/// carrier at all. 10 ms is a few dozen pixels of one channel, a mark you
/// can see, and half a network frame. A run is measured whole, across line
/// boundaries.
///
/// Stuffed zero runs (up to [`ZERO_RUN_MAX_SECS`]) are cut before lines are
/// marked, so most never count here. Some are never cut and so can still
/// mark their line: a run of exact zeros longer than [`ZERO_RUN_MAX_SECS`]
/// (silence, which is meant to), one inside the audio the decoder was already
/// given (`keep` in [`Plan::new`]), one touching known-lost audio, and a
/// run of near-zero samples that are not exact zeros.
const SILENT_RUN_SECS: f64 = 0.010;
/// Runs of at most this many marked display rows are filled; a longer run is
/// a gap too wide to make up, and is left as decoded.
pub const MAX_FILL_ROWS: u32 = 8;

/// A picture's audio, re-timed: built from the whole picture's audio by
/// [`Plan::new`], then read out on the perfect grid by [`Plan::render`].
pub struct Plan {
    /// The input with stuffed zeros cut out, as `f32`.
    audio: Vec<f32>,
    /// Where each cut was: (index in `audio`, raw samples cut before it).
    cuts: Vec<(usize, usize)>,
    /// Anchors as (output sample, input sample into `audio`), both rising.
    anchors: Vec<(f64, f64)>,
    /// Output spans whose audio was lost, filled from one line earlier.
    holes: Vec<(f64, f64)>,
    /// One line, in output samples.
    line_out: f64,
    /// Per radio line: its audio was lost or mistimed, and its rows should
    /// be rebuilt from their neighbours.
    damaged: Vec<bool>,
    /// Per radio line: part of it was lost and filled from the line before.
    /// The fill keeps the timing, but is not that line's picture, so its
    /// rows are rebuilt from their neighbours as well.
    patched: Vec<bool>,
    /// Output position where line 0 starts.
    first: f64,
}

/// Radio lines (sync pulse to sync pulse) in a picture of `timing`: the PD
/// modes send two rows per line.
#[must_use]
pub fn radio_lines(timing: &ScanTiming) -> u32 {
    timing.image_lines / timing.rows_per_line.max(1)
}

/// Seconds from the start of a line to the middle of its sync pulse.
fn pulse_offset_secs(timing: &ScanTiming) -> f64 {
    timing.sync_offset() + timing.sync_seconds / 2.0
}

impl Plan {
    /// Re-time `raw`, a picture's audio at `rate` from the end of its header.
    /// The first `keep` samples are already with the decoder and are left
    /// exactly as they are. `known` lists audio known to be lost, as (start,
    /// length) in `raw`: already filled in by the caller, it keeps its time
    /// (it is never cut as stuffing) and its lines are marked repaired.
    #[must_use]
    pub fn new(
        raw: &[i16],
        keep: usize,
        rate: u32,
        timing: &ScanTiming,
        known: &[(usize, usize)],
    ) -> Self {
        let (audio, cuts) = cut_zero_runs(raw, keep.min(raw.len()), rate, known);
        let decim = rate.div_ceil(SEARCH_RATE_MAX).max(1) as usize;
        let fr = f64::from(rate) / decim as f64;
        let (ind, freq) = sync_band(&audio, decim, fr);
        let prefix = prefix_sums(&ind);

        let lines = radio_lines(timing) as usize;
        let line = timing.line_seconds * fr;
        let offset = pulse_offset_secs(timing) * fr;
        let finder = PulseFinder {
            prefix: &prefix,
            width: (timing.sync_seconds * fr).max(2.0),
            guard: (timing.sync_seconds * fr / 2.0).max(0.001 * fr),
        };
        // The lines the input reaches: a picture cut off early (abandoned
        // on silence or at the end of the stream) holds just its first part,
        // and trailing silence holds no pulses.
        let heard = audio
            .iter()
            .rposition(|s| s.abs() >= f32::from(SILENCE_LEVEL) / 32768.0)
            .map_or(0, |i| i + 1);
        let reach = ((heard as f64 / decim as f64 - offset) / line)
            .ceil()
            .clamp(0.0, lines as f64) as usize;
        let mut found = track(
            &finder,
            lines,
            reach,
            line,
            offset,
            (SEARCH_SECS * fr).min(0.3 * line),
            DAMAGE_SECS * fr,
        );
        // With most pulses lost in noise there is too little to go on, and
        // the decoder's own timing does better: leave the
        // timing alone. "Most" counts only the lines the input reaches.
        if found.iter().filter(|p| p.is_some()).count() * 2 < reach {
            found.fill(None);
        }
        let (pulses, steps) = smooth(&found, line, DAMAGE_SECS * fr);

        // The grid: line k's pulse belongs at `phase + k * line`, with the
        // phase taken from the first pulse found (already smoothed by its
        // run's fit), so the start of the picture, which is already with
        // the decoder, stays where it is.
        let phase = pulses
            .iter()
            .enumerate()
            .find_map(|(k, p)| p.map(|c| c - k as f64 * line))
            .unwrap_or(offset);

        let mut damaged = vec![false; lines];
        let mut patched = vec![false; lines];
        // Anchors and holes in decimated samples first.
        let mut anchors: Vec<(f64, f64)> = Vec::new();
        let mut holes: Vec<(f64, f64)> = Vec::new();
        let line_of = |t: f64| ((t - (phase - offset)) / line).floor().max(0.0) as usize;
        let mut prev: Option<(usize, f64)> = None;
        for (k, p) in pulses.iter().enumerate() {
            let Some(c) = *p else { continue };
            let t = phase + k as f64 * line;
            if let Some((j, cj)) = prev
                && let Some(&(_, _, err)) = steps.iter().find(|s| s.1 == k)
            {
                let tj = phase + j as f64 * line;
                let hole = (k == j + 1 && err.abs() <= 0.25 * line)
                    .then(|| locate_hole(&freq, &pulses, j, cj, c, line, err))
                    .flatten();
                if let Some(h) = hole {
                    anchors.push((tj + h, cj + h));
                    if err < 0.0 {
                        // Audio lost: a gap in the output, filled later.
                        anchors.push((tj + h - err, cj + h + 1e-6));
                        holes.push((tj + h, tj + h - err));
                    } else {
                        // Audio added: skip over it.
                        anchors.push((tj + h + 1e-6, cj + h + err));
                    }
                    let at = line_of(tj + h - err.min(0.0) / 2.0);
                    if let Some(slot) = patched.get_mut(at) {
                        *slot = true;
                    }
                } else {
                    // No single place to put it: stretch, and rebuild the
                    // rows from their neighbours. A Scottie pulse sits
                    // mid-line, so the span between two of them holds the
                    // end of line j and the start of line k.
                    let last = match timing.sync_position {
                        SyncPosition::Scottie => k,
                        SyncPosition::Start => k - 1,
                    };
                    for d in &mut damaged[j..=last.min(lines - 1)] {
                        *d = true;
                    }
                }
            }
            anchors.push((t, c));
            prev = Some((k, c));
        }

        // Decimated index i stands for input samples i*decim .. (i+1)*decim.
        let half = (decim as f64 - 1.0) / 2.0;
        let scale = decim as f64;
        let to_input = |x: f64| x * scale + half;
        Self {
            audio,
            cuts,
            anchors: rising(anchors)
                .into_iter()
                .map(|(o, i)| (to_input(o), to_input(i)))
                .collect(),
            holes: holes
                .into_iter()
                .map(|(a, b)| (to_input(a), to_input(b)))
                .collect(),
            line_out: line * scale,
            damaged,
            patched,
            first: to_input(phase - offset),
        }
        .marked(known, rate)
    }

    /// Per radio line: its audio was lost or mistimed, and the rows it feeds
    /// should be rebuilt from their neighbours ([`fill_rows`]).
    #[must_use]
    pub fn damaged(&self) -> &[bool] {
        &self.damaged
    }

    /// Per radio line: part of it was lost and filled from the line before.
    /// The fill keeps the timing but is not that line's picture, so the rows
    /// it feeds should be rebuilt from their neighbours as well.
    #[must_use]
    pub fn patched(&self) -> &[bool] {
        &self.patched
    }

    /// Mark the lines lost to silence and to `known` holes.
    fn marked(mut self, known: &[(usize, usize)], rate: u32) -> Self {
        let run = (SILENT_RUN_SECS * f64::from(rate)).ceil() as usize;
        self.mark_silent_lines(self.line_out, self.first, run);
        self.mark_known(known);
        self
    }

    /// Mark the lines that known-lost audio falls in: a line wholly lost is
    /// rebuilt from its neighbours, one partly lost was filled from the line
    /// before.
    fn mark_known(&mut self, known: &[(usize, usize)]) {
        let lines = self.damaged.len();
        for &(start, len) in known {
            if len == 0 || lines == 0 {
                continue;
            }
            let t0 = self.output_at(self.cut_pos(start));
            let t1 = self.output_at(self.cut_pos(start + len));
            let from = ((t0 - self.first) / self.line_out).floor().max(0.0) as usize;
            let to = (((t1 - self.first) / self.line_out).ceil().max(0.0) as usize).min(lines);
            for k in from..to {
                let a = self.first + k as f64 * self.line_out;
                let b = a + self.line_out;
                if t0 <= a && t1 >= b {
                    self.damaged[k] = true;
                } else {
                    self.patched[k] = true;
                }
            }
        }
    }

    /// Where raw input sample `raw` is in the audio with zero runs cut.
    fn cut_pos(&self, raw: usize) -> f64 {
        let i = self
            .cuts
            .partition_point(|&(at, removed)| at + removed <= raw);
        let removed = if i == 0 { 0 } else { self.cuts[i - 1].1 };
        raw.saturating_sub(removed) as f64
    }

    /// The output sample that input position `x` (into the cut audio) goes
    /// to: the inverse of [`Self::input_at`].
    fn output_at(&self, x: f64) -> f64 {
        let a = &self.anchors;
        match a.len() {
            0 => x,
            _ if x <= a[0].1 => x - a[0].1 + a[0].0,
            n if x >= a[n - 1].1 => x - a[n - 1].1 + a[n - 1].0,
            _ => {
                let i = a.partition_point(|p| p.1 <= x);
                let (o0, i0) = a[i - 1];
                let (o1, i1) = a[i];
                if i1 - i0 <= 0.0 {
                    o0
                } else {
                    o0 + (x - i0) * (o1 - o0) / (i1 - i0)
                }
            }
        }
    }

    /// Mark lines whose audio is mostly silent, or holds a silent run
    /// longer than `run` samples: a dropout. `first` is the output position
    /// where line 0 starts.
    ///
    /// A run is measured whole, so one that crosses from one line into the
    /// next marks both, even when neither line holds more than `run` of it.
    /// Marking the lines at a dropout's edges can make it more than
    /// [`MAX_FILL_ROWS`] rows long, and then it is left as decoded.
    fn mark_silent_lines(&mut self, line: f64, first: f64, run: usize) {
        // Every silent run longer than `run`, as input spans.
        let mut long = Vec::new();
        let mut start = None;
        for (i, s) in self.audio.iter().enumerate() {
            match (s.abs() < SILENT, start) {
                (true, None) => start = Some(i),
                (false, Some(a)) => {
                    if i - a > run {
                        long.push((a, i));
                    }
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(a) = start
            && self.audio.len() - a > run
        {
            long.push((a, self.audio.len()));
        }
        for k in 0..self.damaged.len() {
            let start = first + k as f64 * line;
            let from = self.input_at(start).max(0.0) as usize;
            let to = (self.input_at(start + line).max(0.0) as usize).min(self.audio.len());
            if to <= from {
                continue;
            }
            let silent = self.audio[from..to]
                .iter()
                .filter(|s| s.abs() < SILENT)
                .count();
            // A run's ragged edge reaching a sample or two into a line does
            // not count: more than a tenth of `run` of it must be inside.
            let i = long.partition_point(|&(_, b)| b <= from);
            let in_run = long[i..]
                .iter()
                .take_while(|&&(a, _)| a < to)
                .any(|&(a, b)| b.min(to) - a.max(from) > run / 10);
            if in_run || silent as f64 > SILENT_SHARE * (to - from) as f64 {
                self.damaged[k] = true;
            }
        }
    }

    /// The input position (into the cut audio) for output sample `t`.
    fn input_at(&self, t: f64) -> f64 {
        let a = &self.anchors;
        match a.len() {
            0 => t,
            _ if t <= a[0].0 => t - a[0].0 + a[0].1,
            n if t >= a[n - 1].0 => t - a[n - 1].0 + a[n - 1].1,
            _ => {
                let i = a.partition_point(|p| p.0 <= t);
                let (o0, i0) = a[i - 1];
                let (o1, i1) = a[i];
                i0 + (t - o0) * (i1 - i0) / (o1 - o0)
            }
        }
    }

    /// The longest output, from `from` up to `to` samples, whose every
    /// sample comes from audio that arrived: at least `from`.
    #[must_use]
    pub fn covered(&self, from: usize, to: usize) -> usize {
        let to = to.max(from);
        let end = self.audio.len() as f64;
        if self.input_at(to as f64) < end {
            return to;
        }
        // `input_at` never falls as `t` rises, so search for the edge.
        let (mut lo, mut hi) = (from, to);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if self.input_at(mid as f64) < end {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    fn in_hole(&self, t: f64) -> bool {
        let i = self.holes.partition_point(|h| h.0 <= t);
        i > 0 && t < self.holes[i - 1].1
    }

    /// The raw input sample (before zero runs were cut) that output sample
    /// `t` comes from, rounded down and clamped to `raw_len`.
    #[must_use]
    pub fn raw_at(&self, t: usize, raw_len: usize) -> usize {
        let x = self.input_at(t as f64).max(0.0) as usize;
        let i = self.cuts.partition_point(|c| c.0 <= x);
        let cut = if i == 0 { 0 } else { self.cuts[i - 1].1 };
        (x + cut).min(raw_len)
    }

    /// Output samples `from..to`, on the perfect grid. Where the input has
    /// run out the output is silent.
    #[must_use]
    pub fn render(&self, from: usize, to: usize) -> Vec<f32> {
        let x = &self.audio;
        let at = |i: isize| -> f32 {
            usize::try_from(i)
                .ok()
                .and_then(|i| x.get(i))
                .copied()
                .unwrap_or(0.0)
        };
        (from..to)
            .map(|t| {
                let mut t = t as f64;
                // Lost audio is made up from the line before, at the same
                // place in the line.
                while self.in_hole(t) {
                    t -= self.line_out;
                }
                if t < 0.0 {
                    return 0.0;
                }
                let pos = self.input_at(t);
                if pos < -1.0 || pos > x.len() as f64 {
                    return 0.0;
                }
                let i = pos.floor();
                let f = (pos - i) as f32;
                let i = i as isize;
                // Catmull-Rom: smooth enough not to add tones of its own.
                let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
                p1 + 0.5
                    * f
                    * (p2 - p0
                        + f * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3
                            + f * (3.0 * (p1 - p2) + p3 - p0)))
            })
            .collect()
    }
}

/// `raw` as `f32` with stuffed zero runs after `keep` cut out, and where each
/// cut was. A run touching `known` lost audio is never cut.
fn cut_zero_runs(
    raw: &[i16],
    keep: usize,
    rate: u32,
    known: &[(usize, usize)],
) -> (Vec<f32>, Vec<(usize, usize)>) {
    let min = ((ZERO_RUN_MIN_SECS * f64::from(rate)).ceil() as usize).max(2);
    let max = (ZERO_RUN_MAX_SECS * f64::from(rate)) as usize;
    let mut audio = Vec::with_capacity(raw.len());
    let mut cuts = Vec::new();
    let mut removed = 0;
    audio.extend(raw[..keep].iter().map(|&s| f32::from(s) / 32768.0));
    let mut i = keep;
    while i < raw.len() {
        if raw[i] != 0 {
            audio.push(f32::from(raw[i]) / 32768.0);
            i += 1;
            continue;
        }
        let run = raw[i..].iter().take_while(|&&s| s == 0).count();
        // A run that reaches the end may be the start of silence: keep it.
        let lost = known.iter().any(|&(s, l)| s < i + run && i < s + l);
        if (min..=max).contains(&run) && i + run < raw.len() && !lost {
            removed += run;
            cuts.push((audio.len(), removed));
        } else {
            audio.extend(std::iter::repeat_n(0.0, run));
        }
        i += run;
    }
    (audio, cuts)
}

/// Per sample of the (decimated) audio: 1 where it is a sync tone, and the
/// instantaneous frequency (NaN where silent).
fn sync_band(audio: &[f32], decim: usize, fr: f64) -> (Vec<u8>, Vec<f32>) {
    // Decimate by averaging, which is low-pass enough for a tone under
    // 2.3 kHz at 16 kHz and up.
    let x: Vec<f32> = audio
        .chunks(decim)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect();
    let n = x.len();
    // Mix the band down around MIX_HZ.
    let w = 2.0 * std::f64::consts::PI * MIX_HZ / fr;
    let mixed: Vec<(f32, f32)> = x
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let (sin, cos) = ((i as f64 * w) % std::f64::consts::TAU).sin_cos();
            (s * cos as f32, -s * sin as f32)
        })
        .collect();
    // Low-pass: a Hann-windowed sinc, cut-off 1 kHz, 2.5 ms long, which
    // keeps 1.1 to 2.3 kHz and removes the image at -3.4 kHz.
    let taps = ((fr / 400.0) as usize) | 1;
    let c = taps / 2;
    let fc = 1000.0 / fr;
    let h: Vec<f32> = (0..taps)
        .map(|k| {
            let m = k as f64 - c as f64;
            let sinc = if m == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * m).sin() / (std::f64::consts::PI * m)
            };
            let hann =
                0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / (taps - 1) as f64).cos();
            (sinc * hann) as f32
        })
        .collect();
    let z: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let mut re = 0.0;
            let mut im = 0.0;
            for (k, hk) in h.iter().enumerate() {
                let j = i + k;
                if j < c || j - c >= n {
                    continue;
                }
                let (a, b) = mixed[j - c];
                re += hk * a;
                im += hk * b;
            }
            (re, im)
        })
        .collect();
    // Frequency from the phase step, averaged over about 1 ms.
    let avg = ((0.001 * fr) as usize).max(1);
    let steps: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            if i == 0 {
                return (0.0, 0.0);
            }
            let (a, b) = z[i];
            let (c0, d0) = z[i - 1];
            (a * c0 + b * d0, b * c0 - a * d0)
        })
        .collect();
    let mut sum = (0.0_f64, 0.0_f64);
    let mut out = vec![0_u8; n];
    let mut freq = vec![f32::NAN; n];
    let to_hz = fr / (2.0 * std::f64::consts::PI);
    // Magnitude floor: silence and stuffed zeros are never sync.
    let floor = f64::from(SILENT) * f64::from(SILENT) * 0.25;
    for i in 0..n + avg / 2 {
        if i < n {
            sum.0 += f64::from(steps[i].0);
            sum.1 += f64::from(steps[i].1);
        }
        if i >= avg {
            sum.0 -= f64::from(steps[i - avg].0);
            sum.1 -= f64::from(steps[i - avg].1);
        }
        let Some(centre) = i.checked_sub(avg / 2) else {
            continue;
        };
        if centre >= n {
            break;
        }
        let power = sum.0.hypot(sum.1) / avg as f64;
        if power < floor {
            continue;
        }
        let hz = MIX_HZ + sum.1.atan2(sum.0) * to_hz;
        freq[centre] = hz as f32;
        if (SYNC_LOW_HZ..=SYNC_HIGH_HZ).contains(&hz) {
            out[centre] = 1;
        }
    }
    (out, freq)
}

/// A timing step: (line before, line after, samples gained or, if
/// negative, lost).
type Step = (usize, usize, f64);

/// Pulse positions with the timing jitter of finding them removed: each run
/// of lines with no step between them is fitted with one straight line.
/// Also returns each step as (line before, line after, size): where two
/// consecutive found pulses are further apart, or closer, than their lines
/// by more than `limit`.
///
/// A step is kept only if it is still bigger than `limit` once the runs
/// either side are fitted; one that was only the jitter of finding two
/// pulses shrinks below that, and the runs either side of it are fitted as
/// one. A run too short to fit (under three pulses) keeps its jitter, so a
/// step next to one must be twice `limit`.
fn smooth(found: &[Option<f64>], line: f64, limit: f64) -> (Vec<Option<f64>>, Vec<Step>) {
    let pulses: Vec<(usize, f64)> = found
        .iter()
        .enumerate()
        .filter_map(|(k, p)| p.map(|c| (k, c)))
        .collect();
    // Candidate steps, as indices into `pulses` where a new run begins.
    let mut cuts: Vec<usize> = (1..pulses.len())
        .filter(|&i| {
            let ((j, cj), (k, c)) = (pulses[i - 1], pulses[i]);
            ((c - cj) - (k - j) as f64 * line).abs() > limit
        })
        .collect();
    loop {
        let mut out = found.to_vec();
        let bounds: Vec<usize> = std::iter::once(0)
            .chain(cuts.iter().copied())
            .chain(std::iter::once(pulses.len()))
            .collect();
        for w in bounds.windows(2) {
            fit(&pulses[w[0]..w[1]], line, &mut out);
        }
        let mut steps = Vec::new();
        let mut kept = Vec::new();
        for (n, &i) in cuts.iter().enumerate() {
            let (j, k) = (pulses[i - 1].0, pulses[i].0);
            let (Some(a), Some(b)) = (out[j], out[k]) else {
                continue;
            };
            let size = (b - a) - (k - j) as f64 * line;
            let fitted = i - bounds[n] >= 3 && bounds[n + 2] - i >= 3;
            if size.abs() > if fitted { limit } else { 2.0 * limit } {
                steps.push((j, k, size));
                kept.push(i);
            }
        }
        if kept.len() == cuts.len() {
            return (out, steps);
        }
        cuts = kept;
    }
}

/// Replace the pulses of `run` in `out` by one straight line fitted through
/// them; a run under three pulses is left as found.
fn fit(run: &[(usize, f64)], line: f64, out: &mut [Option<f64>]) {
    if run.len() < 3 {
        return;
    }
    let n = run.len() as f64;
    let mk = run.iter().map(|r| r.0 as f64).sum::<f64>() / n;
    let mc = run.iter().map(|r| r.1).sum::<f64>() / n;
    let sxy: f64 = run.iter().map(|r| (r.0 as f64 - mk) * (r.1 - mc)).sum();
    let sxx: f64 = run.iter().map(|r| (r.0 as f64 - mk).powi(2)).sum();
    let slope = if sxx > 0.0 { sxy / sxx } else { line };
    for &(k, _) in run {
        out[k] = Some(mc + (k as f64 - mk) * slope);
    }
}

/// Where, `h` samples after pulse `j` (at `cj`), the `err` samples lost
/// (negative) or added (positive) between pulses `j` and `j + 1` (at `ck`)
/// most likely are, judged by which split makes the line look most like
/// the lines either side of it. `None` when there is nothing to compare.
fn locate_hole(
    freq: &[f32],
    pulses: &[Option<f64>],
    j: usize,
    cj: f64,
    ck: f64,
    line: f64,
    err: f64,
) -> Option<f64> {
    // Reference lines: the one before `j` and the one after `j + 1`, each
    // starting at its own pulse.
    let mut refs = Vec::new();
    if let Some(Some(p)) = j.checked_sub(1).map(|i| pulses[i]) {
        refs.push(p);
    } else {
        refs.push(cj - line);
    }
    refs.push(ck);
    let span = (ck - cj).round() as i64;
    if span <= 2 {
        return None;
    }
    let get = |x: f64| -> f32 {
        let i = x.round();
        if i < 0.0 {
            return f32::NAN;
        }
        freq.get(i as usize).copied().unwrap_or(f32::NAN)
    };
    // How unlike the reference lines sample `t` of this span is, if it
    // sits at offset `o` of a line.
    let cost = |t: i64, o: f64| -> f64 {
        if o < 0.0 || o >= line {
            return 0.0;
        }
        let x = get(cj + t as f64);
        let mut total = 0.0;
        for &r in &refs {
            let y = get(r + o);
            if x.is_finite() && y.is_finite() {
                total += f64::from((x - y).abs().min(400.0));
            }
        }
        total
    };
    // Before the hole the span is where it should be; after it, it is
    // `err` off.
    let before: Vec<f64> = (0..span).map(|t| cost(t, t as f64)).collect();
    let after: Vec<f64> = (0..span).map(|t| cost(t, t as f64 - err)).collect();
    let total_after: f64 = after.iter().sum();
    let mut best = (f64::MAX, 0);
    let mut acc_before = 0.0;
    let mut acc_after = 0.0;
    for h in 0..=span as usize {
        let e = acc_before + (total_after - acc_after);
        if e < best.0 {
            best = (e, h);
        }
        if h < span as usize {
            acc_before += before[h];
            acc_after += after[h];
        }
    }
    // Keep the hole inside the span, clear of both pulses' anchors.
    let last = (span as f64 - 1.0 - err.max(0.0)).max(1.0);
    Some((best.1 as f64).clamp(1.0, last))
}

/// `anchors` with any that would make the map run backwards dropped.
fn rising(anchors: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(anchors.len());
    for a in anchors {
        if out.last().is_none_or(|p| a.0 > p.0 + 1e-9 && a.1 >= p.1) {
            out.push(a);
        }
    }
    out
}

fn prefix_sums(ind: &[u8]) -> Vec<u32> {
    let mut p = Vec::with_capacity(ind.len() + 1);
    let mut acc = 0;
    p.push(0);
    for &b in ind {
        acc += u32::from(b);
        p.push(acc);
    }
    p
}

struct PulseFinder<'a> {
    prefix: &'a [u32],
    /// Pulse width, in decimated samples.
    width: f64,
    /// Width of the band either side that should not be sync.
    guard: f64,
}

impl PulseFinder<'_> {
    /// Sync samples in `from..to`, clamped to the audio, and the span's length.
    fn count(&self, from: f64, to: f64) -> (f64, f64) {
        let n = self.prefix.len() - 1;
        let a = from.round().clamp(0.0, n as f64) as usize;
        let b = to.round().clamp(0.0, n as f64) as usize;
        if b <= a {
            return (0.0, 0.0);
        }
        (f64::from(self.prefix[b] - self.prefix[a]), (b - a) as f64)
    }

    /// (share of the pulse in the sync band, score) for a pulse centred at `c`.
    fn score(&self, c: f64) -> (f64, f64) {
        let half = self.width / 2.0;
        let (inside, len) = self.count(c - half, c + half);
        if len < self.width * 0.75 {
            return (0.0, f64::MIN);
        }
        let fill = inside / len;
        let (l, ll) = self.count(c - half - self.guard, c - half);
        let (r, rl) = self.count(c + half, c + half + self.guard);
        let side = if ll + rl > 0.0 {
            (l + r) / (ll + rl)
        } else {
            0.0
        };
        (fill, fill - side)
    }

    /// The best pulse within `radius` of `around`: its centre, if it is
    /// clearly a pulse.
    fn find(&self, around: f64, radius: f64) -> Option<f64> {
        let from = (around - radius).floor() as i64;
        let to = (around + radius).ceil() as i64;
        let mut best = f64::MIN;
        let mut best_fill = 0.0;
        let mut at = Vec::new();
        for i in from..=to {
            let (fill, s) = self.score(i as f64);
            if s > best + 1e-9 {
                best = s;
                best_fill = fill;
                at.clear();
                at.push(i);
            } else if (s - best).abs() <= 1e-9 {
                at.push(i);
            }
        }
        if best_fill < PULSE_FILL || best < PULSE_CONTRAST || at.is_empty() {
            return None;
        }
        // A plateau of equal scores: its middle. Only a contiguous run
        // around the first best counts.
        let first = at[0];
        let mut last = first;
        for &i in &at[1..] {
            if i == last + 1 {
                last = i;
            } else {
                break;
            }
        }
        Some((first + last) as f64 / 2.0)
    }
}

/// The pulse of a line whose pulse was `predicted`, [`LOST_LINES`] or more
/// lines after the last pulse found, when none is near the prediction: the
/// phase is looked for anywhere within half a line either side, scored over
/// the next few lines (up to [`PHASE_LINES`] of the `left` the input still
/// reaches) the way the first phase is found, so one stray tone in noise
/// cannot pass for it.
///
/// A shift near half a line is ambiguous: half a line late and half a line
/// early are equally near the prediction. The better-scoring one is taken,
/// and on a tie the earlier. Taking the wrong one puts the rest of the
/// picture one row off, which on a smooth picture is hard to see; losing
/// lock would lose all of it.
fn reacquire(
    finder: &PulseFinder<'_>,
    predicted: f64,
    line: f64,
    left: usize,
    radius: f64,
) -> Option<f64> {
    let n = PHASE_LINES.min(left);
    // Too few lines left to tell a pulse train from noise.
    if n < PHASE_LINES / 2 {
        return None;
    }
    #[cfg(test)]
    tests::SEARCHES.with(|c| c.set(c.get() + 1));
    let lo = (predicted - line / 2.0).floor() as i64;
    let hi = (predicted + line / 2.0).ceil() as i64;
    let mut best = (f64::MIN, predicted);
    for p in lo..=hi {
        let s: f64 = (0..n)
            .map(|m| finder.score(p as f64 + m as f64 * line).1.max(-1.0))
            .sum();
        if s > best.0 {
            best = (s, p as f64);
        }
    }
    if best.0 < PULSE_CONTRAST * n as f64 {
        return None;
    }
    finder.find(best.1, radius.min(0.1 * line))
}

/// Every radio line's pulse centre (decimated samples), `None` where none
/// was found. A pulse is looked for near where the last one found says it
/// should be, or, once lock is lost, see [`reacquire`]. Only the first
/// `reach` lines are in the input, so lock is never looked for past them.
fn track(
    finder: &PulseFinder<'_>,
    lines: usize,
    reach: usize,
    line: f64,
    offset: f64,
    radius: f64,
    tol: f64,
) -> Vec<Option<f64>> {
    // The phase: the offset within a line (near the nominal one) whose first
    // few pulses score best together.
    let probe = PHASE_LINES.min(lines);
    let mut phase = offset;
    let mut best = f64::MIN;
    let lo = (offset - line / 2.0).floor() as i64;
    let hi = (offset + line / 2.0).ceil() as i64;
    for p in lo..=hi {
        let s: f64 = (0..probe)
            .map(|k| finder.score(p as f64 + k as f64 * line).1.max(-1.0))
            .sum();
        if s > best {
            best = s;
            phase = p as f64;
        }
    }

    let mut out = vec![None; lines];
    let mut last: Option<(usize, f64)> = None;
    // Searches for the phase that failed since lock was lost, and the first
    // line the next may be tried. The first few are tried line by line, as
    // the picture usually comes back within a line or two; after that, in
    // a long stretch of noise, only every few lines. `retry` is not reset
    // when a pulse is found again: it is at most PHASE_LINES / 2 lines past
    // a failed search, and lock must then be lost for LOST_LINES more lines
    // before the next one, which takes it past `retry` anyway.
    let (mut failed, mut retry) = (0, 0);
    for (k, slot) in out.iter_mut().enumerate() {
        let predicted = match last {
            Some((j, c)) => c + (k - j) as f64 * line,
            None => phase + k as f64 * line,
        };
        let missed = last.is_some_and(|(j, _)| k - j >= LOST_LINES) && k >= retry;
        let c = finder.find(predicted, radius).or_else(|| {
            if !missed {
                return None;
            }
            let c = reacquire(finder, predicted, line, reach.saturating_sub(k), radius);
            if c.is_none() {
                failed += 1;
                if failed >= PHASE_LINES / 2 {
                    retry = k + PHASE_LINES / 2;
                }
            }
            c
        });
        if let Some(c) = c {
            *slot = Some(c);
            last = Some((k, c));
            failed = 0;
        }
    }

    // A lone pulse out of step with the pulses either side of it, which
    // agree with each other, is a false find, not a timing step: the
    // timing before and after it is the same. `tol` is the size of the
    // smallest step, so a pulse moved far enough to look like one goes.
    // The neighbours are the nearest pulses found, as first found.
    let found = out.clone();
    let mut before: Option<(usize, f64)> = None;
    for k in 0..lines {
        let Some(c) = found[k] else { continue };
        let after = (k + 1..lines).find_map(|m| found[m].map(|b| (m, b)));
        if let (Some((j, a)), Some((m, b))) = (before, after) {
            let expected = a + (b - a) * (k - j) as f64 / (m - j) as f64;
            if ((b - a) - (m - j) as f64 * line).abs() <= tol && (c - expected).abs() > tol {
                out[k] = None;
            }
        }
        before = Some((k, c));
    }
    // Anchors must rise strictly.
    let mut prev = f64::MIN;
    for slot in &mut out {
        if let Some(c) = *slot {
            if c <= prev {
                *slot = None;
            } else {
                prev = c;
            }
        }
    }
    out
}

/// Rebuild the `marked` rows of a picture from its good rows.
///
/// `rgb` is a `width`-pixel RGB picture, three bytes a pixel, and each marked
/// row among its first `limit` is blended from the nearest good rows above
/// and below. Runs longer than [`MAX_FILL_ROWS`] are left alone. Returns the
/// rows rebuilt.
pub fn fill_rows(rgb: &mut [u8], width: u32, marked: &[bool], limit: u32) -> u32 {
    let stride = width as usize * 3;
    let rows = (limit as usize).min(marked.len()).min(rgb.len() / stride);
    let mut filled = 0;
    let mut y = 0;
    while y < rows {
        if !marked[y] {
            y += 1;
            continue;
        }
        let start = y;
        while y < rows && marked[y] {
            y += 1;
        }
        let end = y; // first good row after the run, or `rows`
        let run = end - start;
        if run as u32 > MAX_FILL_ROWS {
            continue;
        }
        let above = start.checked_sub(1);
        let below = (end < rows).then_some(end);
        if above.is_none() && below.is_none() {
            continue;
        }
        for r in start..end {
            for x in 0..stride {
                let v = match (above, below) {
                    (Some(a), Some(b)) => {
                        let t = (r - a) as f32 / (b - a) as f32;
                        let va = f32::from(rgb[a * stride + x]);
                        let vb = f32::from(rgb[b * stride + x]);
                        (va + (vb - va) * t).round() as u8
                    }
                    (Some(a), None) => rgb[a * stride + x],
                    (None, Some(b)) => rgb[b * stride + x],
                    // Ruled out above: a run with no good row either side.
                    (None, None) => continue,
                };
                rgb[r * stride + x] = v;
            }
        }
        filled += run as u32;
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_FILL_ROWS, PHASE_LINES, Plan, PulseFinder, cut_zero_runs, fill_rows, prefix_sums,
        reacquire, sync_band, track,
    };
    use crate::modes::SCOTTIE_2;
    use std::vec;
    use std::vec::Vec;

    thread_local! {
        /// Whole-line phase searches [`reacquire`] has run on this thread.
        pub(super) static SEARCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    const RATE: f64 = 8000.0;
    /// A Scottie 2 line and sync pulse, in samples at [`RATE`]
    /// (rounded to whole samples, so the test signal needs no fractions).
    const LINE: f64 = 2222.0;
    const SYNC: f64 = 0.009 * RATE;

    /// A tone of `hz` for `n` samples, continuing `phase`.
    fn tone(out: &mut Vec<f32>, hz: f64, n: usize, phase: &mut f64) {
        for _ in 0..n {
            *phase += std::f64::consts::TAU * hz / RATE;
            out.push((phase.sin() * 0.5) as f32);
        }
    }

    /// `lines` lines of a pulse train: a 1200 Hz sync pulse centred
    /// `at` samples into each line, and picture tones between.
    fn lines(out: &mut Vec<f32>, lines: usize, at: f64, phase: &mut f64) {
        for k in 0..lines {
            let start = out.len();
            let pulse = at - SYNC / 2.0;
            tone(out, 1500.0 + (k % 7) as f64 * 100.0, pulse as usize, phase);
            tone(out, 1200.0, SYNC as usize, phase);
            let rest = (start as f64 + LINE) as usize - out.len();
            tone(out, 2000.0 - (k % 5) as f64 * 100.0, rest, phase);
        }
    }

    /// White noise, with (if `bursts`) a stray 1200 Hz burst a sync pulse
    /// long every one to five lines at no regular spacing (a voice or a
    /// carrier passing through the sync band).
    fn noise(out: &mut Vec<f32>, n: usize, seed: u32, bursts: bool, phase: &mut f64) {
        let mut x = seed.max(1);
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let end = out.len() + n;
        while out.len() < end {
            let gap = (next() % (4 * LINE as u32)) as usize + LINE as usize;
            for _ in 0..gap.min(end - out.len()) {
                out.push((f64::from(next() >> 16) / 65536.0 - 0.5) as f32);
            }
            let burst = if bursts { SYNC as usize } else { 0 }.min(end - out.len());
            tone(out, 1200.0, burst, phase);
        }
    }

    /// The pulse finder over `audio`'s sync band.
    fn finder_over(audio: &[f32], run: impl FnOnce(&PulseFinder<'_>)) {
        let (ind, _) = sync_band(audio, 1, RATE);
        let prefix = prefix_sums(&ind);
        run(&PulseFinder {
            prefix: &prefix,
            width: SYNC,
            guard: SYNC / 2.0,
        });
    }

    /// The pulses `track` finds in `audio`, a Scottie 2-like picture of
    /// `n` lines with its first pulse centred at `at`.
    fn tracked(audio: &[f32], n: usize, at: f64) -> Vec<Option<f64>> {
        let mut found = Vec::new();
        finder_over(audio, |f| {
            found = track(f, n, n, LINE, at, 0.045 * RATE, 0.0015 * RATE);
        });
        found
    }

    #[test]
    fn lock_is_regained_after_a_dropout_that_shifts_the_phase() {
        let at = 0.2 * LINE;
        let mut phase = 0.0;
        let mut audio = Vec::new();
        lines(&mut audio, 80, at, &mut phase);
        // About 360 ms of silence, and the sender comes back 0.3 of a line
        // later than the grid: line 81's pulse and on are 0.3 lines late.
        audio.extend(std::iter::repeat_n(0.0, (1.3 * LINE) as usize));
        lines(&mut audio, 100, at, &mut phase);
        let found = tracked(&audio, 180, at);
        assert!(found[..80].iter().all(Option::is_some));
        let missed: Vec<usize> = (82..180)
            .filter(|&k| {
                let want = at + (k as f64 + 0.3) * LINE;
                found[k].is_none_or(|c| (c - want).abs() > 4.0)
            })
            .collect();
        assert!(missed.is_empty(), "missed lines {missed:?}");
    }

    #[test]
    fn lock_is_regained_soon_after_a_long_noisy_dropout() {
        // Three seconds of noise, then the picture again 0.3 of a line early.
        // Searches that keep failing inside the noise back off, but not so
        // far that the pulses after it are found late.
        let at = 0.2 * LINE;
        let mut phase = 0.0;
        let mut audio = Vec::new();
        lines(&mut audio, 80, at, &mut phase);
        let quiet = (10.7 * LINE) as usize;
        noise(&mut audio, quiet, 7, false, &mut phase);
        lines(&mut audio, 100, at, &mut phase);
        let found = tracked(&audio, 190, at);
        let first = (91..190)
            .find(|&k| {
                let want = at + (k as f64 - 0.3) * LINE;
                found[k].is_some_and(|c| (c - want).abs() <= 4.0)
            })
            .expect("lock regained");
        // Pulses are back from line 91; backing off may delay lock by up
        // to PHASE_LINES / 2 lines.
        assert!(
            first <= 91 + PHASE_LINES / 2,
            "lock regained at line {first}"
        );
        let missed: Vec<usize> = (first..190)
            .filter(|&k| {
                let want = at + (k as f64 - 0.3) * LINE;
                found[k].is_none_or(|c| (c - want).abs() > 4.0)
            })
            .collect();
        assert!(missed.is_empty(), "missed lines {missed:?}");
    }

    /// `track` over 80 lines of picture and then noise to line `n`, told the
    /// input reaches `reach` lines: the pulses found, and how many
    /// whole-line phase searches it ran.
    fn searches_in_noise(n: usize, reach: usize) -> (Vec<Option<f64>>, usize) {
        let at = 0.2 * LINE;
        let mut phase = 0.0;
        let mut audio = Vec::new();
        lines(&mut audio, 80, at, &mut phase);
        noise(&mut audio, (n - 80) * LINE as usize, 3, false, &mut phase);
        let mut found = Vec::new();
        SEARCHES.with(|c| c.set(0));
        finder_over(&audio, |f| {
            found = track(f, n, reach, LINE, at, 0.045 * RATE, 0.0015 * RATE);
        });
        (found, SEARCHES.with(std::cell::Cell::get))
    }

    #[test]
    fn a_long_noisy_tail_is_searched_quickly_then_every_few_lines() {
        // The last pulse is line 79. Lock is looked for from line 82: line
        // by line for PHASE_LINES / 2 tries (82..=85), then every
        // PHASE_LINES / 2 lines (89, 93, ...) up to the last line that
        // still has PHASE_LINES / 2 lines of input after it.
        let n = 180;
        let (found, searches) = searches_in_noise(n, n);
        assert!(found[..80].iter().all(Option::is_some));
        assert!(found[80..].iter().all(Option::is_none), "{found:?}");
        let quick = PHASE_LINES / 2;
        let last = n - PHASE_LINES / 2;
        let spaced = (82 + quick + PHASE_LINES / 2 - 1..=last)
            .step_by(PHASE_LINES / 2)
            .count();
        assert_eq!(searches, quick + spaced);
        // Searched at every line, it would be about four times as many.
        assert!(searches * 3 < last - 82, "{searches}");
    }

    #[test]
    fn no_search_runs_past_the_input() {
        // The input reaches line 90 of 180: the searches stop where too few
        // lines of it are left to score, however long the picture is.
        let (_, short) = searches_in_noise(180, 90);
        let (_, long) = searches_in_noise(180, 180);
        // 82..=85 quick, then 89 is too near the end of 90 lines.
        assert_eq!(short, PHASE_LINES / 2);
        assert!(long > short);
    }

    #[test]
    fn noise_after_a_dropout_is_never_re_acquired() {
        // Searching a whole line for the phase must not lock onto noise,
        // even noise with stray sync tones in it: one stray tone does not
        // make a pulse train. Asked at every line of the noise, in every
        // place, it finds nothing. (The ordinary search near the predicted
        // pulse can still take a stray tone, as it always could; the lone
        // pulse check and the step limits deal with those.)
        for (seed, bursts) in (1..=8).flat_map(|s| [(s, true), (s, false)]) {
            let at = 0.2 * LINE;
            let mut phase = 0.0;
            let mut audio = Vec::new();
            lines(&mut audio, 80, at, &mut phase);
            audio.extend(std::iter::repeat_n(0.0, (0.4 * RATE) as usize));
            noise(
                &mut audio,
                (100.0 * LINE) as usize,
                seed,
                bursts,
                &mut phase,
            );
            finder_over(&audio, |f| {
                for k in 82..180 {
                    for nudge in [0.0, 0.25, 0.5, 0.75] {
                        let predicted = at + (k as f64 + nudge) * LINE;
                        let got = reacquire(f, predicted, LINE, 180 - k, 0.045 * RATE);
                        assert!(got.is_none(), "seed {seed}: line {k}+{nudge}: {got:?}");
                    }
                }
            });
        }
    }

    #[test]
    fn stuffed_zeros_are_cut_and_silence_is_kept() {
        let mut raw = vec![100_i16; 50];
        raw.extend([0; 16]); // 1 ms at 16 kHz: stuffing
        raw.extend([100; 50]);
        raw.extend([0; 1600]); // 100 ms: silence
        raw.extend([100; 10]);
        let (audio, cuts) = cut_zero_runs(&raw, 0, 16_000, &[]);
        assert_eq!(audio.len(), raw.len() - 16);
        assert_eq!(cuts, vec![(50, 16)]);
    }

    #[test]
    fn the_samples_already_decoded_are_never_cut() {
        let mut raw = vec![0_i16; 16];
        raw.extend([100; 10]);
        let (audio, cuts) = cut_zero_runs(&raw, 20, 16_000, &[]);
        assert_eq!(audio.len(), raw.len());
        assert_eq!(cuts, Vec::new());
    }

    #[test]
    fn known_lost_audio_is_never_cut() {
        let mut raw = vec![100_i16; 50];
        raw.extend([0; 16]);
        raw.extend([100; 50]);
        let (audio, cuts) = cut_zero_runs(&raw, 0, 16_000, &[(50, 16)]);
        assert_eq!(audio.len(), raw.len());
        assert_eq!(cuts, Vec::new());
    }

    #[test]
    fn marked_rows_are_blended_from_their_neighbours() {
        // Five one-pixel rows: 0, x, x, 90, x (the last has no row below).
        let mut rgb: Vec<u8> = [0, 7, 7, 90, 7].iter().flat_map(|&v| [v; 3]).collect();
        let marked = [false, true, true, false, true];
        assert_eq!(fill_rows(&mut rgb, 1, &marked, 5), 3);
        let col: Vec<u8> = rgb.chunks(3).map(|p| p[0]).collect();
        assert_eq!(col, vec![0, 30, 60, 90, 90]);
    }

    #[test]
    fn a_long_gap_is_not_made_up() {
        let n = MAX_FILL_ROWS as usize + 3;
        let mut rgb = vec![5_u8; n * 3];
        let mut marked = vec![true; n];
        marked[0] = false;
        marked[n - 1] = false;
        let before = rgb.clone();
        assert_eq!(fill_rows(&mut rgb, 1, &marked, n as u32), 0);
        assert_eq!(rgb, before);
    }

    /// Which of the first lines of a Scottie 2 picture `raw` (at 8 kHz, no
    /// sync pulses, so the timing is left alone) are marked damaged.
    fn damaged_lines(raw: &[i16]) -> Vec<usize> {
        let timing = SCOTTIE_2.scan_timing();
        let plan = Plan::new(raw, 0, RATE as u32, &timing, &[]);
        (0..plan.damaged.len())
            .filter(|&k| plan.damaged[k])
            .collect()
    }

    #[test]
    fn a_short_silence_marks_its_line_and_dark_or_quiet_audio_does_not() {
        // Black (1500 Hz) at -40 dBFS: as dark and as quiet as a picture
        // gets, but still a carrier, so no line is marked.
        let n = (10.5 * LINE) as usize;
        let black: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                (328.0 * (std::f64::consts::TAU * 1500.0 * t).sin()) as i16
            })
            .collect();
        assert_eq!(damaged_lines(&black), Vec::<usize>::new());

        // 90 ms of silence inside line 5 marks line 5 alone.
        let mut dropout = black.clone();
        let at = (5.3 * LINE) as usize;
        dropout[at..at + (0.09 * RATE) as usize].fill(0);
        assert_eq!(damaged_lines(&dropout), vec![5]);

        // 20 ms of stuffed zeros is cut before lines are marked.
        let mut stuffed = black[..at].to_vec();
        stuffed.extend([0; 160]);
        stuffed.extend_from_slice(&black[at..]);
        assert_eq!(damaged_lines(&stuffed), Vec::<usize>::new());
    }

    #[test]
    fn a_silent_run_across_a_line_boundary_marks_both_lines() {
        let n = (10.5 * LINE) as usize;
        let black: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                (328.0 * (std::f64::consts::TAU * 1500.0 * t).sin()) as i16
            })
            .collect();
        // Near-silence (not exact zeros, so nothing is cut as stuffing),
        // half either side of the start of line 5.
        let quiet = |secs: f64| {
            let mut audio = black.clone();
            let half = (secs / 2.0 * RATE) as usize;
            let edge = (5.0 * LINE) as usize;
            audio[edge - half..edge + half].fill(3);
            audio
        };
        // 19 ms split 9.5 / 9.5: neither half alone is over 10 ms.
        assert_eq!(damaged_lines(&quiet(0.019)), vec![4, 5]);
        // 8 ms split 4 / 4 is still too short to mark anything.
        assert_eq!(damaged_lines(&quiet(0.008)), Vec::<usize>::new());
    }

    #[test]
    fn output_stops_where_the_input_does() {
        // Half a second of input with no pulses in it: nothing to re-time,
        // and no more output than the input can fill.
        let timing = SCOTTIE_2.scan_timing();
        let plan = Plan::new(&[100_i16; 8000], 0, 16_000, &timing, &[]);
        assert_eq!(plan.covered(0, 20_000), 7999);
        assert_eq!(plan.covered(0, 5_000), 5_000);
        assert_eq!(plan.covered(9_000, 20_000), 9_000);
    }
}
