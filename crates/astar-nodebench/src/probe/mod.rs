// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Bench-side peers that stand in for radios and nodes.
//!
//! * [`usrp`] — the ASL3 node's "radio": a USRP endpoint speaking `app_rpt`'s
//!   `chan_usrp` wire format. Injects PTT + audio, records what the node keys.
//! * [`iax`] — a third IAX2 peer that links into astar-server's listener and
//!   sends/records audio on its bridge.
//!
//! Everything here binds and talks on loopback only; no radio exists.

pub mod iax;
pub mod usrp;

use std::time::{Duration, Instant};

/// One 20 ms voice frame as the bench received it.
#[derive(Debug, Clone, PartialEq)]
pub struct RxFrame {
    /// When the bench thread took the frame off the socket / channel.
    pub at: Instant,
    /// Whether the sender marked itself keyed for this frame.
    pub keyed: bool,
    /// Sender sequence number (USRP `seq`), when the transport carries one.
    pub seq: Option<u32>,
    /// Sender media timestamp in ms (IAX2), when the transport carries one.
    pub ts_ms: Option<i64>,
    /// PCM samples (empty for a USRP unkey packet).
    pub samples: Vec<i16>,
}

/// What a paced transmit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxReport {
    /// When the first frame left.
    pub started: Instant,
    /// When the last frame (and any unkey) left.
    pub finished: Instant,
    /// Voice frames sent.
    pub frames: usize,
}

/// Concatenate the audio of a recording, in arrival order.
#[must_use]
pub fn concat(frames: &[RxFrame]) -> Vec<i16> {
    frames
        .iter()
        .flat_map(|f| f.samples.iter().copied())
        .collect()
}

/// The first frame carrying audio, as an offset from `since`.
#[must_use]
pub fn first_audio_after(frames: &[RxFrame], since: Instant) -> Option<Duration> {
    frames
        .iter()
        .find(|f| !f.samples.is_empty())
        .map(|f| f.at.saturating_duration_since(since))
}

/// Sleep until `deadline` (no-op if it already passed). The pacing primitive:
/// frames are scheduled on an absolute timeline so sleep overshoot never
/// accumulates into drift.
pub(crate) fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        std::thread::sleep(deadline - now);
    }
}

/// Split `audio` into `frame_len`-sample frames, zero-padding the last one.
pub(crate) fn frames_of(audio: &[i16], frame_len: usize) -> Vec<Vec<i16>> {
    audio
        .chunks(frame_len)
        .map(|c| {
            let mut f = c.to_vec();
            f.resize(frame_len, 0);
            f
        })
        .collect()
}
