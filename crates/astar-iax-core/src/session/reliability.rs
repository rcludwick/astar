// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! OSeqno/ISeqno bookkeeping below an FSM.
//!
//! Three responsibilities (see `docs/design/session-fsm.md` §Reliability):
//!
//! 1. `OSeqno` bookkeeping — allocate, ACK-consume, retransmit, give up.
//! 2. `ISeqno` bookkeeping — dedupe, emit ACKs.
//! 3. Piggyback ACK handling on every incoming full frame.
//!
//! # Unit of reliability
//!
//! A `Reliability` instance is the unit of reliability for **either** a single
//! call leg (driven by [`crate::session::fsm::Fsm`]) **or** a single
//! registration (driven by [`crate::session::reg::RegFsm`]). Each FSM owns
//! its own `Reliability`; sequence numbers and in-flight tracking are scoped
//! to one call number pair. The type makes no assumption about which FSM is
//! above it — it only sees frames in and frames out.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::frame::{self, Frame, FullFrame, OwnedFullFrame, Subclass};
use crate::ie::Ies;
use crate::subclass::{FrameType, IaxCommand};

use super::call_no::CallNo;

/// Tunable retransmit knobs. Defaults match RFC 5456 §8.2.3.
///
/// `initial_rto` must be `> Duration::ZERO`. A zero (or sub-millisecond) value
/// would otherwise let the retransmit loop in `tick()` spin forever, since the
/// next-retry deadline would never advance past `now`. As a defensive measure,
/// the runtime clamps any computed per-attempt RTO to a 1ms floor (see
/// `Reliability::tick`), but callers should still supply a sensible
/// (millisecond-scale or larger) `initial_rto`.
#[derive(Debug, Clone, Copy)]
pub struct ReliabilityConfig {
    pub initial_rto: Duration,
    pub max_rto: Duration,
    pub max_attempts: u8,
    pub backoff: f32,
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            initial_rto: Duration::from_secs(1),
            max_rto: Duration::from_secs(4),
            max_attempts: 5,
            backoff: 2.0,
        }
    }
}

#[derive(Debug, Clone)]
struct InFlight {
    /// Fully-encoded wire bytes with seqno/callno already stamped (M6,
    /// iax-f755). Retransmits clone this and flip the retransmission bit at a
    /// fixed offset instead of re-parsing+re-encoding the frame's IEs, which
    /// removes the `.expect()`/`unreachable!()` panic surface on the hot path.
    wire: Vec<u8>,
    #[allow(dead_code)] // reserved for future RTT measurement / stats
    first_sent: Instant,
    last_sent: Instant,
    attempts: u8,
    next_retry_at: Instant,
}

/// Wire offsets of the mutable header fields in a full frame (RFC 5456 §6.1,
/// see `frame::encode_owned_full`). The call numbers occupy bytes 0-3
/// (`scallno` with the FULL flag, `dcallno` with the retrans flag);
/// `oseqno`/`iseqno` are bytes 8/9.
const OFF_OSEQNO: usize = 8;
const OFF_ISEQNO: usize = 9;
/// High bit of byte 2 (`dcallno` MSB) is `IAX_FLAG_RETRANS`.
const RETRANS_BIT: u8 = 0x80;

/// Set the retransmission flag on already-encoded full-frame wire bytes,
/// returning an owned copy. No IE re-parse (M6, iax-f755).
fn with_retransmission(wire: &[u8]) -> Vec<u8> {
    let mut bytes = wire.to_vec();
    if let Some(b) = bytes.get_mut(2) {
        *b |= RETRANS_BIT;
    }
    bytes
}

/// What the runtime should do after `on_frame_in` processed a frame.
#[derive(Debug)]
pub enum RxOutcome<'a> {
    /// Frame is new; pass it up to the FSM. `send_ack` is pre-serialized
    /// ACK bytes to put on the wire if the frame is reliable.
    Deliver {
        frame: Frame<'a>,
        send_ack: Option<Vec<u8>>,
    },
    /// Frame was a pure ACK or already accounted for; do nothing.
    Consumed,
    /// Duplicate (already-seen `ISeqno`); optionally resend ACK to nudge peer.
    Duplicate { resend_ack: Option<Vec<u8>> },
    /// Peer NAK'd our `OSeqno`; runtime decides whether/how to resend.
    Vnak(u8),
    /// In-flight `OSeqno` exhausted retries; FSM must surface a `DeliveryFailed`.
    GaveUp { oseqno: u8 },
}

#[derive(Debug, Default)]
pub struct TickOutcome {
    pub retransmit: Vec<Vec<u8>>,
    pub next_deadline: Option<Instant>,
    pub gave_up: Vec<u8>,
}

pub struct Reliability {
    our_call: CallNo,
    peer_call: Option<CallNo>,
    next_oseqno: u8,
    next_iseqno: u8,
    in_flight: BTreeMap<u8, InFlight>,
    config: ReliabilityConfig,
}

impl Reliability {
    #[must_use]
    pub fn new(our_call: CallNo, config: ReliabilityConfig) -> Self {
        Self {
            our_call,
            peer_call: None,
            next_oseqno: 0,
            next_iseqno: 0,
            in_flight: BTreeMap::new(),
            config,
        }
    }

    /// Record the peer's chosen call number after AUTHREQ / ACCEPT is seen.
    pub fn set_peer_call(&mut self, peer: CallNo) {
        self.peer_call = Some(peer);
    }

    #[must_use]
    pub fn peer_call(&self) -> Option<CallNo> {
        self.peer_call
    }

    /// Next `OSeqno` that `enqueue` will assign (read-only view for the FSM).
    #[must_use]
    pub fn next_oseqno(&self) -> u8 {
        self.next_oseqno
    }

    /// Reset OSeqno/ISeqno counters and drop in-flight tracking.
    ///
    /// Used when the IAX2 CALLTOKEN handshake replaces the initial NEW frame
    /// (RFC 5456 §8.6): the second NEW shares the same call leg but restarts
    /// sequence numbering from 0. The driver detects the FSM transition
    /// `NewSent → NewResent` and calls this before re-enqueueing the NEW.
    pub fn reset(&mut self) {
        self.next_oseqno = 0;
        self.next_iseqno = 0;
        self.in_flight.clear();
    }

    /// Reset for a brand-new transaction: seqnos, in-flight tracking AND the
    /// learned peer call (the next reliable frame goes out with `dest_call=0`).
    ///
    /// Used by the registration refresh (iax-177d): the registrar destroys its
    /// side of the call after REGACK, so a refresh REGREQ must open a fresh
    /// transaction — reusing the old peer call / seqnos made Asterisk drop it
    /// and the registration died at the first refresh.
    pub fn reset_transaction(&mut self) {
        self.reset();
        self.peer_call = None;
    }

    /// Allocate the next `OSeqno`, stamp it into the frame, serialize, track
    /// the in-flight entry, and return the wire bytes.
    pub fn enqueue(&mut self, mut frame: OwnedFullFrame, now: Instant) -> Vec<u8> {
        let oseqno = self.next_oseqno;
        frame.oseqno = oseqno;
        frame.iseqno = self.next_iseqno;
        frame.source_call = self.our_call.value();
        if let Some(peer) = self.peer_call {
            frame.dest_call = peer.value();
        }
        self.next_oseqno = self.next_oseqno.wrapping_add(1);
        // M6 (iax-f755): encode ONCE here with the seqno/callno already stamped
        // into the struct. Retransmits reuse these bytes (flipping the retrans
        // bit) instead of re-parsing the IEs every send.
        let wire = serialize_full(&frame);
        // Defence-in-depth: the bytes we just encoded must carry the seqno we
        // assigned at the fixed offsets the retransmit path patches.
        debug_assert_eq!(wire.get(OFF_OSEQNO).copied(), Some(oseqno));
        debug_assert_eq!(wire.get(OFF_ISEQNO).copied(), Some(frame.iseqno));
        self.in_flight.insert(
            oseqno,
            InFlight {
                wire: wire.clone(),
                first_sent: now,
                last_sent: now,
                attempts: 1,
                next_retry_at: now + self.config.initial_rto,
            },
        );
        wire
    }

    /// Process an incoming frame: piggyback-ACK our in-flight, dedupe, and
    /// decide whether the FSM should see it.
    pub fn on_frame_in<'a>(&mut self, frame: Frame<'a>, _now: Instant) -> RxOutcome<'a> {
        let full = match &frame {
            Frame::Full(f) => f.as_ref(),
            Frame::Mini(_) => {
                return RxOutcome::Deliver {
                    frame,
                    send_ack: None,
                };
            }
        };
        // 1. Consume piggyback ACK: peer's iseqno N means OSeqnos < N are acked.
        self.release_acked(full.iseqno);
        // 2. Pure ACK: subclass=ACK. No delivery to FSM.
        if matches!(full.subclass, Subclass::Iax(IaxCommand::Ack)) {
            return RxOutcome::Consumed;
        }
        // 3. VNAK: surface to runtime. The peer's iseqno is the next OSeqno
        // they expect us to (re)send (RFC 5456 §8.4).
        if matches!(full.subclass, Subclass::Iax(IaxCommand::Vnak)) {
            return RxOutcome::Vnak(full.iseqno);
        }
        // 3a. INVAL bypasses dedup: a restarted peer has no sequence state
        // for this call, so its INVAL may carry any oseqno (RFC 5456
        // §6.9.2). It is about the call itself, not in-order data: deliver
        // to the FSM, send no ACK, and do not advance ISeqno.
        if matches!(full.subclass, Subclass::Iax(IaxCommand::Inval)) {
            return RxOutcome::Deliver {
                frame,
                send_ack: None,
            };
        }
        // 4. Dedup against next expected ISeqno (wrap-safe).
        //
        // Mirror the `release_acked` convention: distance from `next_iseqno`
        // mod 256. `delta == 0` is the expected next frame. `delta < 128` is
        // "ahead" (gap / out-of-order — treat as new for now). `delta >= 128`
        // is "behind" — a duplicate we've already seen and ACKed.
        let delta = full.oseqno.wrapping_sub(self.next_iseqno);
        if delta >= 128 {
            let resend_ack = self.peer_call.map(|peer| {
                serialize_ack(
                    self.our_call,
                    peer,
                    full.timestamp,
                    self.next_oseqno,
                    self.next_iseqno,
                )
            });
            return RxOutcome::Duplicate { resend_ack };
        }
        // 5. New frame: advance ISeqno and emit an ACK if peer is known.
        self.next_iseqno = self.next_iseqno.wrapping_add(1);
        let send_ack = self.peer_call.map(|peer| {
            serialize_ack(
                self.our_call,
                peer,
                full.timestamp,
                self.next_oseqno,
                self.next_iseqno,
            )
        });
        RxOutcome::Deliver { frame, send_ack }
    }

    fn release_acked(&mut self, peer_iseqno: u8) {
        self.in_flight.retain(|&oseqno, _| {
            // Keep oseqno only if it is >= peer_iseqno in wrap arithmetic
            // (still pending). peer_iseqno is the *next* oseqno the peer
            // expects, so frames 0..peer_iseqno-1 (mod 256) are acked.
            // Wrap-safe: distance from peer_iseqno mod 256 in [0, 127] means
            // "at or ahead of peer_iseqno" → still pending.
            let pending = oseqno.wrapping_sub(peer_iseqno);
            pending < 128
        });
    }

    /// Advance time. Anything past its `next_retry_at` is resent (with the
    /// retransmission bit set); anything past `max_attempts` is given up.
    pub fn tick(&mut self, now: Instant) -> TickOutcome {
        let mut out = TickOutcome::default();
        let mut to_remove: Vec<u8> = Vec::new();
        for (&oseqno, entry) in &mut self.in_flight {
            // Drain all expired deadlines for this entry in one tick, so a
            // long stall converges either to a future deadline or to give-up.
            while now >= entry.next_retry_at {
                if entry.attempts >= self.config.max_attempts {
                    out.gave_up.push(oseqno);
                    to_remove.push(oseqno);
                    break;
                }
                // Re-emit the frame with retransmission=true. M6: patch the
                // stored wire bytes instead of re-parsing/re-encoding.
                out.retransmit.push(with_retransmission(&entry.wire));
                entry.attempts = entry.attempts.saturating_add(1);
                entry.last_sent = now;
                let new_rto = next_rto(
                    self.config.initial_rto,
                    self.config.max_rto,
                    self.config.backoff,
                    entry.attempts,
                );
                // Clamp to a 1ms floor: if a caller (or buggy test) builds a
                // `ReliabilityConfig` with `initial_rto == Duration::ZERO`,
                // `next_rto` returns zero and `next_retry_at += ZERO` leaves
                // the deadline at-or-before `now`, spinning the `while` loop
                // forever. The floor guarantees forward progress so we either
                // schedule a future deadline or hit `max_attempts` and give up.
                let new_rto = new_rto.max(Duration::from_millis(1));
                debug_assert!(new_rto > Duration::ZERO, "RTO floor must be > 0");
                // Schedule next retry off the previous deadline (not `now`) so
                // a single tick covering a long stall can advance through
                // multiple expirations and converge to give-up.
                entry.next_retry_at += new_rto;
            }
            if !to_remove.contains(&oseqno) {
                update_next_deadline(&mut out.next_deadline, entry.next_retry_at);
            }
        }
        for k in to_remove {
            self.in_flight.remove(&k);
        }
        out
    }

    /// Answer a VNAK (RFC 5456 §6.9.3): re-serialize every in-flight frame
    /// the peer still expects — `oseqno >= iseqno` in wrap arithmetic — with
    /// the retransmission bit set, ordered by wrap-distance from `iseqno`.
    /// Attempt counts and RTO deadlines are untouched: this is a
    /// peer-requested resend, not a timeout retransmit.
    #[must_use]
    pub fn resend_from(&self, iseqno: u8) -> Vec<Vec<u8>> {
        let mut pending: Vec<(u8, &InFlight)> = self
            .in_flight
            .iter()
            .filter_map(|(&oseqno, entry)| {
                let dist = oseqno.wrapping_sub(iseqno);
                (dist < 128).then_some((dist, entry))
            })
            .collect();
        pending.sort_unstable_by_key(|&(dist, _)| dist);
        pending
            .into_iter()
            .map(|(_, entry)| with_retransmission(&entry.wire))
            .collect()
    }

    /// `cfg(test)` introspection so unit tests can poke at in-flight state
    /// without exposing the `BTreeMap` publicly.
    ///
    /// Ungated by iax-8baf: the inbound leg runtime uses this to detect when
    /// Reliability releases the in-flight ANSWER's oseqno, driving the
    /// `AnswerSent -> Active` transition via `AppCommand::AnswerAcked`.
    #[must_use]
    pub fn has_inflight(&self, oseqno: u8) -> bool {
        self.in_flight.contains_key(&oseqno)
    }

    /// `cfg(test)` introspection: peek at an in-flight entry's `next_retry_at`.
    #[cfg(test)]
    #[must_use]
    pub fn next_retry_at(&self, oseqno: u8) -> Option<Instant> {
        self.in_flight.get(&oseqno).map(|e| e.next_retry_at)
    }
}

fn next_rto(initial: Duration, max_rto: Duration, backoff: f32, attempts: u8) -> Duration {
    let multiplier = backoff.powi(i32::from(attempts.saturating_sub(1)));
    let scaled = initial.mul_f32(multiplier);
    scaled.min(max_rto)
}

fn update_next_deadline(target: &mut Option<Instant>, candidate: Instant) {
    match target {
        Some(existing) if *existing <= candidate => {}
        _ => *target = Some(candidate),
    }
}

/// Serialize an owned full frame to the wire at first transmission (M6,
/// iax-f755). The `OwnedFullFrame` already holds its IE stream pre-encoded in
/// `ie_bytes` (or codec samples in `payload`), so [`frame::encode_owned_full`]
/// assembles the 12-byte header and appends those bytes directly — no
/// `Ies::parse`/`Ies::encode` round-trip, hence no panic surface. Retransmits
/// flip the bit on the stored bytes via [`with_retransmission`].
fn serialize_full(frame: &OwnedFullFrame) -> Vec<u8> {
    frame::encode_owned_full(frame)
}

fn serialize_ack(
    our_call: CallNo,
    peer_call: CallNo,
    timestamp: u32,
    oseqno: u8,
    iseqno: u8,
) -> Vec<u8> {
    encode_ack(
        our_call.value(),
        peer_call.value(),
        timestamp,
        oseqno,
        iseqno,
    )
}

/// Serialize an IAX2 ACK full frame to the wire (M8, iax-f755). Shared by the
/// runtime's [`Reliability::on_frame_in`] path and the test driver's auto-ACK
/// so the empty-IE ACK shape lives in one place. `source_call`/`dest_call` are
/// raw 15-bit call numbers; `oseqno`/`iseqno` are the ACK's own sequence
/// fields (an ACK uses the peer's `OSeqno + 1` as its `ISeqno`).
#[must_use]
pub fn encode_ack(
    source_call: u16,
    dest_call: u16,
    timestamp: u32,
    oseqno: u8,
    iseqno: u8,
) -> Vec<u8> {
    let ack = FullFrame {
        source_call,
        dest_call,
        retransmission: false,
        timestamp,
        oseqno,
        iseqno,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Ack),
        ies: Ies::empty(),
        payload: &[],
    };
    let mut out = Vec::with_capacity(12);
    // ACK frames carry no IEs, so encoding is infallible.
    frame::encode(&Frame::Full(Box::new(ack)), &mut out).expect("ACK has empty IE set");
    out
}

#[cfg(test)]
mod enqueue_tests {
    use super::*;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};

    pub(super) fn frame_for(oseqno_hint: u8) -> OwnedFullFrame {
        OwnedFullFrame {
            source_call: 0,
            dest_call: 0,
            retransmission: false,
            timestamp: 0,
            oseqno: oseqno_hint,
            iseqno: 0,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::New),
            ie_bytes: {
                let mut v = Vec::new();
                Ies::empty().encode(&mut v).expect("empty IE set encodes");
                v
            },
            payload: Vec::new(),
        }
    }

    #[test]
    fn enqueue_assigns_sequential_oseqnos_and_returns_bytes() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        let now = Instant::now();
        let b0 = r.enqueue(frame_for(0), now);
        assert!(!b0.is_empty(), "wire bytes returned");
        assert_eq!(r.next_oseqno(), 1, "next oseqno advanced");
        let _b1 = r.enqueue(frame_for(0), now);
        assert_eq!(r.next_oseqno(), 2);
    }

    #[test]
    fn enqueue_stamps_oseqno_into_wire_bytes() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        let bytes = r.enqueue(frame_for(0xff), Instant::now());
        // Wire offset 8 is oseqno (full-frame header layout in frame.rs).
        assert_eq!(
            bytes[8], 0,
            "first enqueue stamps oseqno 0, ignoring caller's hint"
        );
    }

    // M6 (iax-f755): the fast-path `encode_owned_full` used by enqueue must
    // produce byte-identical wire bytes to the canonical `frame::encode`, and
    // `with_retransmission` must only flip the retrans bit.
    #[test]
    fn fast_path_serialization_matches_canonical_encode() {
        use crate::frame::{Frame, OwnedFrame, encode};
        use crate::ie::Ies;
        use crate::subclass::FrameType;
        // A NEW with real IEs (the case the old path re-parsed every send).
        let owned = OwnedFullFrame::with_ies(
            5,
            7,
            false,
            1234,
            3,
            4,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::New),
            &Ies {
                username: Some("rob"),
                calltoken: Some(b"opaque"),
                ..Ies::empty()
            },
        )
        .unwrap();

        let fast = super::serialize_full(&owned);

        let mut canonical = Vec::new();
        let borrowed = OwnedFrame::Full(owned.clone());
        encode(&borrowed.as_frame().unwrap(), &mut canonical).unwrap();
        assert_eq!(fast, canonical, "fast path matches canonical encode");

        // Retransmission only sets the high bit of byte 2.
        let retrans = super::with_retransmission(&fast);
        let mut expected = fast.clone();
        expected[2] |= 0x80;
        assert_eq!(retrans, expected);
        // And it round-trips back to a parseable frame with retrans set.
        let Frame::Full(p) = crate::frame::parse(&retrans).unwrap() else {
            panic!("full frame")
        };
        assert!(p.retransmission);
        assert_eq!(p.oseqno, 3);
        assert_eq!(p.iseqno, 4);
    }
}

#[cfg(test)]
mod on_frame_in_tests {
    use super::*;
    use crate::frame::{Frame, FullFrame};
    use crate::ie::Ies;
    use crate::subclass::{ControlSubclass, FrameType, IaxCommand};

    fn make_peer_frame(
        our_call_dest: u16,
        peer_src_call: u16,
        oseqno: u8,
        iseqno: u8,
        subclass: Subclass,
        frame_type: FrameType,
    ) -> Frame<'static> {
        Frame::Full(Box::new(FullFrame {
            source_call: peer_src_call,
            dest_call: our_call_dest,
            retransmission: false,
            timestamp: 0,
            oseqno,
            iseqno,
            frame_type,
            subclass,
            ies: Ies::empty(),
            payload: &[],
        }))
    }

    #[test]
    fn pure_ack_is_consumed_and_releases_inflight() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        let now = Instant::now();
        let _ = r.enqueue(super::enqueue_tests::frame_for(0), now);
        r.set_peer_call(CallNo::new(7).unwrap());
        let ack = make_peer_frame(1, 7, 0, 1, Subclass::Iax(IaxCommand::Ack), FrameType::Iax);
        let outcome = r.on_frame_in(ack, now);
        assert!(matches!(outcome, RxOutcome::Consumed));
        assert_eq!(r.next_oseqno(), 1);
        assert!(!r.has_inflight(0), "in-flight 0 acked and released");
    }

    #[test]
    fn duplicate_iseqno_triggers_duplicate_outcome() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        r.set_peer_call(CallNo::new(7).unwrap());
        let now = Instant::now();
        let f0 = make_peer_frame(
            1,
            7,
            0,
            0,
            Subclass::Iax(IaxCommand::AuthReq),
            FrameType::Iax,
        );
        let _ = r.on_frame_in(f0, now);
        let f0_again = make_peer_frame(
            1,
            7,
            0,
            0,
            Subclass::Iax(IaxCommand::AuthReq),
            FrameType::Iax,
        );
        let outcome = r.on_frame_in(f0_again, now);
        assert!(matches!(outcome, RxOutcome::Duplicate { .. }));
    }

    #[test]
    fn vnak_signals_runtime() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        r.set_peer_call(CallNo::new(7).unwrap());
        let now = Instant::now();
        let vnak = make_peer_frame(1, 7, 0, 3, Subclass::Iax(IaxCommand::Vnak), FrameType::Iax);
        let outcome = r.on_frame_in(vnak, now);
        assert!(matches!(outcome, RxOutcome::Vnak(3)));
    }

    #[test]
    fn voice_frame_delivers_with_ack_bytes() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        r.set_peer_call(CallNo::new(7).unwrap());
        let now = Instant::now();
        let voice = make_peer_frame(
            1,
            7,
            0,
            0,
            Subclass::Control(ControlSubclass::Answer),
            FrameType::Control,
        );
        let outcome = r.on_frame_in(voice, now);
        match outcome {
            RxOutcome::Deliver { send_ack, .. } => {
                assert!(send_ack.is_some(), "reliable full frame triggers ACK");
            }
            other => panic!("expected Deliver, got {other:?}"),
        }
    }

    #[test]
    fn inval_bypasses_dedup_and_delivers_without_ack() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        r.set_peer_call(CallNo::new(7).unwrap());
        let now = Instant::now();
        // Advance next_iseqno to 3 with ordinary traffic.
        for o in 0..3u8 {
            let f = make_peer_frame(
                1,
                7,
                o,
                0,
                Subclass::Iax(IaxCommand::AuthReq),
                FrameType::Iax,
            );
            let _ = r.on_frame_in(f, now);
        }
        // Restarted peer: INVAL with oseqno 0 — wrap-"behind" next_iseqno,
        // so the dedup path would otherwise classify it Duplicate.
        let inval = make_peer_frame(1, 7, 0, 0, Subclass::Iax(IaxCommand::Inval), FrameType::Iax);
        match r.on_frame_in(inval, now) {
            RxOutcome::Deliver { send_ack, .. } => {
                assert!(send_ack.is_none(), "INVAL is not ACKed");
            }
            other => panic!("INVAL must always deliver to the FSM, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tick_tests {
    use super::*;

    #[test]
    fn tick_before_deadline_emits_nothing() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        let now = Instant::now();
        let _ = r.enqueue(super::enqueue_tests::frame_for(0), now);
        let outcome = r.tick(now);
        assert!(outcome.retransmit.is_empty());
        assert!(outcome.gave_up.is_empty());
    }

    #[test]
    fn tick_after_rto_retransmits_with_backoff() {
        let cfg = ReliabilityConfig {
            initial_rto: Duration::from_millis(100),
            max_rto: Duration::from_millis(800),
            max_attempts: 5,
            backoff: 2.0,
        };
        let mut r = Reliability::new(CallNo::new(1).unwrap(), cfg);
        let t0 = Instant::now();
        let _ = r.enqueue(super::enqueue_tests::frame_for(0), t0);
        let outcome = r.tick(t0 + Duration::from_millis(150));
        assert_eq!(outcome.retransmit.len(), 1);
        let outcome2 = r.tick(t0 + Duration::from_millis(151));
        assert_eq!(outcome2.retransmit.len(), 0);
    }

    /// Regression for iax-7d65: a `ReliabilityConfig` built with
    /// `initial_rto = Duration::ZERO` made `next_rto` return zero, so the
    /// `while now >= entry.next_retry_at` loop in `tick()` would spin forever
    /// (the deadline never advanced past `now`). The fix clamps every
    /// per-attempt RTO to a 1ms floor, guaranteeing forward progress so the
    /// loop terminates either at a future deadline or at give-up.
    #[test]
    fn tick_with_zero_initial_rto_does_not_hang_and_advances_deadline() {
        let cfg = ReliabilityConfig {
            initial_rto: Duration::ZERO,
            max_rto: Duration::ZERO,
            max_attempts: 3,
            backoff: 2.0,
        };
        let mut r = Reliability::new(CallNo::new(1).unwrap(), cfg);
        let t0 = Instant::now();
        let _ = r.enqueue(super::enqueue_tests::frame_for(0), t0);
        // Pre-fix: this call hangs forever. Post-fix: the loop drains
        // `max_attempts` retries in one tick (each advancing the deadline by
        // 1ms, the clamp floor) and gives up.
        let outcome = r.tick(t0 + Duration::from_millis(10));
        assert!(
            outcome.gave_up.contains(&0),
            "OSeqno 0 must give up once max_attempts is reached",
        );
        assert!(
            !r.has_inflight(0),
            "given-up entry must be removed from in-flight",
        );
    }

    /// Companion to the hang regression: when an entry has retries remaining,
    /// the clamped RTO must still leave the deadline strictly in the future of
    /// `now`. Without the 1ms floor, `next_retry_at` would equal `now` and the
    /// next tick would loop again at zero cost.
    #[test]
    fn tick_with_zero_initial_rto_advances_deadline_for_remaining_retries() {
        let cfg = ReliabilityConfig {
            initial_rto: Duration::ZERO,
            max_rto: Duration::ZERO,
            // 100 attempts ensures we don't hit give-up at t0 — one of the
            // retries will leave the entry in-flight with a future deadline.
            max_attempts: 100,
            backoff: 2.0,
        };
        let mut r = Reliability::new(CallNo::new(1).unwrap(), cfg);
        let t0 = Instant::now();
        let _ = r.enqueue(super::enqueue_tests::frame_for(0), t0);
        let outcome = r.tick(t0);
        assert!(outcome.gave_up.is_empty(), "must not give up below the cap");
        let next = r
            .next_retry_at(0)
            .expect("entry still in flight after partial backoff");
        assert!(
            next > t0,
            "clamp must push next_retry_at strictly past `now`, got {next:?} vs t0={t0:?}",
        );
    }

    #[test]
    fn tick_after_max_attempts_gives_up() {
        let cfg = ReliabilityConfig {
            initial_rto: Duration::from_millis(10),
            max_rto: Duration::from_millis(40),
            max_attempts: 3,
            backoff: 2.0,
        };
        let mut r = Reliability::new(CallNo::new(1).unwrap(), cfg);
        let t0 = Instant::now();
        let _ = r.enqueue(super::enqueue_tests::frame_for(0), t0);
        let outcome = r.tick(t0 + Duration::from_secs(10));
        assert!(!outcome.gave_up.is_empty(), "OSeqno 0 gave up");
        assert!(outcome.gave_up.contains(&0));
    }
}

#[cfg(test)]
mod wrap_dedup_tests {
    //! Regression coverage for iax-b47c: raw `u8 <` comparison in the `ISeqno`
    //! dedupe path misfired across 256-frame wrap boundaries.
    use super::*;
    use crate::frame::{Frame, FullFrame};
    use crate::ie::Ies;
    use crate::subclass::{ControlSubclass, FrameType};
    use proptest::prelude::*;

    fn peer_full(oseqno: u8) -> Frame<'static> {
        Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno,
            iseqno: 0,
            frame_type: FrameType::Control,
            subclass: Subclass::Control(ControlSubclass::Answer),
            ies: Ies::empty(),
            payload: &[],
        }))
    }

    fn fresh_reliability() -> Reliability {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        r.set_peer_call(CallNo::new(7).unwrap());
        r
    }

    /// 300 in-order frames must never dedup. Pre-fix, the 257th frame
    /// (oseqno=0, `next_iseqno`=256→0) and everything after misfires because
    /// `next_iseqno` wraps but the raw `u8` compare can't see it.
    #[test]
    fn three_hundred_in_order_frames_never_dedup() {
        let mut r = fresh_reliability();
        let now = Instant::now();
        for i in 0u32..300 {
            let oseqno = (i & 0xff) as u8;
            let outcome = r.on_frame_in(peer_full(oseqno), now);
            assert!(
                matches!(outcome, RxOutcome::Deliver { .. }),
                "frame #{i} (oseqno={oseqno}) should be Deliver, got {outcome:?}",
            );
        }
    }

    /// `next_iseqno` wraps to 5; a delayed re-delivery of oseqno=250 (which we
    /// already saw 11 frames ago) must be deduped, not re-delivered.
    #[test]
    fn delayed_duplicate_after_wrap_is_deduped() {
        let mut r = fresh_reliability();
        let now = Instant::now();
        // Drive 261 fresh frames: oseqnos 0..=255, then 0..=4. next_iseqno = 5.
        for i in 0u32..261 {
            let oseqno = (i & 0xff) as u8;
            let _ = r.on_frame_in(peer_full(oseqno), now);
        }
        // oseqno=250 was seen ~16 frames back. It is "behind" next_iseqno=5
        // in wrap arithmetic (5 - 250 = 11 ahead, so 250 is 245 behind).
        let outcome = r.on_frame_in(peer_full(250), now);
        assert!(
            matches!(outcome, RxOutcome::Duplicate { .. }),
            "stale oseqno=250 after wrap must dedup, got {outcome:?}",
        );
    }

    /// `next_iseqno` = 250; a fresh oseqno=0 (which arrives later, after wrap)
    /// must be delivered. Pre-fix the raw `u8` compare says 0 < 250 → duplicate.
    #[test]
    fn fresh_frame_at_wrap_boundary_is_delivered() {
        let mut r = fresh_reliability();
        let now = Instant::now();
        // Drive 250 fresh frames so next_iseqno = 250.
        for i in 0u8..250 {
            let _ = r.on_frame_in(peer_full(i), now);
        }
        // Now deliver oseqnos 250..=255, then 0 — the post-wrap fresh frame.
        for oseqno in 250u8..=255 {
            let outcome = r.on_frame_in(peer_full(oseqno), now);
            assert!(
                matches!(outcome, RxOutcome::Deliver { .. }),
                "oseqno={oseqno} should deliver, got {outcome:?}",
            );
        }
        let outcome = r.on_frame_in(peer_full(0), now);
        assert!(
            matches!(outcome, RxOutcome::Deliver { .. }),
            "post-wrap fresh oseqno=0 must deliver, got {outcome:?}",
        );
    }

    proptest! {
        /// Property: feeding N strictly-sequential frames (N may span multiple
        /// wraps) must produce N Deliver outcomes — never Duplicate, never
        /// Consumed, never Vnak.
        #[test]
        fn prop_in_order_sequence_never_dedups(n in 1u32..2000) {
            let mut r = fresh_reliability();
            let now = Instant::now();
            for i in 0..n {
                let oseqno = (i & 0xff) as u8;
                let outcome = r.on_frame_in(peer_full(oseqno), now);
                prop_assert!(
                    matches!(outcome, RxOutcome::Deliver { .. }),
                    "frame #{} (oseqno={}) should be Deliver, got {:?}",
                    i, oseqno, outcome,
                );
            }
        }

        /// Property: after N fresh in-order frames, replaying any of the last
        /// 127 oseqnos must dedup (they are unambiguously "behind"
        /// next_iseqno in u8 wrap arithmetic).
        #[test]
        fn prop_recent_replays_always_dedup(n in 128u32..1000, lookback in 1u8..127) {
            let mut r = fresh_reliability();
            let now = Instant::now();
            for i in 0..n {
                let oseqno = (i & 0xff) as u8;
                let _ = r.on_frame_in(peer_full(oseqno), now);
            }
            // Last delivered oseqno was (n-1) & 0xff. Replay something
            // `lookback` frames earlier.
            let last = ((n - 1) & 0xff) as u8;
            let replay = last.wrapping_sub(lookback);
            let outcome = r.on_frame_in(peer_full(replay), now);
            prop_assert!(
                matches!(outcome, RxOutcome::Duplicate { .. }),
                "replay of oseqno={} after {} frames should dedup, got {:?}",
                replay, n, outcome,
            );
        }
    }
}

#[cfg(test)]
mod resend_tests {
    use super::*;
    use crate::frame::{Frame, FullFrame};
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};

    fn ack_from_peer(iseqno: u8) -> Frame<'static> {
        Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::Ack),
            ies: Ies::empty(),
            payload: &[],
        }))
    }

    fn rel_with_inflight(n: u8) -> Reliability {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        r.set_peer_call(CallNo::new(7).unwrap());
        let now = Instant::now();
        for _ in 0..n {
            let _ = r.enqueue(super::enqueue_tests::frame_for(0), now);
        }
        r
    }

    #[test]
    fn resend_from_returns_unacked_in_order_with_retransmission_bit() {
        let mut r = rel_with_inflight(3); // oseqnos 0, 1, 2 in flight
        // Peer acked oseqno 0 (its iseqno = 1).
        let _ = r.on_frame_in(ack_from_peer(1), Instant::now());
        let before = r.next_retry_at(1).expect("oseqno 1 still in flight");

        let resent = r.resend_from(1);
        assert_eq!(resent.len(), 2, "oseqnos 1 and 2 are >= the VNAK'd iseqno");
        // Wire byte 8 is oseqno; byte 2 top bit is the retransmission flag.
        assert_eq!(resent[0][8], 1);
        assert_eq!(resent[1][8], 2);
        assert!(resent.iter().all(|b| b[2] & 0x80 == 0x80), "R bit set");
        // Peer-requested resend: attempts/RTO untouched, entries retained.
        assert_eq!(r.next_retry_at(1), Some(before));
        assert!(r.has_inflight(1) && r.has_inflight(2));
    }

    #[test]
    fn resend_from_includes_the_exact_iseqno_frame() {
        let r = rel_with_inflight(1); // oseqno 0
        let resent = r.resend_from(0);
        assert_eq!(
            resent.len(),
            1,
            "oseqno == iseqno is the frame the peer wants next"
        );
        assert_eq!(resent[0][8], 0);
    }

    #[test]
    fn resend_from_is_empty_when_nothing_pending() {
        let mut r = rel_with_inflight(2);
        let _ = r.on_frame_in(ack_from_peer(2), Instant::now()); // acks 0 and 1
        assert!(r.resend_from(2).is_empty());
    }

    #[test]
    fn resend_from_is_wrap_safe_and_wrap_ordered() {
        let mut r = Reliability::new(CallNo::new(1).unwrap(), ReliabilityConfig::default());
        r.set_peer_call(CallNo::new(7).unwrap());
        let now = Instant::now();
        // Drive oseqno up to 254, releasing each frame as we go so the
        // in-flight window never exceeds 1 and wrap arithmetic stays valid.
        for i in 0u16..254 {
            let _ = r.enqueue(super::enqueue_tests::frame_for(0), now);
            // Ack the frame we just sent: peer_iseqno = i+1 (next expected).
            // Intentional truncation: i stays within 0..254 so this wraps
            // exactly as expected for u8 oseqno arithmetic.
            #[allow(clippy::cast_possible_truncation)]
            let ack_iseqno = (i as u8).wrapping_add(1);
            let _ = r.on_frame_in(ack_from_peer(ack_iseqno), now);
        }
        // All released — in-flight should be empty.
        assert!(
            r.resend_from(254).is_empty(),
            "nothing in flight before wrap batch"
        );
        // In flight across the wrap: oseqnos 254, 255, 0.
        for _ in 0..3 {
            let _ = r.enqueue(super::enqueue_tests::frame_for(0), now);
        }
        let resent = r.resend_from(254);
        let oseqnos: Vec<u8> = resent.iter().map(|b| b[8]).collect();
        assert_eq!(
            oseqnos,
            vec![254, 255, 0],
            "wrap-distance order, not raw u8 order"
        );
    }
}
