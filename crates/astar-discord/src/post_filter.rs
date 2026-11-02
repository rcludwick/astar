// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The unamblify seam.
//!
//! `~/dev/unamblify` specifies a real-time AMBE post-filter as "a `Send`
//! object with `push_frame` / `pull_frame`" fed by ONNX graphs, in `ll5`
//! (100 ms lookahead) and `ll20` (400 ms) variants. As of 2026-09-17 that
//! project's `docs/design/realtime.md` reads "planned. Nothing built.", so
//! [`Bypass`] is the only implementation here.
//!
//! Running an unproven post-filter in this crate rather than in astar's
//! shared RX path is the point: it cannot degrade what goes out on the air,
//! because only Discord listeners hear it.

/// A receive-side post-filter over 20 ms mono i16 frames.
pub trait PostFilter: Send {
    /// Feed one frame at the input rate.
    fn push_frame(&mut self, pcm: &[i16]);

    /// Take one output frame; `None` until enough lookahead has accumulated.
    fn pull_frame(&mut self) -> Option<Vec<i16>>;

    /// Output rate, which may exceed the input rate: unamblify's `full`
    /// profile emits 16 kHz bandwidth-extended audio from 8 kHz input, and
    /// the resampler downstream has to know.
    fn output_rate(&self) -> u32;
}

/// Pass-through: sample-exact, no delay, no rate change. What ships today.
pub struct Bypass {
    rate: u32,
    pending: Option<Vec<i16>>,
}

impl Bypass {
    #[must_use]
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            pending: None,
        }
    }
}

impl PostFilter for Bypass {
    fn push_frame(&mut self, pcm: &[i16]) {
        self.pending = Some(pcm.to_vec());
    }

    fn pull_frame(&mut self) -> Option<Vec<i16>> {
        self.pending.take()
    }

    fn output_rate(&self) -> u32 {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_is_sample_exact_and_adds_no_delay() {
        let mut f = Bypass::new(8000);
        assert_eq!(f.output_rate(), 8000);
        assert!(f.pull_frame().is_none(), "nothing pushed yet");
        f.push_frame(&[1i16, -2, 3]);
        assert_eq!(f.pull_frame(), Some(vec![1i16, -2, 3]));
        assert!(f.pull_frame().is_none(), "one frame in, one frame out");
    }
}
