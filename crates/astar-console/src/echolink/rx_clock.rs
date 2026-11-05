// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The sender clock an `EchoLink` receive lane plays out against.
//!
//! The station mixer plays a lane out of its adaptive jitter buffer
//! (`iax-rxjb`, the same one an `AllStarLink` call uses) only when the lane's
//! frames carry the sender's clock. Without one a lane is a plain queue:
//! every packet that arrives late adds its lateness to the delay for good,
//! and a sender whose clock runs fast adds a little more every minute. On a
//! busy conference that never pauses, the queue never empties, so the delay
//! only grows (`astar-el-hollow`).
//!
//! An `EchoLink` audio packet is RTP-shaped, but the only field a station can
//! be relied on to fill is the sequence number: astar itself, like other
//! clients, sends the timestamp as zero. The sequence number is enough. Each
//! packet carries exactly four 20 ms GSM frames, so packet `n` of an over
//! starts `80 × n` ms after the first one on the sender's clock, and a gap in
//! the numbers is a gap in the audio.
//!
//! Between overs the sender's numbering means nothing in time (the far end
//! may pause, or restart its numbers), so each over is anchored afresh: its
//! first packet sits at the time it arrived, never earlier than where the
//! last over ended. The jitter buffer sees that as a talk spurt starting,
//! which is what it is.

use std::time::Instant;

use astar_audio::RxFrame;
use astar_echolink::voice::FRAMES_PER_PACKET;

/// One GSM frame, in ms.
const FRAME_MS: i64 = 20;
/// One packet, in ms.
#[allow(clippy::cast_possible_wrap)]
const PACKET_MS: i64 = FRAME_MS * FRAMES_PER_PACKET as i64;
/// A jump in the sequence numbers this big (2 s of audio) inside one over is
/// not loss but a sender that renumbered; anchor afresh rather than schedule
/// the audio seconds into the future.
const MAX_SEQ_STEP: u16 = 25;

/// Where the current over sits on the sender timeline.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    /// The over's first sequence number, and its packets counted on past
    /// the 16-bit wrap.
    last_seq: u16,
    packets: i64,
    /// Sender time of the over's first packet, ms since `epoch`.
    ts0: i64,
}

/// Gives each received frame a place on a sender timeline, so the mixer can
/// play the lane out of its jitter buffer.
#[derive(Debug)]
pub(crate) struct RxTimeline {
    epoch: Instant,
    over: Option<Anchor>,
    /// Sender time just past the last frame stamped, ms since `epoch`.
    end_ts: i64,
}

impl RxTimeline {
    pub(crate) fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            over: None,
            end_ts: 0,
        }
    }

    /// The over has ended; the next packet starts a new one.
    pub(crate) fn end_over(&mut self) {
        self.over = None;
    }

    /// Stamp one packet's decoded frames, which arrived at `now`. The caller
    /// has already dropped a packet that is not newer than the last one.
    pub(crate) fn stamp(&mut self, seq: u16, now: Instant, frames: Vec<Vec<i16>>) -> Vec<RxFrame> {
        let now_ms = i64::try_from(now.saturating_duration_since(self.epoch).as_millis())
            .unwrap_or(i64::MAX);
        let ts = match self.over.as_mut() {
            Some(a) if seq.wrapping_sub(a.last_seq) <= MAX_SEQ_STEP => {
                a.packets += i64::from(seq.wrapping_sub(a.last_seq));
                a.last_seq = seq;
                a.ts0 + a.packets * PACKET_MS
            }
            _ => {
                let ts0 = now_ms.max(self.end_ts);
                self.over = Some(Anchor {
                    last_seq: seq,
                    packets: 0,
                    ts0,
                });
                ts0
            }
        };
        let mut at = ts;
        let stamped = frames
            .into_iter()
            .map(|pcm| {
                let f = RxFrame::timed(pcm, at, FRAME_MS);
                at += FRAME_MS;
                f
            })
            .collect();
        self.end_ts = self.end_ts.max(at);
        stamped
    }
}

#[cfg(test)]
mod tests {
    //! `astar-el-hollow`: a busy conference never pauses long enough for the
    //! receive lane to empty, so whatever cushion the lane builds up it keeps.
    //! These drive [`RxTimeline`] and a real [`Mixer`] on a made-up clock
    //! through ten minutes of continuous traffic, so ten minutes take a
    //! fraction of a second.
    use super::*;
    use astar_audio::{Mixer, MixerClock, RxJitterConfig, RxJitterSettings};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// 20 ms at 8 kHz.
    const FRAME: usize = 160;
    /// Device callback: 10 ms.
    const CALLBACK_MS: i64 = 10;
    const PACKET_MS: f64 = 80.0;

    /// A tiny deterministic PRNG, so a failure reproduces.
    struct Lcg(u64);
    impl Lcg {
        fn next_f64(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            #[allow(clippy::cast_precision_loss)]
            let v = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
            v
        }
    }

    /// What one run measured.
    #[derive(Debug)]
    struct Run {
        /// Median playout latency (send to speaker) per minute, ms.
        minute_median_ms: Vec<i64>,
        /// Worst latency in the last minute, ms.
        last_minute_max_ms: i64,
        /// Frames never played (dropped late or to shrink), out of all sent.
        frames_missing: usize,
        frames_sent: usize,
        /// Frame ids in the order the speaker played them, one entry per
        /// sample run, for the order and repeat checks.
        played: Vec<(i16, usize)>,
    }

    /// Ten minutes of back-to-back 80 ms packets from a sender whose clock
    /// runs `ppm` fast (negative: slow), over a network that adds 30 to 70 ms
    /// and, now and then, holds everything back for 250 ms.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn simulate(ppm: f64, seed: u64, jb: RxJitterConfig) -> Run {
        const MINUTES: usize = 10;
        let packets = (MINUTES as f64 * 60_000.0 / PACKET_MS) as usize;
        let mut rng = Lcg(seed);
        // Sender time of packet i in our (the speaker's) ms.
        let period = PACKET_MS / (1.0 + ppm * 1e-6);
        let mut arrivals: Vec<(f64, usize)> = Vec::with_capacity(packets);
        let mut spike_until = f64::NEG_INFINITY;
        for i in 0..packets {
            let sent = i as f64 * period;
            if sent > spike_until && rng.next_f64() < 1.0 / 400.0 {
                spike_until = sent + 250.0;
            }
            // And once, three minutes in, a 600 ms outage (a Wi-Fi hiccup).
            if (180_000.0..180_000.0 + period).contains(&sent) {
                spike_until = sent + 600.0;
            }
            let mut at = sent + 30.0 + rng.next_f64() * 40.0;
            if sent <= spike_until {
                at = at.max(spike_until + 30.0);
            }
            arrivals.push((at, i));
        }
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));

        let (clock, cell) = MixerClock::fake();
        let mut mixer = Mixer::new()
            .with_clock(clock)
            .with_settings(Arc::new(RxJitterSettings::new(jb)));
        let (tx, rx) = channel();
        mixer.add_call(rx);
        let epoch = Instant::now();
        let mut timeline = RxTimeline::new(epoch);

        let mut next = 0usize;
        let end_ms = (packets as f64 * period + 2_000.0) as i64;
        let mut lat_by_minute: Vec<Vec<i64>> = vec![Vec::new(); MINUTES + 1];
        let mut played: Vec<(i16, usize)> = Vec::new();
        let mut out = vec![0f32; FRAME / 2];
        let mut now = 0i64;
        while now < end_ms {
            while next < arrivals.len() && arrivals[next].0 <= now as f64 {
                let i = arrivals[next].1;
                let frames: Vec<Vec<i16>> = (0..4)
                    .map(|j| vec![(i * 4 + j + 1) as i16; FRAME])
                    .collect();
                let at = epoch + Duration::from_secs_f64(arrivals[next].0 / 1_000.0);
                for f in timeline.stamp(i as u16, at, frames) {
                    tx.send(f).unwrap();
                }
                next += 1;
            }
            cell.store(now, Ordering::Relaxed);
            let n = mixer.read(&mut out);
            if let Some(&s) = out[..n].iter().find(|s| **s != 0.0) {
                let id = (s * 32_768.0).round() as i16;
                let k = (id - 1) as usize;
                let sent = (k / 4) as f64 * period + (k % 4) as f64 * 20.0;
                let lat = now - sent as i64;
                lat_by_minute[(now / 60_000) as usize].push(lat);
            }
            for &s in &out[..n] {
                let id = (s * 32_768.0).round() as i16;
                if id == 0 {
                    continue;
                }
                match played.last_mut() {
                    Some((last, count)) if *last == id => *count += 1,
                    _ => played.push((id, 1)),
                }
            }
            now += CALLBACK_MS;
        }
        let median = |v: &mut Vec<i64>| {
            v.sort_unstable();
            v.get(v.len() / 2).copied().unwrap_or(0)
        };
        let last_minute_max_ms = lat_by_minute[MINUTES - 1]
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        let minute_median_ms = lat_by_minute[..MINUTES].iter_mut().map(median).collect();
        let mut distinct: Vec<i16> = played.iter().map(|p| p.0).collect();
        distinct.dedup();
        Run {
            minute_median_ms,
            last_minute_max_ms,
            frames_missing: packets * 4 - distinct.len(),
            frames_sent: packets * 4,
            played,
        }
    }

    /// The most a frame may wait between leaving the sender and reaching
    /// the speaker: the network's worst ordinary delay (70 ms), the jitter
    /// buffer's ceiling, and 40 ms for the packet's own framing and the
    /// device callback.
    fn bound_ms(jb: RxJitterConfig) -> i64 {
        70 + i64::from(jb.max_ms) + 40
    }

    fn check(ppm: f64, seed: u64, jb: RxJitterConfig) -> Run {
        let run = simulate(ppm, seed, jb);
        eprintln!(
            "ppm {ppm:+}, max {} ms: per-minute median latency {:?} ms, last-minute max {} ms, \
             {} of {} frames not played",
            jb.max_ms,
            run.minute_median_ms,
            run.last_minute_max_ms,
            run.frames_missing,
            run.frames_sent
        );
        // Never reordered: frame ids only ever go up.
        for w in run.played.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "frame {} played after frame {}",
                w[1].0,
                w[0].0
            );
        }
        // Never repeated: no frame plays more than its own 160 samples.
        for &(id, count) in &run.played {
            assert!(count <= FRAME, "frame {id} played {count} samples");
        }
        // Bounded: every minute, and the very end, inside what the network
        // and the buffer's ceiling account for, spikes and the outage and
        // ten minutes of drift notwithstanding.
        let bound = bound_ms(jb);
        for (m, &median) in run.minute_median_ms.iter().enumerate() {
            assert!(
                median <= bound,
                "ppm {ppm:+}: minute {m}'s median latency was {median} ms, over {bound} ms"
            );
        }
        assert!(
            run.last_minute_max_ms <= bound + 40,
            "ppm {ppm:+}: the last minute peaked at {} ms",
            run.last_minute_max_ms
        );
        // Few frames lost to the spikes, the outage and the drift: under 2%.
        assert!(
            run.frames_missing * 50 < run.frames_sent,
            "ppm {ppm:+}: {} of {} frames never played",
            run.frames_missing,
            run.frames_sent
        );
        run
    }

    #[test]
    fn ten_busy_minutes_from_a_fast_sender_stay_within_bounds() {
        check(200.0, 1, RxJitterConfig::default());
    }

    #[test]
    fn ten_busy_minutes_from_a_slow_sender_stay_within_bounds() {
        check(-200.0, 2, RxJitterConfig::default());
    }

    #[test]
    fn ten_busy_minutes_from_an_exact_sender_stay_within_bounds() {
        check(0.0, 3, RxJitterConfig::default());
    }

    /// The Quick settings Max applies to `EchoLink` as it does to
    /// `AllStarLink`: a lower ceiling holds less audio back.
    #[test]
    fn a_lower_max_holds_the_lane_closer() {
        let tight = RxJitterConfig {
            enabled: true,
            min_ms: 20,
            max_ms: 100,
        };
        let loose = check(200.0, 1, RxJitterConfig::default());
        let close = check(200.0, 1, tight);
        let last = |r: &Run| *r.minute_median_ms.last().unwrap();
        assert!(
            last(&close) + 30 <= last(&loose),
            "max 100 ms ended at {} ms, max 200 ms at {} ms",
            last(&close),
            last(&loose)
        );
    }
}
