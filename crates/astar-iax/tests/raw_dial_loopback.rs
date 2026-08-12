// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Loopback regression for `astar_iax::dial_raw` (iax-64b6): the headless
//! raw-frame outbound dial. A hand-rolled UDP peer plays the callee side of a
//! Standard IAX2 NEW (`CALLED_NUMBER`="s") — replies ACCEPT (which the outbound
//! FSM treats as answered) and ACKs reliably — and the test asserts:
//!   1. `dial_raw` reaches `CallEvent::Answered`.
//!   2. After answered, keying PTT + pushing a PCM frame into `tx_frames`
//!      puts a VOICE/mini datagram on the wire at the peer.
//!   3. `call.hangup` tears the runtime down cleanly.
//!
//! No audio devices: `dial_raw` exposes the call's frame channels directly, so
//! this needs no audio backend at all (unlike `wt_loopback.rs`'s Manager path).

use std::net::UdpSocket;
use std::thread;
use std::time::{Duration, Instant};

use astar_iax::{CallEvent, CallMode, dial_raw};

use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};

fn encode_iax(
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

/// ACK a received full frame: mirror call numbers, echo timestamp.
fn encode_ack(received: &FullFrame, server_call: u16) -> Vec<u8> {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: server_call,
        dest_call: received.source_call,
        retransmission: false,
        timestamp: received.timestamp,
        oseqno: received.iseqno,
        iseqno: received.oseqno.wrapping_add(1),
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::Ack),
        ies: Ies::empty(),
        payload: &[],
    }));
    let mut out = Vec::with_capacity(64);
    encode(&frame, &mut out).expect("ack encodes");
    out
}

/// What the fake peer observed / captured.
#[derive(Default)]
struct PeerReport {
    /// `CALLED_NUMBER` from the NEW (proves the Standard dial dest).
    called_number: Option<String>,
    /// True once a VOICE (full or mini) datagram is seen.
    saw_voice: bool,
}

/// Fake callee: completes a Standard handshake (NEW → AUTHREQ, AUTHREP →
/// ACCEPT), ACKs full frames so the client makes progress, captures the NEW's
/// `CALLED_NUMBER`, and flags the first VOICE/mini frame it receives.
/// Reports back over `report_tx` on each interesting event and exits on HANGUP /
/// read timeout.
fn run_fake_peer(peer: &UdpSocket, report_tx: &std::sync::mpsc::Sender<PeerReport>) {
    const SERVER_CALL: u16 = 7;
    let mut buf = [0u8; 4096];
    let mut sent_authreq = false;
    let mut sent_accept = false;
    let mut reported_voice = false;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Ok((n, src)) = peer.recv_from(&mut buf) else {
            break; // read timeout: client likely gone
        };
        let Ok(frame) = parse_lenient(&buf[..n]) else {
            continue;
        };
        let ff = match frame {
            Frame::Full(ff) => ff,
            // A mini frame IS voice by definition.
            Frame::Mini(_) => {
                if !reported_voice {
                    reported_voice = true;
                    let _ = report_tx.send(PeerReport {
                        saw_voice: true,
                        ..PeerReport::default()
                    });
                }
                continue;
            }
        };

        if ff.frame_type == FrameType::Voice && !reported_voice {
            reported_voice = true;
            let _ = report_tx.send(PeerReport {
                saw_voice: true,
                ..PeerReport::default()
            });
        }

        let client_call = ff.source_call;
        if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
            continue;
        }

        // ACK every non-ACK full frame so the client stops retransmitting.
        let ack = encode_ack(&ff, SERVER_CALL);
        let _ = peer.send_to(&ack, src);

        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
                let _ = report_tx.send(PeerReport {
                    called_number: ff.ies.called_number.map(String::from),
                    ..PeerReport::default()
                });
                sent_authreq = true;
                let authreq = encode_iax(
                    SERVER_CALL,
                    client_call,
                    0,
                    ff.oseqno.wrapping_add(1),
                    IaxCommand::AuthReq,
                    Ies {
                        challenge: Some("c0ffee"),
                        authmethods: Some(2),
                        ..Ies::empty()
                    },
                );
                let _ = peer.send_to(&authreq, src);
            }
            Subclass::Iax(IaxCommand::AuthRep) if !sent_accept => {
                sent_accept = true;
                let accept = encode_iax(
                    SERVER_CALL,
                    client_call,
                    1,
                    ff.oseqno.wrapping_add(1),
                    IaxCommand::Accept,
                    Ies::empty(),
                );
                let _ = peer.send_to(&accept, src);
            }
            Subclass::Iax(IaxCommand::Hangup) | Subclass::Control(ControlSubclass::Hangup) => break,
            _ => {}
        }
    }
}

#[test]
fn dial_raw_reaches_answered_and_tx_frame_hits_the_wire() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let (report_tx, report_rx) = std::sync::mpsc::channel::<PeerReport>();
    let peer_thread = thread::spawn(move || run_fake_peer(&peer, &report_tx));

    // Headless raw dial: no audio backend, frame channels exposed directly.
    let raw = dial_raw(peer_addr, "echo", "s", "", CallMode::Standard).expect("dial_raw spawns");

    // 1. The dial must reach Answered.
    let answered = {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut got = false;
        while Instant::now() < deadline {
            match raw.events.recv_timeout(Duration::from_millis(200)) {
                Ok(CallEvent::Answered { .. }) => {
                    got = true;
                    break;
                }
                Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        got
    };
    assert!(answered, "dial_raw must reach CallEvent::Answered");

    // 2. Key PTT and push PCM frames; they must reach the wire as VOICE.
    raw.call.set_ptt(true).expect("set_ptt should not error");
    for _ in 0..12 {
        raw.tx_frames
            .send(vec![10_000_i16; 160])
            .expect("tx_frames.send must not error while the call is up");
        thread::sleep(Duration::from_millis(20));
    }

    // Collect peer observations: CALLED_NUMBER and whether voice reached it.
    let mut called_number: Option<String> = None;
    let mut saw_voice = false;
    let collect_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < collect_deadline {
        match report_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(r) => {
                if r.called_number.is_some() {
                    called_number = r.called_number;
                }
                saw_voice |= r.saw_voice;
                if saw_voice {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    assert_eq!(
        called_number.as_deref(),
        Some("s"),
        "Standard dial must send CALLED_NUMBER=\"s\""
    );
    assert!(
        saw_voice,
        "a keyed tx_frames.send must put a VOICE/mini frame on the wire"
    );

    // 3. Clean teardown joins the runtime thread.
    raw.call.hangup(None).expect("hangup should join cleanly");
    peer_thread.join().expect("peer thread joined");
}
