// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The over splitter: turns the on-off flow of a conference member (nothing
//! is sent while nobody talks) into one run of audio per over.
//!
//! Pure: the time is a parameter, the clock is never read.

use std::time::{Duration, Instant};

/// The two numbers that pin the splitter's behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitterConfig {
    /// A gap this long or longer ends the over; a shorter one is filled.
    pub merge_gap: Duration,
    /// Silence appended after the last voice frame of a closed over.
    pub tail: Duration,
    /// Length of one frame.
    pub frame: Duration,
}

impl Default for SplitterConfig {
    fn default() -> Self {
        Self {
            merge_gap: Duration::from_millis(500),
            tail: Duration::from_secs(1),
            frame: Duration::from_millis(20),
        }
    }
}

/// What the splitter tells its caller to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverEvent {
    /// A new over begins; open a file.
    Start,
    /// Samples to append (voice or silence).
    Audio(Vec<i16>),
    /// The over is closed, tail already emitted; close the file.
    End,
}

/// Splits a frame flow into overs.
///
/// The splitter keeps a cursor: the nominal time the audio written so far
/// has reached, one frame per voice frame plus any silence it filled. A
/// frame is placed at the cursor unless it arrived more than
/// [`MAX_SLIP_FRAMES`] frames past it, so arrival jitter never splices
/// silence into an over; only a real gap in the flow is filled. Callers
/// that drain a queue at one instant pass the whole batch to
/// [`OverSplitter::push_batch`], which back-dates the frames one frame
/// apart so a stall of the reader is not mistaken for a gap.
pub struct OverSplitter {
    cfg: SplitterConfig,
    frame_samples: usize,
    /// Where the written audio has reached while an over is open.
    cursor: Option<Instant>,
}

/// A frame that arrives at most this many frames past the cursor is jitter:
/// it is written at the cursor with no silence before it.
pub const MAX_SLIP_FRAMES: u32 = 2;

impl OverSplitter {
    /// `frame_samples` is the samples per frame (320 at 16 kHz, 20 ms).
    #[must_use]
    pub fn new(cfg: SplitterConfig, frame_samples: usize) -> Self {
        assert!(frame_samples > 0, "frame_samples must be non-zero");
        assert!(!cfg.frame.is_zero(), "frame must be non-zero");
        Self {
            cfg,
            frame_samples,
            cursor: None,
        }
    }

    /// Whether an over is currently open.
    #[must_use]
    pub fn in_over(&self) -> bool {
        self.cursor.is_some()
    }

    /// Whole frames in `d`, rounded to nearest.
    fn frames_in(&self, d: Duration) -> usize {
        let f = self.cfg.frame.as_nanos();
        usize::try_from((d.as_nanos() + f / 2) / f).unwrap_or(usize::MAX)
    }

    /// Feed one voice frame that arrived at `at`. A frame `merge_gap` or
    /// more past the cursor closes the open over (tail and `End`) and
    /// starts a new one; one more than [`MAX_SLIP_FRAMES`] frames past it
    /// is preceded by the silence that fills the gap; anything nearer is
    /// written straight after the audio before it.
    pub fn push(&mut self, frame: &[i16], at: Instant) -> Vec<OverEvent> {
        let mut out = Vec::new();
        self.push_into(frame, at, &mut out);
        out
    }

    /// Feed `frames`, all drained from a queue at `now`. They are taken to
    /// have arrived one frame apart, the last at `now`, the first
    /// `(k - 1)` frames earlier, so a batch adds no silence of its own.
    /// Call [`OverSplitter::poll`] after the batch, never before it.
    pub fn push_batch<F: AsRef<[i16]>>(&mut self, frames: &[F], now: Instant) -> Vec<OverEvent> {
        let mut out = Vec::new();
        let k = u32::try_from(frames.len()).unwrap_or(u32::MAX);
        for (i, frame) in frames.iter().enumerate() {
            let back = k
                .saturating_sub(1)
                .saturating_sub(u32::try_from(i).unwrap_or(u32::MAX));
            let at = now
                .checked_sub(self.cfg.frame.saturating_mul(back))
                .unwrap_or(now);
            self.push_into(frame.as_ref(), at, &mut out);
        }
        out
    }

    fn push_into(&mut self, frame: &[i16], at: Instant, out: &mut Vec<OverEvent>) {
        let next = match self.cursor {
            None => {
                out.push(OverEvent::Start);
                at
            }
            Some(cursor) => {
                let ahead = at.saturating_duration_since(cursor);
                if ahead >= self.cfg.merge_gap {
                    self.close(out);
                    out.push(OverEvent::Start);
                    at
                } else if ahead > self.cfg.frame.saturating_mul(MAX_SLIP_FRAMES) {
                    let gap = self.frames_in(ahead);
                    let fill = gap.saturating_sub(1);
                    if fill > 0 {
                        out.push(OverEvent::Audio(vec![0; fill * self.frame_samples]));
                    }
                    cursor + self.cfg.frame * u32::try_from(gap).unwrap_or(u32::MAX)
                } else {
                    cursor + self.cfg.frame
                }
            }
        };
        out.push(OverEvent::Audio(frame.to_vec()));
        self.cursor = Some(next);
    }

    /// Emits the tail and `End` of the open over.
    fn close(&mut self, out: &mut Vec<OverEvent>) {
        self.cursor = None;
        let tail = self.frames_in(self.cfg.tail) * self.frame_samples;
        if tail > 0 {
            out.push(OverEvent::Audio(vec![0; tail]));
        }
        out.push(OverEvent::End);
    }

    /// Advance the clock with no frame: closes the over once `now` is
    /// `merge_gap` past the cursor, emitting the tail exactly once. Call it
    /// only after every queued frame has been pushed.
    pub fn poll(&mut self, now: Instant) -> Vec<OverEvent> {
        let Some(cursor) = self.cursor else {
            return Vec::new();
        };
        if now.saturating_duration_since(cursor) < self.cfg.merge_gap {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.close(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: usize = 320;
    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn splitter() -> OverSplitter {
        OverSplitter::new(SplitterConfig::default(), F)
    }
    fn frame() -> Vec<i16> {
        vec![7; F]
    }
    fn total(ev: &[OverEvent]) -> usize {
        ev.iter()
            .map(|e| match e {
                OverEvent::Audio(a) => a.len(),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn first_frame_starts_an_over() {
        let mut s = splitter();
        let ev = s.push(&frame(), Instant::now());
        assert_eq!(ev, vec![OverEvent::Start, OverEvent::Audio(frame())]);
        assert!(s.in_over());
    }

    #[test]
    fn a_short_gap_is_filled_and_merged() {
        let mut s = splitter();
        let t0 = Instant::now();
        for i in 0..=10 {
            s.push(&frame(), t0 + MS(i * 20));
        }
        let ev = s.push(&frame(), t0 + MS(600));
        assert!(!ev.contains(&OverEvent::End));
        assert!(!ev.contains(&OverEvent::Start));
        assert_eq!(ev[0], OverEvent::Audio(vec![0; 19 * F]));
        assert_eq!(ev[1], OverEvent::Audio(frame()));
        assert_eq!(ev.len(), 2);
    }

    #[test]
    fn a_half_second_gap_ends_the_over_with_a_one_second_tail() {
        let mut s = splitter();
        let t0 = Instant::now();
        for i in 0..10 {
            s.push(&frame(), t0 + MS(i * 20));
        }
        let last = t0 + MS(180);
        assert_eq!(s.poll(last + MS(499)), [] as [crate::over::OverEvent; 0]);
        let ev = s.poll(last + MS(500));
        assert_eq!(ev, vec![OverEvent::Audio(vec![0; 50 * F]), OverEvent::End]);
        assert_eq!(total(&ev), 50 * 320);
        assert!(!s.in_over());
    }

    #[test]
    fn a_frame_after_the_end_starts_a_new_over() {
        let mut s = splitter();
        let t0 = Instant::now();
        s.push(&frame(), t0);
        s.poll(t0 + MS(500));
        let ev = s.push(&frame(), t0 + MS(700));
        assert_eq!(ev, vec![OverEvent::Start, OverEvent::Audio(frame())]);
    }

    #[test]
    fn a_long_gap_seen_by_push_closes_then_restarts() {
        let mut s = splitter();
        let t0 = Instant::now();
        s.push(&frame(), t0);
        let ev = s.push(&frame(), t0 + MS(2000));
        assert_eq!(
            ev,
            vec![
                OverEvent::Audio(vec![0; 50 * F]),
                OverEvent::End,
                OverEvent::Start,
                OverEvent::Audio(frame()),
            ]
        );
    }

    #[test]
    fn poll_while_quiet_emits_nothing() {
        let mut s = splitter();
        let t0 = Instant::now();
        assert_eq!(s.poll(t0), [] as [crate::over::OverEvent; 0]);
        assert_eq!(s.poll(t0 + MS(10_000)), [] as [crate::over::OverEvent; 0]);
    }

    #[test]
    fn the_tail_is_written_once_even_when_poll_runs_late() {
        let mut s = splitter();
        let t0 = Instant::now();
        s.push(&frame(), t0);
        let ev = s.poll(t0 + Duration::from_secs(5));
        assert_eq!(total(&ev), 50 * F);
        assert_eq!(
            s.poll(t0 + Duration::from_secs(6)),
            [] as [crate::over::OverEvent; 0]
        );
    }

    fn has(ev: &[OverEvent], e: &OverEvent) -> bool {
        ev.contains(e)
    }

    #[test]
    fn a_batch_drained_at_one_instant_adds_no_fill() {
        let mut s = splitter();
        let t0 = Instant::now();
        let mut ev = Vec::new();
        for i in 0..10 {
            ev.extend(s.push(&frame(), t0 + MS(i * 20)));
        }
        // The reader slept 100 ms; five frames waited in its queue.
        let batch = vec![frame(); 5];
        ev.extend(s.push_batch(&batch, t0 + MS(280)));
        ev.extend(s.poll(t0 + MS(280)));
        assert_eq!(total(&ev), 15 * F, "voice only, no silence spliced in");
        assert!(!has(&ev, &OverEvent::End));
    }

    #[test]
    fn jitter_within_two_frames_is_written_back_to_back() {
        let mut s = splitter();
        let t0 = Instant::now();
        let mut ev = s.push(&frame(), t0);
        // 35, 20, 10 and 35 ms apart: late and early, never more than two
        // frames past the cursor.
        for at in [35, 55, 65, 100] {
            ev.extend(s.push(&frame(), t0 + MS(at)));
        }
        assert_eq!(total(&ev), 5 * F);
    }

    #[test]
    fn a_600_ms_stall_of_the_reader_does_not_split_the_over() {
        let mut s = splitter();
        let t0 = Instant::now();
        let mut ev = Vec::new();
        for i in 0..10 {
            ev.extend(s.push(&frame(), t0 + MS(i * 20)));
        }
        let last = t0 + MS(180);
        // Thirty frames queued while the reader was stalled for 600 ms.
        let batch = vec![frame(); 30];
        ev.extend(s.push_batch(&batch, last + MS(600)));
        ev.extend(s.poll(last + MS(600)));
        assert!(!has(&ev, &OverEvent::End), "{} events", ev.len());
        assert_eq!(ev.iter().filter(|e| **e == OverEvent::Start).count(), 1);
        assert_eq!(total(&ev), 40 * F);
        assert!(s.in_over());
    }

    #[test]
    fn a_push_480_ms_past_the_cursor_merges_and_500_ms_splits() {
        let t0 = Instant::now();
        let mut s = splitter();
        s.push(&frame(), t0);
        let ev = s.push(&frame(), t0 + MS(480));
        assert_eq!(
            ev,
            vec![OverEvent::Audio(vec![0; 23 * F]), OverEvent::Audio(frame())]
        );
        let mut s = splitter();
        s.push(&frame(), t0);
        let ev = s.push(&frame(), t0 + MS(500));
        assert_eq!(
            ev,
            vec![
                OverEvent::Audio(vec![0; 50 * F]),
                OverEvent::End,
                OverEvent::Start,
                OverEvent::Audio(frame()),
            ]
        );
    }

    #[test]
    fn a_gap_after_a_batch_is_measured_from_the_cursor() {
        let mut s = splitter();
        let t0 = Instant::now();
        let batch = vec![frame(); 3];
        let mut ev = s.push_batch(&batch, t0 + MS(40));
        // The batch puts the cursor at t0 + 40 ms; a frame 100 ms later
        // fills 4 frames, and the over closes 500 ms after that frame.
        ev.extend(s.push(&frame(), t0 + MS(140)));
        assert_eq!(total(&ev), (3 + 4 + 1) * F);
        assert_eq!(s.poll(t0 + MS(639)), [] as [crate::over::OverEvent; 0]);
        assert_eq!(s.poll(t0 + MS(640)).last(), Some(&OverEvent::End));
    }
}
