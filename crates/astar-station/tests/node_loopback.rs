// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Offline Node/Auto end-to-end: a Station in Node mode binds an ephemeral
//! loopback listener; a peer UDP socket sends a hand-built NEW; driving the
//! Station's snapshot poll auto-answers + adopts + routes the inbound call.
#![allow(clippy::too_many_lines)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use astar_console::ConsoleSession;
use astar_iax::{BridgeConfig, BridgeMode, CodecPolicy, IncomingAuthPolicy, IncomingCallPolicy};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_station::{
    AnswerPolicy, CallStatus, ForcedReason, ForcedRecord, NodeConfig, OperatingMode, PttSource,
    RegisterConfig, Station, StationConfig, StationError, StationEvent,
};

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
    drain_and_ack(sock, listener_addr, my_call, None);
}

/// `pump_acks` that stops after `budget`. A mix-minus bridge streams every
/// other leg's audio to each peer every 20 ms, so with two legs up a peer's
/// socket never stays quiet for the 50 ms read timeout and an unbounded drain
/// of one peer never returns, starving the other peer's ACKs until its leg
/// times out.
fn pump_acks_for(sock: &UdpSocket, listener_addr: SocketAddr, my_call: u16, budget: Duration) {
    drain_and_ack(sock, listener_addr, my_call, Some(Instant::now() + budget));
}

fn drain_and_ack(
    sock: &UdpSocket,
    listener_addr: SocketAddr,
    my_call: u16,
    until: Option<Instant>,
) {
    let mut buf = [0u8; 4096];
    while until.is_none_or(|end| Instant::now() < end) {
        let Ok((n, _src)) = sock.recv_from(&mut buf) else {
            break;
        };
        let bytes = buf[..n].to_vec();
        let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
            continue;
        };
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
    }
}

fn node_station() -> Station {
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let cfg = StationConfig {
        mode: OperatingMode::Node,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy,
            answer: AnswerPolicy::Auto,
            register: None,
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())))
}

fn manual_node_station() -> Station {
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let cfg = StationConfig {
        mode: OperatingMode::Node,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy,
            answer: AnswerPolicy::Manual,
            register: None,
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())))
}

#[test]
fn node_auto_answers_inbound_new_and_can_key() {
    let station = node_station();
    // Mode applied at construction starts the engine; set_mode is idempotent.
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut answered = false;
    while Instant::now() < deadline && !answered {
        let snap = station.snapshot(); // drives node.poll(): answer + adopt + route
        pump_acks(&peer, addr, PEER_CALL);
        if snap.status == CallStatus::Answered {
            answered = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(answered, "inbound call must reach Answered");
    assert_eq!(station.snapshot().mode, OperatingMode::Node);
    // Keying the routed handset mic succeeds (not NotConnected).
    station.set_ptt(true).expect("key the routed mic");
    station.set_ptt(false).expect("unkey");
}

#[test]
fn node_rejects_second_inbound_while_at_cap() {
    // Use max_calls=1 so the second inbound offer is rejected at capacity.
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let cfg = StationConfig {
        mode: OperatingMode::Node,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy,
            answer: AnswerPolicy::Auto,
            register: None,
            max_calls: 1, // cap at 1 so the second is rejected
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    let station =
        Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())));
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("bound");

    let (peer1, _a1) = peer_socket();
    peer1
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && station.snapshot().status != CallStatus::Answered {
        let _ = station.snapshot();
        pump_acks(&peer1, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        station.snapshot().status,
        CallStatus::Answered,
        "first call up"
    );

    // Second caller (distinct source_call) while busy → REJECT/HANGUP back.
    let (peer2, _a2) = peer_socket();
    peer2
        .send_to(&new_datagram(valid_new_ies(), PEER_CALL + 1), addr)
        .unwrap();
    let mut rejected = false;
    let d2 = Instant::now() + Duration::from_secs(5);
    while Instant::now() < d2 && !rejected {
        let _ = station.snapshot(); // drives poll → reject(busy)
        let mut buf = [0u8; 4096];
        while let Ok((n, _)) = peer2.recv_from(&mut buf) {
            if let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
                && matches!(
                    f.subclass,
                    Subclass::Iax(IaxCommand::Reject | IaxCommand::Hangup)
                )
            {
                rejected = true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(rejected, "second inbound while busy is rejected");
    // The first call is unaffected.
    assert_eq!(station.snapshot().status, CallStatus::Answered);
}

#[test]
fn node_manual_surfaces_incoming_then_answer_bridges() {
    let station = manual_node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("bound");

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Drive next_event (which polls + parks) + pump ACKs until the offer rings.
    let mut from = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && from.is_none() {
        if let Some(astar_station::StationEvent::IncomingCall { from: f }) = station.next_event() {
            from = Some(f);
        }
        pump_acks(&peer, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        from.as_deref(),
        Some("1001"),
        "rings with the caller node id"
    );
    // Still idle until answered.
    assert_ne!(station.snapshot().status, CallStatus::Answered);

    station.answer().expect("answer the ringing call");
    let d2 = Instant::now() + Duration::from_secs(5);
    let mut answered = false;
    while Instant::now() < d2 && !answered {
        if station.snapshot().status == CallStatus::Answered {
            answered = true;
        }
        pump_acks(&peer, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(answered, "answered offer bridges to Answered");
    station.set_ptt(true).expect("key");
    station.set_ptt(false).expect("unkey");
}

#[test]
fn node_manual_reject_sends_hangup() {
    let station = manual_node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("bound");
    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    let mut rang = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !rang {
        if let Some(astar_station::StationEvent::IncomingCall { .. }) = station.next_event() {
            rang = true;
        }
        pump_acks(&peer, addr, PEER_CALL);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(rang, "offer rings");
    station.reject().expect("reject");

    let mut got = false;
    let d2 = Instant::now() + Duration::from_secs(5);
    while Instant::now() < d2 && !got {
        let _ = station.snapshot();
        let mut buf = [0u8; 4096];
        while let Ok((n, _)) = peer.recv_from(&mut buf) {
            if let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
                && matches!(
                    f.subclass,
                    Subclass::Iax(IaxCommand::Reject | IaxCommand::Hangup)
                )
            {
                got = true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(got, "reject() sends REJECT/HANGUP on the wire");
}

#[test]
fn node_register_some_fires_regreq_at_the_registrar() {
    // Stub registrar: a bound UDP socket that just observes the REGREQ.
    let reg_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    reg_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let reg_addr = reg_sock.local_addr().unwrap();

    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let cfg = StationConfig {
        mode: OperatingMode::Wt, // start Wt; switch below so the resolver is set first
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy,
            answer: AnswerPolicy::Auto,
            register: Some(RegisterConfig {
                fallbacks: Vec::new(),
                peer: reg_addr,
                username: "77777test".to_string(),
                refresh: Duration::from_secs(60),
            }),
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    let station =
        Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())));
    station.set_secret_resolver(Box::new(|_user| "stub-pw".to_string()));
    station.set_mode(OperatingMode::Node).unwrap();

    // The registrar receives a REGREQ from our node.
    let mut buf = [0u8; 4096];
    let got_regreq = {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut found = false;
        while Instant::now() < deadline && !found {
            if let Ok((n, _src)) = reg_sock.recv_from(&mut buf)
                && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
                && matches!(f.subclass, Subclass::Iax(IaxCommand::RegReq))
            {
                found = true;
            }
        }
        found
    };
    assert!(
        got_regreq,
        "register=Some must fire a REGREQ at the registrar"
    );
}

#[test]
fn node_register_without_resolver_surfaces_register_failed() {
    let cfg = StationConfig {
        mode: OperatingMode::Wt,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy: IncomingCallPolicy {
                auth: IncomingAuthPolicy::Off,
                ..IncomingCallPolicy::default()
            },
            answer: AnswerPolicy::Auto,
            register: Some(RegisterConfig {
                fallbacks: Vec::new(),
                peer: "127.0.0.1:1".parse().unwrap(),
                username: "x".to_string(),
                refresh: Duration::from_secs(60),
            }),
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    let station =
        Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())));
    // No resolver set.
    station.set_mode(OperatingMode::Node).unwrap();
    let mut got = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !got {
        if let Some(StationEvent::RegisterFailed { .. }) = station.next_event() {
            got = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        got,
        "register=Some with no resolver surfaces RegisterFailed"
    );
}

/// Secret-free guard: the resolver returns "stub-pw", but no `StationEvent`
/// (including `RegisterFailed.reason`) and no `RegisterConfig`/`NodeConfig`
/// `Debug` may ever leak that secret.
#[test]
fn register_secret_never_appears_in_events_or_config_debug() {
    const SECRET: &str = "stub-pw-do-not-leak";

    // RegisterConfig Debug must not carry a secret (it doesn't hold one).
    let reg = RegisterConfig {
        fallbacks: Vec::new(),
        peer: "127.0.0.1:1".parse().unwrap(),
        username: "77777".to_string(),
        refresh: Duration::from_secs(60),
    };
    assert!(
        !format!("{reg:?}").contains(SECRET),
        "RegisterConfig Debug must be secret-free"
    );

    // Drive a real registration whose resolver returns the secret, then assert
    // the secret never appears in any surfaced StationEvent Debug.
    let cfg = StationConfig {
        mode: OperatingMode::Wt,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy: IncomingCallPolicy {
                auth: IncomingAuthPolicy::Off,
                ..IncomingCallPolicy::default()
            },
            answer: AnswerPolicy::Auto,
            register: Some(RegisterConfig {
                fallbacks: Vec::new(),
                // An unreachable registrar address so registration will fail and
                // surface a RegisterFailed event whose Debug we can inspect.
                peer: "127.0.0.1:1".parse().unwrap(),
                username: "77777".to_string(),
                refresh: Duration::from_secs(60),
            }),
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    let station =
        Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())));
    station.set_secret_resolver(Box::new(|_user| SECRET.to_string()));
    station.set_mode(OperatingMode::Node).unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(ev) = station.next_event() {
            assert!(
                !format!("{ev:?}").contains(SECRET),
                "no StationEvent Debug may leak the registrar secret: {ev:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// iax-a82f: in a multi-call node, EACH answered caller must produce a fresh
/// `StationEvent::Answered` (that edge is what fires the node's join greeting).
/// The old single-`status` latch stayed `Answered` across concurrent callers,
/// so only the FIRST caller got the greeting; every later caller got silence.
/// Drive two concurrent inbound calls and assert Answered fires TWICE.
#[test]
fn each_concurrent_inbound_call_fires_a_fresh_answered_event() {
    const CALL1: u16 = 13885;
    const CALL2: u16 = 13886;

    let station = node_station(); // Auto answer, max_calls default (>1)
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");

    let (peer1, _a1) = peer_socket();
    let (peer2, _a2) = peer_socket();

    // Bring up call 1.
    peer1
        .send_to(&new_datagram(valid_new_ies(), CALL1), addr)
        .unwrap();

    let mut answered_events = 0usize;
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut dialed_second = false;
    while Instant::now() < deadline && answered_events < 2 {
        // Drain lifecycle events, counting Answered edges.
        while let Some(ev) = station.next_event() {
            if matches!(ev, StationEvent::Answered) {
                answered_events += 1;
            }
        }
        // Keep both legs' Reliability releasing so their ANSWERs settle.
        pump_acks(&peer1, addr, CALL1);
        if dialed_second {
            pump_acks(&peer2, addr, CALL2);
        }
        // Once call 1 is up, bring up call 2 WITHOUT tearing down call 1 —
        // this is the concurrent path the single-status latch mishandled.
        if !dialed_second && answered_events >= 1 {
            peer2
                .send_to(&new_datagram(valid_new_ies(), CALL2), addr)
                .unwrap();
            dialed_second = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(
        answered_events, 2,
        "each concurrent caller must fire its own Answered edge (got {answered_events})"
    );
}

/// ble-ptt: on a multi-call node a second caller linking is not the end of
/// a held key. The console's adopt already moves the active call to the
/// newcomer and clears its key mirror, so the station releases every hold
/// WITHOUT latching it: the next `/key` keys the new active call exactly as it
/// did before the arbiter existed. A held `/key` (Legacy) is never latched by
/// a call change anyway; the Manual hold beside it is what proves the Node
/// identity (`NODE_PTT_CALL_SEQ`), because without it a new caller would
/// latch every non-Legacy source.
#[test]
fn a_second_caller_linking_does_not_latch_a_held_legacy_key() {
    const CALL1: u16 = 13885;
    const CALL2: u16 = 13886;

    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");
    let (peer1, _a1) = peer_socket();
    let (peer2, _a2) = peer_socket();

    peer1
        .send_to(&new_datagram(valid_new_ies(), CALL1), addr)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && station.snapshot().status != CallStatus::Answered {
        pump_acks(&peer1, addr, CALL1);
        std::thread::sleep(Duration::from_millis(20));
    }
    let first = station.snapshot();
    assert_eq!(first.status, CallStatus::Answered, "call 1 must answer");
    assert_eq!(first.mode, OperatingMode::Node);

    station
        .set_ptt(true)
        .expect("key call 1, as POST /key does");
    station
        .ptt_source(PttSource::Manual, true)
        .expect("a Manual hold joins the keyed call");
    assert_eq!(
        station.ptt_snapshot().sources,
        PttSource::Legacy.bit() | PttSource::Manual.bit()
    );

    peer2
        .send_to(&new_datagram(valid_new_ies(), CALL2), addr)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && station.snapshot().answered_seq == first.answered_seq {
        pump_acks(&peer1, addr, CALL1);
        pump_acks(&peer2, addr, CALL2);
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = station.snapshot();
    assert_eq!(
        second.answered_seq,
        first.answered_seq + 1,
        "call 2 must answer without call 1 hanging up"
    );
    assert_eq!(second.status, CallStatus::Answered);

    let ptt = station.ptt_snapshot();
    assert_eq!(
        ptt.latched, 0,
        "a new caller must not latch any held source"
    );
    assert_eq!(
        ptt.sources, 0,
        "the hold is released to match the console, which dropped the key"
    );
    assert!(!second.ptt, "the console reports unkeyed after the adopt");
    // Handset mode: routing the mic to call 2 demoted call 1 to monitor-only
    // and sent its PTT-off (`Manager::route`), so no leg is left keyed.
    assert_eq!(
        second.calls.iter().map(|c| c.keyed).collect::<Vec<_>>(),
        vec![false, false],
        "after call 2 links, neither IAX2 leg may still be keyed"
    );
    station
        .set_ptt(true)
        .expect("the next /key keys the new active call, as before");
    assert_eq!(station.ptt_snapshot().sources, PttSource::Legacy.bit());
    assert_eq!(
        station.snapshot().calls.iter().filter(|c| c.keyed).count(),
        1,
        "the re-key keys exactly one leg, the new active call"
    );
    station.set_ptt(false).expect("unkey");
}

/// ble-ptt: the watchdog's forced unkey reaches an IAX2 call, not only an
/// M17 session. A `/key` (a Legacy press) on an answered Node call is cut off
/// by the transmit time-out through `Manager::unkey`, the console and every
/// IAX2 leg read unkeyed, and Legacy stays latched until `set_ptt(false)`:
/// the time-out is the one event (with `stop_all`) that latches Legacy.
/// The 200 ms time-out comes from the `test-hooks` hook.
#[test]
fn the_transmit_time_out_unkeys_an_iax2_node_call() {
    const CALL1: u16 = 13887;

    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");
    let (peer1, _a1) = peer_socket();
    peer1
        .send_to(&new_datagram(valid_new_ies(), CALL1), addr)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && station.snapshot().status != CallStatus::Answered {
        pump_acks(&peer1, addr, CALL1);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        station.snapshot().status,
        CallStatus::Answered,
        "call 1 must answer"
    );

    station
        .set_tx_timeout_for_test(Duration::from_millis(200))
        .expect("200 ms is inside the test hook's range");
    station
        .set_ptt(true)
        .expect("key call 1, as POST /key does");
    assert_eq!(station.ptt_snapshot().sources, PttSource::Legacy.bit());
    assert_eq!(
        station.snapshot().calls.iter().filter(|c| c.keyed).count(),
        1,
        "the /key keyed the IAX2 leg"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && station.ptt_snapshot().sources != 0 {
        pump_acks(&peer1, addr, CALL1);
        std::thread::sleep(Duration::from_millis(10));
    }
    let ptt = station.ptt_snapshot();
    assert_eq!(
        ptt.sources, 0,
        "the watchdog must apply the transmit time-out"
    );
    assert_eq!(
        ptt.forced,
        ForcedRecord {
            seq: 1,
            sources: PttSource::Legacy.bit(),
            reason: Some(ForcedReason::TxTimeout),
        }
    );
    assert_eq!(
        ptt.latched,
        PttSource::Legacy.bit(),
        "the time-out latches Legacy"
    );
    let snap = station.snapshot();
    assert!(!snap.ptt, "the watchdog's unkey must reach the console");
    assert_eq!(
        snap.calls.iter().filter(|c| c.keyed).count(),
        0,
        "the watchdog's unkey must reach the IAX2 leg through Manager::unkey"
    );
    assert!(
        matches!(station.set_ptt(true), Err(StationError::PttLatched)),
        "a /key after the time-out needs an /unkey first"
    );
    station
        .set_ptt(false)
        .expect("set_ptt(false) re-arms Legacy");
    station
        .set_tx_timeout(Duration::from_secs(60))
        .expect("60 s is in range");
    station
        .set_ptt(true)
        .expect("a fresh /key after /unkey keys");
    assert_eq!(station.ptt_snapshot().sources, PttSource::Legacy.bit());
    station.set_ptt(false).expect("unkey");
}

/// ble-ptt: on a mix-minus node (`BridgeMode::Bridge`, the node daemon's
/// default) an adopted caller has no routed mic, so `Manager::key` refuses
/// it with `NotRouted` and a `/key` (a Legacy press) cannot key any IAX2
/// leg. A refused `/key` latches nothing (Legacy latches only on the
/// transmit time-out), so the next `/key` gets the same refusal rather than
/// `PttLatched`, and no leg reads keyed, before or after a second node links.
#[test]
fn a_key_on_a_mix_minus_node_is_refused_and_keys_no_leg() {
    const CALL1: u16 = 13888;
    const CALL2: u16 = 13889;
    // Bounded drains: see `pump_acks_for`.
    const MIX_MINUS_PUMP: Duration = Duration::from_millis(40);

    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    station
        .set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            ..BridgeConfig::default()
        })
        .expect("bridge mode applies to the node");
    let addr = station.node_bind_addr().expect("node listener bound");
    let (peer1, _a1) = peer_socket();
    let (peer2, _a2) = peer_socket();

    peer1
        .send_to(&new_datagram(valid_new_ies(), CALL1), addr)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && station.snapshot().status != CallStatus::Answered {
        pump_acks_for(&peer1, addr, CALL1, MIX_MINUS_PUMP);
        std::thread::sleep(Duration::from_millis(20));
    }
    let first = station.snapshot();
    assert_eq!(first.status, CallStatus::Answered, "call 1 must answer");

    assert!(
        matches!(station.set_ptt(true), Err(StationError::Iax(_))),
        "a mix-minus leg has no routed mic, so the console refuses the key"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "the refused press is rolled back");
    assert_eq!(ptt.latched, 0, "a refused /key is not latched");
    assert!(
        matches!(station.set_ptt(true), Err(StationError::Iax(_))),
        "the next /key gets the console's refusal again, never PttLatched"
    );
    let snap = station.snapshot();
    assert_eq!(snap.calls.len(), 1, "call 1 is in the pool");
    assert!(snap.calls.iter().all(|c| !c.keyed), "no leg is keyed");

    peer2
        .send_to(&new_datagram(valid_new_ies(), CALL2), addr)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && station.snapshot().answered_seq == first.answered_seq {
        pump_acks_for(&peer1, addr, CALL1, MIX_MINUS_PUMP);
        pump_acks_for(&peer2, addr, CALL2, MIX_MINUS_PUMP);
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = station.snapshot();
    assert_eq!(
        second.answered_seq,
        first.answered_seq + 1,
        "call 2 must answer without call 1 hanging up"
    );
    assert_eq!(second.calls.len(), 2, "both legs are in the pool");
    assert!(
        second.calls.iter().all(|c| !c.keyed),
        "after call 2 links, no IAX2 leg is keyed"
    );
    assert!(
        matches!(station.set_ptt(true), Err(StationError::Iax(_))),
        "a /key with two mix-minus legs up is refused too"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(
        (ptt.sources, ptt.latched),
        (0, 0),
        "nothing held, nothing latched"
    );
    assert!(
        station.snapshot().calls.iter().all(|c| !c.keyed),
        "and still no IAX2 leg is keyed"
    );
}

// ---- iax-4348: the node inherits the station's codec policy ----------------
//
// `StationConfig.codec_policy` pins the station's pipeline rate at
// construction, and the inbound policy defaults to it. Before that default
// existed, `IncomingCallPolicy::default()`'s `UlawOnly` reached
// `start_inbound`, which sets `station_policy` from it — so a prefer_slin16
// node dropped to 8 kHz the moment its listener came up, and stayed there
// (a listening engine is never idle enough to rebuild). And on an engine
// already at 16 kHz, the 8 kHz leg the listener built was refused at adopt
// with "listener/station sample-rate mismatch" — a node that answered
// nothing.

#[test]
fn a_prefer_slin16_node_adopts_a_ulaw_caller_on_its_16k_pipeline() {
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Off,
        // codec_policy deliberately left at its own default: this is the
        // caller who never chose, and must inherit the station's.
        ..IncomingCallPolicy::default()
    };
    let cfg = StationConfig {
        mode: OperatingMode::Node,
        codec_policy: CodecPolicy::PreferSlin16,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy,
            answer: AnswerPolicy::Auto,
            register: None,
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = Station::with_shared_session(
        cfg,
        Arc::clone(&session),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let addr = station.node_bind_addr().expect("node listener bound");
    assert_eq!(
        session.lock().unwrap().pipeline_sample_rate(),
        16_000,
        "starting the listener must not drop a prefer_slin16 station to 8 kHz"
    );

    // A µ-law-only caller: the far end's narrow capability must not be able
    // to change what the STATION's own bus runs at.
    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut answered = false;
    while Instant::now() < deadline && !answered {
        let snap = station.snapshot();
        pump_acks(&peer, addr, PEER_CALL);
        if snap.status == CallStatus::Answered {
            answered = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        answered,
        "the µ-law caller must be adopted, not refused for a rate mismatch"
    );
    assert_eq!(
        session.lock().unwrap().pipeline_sample_rate(),
        16_000,
        "the station's pipeline stays 16 kHz across an adopted 8 kHz-codec leg"
    );
}
