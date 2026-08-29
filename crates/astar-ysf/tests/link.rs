// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The client state machine driven against the loopback reflector over a
//! real socket.
//!
//! Everything binds `127.0.0.1:0`. That is a house rule, not a convenience:
//! astar's tests never transmit anywhere else, and a YSF link is exactly
//! the kind of thing that would otherwise be tempting to point at a live
//! reflector "just to see".

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_ysf::fich::{DataType, Fich, FrameInfo};
use astar_ysf::frame::{self, PAYLOAD_LEN};
use astar_ysf::fsm::{FsmAction, LinkState, YsfFsm};
use astar_ysf::reflector::Reflector;
use astar_ysf::wire::{self, Callsign, DataPacket};

const LOOPBACK: &str = "127.0.0.1:0";
/// Long enough for a datagram to cross loopback, short enough that a
/// failing test fails quickly rather than hanging.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

fn client_socket() -> UdpSocket {
    let socket = UdpSocket::bind(LOOPBACK).expect("bind loopback");
    socket
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    socket
}

fn call(s: &str) -> Callsign {
    Callsign::new(s).expect("a legal callsign")
}

/// Drives the FSM until it reports `Linked`, or gives up.
fn link(fsm: &mut YsfFsm, socket: &UdpSocket, reflector: SocketAddr) {
    for datagram in fsm.connect(Instant::now()) {
        socket.send_to(&datagram, reflector).expect("send poll");
    }
    let mut buf = [0u8; 512];
    for _ in 0..8 {
        let Ok((len, _)) = socket.recv_from(&mut buf) else {
            break;
        };
        if fsm.on_packet(&buf[..len], Instant::now()) == FsmAction::Linked {
            return;
        }
    }
    assert_eq!(fsm.state(), LinkState::Linked, "the link never came up");
}

fn voice_frame(counter: u8, end: bool) -> DataPacket {
    let fich = Fich {
        frame_info: if end {
            FrameInfo::Terminator
        } else {
            FrameInfo::Communications
        },
        data_type: DataType::VDMode2,
        frame_number: counter % 8,
        frame_total: 7,
        ..Fich::default()
    };
    let payload: Vec<u8> = (0..PAYLOAD_LEN)
        .map(|i| u8::try_from((i + usize::from(counter)) % 256).unwrap_or(0))
        .collect();
    DataPacket {
        gateway: call("KC0ABC"),
        source: call("KC0ABC"),
        destination: call("ALL"),
        counter,
        end,
        frame: frame::build(fich, &payload),
    }
}

#[test]
fn a_client_links_to_the_loopback_reflector() {
    let reflector = Reflector::bind(LOOPBACK.parse().expect("loopback addr")).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let socket = client_socket();
    let mut fsm = YsfFsm::new("KC0ABC").expect("a legal callsign");
    link(&mut fsm, &socket, addr);

    assert_eq!(fsm.state(), LinkState::Linked);
    handle.shutdown();
}

#[test]
fn unlinking_deregisters_the_client() {
    let reflector = Reflector::bind(LOOPBACK.parse().expect("loopback addr")).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let socket = client_socket();
    let mut fsm = YsfFsm::new("KC0ABC").expect("a legal callsign");
    link(&mut fsm, &socket, addr);
    assert_eq!(wait_for_clients(&handle, 1), 1);

    for datagram in fsm.unlink(Instant::now()) {
        socket.send_to(&datagram, addr).expect("send unlink");
    }
    assert_eq!(wait_for_clients(&handle, 0), 0);
    handle.shutdown();
}

/// Polls the reflector's client count until it reaches `want`, or a second
/// goes by. Sockets and threads make exact timing a lie; the count is the
/// observable, so wait on it rather than on a sleep.
fn wait_for_clients(handle: &astar_ysf::reflector::ReflectorHandle, want: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if handle.client_count() == want {
            return want;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    handle.client_count()
}

#[test]
fn one_clients_audio_reaches_another_verbatim() {
    let reflector = Reflector::bind(LOOPBACK.parse().expect("loopback addr")).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let talker_socket = client_socket();
    let mut talker = YsfFsm::new("KC0ABC").expect("a legal callsign");
    link(&mut talker, &talker_socket, addr);

    let listener_socket = client_socket();
    let mut listener = YsfFsm::new("W1AW").expect("a legal callsign");
    link(&mut listener, &listener_socket, addr);
    assert_eq!(wait_for_clients(&handle, 2), 2);

    let sent = voice_frame(3, false);
    talker_socket
        .send_to(&wire::data(&sent), addr)
        .expect("send audio");

    let mut buf = [0u8; 512];
    let received = loop {
        let (len, _) = listener_socket.recv_from(&mut buf).expect("audio arrives");
        if let FsmAction::Data(data) = listener.on_packet(&buf[..len], Instant::now()) {
            break data;
        }
    };

    assert_eq!(*received, sent, "the reflector must not rewrite a frame");

    // And the frame still decodes on the far side — this is the end-to-end
    // proof that the FICH survives a trip through the wire format.
    let frame = frame::Frame::new(&received.frame).expect("a 120-byte frame");
    assert!(frame.has_sync());
    let fich = frame.fich().expect("a readable FICH");
    assert_eq!(fich.data_type, DataType::VDMode2);
    assert_eq!(fich.frame_number, 3);

    handle.shutdown();
}

#[test]
fn a_talker_does_not_hear_their_own_frames_back() {
    let reflector = Reflector::bind(LOOPBACK.parse().expect("loopback addr")).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let socket = client_socket();
    let mut fsm = YsfFsm::new("KC0ABC").expect("a legal callsign");
    link(&mut fsm, &socket, addr);

    socket
        .send_to(&wire::data(&voice_frame(1, true)), addr)
        .expect("send audio");

    // The only thing that should come back is a poll reply, if anything.
    let mut buf = [0u8; 512];
    while let Ok((len, _)) = socket.recv_from(&mut buf) {
        assert_ne!(&buf[..4], b"YSFD", "a relay must not echo the sender");
        let _ = len;
    }
    handle.shutdown();
}

#[test]
fn the_parrot_plays_a_transmission_back_to_its_sender() {
    let reflector = Reflector::bind_parrot_with_timeouts(
        LOOPBACK.parse().expect("loopback addr"),
        Duration::from_secs(30),
        Duration::from_millis(20),
    )
    .expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let socket = client_socket();
    let mut fsm = YsfFsm::new("KC0ABC").expect("a legal callsign");
    link(&mut fsm, &socket, addr);

    let sent: Vec<DataPacket> = (0..3).map(|i| voice_frame(i, i == 2)).collect();
    for packet in &sent {
        socket
            .send_to(&wire::data(packet), addr)
            .expect("send audio");
    }

    // Playback is paced at the on-air frame rate, so allow for it.
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut heard = Vec::new();
    let mut buf = [0u8; 512];
    while heard.len() < sent.len() {
        let Ok((len, _)) = socket.recv_from(&mut buf) else {
            break;
        };
        if let FsmAction::Data(data) = fsm.on_packet(&buf[..len], Instant::now()) {
            heard.push(*data);
        }
    }

    assert_eq!(
        heard, sent,
        "the parrot plays the transmission back in order"
    );
    handle.shutdown();
}

#[test]
fn a_reflector_that_goes_quiet_times_the_link_out() {
    // No reflector at all: the address is bound and immediately dropped, so
    // nothing answers. The client must give up rather than sit on
    // "connecting" — this is the wrong-address case an operator will
    // actually hit.
    let dead: SocketAddr = {
        let socket = UdpSocket::bind(LOOPBACK).expect("bind loopback");
        socket.local_addr().expect("local addr")
    };

    let socket = client_socket();
    let mut fsm = YsfFsm::with_timing("KC0ABC", Duration::from_millis(50), Duration::from_secs(30))
        .expect("a legal callsign");
    let start = Instant::now();
    for datagram in fsm.connect(start) {
        let _ = socket.send_to(&datagram, dead);
    }
    assert_eq!(fsm.state(), LinkState::Linking);
    assert_eq!(
        fsm.tick(start + Duration::from_secs(30)),
        FsmAction::Timeout
    );
    assert_eq!(fsm.state(), LinkState::Failed);
}
