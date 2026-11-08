// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The packet log's tap on `AllStar` calls, end to end over loopback.
//!
//! Each test installs a `packet_tap_channel` on a `PacketSlot` and reads the
//! `PacketEvent`s directly. The fake node and the fake caller run a real
//! CALLTOKEN and MD5 login, so the bytes the tap must never hold really do
//! cross the socket the tap sits on.

#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use astar_audio::OutputId;
use astar_iax::manager::DialSpec;
use astar_iax::{
    Call, CallId, CallMode, CodecPolicy, IncomingAuthPolicy, IncomingCallEvent,
    IncomingCallListener, IncomingCallPolicy, IncomingCallTokenPolicy, IncomingDecisionPolicy,
    Manager, Registrar, RegistrationEvent,
};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, MiniFrame, encode, is_media, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::{Secret, md5_response, md5_verify};
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_record::packet_tap::{
    CallMeta, PacketCallId, PacketEvent, PacketNetwork, PacketSlot, packet_tap_channel,
};

mod common;

const SECRET: &str = "tap-canary-secret";
const USER: &str = "tap-canary-user";
const CHALLENGE: &str = "tap-canary-challenge";
const TOKEN: &[u8] = b"tap-canary-call-token";
const NODE_CALL: u16 = 21;
const PEER_CALL: u16 = 13885;
const WAIT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Frames.
// ---------------------------------------------------------------------------

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode(frame, &mut bytes).expect("test frame encodes");
    bytes
}

fn iax(source: u16, dest: u16, oseqno: u8, iseqno: u8, cmd: IaxCommand, ies: Ies<'_>) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call: source,
        dest_call: dest,
        retransmission: false,
        timestamp: 0,
        oseqno,
        iseqno,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(cmd),
        ies,
        payload: &[],
    })))
}

fn full_voice(source: u16, dest: u16, oseqno: u8, iseqno: u8, payload: &[u8]) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call: source,
        dest_call: dest,
        retransmission: false,
        timestamp: 20,
        oseqno,
        iseqno,
        frame_type: FrameType::Voice,
        subclass: Subclass::Voice(VoiceFormat::G711U),
        ies: Ies::empty(),
        payload,
    })))
}

fn mini_voice(source: u16, timestamp: u16, payload: &[u8]) -> Vec<u8> {
    enc(&Frame::Mini(MiniFrame {
        source_call: source,
        timestamp,
        payload,
    }))
}

/// An owned view of a full frame's fields a fake needs.
#[derive(Debug, Clone)]
struct Seen {
    source_call: u16,
    timestamp: u32,
    oseqno: u8,
    iseqno: u8,
    subclass: Subclass,
    calltoken: Option<Vec<u8>>,
    challenge: Option<String>,
    md5: Option<String>,
}

fn is(s: &Seen, cmd: IaxCommand) -> bool {
    s.subclass == Subclass::Iax(cmd)
}

/// The next full frame on `sock`, with who sent it.
fn next_full(sock: &UdpSocket, deadline: Instant) -> Option<(Seen, SocketAddr)> {
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok((n, src)) = sock.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
        {
            let seen = Seen {
                source_call: f.source_call,
                timestamp: f.timestamp,
                oseqno: f.oseqno,
                iseqno: f.iseqno,
                subclass: f.subclass,
                calltoken: f.ies.calltoken.map(<[u8]>::to_vec),
                challenge: f.ies.challenge.map(str::to_string),
                md5: f.ies.md5_result.map(str::to_string),
            };
            return Some((seen, src));
        }
    }
    None
}

fn ack(sock: &UdpSocket, to: SocketAddr, my_call: u16, f: &Seen) {
    let frame = enc(&Frame::Full(Box::new(FullFrame {
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
    let _ = sock.send_to(&frame, to);
}

fn udp() -> (UdpSocket, SocketAddr) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let addr = sock.local_addr().unwrap();
    (sock, addr)
}

// ---------------------------------------------------------------------------
// Reading the tap.
// ---------------------------------------------------------------------------

fn tapped_slot() -> (PacketSlot, Receiver<PacketEvent>) {
    let (tap, rx) = packet_tap_channel(256, 256);
    let slot = PacketSlot::default();
    slot.set(Some(tap));
    (slot, rx)
}

/// Everything the tap offers until `want` packets have arrived or time runs out.
fn until_packets(rx: &Receiver<PacketEvent>, want: usize) -> Vec<PacketEvent> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while packets(&seen).len() < want {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(ev) => seen.push(ev),
            Err(_) => break,
        }
    }
    seen
}

fn packets(events: &[PacketEvent]) -> Vec<(SocketAddr, &[u8])> {
    events
        .iter()
        .filter_map(|e| match e {
            PacketEvent::Packet { from, bytes, .. } => Some((*from, &**bytes)),
            _ => None,
        })
        .collect()
}

fn starts(events: &[PacketEvent]) -> Vec<(PacketCallId, &CallMeta)> {
    events
        .iter()
        .filter_map(|e| match e {
            PacketEvent::Start { call, meta, .. } => Some((*call, &**meta)),
            _ => None,
        })
        .collect()
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn wait_for_end(rx: &Receiver<PacketEvent>, want: PacketCallId) {
    let deadline = Instant::now() + WAIT;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PacketEvent::End { call, .. }) if call == want => return,
            Ok(_) => {}
            Err(e) => panic!("no End for the call: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// A dialled call, against a fake node.
// ---------------------------------------------------------------------------

fn dial_spec(peer: SocketAddr) -> DialSpec {
    DialSpec {
        id: CallId::from_raw(1),
        node: "1999".to_string(),
        peer,
        output: OutputId::new("out:s"),
        caller_id: USER.to_string(),
        secret: SECRET.to_string(),
        mode: CallMode::Standard,
        dest: "1999".to_string(),
        frame_observer: None,
        codec_policy: CodecPolicy::default(),
    }
}

/// Play the node's half of the login: CALLTOKEN, then MD5, then ACCEPT.
/// `accept_ies` go on the ACCEPT. Returns the caller's address and call
/// number.
fn accept_dial_with(node: &UdpSocket, accept_ies: Ies<'_>) -> (SocketAddr, u16) {
    let deadline = Instant::now() + WAIT;
    while let Some((f, src)) = next_full(node, deadline) {
        if is(&f, IaxCommand::Ack) {
            continue;
        }
        if is(&f, IaxCommand::New) && f.calltoken.as_deref() != Some(TOKEN) {
            let ies = Ies {
                calltoken: Some(TOKEN),
                ..Ies::empty()
            };
            let token = iax(9000, f.source_call, 0, 1, IaxCommand::CallToken, ies);
            node.send_to(&token, src).unwrap();
            continue;
        }
        ack(node, src, NODE_CALL, &f);
        if is(&f, IaxCommand::New) {
            let ies = Ies {
                challenge: Some(CHALLENGE),
                authmethods: Some(2),
                ..Ies::empty()
            };
            let next = f.oseqno.wrapping_add(1);
            let authreq = iax(NODE_CALL, f.source_call, 0, next, IaxCommand::AuthReq, ies);
            node.send_to(&authreq, src).unwrap();
        } else if is(&f, IaxCommand::AuthRep) {
            let md5 = f.md5.as_deref().expect("AUTHREP carries an MD5 result");
            assert!(md5_verify(CHALLENGE, SECRET, md5), "a real MD5 login");
            let next = f.oseqno.wrapping_add(1);
            let accept = iax(
                NODE_CALL,
                f.source_call,
                1,
                next,
                IaxCommand::Accept,
                accept_ies,
            );
            node.send_to(&accept, src).unwrap();
            return (src, f.source_call);
        }
    }
    panic!("the dial never reached AUTHREP");
}

fn wait_active(mgr: &Manager, id: CallId) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if mgr
            .snapshot()
            .calls
            .iter()
            .any(|c| c.id == id && c.is_active())
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the call never went active");
}

struct Dialled {
    mgr: Manager,
    id: CallId,
    node: UdpSocket,
    node_addr: SocketAddr,
    caller: SocketAddr,
    caller_call: u16,
}

fn dialled(slot: PacketSlot) -> Dialled {
    let (node, node_addr) = udp();
    dialled_as(slot, node, node_addr, dial_spec(node_addr), Ies::empty())
}

/// [`dialled`] with this spec, and these IEs on the node's ACCEPT.
fn dialled_as(
    slot: PacketSlot,
    node: UdpSocket,
    node_addr: SocketAddr,
    spec: DialSpec,
    accept_ies: Ies<'_>,
) -> Dialled {
    let mut mgr = common::null_manager();
    mgr.set_packet_slot(slot);
    let id = mgr.dial(spec).expect("dial");
    let (caller, caller_call) = accept_dial_with(&node, accept_ies);
    wait_active(&mgr, id);
    Dialled {
        mgr,
        id,
        node,
        node_addr,
        caller,
        caller_call,
    }
}

#[test]
fn a_dialled_call_logs_voice_and_no_auth() {
    let (slot, rx) = tapped_slot();
    let mut d = dialled(slot);

    let full = full_voice(NODE_CALL, d.caller_call, 2, 3, &[0x11; 160]);
    d.node.send_to(&full, d.caller).unwrap();
    let mut sent = vec![full];
    for i in 0..4u8 {
        let mini = mini_voice(NODE_CALL, 40 + u16::from(i) * 20, &[0x20 + i; 160]);
        d.node.send_to(&mini, d.caller).unwrap();
        sent.push(mini);
    }

    let events = until_packets(&rx, sent.len());
    let got = packets(&events);
    assert!(got.len() >= sent.len(), "only {} packets", got.len());
    for (from, bytes) in &got {
        assert_eq!(*from, d.node_addr);
        assert!(is_media(bytes), "not media: {bytes:02x?}");
        assert!(!holds(bytes, CHALLENGE.as_bytes()), "challenge in the log");
        assert!(!holds(bytes, TOKEN), "call token in the log");
    }
    for frame in &sent {
        assert!(
            got.iter().any(|(_, b)| b == frame),
            "a voice frame is missing"
        );
    }

    let start = starts(&events);
    let [(call, meta)] = start.as_slice() else {
        panic!("one Start, not {start:?}");
    };
    assert_eq!(meta.network, PacketNetwork::AllStar);
    assert_eq!(meta.label, "AllStar 1999");
    assert_eq!(meta.peer, d.node_addr);
    assert_eq!(
        meta.local, d.caller,
        "local is the connected socket's address"
    );

    let samples = d.mgr.packet_log_samples();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].0, *call);
    assert_eq!(samples[0].1.underruns, Some(0));

    let call = *call;
    d.mgr.hangup(d.id, None).unwrap();
    wait_for_end(&rx, call);
}

/// The node picks slin, which is nobody's default, so only a codec read from
/// the call can pass.
#[test]
fn an_outgoing_call_names_its_codec_in_start() {
    let (slot, rx) = tapped_slot();
    let (node, node_addr) = udp();
    let mut mgr = common::null_manager();
    mgr.set_packet_slot(slot);
    let spec = DialSpec {
        codec_policy: CodecPolicy::PreferSlin,
        ..dial_spec(node_addr)
    };
    let id = mgr.dial(spec).expect("dial");
    let (caller, _) = accept_dial_with(
        &node,
        Ies {
            format: Some(VoiceFormat::Slin.as_u32()),
            ..Ies::empty()
        },
    );
    wait_active(&mgr, id);

    let mini = mini_voice(NODE_CALL, 40, &[0x11; 320]);
    node.send_to(&mini, caller).unwrap();

    let events = until_packets(&rx, 1);
    let start = starts(&events);
    let [(call, meta)] = start.as_slice() else {
        panic!("one Start, not {start:?}");
    };
    assert_eq!(meta.codec, Some(VoiceFormat::Slin.wire_name()));

    let call = *call;
    mgr.hangup(id, None).unwrap();
    wait_for_end(&rx, call);
}

/// A capture switched on mid-call never sees the NEW or the ACCEPT, and mini
/// frames name no format, so the call itself has to say. The call runs slin,
/// which is nobody's default, so only a codec read from the call can pass.
#[test]
fn a_tap_installed_mid_call_still_names_the_codec() {
    let slot = PacketSlot::default();
    let (node, node_addr) = udp();
    let spec = DialSpec {
        codec_policy: CodecPolicy::PreferSlin,
        ..dial_spec(node_addr)
    };
    let accept_ies = Ies {
        format: Some(VoiceFormat::Slin.as_u32()),
        ..Ies::empty()
    };
    let mut d = dialled_as(slot.clone(), node, node_addr, spec, accept_ies);

    // The caller's ACK is the proof the call carried this voice untapped.
    let full = full_voice(NODE_CALL, d.caller_call, 2, 3, &[0x11; 320]);
    d.node.send_to(&full, d.caller).unwrap();
    let deadline = Instant::now() + WAIT;
    while let Some((f, _)) = next_full(&d.node, deadline) {
        if is(&f, IaxCommand::Ack) && f.timestamp == 20 {
            break;
        }
    }
    assert!(
        Instant::now() < deadline,
        "the voice was never acknowledged"
    );

    let (tap, rx) = packet_tap_channel(256, 256);
    slot.set(Some(tap));
    let mini = mini_voice(NODE_CALL, 60, &[0x22; 320]);
    d.node.send_to(&mini, d.caller).unwrap();

    let events = until_packets(&rx, 1);
    let start = starts(&events);
    let [(call, meta)] = start.as_slice() else {
        panic!("one Start, not {start:?}");
    };
    assert_eq!(meta.codec, Some(VoiceFormat::Slin.wire_name()));

    let call = *call;
    d.mgr.hangup(d.id, None).unwrap();
    wait_for_end(&rx, call);
}

/// The dial path reads `src` from a connected socket and does not check it,
/// so this pins the reason it need not: only the peer reaches the tap.
#[test]
fn a_stranger_on_a_dialled_call_is_not_logged() {
    let (slot, rx) = tapped_slot();
    let d = dialled(slot);

    let (stranger, _) = udp();
    let _ = stranger.send_to(&mini_voice(NODE_CALL, 40, &[0xEE; 160]), d.caller);
    let forged = full_voice(NODE_CALL, d.caller_call, 2, 3, &[0xEE; 160]);
    let _ = stranger.send_to(&forged, d.caller);

    let real = mini_voice(NODE_CALL, 60, &[0x11; 160]);
    d.node.send_to(&real, d.caller).unwrap();

    let events = until_packets(&rx, 1);
    assert_eq!(packets(&events), vec![(d.node_addr, real.as_slice())]);
    assert!(rx.try_recv().is_err(), "nothing followed the real frame");
}

// ---------------------------------------------------------------------------
// An inbound leg, from a fake caller.
// ---------------------------------------------------------------------------

fn new_ies(token: &[u8]) -> Ies<'_> {
    Ies {
        username: Some(USER),
        called_number: Some("1998"),
        calling_number: Some("1999"),
        capability: Some(VoiceFormat::G711U.as_u32()),
        format: Some(VoiceFormat::G711U.as_u32()),
        version: Some(2),
        calltoken: Some(token),
        ..Ies::empty()
    }
}

fn auto_answer_policy(slot: PacketSlot) -> IncomingCallPolicy {
    IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Off,
        calltoken: IncomingCallTokenPolicy::Never,
        packet_slot: slot,
        ..IncomingCallPolicy::default()
    }
}

fn listen(policy: IncomingCallPolicy) -> (IncomingCallListener, Receiver<IncomingCallEvent>) {
    IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts")
}

/// Wait for `cmd`, acknowledging everything else the listener sends.
fn wait_for(sock: &UdpSocket, my_call: u16, cmd: IaxCommand) -> Seen {
    let deadline = Instant::now() + WAIT;
    while let Some((f, src)) = next_full(sock, deadline) {
        if is(&f, cmd) {
            return f;
        }
        if !is(&f, IaxCommand::Ack) {
            ack(sock, src, my_call, &f);
        }
    }
    panic!("no {cmd:?} from the listener");
}

/// The unified call auto-answer hands over.
fn answered(events: &Receiver<IncomingCallEvent>) -> Call {
    match events.recv_timeout(WAIT).expect("an incoming call event") {
        IncomingCallEvent::Answered { call, .. } => call,
        IncomingCallEvent::Incoming(_) => panic!("auto-accept yields Answered"),
    }
}

/// Call the listener with no login; returns the listener's call number.
fn call_in(sock: &UdpSocket, to: SocketAddr, my_call: u16) -> u16 {
    let new = iax(my_call, 0, 0, 0, IaxCommand::New, new_ies(b""));
    sock.send_to(&new, to).unwrap();
    wait_for(sock, my_call, IaxCommand::Accept).source_call
}

#[test]
fn an_inbound_leg_logs_voice_and_no_auth() {
    let (slot, rx) = tapped_slot();
    let mut credentials = HashMap::new();
    credentials.insert(USER.to_string(), Arc::new(Secret::new(SECRET.to_string())));
    let (listener, events) = listen(IncomingCallPolicy {
        auth: IncomingAuthPolicy::Required,
        calltoken: IncomingCallTokenPolicy::Always,
        credentials,
        ..auto_answer_policy(slot)
    });
    let to = listener.local_addr();
    let (peer, peer_addr) = udp();

    // CALLTOKEN round, then the MD5 round, with the listener's own canaries.
    let new = iax(PEER_CALL, 0, 0, 0, IaxCommand::New, new_ies(b""));
    peer.send_to(&new, to).unwrap();
    let token = wait_for(&peer, PEER_CALL, IaxCommand::CallToken)
        .calltoken
        .expect("CALLTOKEN carries a token");
    let new = iax(PEER_CALL, 0, 0, 0, IaxCommand::New, new_ies(&token));
    peer.send_to(&new, to).unwrap();
    let authreq = wait_for(&peer, PEER_CALL, IaxCommand::AuthReq);
    let leg_call = authreq.source_call;
    let challenge = authreq.challenge.expect("AUTHREQ carries a challenge");
    let md5 = md5_response(&challenge, SECRET);
    let ies = Ies {
        md5_result: Some(&md5),
        ..Ies::empty()
    };
    let reply = iax(PEER_CALL, leg_call, 1, 1, IaxCommand::AuthRep, ies);
    peer.send_to(&reply, to).unwrap();
    let accept = wait_for(&peer, PEER_CALL, IaxCommand::Accept);
    assert_eq!(accept.source_call, leg_call);
    let call = answered(&events);

    let full = full_voice(PEER_CALL, leg_call, 2, 3, &[0x11; 160]);
    peer.send_to(&full, to).unwrap();
    let mut sent = vec![full];
    for i in 0..4u8 {
        let mini = mini_voice(PEER_CALL, 40 + u16::from(i) * 20, &[0x20 + i; 160]);
        peer.send_to(&mini, to).unwrap();
        sent.push(mini);
    }

    let tapped = until_packets(&rx, sent.len());
    let got = packets(&tapped);
    assert!(got.len() >= sent.len(), "only {} packets", got.len());
    for (from, bytes) in &got {
        assert_eq!(*from, peer_addr, "the from is the peer");
        assert!(is_media(bytes), "not media: {bytes:02x?}");
        assert!(!holds(bytes, challenge.as_bytes()), "challenge in the log");
        assert!(!holds(bytes, md5.as_bytes()), "MD5 reply in the log");
        assert!(!holds(bytes, &token), "call token in the log");
    }
    for frame in &sent {
        assert!(
            got.iter().any(|(_, b)| b == frame),
            "a voice frame is missing"
        );
    }

    let start = starts(&tapped);
    let [(id, meta)] = start.as_slice() else {
        panic!("one Start, not {start:?}");
    };
    assert_eq!(meta.network, PacketNetwork::AllStar);
    assert_eq!(meta.codec, Some(VoiceFormat::G711U.wire_name()));
    assert_eq!(meta.label, "AllStar inbound 1999");
    assert_eq!(meta.peer, peer_addr);
    assert_eq!(meta.local, to);
    assert_eq!(call.packet_call(), Some(*id));

    let id = *id;
    call.hangup(None).unwrap();
    wait_for_end(&rx, id);
}

/// The peer's voice follows the ACCEPT at once, with no ACK to wake the leg
/// first, so the codec has to be published before the ACCEPT goes out.
#[test]
fn an_inbound_call_names_its_codec_in_start() {
    let (slot, rx) = tapped_slot();
    let (listener, events) = listen(IncomingCallPolicy {
        codec_policy: CodecPolicy::PreferSlin,
        ..auto_answer_policy(slot)
    });
    let to = listener.local_addr();
    let (peer, _) = udp();

    let slin = VoiceFormat::Slin.as_u32();
    let ies = Ies {
        capability: Some(slin),
        format: Some(slin),
        ..new_ies(b"")
    };
    let new = iax(PEER_CALL, 0, 0, 0, IaxCommand::New, ies);
    peer.send_to(&new, to).unwrap();
    wait_for(&peer, PEER_CALL, IaxCommand::Accept);
    let mini = mini_voice(PEER_CALL, 40, &[0x11; 320]);
    peer.send_to(&mini, to).unwrap();

    let tapped = until_packets(&rx, 1);
    let start = starts(&tapped);
    let [(id, meta)] = start.as_slice() else {
        panic!("one Start, not {start:?}");
    };
    assert_eq!(meta.codec, Some(VoiceFormat::Slin.wire_name()));

    let id = *id;
    answered(&events).hangup(None).unwrap();
    wait_for_end(&rx, id);
}

#[test]
fn an_inbound_leg_with_no_calling_number_is_labelled_by_its_address() {
    let (slot, rx) = tapped_slot();
    let (listener, events) = listen(auto_answer_policy(slot));
    let to = listener.local_addr();
    let (peer, peer_addr) = udp();

    let ies = Ies {
        calling_number: None,
        ..new_ies(b"")
    };
    peer.send_to(&iax(PEER_CALL, 0, 0, 0, IaxCommand::New, ies), to)
        .unwrap();
    wait_for(&peer, PEER_CALL, IaxCommand::Accept);
    let _call = answered(&events);

    peer.send_to(&mini_voice(PEER_CALL, 40, &[0x11; 160]), to)
        .unwrap();
    let tapped = until_packets(&rx, 1);
    let start = starts(&tapped);
    assert_eq!(start[0].1.label, format!("AllStar inbound {peer_addr}"));
}

#[test]
fn each_inbound_leg_gets_its_own_call_id() {
    let (slot, rx) = tapped_slot();
    let (listener, events) = listen(auto_answer_policy(slot));
    let to = listener.local_addr();

    let (a, _) = udp();
    let (b, _) = udp();
    call_in(&a, to, 100);
    let call_a = answered(&events);
    call_in(&b, to, 200);
    let call_b = answered(&events);

    a.send_to(&mini_voice(100, 40, &[0x11; 160]), to).unwrap();
    b.send_to(&mini_voice(200, 40, &[0x22; 160]), to).unwrap();

    let tapped = until_packets(&rx, 2);
    let start = starts(&tapped);
    assert_eq!(start.len(), 2, "two Starts, not {start:?}");
    assert_ne!(start[0].0, start[1].0);
    let ids = [start[0].0, start[1].0];
    assert!(ids.contains(&call_a.packet_call().expect("a leg has a tap")));
    assert!(ids.contains(&call_b.packet_call().expect("a leg has a tap")));
    assert_ne!(call_a.packet_call(), call_b.packet_call());
}

#[test]
fn a_stranger_on_the_listener_port_is_not_logged() {
    let (slot, rx) = tapped_slot();
    let (listener, events) = listen(auto_answer_policy(slot));
    let to = listener.local_addr();
    let (peer, peer_addr) = udp();
    let leg_call = call_in(&peer, to, PEER_CALL);
    let _call = answered(&events);

    // Another address, naming the live call both ways a frame can.
    let (stranger, _) = udp();
    let forged_mini = mini_voice(PEER_CALL, 40, &[0xEE; 160]);
    stranger.send_to(&forged_mini, to).unwrap();
    let forged_full = full_voice(PEER_CALL, leg_call, 2, 3, &[0xEE; 160]);
    stranger.send_to(&forged_full, to).unwrap();
    // The right address with the wrong peer call: a previous call's tail.
    let stale_full = full_voice(PEER_CALL + 1, leg_call, 2, 3, &[0xEE; 160]);
    peer.send_to(&stale_full, to).unwrap();
    let stale_mini = mini_voice(PEER_CALL + 1, 40, &[0xEE; 160]);
    peer.send_to(&stale_mini, to).unwrap();

    // One socket, one demux thread: this arrives after all of the above.
    let real = mini_voice(PEER_CALL, 60, &[0x11; 160]);
    peer.send_to(&real, to).unwrap();

    let tapped = until_packets(&rx, 1);
    assert_eq!(packets(&tapped), vec![(peer_addr, real.as_slice())]);
}

// ---------------------------------------------------------------------------
// Where the tap sits, and what it leaves alone.
// ---------------------------------------------------------------------------

#[test]
fn an_out_of_order_duplicate_is_logged_before_the_gate() {
    // Dialled: the runtime's reliability layer would swallow the repeat.
    let (slot, rx) = tapped_slot();
    let d = dialled(slot);
    let full = full_voice(NODE_CALL, d.caller_call, 2, 3, &[0x11; 160]);
    d.node.send_to(&full, d.caller).unwrap();
    d.node.send_to(&full, d.caller).unwrap();
    let events = until_packets(&rx, 2);
    assert_eq!(
        packets(&events),
        vec![
            (d.node_addr, full.as_slice()),
            (d.node_addr, full.as_slice())
        ]
    );

    // Inbound: the same repeat, on the listener.
    let (slot, rx) = tapped_slot();
    let (listener, incoming) = listen(auto_answer_policy(slot));
    let to = listener.local_addr();
    let (peer, peer_addr) = udp();
    let leg_call = call_in(&peer, to, PEER_CALL);
    let _call = answered(&incoming);
    let full = full_voice(PEER_CALL, leg_call, 2, 3, &[0x22; 160]);
    peer.send_to(&full, to).unwrap();
    peer.send_to(&full, to).unwrap();
    let events = until_packets(&rx, 2);
    assert_eq!(
        packets(&events),
        vec![(peer_addr, full.as_slice()), (peer_addr, full.as_slice())]
    );
}

#[test]
fn registration_is_never_tapped() {
    let (slot, rx) = tapped_slot();
    let mut mgr = common::null_manager();
    mgr.set_packet_slot(slot);

    let (registrar, registrar_addr) = udp();
    let fake = std::thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        while let Some((f, src)) = next_full(&registrar, deadline) {
            let next = f.oseqno.wrapping_add(1);
            if is(&f, IaxCommand::RegReq) && f.md5.is_none() {
                let ies = Ies {
                    authmethods: Some(2),
                    challenge: Some(CHALLENGE),
                    ..Ies::empty()
                };
                let auth = iax(5000, f.source_call, 0, next, IaxCommand::RegAuth, ies);
                registrar.send_to(&auth, src).unwrap();
            } else if is(&f, IaxCommand::RegReq) {
                let ies = Ies {
                    refresh: Some(60),
                    ..Ies::empty()
                };
                let regack = iax(5000, f.source_call, 1, next, IaxCommand::RegAck, ies);
                registrar.send_to(&regack, src).unwrap();
            } else if is(&f, IaxCommand::RegRel) {
                let done = iax(
                    5000,
                    f.source_call,
                    2,
                    next,
                    IaxCommand::RegAck,
                    Ies::empty(),
                );
                registrar.send_to(&done, src).unwrap();
                return;
            }
        }
    });

    let secret = Arc::new(Secret::new(SECRET.to_string()));
    let (registration, events) = Registrar::new(registrar_addr, USER, secret)
        .register()
        .expect("registration starts");
    let deadline = Instant::now() + WAIT;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(RegistrationEvent::Registered { .. }) => break,
            Ok(RegistrationEvent::Failed(r)) => panic!("registration failed: {r:?}"),
            Ok(_) => {}
            Err(e) => panic!("never registered: {e}"),
        }
    }
    registration.deregister().expect("deregister");
    fake.join().unwrap();

    assert_eq!(rx.try_iter().count(), 0, "registration reached the tap");
}

#[test]
fn no_tap_means_no_events_and_no_change() {
    let d = dialled(PacketSlot::default());

    // The call behaves as ever: voice is taken and acknowledged.
    let full = full_voice(NODE_CALL, d.caller_call, 2, 3, &[0x11; 160]);
    d.node.send_to(&full, d.caller).unwrap();
    let deadline = Instant::now() + WAIT;
    let acked = std::iter::from_fn(|| next_full(&d.node, deadline))
        .any(|(f, _)| is(&f, IaxCommand::Ack) && f.iseqno == 3);
    assert!(acked, "the voice frame was not acknowledged");

    assert_eq!(d.mgr.snapshot().calls.len(), 1);
}
