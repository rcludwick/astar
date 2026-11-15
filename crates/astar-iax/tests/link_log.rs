// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The link log: every IAX2 link writes a short story at `info` on the target
//! `astar_iax::link` — it starts (direction, far address, claimed node), its
//! codec settles, it comes up, it ends (who hung up, the cause, how long it
//! lasted) — and a link that ends under ten seconds after coming up adds a
//! `warn`. A real node dropping its link to astar-server after a couple of
//! seconds left no trace at all before this.
//!
//! Every test captures `tracing` output through one process-wide subscriber
//! and picks out its own lines by a node number no other test uses, so the
//! tests can run in parallel. 127.0.0.1 only: no node is ever contacted.

#![allow(clippy::too_many_lines)]

use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use astar_iax::{
    CallEvent, CallMode, IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener,
    IncomingCallPolicy, IncomingCallTokenPolicy, IncomingDecisionPolicy, RawDial, dial_raw,
};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand, VoiceFormat};

// --- capturing tracing output ------------------------------------------------

/// Everything any thread logged, as the default `fmt` layer prints it.
static CAPTURED: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install the process-wide capturing subscriber once. A global one, not a
/// scoped default, because the lines are written on the call's own runtime
/// thread, never on the test's. DEBUG, so the secrets test reads every line
/// any target writes, not only the link log's.
fn capture() {
    CAPTURED.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || CaptureWriter(Arc::clone(&sink)))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        buf
    });
}

/// Every captured line so far.
fn all_lines() -> Vec<String> {
    let buf = CAPTURED.get().expect("capture() first");
    let bytes = buf.lock().expect("capture poisoned").clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

/// The link-log lines (target `astar_iax::link`) that name `node`.
fn link_lines(node: &str) -> Vec<String> {
    let needle = format!("node={node}");
    all_lines()
        .into_iter()
        .filter(|l| l.contains("astar_iax::link:"))
        .filter(|l| {
            l.split_whitespace()
                .any(|word| word == needle || word.starts_with(&format!("{needle},")))
        })
        .collect()
}

/// Wait until a link-log line for `node` contains every one of `parts`.
fn wait_for_line(node: &str, parts: &[&str], within: Duration) -> String {
    let deadline = Instant::now() + within;
    loop {
        if let Some(l) = link_lines(node)
            .into_iter()
            .find(|l| parts.iter().all(|p| l.contains(p)))
        {
            return l;
        }
        assert!(
            Instant::now() < deadline,
            "no link line for node {node} containing {parts:?}; lines were:\n{}",
            link_lines(node).join("\n")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// --- a link between the engine's own two ends ----------------------------------

/// One link on loopback: the engine dials (`from_node` → `to_node`) into the
/// engine's own listener, which auto-answers. Both ends are up on return.
struct LinkPair {
    listener: IncomingCallListener,
    out: RawDial,
    inbound: astar_iax::Call,
    inbound_events: std::sync::mpsc::Receiver<CallEvent>,
}

fn auto_listener(
    policy: IncomingCallPolicy,
) -> (
    IncomingCallListener,
    std::sync::mpsc::Receiver<IncomingCallEvent>,
) {
    IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts")
}

fn wait_answered(events: &std::sync::mpsc::Receiver<CallEvent>, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(CallEvent::Answered { .. }) => return,
            Ok(CallEvent::Hangup { reason }) => panic!("{what} hung up early: {reason}"),
            Ok(_) => {}
            Err(e) => panic!("{what} never answered: {e}"),
        }
    }
}

fn link_pair(from_node: &str, to_node: &str, secret: &str, policy: IncomingCallPolicy) -> LinkPair {
    let (listener, events) = auto_listener(policy);
    let out = dial_raw(
        listener.local_addr(),
        from_node,
        to_node,
        secret,
        CallMode::NodeLink {
            node: from_node.into(),
        },
    )
    .expect("dial");
    let (inbound, inbound_events) = match events.recv_timeout(Duration::from_secs(8)) {
        Ok(IncomingCallEvent::Answered { call, events }) => (call, events),
        Ok(IncomingCallEvent::Incoming(_)) => panic!("AutoAccept yields Answered"),
        Err(e) => panic!("the call never reached the listener: {e}"),
    };
    wait_answered(&out.events, "the outbound leg");
    wait_answered(&inbound_events, "the inbound leg");
    LinkPair {
        listener,
        out,
        inbound,
        inbound_events,
    }
}

fn open_policy() -> IncomingCallPolicy {
    IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Off,
        calltoken: IncomingCallTokenPolicy::Never,
        ..IncomingCallPolicy::default()
    }
}

/// Our call number on the hand-rolled peers.
const SERVER_CALL: u16 = 9;
/// The hand-rolled caller's call number in the secrets test.
const PEER_CALL: u16 = 4242;
/// The secrets the secrets test hands the engine; no log line may hold them.
const SECRET_IN: &str = "inbound-s3cret-7f2a";
const SECRET_OUT: &str = "outbound-s3cret-91cd";

// --- the tests ---------------------------------------------------------------

#[test]
fn an_outbound_link_logs_its_start_codec_and_up() {
    capture();
    let pair = link_pair("2101", "2102", "", open_policy());

    let start = wait_for_line(
        "2102",
        &["link starting", "direction=out"],
        Duration::from_secs(2),
    );
    assert!(start.contains(" INFO "), "{start}");
    assert!(
        start.contains(&format!("peer={}", pair.listener.local_addr())),
        "the far address is the listener we dialled: {start}"
    );
    let codec = wait_for_line(
        "2102",
        &["link codec settled", "direction=out"],
        Duration::from_secs(2),
    );
    assert!(codec.contains("codec=ulaw"), "{codec}");
    let up = wait_for_line(
        "2102",
        &["link up", "direction=out"],
        Duration::from_secs(2),
    );
    assert!(up.contains(" INFO "), "{up}");
    drop(pair);
}

#[test]
fn an_inbound_link_logs_its_start_codec_and_up() {
    capture();
    let pair = link_pair("2201", "2202", "", open_policy());

    // The inbound leg names the node the caller claims to be.
    let start = wait_for_line(
        "2201",
        &["link starting", "direction=in"],
        Duration::from_secs(2),
    );
    assert!(start.contains("peer=127.0.0.1:"), "{start}");
    let codec = wait_for_line(
        "2201",
        &["link codec settled", "direction=in"],
        Duration::from_secs(2),
    );
    assert!(codec.contains("codec=ulaw"), "{codec}");
    wait_for_line("2201", &["link up", "direction=in"], Duration::from_secs(2));
    drop(pair);
}

#[test]
fn a_local_hangup_is_logged_as_ours_and_the_far_end_logs_it_as_theirs() {
    capture();
    let pair = link_pair("2301", "2302", "", open_policy());
    let LinkPair {
        listener: _listener,
        out,
        inbound,
        inbound_events,
    } = pair;

    out.call
        .hangup(Some("link test over".into()))
        .expect("hangup");

    // Our side: we hung up, with the cause we gave.
    let ours = wait_for_line(
        "2302",
        &["link ended", "direction=out"],
        Duration::from_secs(5),
    );
    assert!(ours.contains("ended_by=us"), "{ours}");
    assert!(ours.contains("link test over"), "{ours}");
    assert!(ours.contains("duration_s="), "{ours}");

    // Their side: the HANGUP came from the peer, with its CAUSE text.
    let theirs = wait_for_line(
        "2301",
        &["link ended", "direction=in"],
        Duration::from_secs(5),
    );
    assert!(theirs.contains("ended_by=them"), "{theirs}");
    assert!(theirs.contains("link test over"), "{theirs}");
    drop((inbound, inbound_events));
}

#[test]
fn a_link_that_ends_within_ten_seconds_also_logs_a_warning() {
    capture();
    let pair = link_pair("2401", "2402", "", open_policy());
    pair.out.call.hangup(None).expect("hangup");

    let warn = wait_for_line(
        "2402",
        &["short link", "direction=out"],
        Duration::from_secs(5),
    );
    assert!(warn.contains(" WARN "), "{warn}");
    assert!(warn.contains("duration_s="), "{warn}");
    let far = wait_for_line(
        "2401",
        &["short link", "direction=in"],
        Duration::from_secs(5),
    );
    assert!(far.contains(" WARN "), "{far}");
    // Exactly one end line each: the warning is extra, not a replacement.
    let ends = link_lines("2402")
        .into_iter()
        .filter(|l| l.contains("link ended"))
        .count();
    assert_eq!(ends, 1, "one end line for the outbound leg");
    drop((pair.inbound, pair.inbound_events));
}

/// A peer that answers and then goes silent: the engine's PINGs are never
/// acknowledged, its retransmissions give up, and the link ends as a time-out.
/// Slow by nature (the retransmission ladder runs about 15 s).
#[test]
fn a_silent_peer_ends_the_link_as_a_time_out() {
    capture();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let out = dial_raw(
        peer_addr,
        "2501",
        "2502",
        "",
        CallMode::NodeLink {
            node: "2501".into(),
        },
    )
    .expect("dial");

    // Answer the NEW with an ACCEPT, ACK it, and then say nothing more.
    let mut buf = [0u8; 4096];
    let (n, src) = peer.recv_from(&mut buf).expect("the NEW arrives");
    let Ok(Frame::Full(new)) = parse_lenient(&buf[..n]) else {
        panic!("a full frame");
    };
    assert_eq!(new.subclass, Subclass::Iax(IaxCommand::New));
    peer.send_to(&ack_for(&new, SERVER_CALL), src).unwrap();
    let accept = enc(&Frame::Full(Box::new(FullFrame {
        source_call: SERVER_CALL,
        dest_call: new.source_call,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: new.oseqno.wrapping_add(1),
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Accept),
        ies: Ies {
            format: Some(VoiceFormat::G711U.as_u32()),
            ..Ies::empty()
        },
        payload: &[],
    })));
    peer.send_to(&accept, src).unwrap();
    wait_answered(&out.events, "the outbound leg");
    wait_for_line(
        "2502",
        &["link up", "direction=out"],
        Duration::from_secs(2),
    );

    let end = wait_for_line(
        "2502",
        &["link ended", "direction=out"],
        Duration::from_secs(45),
    );
    assert!(end.contains("ended_by=timeout"), "{end}");
    assert!(end.contains("timed out"), "{end}");
    drop(out);
    drop(peer);
}

#[test]
fn no_line_carries_the_secret_or_the_call_token() {
    capture();

    // (a) A hand-rolled caller, so the test knows the call token the listener
    // hands out: NEW → CALLTOKEN → NEW+token → AUTHREQ → AUTHREP → ACCEPT →
    // ANSWER, then it hangs up.
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Required,
        calltoken: IncomingCallTokenPolicy::Always,
        credential_resolver: Some(Arc::new(|u: &str| {
            if u == "2601" {
                SECRET_IN.to_string()
            } else {
                String::new()
            }
        })),
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = auto_listener(policy);
    let listener_addr = listener.local_addr();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let offer = |token: Option<&[u8]>| {
        let ies = Ies {
            called_number: Some("2602"),
            calling_number: Some("2601"),
            username: Some("2601"),
            capability: Some(VoiceFormat::G711U.as_u32()),
            format: Some(VoiceFormat::G711U.as_u32()),
            version: Some(2),
            calltoken: Some(token.unwrap_or(b"")),
            ..Ies::empty()
        };
        enc(&Frame::Full(Box::new(FullFrame {
            source_call: PEER_CALL,
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
    };
    peer.send_to(&offer(None), listener_addr).unwrap();
    let token: Vec<u8> = {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut tok = None;
        while Instant::now() < deadline && tok.is_none() {
            let mut buf = [0u8; 4096];
            if let Ok((n, _)) = peer.recv_from(&mut buf)
                && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
                && f.subclass == Subclass::Iax(IaxCommand::CallToken)
            {
                tok = f.ies.calltoken.map(<[u8]>::to_vec);
            }
        }
        tok.expect("a CALLTOKEN challenge")
    };
    let token_text = String::from_utf8_lossy(&token).to_string();
    assert!(token_text.len() >= 8, "a real token: {token_text:?}");
    peer.send_to(&offer(Some(&token)), listener_addr).unwrap();
    let authreq = recv_until(&peer, listener_addr, PEER_CALL, |f| {
        f.subclass == Subclass::Iax(IaxCommand::AuthReq)
    })
    .expect("AUTHREQ");
    let Ok(Frame::Full(f)) = parse_lenient(&authreq) else {
        panic!("AUTHREQ parses");
    };
    let challenge = f.ies.challenge.expect("a challenge").to_string();
    let leg_call = f.source_call;
    let md5 = astar_iax_core::session::auth::md5_response(&challenge, SECRET_IN);
    let auth_reply = enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: leg_call,
        retransmission: false,
        timestamp: 0,
        oseqno: 1,
        iseqno: 1,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::AuthRep),
        ies: Ies {
            md5_result: Some(&md5),
            ..Ies::empty()
        },
        payload: &[],
    })));
    peer.send_to(&auth_reply, listener_addr).unwrap();
    let mut next_in = 0u8;
    recv_until(&peer, listener_addr, PEER_CALL, |f| {
        next_in = f.oseqno.wrapping_add(1);
        f.frame_type == FrameType::Control
            && f.subclass == Subclass::Control(ControlSubclass::Answer)
    })
    .expect("ANSWER");
    let Ok(IncomingCallEvent::Answered {
        call,
        events: leg_events,
    }) = events.recv_timeout(Duration::from_secs(2))
    else {
        panic!("the authenticated leg is answered");
    };
    wait_for_line(
        "2601",
        &["link starting", "direction=in"],
        Duration::from_secs(2),
    );
    wait_for_line("2601", &["link up", "direction=in"], Duration::from_secs(2));
    let hangup = enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: leg_call,
        retransmission: false,
        timestamp: 200,
        oseqno: 2,
        iseqno: next_in,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Hangup),
        ies: Ies::empty(),
        payload: &[],
    })));
    peer.send_to(&hangup, listener_addr).unwrap();
    wait_for_line(
        "2601",
        &["link ended", "direction=in"],
        Duration::from_secs(5),
    );
    drop((call, leg_events));

    // (b) The engine dials with a secret into a listener that demands it.
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Required,
        calltoken: IncomingCallTokenPolicy::Never,
        credential_resolver: Some(Arc::new(|u: &str| {
            if u == "2603" {
                SECRET_OUT.to_string()
            } else {
                String::new()
            }
        })),
        ..IncomingCallPolicy::default()
    };
    let pair = link_pair("2603", "2604", SECRET_OUT, policy);
    pair.out.call.hangup(None).expect("hangup");
    wait_for_line(
        "2604",
        &["link ended", "direction=out"],
        Duration::from_secs(5),
    );
    wait_for_line(
        "2603",
        &["link ended", "direction=in"],
        Duration::from_secs(5),
    );
    drop((pair.inbound, pair.inbound_events));

    // Every line any target wrote, not only the link log's.
    for line in all_lines() {
        for needle in [SECRET_IN, SECRET_OUT, token_text.as_str(), md5.as_str()] {
            assert!(
                !line.contains(needle),
                "a log line leaks {needle:?}: {line}"
            );
        }
    }
    drop(listener);
}

/// A caller that never answers the CALLTOKEN challenge, as a scanner or a
/// spoofed source never can: the listener answers it inline and keeps
/// nothing, so there is no leg, no offer and no link-log line at all.
#[test]
fn a_caller_that_never_echoes_the_call_token_costs_nothing_and_logs_nothing() {
    capture();
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        calltoken: IncomingCallTokenPolicy::Always,
        ..IncomingCallPolicy::default()
    };
    let (listener, events) = auto_listener(policy);
    let to = listener.local_addr();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    peer.send_to(&new_offer(PEER_CALL, "2701", None), to)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 4096];
    let mut challenged = false;
    while !challenged && Instant::now() < deadline {
        if let Ok((n, _)) = peer.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
        {
            challenged = f.subclass == Subclass::Iax(IaxCommand::CallToken);
        }
    }
    assert!(challenged, "a CALLTOKEN challenge");
    assert!(
        events.recv_timeout(Duration::from_millis(500)).is_err(),
        "no offer for an unverified caller"
    );
    let lines = link_lines("2701");
    assert!(lines.is_empty(), "no link-log line at any level: {lines:?}");
    drop(listener);
}

/// What a caller puts in its NEW and its HANGUP is not ours to print raw: a
/// newline would forge a log line, and ESC sequences repaint a terminal.
#[test]
fn peer_supplied_text_is_cleaned_before_it_is_logged() {
    capture();
    let (listener, events) = auto_listener(open_policy());
    let to = listener.local_addr();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    peer.send_to(&new_offer(PEER_CALL, "1999\nWARN fake", None), to)
        .unwrap();
    let mut leg_call = 0;
    let mut next_in = 0u8;
    recv_until(&peer, to, PEER_CALL, |f| {
        leg_call = f.source_call;
        next_in = f.oseqno.wrapping_add(1);
        f.frame_type == FrameType::Control
            && f.subclass == Subclass::Control(ControlSubclass::Answer)
    })
    .expect("ANSWER");
    let Ok(IncomingCallEvent::Answered { call, events: ce }) =
        events.recv_timeout(Duration::from_secs(2))
    else {
        panic!("auto-accept yields Answered");
    };
    let start = wait_for_line("1999?WARN?fake", &["link starting"], Duration::from_secs(2));
    assert!(start.contains(" INFO "), "{start}");

    let long_cause = format!("bye\u{1b}[2J{}", "x".repeat(200));
    let hangup = enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: leg_call,
        retransmission: false,
        timestamp: 100,
        oseqno: 1,
        iseqno: next_in,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Hangup),
        ies: Ies {
            cause: Some(&long_cause),
            ..Ies::empty()
        },
        payload: &[],
    })));
    peer.send_to(&hangup, to).unwrap();
    let end = wait_for_line("1999?WARN?fake", &["link ended"], Duration::from_secs(3));
    assert!(end.contains("bye?[2Jxxx"), "ESC became ?: {end}");
    let cause = end
        .split("cause=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap();
    assert_eq!(cause.chars().count(), 64, "cut to 64 characters: {cause:?}");

    for line in all_lines() {
        assert!(!line.starts_with("WARN fake"), "a forged line: {line}");
        assert!(!line.contains('\u{1b}'), "a raw ESC: {line:?}");
    }
    drop((call, ce, listener));
}

/// Our hangup to an `app_rpt` node is held while it reads `!!DISCONNECT!!`.
/// The node's HANGUP inside that window only answers ours: we started the
/// teardown on the wire, so the link log says `us`, with our cause.
#[test]
fn a_peer_hangup_during_our_held_hangup_is_ours() {
    let end = app_rpt_link_ends("2801", "2802", true);
    assert!(end.contains("ended_by=us"), "{end}");
    assert!(end.contains("cause=\"ours\""), "{end}");
}

/// The same `app_rpt` link, but the node hangs up while we had sent nothing
/// toward ending it: that end is `them`, with their cause.
#[test]
fn a_peer_hangup_with_nothing_from_us_is_theirs() {
    let end = app_rpt_link_ends("2803", "2804", false);
    assert!(end.contains("ended_by=them"), "{end}");
    assert!(end.contains("cause=\"theirs\""), "{end}");
}

/// An outbound node link from `from` to `to` whose far end is an `app_rpt`
/// node (it sends `!NEWKEY1!`). With `ours_first`, we hang up and the node
/// hangs up inside our `!!DISCONNECT!!` grace; without, the node hangs up
/// unprompted. Returns the `link ended` line.
fn app_rpt_link_ends(from: &str, to: &str, ours_first: bool) -> String {
    capture();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let raw = dial_raw(
        peer.local_addr().unwrap(),
        from,
        to,
        "",
        CallMode::NodeLink { node: from.into() },
    )
    .expect("dial");
    let mut buf = [0u8; 4096];
    let (n, from) = peer.recv_from(&mut buf).expect("NEW");
    let Ok(Frame::Full(new)) = parse_lenient(&buf[..n]) else {
        panic!("a full frame");
    };
    let astar = new.source_call;
    let send = |oseqno: u8,
                iseqno: u8,
                frame_type: FrameType,
                subclass: Subclass,
                ies: Ies<'_>,
                payload: &[u8]| {
        peer.send_to(
            &enc(&Frame::Full(Box::new(FullFrame {
                source_call: SERVER_CALL,
                dest_call: astar,
                retransmission: false,
                timestamp: 10 + u32::from(oseqno),
                oseqno,
                iseqno,
                frame_type,
                subclass,
                ies,
                payload,
            }))),
            from,
        )
        .unwrap();
    };
    send(
        0,
        1,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Accept),
        Ies {
            format: Some(VoiceFormat::G711U.as_u32()),
            ..Ies::empty()
        },
        &[],
    );
    send(
        1,
        1,
        FrameType::Control,
        Subclass::Control(ControlSubclass::Answer),
        Ies::empty(),
        &[],
    );
    wait_answered(&raw.events, "the outbound leg");
    // The node's !NEWKEY1!, as app_rpt opens every link: now it is an app_rpt peer.
    send(
        2,
        1,
        FrameType::Text,
        Subclass::Raw(0),
        Ies::empty(),
        b"!NEWKEY1!\0",
    );
    std::thread::sleep(Duration::from_millis(300));
    while peer.recv_from(&mut buf).is_ok() {}

    if !ours_first {
        // Nothing from us: the node just hangs up.
        send(
            3,
            1,
            FrameType::Iax,
            Subclass::Iax(IaxCommand::Hangup),
            Ies {
                cause: Some("theirs"),
                ..Ies::empty()
            },
            &[],
        );
        return wait_for_line(to, &["link ended", "direction=out"], Duration::from_secs(2));
    }

    // Our hangup: !!DISCONNECT!! now, the HANGUP held 250 ms behind it.
    let call = raw.call;
    let hanging = std::thread::spawn(move || call.hangup(Some("ours".into())));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut disc_oseq = None;
    while disc_oseq.is_none() && Instant::now() < deadline {
        if let Ok((n, _)) = peer.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
            && f.frame_type == FrameType::Text
            && f.payload.starts_with(b"!!DISCONNECT!!")
        {
            disc_oseq = Some(f.oseqno);
        }
    }
    let disc_oseq = disc_oseq.expect("!!DISCONNECT!! goes first");
    // Inside the window, the node hangs up first.
    send(
        3,
        disc_oseq.wrapping_add(1),
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Hangup),
        Ies {
            cause: Some("theirs"),
            ..Ies::empty()
        },
        &[],
    );
    hanging.join().expect("hangup thread").expect("hangup");
    wait_for_line(to, &["link ended", "direction=out"], Duration::from_secs(2))
}

/// A NEW offer from a hand-rolled caller, with or without an echoed token.
fn new_offer(source_call: u16, calling: &str, token: Option<&[u8]>) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies: Ies {
            called_number: Some("1998"),
            calling_number: Some(calling),
            capability: Some(VoiceFormat::G711U.as_u32()),
            format: Some(VoiceFormat::G711U.as_u32()),
            version: Some(2),
            calltoken: Some(token.unwrap_or(b"")),
            ..Ies::empty()
        },
        payload: &[],
    })))
}

// --- frame helpers -------------------------------------------------------------

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode(frame, &mut bytes).expect("encode");
    bytes
}

fn ack_for(received: &FullFrame<'_>, my_call: u16) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call: my_call,
        dest_call: received.source_call,
        retransmission: false,
        timestamp: received.timestamp,
        oseqno: received.iseqno,
        iseqno: received.oseqno.wrapping_add(1),
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Ack),
        ies: Ies::empty(),
        payload: &[],
    })))
}

/// Receive frames until `pred` matches one, acknowledging every non-ACK full frame so
/// the listener's reliability releases.
fn recv_until<F>(sock: &UdpSocket, to: SocketAddr, my_call: u16, mut pred: F) -> Option<Vec<u8>>
where
    F: FnMut(&FullFrame<'_>) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let bytes = buf[..n].to_vec();
            let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
                continue;
            };
            if !matches!(f.subclass, Subclass::Iax(IaxCommand::Ack)) {
                let _ = sock.send_to(&ack_for(&f, my_call), to);
            }
            if pred(&f) {
                return Some(bytes);
            }
        }
    }
    None
}
