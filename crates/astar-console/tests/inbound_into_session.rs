// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Inbound-into-session end-to-end (iax-a1fb P1): a `ConsoleSession` starts its
//! inbound listener on an ephemeral loopback port, a peer UDP socket sends a
//! hand-built NEW, and driving `poll_inbound` auto-answers + adopts the inbound
//! call into the SAME session `Manager` the WT dial path uses.
//!
//! Mirrors the iax-8baf `incoming_loopback` harness (`peer_socket`,
//! `valid_new_ies`, `new_datagram`, the ACK pump) but drives the console-side
//! adopt/poll instead of the raw listener.
#![allow(clippy::too_many_lines)]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_audio::NullBackend;
use astar_console::{AnswerPolicy, ConsoleSession};
use astar_iax::{IncomingAuthPolicy, IncomingCallPolicy, IncomingDecisionPolicy, KnownNodes};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};

const PEER_CALL: u16 = 13885;

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    encode(frame, &mut b).expect("encode");
    b
}

fn new_datagram(ies: Ies<'_>, source_call: u16) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies,
        payload: &[],
    })))
}

fn valid_new_ies() -> Ies<'static> {
    Ies {
        called_number: Some("s"),
        calling_number: Some("1001"),
        calling_name: Some("Rob"),
        capability: Some(VoiceFormat::G711U.as_u32() | VoiceFormat::G711A.as_u32()),
        format: Some(VoiceFormat::G711U.as_u32()),
        version: Some(2),
        calltoken: Some(b""),
        ..Ies::empty()
    }
}

fn peer_socket() -> (UdpSocket, SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let a = s.local_addr().unwrap();
    (s, a)
}

/// Drain whatever the listener has sent us and ACK every reliable full frame
/// (so the leg's Reliability releases → ANSWER's oseqno frees → Answered fires).
fn pump_acks(sock: &UdpSocket, listener_addr: SocketAddr, my_call: u16) {
    let mut buf = [0u8; 4096];
    // Bound the drain so a regression that makes the listener flood (e.g. an
    // INVAL/ACK storm against a reaped leg) fails fast here instead of hanging
    // CI. A correct listener never floods, so a healthy pump reads a few frames
    // and then times out well under this cap.
    let mut budget = 512;
    while let Ok((n, _src)) = sock.recv_from(&mut buf) {
        budget -= 1;
        assert!(
            budget > 0,
            "pump_acks drained 512 frames without quiescing — the listener is \
             flooding (likely an INVAL/ACK storm against a reaped leg)"
        );
        let bytes = buf[..n].to_vec();
        let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
            continue;
        };
        // Never ACK an ACK, and never ACK an INVAL: an INVAL is terminal and
        // unanswered (RFC 5456), so ACKing it would feed a ping-pong against a
        // listener that INVALs unknown-dest frames.
        if !matches!(
            f.subclass,
            Subclass::Iax(IaxCommand::Ack | IaxCommand::Inval)
        ) {
            let ack = enc(&Frame::Full(Box::new(FullFrame {
                source_call: my_call,
                dest_call: f.source_call,
                retransmission: false,
                timestamp: f.timestamp,
                oseqno: f.iseqno,
                iseqno: f.oseqno.wrapping_add(1),
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::Ack),
                ies: Ies::empty(),
                payload: &[],
            })));
            let _ = sock.send_to(&ack, listener_addr);
        }
    }
}

/// Like [`pump_acks`] but also returns the listener leg's `source_call` learned
/// from the last non-ACK frame it sent us (needed to address a HANGUP back).
fn pump_acks_learn(sock: &UdpSocket, listener_addr: SocketAddr, my_call: u16) -> Option<u16> {
    let mut buf = [0u8; 4096];
    let mut learned = None;
    // See `pump_acks`: bound the drain so a listener flood fails fast.
    let mut budget = 512;
    while let Ok((n, _src)) = sock.recv_from(&mut buf) {
        budget -= 1;
        assert!(
            budget > 0,
            "pump_acks_learn drained 512 frames without quiescing — the listener \
             is flooding (likely an INVAL/ACK storm against a reaped leg)"
        );
        let bytes = buf[..n].to_vec();
        let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
            continue;
        };
        if !matches!(
            f.subclass,
            Subclass::Iax(IaxCommand::Ack | IaxCommand::Inval)
        ) {
            learned = Some(f.source_call);
            let ack = enc(&Frame::Full(Box::new(FullFrame {
                source_call: my_call,
                dest_call: f.source_call,
                retransmission: false,
                timestamp: f.timestamp,
                oseqno: f.iseqno,
                iseqno: f.oseqno.wrapping_add(1),
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::Ack),
                ies: Ies::empty(),
                payload: &[],
            })));
            let _ = sock.send_to(&ack, listener_addr);
        }
    }
    learned
}

/// A HANGUP from the peer, addressed to the listener's leg (`dest_call`).
fn hangup_datagram(source_call: u16, dest_call: u16, oseqno: u8) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call,
        retransmission: false,
        timestamp: 0,
        oseqno,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Hangup),
        ies: Ies::empty(),
        payload: &[],
    })))
}

/// Build a session listening on an ephemeral loopback port that auto-answers
/// inbound NEWs (auth Off), returning the session and its bound address.
fn session_listening_auto() -> (ConsoleSession, SocketAddr) {
    session_listening_with_cap(20)
}

/// Build a session listening with `AnswerPolicy::Manual` (auth Off, cap 20).
fn session_listening_manual() -> (ConsoleSession, SocketAddr) {
    let mut session = ConsoleSession::new();
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        // Pin to Never so the test is valid under any build profile
        // (release defaults to Always, which adds a CALLTOKEN round-trip).
        calltoken: astar_iax::IncomingCallTokenPolicy::Never,
        ..IncomingCallPolicy::default()
    };
    session
        .start_inbound(
            "127.0.0.1:0".parse().unwrap(),
            policy,
            AnswerPolicy::Manual,
            20,
            || -> Box<dyn astar_audio::AudioBackend> { Box::new(NullBackend::new()) },
            (None, None),
        )
        .expect("inbound listener starts");
    let addr = session.inbound_addr().expect("listener bound");
    (session, addr)
}

/// Build a session with a specific `max_calls` cap.
fn session_listening_with_cap(max_calls: usize) -> (ConsoleSession, SocketAddr) {
    let mut session = ConsoleSession::new();
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        // Pin to Never so the cap-rejection test is valid under any build profile
        // (release defaults to Always, which adds a CALLTOKEN round-trip that
        // can make `inbound_past_cap_is_rejected` vacuous).
        calltoken: astar_iax::IncomingCallTokenPolicy::Never,
        ..IncomingCallPolicy::default()
    };
    session
        .start_inbound(
            "127.0.0.1:0".parse().unwrap(),
            policy,
            AnswerPolicy::Auto,
            max_calls,
            || -> Box<dyn astar_audio::AudioBackend> { Box::new(NullBackend::new()) },
            (None, None),
        )
        .expect("inbound listener starts");
    let addr = session.inbound_addr().expect("listener bound");
    (session, addr)
}

/// Build a session listening with an inbound node allowlist (auth Off, cap 20,
/// Auto answer). Callers whose node id is not in `allowed` are rejected at
/// call-setup time before adopt.
fn session_listening_with_allowlist(allowed: &[&str]) -> (ConsoleSession, SocketAddr) {
    let mut session = ConsoleSession::new();
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        calltoken: astar_iax::IncomingCallTokenPolicy::Never,
        ..IncomingCallPolicy::default()
    };
    session
        .start_inbound_with_allowlist(
            "127.0.0.1:0".parse().unwrap(),
            policy,
            AnswerPolicy::Auto,
            20,
            Some(KnownNodes::from_iter_labels(allowed.iter().copied())),
            || -> Box<dyn astar_audio::AudioBackend> { Box::new(NullBackend::new()) },
            (None, None),
        )
        .expect("inbound listener starts");
    let addr = session.inbound_addr().expect("listener bound");
    (session, addr)
}

#[test]
fn inbound_new_is_adopted_into_the_session_manager() {
    let (mut session, addr) = session_listening_auto();

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut adopted = false;
    while Instant::now() < deadline && !adopted {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        if session.call_count() == 1 {
            adopted = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        adopted,
        "inbound call adopted into the SAME session engine (call_count == 1)"
    );
    assert_eq!(session.call_count(), 1);
}

/// Two NEWs from two distinct peer sockets, cap=20 → both are adopted
/// (`call_count` == 2). Verifies the cap check uses `manager.call_count()`, not
/// `self.active.is_some()`.
#[test]
fn second_inbound_under_cap_is_adopted_not_rejected() {
    const PEER_CALL_1: u16 = 13885;
    const PEER_CALL_2: u16 = 13886;

    let (mut session, addr) = session_listening_with_cap(20);

    let (peer1, _pa1) = peer_socket();
    let (peer2, _pa2) = peer_socket();

    // Send first NEW and drive the handshake until it is adopted.
    peer1
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL_1), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() < 1 {
        session.poll_inbound();
        pump_acks(&peer1, addr, PEER_CALL_1);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.call_count(), 1, "first call must be adopted");

    // Send second NEW from a different peer and drive the handshake.
    peer2
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL_2), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() < 2 {
        session.poll_inbound();
        pump_acks(&peer2, addr, PEER_CALL_2);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        session.call_count(),
        2,
        "second inbound under cap must be adopted, not rejected (call_count == 2)"
    );
}

/// Manual-mode inbound: a NEW is received, parked (not adopted), then
/// `answer_pending()` is called and the call is adopted into the session.
#[test]
fn manual_answer_adopts_the_parked_call() {
    let (mut session, addr) = session_listening_manual();

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Pump for up to 1 s to let the listener receive and park the offer.
    // We stop as soon as the park is visible or time runs out.
    // Unlike Auto mode we cannot detect the parked state via call_count
    // (it stays 0), so we run for a fixed window sufficient for the actor
    // thread to deliver the Incoming event.
    let park_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < park_deadline {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }

    // Step 1: not adopted yet — still parked under Manual.
    assert_eq!(
        session.call_count(),
        0,
        "manual: not adopted until answer()"
    );

    // Step 2: the IncomingCall caller string surfaced exactly once.
    assert!(
        session.take_incoming_from().is_some(),
        "IncomingCall caller surfaced via take_incoming_from()"
    );

    // Step 3: answer_pending() promotes the parked offer → adopted call.
    session.answer_pending().expect("answer ok");

    // Pump again so the adopt handshake completes and call_count reflects it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() < 1 {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        session.call_count(),
        1,
        "call adopted after answer_pending()"
    );
}

/// Two NEWs from two distinct peer sockets, cap=20 → `snapshot().calls`
/// contains two entries (iax-a1fb P5). Verifies the concurrent-call list is
/// surfaced in the snapshot and is secret-free (`CallSnapshot` fields are node ids,
/// device names, and health counters — no credentials).
#[test]
fn snapshot_lists_all_concurrent_calls() {
    const PEER_CALL_1: u16 = 13889;
    const PEER_CALL_2: u16 = 13890;

    let (mut session, addr) = session_listening_with_cap(20);

    let (peer1, _pa1) = peer_socket();
    let (peer2, _pa2) = peer_socket();

    // Send first NEW and drive the handshake until it is adopted.
    peer1
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL_1), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() < 1 {
        session.poll_inbound();
        pump_acks(&peer1, addr, PEER_CALL_1);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.call_count(), 1, "first call must be adopted");

    // Send second NEW from a different peer and drive the handshake.
    peer2
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL_2), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() < 2 {
        session.poll_inbound();
        pump_acks(&peer2, addr, PEER_CALL_2);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.call_count(), 2, "both calls must be adopted");

    // The snapshot must surface both calls in `calls` (iax-a1fb P5).
    let snap = session.snapshot();
    assert_eq!(
        snap.calls.len(),
        2,
        "snapshot.calls must list all {} concurrent calls",
        snap.calls.len()
    );

    // Secret-free: CallSnapshot Debug must contain no credentials.
    // (Spot-check by asserting no `:` between quotes suggesting a password.)
    for cs in &snap.calls {
        let dbg = format!("{cs:?}");
        assert!(
            !dbg.contains("secret") && !dbg.contains("password"),
            "CallSnapshot Debug must be credential-free: {dbg}"
        );
    }
}

/// A peer that hangs up an adopted inbound call must have its leg reaped from
/// the pool, so the slot it held is freed. Without the reaper the `Hungup` leg
/// lingers in `manager.calls`, `call_count()` never drops, and (with a low
/// `max_calls`) every subsequent inbound offer is permanently busy-rejected —
/// the bug that wedged a node after `max_calls` callers had come and gone.
#[test]
fn hung_up_inbound_leg_is_reaped_from_the_pool() {
    let (mut session, addr) = session_listening_with_cap(1);
    let (peer, _pa) = peer_socket();

    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Establish + adopt, learning the listener leg's call number on the way.
    let mut listener_call: u16 = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() < 1 {
        let _ = session.snapshot();
        if let Some(c) = pump_acks_learn(&peer, addr, PEER_CALL) {
            listener_call = c;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.call_count(), 1, "call adopted");
    assert_ne!(listener_call, 0, "learned the listener leg call number");

    // Peer hangs up the established leg.
    peer.send_to(&hangup_datagram(PEER_CALL, listener_call, 1), addr)
        .unwrap();

    // The leg must be reaped so the pool returns to 0.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() > 0 {
        let _ = session.snapshot();
        let _ = pump_acks_learn(&peer, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        session.call_count(),
        0,
        "hung-up inbound leg must be reaped from the pool"
    );
}

/// Two NEWs from two distinct peer sockets, cap=1 → second is rejected
/// (`call_count` stays 1).
#[test]
fn inbound_past_cap_is_rejected() {
    const PEER_CALL_1: u16 = 13885;
    const PEER_CALL_2: u16 = 13887;

    let (mut session, addr) = session_listening_with_cap(1);

    let (peer1, _pa1) = peer_socket();
    let (peer2, _pa2) = peer_socket();

    // Send first NEW and drive the handshake until it is adopted.
    peer1
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL_1), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && session.call_count() < 1 {
        session.poll_inbound();
        pump_acks(&peer1, addr, PEER_CALL_1);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.call_count(), 1, "first call must be adopted");

    // Send second NEW — it must be rejected (at cap).
    peer2
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL_2), addr)
        .unwrap();

    // Drive poll_inbound for up to 2 s; call_count must stay at 1.
    // Do NOT pump acks for peer2 here: pumping could spin in a tight loop
    // if the listener sends rapid CALLTOKEN frames; the cap check fires in
    // poll_inbound regardless of whether the CALLTOKEN exchange completes.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        session.poll_inbound();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        session.call_count(),
        1,
        "second inbound past cap must be rejected (call_count stays 1)"
    );
}

/// A NEW from a node that is NOT on a non-empty allowlist must be rejected at
/// call-setup time: it is never adopted (`call_count` stays 0). The caller node
/// id is `valid_new_ies().calling_number` = "1001"; the allowlist contains only
/// "55553".
#[test]
fn inbound_from_node_not_on_allowlist_is_rejected() {
    let (mut session, addr) = session_listening_with_allowlist(&["55553"]);

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Drive poll_inbound for up to 2 s; call_count must stay at 0 (rejected).
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        session.call_count(),
        0,
        "caller not on the allowlist must be rejected (call_count stays 0)"
    );
}

/// A NEW from a node that IS on the allowlist must be adopted normally
/// (`call_count` == 1). "1001" is the caller node id from `valid_new_ies()`.
#[test]
fn inbound_from_node_on_allowlist_is_adopted() {
    let (mut session, addr) = session_listening_with_allowlist(&["1001"]);

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut adopted = false;
    while Instant::now() < deadline && !adopted {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        if session.call_count() == 1 {
            adopted = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        adopted,
        "caller on the allowlist must be adopted (call_count == 1)"
    );
    assert_eq!(session.call_count(), 1);
}

/// An empty allowlist (`Some` but no entries) admits as today — backward compat
/// with the no-allowlist path. The caller "1001" is adopted (`call_count` == 1).
#[test]
fn inbound_with_empty_allowlist_admits_as_today() {
    let (mut session, addr) = session_listening_with_allowlist(&[]);

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut adopted = false;
    while Instant::now() < deadline && !adopted {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        if session.call_count() == 1 {
            adopted = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        adopted,
        "empty allowlist must admit as today (call_count == 1)"
    );
    assert_eq!(session.call_count(), 1);
}
