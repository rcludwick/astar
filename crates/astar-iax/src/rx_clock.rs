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

use std::sync::mpsc;

use astar_audio::RxFrame;
use astar_iax_core::VoiceFormat;

/// Rebuilds a monotonic 32-bit-wide millisecond clock from the wire's mix of
/// full (32-bit) and mini (16-bit) timestamps.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RxTimestamps {
    /// Highest timestamp seen so far — the epoch anchor. It never walks
    /// backwards, so one reordered frame can't drag the high half with it.
    last: i64,
}

/// Half of a 16-bit wrap: the distance past which "ahead" is better read as
/// "behind, one epoch back".
const HALF_WRAP: i64 = 0x8000;
/// One 16-bit wrap.
const WRAP: i64 = 0x1_0000;

impl RxTimestamps {
    /// Extend one wire timestamp to the full millisecond clock.
    pub(crate) fn extend(&mut self, raw: u32) -> i64 {
        let raw = i64::from(raw);
        let ts = if raw > 0xFFFF {
            // Only a full frame can carry this; take it as it stands.
            raw
        } else {
            // Either a mini frame, or a full frame in the call's first 65 s —
            // and in that second case the anchor's high half is still zero, so
            // the same arithmetic gives the same answer.
            let mut candidate = (self.last & !0xFFFF) | raw;
            if candidate + HALF_WRAP < self.last {
                candidate += WRAP;
            } else if candidate > self.last + HALF_WRAP && candidate >= WRAP {
                candidate -= WRAP;
            }
            candidate
        };
        self.last = self.last.max(ts);
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
/// the sender's clock, so the bus lane's jitter buffer can schedule it.
///
/// Undecodable frames are dropped, with one warning per call — the RX codec
/// edge's existing contract (iax-31f7 / iax-4348).
pub(crate) fn forward_voice(
    edge: &mut crate::codec_edge::EdgeAudio,
    clock: &mut RxTimestamps,
    spk_tx: &mpsc::Sender<RxFrame>,
    format: VoiceFormat,
    payload: &[u8],
    ts: u32,
    decode_warned: &mut bool,
) {
    match edge.decode(format, payload) {
        Some(pcm) => {
            let ts_ms = clock.extend(ts);
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

    #[test]
    fn full_frame_timestamps_pass_through() {
        let mut c = RxTimestamps::default();
        assert_eq!(c.extend(0), 0);
        assert_eq!(c.extend(20), 20);
        assert_eq!(c.extend(1_000_000), 1_000_000);
    }

    /// A mini frame carries only the low 16 bits. Past the first wrap the
    /// receiver has to supply the high half, or the clock appears to jump back
    /// 65 seconds every 65 seconds.
    #[test]
    fn mini_frame_timestamps_are_extended_across_the_wrap() {
        let mut c = RxTimestamps::default();
        // A full frame puts us just under the wrap.
        assert_eq!(c.extend(65_500), 65_500);
        // Minis continue, then wrap to a small number.
        assert_eq!(c.extend(65_520), 65_520);
        assert_eq!(c.extend(4), 65_540, "wrapped, not restarted");
        assert_eq!(c.extend(24), 65_560);
        // And the second wrap works the same way.
        let mut c = RxTimestamps::default();
        for i in 0..4_i64 {
            let raw = u32::try_from(i * 20).unwrap();
            assert_eq!(c.extend(raw), i * 20);
        }
        assert_eq!(c.extend(65_535), 65_535);
        assert_eq!(c.extend(0), 65_536);
    }

    /// One reordered frame must not drag the epoch backwards for the frames
    /// that follow it.
    #[test]
    fn a_reordered_frame_does_not_move_the_epoch() {
        let mut c = RxTimestamps::default();
        assert_eq!(c.extend(65_535), 65_535);
        assert_eq!(c.extend(20), 65_556, "first past the wrap");
        assert_eq!(c.extend(65_515), 65_515, "the straggler, still in epoch 0");
        assert_eq!(c.extend(40), 65_576, "and the next one carries on");
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
                &mut warned,
            );
        }
        assert!(rx.try_recv().is_err(), "nothing reached the bus");
        assert!(warned, "and it said so, once");
    }
}
