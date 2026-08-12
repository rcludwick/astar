// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! iax-a307 acceptance: an established call survives 10% loss for 60s, a 5s
//! blackhole produces exactly one `ConnectionLost` then one `ConnectionRestored`,
//! and a replayed full frame reaches the app exactly once.
//!
//! Fully deterministic: simulated clock (`base + offset` Instants), an
//! in-process fake peer instead of a socket, and a seeded LCG drop pattern.
//! The driver mirrors `crates/astar-iax/src/client.rs::run_loop` (actions →
//! enqueue/send/timers, `RxOutcome` policy incl. `VNAK→resend_from`), minus the
//! CALLTOKEN reset (this peer authenticates without CALLTOKEN).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_iax_core::frame::{self, Frame, FullFrame, Subclass, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::{
    Action, AppCommand, AppEvent, Event, Fsm, SessionState, TimerKind,
};
use astar_iax_core::session::reliability::{Reliability, ReliabilityConfig, RxOutcome};
use astar_iax_core::subclass::{FrameType, IaxCommand};

/// Deterministic LCG (MMIX constants) — reproducible, no rand dependency.
struct Lcg(u64);

impl Lcg {
    fn chance(&mut self, pct: u64) -> bool {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % 100 < pct
    }
}

const SERVER_CALL: u16 = 7;

/// Minimal in-process responder: handshake (NEW→AUTHREQ, AUTHREP→ACCEPT),
/// cumulative ACKs, PING→PONG, LAGRQ→LAGRP. It never retransmits its own
/// replies — a dropped PONG is simply re-elicited by the client's next
/// PING or RTO retransmit.
struct FakePeer {
    oseqno: u8,
    /// Next client oseqno we expect (cumulative-ACK convention).
    expected: u8,
    sent_authreq: bool,
    sent_accept: bool,
}

impl FakePeer {
    fn new() -> Self {
        Self {
            oseqno: 0,
            expected: 0,
            sent_authreq: false,
            sent_accept: false,
        }
    }

    fn frame_bytes(
        &self,
        client_call: u16,
        ts: u32,
        cmd: IaxCommand,
        oseqno: u8,
        ies: Ies<'_>,
    ) -> Vec<u8> {
        let f = Frame::Full(Box::new(FullFrame {
            source_call: SERVER_CALL,
            dest_call: client_call,
            retransmission: false,
            timestamp: ts,
            oseqno,
            iseqno: self.expected,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(cmd),
            ies,
            payload: &[],
        }));
        let mut out = Vec::with_capacity(64);
        frame::encode(&f, &mut out).expect("peer frame encodes");
        out
    }

    /// Take the next reply oseqno.
    fn next_oseqno(&mut self) -> u8 {
        let o = self.oseqno;
        self.oseqno = self.oseqno.wrapping_add(1);
        o
    }

    /// Process one datagram from the client; return reply datagrams.
    fn on_datagram(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let Ok(Frame::Full(ff)) = parse_lenient(bytes) else {
            return Vec::new(); // ignore minis
        };
        let client_call = ff.source_call;
        let mut out = Vec::new();
        if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
            return out;
        }
        // Cumulative-ACK sequencing: only the expected oseqno is processed;
        // behind = duplicate (re-ACK only); ahead = a gap the client's RTO
        // will fill (ACK the current expectation, process nothing).
        let is_new = ff.oseqno == self.expected;
        if is_new {
            self.expected = self.expected.wrapping_add(1);
        }
        out.push(self.frame_bytes(
            client_call,
            ff.timestamp,
            IaxCommand::Ack,
            self.oseqno,
            Ies::empty(),
        ));
        if !is_new {
            return out;
        }
        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !self.sent_authreq => {
                self.sent_authreq = true;
                let o = self.next_oseqno();
                out.push(self.frame_bytes(
                    client_call,
                    0,
                    IaxCommand::AuthReq,
                    o,
                    Ies {
                        challenge: Some("c0ffee"),
                        authmethods: Some(2),
                        ..Ies::empty()
                    },
                ));
            }
            Subclass::Iax(IaxCommand::AuthRep) if !self.sent_accept => {
                self.sent_accept = true;
                let o = self.next_oseqno();
                out.push(self.frame_bytes(client_call, 0, IaxCommand::Accept, o, Ies::empty()));
            }
            Subclass::Iax(IaxCommand::Ping) => {
                let o = self.next_oseqno();
                // RFC 5456 §6.7.3: PONG echoes the PING timestamp.
                out.push(self.frame_bytes(
                    client_call,
                    ff.timestamp,
                    IaxCommand::Pong,
                    o,
                    Ies::empty(),
                ));
            }
            Subclass::Iax(IaxCommand::LagRq) => {
                let o = self.next_oseqno();
                out.push(self.frame_bytes(
                    client_call,
                    ff.timestamp,
                    IaxCommand::LagRp,
                    o,
                    Ies::empty(),
                ));
            }
            _ => {}
        }
        out
    }
}

struct Harness {
    base: Instant,
    now_off: Duration,
    fsm: Fsm,
    rel: Reliability,
    /// (deadline-as-offset, kind) — mirrors `run_loop`'s timer wheel.
    timers: Vec<(Duration, TimerKind)>,
    peer: FakePeer,
    lcg: Lcg,
    /// Drop probability (%) applied independently per datagram, both ways.
    loss_pct: u64,
    events: Vec<AppEvent>,
    to_peer: VecDeque<Vec<u8>>,
    to_client: VecDeque<Vec<u8>>,
}

impl Harness {
    fn new(seed: u64) -> Self {
        let creds = Credentials {
            username: "acceptance".to_string(),
            password: Arc::new(Secret::new(String::new())),
            allowed_methods: AuthMethods::MD5,
        };
        let our_call = CallNo::new(1).expect("CallNo::new(1) is valid");
        Self {
            base: Instant::now(),
            now_off: Duration::ZERO,
            fsm: Fsm::new(creds, our_call),
            rel: Reliability::new(our_call, ReliabilityConfig::default()),
            timers: Vec::new(),
            peer: FakePeer::new(),
            lcg: Lcg(seed),
            loss_pct: 0,
            events: Vec::new(),
            to_peer: VecDeque::new(),
            to_client: VecDeque::new(),
        }
    }

    fn now(&self) -> Instant {
        self.base + self.now_off
    }

    fn dispatch(&mut self, actions: impl IntoIterator<Item = Action>) {
        for action in actions {
            match action {
                Action::SendReliable(frame) => {
                    let bytes = self.rel.enqueue(frame, self.now());
                    self.to_peer.push_back(bytes);
                }
                Action::SendUnreliable(bytes) => self.to_peer.push_back(bytes),
                Action::SetPeerCall(peer) => self.rel.set_peer_call(peer),
                Action::ResetReliability => self.rel.reset(),
                Action::SetTimer(kind, dur) => {
                    self.timers.retain(|(_, k)| *k != kind);
                    self.timers.push((self.now_off + dur, kind));
                }
                Action::CancelTimer(kind) => self.timers.retain(|(_, k)| *k != kind),
                Action::AppEvent(ev) => self.events.push(ev),
                Action::LogInvalid { .. } => {}
            }
        }
    }

    fn client_rx(&mut self, bytes: &[u8]) {
        let Ok(frame) = parse_lenient(bytes) else {
            return;
        };
        let now = self.now();
        match self.rel.on_frame_in(frame, now) {
            RxOutcome::Deliver { frame, send_ack } => {
                if let Some(ack) = send_ack {
                    self.to_peer.push_back(ack);
                }
                let actions = self.fsm.handle(Event::Frame { frame, now });
                self.dispatch(actions);
            }
            RxOutcome::Consumed => {}
            RxOutcome::Duplicate { resend_ack } => {
                if let Some(b) = resend_ack {
                    self.to_peer.push_back(b);
                }
            }
            RxOutcome::Vnak(iseqno) => {
                for b in self.rel.resend_from(iseqno) {
                    self.to_peer.push_back(b);
                }
            }
            RxOutcome::GaveUp { oseqno } => {
                let actions = self.fsm.handle(Event::DeliveryFailed { oseqno });
                self.dispatch(actions);
            }
        }
    }

    /// Drain both wire directions until quiescent, dropping per `loss_pct`.
    fn pump(&mut self) {
        loop {
            let mut progressed = false;
            while let Some(bytes) = self.to_peer.pop_front() {
                progressed = true;
                if self.lcg.chance(self.loss_pct) {
                    continue; // dropped on the floor
                }
                for reply in self.peer.on_datagram(&bytes) {
                    self.to_client.push_back(reply);
                }
            }
            while let Some(bytes) = self.to_client.pop_front() {
                progressed = true;
                if self.lcg.chance(self.loss_pct) {
                    continue;
                }
                self.client_rx(&bytes);
            }
            if !progressed {
                break;
            }
        }
    }

    /// Advance simulated time, fire timers and retransmits, drain the wire.
    fn step(&mut self, step: Duration) {
        self.now_off += step;
        let now_off = self.now_off;
        let mut fired = Vec::new();
        self.timers.retain(|(at, kind)| {
            if *at <= now_off {
                fired.push(*kind);
                false
            } else {
                true
            }
        });
        for kind in fired {
            let actions = self.fsm.handle(Event::Timer {
                kind,
                now: self.now(),
            });
            self.dispatch(actions);
        }
        let tick = self.rel.tick(self.now());
        for bytes in tick.retransmit {
            self.to_peer.push_back(bytes);
        }
        for oseqno in tick.gave_up {
            let actions = self.fsm.handle(Event::DeliveryFailed { oseqno });
            self.dispatch(actions);
        }
        self.pump();
    }

    fn establish(&mut self) {
        let actions = self.fsm.handle(Event::App(AppCommand::StartCall {
            dest: "55553".to_string(),
            now: self.now(),
        }));
        self.dispatch(actions);
        self.pump();
        assert!(
            matches!(self.fsm.state(), SessionState::Active(_)),
            "handshake must reach Active, got {}",
            self.fsm.state().name()
        );
    }

    fn count(&self, f: impl Fn(&AppEvent) -> bool) -> usize {
        self.events.iter().filter(|e| f(e)).count()
    }
}

#[test]
fn call_survives_60s_of_10_percent_loss() {
    let mut h = Harness::new(0xA307_2026);
    h.establish();
    h.loss_pct = 10;
    for _ in 0..600 {
        h.step(Duration::from_millis(100));
    }
    assert!(
        matches!(h.fsm.state(), SessionState::Active(_)),
        "call must still be Active after 60s of 10% loss, got {}",
        h.fsm.state().name()
    );
    assert_eq!(
        h.count(|e| matches!(e, AppEvent::Disconnected { .. })),
        0,
        "loss alone must never disconnect"
    );
    assert!(
        h.fsm.rtt().is_some(),
        "keepalive exchanges must have produced an RTT estimate"
    );
}

#[test]
fn five_second_blackhole_emits_lost_then_restored_exactly_once() {
    let mut h = Harness::new(0xA307);
    h.establish();
    // 10s of healthy traffic.
    for _ in 0..100 {
        h.step(Duration::from_millis(100));
    }
    assert_eq!(h.count(|e| matches!(e, AppEvent::ConnectionLost)), 0);
    // 5s blackhole: drop everything both ways.
    h.loss_pct = 100;
    for _ in 0..50 {
        h.step(Duration::from_millis(100));
    }
    assert_eq!(
        h.count(|e| matches!(e, AppEvent::ConnectionLost)),
        1,
        "exactly one ConnectionLost during the blackhole"
    );
    assert_eq!(h.count(|e| matches!(e, AppEvent::ConnectionRestored)), 0);
    assert!(
        matches!(h.fsm.state(), SessionState::Active(_)),
        "a blackhole must not tear the call down"
    );
    // Traffic resumes. Recovery is driven by RTO retransmits of the PINGs
    // sent into the blackhole (backoff 1s→2s→4s), so the first PONG can land
    // as late as ~4s after the hole closes; give it a 10s window.
    h.loss_pct = 0;
    for _ in 0..100 {
        h.step(Duration::from_millis(100));
    }
    assert_eq!(
        h.count(|e| matches!(e, AppEvent::ConnectionRestored)),
        1,
        "exactly one ConnectionRestored after recovery"
    );
    assert_eq!(
        h.count(|e| matches!(e, AppEvent::ConnectionLost)),
        1,
        "no second Lost without a second loss episode"
    );
    assert!(matches!(h.fsm.state(), SessionState::Active(_)));
}

#[test]
fn replayed_full_frame_is_delivered_to_the_app_exactly_once() {
    let mut h = Harness::new(1234);
    h.establish();
    // Peer sends a DTMF END full frame, then the network replays the
    // identical datagram (same oseqno).
    let dtmf = Frame::Full(Box::new(FullFrame {
        source_call: SERVER_CALL,
        dest_call: 1,
        retransmission: false,
        timestamp: 100,
        oseqno: h.peer.oseqno,
        iseqno: h.peer.expected,
        frame_type: FrameType::DtmfEnd,
        subclass: Subclass::Dtmf('7'),
        ies: Ies::empty(),
        payload: &[],
    }));
    let mut bytes = Vec::new();
    frame::encode(&dtmf, &mut bytes).expect("test frame encodes");
    h.to_client.push_back(bytes.clone());
    h.to_client.push_back(bytes); // exact replay
    h.pump();
    assert_eq!(
        h.count(|e| matches!(e, AppEvent::DtmfReceived('7'))),
        1,
        "a duplicated full frame must reach the app exactly once"
    );
}
