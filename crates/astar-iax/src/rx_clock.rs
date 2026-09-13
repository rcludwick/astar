// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The receive side's media clock: putting the sender's timestamp back
//! together, and handing the decoded frame to the output bus with it attached
//! (iax-rxjb).
//!
//! The mixer lane's jitter buffer schedules playout against the SENDER's
//! clock, so a decoded frame is worth nothing to it without one. Two shapes of
//! timestamp arrive on the wire (RFC 5456 §6.4):
//!
//! * a full voice frame carries the whole 32-bit millisecond timestamp;
//! * a mini frame carries only its low 16 bits, and wraps every 65.536 s.
//!
//! The session FSM reports both as a plain `u32`, so the receiver is the one
//! that has to put the high half back — otherwise every 65 seconds of a call
//! the buffer would see the clock fall off a cliff and resynchronise, which is
//! audible.
//!
//! # Sixteen bits are not enough on their own
//!
//! The obvious reconstruction — pick the epoch whose candidate lands nearest
//! the last timestamp seen — is wrong on a repeater, and wrong in exactly the
//! place it hurts. Between overs a node goes quiet for as long as nobody is
//! talking, while its media clock keeps running; come back after 40 seconds and
//! the correct candidate is 40 s AHEAD, which "nearest the last timestamp"
//! reads as 25 s BEHIND and corrects by subtracting a whole wrap. The first
//! frames of the new transmission then land 65 s in the past, the jitter buffer
//! calls that a delay discontinuity, and the first ~80 ms of somebody's over is
//! dropped — the very symptom this module exists to prevent.
//!
//! So the anchor is not "the last timestamp" but **where the sender's clock
//! should be by now**: its last known reading plus however long the RECEIVER
//! has since waited. The two clocks run at the same rate, so that estimate is
//! good to within jitter and drift — orders of magnitude inside the ±32.768 s
//! a 16-bit field can express — and it stays right across a gap of any length.

use std::sync::mpsc;
use std::time::Instant;

use astar_audio::RxFrame;
use astar_iax_core::VoiceFormat;

/// Rebuilds a monotonic 32-bit-wide millisecond clock from the wire's mix of
/// full (32-bit) and mini (16-bit) timestamps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RxTimestamps {
    /// Receiver-side origin the arrival times are measured from.
    origin: Instant,
    /// Highest timestamp extended so far. It never walks backwards, so one
    /// reordered frame can't drag the epoch with it.
    last_ts: i64,
    /// Receiver time of the most recent arrival, ms since `origin`; `None`
    /// before the first frame.
    last_rx_ms: Option<i64>,
}

/// Half of a 16-bit wrap: the distance past which "ahead of where the sender's
/// clock should be" is better read as "behind it, one epoch back".
const HALF_WRAP: i64 = 0x8000;
/// One 16-bit wrap.
const WRAP: i64 = 0x1_0000;

impl Default for RxTimestamps {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            last_ts: 0,
            last_rx_ms: None,
        }
    }
}

impl RxTimestamps {
    /// Extend one wire timestamp to the full millisecond clock. `now` is when
    /// the frame arrived.
    pub(crate) fn extend(&mut self, raw: u32, now: Instant) -> i64 {
        let rx_ms = i64::try_from(now.saturating_duration_since(self.origin).as_millis())
            .unwrap_or(i64::MAX);
        let raw = i64::from(raw);
        let ts = if raw > 0xFFFF {
            // Only a full frame can carry this; take it as it stands.
            raw
        } else if let Some(last_rx_ms) = self.last_rx_ms {
            // Either a mini frame, or a full frame in the call's first 65 s —
            // and in that second case the estimate below has a zero high half
            // too, so the same arithmetic gives the same answer.
            let expected = self.last_ts + (rx_ms - last_rx_ms).max(0);
            let mut candidate = (expected & !0xFFFF) | raw;
            if candidate + HALF_WRAP < expected {
                candidate += WRAP;
            } else if candidate > expected + HALF_WRAP && candidate >= WRAP {
                candidate -= WRAP;
            }
            candidate
        } else {
            // The first frame we have ever seen on this leg. There is nothing
            // to extend against and nothing to lose by not extending: the
            // mixer lane normalises on its own first frame, so only the
            // DIFFERENCES between timestamps ever matter downstream.
            raw
        };
        self.last_ts = self.last_ts.max(ts);
        self.last_rx_ms = Some(rx_ms);
        ts
    }
}

/// Duration in ms of `samples` at `rate`.
fn frame_ms(samples: usize, rate: u32) -> i64 {
    if rate == 0 {
        return 0;
    }
    let samples = i64::try_from(samples).unwrap_or(0);
    samples * 1000 / i64::from(rate)
}

/// Decode one received voice frame and hand it to the output bus stamped with
/// the sender's clock, so the bus lane's jitter buffer can schedule it. `now`
/// is when the frame arrived.
///
/// Undecodable frames are dropped, with one warning per call — the RX codec
/// edge's existing contract (iax-31f7 / iax-4348).
#[allow(clippy::too_many_arguments)]
pub(crate) fn forward_voice(
    edge: &mut crate::codec_edge::EdgeAudio,
    clock: &mut RxTimestamps,
    spk_tx: &mpsc::Sender<RxFrame>,
    format: VoiceFormat,
    payload: &[u8],
    ts: u32,
    now: Instant,
    decode_warned: &mut bool,
) {
    match edge.decode(format, payload) {
        Some(pcm) => {
            let ts_ms = clock.extend(ts, now);
            let ms = frame_ms(pcm.len(), edge.bus_rate());
            let _ = spk_tx.send(RxFrame::timed(pcm, ts_ms, ms));
        }
        None if !*decode_warned => {
            *decode_warned = true;
            tracing::warn!(
                ?format,
                len = payload.len(),
                "dropping undecodable RX voice"
            );
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A receiver clock a test drives by hand: `at(ms)` is `ms` after the
    /// timestamp extender's own origin.
    struct Rx {
        clock: RxTimestamps,
        origin: Instant,
    }

    impl Rx {
        fn new() -> Self {
            let clock = RxTimestamps::default();
            let origin = clock.origin;
            Self { clock, origin }
        }

        /// A frame stamped `raw` arriving `at_ms` into the call.
        fn at(&mut self, raw: u32, at_ms: i64) -> i64 {
            let at_ms = u64::try_from(at_ms).expect("test times are positive");
            self.clock
                .extend(raw, self.origin + Duration::from_millis(at_ms))
        }
    }

    /// The low 16 bits of a sender timestamp — what a mini frame carries.
    fn mini(sender_ts: i64) -> u32 {
        u32::try_from(sender_ts & 0xFFFF).expect("16 bits fit in a u32")
    }

    #[test]
    fn full_frame_timestamps_pass_through() {
        let mut rx = Rx::new();
        assert_eq!(rx.at(0, 0), 0);
        assert_eq!(rx.at(20, 20), 20);
        assert_eq!(rx.at(1_000_000, 40), 1_000_000);
    }

    /// A mini frame carries only the low 16 bits. Past the first wrap the
    /// receiver has to supply the high half, or the clock appears to jump back
    /// 65 seconds every 65 seconds.
    #[test]
    fn mini_frame_timestamps_are_extended_across_the_wrap() {
        let mut rx = Rx::new();
        // A steady 20 ms stream straight through the wrap, sender and receiver
        // on the same cadence.
        let mut last = 0;
        for step in 0..40_i64 {
            let sender_ts = 65_440 + step * 20;
            last = rx.at(mini(sender_ts), 65_440 + step * 20);
            assert_eq!(
                last,
                sender_ts,
                "step {step} (raw {}) must extend to the sender's clock",
                mini(sender_ts)
            );
        }
        assert!(last > 66_000, "and we are past the wrap: {last}");
    }

    /// One reordered frame must not drag the epoch backwards for the frames
    /// that follow it.
    #[test]
    fn a_reordered_frame_does_not_move_the_epoch() {
        let mut rx = Rx::new();
        assert_eq!(rx.at(65_535, 65_535), 65_535);
        assert_eq!(rx.at(20, 65_555), 65_556, "first past the wrap");
        assert_eq!(
            rx.at(65_515, 65_575),
            65_515,
            "the straggler, still in epoch 0"
        );
        assert_eq!(rx.at(40, 65_595), 65_576, "and the next one carries on");
    }

    /// A node is quiet between overs while its media clock keeps running. A
    /// gap of ANY length must come back on the right epoch: the sender's clock
    /// has moved forward by the gap, so that is where we look for it.
    ///
    /// 20 s is inside half a wrap, 40 s and 70 s straddle it (the case that
    /// used to subtract a whole wrap and drop the first frames of the new
    /// transmission), 200 s is three wraps out.
    #[test]
    fn a_silence_gap_between_overs_comes_back_on_the_right_epoch() {
        for gap_ms in [20_000_i64, 40_000, 70_000, 200_000] {
            let mut rx = Rx::new();
            // A first over: 1 s of 20 ms frames from a clock already 30 s in.
            let base = 30_000_i64;
            for step in 0..50_i64 {
                let ts = base + step * 20;
                assert_eq!(rx.at(mini(ts), ts), ts, "gap {gap_ms}: first over");
            }
            // Silence, then a second over. The sender's clock ran the whole time.
            let resume = base + 50 * 20 + gap_ms;
            for step in 0..50_i64 {
                let ts = resume + step * 20;
                assert_eq!(
                    rx.at(mini(ts), ts),
                    ts,
                    "gap {gap_ms}: frame {step} of the over after the gap"
                );
            }
        }
    }

    /// The frames either side of a wrap arrive out of order — the usual
    /// network reordering, at the one moment the arithmetic is delicate.
    #[test]
    fn frames_reordered_across_the_wrap_land_on_the_right_epochs() {
        let mut rx = Rx::new();
        // Settle on a steady stream up to just before the wrap.
        for step in 0..10_i64 {
            let ts = 65_300 + step * 20;
            assert_eq!(rx.at(mini(ts), ts), ts);
        }
        // 65_500 is due next but 65_540 (past the wrap, raw 4) overtakes it.
        assert_eq!(rx.at(mini(65_540), 65_500), 65_540, "the early one");
        assert_eq!(
            rx.at(mini(65_500), 65_520),
            65_500,
            "the late one, in epoch 0"
        );
        assert_eq!(rx.at(mini(65_560), 65_540), 65_560, "and back in order");
    }

    /// Joining a stream whose first frame is a mini: there is nothing to
    /// extend against, so it is taken at face value — and the stream still
    /// carries on correctly through the wrap, because only the differences
    /// matter downstream.
    #[test]
    fn a_call_whose_first_frame_is_a_mini_still_tracks_the_wrap() {
        let mut rx = Rx::new();
        assert_eq!(rx.at(12_345, 0), 12_345, "taken at face value");
        // From here the sender's clock reads 12_345 + elapsed; run it past the
        // wrap and check every step lands one frame after the last.
        let mut previous = 12_345_i64;
        for step in 1..3_000_i64 {
            let ts = 12_345 + step * 20;
            let got = rx.at(mini(ts), step * 20);
            assert_eq!(got - previous, 20, "step {step} must advance one frame");
            previous = got;
        }
        assert!(previous > 65_536, "and we crossed the wrap: {previous}");
    }

    #[test]
    fn frame_ms_is_the_sample_count_at_the_bus_rate() {
        assert_eq!(frame_ms(160, 8_000), 20);
        assert_eq!(frame_ms(320, 16_000), 20);
        assert_eq!(frame_ms(0, 8_000), 0);
        assert_eq!(frame_ms(160, 0), 0, "no rate, no claim");
    }

    /// The whole point: what reaches the output bus carries the wire
    /// timestamp, so the lane's jitter buffer has a clock to schedule against.
    #[test]
    fn a_forwarded_frame_carries_the_wire_timestamp() {
        let mut edge = crate::codec_edge::EdgeAudio::new(8_000);
        let mut clock = RxTimestamps::default();
        let (tx, rx) = mpsc::channel();
        let mut warned = false;
        // 160 bytes of µ-law = one 20 ms frame at 8 kHz.
        let payload = vec![0xFF_u8; 160];
        forward_voice(
            &mut edge,
            &mut clock,
            &tx,
            VoiceFormat::G711U,
            &payload,
            12_340,
            Instant::now(),
            &mut warned,
        );
        let frame = rx.try_recv().expect("frame forwarded");
        let clock = frame.clock.expect("stamped with the sender's clock");
        assert_eq!(clock.ts_ms, 12_340);
        assert_eq!(clock.ms, 20);
        assert_eq!(frame.pcm.len(), 160);
        assert!(!warned, "a decodable frame warns about nothing");
    }

    /// An undecodable frame is dropped and warned about exactly once — the
    /// codec edge's existing contract, unchanged.
    #[test]
    fn an_undecodable_frame_is_dropped_and_warned_once() {
        let mut edge = crate::codec_edge::EdgeAudio::new(8_000);
        let mut clock = RxTimestamps::default();
        let (tx, rx) = mpsc::channel();
        let mut warned = false;
        for _ in 0..3 {
            forward_voice(
                &mut edge,
                &mut clock,
                &tx,
                VoiceFormat::Gsm,
                &[0_u8; 3],
                0,
                Instant::now(),
                &mut warned,
            );
        }
        assert!(rx.try_recv().is_err(), "nothing reached the bus");
        assert!(warned, "and it said so, once");
    }
}
