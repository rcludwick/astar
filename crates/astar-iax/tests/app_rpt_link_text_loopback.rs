// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `app_rpt` link conversation that rides on TEXT frames, against a
//! hand-driven peer on 127.0.0.1 that behaves like an `app_rpt` node.
//!
//! - nodebench finding 6: `app_rpt` opens every link with `!NEWKEY1!` and
//!   expects it back within 2 s; without it, it hears the link only while
//!   RADIO_KEY says so. On a node-dialled link the `!NEWKEY1!` is also what
//!   acknowledges our ANSWER, and the leg handed it to the FSM while it was
//!   still in `AnswerSent`, which dropped it.
//! - nodebench finding 3: `app_rpt` keeps a link that hangs up without
//!   `!!DISCONNECT!!` for its `DISC_TIME` (10 s), expecting it to come back,
//!   and astar's HANGUP was stamped timestamp 0.
//!
//! Nothing here binds 4569 or leaves loopback.

#![allow(clippy::too_many_lines, clippy::doc_markdown)]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_iax::{
    CallEvent, CallMode, IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener,
    IncomingCallPolicy, IncomingCallTokenPolicy, IncomingDecisionPolicy, dial_raw,
};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand, VoiceFormat};

#[allow(clippy::too_many_arguments)]
fn full(
    source_call: u16,
    dest_call: u16,
    oseqno: u8,
    iseqno: u8,
    timestamp: u32,
    frame_type: FrameType,
    subclass: Subclass,
    ies: Ies<'_>,
    payload: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode(
        &Frame::Full(Box::new(FullFrame {
            source_call,
            dest_call,
            retransmission: false,
            timestamp,
            oseqno,
            iseqno,
            frame_type,
            subclass,
            ies,
            payload,
        })),
        &mut bytes,
    )
    .expect("encode");
    bytes
}

/// A TEXT frame as `app_rpt` sends it: the string with its C terminator.
fn text(source_call: u16, dest_call: u16, oseqno: u8, iseqno: u8, body: &str) -> Vec<u8> {
    let mut payload = body.as_bytes().to_vec();
    payload.push(0);
    full(
        source_call,
        dest_call,
        oseqno,
        iseqno,
        40,
        FrameType::Text,
        Subclass::Raw(0),
        Ies::empty(),
        &payload,
    )
}

#[derive(Debug, Clone)]
struct Seen {
    source_call: u16,
    oseqno: u8,
    timestamp: u32,
    frame_type: FrameType,
    subclass: Subclass,
    payload: Vec<u8>,
}

impl Seen {
    fn is(&self, cmd: IaxCommand) -> bool {
        self.frame_type == FrameType::Iax && self.subclass == Subclass::Iax(cmd)
    }
    fn is_text(&self, body: &str) -> bool {
        self.frame_type == FrameType::Text && self.payload == body.as_bytes()
    }
    fn is_control(&self, c: ControlSubclass) -> bool {
        self.frame_type == FrameType::Control && self.subclass == Subclass::Control(c)
    }
}

fn recv_one(sock: &UdpSocket, deadline: Instant) -> Option<(Seen, SocketAddr)> {
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok((n, from)) = sock.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
        {
            return Some((
                Seen {
                    source_call: f.source_call,
                    oseqno: f.oseqno,
                    timestamp: f.timestamp,
                    frame_type: f.frame_type,
                    subclass: f.subclass,
                    payload: f.payload.to_vec(),
                },
                from,
            ));
        }
    }
    None
}

fn until(sock: &UdpSocket, wait: Duration, mut pred: impl FnMut(&Seen) -> bool) -> Vec<Seen> {
    let deadline = Instant::now() + wait;
    let mut seen = Vec::new();
    while let Some((s, _)) = recv_one(sock, deadline) {
        let hit = pred(&s);
        seen.push(s);
        if hit {
            break;
        }
    }
    seen
}

fn peer_socket() -> UdpSocket {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    sock
}

fn listener() -> (
    IncomingCallListener,
    std::sync::mpsc::Receiver<IncomingCallEvent>,
) {
    IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(IncomingCallPolicy {
            decision: IncomingDecisionPolicy::AutoAccept,
            auth: IncomingAuthPolicy::Off,
            calltoken: IncomingCallTokenPolicy::Never,
            ..IncomingCallPolicy::default()
        })
        .start()
        .expect("listener starts")
}

const PEER: u16 = 4242;

/// A node dials the listener: NEW, ACCEPT, ANSWER. Returns the listener's
/// call number, the ANSWER, and the answered call with its events.
fn node_dials_in(
    sock: &UdpSocket,
    to: SocketAddr,
    events: &std::sync::mpsc::Receiver<IncomingCallEvent>,
) -> (
    u16,
    Seen,
    astar_iax::Call,
    std::sync::mpsc::Receiver<CallEvent>,
) {
    let ies = Ies {
        called_number: Some("1998"),
        calling_number: Some("1999"),
        capability: Some(VoiceFormat::G711U.as_u32()),
        format: Some(VoiceFormat::G711U.as_u32()),
        version: Some(2),
        ..Ies::empty()
    };
    let new = full(
        PEER,
        0,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::New),
        ies,
        &[],
    );
    sock.send_to(&new, to).unwrap();
    let seen = until(sock, Duration::from_secs(2), |s| {
        s.is_control(ControlSubclass::Answer)
    });
    let answer = seen
        .last()
        .filter(|s| s.is_control(ControlSubclass::Answer))
        .cloned()
        .unwrap_or_else(|| panic!("ANSWER; saw {seen:?}"));
    let ev = events
        .recv_timeout(Duration::from_secs(2))
        .expect("IncomingCallEvent");
    let IncomingCallEvent::Answered { call, events: ce } = ev else {
        panic!("auto-accept yields Answered");
    };
    (answer.source_call, answer, call, ce)
}

/// `app_rpt` sends `!NEWKEY1!` straight after ANSWER, and that frame is what
/// acknowledges the ANSWER (its iseqno covers it): there is no bare ACK
/// first. The leg must answer it.
#[test]
fn newkey_that_acknowledges_the_answer_is_echoed() {
    let (l, events) = listener();
    let to = l.local_addr();
    let sock = peer_socket();
    let (our, answer, _call, _ce) = node_dials_in(&sock, to, &events);
    sock.send_to(
        &text(PEER, our, 1, answer.oseqno.wrapping_add(1), "!NEWKEY1!"),
        to,
    )
    .unwrap();
    let seen = until(&sock, Duration::from_secs(1), |s| s.is_text("!NEWKEY1!"));
    assert!(
        seen.iter().any(|s| s.is_text("!NEWKEY1!")),
        "the leg must send !NEWKEY1! back; saw {seen:?}"
    );
}

/// The ordinary order on ASL3: a bare ACK of the ANSWER first, then
/// `!NEWKEY1!\0` to an Active leg.
#[test]
fn newkey_with_its_c_terminator_is_echoed() {
    let (l, events) = listener();
    let to = l.local_addr();
    let sock = peer_socket();
    let (our, answer, _call, _ce) = node_dials_in(&sock, to, &events);
    let iseq = answer.oseqno.wrapping_add(1);
    sock.send_to(
        &full(
            PEER,
            our,
            1,
            iseq,
            20,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::Ack),
            Ies::empty(),
            &[],
        ),
        to,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    sock.send_to(&text(PEER, our, 1, iseq, "!NEWKEY1!"), to)
        .unwrap();
    let seen = until(&sock, Duration::from_secs(1), |s| s.is_text("!NEWKEY1!"));
    assert!(
        seen.iter().any(|s| s.is_text("!NEWKEY1!")),
        "the leg must send !NEWKEY1! back; saw {seen:?}"
    );
}

/// An inbound link, Active, from a peer that is (`app_rpt`) or is not
/// (`!app_rpt`) an app_rpt node.
fn inbound_active(
    app_rpt: bool,
) -> (
    IncomingCallListener,
    UdpSocket,
    astar_iax::Call,
    std::sync::mpsc::Receiver<CallEvent>,
) {
    let (l, events) = listener();
    let to = l.local_addr();
    let sock = peer_socket();
    let (our, answer, call, ce) = node_dials_in(&sock, to, &events);
    let iseq = answer.oseqno.wrapping_add(1);
    // A bare ACK of the ANSWER, so the leg goes Active.
    sock.send_to(
        &full(
            PEER,
            our,
            1,
            iseq,
            20,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::Ack),
            Ies::empty(),
            &[],
        ),
        to,
    )
    .unwrap();
    if app_rpt {
        // app_rpt's periodic link list marks the peer as a node.
        sock.send_to(&text(PEER, our, 1, iseq, "L "), to).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    let _ = until(&sock, Duration::from_millis(50), |_| false);
    (l, sock, call, ce)
}

/// Hanging up a link we accepted says `!!DISCONNECT!!` first, so the node
/// drops it at once instead of holding it for DISC_TIME, and the HANGUP
/// carries the call's clock, not 0.
#[test]
fn hanging_up_an_inbound_link_says_disconnect_with_a_real_timestamp() {
    let (_l, sock, call, _ce) = inbound_active(true);
    call.hangup(None).expect("hangup");
    let seen = until(&sock, Duration::from_secs(1), |s| s.is(IaxCommand::Hangup));
    assert_teardown(&seen);
}

/// A peer that is not an app_rpt node gets a plain HANGUP, at once.
#[test]
fn hanging_up_a_plain_iax_peer_sends_no_disconnect_text() {
    let (_l, sock, call, _ce) = inbound_active(false);
    call.hangup(None).expect("hangup");
    let seen = until(&sock, Duration::from_secs(1), |s| s.is(IaxCommand::Hangup));
    assert!(
        seen.iter().any(|s| s.is(IaxCommand::Hangup)),
        "HANGUP; saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|s| s.is_text("!!DISCONNECT!!")),
        "no app_rpt text to a plain peer: {seen:?}"
    );
}

/// The same for a link astar dials (the `POST /link disconnect` case).
#[test]
fn hanging_up_an_outbound_link_says_disconnect_with_a_real_timestamp() {
    let sock = peer_socket();
    let to = sock.local_addr().unwrap();
    let raw = dial_raw(
        to,
        "1998",
        "1999",
        "",
        CallMode::NodeLink {
            node: "1998".into(),
        },
    )
    .expect("dial");
    let (new, from) = recv_one(&sock, Instant::now() + Duration::from_secs(2)).expect("NEW");
    assert!(new.is(IaxCommand::New), "{new:?}");
    let astar = new.source_call;
    let accept_ies = Ies {
        format: Some(VoiceFormat::G711U.as_u32()),
        ..Ies::empty()
    };
    sock.send_to(
        &full(
            PEER,
            astar,
            0,
            1,
            3,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::Accept),
            accept_ies,
            &[],
        ),
        from,
    )
    .unwrap();
    sock.send_to(
        &full(
            PEER,
            astar,
            1,
            1,
            4,
            FrameType::Control,
            Subclass::Control(ControlSubclass::Answer),
            Ies::empty(),
            &[],
        ),
        from,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match raw
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(CallEvent::Answered { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("call never answered: {e}"),
        }
    }
    // The node's !NEWKEY1!, as app_rpt opens every link.
    sock.send_to(&text(PEER, astar, 2, 1, "!NEWKEY1!"), from)
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let _ = until(&sock, Duration::from_millis(50), |_| false);
    raw.call.hangup(None).expect("hangup");
    let seen = until(&sock, Duration::from_secs(1), |s| s.is(IaxCommand::Hangup));
    assert_teardown(&seen);
}

fn assert_teardown(seen: &[Seen]) {
    let disc = seen
        .iter()
        .position(|s| s.is_text("!!DISCONNECT!!"))
        .unwrap_or_else(|| panic!("!!DISCONNECT!! before HANGUP; saw {seen:?}"));
    let hangup = seen
        .iter()
        .position(|s| s.is(IaxCommand::Hangup))
        .unwrap_or_else(|| panic!("HANGUP; saw {seen:?}"));
    assert!(disc < hangup, "!!DISCONNECT!! goes first: {seen:?}");
    assert!(
        seen[disc].oseqno.wrapping_add(1) == seen[hangup].oseqno,
        "consecutive reliable frames: {seen:?}"
    );
    assert!(
        seen[hangup].timestamp >= 200,
        "HANGUP carries the call clock (>= 200 ms in), got {}",
        seen[hangup].timestamp
    );
    let gap = seen[hangup].timestamp.saturating_sub(seen[disc].timestamp);
    assert!(
        gap >= 200,
        "HANGUP is held while the node reads !!DISCONNECT!!: {gap} ms apart"
    );
}
