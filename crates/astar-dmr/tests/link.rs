// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A `DmrFsm` against the loopback master, over a real UDP socket on
//! `127.0.0.1` and nothing else. CLAUDE.md forbids a test that touches any
//! other address, and DMR raises the stakes: a real master authenticates by a
//! registered radio ID, so a stray packet is attributable to a licence.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use astar_dmr::{DmrFsm, FailureStage, FsmAction, LinkState, Master, Timeslot, wire};

const TG: u32 = 31_313;
const PASSWORD: &str = "passw0rd";

fn fsm(id: u32) -> DmrFsm {
    DmrFsm::new(
        wire::RadioId::new(id).expect("id"),
        TG,
        Timeslot::Ts2,
        wire::ConfigFields::softclient("KC0ABC", "astar test"),
        PASSWORD.to_string(),
    )
}

/// Drive one FSM to `Linked` over a real socket, answering whatever the
/// master sends until the state settles or the deadline passes.
fn link(sock: &UdpSocket, addr: std::net::SocketAddr, fsm: &mut DmrFsm) {
    for d in fsm.connect(Instant::now()) {
        sock.send_to(&d, addr).expect("send");
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; astar_dmr::master::MAX_DATAGRAM];
    while fsm.state() != LinkState::Linked && Instant::now() < deadline {
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        if let FsmAction::Send(out) = fsm.on_packet(&buf[..n], Instant::now()) {
            sock.send_to(&out, addr).expect("send");
        }
    }
}

fn client() -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    s.set_read_timeout(Some(Duration::from_millis(400)))
        .expect("timeout");
    s
}

#[test]
fn a_client_completes_the_real_handshake_against_the_loopback_master() {
    let m = Master::bind("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind");
    let addr = m.local_addr();
    let handle = m.run();

    let sock = client();
    let mut f = fsm(3_153_591);
    link(&sock, addr, &mut f);
    assert_eq!(f.state(), LinkState::Linked);
    assert_eq!(handle.peer_count(), 1);

    for d in f.close(Instant::now()) {
        sock.send_to(&d, addr).expect("send");
    }
    handle.shutdown();
}

#[test]
fn a_wrong_password_is_refused_at_the_authorisation_step() {
    // The master checks SHA256(salt || password) for real. A fixture that
    // accepted anything would let a broken digest ship.
    let m = Master::bind("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind");
    let addr = m.local_addr();
    let handle = m.run();

    let sock = client();
    let mut f = DmrFsm::new(
        wire::RadioId::new(3_153_591).expect("id"),
        TG,
        Timeslot::Ts2,
        wire::ConfigFields::softclient("KC0ABC", "astar test"),
        "not-the-password".to_string(),
    );
    for d in f.connect(Instant::now()) {
        sock.send_to(&d, addr).expect("send");
    }
    let mut buf = [0u8; astar_dmr::master::MAX_DATAGRAM];
    let deadline = Instant::now() + Duration::from_secs(2);
    while f.state() != LinkState::Failed && Instant::now() < deadline {
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        if let FsmAction::Send(out) = f.on_packet(&buf[..n], Instant::now()) {
            sock.send_to(&out, addr).expect("send");
        }
    }
    assert_eq!(f.state(), LinkState::Failed);
    assert_eq!(handle.auth_failures(), 1);
    assert_eq!(handle.peer_count(), 0);
    handle.shutdown();
}

#[test]
fn a_frame_from_one_peer_reaches_the_other_verbatim() {
    // Verbatim relay is load-bearing: a master that re-encoded would hide
    // exactly the framing bugs a bench fixture exists to catch.
    let m = Master::bind("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind");
    let addr = m.local_addr();
    let handle = m.run();

    let (a, b) = (client(), client());
    let (mut fa, mut fb) = (fsm(3_153_591), fsm(3_153_592));
    link(&a, addr, &mut fa);
    link(&b, addr, &mut fb);
    assert_eq!(handle.peer_count(), 2);

    let mut burst = [0u8; wire::BURST_LEN];
    burst[0] = 0xA5;
    let packet = wire::DataPacket {
        seq: 0,
        src_id: 3_153_591,
        dst_id: TG,
        peer_id: 3_153_591,
        slot: Timeslot::Ts2,
        call_type: wire::CallType::Group,
        frame_type: wire::FrameType::VoiceSync,
        stream_id: [9, 9, 9, 9],
        burst,
        ber: 0,
        rssi: 0,
    };
    a.send_to(&wire::data(&packet).expect("ids in range"), addr)
        .expect("send frame");

    let mut buf = [0u8; astar_dmr::master::MAX_DATAGRAM];
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(Instant::now() < deadline, "the frame never arrived");
        let Ok((n, _)) = b.recv_from(&mut buf) else {
            continue;
        };
        if &buf[..4] == b"DMRD" {
            assert_eq!(&buf[..n], &wire::data(&packet).expect("ids in range"));
            break;
        }
    }
    handle.shutdown();
}

#[test]
fn a_frame_is_never_relayed_back_to_its_sender() {
    // A master that echoed would make a client hear its own transmission and
    // read the parrot test as passing when nothing round-tripped.
    let m = Master::bind("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind");
    let addr = m.local_addr();
    let handle = m.run();

    let sock = client();
    let mut f = fsm(3_153_591);
    link(&sock, addr, &mut f);

    let packet = wire::DataPacket {
        seq: 0,
        src_id: 3_153_591,
        dst_id: TG,
        peer_id: 3_153_591,
        slot: Timeslot::Ts2,
        call_type: wire::CallType::Group,
        frame_type: wire::FrameType::VoiceSync,
        stream_id: [1, 1, 1, 1],
        burst: [0u8; wire::BURST_LEN],
        ber: 0,
        rssi: 0,
    };
    sock.send_to(&wire::data(&packet).expect("ids in range"), addr)
        .expect("send");

    let mut buf = [0u8; astar_dmr::master::MAX_DATAGRAM];
    while let Ok((n, _)) = sock.recv_from(&mut buf) {
        assert_ne!(&buf[..4], b"DMRD", "the master echoed our own frame");
        let _ = n;
    }
    handle.shutdown();
}

#[test]
fn an_unauthenticated_peer_cannot_inject_a_frame() {
    // hblink.py gates DMRD on `_peer_id in self._peers and CONNECTION == 'YES'
    // and SOCKADDR == _sockaddr`. A fixture without that check would let a
    // client pass a test no real master would let it pass.
    let m = Master::bind("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind");
    let addr = m.local_addr();
    let handle = m.run();

    let listener = client();
    let mut f = fsm(3_153_591);
    link(&listener, addr, &mut f);

    let stranger = client();
    let packet = wire::DataPacket {
        seq: 0,
        src_id: 999,
        dst_id: TG,
        peer_id: 999,
        slot: Timeslot::Ts2,
        call_type: wire::CallType::Group,
        frame_type: wire::FrameType::VoiceSync,
        stream_id: [2, 2, 2, 2],
        burst: [0u8; wire::BURST_LEN],
        ber: 0,
        rssi: 0,
    };
    stranger
        .send_to(&wire::data(&packet).expect("ids in range"), addr)
        .expect("send");

    let mut buf = [0u8; astar_dmr::master::MAX_DATAGRAM];
    while let Ok((n, _)) = listener.recv_from(&mut buf) {
        assert_ne!(&buf[..4], b"DMRD", "an unauthenticated frame was relayed");
        let _ = n;
    }
    handle.shutdown();
}

#[test]
fn the_parrot_replays_a_transmission_to_its_sender() {
    let m = Master::bind_parrot("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind");
    let addr = m.local_addr();
    let handle = m.run();

    let sock = client();
    let mut f = fsm(3_153_591);
    link(&sock, addr, &mut f);

    let mut sent = Vec::new();
    for seq in 0..4u8 {
        let end = seq == 3;
        let packet = wire::DataPacket {
            seq,
            src_id: 3_153_591,
            dst_id: TG,
            peer_id: 3_153_591,
            slot: Timeslot::Ts2,
            call_type: wire::CallType::Group,
            frame_type: if end {
                wire::FrameType::DataSync { data_type: 0x02 }
            } else {
                wire::FrameType::Voice { n: seq }
            },
            stream_id: [7, 7, 7, 7],
            burst: [seq; wire::BURST_LEN],
            ber: 0,
            rssi: 0,
        };
        sent.push(wire::data(&packet).expect("ids in range").to_vec());
        sock.send_to(sent.last().expect("just pushed"), addr)
            .expect("send");
    }

    let mut got = Vec::new();
    let mut buf = [0u8; astar_dmr::master::MAX_DATAGRAM];
    let deadline = Instant::now() + Duration::from_secs(3);
    while got.len() < sent.len() && Instant::now() < deadline {
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        if &buf[..4] == b"DMRD" {
            got.push(buf[..n].to_vec());
        }
    }
    assert_eq!(got, sent, "the parrot replayed the transmission verbatim");
    handle.shutdown();
}

#[test]
fn a_login_failure_is_reported_as_a_login_failure() {
    let m = Master::bind("127.0.0.1:0".parse().expect("v4"), PASSWORD).expect("bind");
    let addr = m.local_addr();
    let handle = m.run();

    let sock = client();
    // The first thing back from a master is an RPTACK carrying a salt, and it
    // is ten bytes -- the same shape as the RPTACK that later acknowledges a
    // step. An FSM that read the salt as an acknowledgement, or the ack as a
    // refusal, would fail a login that was about to succeed.
    let mut f = fsm(3_153_591);
    for d in f.connect(Instant::now()) {
        sock.send_to(&d, addr).expect("send");
    }
    let mut buf = [0u8; astar_dmr::master::MAX_DATAGRAM];
    let (n, _) = sock.recv_from(&mut buf).expect("an ack with a salt");
    assert_eq!(&buf[..6], b"RPTACK");
    assert_eq!(n, 10);
    assert_ne!(
        f.on_packet(&buf[..n], Instant::now()),
        FsmAction::Failed(FailureStage::Login)
    );
    handle.shutdown();
}
