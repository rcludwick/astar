// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A conference member's jitter cushion (iax-confjb).
//!
//! The conference bridge mixes once every 20 ms tick. Without a cushion it
//! read each member's input with nothing held back, so a frame that arrived
//! a few ms after its tick edge missed it: that tick sent the other members
//! silence, the next tick found two frames, and the over came out with a
//! 20 ms stall and then a 40 ms repeat or gap it never had (nodebench,
//! `harness/nodebench/RIG.md`, Speech echo, S3 and S6).
//!
//! [`Dejitter`] holds a member's frames back by a small number of ticks and
//! plays them by the **sender's own timestamp**, one frame per tick:
//!
//! * **Depth.** The first frame of a talk spurt is held for
//!   [`BASE_HOLD_TICKS`] ticks (2: 40 ms) before it plays, and each later
//!   frame plays at the tick its timestamp puts it on. Because the frame was
//!   drained at a tick edge somewhere up to 20 ms after it arrived, the delay
//!   it adds is 40–60 ms, and a frame up to 40 ms later than the spurt's
//!   first one still makes its tick. 40 ms is Asterisk's own
//!   `target_extra`, the slack the receive jitter buffer
//!   ([`crate::IAX_JITTER_CONFIG`]) sits on, so a link through astar-server
//!   costs what a link through a stock node already does.
//! * **A late frame.** If a frame misses its tick anyway, that tick is
//!   silence. When the frame then arrives with nothing queued behind it, the
//!   schedule slips back one tick so it plays, and the hold deepens by one
//!   tick for the rest of the member's life, up to [`MAX_HOLD_TICKS`] (4:
//!   80 ms). Past that, a late frame is dropped, and so is a frame for a
//!   slot that already played (a duplicate or a retransmission).
//! * **A lost frame.** A slot whose frame never came, with later frames
//!   already queued, is one tick of silence in its place, and the frames
//!   after it keep their timing. No waveform is invented: the receive jitter
//!   buffer's ruling (`docs/design/rx-jitter-buffer.md`) holds here too.
//! * **Realign.** Five ticks (100 ms) with nothing queued and nothing played
//!   end the spurt; the next frame anchors a new one with a fresh hold, so
//!   latency never carries from one over to the next. A spurt with no late
//!   frame relaxes the hold by one tick, back towards the base.
//! * **Bounded.** If the queue ever runs more than two ticks deeper than the
//!   hold (the bridge's own clock stalled, or a burst after an outage), the
//!   oldest frames are dropped so the delay stays within the hold.
//!
//! The cushion is per member, so one member's late frames never hold up the
//! others: each tick takes whatever every cushion has due and mixes it.

use std::collections::BTreeMap;

/// Ticks the first frame of a talk spurt is held before it plays (40 ms).
pub(crate) const BASE_HOLD_TICKS: u32 = 2;

/// The deepest the hold grows after late frames (80 ms).
pub(crate) const MAX_HOLD_TICKS: u32 = 4;

/// Consecutive ticks with nothing queued that end a talk spurt (100 ms, the
/// bridge's own transmit hangover).
const END_TICKS: u32 = 5;

/// How far past the hold the queue may run before its oldest frames are
/// dropped, in ticks.
const TRIM_SLACK: i64 = 2;

/// A timestamp this many frames away from where the spurt is playing is a
/// jump in the sender's clock, not jitter (1 s): start the spurt over on it.
const RESYNC_SLOTS: i64 = 50;

/// The longest a cushion can hold audio after its last frame arrived: the
/// deepest hold, the trim slack above it, and the frame being played
/// (`(MAX_HOLD_TICKS + TRIM_SLACK + 1)` ticks of 20 ms, 140 ms). Past this,
/// everything it held has played.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub const MAX_DRAIN: std::time::Duration =
    std::time::Duration::from_millis((MAX_HOLD_TICKS as u64 + TRIM_SLACK as u64 + 1) * 20);

/// Frame duration assumed for a frame that does not carry one.
const DEFAULT_FRAME_MS: i64 = 20;

/// One talk spurt: where the sender's clock was when it began, and which
/// frame of it plays next.
#[derive(Debug)]
struct Spurt {
    /// Sender timestamp of the frame the spurt is anchored on.
    anchor_ts: i64,
    /// Frame duration, ms: the spacing of the slots.
    frame_ms: i64,
    /// Slot (frames since the anchor) that plays at the next tick.
    next: i64,
    /// Ticks still to wait before the first frame plays.
    wait: u32,
    /// Consecutive ticks with nothing queued.
    empty: u32,
    /// Whether a late frame deepened the hold during this spurt.
    slipped: bool,
    /// The last slot that played. A frame for it or anything older is a
    /// duplicate or too late, never a reason to slip.
    last_played: Option<i64>,
}

impl Spurt {
    fn new(anchor_ts: i64, frame_ms: i64, hold: u32) -> Self {
        Self {
            anchor_ts,
            frame_ms,
            next: 0,
            wait: hold,
            empty: 0,
            slipped: false,
            last_played: None,
        }
    }

    /// The slot a timestamp falls in, rounded to the nearest frame.
    fn slot_of(&self, ts_ms: i64) -> i64 {
        let d = ts_ms - self.anchor_ts;
        let half = self.frame_ms / 2;
        if d >= 0 {
            (d + half) / self.frame_ms
        } else {
            -((-d + half) / self.frame_ms)
        }
    }
}

/// A member's jitter cushion: frames go in as they arrive ([`Dejitter::put`])
/// and come out one per bridge tick ([`Dejitter::tick`]). See the module docs.
#[derive(Debug)]
pub(crate) struct Dejitter {
    /// Frames waiting to play, by slot.
    queue: BTreeMap<i64, Vec<i16>>,
    /// The talk spurt in progress, if any.
    spurt: Option<Spurt>,
    /// Current hold, ticks: [`BASE_HOLD_TICKS`]..=[`MAX_HOLD_TICKS`].
    hold: u32,
}

impl Default for Dejitter {
    fn default() -> Self {
        Self {
            queue: BTreeMap::new(),
            spurt: None,
            hold: BASE_HOLD_TICKS,
        }
    }
}

impl Dejitter {
    /// Take one arrival: its PCM, the sender's timestamp and its duration.
    pub(crate) fn put(&mut self, pcm: Vec<i16>, ts_ms: i64, frame_ms: i64) {
        let frame_ms = if frame_ms > 0 {
            frame_ms
        } else {
            DEFAULT_FRAME_MS
        };
        let hold = self.hold;
        let spurt = self
            .spurt
            .get_or_insert_with(|| Spurt::new(ts_ms, frame_ms, hold));
        let mut slot = spurt.slot_of(ts_ms);
        if (slot - spurt.next).abs() > RESYNC_SLOTS {
            // The sender's clock jumped: what is queued belongs to the old
            // timeline. Start the spurt over on this frame.
            self.queue.clear();
            *spurt = Spurt::new(ts_ms, frame_ms, hold);
            slot = 0;
        } else if spurt.last_played.is_some_and(|p| slot <= p) {
            // Already played (a duplicate or a retransmission), or older.
            return;
        } else if slot < spurt.next {
            if spurt.wait > 0 && slot >= spurt.next - i64::from(hold) {
                // Reordered ahead of the spurt's first frame, before anything
                // has played: it still has its place.
                spurt.next = slot;
            } else if slot == spurt.next - 1 && self.queue.is_empty() && hold < MAX_HOLD_TICKS {
                // It missed its tick by one, which played silence; nothing is
                // queued behind it. Slip the schedule back one tick so it
                // plays, and hold one tick deeper from here on.
                spurt.next = slot;
                spurt.slipped = true;
                self.hold += 1;
            } else {
                // Too late to play: its slot is gone.
                return;
            }
        }
        self.queue.entry(slot).or_insert(pcm);
        // Bound the delay: a queue far deeper than the hold means the
        // bridge's clock fell behind or a burst followed an outage.
        if let Some((&last, _)) = self.queue.last_key_value() {
            let hold = i64::from(self.hold);
            if last - spurt.next > hold + TRIM_SLACK {
                let keep_from = last - hold;
                self.queue = self.queue.split_off(&keep_from);
                spurt.next = keep_from;
            }
        }
    }

    /// One bridge tick: the frame due now, or `None` while holding, for a
    /// lost or late frame (a tick of silence), or between spurts.
    pub(crate) fn tick(&mut self) -> Option<Vec<i16>> {
        let spurt = self.spurt.as_mut()?;
        if spurt.wait > 0 {
            spurt.wait -= 1;
            return None;
        }
        if let Some(pcm) = self.queue.remove(&spurt.next) {
            spurt.last_played = Some(spurt.next);
            spurt.next += 1;
            spurt.empty = 0;
            return Some(pcm);
        }
        // This slot's frame is lost or late: a tick of silence, and the
        // schedule moves on.
        spurt.next += 1;
        if self.queue.is_empty() {
            spurt.empty += 1;
            if spurt.empty >= END_TICKS {
                if !spurt.slipped {
                    self.hold = self.hold.saturating_sub(1).max(BASE_HOLD_TICKS);
                }
                self.spurt = None;
            }
        } else {
            spurt.empty = 0;
        }
        None
    }

    /// Whether frames are waiting to play: received, and not yet due.
    pub(crate) fn holds_audio(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The hold the next spurt starts with, ticks (tests).
    #[cfg(test)]
    fn hold(&self) -> u32 {
        self.hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(k: i16) -> Vec<i16> {
        vec![k; 4]
    }

    /// Run `ticks` ticks, returning what each played (its first sample).
    fn run(d: &mut Dejitter, ticks: usize) -> Vec<Option<i16>> {
        (0..ticks).map(|_| d.tick().map(|p| p[0])).collect()
    }

    #[test]
    fn the_first_frame_waits_the_hold_then_frames_play_in_order() {
        let mut d = Dejitter::default();
        d.put(f(1), 1000, 20);
        assert_eq!(run(&mut d, 2), [None, None], "held 40 ms");
        d.put(f(2), 1020, 20);
        d.put(f(3), 1040, 20);
        assert_eq!(run(&mut d, 3), [Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn a_reordered_pair_plays_in_timestamp_order() {
        let mut d = Dejitter::default();
        d.put(f(1), 0, 20);
        d.tick();
        d.put(f(3), 40, 20);
        d.put(f(2), 20, 20);
        d.tick();
        assert_eq!(run(&mut d, 3), [Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn a_frame_reordered_ahead_of_the_first_keeps_its_place() {
        let mut d = Dejitter::default();
        d.put(f(2), 20, 20);
        d.put(f(1), 0, 20);
        assert_eq!(run(&mut d, 4), [None, None, Some(1), Some(2)]);
    }

    #[test]
    fn a_lost_frame_is_one_tick_of_silence_in_its_place() {
        let mut d = Dejitter::default();
        d.put(f(1), 0, 20);
        d.put(f(3), 40, 20);
        d.put(f(4), 60, 20);
        run(&mut d, 2);
        assert_eq!(run(&mut d, 4), [Some(1), None, Some(3), Some(4)]);
    }

    #[test]
    fn a_frame_later_than_the_hold_slips_once_and_the_hold_deepens() {
        let mut d = Dejitter::default();
        d.put(f(1), 0, 20);
        run(&mut d, 2);
        assert_eq!(run(&mut d, 2), [Some(1), None], "frame 2 missed its tick");
        d.put(f(2), 20, 20);
        d.put(f(3), 40, 20);
        assert_eq!(run(&mut d, 2), [Some(2), Some(3)], "slipped one tick");
        assert_eq!(d.hold(), BASE_HOLD_TICKS + 1);
        // A frame two ticks too late is dropped.
        run(&mut d, 2);
        d.put(f(4), 60, 20);
        assert_eq!(run(&mut d, 1), [None]);
    }

    #[test]
    fn a_duplicate_of_a_frame_already_played_is_dropped() {
        // A retransmitted or duplicated frame for the slot just played, with
        // nothing queued: it is not late audio, so it must not slip the
        // schedule, replay 20 ms, or deepen the hold.
        let mut d = Dejitter::default();
        d.put(f(1), 0, 20);
        run(&mut d, 2);
        assert_eq!(run(&mut d, 1), [Some(1)]);
        d.put(f(1), 0, 20);
        assert_eq!(run(&mut d, 1), [None], "not played again");
        assert_eq!(d.hold(), BASE_HOLD_TICKS);
        d.put(f(3), 40, 20);
        d.put(f(1), 0, 20);
        assert_eq!(run(&mut d, 1), [Some(3)], "the schedule kept its place");
    }

    #[test]
    fn the_hold_never_grows_past_its_ceiling() {
        let mut d = Dejitter::default();
        d.put(f(0), 0, 20);
        run(&mut d, 2);
        assert_eq!(run(&mut d, 1), [Some(0)]);
        for k in 1..10_i16 {
            // Every frame misses its tick by one.
            d.tick();
            d.put(f(k), 20 * i64::from(k), 20);
            d.tick();
        }
        assert_eq!(d.hold(), MAX_HOLD_TICKS);
    }

    #[test]
    fn a_new_spurt_realigns_with_a_fresh_hold() {
        let mut d = Dejitter::default();
        for k in 0..5 {
            d.put(f(1), 20 * k, 20);
        }
        let first: Vec<_> = run(&mut d, 20);
        assert_eq!(first.iter().flatten().count(), 5);
        // A second over 3 s later on the sender's clock.
        d.put(f(9), 3000, 20);
        assert_eq!(run(&mut d, 3), [None, None, Some(9)]);
    }

    #[test]
    fn a_clean_spurt_relaxes_a_deepened_hold() {
        let mut d = Dejitter::default();
        d.put(f(1), 0, 20);
        run(&mut d, 4);
        d.put(f(2), 20, 20); // late: slips, hold 3
        run(&mut d, 10);
        assert_eq!(d.hold(), BASE_HOLD_TICKS + 1);
        d.put(f(3), 5000, 20);
        run(&mut d, 10);
        assert_eq!(d.hold(), BASE_HOLD_TICKS);
    }

    #[test]
    fn a_burst_deeper_than_the_hold_is_trimmed() {
        let mut d = Dejitter::default();
        d.put(f(0), 0, 20);
        run(&mut d, 2);
        // The bridge played frame 0, then 10 frames arrive at once.
        assert_eq!(run(&mut d, 1), [Some(0)]);
        for k in 1..=10_i16 {
            d.put(f(k), 20 * i64::from(k), 20);
        }
        let played: Vec<_> = run(&mut d, 6).into_iter().flatten().collect();
        assert_eq!(played, [7, 8, 9, 10], "the oldest frames were dropped");
    }

    #[test]
    fn a_clock_jump_mid_spurt_starts_over() {
        let mut d = Dejitter::default();
        d.put(f(1), 0, 20);
        run(&mut d, 3);
        d.put(f(2), 1_000_000, 20);
        assert_eq!(run(&mut d, 3), [None, None, Some(2)]);
    }
}
