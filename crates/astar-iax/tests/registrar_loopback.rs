// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end test: spawn `Registrar`, point it at a hand-rolled UDP "registrar"
//! that responds REGREQ → REGAUTH → REGREQ(+MD5) → REGACK, then deregister.

#![allow(
    clippy::too_many_lines,
    clippy::manual_let_else,
    clippy::manual_assert,
    clippy::needless_continue,
    clippy::match_same_arms
)]

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_iax::{Registrar, RegistrationEvent};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::Secret;
use astar_iax_core::subclass::{FrameType, IaxCommand};

#[test]
fn registrar_handshake_reaches_registered_and_released() {
    // Fake registrar: bind a UDP port and respond to the conversation.
    let registrar_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    registrar_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let registrar_addr = registrar_sock.local_addr().unwrap();

    let fake_server = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // Step 1: expect REGREQ.
        let (n, client) = registrar_sock.recv_from(&mut buf).expect("REGREQ recv");
        let req = parse_lenient(&buf[..n]).expect("parse REGREQ");
        let client_call = match &req {
            Frame::Full(ff) => ff.source_call,
            Frame::Mini(_) => panic!("non-full frame"),
        };
        // Reply REGAUTH (MD5, challenge "c0ffee").
        let regauth_ies = Ies {
            authmethods: Some(2),
            challenge: Some("c0ffee"),
            ..Ies::empty()
        };
        let regauth = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 1,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAuth),
            ies: regauth_ies,
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regauth, &mut bytes).expect("encode REGAUTH");
        registrar_sock.send_to(&bytes, client).unwrap();

        // Step 2: expect REGREQ(+MD5). (Drain ACKs / dup REGREQ until we see one with md5_result.)
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() > deadline {
                panic!("did not receive REGREQ(+MD5) within deadline");
            }
            let (n, _) = match registrar_sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let Ok(req2) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if let Frame::Full(ff) = &req2
                && matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq))
            {
                // Real Asterisk matches an in-progress registration by
                // dest_call. The post-auth REGREQ MUST be addressed to the
                // registrar's scallno (5000) — a dest_call=0 frame (the
                // pre-SetPeerCall bug) would be silently dropped, so reject
                // it here too and keep waiting (the client then times out).
                if ff.dest_call != 5000 {
                    continue;
                }
                let mut b = Vec::new();
                ff.ies.encode(&mut b).unwrap();
                let ies = Ies::parse(&b).unwrap();
                if ies.md5_result.is_some() {
                    break;
                }
            }
        }
        // Reply REGACK with refresh=60.
        let regack_ies = Ies {
            refresh: Some(60),
            ..Ies::empty()
        };
        let regack = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 1,
            iseqno: 2,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAck),
            ies: regack_ies,
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regack, &mut bytes).expect("encode REGACK");
        registrar_sock.send_to(&bytes, client).unwrap();

        // Step 3: expect REGREL.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() > deadline {
                panic!("did not receive REGREL within deadline");
            }
            let (n, _) = match registrar_sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let Ok(rel) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if let Frame::Full(ff) = &rel
                && matches!(ff.subclass, Subclass::Iax(IaxCommand::RegRel))
            {
                break;
            }
        }
        // Reply ACK.
        let ack = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: 3,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::Ack),
            ies: Ies::empty(),
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&ack, &mut bytes).expect("encode ACK");
        registrar_sock.send_to(&bytes, client).unwrap();
    });

    let (registration, events) =
        Registrar::new(registrar_addr, "u", Arc::new(Secret::new("hunter2".into())))
            .register()
            .expect("spawn registration");

    // Wait for Registered.
    let mut got_registered = false;
    let mut got_released = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(RegistrationEvent::Registered { .. }) => {
                got_registered = true;
                // Trigger deregister.
                registration.deregister().expect("deregister");
                // After deregister, the thread is joined; events may still drain.
                while let Ok(ev) = events.recv_timeout(Duration::from_millis(200)) {
                    if matches!(ev, RegistrationEvent::Released) {
                        got_released = true;
                    }
                }
                break;
            }
            Ok(RegistrationEvent::Failed(r)) => panic!("registration failed: {r:?}"),
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    let _ = fake_server.join();
    assert!(got_registered, "did not receive Registered event");
    assert!(got_released, "did not receive Released event");
}

/// iax-177d: a refresh must open a BRAND-NEW transaction — fresh seqnos,
/// `dest_call = 0`, full handshake re-run. The old code reused the dead
/// initial transaction (old peer call + continued seqnos); a real registrar
/// has destroyed that call after REGACK, so it dropped the refresh REGREQ and
/// the registration died at its very first refresh (~60 s in), while
/// `is_registered` stayed true. This registrar grants refresh=1 s, then
/// validates the refresh round byte-for-byte.
#[test]
fn refresh_opens_a_fresh_transaction_and_reauths() {
    let registrar_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    registrar_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let registrar_addr = registrar_sock.local_addr().unwrap();

    let fake_server = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // ── Round 1: the initial registration (REGAUTH scallno 5000) ──
        let (n, client) = registrar_sock.recv_from(&mut buf).expect("REGREQ recv");
        let req = parse_lenient(&buf[..n]).expect("parse REGREQ");
        let client_call = match &req {
            Frame::Full(ff) => ff.source_call,
            Frame::Mini(_) => panic!("non-full frame"),
        };
        let regauth = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 1,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAuth),
            ies: Ies {
                authmethods: Some(2),
                challenge: Some("c0ffee"),
                ..Ies::empty()
            },
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regauth, &mut bytes).expect("encode REGAUTH");
        registrar_sock.send_to(&bytes, client).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() > deadline {
                panic!("no REGREQ(+MD5) in round 1");
            }
            let Ok((n, _)) = registrar_sock.recv_from(&mut buf) else {
                continue;
            };
            let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq)) && ff.dest_call == 5000 {
                let mut b = Vec::new();
                ff.ies.encode(&mut b).unwrap();
                if Ies::parse(&b).unwrap().md5_result.is_some() {
                    break;
                }
            }
        }
        // Grant a 1-second refresh so the refresh round happens fast.
        let regack = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 1,
            iseqno: 2,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAck),
            ies: Ies {
                refresh: Some(1),
                ..Ies::empty()
            },
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regack, &mut bytes).expect("encode REGACK");
        registrar_sock.send_to(&bytes, client).unwrap();

        // ── Round 2: the refresh MUST arrive as a fresh transaction ──
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if Instant::now() > deadline {
                panic!("no refresh REGREQ within deadline");
            }
            let Ok((n, _)) = registrar_sock.recv_from(&mut buf) else {
                continue;
            };
            let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if !matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq)) {
                continue; // ACK of our REGACK etc.
            }
            let mut b = Vec::new();
            ff.ies.encode(&mut b).unwrap();
            if Ies::parse(&b).unwrap().md5_result.is_some() {
                continue; // late round-1 retransmit
            }
            // THE assertion this test exists for: a real registrar destroyed
            // call 5000 after the REGACK — a refresh addressed to it (or with
            // continued seqnos) is silently dropped.
            assert_eq!(
                ff.dest_call, 0,
                "refresh REGREQ reused the dead transaction's peer call"
            );
            assert_eq!(
                ff.oseqno, 0,
                "refresh REGREQ reused the dead transaction's seqnos"
            );
            break;
        }
        // Fresh challenge from a NEW registrar scallno (6000): proves the
        // client re-learns the peer call instead of reusing 5000.
        let regauth2 = Frame::Full(Box::new(FullFrame {
            source_call: 6000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 1,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAuth),
            ies: Ies {
                authmethods: Some(2),
                challenge: Some("beefcafe"),
                ..Ies::empty()
            },
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regauth2, &mut bytes).expect("encode REGAUTH round 2");
        registrar_sock.send_to(&bytes, client).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() > deadline {
                panic!("no post-auth REGREQ in the refresh round");
            }
            let Ok((n, _)) = registrar_sock.recv_from(&mut buf) else {
                continue;
            };
            let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq)) && ff.dest_call == 6000 {
                let mut b = Vec::new();
                ff.ies.encode(&mut b).unwrap();
                if Ies::parse(&b).unwrap().md5_result.is_some() {
                    break;
                }
            }
        }
        // Ack the refresh with a long grant so the test ends quietly.
        let regack2 = Frame::Full(Box::new(FullFrame {
            source_call: 6000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 1,
            iseqno: 2,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAck),
            ies: Ies {
                refresh: Some(60),
                ..Ies::empty()
            },
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regack2, &mut bytes).expect("encode REGACK round 2");
        registrar_sock.send_to(&bytes, client).unwrap();
    });

    let (_registration, events) =
        Registrar::new(registrar_addr, "u", Arc::new(Secret::new("hunter2".into())))
            .register()
            .expect("spawn registration");

    // Expect: Registered, then Refreshing, then Registered again — never Failed.
    let mut registered_count = 0;
    let mut got_refreshing = false;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && registered_count < 2 {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(RegistrationEvent::Registered { .. }) => registered_count += 1,
            Ok(RegistrationEvent::Refreshing) => got_refreshing = true,
            Ok(RegistrationEvent::Failed(r)) => panic!("refresh failed the registration: {r:?}"),
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    let _ = fake_server.join();
    assert!(
        got_refreshing,
        "no Refreshing event before the refresh round"
    );
    assert_eq!(
        registered_count, 2,
        "the refresh round did not complete a second registration"
    );
}

/// iax-3b9d: a VNAK during registration must trigger a retransmit (RFC 5456
/// §6.9.3), not fail the registration. The fake registrar answers the
/// post-auth REGREQ(+MD5) with a VNAK (iseqno=0, "resend everything"); the
/// client must re-send the in-flight REGREQ(+MD5) rather than emit
/// `RegistrationEvent::Failed`. Once the resend arrives, the registrar
/// completes with REGACK and the registration reaches `Registered`.
#[test]
fn registrar_vnak_triggers_retransmit_not_failure() {
    let registrar_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    registrar_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let registrar_addr = registrar_sock.local_addr().unwrap();

    let fake_server = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // Step 1: expect REGREQ.
        let (n, client) = registrar_sock.recv_from(&mut buf).expect("REGREQ recv");
        let req = parse_lenient(&buf[..n]).expect("parse REGREQ");
        let client_call = match &req {
            Frame::Full(ff) => ff.source_call,
            Frame::Mini(_) => panic!("non-full frame"),
        };
        // Reply REGAUTH (MD5, challenge "c0ffee").
        let regauth_ies = Ies {
            authmethods: Some(2),
            challenge: Some("c0ffee"),
            ..Ies::empty()
        };
        let regauth = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 1,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAuth),
            ies: regauth_ies,
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regauth, &mut bytes).expect("encode REGAUTH");
        registrar_sock.send_to(&bytes, client).unwrap();

        // Step 2: expect the FIRST post-auth REGREQ(+MD5). Answer it with a
        // VNAK (iseqno=0 → resend everything still in flight) instead of a
        // REGACK. The OLD code mapped VNAK → DeliveryFailed and killed the
        // registration here.
        let deadline = Instant::now() + Duration::from_secs(2);
        let first_oseqno = loop {
            if Instant::now() > deadline {
                panic!("did not receive first REGREQ(+MD5) within deadline");
            }
            let (n, _) = match registrar_sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let Ok(req2) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if let Frame::Full(ff) = &req2
                && matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq))
                && ff.dest_call == 5000
            {
                let mut b = Vec::new();
                ff.ies.encode(&mut b).unwrap();
                let ies = Ies::parse(&b).unwrap();
                if ies.md5_result.is_some() {
                    break ff.oseqno;
                }
            }
        };

        // Send VNAK(iseqno=0): "resend all unacked outbound frames".
        let vnak = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 1,
            iseqno: 0,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::Vnak),
            ies: Ies::empty(),
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&vnak, &mut bytes).expect("encode VNAK");
        registrar_sock.send_to(&bytes, client).unwrap();

        // Step 3: expect the RETRANSMITTED REGREQ(+MD5) (retransmission bit
        // set, same oseqno). This proves the client resent rather than failed.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() > deadline {
                panic!("did not receive retransmitted REGREQ(+MD5) after VNAK");
            }
            let (n, _) = match registrar_sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let Ok(req3) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if let Frame::Full(ff) = &req3
                && matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq))
                && ff.dest_call == 5000
                && ff.retransmission
                && ff.oseqno == first_oseqno
            {
                let mut b = Vec::new();
                ff.ies.encode(&mut b).unwrap();
                let ies = Ies::parse(&b).unwrap();
                if ies.md5_result.is_some() {
                    break;
                }
            }
        }

        // Reply REGACK with refresh=60.
        let regack_ies = Ies {
            refresh: Some(60),
            ..Ies::empty()
        };
        let regack = Frame::Full(Box::new(FullFrame {
            source_call: 5000,
            dest_call: client_call,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: u8::try_from(u16::from(first_oseqno) + 1).unwrap_or(2),
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::RegAck),
            ies: regack_ies,
            payload: &[],
        }));
        let mut bytes = Vec::new();
        encode(&regack, &mut bytes).expect("encode REGACK");
        registrar_sock.send_to(&bytes, client).unwrap();
    });

    let (_registration, events) =
        Registrar::new(registrar_addr, "u", Arc::new(Secret::new("hunter2".into())))
            .register()
            .expect("spawn registration");

    let mut got_registered = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(RegistrationEvent::Registered { .. }) => {
                got_registered = true;
                break;
            }
            Ok(RegistrationEvent::Failed(r)) => panic!("VNAK wrongly failed registration: {r:?}"),
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    let _ = fake_server.join();
    assert!(
        got_registered,
        "registration did not reach Registered after VNAK retransmit"
    );
}
