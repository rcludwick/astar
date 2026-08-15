// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station::announce` forwards to the active call's Manager (iax-da05).

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use astar_iax::{IncomingAuthPolicy, IncomingCallPolicy};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_station::{AnswerPolicy, CallStatus, NodeConfig, OperatingMode, Station, StationConfig};

const PEER_CALL: u16 = 14001;

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

fn peer_socket() -> (UdpSocket, std::net::SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let a = s.local_addr().unwrap();
    (s, a)
}

/// Drain incoming datagrams and ACK every reliable full frame.
fn pump_acks(sock: &UdpSocket, listener_addr: std::net::SocketAddr, my_call: u16) {
    let mut buf = [0u8; 4096];
    while let Ok((n, _src)) = sock.recv_from(&mut buf) {
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

// --- Tests ---

#[test]
fn station_announce_with_no_active_call_errors() {
    // An idle Station with no active call must return Err (NotConnected).
    let station = node_station();
    let req = astar_iax::AnnounceRequest::say("test");
    assert!(
        station.announce(req).is_err(),
        "announce with no active call must fail"
    );
}

#[test]
fn station_announce_on_active_node_call_is_ok() {
    // Bring up a Node station with an auto-answered inbound call, then
    // announce a PCM phrase to-air on the active call.
    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Drive snapshot (answers + adopts + routes) until Answered.
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
    assert!(answered, "inbound call must reach Answered before announce");

    // Announce a PCM phrase to-air on the active call.
    let pcm: std::sync::Arc<[i16]> = vec![0_i16; 320].into();
    let req = astar_iax::AnnounceRequest {
        phrase: astar_iax::Phrase::Pcm(pcm),
        destination: astar_iax::Destination::ToAir,
        policy: astar_iax::AnnouncePolicyReq::Seize,
        priority: 5,
    };
    assert!(
        station.announce(req).is_ok(),
        "announce on active call must succeed"
    );
}
