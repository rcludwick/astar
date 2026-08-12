// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Drive the Session through AUTHREQ + ACCEPT against a fake UDP peer
//! and assert the FSM reaches Active.

use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use astar_conformance::driver::Session;
use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::fsm::{AppCommand, SessionState};
use astar_iax_core::subclass::{FrameType, IaxCommand};

fn test_creds() -> Credentials {
    Credentials {
        username: "astartest".into(),
        password: Arc::new(Secret::new("supersecret".into())),
        allowed_methods: AuthMethods::MD5,
    }
}

fn encode_frame(
    server_call: u16,
    client_call: u16,
    oseqno: u8,
    iseqno: u8,
    cmd: IaxCommand,
    ies: Ies<'_>,
) -> Vec<u8> {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: server_call,
        dest_call: client_call,
        retransmission: false,
        timestamp: 0,
        oseqno,
        iseqno,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(cmd),
        ies,
        payload: &[],
    }));
    let mut out = Vec::with_capacity(128);
    encode(&frame, &mut out).expect("test frame must encode");
    out
}

#[test]
fn driver_reaches_active_against_fake_server() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let server_addr = server.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let (_n, client) = server.recv_from(&mut buf).expect("server saw NEW");
        let authreq = encode_frame(
            7,
            1,
            0,
            1,
            IaxCommand::AuthReq,
            Ies {
                challenge: Some("c0ffee"),
                authmethods: Some(2),
                ..Ies::empty()
            },
        );
        server.send_to(&authreq, client).unwrap();
        let (_n, _) = server.recv_from(&mut buf).expect("server saw AUTHREP");
        let accept = encode_frame(7, 1, 1, 2, IaxCommand::Accept, Ies::empty());
        server.send_to(&accept, client).unwrap();
    });

    let mut session = Session::connect(server_addr, test_creds()).unwrap();
    session.send_app_command(AppCommand::StartCall {
        dest: "s".into(),
        now: Instant::now(),
    });
    session
        .run_event_loop_until(
            |fsm| matches!(fsm.state(), SessionState::Active(_)),
            Duration::from_secs(2),
        )
        .expect("reached Active");

    server_thread.join().unwrap();
    assert!(matches!(session.state(), SessionState::Active(_)));
}

/// iax-3b9d: a VNAK from the peer must trigger a retransmit (RFC 5456
/// §6.9.3), not a `DeliveryFailed` that kills the call. The fake server
/// answers the initial NEW with a VNAK (iseqno=0 → resend everything in
/// flight); the driver must re-send the NEW (retransmission bit set) rather
/// than fail. After the resend, the handshake proceeds normally to Active.
#[test]
fn driver_retransmits_new_on_vnak_instead_of_failing() {
    use astar_iax_core::frame::parse_lenient;

    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let server_addr = server.local_addr().unwrap();

    let server_thread = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // Step 1: receive the initial NEW.
        let (n, client) = server.recv_from(&mut buf).expect("server saw NEW");
        let new = parse_lenient(&buf[..n]).expect("parse NEW");
        let new_oseqno = match &new {
            Frame::Full(ff) => ff.oseqno,
            Frame::Mini(_) => panic!("NEW was a mini frame"),
        };

        // Step 2: reply VNAK(iseqno=0) — "resend all unacked outbound frames".
        // OLD code mapped this to DeliveryFailed and killed the call.
        let vnak = encode_frame(7, 1, 0, 0, IaxCommand::Vnak, Ies::empty());
        server.send_to(&vnak, client).unwrap();

        // Step 3: expect the retransmitted NEW (retransmission bit, same
        // oseqno). This proves the driver resent rather than failed.
        let mut saw_retransmit = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let Ok((n, _)) = server.recv_from(&mut buf) else {
                continue;
            };
            if let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n])
                && matches!(ff.subclass, Subclass::Iax(IaxCommand::New))
                && ff.retransmission
                && ff.oseqno == new_oseqno
            {
                saw_retransmit = true;
                break;
            }
        }
        assert!(saw_retransmit, "driver did not retransmit NEW after VNAK");

        // Step 4: complete the handshake normally.
        let authreq = encode_frame(
            7,
            1,
            0,
            1,
            IaxCommand::AuthReq,
            Ies {
                challenge: Some("c0ffee"),
                authmethods: Some(2),
                ..Ies::empty()
            },
        );
        server.send_to(&authreq, client).unwrap();
        let (_n, _) = server.recv_from(&mut buf).expect("server saw AUTHREP");
        let accept = encode_frame(7, 1, 1, 2, IaxCommand::Accept, Ies::empty());
        server.send_to(&accept, client).unwrap();
    });

    let mut session = Session::connect(server_addr, test_creds()).unwrap();
    session.send_app_command(AppCommand::StartCall {
        dest: "s".into(),
        now: Instant::now(),
    });
    session
        .run_event_loop_until(
            |fsm| matches!(fsm.state(), SessionState::Active(_)),
            Duration::from_secs(3),
        )
        .expect("reached Active after VNAK retransmit");

    server_thread.join().unwrap();
    assert!(matches!(session.state(), SessionState::Active(_)));
}
