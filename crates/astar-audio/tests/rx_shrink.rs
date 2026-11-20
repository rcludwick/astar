// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The receive jitter buffer must not shed frames from an SSTV picture.
//!
//! These drive the real [`Mixer`] and its jitter buffer with a fake clock,
//! tag every frame with its own sample value, and list the frames that never
//! came out. The scenario is the one that cost six frames of a received
//! picture on 2026-11-11: a short key-up that arrives late, a 4 s gap, then
//! a long transmission that arrives on time. The late key-up used to inflate
//! the delay history the next over was measured against; ~5 s into the
//! picture the target fell, and the buffer shed the excess one frame per
//! 500 ms.
#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;

use astar_audio::mixer::{Mixer, MixerClock, RxFrame};

/// One device callback: 10 ms at 16 kHz.
const CB: usize = 160;
/// One 20 ms frame at 16 kHz.
const FRAME: usize = 320;

/// Frames whose tag never came out of the mixer, between the first and last
/// frame that did.
fn missing(
    arrive: &dyn Fn(i64) -> i64,
    ts: &dyn Fn(i64) -> i64,
    frames: i64,
    hold: &dyn Fn(i64) -> bool,
) -> Vec<i64> {
    let (clock, cell) = MixerClock::fake();
    let hold_cell = Arc::new(AtomicBool::new(false));
    let mut mixer = Mixer::new()
        .with_clock(clock)
        .with_shrink_hold(Arc::clone(&hold_cell));
    let (tx, rx) = channel();
    mixer.add_call(rx);
    let mut sched: Vec<(i64, i64)> = (0..frames).map(|k| (arrive(k), k)).collect();
    sched.sort_unstable();
    let mut next = 0;
    let mut played: Vec<i64> = Vec::new();
    let mut out = vec![0.0_f32; CB];
    let end = arrive(frames - 1) + 2_000;
    let mut t = 0;
    while t < end {
        while next < sched.len() && sched[next].0 <= t {
            let k = sched[next].1;
            let tag = i16::try_from(k % 30_000 + 1).expect("fits");
            tx.send(RxFrame::timed(vec![tag; FRAME], ts(k), 20))
                .expect("mixer alive");
            next += 1;
        }
        hold_cell.store(hold(t), Ordering::Relaxed);
        cell.store(t, Ordering::Relaxed);
        let n = mixer.read(&mut out);
        for &s in &out[..n] {
            let v = (s * 32_768.0).round() as i64;
            if v != 0 && played.last() != Some(&(v - 1)) {
                played.push(v - 1);
            }
        }
        t += 10;
    }
    let (Some(&first), Some(&last)) = (played.first(), played.last()) else {
        return Vec::new();
    };
    (first..=last).filter(|k| !played.contains(k)).collect()
}

const KEY_UP: i64 = 8;
const GAP: i64 = 200; // frames: 4 s
const FRAMES: i64 = 3_500; // 70 s, the length of a Robot 36 with margin

fn key_up_ts(k: i64) -> i64 {
    if k < KEY_UP {
        100_000 + k * 20
    } else {
        100_000 + (k + GAP) * 20
    }
}

/// The key-up arrives `late` ms late; the over after the gap is on time.
fn key_up_late(late: i64) -> impl Fn(i64) -> i64 {
    move |k| {
        if k < KEY_UP {
            1_000 + k * 20 + 30 + late
        } else {
            1_000 + (k + GAP) * 20 + 30
        }
    }
}

#[test]
fn a_late_key_up_sheds_nothing_from_the_next_over() {
    for late in [60, 120, 160, 200] {
        let lost = missing(&key_up_late(late), &key_up_ts, FRAMES, &|_| false);
        assert!(
            lost.is_empty(),
            "key-up {late} ms late: frames {lost:?} never played"
        );
    }
}

#[test]
fn the_six_drop_scenario_sheds_nothing_with_the_hold_set() {
    let lost = missing(&key_up_late(160), &key_up_ts, FRAMES, &|_| true);
    assert!(lost.is_empty(), "frames {lost:?} never played");
}

/// A sender 1 % fast that never pauses, all one spurt: the reset cannot
/// help (there is no new spurt), so this is the hold on its own. Without
/// it the buffer sheds the surplus, which is right for voice (`65b722b`);
/// with it, every frame plays.
#[test]
fn the_hold_alone_keeps_a_fast_senders_frames() {
    let fast = |k: i64| 1_000 + k * 198 / 10 + 30;
    let ts = |k: i64| 100_000 + k * 20;
    let lost = missing(&fast, &ts, FRAMES, &|_| false);
    assert!(!lost.is_empty(), "a fast sender should be shrunk for voice");
    let lost = missing(&fast, &ts, FRAMES, &|_| true);
    assert!(lost.is_empty(), "held, yet frames {lost:?} never played");
}
