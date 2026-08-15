// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A sliding-window peak-hold for VU meters (astar-3f3a).
//!
//! Port of AstarCore's `PeakHold.swift`: reports the maximum value seen within
//! the last `window`. A fast-jittering signal (a level meter polled ~20 Hz)
//! then reads steadily — a transient peak stays visible for `window` before
//! the window slides past it. Time is passed in (no hidden clock) so tests
//! drive it deterministically.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Sliding-window max over `(time, value)` samples.
pub struct PeakHold {
    window: Duration,
    samples: VecDeque<(Instant, f32)>,
}

impl PeakHold {
    /// The meter smoothing window the Mac app uses (~250 ms).
    pub const METER_WINDOW: Duration = Duration::from_millis(250);

    /// A peak-hold reporting the max over the trailing `window`.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
        }
    }

    /// Record `value` at `now`, drop samples older than `window`, and return
    /// the max over the trailing window (the value itself if it's the only
    /// sample).
    pub fn push(&mut self, value: f32, now: Instant) -> f32 {
        self.samples.push_back((now, value));
        while let Some(&(t, _)) = self.samples.front() {
            if now.duration_since(t) > self.window {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        self.samples.iter().map(|&(_, v)| v).fold(value, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn single_sample_returns_itself() {
        let mut p = PeakHold::new(Duration::from_millis(250));
        assert_eq!(p.push(-30.0, t0()), -30.0);
    }

    #[test]
    fn holds_the_max_within_the_window() {
        let start = t0();
        let mut p = PeakHold::new(Duration::from_millis(250));
        p.push(-40.0, start);
        p.push(-10.0, start + Duration::from_millis(50)); // the peak
                                                          // 100 ms later the signal has dropped, but the peak is still inside
                                                          // the 250 ms window — the meter must keep reporting it.
        assert_eq!(p.push(-50.0, start + Duration::from_millis(150)), -10.0);
    }

    #[test]
    fn peak_expires_once_the_window_slides_past() {
        let start = t0();
        let mut p = PeakHold::new(Duration::from_millis(250));
        p.push(-10.0, start); // the peak
                              // 300 ms later the peak is older than the window: it must be gone,
                              // leaving the most recent (quieter) samples.
        assert_eq!(p.push(-50.0, start + Duration::from_millis(300)), -50.0);
    }

    #[test]
    fn rising_signal_tracks_immediately() {
        let start = t0();
        let mut p = PeakHold::new(Duration::from_millis(250));
        p.push(-50.0, start);
        // A louder sample must show instantly (hold delays decay, not attack).
        assert_eq!(p.push(-5.0, start + Duration::from_millis(50)), -5.0);
    }
}
