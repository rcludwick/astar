// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Fixed scenes for the golden tests, the perf test and `frame_preview`.
//!
//! Every input is made up here (synthetic callsigns, synthesised audio), so
//! the same build always draws the same frames. Not part of the crate's
//! API.

use astar_netarchive::{Ended, SessionMeta};

use crate::scene::{FrameState, Mode, SceneBuilder, SessionInfo};

/// The session the scenes share: "Tuesday hub net" on M17, starting at
/// 19:00:00 at UTC−7, with N0CALL as the recorder's own call.
#[must_use]
pub fn meta(net: &str, target: &str) -> SessionMeta {
    SessionMeta {
        format: 1,
        net: net.into(),
        network: "m17".into(),
        target: target.into(),
        window_start: "2026-11-03T18:55:00-07:00".into(),
        window_end: "2026-11-03T20:30:00-07:00".into(),
        visibility: "public".into(),
        my_side: true,
        sample_rate: 8_000,
        started_at: "2026-11-04T02:00:00Z".into(),
        ended_at: None,
        ended: None::<Ended>,
        rejoins: 0,
        dropped_frames: 0,
        overs: 0,
        astar_version: "0.0.0".into(),
        my_callsigns: vec!["N0CALL".into()],
    }
}

/// A repeatable value in 0..1 for knot `k` of over `seed`.
fn noise(seed: u32, k: u64) -> f64 {
    let mut h = k
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(seed).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 29;
    #[allow(clippy::cast_precision_loss)] // 24 bits
    let v = (h >> 40) as f64 / f64::from(1u32 << 24);
    v
}

/// One sample of a made-up voice, `i` samples into over `seed`: a few
/// harmonics under a loudness that wanders every 70 ms, with a breath
/// every few seconds.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn voice(seed: u32, i: u64) -> i16 {
    #[allow(clippy::cast_precision_loss)] // sample indices stay far below 2^52
    let t = i as f64 / 8_000.0;
    let knot = t / 0.07;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // t ≥ 0
    let k = knot.floor() as u64;
    let x = knot.fract();
    let x = x * x * (3.0 - 2.0 * x);
    let level = noise(seed, k) * (1.0 - x) + noise(seed, k + 1) * x;
    let breath = if (t + f64::from(seed)) % 4.3 > 3.95 {
        0.03
    } else {
        1.0
    };
    let env = level.powf(1.6) * breath;
    let f0 = 140.0 + 25.0 * f64::from(seed).sin();
    let tau = std::f64::consts::TAU;
    let tone = (tau * f0 * t).sin() * 0.6
        + (tau * f0 * 2.0 * t).sin() * 0.25
        + (tau * f0 * 3.0 * t).sin() * 0.15;
    #[allow(clippy::cast_possible_truncation)] // |value| < 30 000
    let v = (tone * env * 30_000.0) as i16;
    v
}

/// One over: `(seq, talker, start_ms, duration_ms)`; `None` duration means
/// it is still open.
pub type OverSpec<'a> = (u32, &'a str, u64, Option<u64>);

/// Feed the overs and their audio (in 20 ms blocks) up to `until_ms`.
pub fn feed(b: &mut SceneBuilder, overs: &[OverSpec<'_>], until_ms: u64) {
    for &(seq, talker, start, dur) in overs {
        if start > until_ms {
            break;
        }
        b.over_start(seq, Some(talker), start);
        let end = dur.map_or(until_ms, |d| (start + d).min(until_ms));
        let mut t = start;
        while t < end {
            let block: Vec<i16> = (0..160).map(|k| voice(seq, (t - start) * 8 + k)).collect();
            b.audio(t, &block);
            t += 20;
        }
        if let Some(d) = dur.filter(|d| start + d <= until_ms) {
            b.over_end(seq, d);
        }
    }
}

/// The mockup's conversation: NC0NTL at 19:00:12 for 1:42, N0CALL (mine)
/// at 19:02:01 for 0:18, then KX0AAA from 19:02:24.
pub const MOCKUP_OVERS: [OverSpec<'static>; 3] = [
    (1, "NC0NTL", 12_000, Some(102_000)),
    (2, "N0CALL", 121_000, Some(18_000)),
    (3, "KX0AAA", 144_000, None),
];

/// Frame A as the mockup draws it, live: KX0AAA 14 s into an over.
#[must_use]
pub fn live_talking(capacity: usize) -> FrameState {
    let mut b = builder(&meta("Tuesday hub net", "M17-XXX C"), Mode::Live, capacity);
    feed(&mut b, &MOCKUP_OVERS, 158_000);
    b.frame_at(158_000)
}

/// The recorded video between overs: KX0AAA spoke last and is dimmed; the
/// clock shows, and the ring is progress through the net.
#[must_use]
pub fn recorded_gap(capacity: usize) -> FrameState {
    let mut b = builder(
        &meta("Tuesday hub net", "M17-XXX C"),
        Mode::Recorded { total_ms: 600_000 },
        capacity,
    );
    let overs = [
        MOCKUP_OVERS[0],
        MOCKUP_OVERS[1],
        (3, "KX0AAA  D", 144_000, Some(31_000)),
    ];
    feed(&mut b, &overs, 182_000);
    b.frame_at(182_000)
}

/// Forty-character and emoji callsigns on both sides, a net name and target
/// far too long for the frame, and a long talker.
#[must_use]
pub fn hostile_names(capacity: usize) -> FrameState {
    const LONG: &str = "KX0AAA/KX0BBB/KX0CCC/KX0DDD/KX0EEE/KX0FF";
    const EMOJI: &str = "🙂📻🔥KX0AAA🙂📻🔥🙂📻";
    let mut m = meta(
        "The extraordinarily long-winded Tuesday evening hub net and swap meet of the north",
        "M17-XXX C via a very long reflector description that never ends",
    );
    m.my_callsigns = vec![LONG.into()];
    let mut b = builder(&m, Mode::Live, capacity);
    let overs = [
        (1, EMOJI, 10_000, Some(20_000)),
        (2, LONG, 32_000, Some(9_000)),
        (3, "N0CALL", 43_000, Some(6_000)),
        (4, "WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW", 51_000, None),
    ];
    feed(&mut b, &overs, 60_000);
    b.frame_at(60_000)
}

fn builder(m: &SessionMeta, mode: Mode, capacity: usize) -> SceneBuilder {
    let info = SessionInfo::from_meta(m, &[]).expect("the demo session.json is valid");
    let mut b = SceneBuilder::new(info, mode);
    b.set_capacity(capacity);
    b
}
