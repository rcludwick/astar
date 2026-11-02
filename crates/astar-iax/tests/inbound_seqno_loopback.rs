// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! iax-cnlink: inbound-leg sequence numbering and stale-frame demux, against
//! a hand-driven peer on 127.0.0.1 that behaves like `chan_iax2`.
//!
//! The nodebench rig showed a node-dialled re-link to astar-server dropping
//! 1.2-2.5 s after it came up. Two defects compound:
//!
//! 1. astar-server's ACKs on an inbound leg ran one behind: a peer frame with
//!    `oseqno N` was acknowledged with `iseqno N` instead of `N + 1`. The
//!    opening NEW (oseqno 0) was consumed by the listener's demux or the
//!    CALLTOKEN reset, so it was never counted. The last frame of every call
//!    (the peer's HANGUP) was therefore never acknowledged, and `chan_iax2`
//!    retransmitted it.
//! 2. The retransmitted HANGUP carries the OLD call's source call number but
//!    our (immediately reused) call number as its destination. The listener
//!    routed it by destination alone, so it tore down the NEW call.
//!
//! Nothing here binds 4569 or leaves loopback.

#![allow(clippy::too_many_lines)]

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

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode(frame, &mut bytes).expect("encode");
    bytes
}

#[allow(clippy::too_many_arguments)]
fn full(
    source_call: u16,
    dest_call: u16,
    oseqno: u8,
    iseqno: u8,
    retransmission: bool,
    frame_type: FrameType,
    subclass: Subclass,
    ies: Ies<'_>,
) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call,
        retransmission,
        timestamp: 0,
        oseqno,
        iseqno,
        frame_type,
        subclass,
        ies,
        payload: &[],
    })))
}

fn new_ies(token: Option<&[u8]>) -> Ies<'_> {
    Ies {
        called_number: Some("1998"),
        calling_number: Some("1999"),
        capability: Some(VoiceFormat::G711U.as_u32()),
        format: Some(VoiceFormat::G711U.as_u32()),
        version: Some(2),
        calltoken: Some(token.unwrap_or(b"")),
        ..Ies::empty()
    }
}

fn listener(
    calltoken: IncomingCallTokenPolicy,
) -> (
    IncomingCallListener,
    std::sync::mpsc::Receiver<IncomingCallEvent>,
) {
    IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            decision: IncomingDecisionPolicy::AutoAccept,
            auth: IncomingAuthPolicy::Off,
            calltoken,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts")
}

fn peer_socket() -> UdpSocket {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    sock
}

/// An owned view of the header fields a test asserts on.
#[derive(Debug, Clone)]
struct Seen {
    source_call: u16,
    dest_call: u16,
    oseqno: u8,
    iseqno: u8,
    frame_type: FrameType,
    subclass: Subclass,
    calltoken: Option<Vec<u8>>,
}

/// Receive one full frame, or `None` once `deadline` passes.
fn recv_one(sock: &UdpSocket, deadline: Instant) -> Option<Seen> {
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok((n, _)) = sock.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
        {
            return Some(Seen {
                source_call: f.source_call,
                dest_call: f.dest_call,
                oseqno: f.oseqno,
                iseqno: f.iseqno,
                frame_type: f.frame_type,
                subclass: f.subclass,
                calltoken: f.ies.calltoken.map(<[u8]>::to_vec),
            });
        }
    }
    None
}

/// Receive full frames for `wait`, collecting each. Does NOT ack: the tests
/// drive acknowledgement explicitly, like `chan_iax2` would.
fn collect(sock: &UdpSocket, wait: Duration) -> Vec<Seen> {
    let deadline = Instant::now() + wait;
    std::iter::from_fn(|| recv_one(sock, deadline)).collect()
}

/// Receive until `pred` matches (up to 2 s), returning the match and
/// everything seen before it.
fn until(sock: &UdpSocket, mut pred: impl FnMut(&Seen) -> bool) -> (Option<Seen>, Vec<Seen>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    while let Some(s) = recv_one(sock, deadline) {
        if pred(&s) {
            return (Some(s), seen);
        }
        seen.push(s);
    }
    (None, seen)
}

fn is(s: &Seen, cmd: IaxCommand) -> bool {
    s.frame_type == FrameType::Iax && s.subclass == Subclass::Iax(cmd)
}

fn is_answer(s: &Seen) -> bool {
    s.frame_type == FrameType::Control && s.subclass == Subclass::Control(ControlSubclass::Answer)
}

/// ACK a frame the listener sent, the way `chan_iax2` does (iseqno = oseqno+1).
fn ack(sock: &UdpSocket, to: SocketAddr, my_call: u16, f: &Seen, my_oseqno: u8) {
    let b = full(
        my_call,
        f.source_call,
        my_oseqno,
        f.oseqno.wrapping_add(1),
        false,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Ack),
        Ies::empty(),
    );
    let _ = sock.send_to(&b, to);
}

/// Bring a call up from `my_call`: NEW (through the CALLTOKEN round when
/// `token_round`), ACCEPT, ANSWER, acknowledging like `chan_iax2`. Returns the
/// listener's call number, the ACCEPT as seen, and the unified call's events.
fn bring_up(
    sock: &UdpSocket,
    to: SocketAddr,
    events: &std::sync::mpsc::Receiver<IncomingCallEvent>,
    my_call: u16,
    token_round: bool,
) -> (
    u16,
    Seen,
    astar_iax::Call,
    std::sync::mpsc::Receiver<CallEvent>,
) {
    let new0 = full(
        my_call,
        0,
        0,
        0,
        false,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::New),
        new_ies(None),
    );
    sock.send_to(&new0, to).unwrap();
    if token_round {
        let (ct, _) = until(sock, |s| is(s, IaxCommand::CallToken));
        let token = ct.expect("CALLTOKEN").calltoken.expect("token IE");
        // chan_iax2 resend_with_token: sequence numbers restart at 0.
        let new1 = full(
            my_call,
            0,
            0,
            0,
            false,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::New),
            new_ies(Some(&token)),
        );
        sock.send_to(&new1, to).unwrap();
    }
    let (accept, _) = until(sock, |s| is(s, IaxCommand::Accept));
    let accept = accept.expect("ACCEPT");
    // The ANSWER follows; ACK it so the leg goes Active.
    let (answer, _) = until(sock, is_answer);
    let answer = answer.expect("ANSWER");
    ack(sock, to, my_call, &answer, 1);
    let ev = events
        .recv_timeout(Duration::from_secs(2))
        .expect("IncomingCallEvent");
    let IncomingCallEvent::Answered { call, events: ce } = ev else {
        panic!("auto-accept yields Answered");
    };
    // Wait for the leg to go Active (the ANSWER's ACK is processed).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match ce.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(CallEvent::Answered { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("leg never went Active: {e}"),
        }
    }
    (accept.source_call, accept, call, ce)
}

/// The opening NEW is part of the call: ACCEPT must acknowledge it with
/// iseqno 1, as `chan_iax2` does. (No CALLTOKEN round.)
#[test]
fn accept_acknowledges_the_opening_new() {
    let (l, events) = listener(IncomingCallTokenPolicy::Never);
    let to = l.local_addr();
    let sock = peer_socket();
    let (_our, accept, _call, _ce) = bring_up(&sock, to, &events, 1692, false);
    assert_eq!(
        accept.iseqno, 1,
        "ACCEPT must ack the peer's NEW (oseqno 0) with iseqno 1"
    );
}

/// Same, after the CALLTOKEN round: the token-bearing NEW restarts the peer's
/// numbering at oseqno 0, and that NEW is the first frame of the real call.
#[test]
fn accept_acknowledges_the_token_new() {
    let (l, events) = listener(IncomingCallTokenPolicy::Always);
    let to = l.local_addr();
    let sock = peer_socket();
    let (_our, accept, _call, _ce) = bring_up(&sock, to, &events, 1692, true);
    assert_eq!(
        accept.iseqno, 1,
        "ACCEPT must ack the token NEW (oseqno 0) with iseqno 1"
    );
}

/// The peer's HANGUP (here oseqno 2: NEW=0, the ACK of ANSWER does not count,
/// then PING=1, HANGUP=2) is acknowledged with iseqno 3.
#[test]
fn peer_hangup_is_acked_with_oseqno_plus_one() {
    for token_round in [false, true] {
        let policy = if token_round {
            IncomingCallTokenPolicy::Always
        } else {
            IncomingCallTokenPolicy::Never
        };
        let (l, events) = listener(policy);
        let to = l.local_addr();
        let sock = peer_socket();
        let (our, _accept, _call, _ce) = bring_up(&sock, to, &events, 1692, token_round);

        let ping_frame = full(
            1692,
            our,
            1,
            2,
            false,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::Ping),
            Ies::empty(),
        );
        sock.send_to(&ping_frame, to).unwrap();
        let (reply, seen_before) = until(&sock, |s| is(s, IaxCommand::Pong));
        let reply = reply.unwrap_or_else(|| panic!("PONG; saw {seen_before:?}"));
        assert_eq!(reply.iseqno, 2, "PONG piggyback-acks PING (oseqno 1)");
        ack(&sock, to, 1692, &reply, 2);

        let hangup = full(
            1692,
            our,
            2,
            reply.oseqno.wrapping_add(1),
            false,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::Hangup),
            Ies::empty(),
        );
        sock.send_to(&hangup, to).unwrap();
        let (a, seen) = until(&sock, |s| is(s, IaxCommand::Ack) && s.dest_call == 1692);
        let a =
            a.unwrap_or_else(|| panic!("ACK of HANGUP (token_round={token_round}); saw {seen:?}"));
        assert_eq!(
            a.iseqno, 3,
            "HANGUP oseqno 2 must be acked with iseqno 3 (token_round={token_round})"
        );
    }
}

/// A frame whose source call is not the leg's peer (a stale retransmit from
/// a previous call that used the same destination call number) must not
/// reach, or tear down, the current call. It is answered as `chan_iax2`
/// answers a frame for a call it does not know: INVAL to the stale source.
#[test]
fn stale_hangup_from_previous_call_does_not_drop_the_relink() {
    let (l, events) = listener(IncomingCallTokenPolicy::Never);
    let to = l.local_addr();
    let sock = peer_socket();

    // First link, then the peer hangs it up.
    let (our_a, _, _call_a, _ce_a) = bring_up(&sock, to, &events, 1692, false);
    let hangup = full(
        1692,
        our_a,
        1,
        2,
        false,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Hangup),
        Ies::empty(),
    );
    sock.send_to(&hangup, to).unwrap();
    let _ = collect(&sock, Duration::from_millis(400)); // leg reaped

    // Re-link from a new peer call number; astar reuses its call number.
    let (our_b, _, _call_b, ce_b) = bring_up(&sock, to, &events, 11973, false);
    assert_eq!(
        our_a, our_b,
        "precondition: the listener reuses the freed call number"
    );

    // chan_iax2 retransmits the old HANGUP (retrans bit set, old source call).
    let stale = full(
        1692,
        our_b,
        1,
        2,
        true,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Hangup),
        Ies::empty(),
    );
    sock.send_to(&stale, to).unwrap();
    let replies = collect(&sock, Duration::from_millis(300));
    assert!(
        !replies
            .iter()
            .any(|s| s.dest_call == 11973 && is(s, IaxCommand::Ack)),
        "the stale HANGUP must not be acknowledged as the new call's: {replies:?}"
    );
    assert!(
        replies
            .iter()
            .any(|s| s.dest_call == 1692 && is(s, IaxCommand::Inval)),
        "the stale HANGUP gets INVAL addressed to the old call: {replies:?}"
    );
    while let Ok(ev) = ce_b.try_recv() {
        assert!(
            !matches!(ev, CallEvent::Hangup { .. }),
            "the new call must survive the stale HANGUP"
        );
    }

    // The new link still answers: PING gets PONG, not INVAL.
    let ping_frame = full(
        11973,
        our_b,
        1,
        2,
        false,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Ping),
        Ies::empty(),
    );
    sock.send_to(&ping_frame, to).unwrap();
    let (reply, seen) = until(&sock, |s| {
        is(s, IaxCommand::Pong) || is(s, IaxCommand::Inval)
    });
    let reply = reply.unwrap_or_else(|| panic!("a reply to PING; saw {seen:?}"));
    assert!(is(&reply, IaxCommand::Pong), "new link alive: {reply:?}");
    assert_eq!(reply.dest_call, 11973);
}

/// A peer whose CALLTOKEN was lost retransmits its opening NEW, still with
/// no token. `chan_iax2` answers every token-less NEW with CALLTOKEN; the leg
/// rejected it ("Call Token Invalid") and the call never came up.
#[test]
fn tokenless_new_retransmit_gets_the_calltoken_again_not_reject() {
    let (l, events) = listener(IncomingCallTokenPolicy::Always);
    let to = l.local_addr();
    let sock = peer_socket();
    let new0 = full(
        1692,
        0,
        0,
        0,
        false,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::New),
        new_ies(None),
    );
    sock.send_to(&new0, to).unwrap();
    let (first, _) = until(&sock, |s| is(s, IaxCommand::CallToken));
    let first = first.expect("CALLTOKEN");

    // The CALLTOKEN "was lost": the peer retransmits the same NEW.
    let retrans = full(
        1692,
        0,
        0,
        0,
        true,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::New),
        new_ies(None),
    );
    sock.send_to(&retrans, to).unwrap();
    let (again, seen) = until(&sock, |s| {
        is(s, IaxCommand::CallToken) || is(s, IaxCommand::Reject)
    });
    let again = again.unwrap_or_else(|| panic!("a reply to the retransmit; saw {seen:?}"));
    assert!(
        is(&again, IaxCommand::CallToken),
        "token-less NEW retransmit must get CALLTOKEN, not {again:?}"
    );
    assert_eq!(again.calltoken, first.calltoken, "the same token");
    assert_eq!(again.oseqno, 0, "CALLTOKEN restarts nothing: oseqno 0");

    // And the token NEW still completes the call.
    let token = again.calltoken.expect("token IE");
    let new1 = full(
        1692,
        0,
        0,
        0,
        false,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::New),
        new_ies(Some(&token)),
    );
    sock.send_to(&new1, to).unwrap();
    let (accept, seen) = until(&sock, |s| {
        is(s, IaxCommand::Accept) || is(s, IaxCommand::Reject)
    });
    let accept = accept.unwrap_or_else(|| panic!("ACCEPT; saw {seen:?}"));
    assert!(is(&accept, IaxCommand::Accept), "{accept:?}");
    let _ = events;
}

/// `app_rpt`'s `ilink 1` sends `!!DISCONNECT!!` and its HANGUP in the same
/// millisecond. The HANGUP must still be acknowledged: the leg used to end
/// on the TEXT, the HANGUP went unanswered, and the node retransmitted it.
#[test]
fn hangup_right_after_disconnect_text_is_acked() {
    let (l, events) = listener(IncomingCallTokenPolicy::Never);
    let to = l.local_addr();
    let sock = peer_socket();
    let (our, _accept, _call, _ce) = bring_up(&sock, to, &events, 1692, false);
    let text = enc(&Frame::Full(Box::new(FullFrame {
        source_call: 1692,
        dest_call: our,
        retransmission: false,
        timestamp: 0,
        oseqno: 1,
        iseqno: 2,
        frame_type: FrameType::Text,
        subclass: Subclass::Raw(0),
        ies: Ies::empty(),
        payload: b"!!DISCONNECT!!\0",
    })));
    let hangup = full(
        1692,
        our,
        2,
        2,
        false,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Hangup),
        Ies::empty(),
    );
    sock.send_to(&text, to).unwrap();
    sock.send_to(&hangup, to).unwrap();
    let seen = collect(&sock, Duration::from_millis(500));
    assert!(
        seen.iter()
            .any(|s| is(s, IaxCommand::Ack) && s.dest_call == 1692 && s.iseqno == 3),
        "the HANGUP (oseqno 2) is ACKed with iseqno 3: {seen:?}"
    );
    assert!(
        !seen.iter().any(|s| is(s, IaxCommand::Inval)),
        "no INVAL: {seen:?}"
    );
}
