// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What forgetting the delay history at the start of each over costs on a
//! jittery link.
//!
//! The reset (`spurt_history_reset`) starts every over with an empty
//! history, so the buffer has to learn the link's jitter again, and while it
//! learns it can conceal (interpolate) a few more late frames. This pins
//! that cost: three 5 s overs, 4 s apart, each frame arriving a seeded
//! random amount late, played through [`IAX_JITTER_CONFIG`] with the reset
//! on and, as the baseline, off. Overs 2 and 3 are counted, since over 1
//! has no history to forget either way.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use astar_audio::IAX_JITTER_CONFIG;
use astar_codec::jitter::{Frame, FrameType, GetResult, JitterBuf, JitterConfig};

const OVER: i64 = 250; // frames: 5 s
const GAP: i64 = 200; // frames: 4 s
const OVERS: i64 = 3;
const SEEDS: u64 = 20;

/// xorshift64*: a seeded generator, so a failure replays.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: i64) -> i64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        i64::try_from(x >> 33).expect("31 bits") % n
    }
}

#[derive(Clone, Copy, Default, Debug)]
struct Cost {
    concealed: i64,
    dropped: i64,
}

fn run(cfg: JitterConfig, jitter_ms: i64, seed: u64) -> Cost {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let ts = |k: i64| 1_000 + (k + (k / OVER) * GAP) * 20;
    let total = OVER * OVERS;
    let mut sched: Vec<(i64, i64)> = (0..total)
        .map(|k| (ts(k) + 30 + rng.below(jitter_ms + 1), k))
        .collect();
    sched.sort_unstable();
    let over2_starts = ts(OVER);
    let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(cfg);
    let mut next = 0;
    let mut cost = Cost::default();
    let mut dropped_at_start = None;
    let end = sched.iter().map(|s| s.0).max().expect("frames") + 2_000;
    for now in (0..end).step_by(10) {
        while next < sched.len() && sched[next].0 <= now {
            let k = sched[next].1;
            let _ = jb.put(
                Frame {
                    data: vec![0],
                    ts: ts(k),
                    ms: 20,
                    frame_type: FrameType::Voice,
                },
                now,
            );
            next += 1;
        }
        if now >= over2_starts && dropped_at_start.is_none() {
            dropped_at_start = Some(jb.info().frames_dropped);
        }
        loop {
            match jb.next() {
                Some(due) if due <= now => {}
                _ => break,
            }
            match jb.get(now, 20) {
                GetResult::Interpolate => {
                    if now >= over2_starts {
                        cost.concealed += 1;
                    }
                }
                GetResult::Ok(_) | GetResult::Drop(_) => {}
                GetResult::Empty | GetResult::NoFrame => break,
            }
        }
    }
    cost.dropped = jb.info().frames_dropped - dropped_at_start.expect("over 2 began");
    cost
}

/// The extra concealed and dropped frames the reset costs, summed over all
/// the seeds, and the worst single seed.
fn extra(jitter_ms: i64) -> (Cost, Cost, Cost, Cost) {
    let (mut on, mut off) = (Cost::default(), Cost::default());
    let (mut worst_c, mut worst_d) = (0, 0);
    for seed in 1..=SEEDS {
        let a = run(IAX_JITTER_CONFIG, jitter_ms, seed);
        let b = run(
            JitterConfig {
                spurt_history_reset: false,
                ..IAX_JITTER_CONFIG
            },
            jitter_ms,
            seed,
        );
        on.concealed += a.concealed;
        on.dropped += a.dropped;
        off.concealed += b.concealed;
        off.dropped += b.dropped;
        worst_c = worst_c.max(a.concealed - b.concealed);
        worst_d = worst_d.max(a.dropped - b.dropped);
    }
    println!(
        "jitter {jitter_ms} ms over {SEEDS} seeds: reset on {on:?}, off {off:?}, \
         worst seed extra concealed {worst_c}, dropped {worst_d}"
    );
    (
        on,
        off,
        Cost {
            concealed: worst_c,
            dropped: 0,
        },
        Cost {
            concealed: 0,
            dropped: worst_d,
        },
    )
}

/// Measured with these seeds on 2026-11-11, summed over the 20 and worst
/// single seed: 60 ms jitter +34 concealed (worst +5), 100 ms +64 (worst
/// +8), and no extra drops at either. The bounds sit a little above.
#[test]
fn the_over_start_reset_costs_a_few_concealed_frames_on_a_jittery_link() {
    // (jitter ms, total concealed bound, worst-seed concealed bound)
    for (jitter_ms, max_total, max_worst) in [(60, 45, 7), (100, 80, 10)] {
        let (on, off, worst_c, worst_d) = extra(jitter_ms);
        let extra_concealed = on.concealed - off.concealed;
        assert!(
            extra_concealed <= max_total,
            "{jitter_ms} ms: the reset concealed {extra_concealed} more frames in total \
             ({} on vs {} off)",
            on.concealed,
            off.concealed
        );
        assert!(
            worst_c.concealed <= max_worst,
            "{jitter_ms} ms: one seed concealed {} more frames",
            worst_c.concealed
        );
        assert!(
            on.dropped - off.dropped <= 2 && worst_d.dropped <= 1,
            "{jitter_ms} ms: the reset dropped {} more frames ({} on vs {} off)",
            on.dropped - off.dropped,
            on.dropped,
            off.dropped
        );
    }
}
