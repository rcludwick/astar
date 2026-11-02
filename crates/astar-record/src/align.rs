// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The 20 ms aligner: turns uneven receive chunks and keyed-only transmit
//! frames into one frame per direction per tick.

use std::collections::VecDeque;

/// Frames queued before a direction starts being taken (60 ms).
pub const PRIME_FRAMES: usize = 3;
/// Beyond this many queued frames (200 ms) the oldest are thrown away.
pub const MAX_BACKLOG_FRAMES: usize = 10;

#[derive(Default)]
struct Lane {
    q: VecDeque<i16>,
    primed: bool,
}

impl Lane {
    fn take(&mut self, frame: usize, dropped: &mut u64) -> Vec<i16> {
        let queued = self.q.len() / frame;
        if queued > MAX_BACKLOG_FRAMES {
            let excess = queued - PRIME_FRAMES;
            self.q.drain(..excess * frame);
            *dropped += excess as u64;
        }
        if !self.primed && self.q.len() >= PRIME_FRAMES * frame {
            self.primed = true;
        }
        if self.primed {
            if self.q.len() >= frame {
                return self.q.drain(..frame).collect();
            }
            self.primed = false;
        }
        vec![0; frame]
    }
}

/// Aligns the receive and transmit streams to fixed frames.
pub struct Aligner {
    frame: usize,
    rx: Lane,
    tx: Lane,
    dropped: u64,
}

impl Aligner {
    /// `frame_samples` is the samples per 20 ms frame (320 at 16 kHz).
    #[must_use]
    pub fn new(frame_samples: usize) -> Self {
        assert!(frame_samples > 0, "frame_samples must be non-zero");
        Self {
            frame: frame_samples,
            rx: Lane::default(),
            tx: Lane::default(),
            dropped: 0,
        }
    }

    pub fn push_rx(&mut self, pcm: &[i16]) {
        self.rx.q.extend(pcm);
    }

    pub fn push_tx(&mut self, pcm: &[i16]) {
        self.tx.q.extend(pcm);
    }

    /// One frame per direction, each exactly `frame_samples` long.
    pub fn tick(&mut self) -> (Vec<i16>, Vec<i16>) {
        let rx = self.rx.take(self.frame, &mut self.dropped);
        let tx = self.tx.take(self.frame, &mut self.dropped);
        (rx, tx)
    }

    /// Frames thrown away because a direction ran more than 200 ms behind.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: usize = 320;

    fn frame(v: i16) -> Vec<i16> {
        vec![v; F]
    }

    #[test]
    fn aligner_fills_a_silent_direction_with_zeros() {
        let mut a = Aligner::new(F);
        for _ in 0..5 {
            a.push_rx(&frame(7));
        }
        for _ in 0..5 {
            let (rx, tx) = a.tick();
            assert_eq!(rx, frame(7));
            assert_eq!(tx, frame(0));
        }
    }

    #[test]
    fn aligner_waits_for_three_frames_before_taking() {
        let mut a = Aligner::new(F);
        a.push_rx(&frame(1));
        a.push_rx(&frame(2));
        assert_eq!(a.tick().0, frame(0));
        a.push_rx(&frame(3));
        assert_eq!(a.tick().0, frame(1));
        assert_eq!(a.tick().0, frame(2));
        assert_eq!(a.tick().0, frame(3));
        // dry: silence, and it waits for the cushion again
        assert_eq!(a.tick().0, frame(0));
        a.push_rx(&frame(4));
        a.push_rx(&frame(5));
        assert_eq!(a.tick().0, frame(0));
    }

    #[test]
    fn aligner_reframes_uneven_chunks_in_order() {
        let mut a = Aligner::new(F);
        let ramp: Vec<i16> = (1..=5120i16).collect();
        let mut out = Vec::new();
        for chunk in ramp.chunks(512) {
            a.push_rx(chunk);
            let (rx, _) = a.tick();
            out.extend(rx);
        }
        for _ in 0..12 {
            out.extend(a.tick().0);
        }
        let first = out.iter().position(|&s| s != 0).unwrap();
        assert_eq!(first % F, 0);
        assert_eq!(&out[first..first + ramp.len()], &ramp[..]);
    }

    #[test]
    fn aligner_drops_oldest_beyond_ten_frames() {
        let mut a = Aligner::new(F);
        for i in 1..=15 {
            a.push_rx(&frame(i));
        }
        assert_eq!(a.dropped(), 0);
        assert_eq!(a.tick().0, frame(13));
        assert_eq!(a.dropped(), 12);
    }

    #[test]
    fn minute_three_lines_up_on_both_channels() {
        let mut a = Aligner::new(F);
        for k in 1..=9000i32 {
            let mut f = frame(5);
            f[0] = i16::try_from(k).unwrap();
            a.push_rx(&f);
            a.push_tx(&f);
            let (rx, tx) = a.tick();
            assert_eq!(rx[0], tx[0], "tick {k}");
            if k >= 3 {
                assert_eq!(i32::from(rx[0]), k - 2, "tick {k}");
                assert_ne!(rx[0], 0);
            } else {
                assert_eq!(rx[0], 0);
            }
        }
        assert_eq!(a.dropped(), 0);
    }
}
