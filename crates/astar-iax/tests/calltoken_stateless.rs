// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The CALLTOKEN round holds nothing per unverified source.
//!
//! Under a policy that requires call tokens, a NEW without a token is
//! answered inline with CALLTOKEN: no leg, no thread, no call number, no
//! offer to the app. The token is a keyed hash over the caller's address,
//! its source call number and the time, so only a NEW that echoes a fresh
//! token from that same address and call number gets a leg. A forged source
//! never sees its token, so it can never cost the node anything.
//!
//! 127.0.0.1 only.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_iax::{
    CallEvent, IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener, IncomingCallPolicy,
    IncomingCallTokenPolicy, IncomingDecisionPolicy,
};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand, VoiceFormat};

fn listener(
    decision: IncomingDecisionPolicy,
) -> (
    IncomingCallListener,
    std::sync::mpsc::Receiver<IncomingCallEvent>,
) {
    IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            decision,
            auth: IncomingAuthPolicy::Off,
            calltoken: IncomingCallTokenPolicy::Always,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts")
}

fn caller() -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    s
}

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    encode(frame, &mut b).expect("encode");
    b
}

fn new_frame(source_call: u16, token: Option<&[u8]>) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call: 0,
        retransmission: false,
        timestamp: 3,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies: Ies {
            called_number: Some("1998"),
            calling_number: Some("1999"),
            capability: Some(VoiceFormat::G711U.as_u32()),
            format: Some(VoiceFormat::G711U.as_u32()),
            version: Some(2),
            calltoken: Some(token.unwrap_or(b"")),
            ..Ies::empty()
        },
        payload: &[],
    })))
}

/// The first full frame back within 1 s: (command or control, its source
/// call, the CALLTOKEN IE, the CAUSE IE, the whole frame's oseqno).
#[derive(Debug)]
struct Reply {
    subclass: Subclass,
    source_call: u16,
    oseqno: u8,
    token: Option<Vec<u8>>,
    cause: Option<String>,
}

fn reply(sock: &UdpSocket) -> Option<Reply> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok((n, _)) = sock.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
        {
            if f.subclass == Subclass::Iax(IaxCommand::Ack) {
                continue;
            }
            return Some(Reply {
                subclass: f.subclass,
                source_call: f.source_call,
                oseqno: f.oseqno,
                token: f.ies.calltoken.map(<[u8]>::to_vec),
                cause: f.ies.cause.map(str::to_string),
            });
        }
    }
    None
}

/// Send a token-less NEW and return the token the CALLTOKEN carries.
fn token_for(sock: &UdpSocket, to: SocketAddr, source_call: u16) -> Vec<u8> {
    sock.send_to(&new_frame(source_call, None), to).unwrap();
    let r = reply(sock).expect("a reply to the token-less NEW");
    assert_eq!(r.subclass, Subclass::Iax(IaxCommand::CallToken), "{r:?}");
    r.token.expect("CALLTOKEN carries a token")
}

fn no_offer(events: &std::sync::mpsc::Receiver<IncomingCallEvent>, why: &str) {
    assert!(
        events.recv_timeout(Duration::from_millis(300)).is_err(),
        "an offer reached the app: {why}"
    );
}

/// (a) + (e): a flood of token-less NEWs from many source ports costs
/// nothing: each gets a CALLTOKEN, no offer reaches the app, and the first
/// real caller afterwards gets call number 1, so none were allocated.
#[test]
fn token_less_news_allocate_nothing_and_surface_no_offer() {
    let (l, events) = listener(IncomingDecisionPolicy::AppDecide);
    let to = l.local_addr();
    for i in 0..64u16 {
        let s = caller();
        let token = token_for(&s, to, 1000 + i);
        assert_ne!(token, Vec::<u8>::new());
        assert!(
            token.iter().all(|b| (0x21..=0x7e).contains(b)),
            "printable, no NUL: {token:?}"
        );
    }
    no_offer(&events, "64 unverified NEWs");

    // A caller that echoes its token gets a leg, and it is call number 1.
    let s = caller();
    let token = token_for(&s, to, 4242);
    s.send_to(&new_frame(4242, Some(&token)), to).unwrap();
    let accept = reply(&s).expect("ACCEPT");
    assert_eq!(
        accept.subclass,
        Subclass::Iax(IaxCommand::Accept),
        "{accept:?}"
    );
    assert_eq!(accept.source_call, 1, "the flood allocated no call number");
    match events.recv_timeout(Duration::from_secs(2)) {
        Ok(IncomingCallEvent::Incoming(_)) => {}
        Ok(IncomingCallEvent::Answered { .. }) => panic!("AppDecide yields Incoming"),
        Err(e) => panic!("the verified caller's offer never surfaced: {e}"),
    }
    no_offer(&events, "exactly one offer");
}

/// A retransmitted token-less NEW (its CALLTOKEN was lost) gets a CALLTOKEN
/// again, never a REJECT.
#[test]
fn a_retransmitted_token_less_new_gets_calltoken_again() {
    let (l, _events) = listener(IncomingDecisionPolicy::AutoAccept);
    let s = caller();
    let first = token_for(&s, l.local_addr(), 77);
    let again = token_for(&s, l.local_addr(), 77);
    assert_ne!(first, Vec::<u8>::new());
    assert_ne!(again, Vec::<u8>::new());
}

/// (b): a token is good only for the address and call number it was issued
/// to. Replayed from another port, or with another call number, it is
/// refused with REJECT and costs nothing.
#[test]
fn a_token_replayed_from_elsewhere_is_refused() {
    let (l, events) = listener(IncomingDecisionPolicy::AppDecide);
    let to = l.local_addr();
    let a = caller();
    let token = token_for(&a, to, 500);

    let b = caller();
    b.send_to(&new_frame(500, Some(&token)), to).unwrap();
    let r = reply(&b).expect("a reply to the replay");
    assert_eq!(r.subclass, Subclass::Iax(IaxCommand::Reject), "{r:?}");
    assert!(r.cause.is_some(), "{r:?}");

    a.send_to(&new_frame(501, Some(&token)), to).unwrap();
    let r = reply(&a).expect("a reply to the other call number");
    assert_eq!(r.subclass, Subclass::Iax(IaxCommand::Reject), "{r:?}");

    a.send_to(
        &new_frame(500, Some(b"0?00000000000000000000000000000000")),
        to,
    )
    .unwrap();
    let r = reply(&a).expect("a reply to a forged token");
    assert_eq!(r.subclass, Subclass::Iax(IaxCommand::Reject), "{r:?}");
    no_offer(&events, "every token was refused");
}

/// (d): the whole round, and the call comes up.
#[test]
fn the_token_round_completes_and_the_call_comes_up() {
    let (l, events) = listener(IncomingDecisionPolicy::AutoAccept);
    let to = l.local_addr();
    let s = caller();
    let token = token_for(&s, to, 900);
    s.send_to(&new_frame(900, Some(&token)), to).unwrap();
    // A retransmit of the token NEW goes to the same leg, never a second one.
    s.send_to(&new_frame(900, Some(&token)), to).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 4096];
    let mut answered_on_wire = false;
    while !answered_on_wire && Instant::now() < deadline {
        if let Ok((n, _)) = s.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
        {
            if f.subclass != Subclass::Iax(IaxCommand::Ack) {
                let ack = enc(&Frame::Full(Box::new(FullFrame {
                    source_call: 900,
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
                s.send_to(&ack, to).unwrap();
            }
            answered_on_wire = f.subclass == Subclass::Control(ControlSubclass::Answer);
        }
    }
    assert!(answered_on_wire, "ANSWER after the token round");
    let Ok(IncomingCallEvent::Answered {
        call: _call,
        events: ce,
    }) = events.recv_timeout(Duration::from_secs(2))
    else {
        panic!("the call is delivered");
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match ce.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(CallEvent::Answered { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("never Answered: {e}"),
        }
    }
    assert!(
        events.recv_timeout(Duration::from_millis(300)).is_err(),
        "one leg, however often the token NEW arrives"
    );
}

/// A policy without call tokens is untouched: the NEW is answered at once.
#[test]
fn without_a_token_policy_the_new_is_accepted_at_once() {
    let (l, _events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            decision: IncomingDecisionPolicy::AutoAccept,
            auth: IncomingAuthPolicy::Off,
            calltoken: IncomingCallTokenPolicy::Never,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts");
    let s = caller();
    s.send_to(&new_frame(31, None), l.local_addr()).unwrap();
    let r = reply(&s).expect("a reply");
    assert_eq!(r.subclass, Subclass::Iax(IaxCommand::Accept), "{r:?}");
    let _ = r.oseqno;
}
