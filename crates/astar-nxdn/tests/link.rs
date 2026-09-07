// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! An `NxdnFsm` against the loopback reflector, over a real UDP socket on
//! `127.0.0.1` and nothing else. CLAUDE.md forbids a test that touches any
//! other address, which is exactly why `reflector.rs` exists.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use astar_nxdn::{FsmAction, LinkState, NxdnFsm, Reflector, wire};

const TG: u16 = 31313;

#[test]
fn a_client_links_to_the_loopback_reflector_and_stays_linked() {
    let r = Reflector::bind("127.0.0.1:0".parse().expect("v4"), TG).expect("bind");
    let addr = r.local_addr();
    let handle = r.run();

    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    let mut fsm = NxdnFsm::new("KC0ABC", TG).expect("callsign");
    for d in fsm.connect(Instant::now()) {
        sock.send_to(&d, addr).expect("send poll");
    }
    let mut buf = [0u8; astar_nxdn::reflector::MAX_DATAGRAM];
    let (n, _) = sock.recv_from(&mut buf).expect("echoed poll");
    assert_eq!(fsm.on_packet(&buf[..n], Instant::now()), FsmAction::Linked);
    assert_eq!(fsm.state(), LinkState::Linked);
    assert_eq!(handle.client_count(), 1);

    for d in fsm.unlink(Instant::now()) {
        sock.send_to(&d, addr).expect("send unlink");
    }
    handle.shutdown();
}

#[test]
fn a_frame_from_one_client_reaches_the_other_verbatim() {
    // Verbatim relay is load-bearing: a reflector that re-encoded would hide
    // exactly the framing bugs a bench fixture exists to catch.
    let r = Reflector::bind("127.0.0.1:0".parse().expect("v4"), TG).expect("bind");
    let addr = r.local_addr();
    let handle = r.run();

    let a = UdpSocket::bind("127.0.0.1:0").expect("bind a");
    let b = UdpSocket::bind("127.0.0.1:0").expect("bind b");
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("timeout");
        s.send_to(
            &wire::poll(&wire::Callsign::new("W1AW").expect("callsign"), TG),
            addr,
        )
        .expect("poll");
        let mut buf = [0u8; 64];
        s.recv_from(&mut buf).expect("echo");
    }

    let mut frame = [0u8; wire::FRAME_LEN];
    frame[0] = 0x81;
    let packet = wire::DataPacket {
        src_id: 4242,
        dst_id: TG,
        group: true,
        data: false,
        start: true,
        end: false,
        frame,
    };
    a.send_to(&wire::data(&packet), addr).expect("send frame");

    let mut buf = [0u8; astar_nxdn::reflector::MAX_DATAGRAM];
    let (n, _) = b.recv_from(&mut buf).expect("relayed frame");
    assert_eq!(&buf[..n], &wire::data(&packet));
    handle.shutdown();
}

#[test]
fn a_poll_for_the_wrong_talkgroup_never_registers() {
    // NXDNReflector.cpp: `if (id == tg)` gates the whole registration.
    let r = Reflector::bind("127.0.0.1:0".parse().expect("v4"), TG).expect("bind");
    let addr = r.local_addr();
    let handle = r.run();

    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    sock.set_read_timeout(Some(Duration::from_millis(250)))
        .expect("timeout");
    sock.send_to(
        &wire::poll(&wire::Callsign::new("W1AW").expect("callsign"), 100),
        addr,
    )
    .expect("poll");
    let mut buf = [0u8; 64];
    assert!(
        sock.recv_from(&mut buf).is_err(),
        "no echo for the wrong TG"
    );
    assert_eq!(handle.client_count(), 0);
    handle.shutdown();
}

#[test]
fn the_parrot_plays_a_transmission_back_to_its_sender() {
    // The parrot is the only reason `nxdn_parrot` exists, so it gets a test
    // rather than a promise: three frames in, the same three back, in order,
    // after the transmission's end flag.
    let r = Reflector::bind_parrot_with_timeouts(
        "127.0.0.1:0".parse().expect("v4"),
        TG,
        Duration::from_secs(30),
        Duration::from_millis(20),
    )
    .expect("bind");
    let addr = r.local_addr();
    let handle = r.run();

    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    let mut fsm = NxdnFsm::new("KC0ABC", TG).expect("callsign");
    for d in fsm.connect(Instant::now()) {
        sock.send_to(&d, addr).expect("send poll");
    }
    let mut buf = [0u8; astar_nxdn::reflector::MAX_DATAGRAM];
    let (n, _) = sock.recv_from(&mut buf).expect("echoed poll");
    assert_eq!(fsm.on_packet(&buf[..n], Instant::now()), FsmAction::Linked);

    let sent: Vec<wire::DataPacket> = (0..3u8)
        .map(|i| {
            let mut frame = [0u8; wire::FRAME_LEN];
            frame[0] = 0x81;
            frame[1] = i;
            wire::DataPacket {
                src_id: 4242,
                dst_id: TG,
                group: true,
                data: false,
                start: i == 0,
                end: i == 2,
                frame,
            }
        })
        .collect();
    for packet in &sent {
        sock.send_to(&wire::data(packet), addr).expect("send audio");
    }

    // Playback is paced at the on-air frame rate, so allow for it.
    sock.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let mut heard = Vec::new();
    while heard.len() < sent.len() {
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            break;
        };
        if let FsmAction::Data(data) = fsm.on_packet(&buf[..n], Instant::now()) {
            heard.push(*data);
        }
    }

    assert_eq!(
        heard, sent,
        "the parrot plays the transmission back in order"
    );
    handle.shutdown();
}
