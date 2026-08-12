// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end inbound tests: bind an `IncomingCallListener` on an ephemeral
//! loopback port, send hand-built NEW datagrams from a test UDP socket, and
//! assert the on-wire responses (ACCEPT/ANSWER/REJECT/INVAL/PONG) + the
//! surfaced events. Mirrors `registrar_loopback.rs`.
//!
//! Nothing binds the fixed IAX2 port 4569 or hits the network — every test uses
//! `127.0.0.1:0` so it cannot collide with a running harness/parrot.

#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_iax::{
    IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener, IncomingCallPolicy,
    IncomingCallTokenPolicy, IncomingDecisionPolicy,
};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::Secret;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};

const PEER_CALL: u16 = 13885;

/// Encode a frame to bytes.
fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode(frame, &mut bytes).expect("encode");
    bytes
}

/// Build a `NEW` datagram (peer to listener, with dcallno zero).
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

/// A standard, valid NEW offer (no auth, no calltoken).
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

/// Bind a peer UDP socket with a short read timeout.
fn peer_socket() -> (UdpSocket, SocketAddr) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let addr = sock.local_addr().unwrap();
    (sock, addr)
}

/// Receive frames from the listener until `pred` matches one or we time out.
/// ACKs every full frame the listener sends so its Reliability releases.
fn recv_until<F>(
    sock: &UdpSocket,
    listener_addr: SocketAddr,
    my_call: u16,
    mut pred: F,
) -> Option<Vec<u8>>
where
    F: FnMut(&FullFrame<'_>) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok((n, _src)) = sock.recv_from(&mut buf) {
            {
                let bytes = buf[..n].to_vec();
                let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
                    continue;
                };
                // ACK any reliable full frame so the leg's Reliability releases
                // (so the ANSWER's oseqno frees → AnswerAcked fires).
                if !matches!(f.subclass, Subclass::Iax(IaxCommand::Ack)) {
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
                if pred(&f) {
                    return Some(bytes);
                }
            }
        }
    }
    None
}

#[test]
fn auto_answer_new_yields_accept_answer_and_unified_call() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), listener_addr)
        .unwrap();

    // Expect an ACCEPT (FORMAT IE) then an ANSWER (Control).
    let accept = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Accept)
    });
    assert!(accept.is_some(), "ACCEPT must arrive on the wire");
    let answer = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.frame_type == FrameType::Control
            && f.subclass == Subclass::Control(astar_iax_core::subclass::ControlSubclass::Answer)
    });
    assert!(answer.is_some(), "ANSWER (Control) must arrive on the wire");

    // The auto-answer path delivers the UNIFIED Call directly.
    let ev = events
        .recv_timeout(Duration::from_secs(2))
        .expect("an IncomingCallEvent arrives");
    match ev {
        IncomingCallEvent::Answered {
            call,
            events: call_events,
        } => {
            // It's the same Call keystone an outbound dial yields; snapshot is
            // secret-free and the adopt-rx/tx ends are present (poolable).
            let snap = call.snapshot();
            assert!(snap.node.is_empty());
            // iax-31f7: once the leg reaches Active (the ANSWER was acked by
            // the peer above), CallEvent::Answered must carry the negotiated
            // format — this offer negotiates ulaw.
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match call_events.recv_timeout(remaining) {
                    Ok(astar_iax::CallEvent::Answered { format }) => {
                        assert_eq!(format, VoiceFormat::G711U, "Answered carries negotiation");
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => panic!("no CallEvent::Answered before deadline: {e}"),
                }
            }
            // iax-31f7: the leg run-loop publishes the negotiated codec once
            // per event-handling pass; poll briefly since the ACCEPT/ANSWER
            // exchange above already drove several loop passes but the store
            // is async w.r.t. this test thread.
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut negotiated = snap.negotiated_format;
            while negotiated.is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
                negotiated = call.snapshot().negotiated_format;
            }
            assert_eq!(
                negotiated,
                Some(VoiceFormat::G711U),
                "default-policy loopback call must negotiate ulaw"
            );
        }
        IncomingCallEvent::Incoming(_) => panic!("expected Answered, got Incoming"),
    }
}

#[test]
fn app_decide_surfaces_incoming_then_answer_yields_unified_call() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), listener_addr)
        .unwrap();

    // ACCEPT arrives (we must keep ACKing so retransmits don't pile up).
    let accept = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Accept)
    });
    assert!(accept.is_some(), "ACCEPT arrives in AppDecide mode");

    let ev = events
        .recv_timeout(Duration::from_secs(2))
        .expect("IncomingCallEvent arrives");
    let incoming = match ev {
        IncomingCallEvent::Incoming(c) => c,
        IncomingCallEvent::Answered { .. } => panic!("expected Incoming in AppDecide"),
    };
    assert_eq!(incoming.peer_call.value(), PEER_CALL);
    assert_eq!(incoming.calling_name.as_deref(), Some("Rob"));
    assert_eq!(incoming.called_number.as_deref(), Some("s"));

    // Answer it → unified Call + ANSWER on the wire.
    let (call, _call_events) = incoming.answer().expect("answer");
    let answer = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.frame_type == FrameType::Control
            && f.subclass == Subclass::Control(astar_iax_core::subclass::ControlSubclass::Answer)
    });
    assert!(answer.is_some(), "ANSWER arrives after answer()");
    let _ = call.snapshot();
}

#[test]
fn reject_sends_hangup_with_cause() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), listener_addr)
        .unwrap();

    let ev = events.recv_timeout(Duration::from_secs(2)).expect("event");
    let incoming = match ev {
        IncomingCallEvent::Incoming(c) => c,
        IncomingCallEvent::Answered { .. } => panic!("expected Incoming"),
    };
    incoming.reject(Some("Busy".into())).expect("reject");
    let hangup = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Hangup)
    });
    assert!(hangup.is_some(), "REJECT/HANGUP arrives after reject()");
}

#[test]
fn malformed_new_missing_called_number_is_rejected_no_event() {
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            auth: IncomingAuthPolicy::Off,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    // Missing CALLED_NUMBER → MandatoryIeMissing → inline REJECT, no spawn.
    let ies = Ies {
        version: Some(2),
        capability: Some(VoiceFormat::G711U.as_u32()),
        ..Ies::empty()
    };
    peer.send_to(&new_datagram(ies, PEER_CALL), listener_addr)
        .unwrap();

    let reject = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Reject)
    });
    assert!(reject.is_some(), "inline REJECT for malformed NEW");
    assert!(
        events.recv_timeout(Duration::from_millis(300)).is_err(),
        "no IncomingCallEvent for a rejected NEW"
    );
}

#[test]
fn unknown_user_with_auth_required_is_rejected_no_spawn() {
    let mut creds = HashMap::new();
    creds.insert(
        "known".to_string(),
        std::sync::Arc::new(Secret::new("pw".to_string())),
    );
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Required,
        credentials: creds,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    let ies = Ies {
        username: Some("nobody"),
        ..valid_new_ies()
    };
    peer.send_to(&new_datagram(ies, PEER_CALL), listener_addr)
        .unwrap();

    let reject = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Reject)
    });
    assert!(
        reject.is_some(),
        "REJECT for unknown user under auth=Required"
    );
    assert!(
        events.recv_timeout(Duration::from_millis(300)).is_err(),
        "no event for an unknown-user NEW"
    );
}

#[test]
fn unknown_dest_call_gets_inval() {
    let (listener, _events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            auth: IncomingAuthPolicy::Off,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    // A full frame addressed to a dest_call we never allocated.
    let stray = enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: 999,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Ping),
        ies: Ies::empty(),
        payload: &[],
    })));
    peer.send_to(&stray, listener_addr).unwrap();

    let mut buf = [0u8; 4096];
    let inval = peer
        .recv_from(&mut buf)
        .ok()
        .and_then(|(n, _)| match parse_lenient(&buf[..n]) {
            Ok(Frame::Full(f)) if f.subclass == Subclass::Iax(IaxCommand::Inval) => Some(()),
            _ => None,
        });
    assert!(inval.is_some(), "INVAL for unknown dest_call");
}

#[test]
fn poke_gets_pong_no_spawn() {
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            auth: IncomingAuthPolicy::Off,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    let poke = enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Poke),
        ies: Ies::empty(),
        payload: &[],
    })));
    peer.send_to(&poke, listener_addr).unwrap();

    let mut buf = [0u8; 4096];
    let pong = peer
        .recv_from(&mut buf)
        .ok()
        .and_then(|(n, _)| match parse_lenient(&buf[..n]) {
            Ok(Frame::Full(f)) if f.subclass == Subclass::Iax(IaxCommand::Pong) => Some(()),
            _ => None,
        });
    assert!(pong.is_some(), "PONG for a POKE");
    assert!(
        events.recv_timeout(Duration::from_millis(300)).is_err(),
        "POKE does not spawn a leg / surface an event"
    );
}

#[test]
fn duplicate_new_does_not_spawn_twice() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    let dg = new_datagram(valid_new_ies(), PEER_CALL);
    peer.send_to(&dg, listener_addr).unwrap();
    peer.send_to(&dg, listener_addr).unwrap(); // retransmit

    // Exactly one Incoming event despite two identical NEWs.
    let first = events
        .recv_timeout(Duration::from_secs(2))
        .expect("first event");
    assert!(matches!(first, IncomingCallEvent::Incoming(_)));
    assert!(
        events.recv_timeout(Duration::from_millis(400)).is_err(),
        "a retransmitted NEW must not spawn a second leg"
    );
}

#[test]
fn spoofed_source_frame_is_dropped() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), listener_addr)
        .unwrap();
    let ev = events.recv_timeout(Duration::from_secs(2)).expect("event");
    let incoming = match ev {
        IncomingCallEvent::Incoming(c) => c,
        IncomingCallEvent::Answered { .. } => panic!("expected Incoming"),
    };
    let our_call = incoming.our_call.value();

    // A *different* source sends a frame addressed to the leg's dest_call. The
    // Listener must drop it (source-addr pin) — no INVAL back to the spoofer,
    // the leg stays up. We assert the spoofer gets no reply.
    let (spoofer, _spoof_addr) = peer_socket();
    let stray = enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: our_call,
        retransmission: false,
        timestamp: 0,
        oseqno: 1,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Hangup),
        ies: Ies::empty(),
        payload: &[],
    })));
    spoofer.send_to(&stray, listener_addr).unwrap();
    let mut buf = [0u8; 4096];
    assert!(
        spoofer.recv_from(&mut buf).is_err(),
        "spoofed-source frame must be silently dropped (no reply)"
    );
    // The leg is still alive: answer still works.
    let (_call, _e) = incoming.answer().expect("leg still alive after spoof drop");
}

// --- iax-99cd: dynamic credential resolver on the inbound policy -------------

/// Build an AUTHREP datagram carrying `md5(challenge || secret)`, addressed to
/// the leg's `dest_call` (the listener's call number, learned from the AUTHREQ).
fn authrep_datagram(md5_hex: &str, source_call: u16, dest_call: u16) -> Vec<u8> {
    let ies = Ies {
        md5_result: Some(md5_hex),
        ..Ies::empty()
    };
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call,
        retransmission: false,
        timestamp: 0,
        oseqno: 1,
        iseqno: 1,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::AuthRep),
        ies,
        payload: &[],
    })))
}

/// auth=Required + a `credential_resolver` that returns the secret for the
/// offered username → full MD5 handshake NEW → AUTHREQ → AUTHREP → ACCEPT.
#[test]
fn auth_required_resolver_hit_completes_md5_handshake() {
    use std::sync::Arc;
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Required,
        decision: IncomingDecisionPolicy::AutoAccept,
        // Static map empty on purpose: the resolver is the only source.
        credential_resolver: Some(Arc::new(|u: &str| {
            if u == "allstar-public" {
                "supersecret".to_string()
            } else {
                String::new()
            }
        })),
        ..IncomingCallPolicy::default()
    };
    let (listener, _events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    let ies = Ies {
        username: Some("allstar-public"),
        ..valid_new_ies()
    };
    peer.send_to(&new_datagram(ies, PEER_CALL), listener_addr)
        .unwrap();

    // The listener must CHALLENGE (AUTHREQ), not REJECT.
    let authreq = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::AuthReq)
    })
    .expect("AUTHREQ for a resolver-hit username under auth=Required");

    // Read the challenge IE and answer with md5(challenge || secret).
    let Ok(Frame::Full(f)) = parse_lenient(&authreq) else {
        panic!("AUTHREQ parses");
    };
    let challenge = f.ies.challenge.expect("AUTHREQ carries a challenge");
    // The leg's call number — the peer addresses subsequent frames to it.
    let leg_call = f.source_call;
    let md5 = astar_iax_core::session::auth::md5_response(challenge, "supersecret");
    peer.send_to(&authrep_datagram(&md5, PEER_CALL, leg_call), listener_addr)
        .unwrap();

    let accept = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Accept)
    });
    assert!(
        accept.is_some(),
        "ACCEPT after a correct AUTHREP completes the handshake"
    );
}

/// auth=Required + resolver MISS for the offered username → REJECT "No such user",
/// no AUTHREQ, no spawn.
#[test]
fn auth_required_resolver_miss_rejects_no_such_user() {
    use std::sync::Arc;
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Required,
        credential_resolver: Some(Arc::new(|_u: &str| String::new())),
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    let ies = Ies {
        username: Some("nobody"),
        ..valid_new_ies()
    };
    peer.send_to(&new_datagram(ies, PEER_CALL), listener_addr)
        .unwrap();

    let reject = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Reject)
    });
    assert!(
        reject.is_some(),
        "REJECT for a resolver-miss username under auth=Required"
    );
    assert!(
        events.recv_timeout(Duration::from_millis(300)).is_err(),
        "no event for a rejected unknown-user NEW"
    );
}

/// auth=Optional + resolver HIT → the listener challenges (AUTHREQ) rather than
/// admitting anonymously.
#[test]
fn auth_optional_resolver_hit_challenges() {
    use std::sync::Arc;
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Optional,
        decision: IncomingDecisionPolicy::AutoAccept,
        credential_resolver: Some(Arc::new(|u: &str| {
            if u == "known" {
                "pw".to_string()
            } else {
                String::new()
            }
        })),
        ..IncomingCallPolicy::default()
    };
    let (listener, _events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    let ies = Ies {
        username: Some("known"),
        ..valid_new_ies()
    };
    peer.send_to(&new_datagram(ies, PEER_CALL), listener_addr)
        .unwrap();

    let authreq = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::AuthReq)
    });
    assert!(
        authreq.is_some(),
        "Optional + resolver-hit challenges with AUTHREQ"
    );
}

/// auth=Optional + resolver MISS → admit anonymously (ACCEPT, never AUTHREQ).
#[test]
fn auth_optional_resolver_miss_admits_anonymously() {
    use std::sync::Arc;
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Optional,
        decision: IncomingDecisionPolicy::AutoAccept,
        credential_resolver: Some(Arc::new(|_u: &str| String::new())),
        ..IncomingCallPolicy::default()
    };
    let (listener, _events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    let ies = Ies {
        username: Some("nobody"),
        ..valid_new_ies()
    };
    peer.send_to(&new_datagram(ies, PEER_CALL), listener_addr)
        .unwrap();

    // First frame back must be ACCEPT, never AUTHREQ.
    let accept = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        matches!(
            f.subclass,
            Subclass::Iax(IaxCommand::Accept | IaxCommand::AuthReq)
        )
    })
    .expect("a setup frame arrives");
    let Ok(Frame::Full(f)) = parse_lenient(&accept) else {
        panic!("frame parses");
    };
    assert_eq!(
        f.subclass,
        Subclass::Iax(IaxCommand::Accept),
        "Optional + resolver-miss admits anonymously (ACCEPT, not AUTHREQ)"
    );
}

/// iax-7bdc: an app decision (answer) that lands DURING the calltoken
/// handshake must be parked, not dropped. Live repro: the offer surfaces at
/// leg spawn, and over the internet the token echo takes one RTT — the node's
/// Auto policy answered before the token-bearing NEW arrived, the FSM's
/// catch-all dropped the `AnswerIncoming`, and the leg sat in `AcceptSent` until
/// its retries expired: the caller never got ANSWER (or the join greeting).
/// On loopback the echo lands in the same run-loop pass, which is why every
/// pre-existing test passed.
#[test]
fn answer_during_calltoken_handshake_is_parked_not_dropped() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        calltoken: IncomingCallTokenPolicy::Always,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let listener_addr = listener.local_addr();

    let (peer, _peer_addr) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), listener_addr)
        .unwrap();

    // The CALLTOKEN challenge arrives. Do NOT ack it (a real caller responds
    // with the token-bearing NEW, never an ACK).
    let mut buf = [0u8; 4096];
    let token: Vec<u8> = {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut tok = None;
        while Instant::now() < deadline && tok.is_none() {
            if let Ok((n, _src)) = peer.recv_from(&mut buf)
                && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
                && f.subclass == Subclass::Iax(IaxCommand::CallToken)
            {
                tok = f.ies.calltoken.map(<[u8]>::to_vec);
            }
        }
        tok.expect("CALLTOKEN challenge with a token IE")
    };

    // The offer surfaced at spawn; the app answers it NOW — before the token
    // echo. A real app cannot know the handshake state, so this must work.
    let ev = events
        .recv_timeout(Duration::from_secs(2))
        .expect("IncomingCallEvent arrives");
    let incoming = match ev {
        IncomingCallEvent::Incoming(c) => c,
        IncomingCallEvent::Answered { .. } => panic!("expected Incoming in AppDecide"),
    };
    let (call, call_events) = incoming.answer().expect("answer");

    // Now echo the token in a fresh NEW (seqnos reset, dcallno 0), as a real
    // caller does one RTT later.
    let ies = Ies {
        calltoken: Some(&token),
        ..valid_new_ies()
    };
    peer.send_to(&new_datagram(ies, PEER_CALL), listener_addr)
        .unwrap();

    // ACCEPT then ANSWER must both arrive: the parked answer is replayed the
    // moment the handshake reaches AcceptSent.
    let accept = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::Accept)
    });
    assert!(accept.is_some(), "ACCEPT arrives after the token echo");
    let answer = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.frame_type == FrameType::Control
            && f.subclass == Subclass::Control(astar_iax_core::subclass::ControlSubclass::Answer)
    });
    assert!(
        answer.is_some(),
        "ANSWER must arrive even though answer() preceded the token echo"
    );

    // And the unified call becomes fully Active (AnswerAcked → Answered).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match call_events.recv_timeout(remaining) {
            Ok(astar_iax::CallEvent::Answered { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("no CallEvent::Answered before deadline: {e}"),
        }
    }
    let _ = call.snapshot();
}
