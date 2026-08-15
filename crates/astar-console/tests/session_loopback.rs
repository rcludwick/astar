// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration test: a real `ConsoleSession` against a fake web-transceiver
//! peer + stub audio backend. Offline, deterministic (iax-dd42).

use std::net::UdpSocket;
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_console::{CallStatus, ConsoleConfig, ConsoleSession};
use astar_iax::CodecPolicy;
use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};

// ---- stub audio backend (no real devices) -------------------------------

struct StubHandle;
impl StreamHandle for StubHandle {
    fn stop(self: Box<Self>) {}
    fn pause(&self) -> Result<(), AudioError> {
        Ok(())
    }
    fn resume(&self) -> Result<(), AudioError> {
        Ok(())
    }
}

fn dev(dir: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction: dir,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

struct StubBackend;
impl AudioBackend for StubBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "stub-in"),
            dev(Direction::Output, "stub-out"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "stub-in"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "stub-out"))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _s: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(StubHandle))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _s: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(StubHandle))
    }
}

// ---- fake web-transceiver peer ------------------------------------------

fn encode_iax(
    server: u16,
    client: u16,
    oseq: u8,
    iseq: u8,
    cmd: IaxCommand,
    ies: Ies<'_>,
) -> Vec<u8> {
    let f = Frame::Full(Box::new(FullFrame {
        source_call: server,
        dest_call: client,
        retransmission: false,
        timestamp: 0,
        oseqno: oseq,
        iseqno: iseq,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(cmd),
        ies,
        payload: &[],
    }));
    let mut out = Vec::with_capacity(128);
    encode(&f, &mut out).expect("encode");
    out
}

fn encode_ack(received: &FullFrame, server: u16) -> Vec<u8> {
    let f = Frame::Full(Box::new(FullFrame {
        source_call: server,
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
    encode(&f, &mut out).expect("encode ack");
    out
}

/// Complete the WT handshake: NEW -> AUTHREQ, AUTHREP -> ACCEPT, ACK everything,
/// exit on HANGUP or read timeout. ACCEPT alone drives the client to Answered;
/// the web-transceiver path has no separate ANSWER frame.
fn run_wt_peer(peer: &UdpSocket) {
    const SERVER: u16 = 21;
    let mut buf = [0u8; 4096];
    let mut sent_authreq = false;
    let mut sent_accept = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let Ok((n, src)) = peer.recv_from(&mut buf) else {
            break;
        };
        let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
            continue;
        };
        let client = ff.source_call;
        let _ = peer.send_to(&encode_ack(&ff, SERVER), src);
        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
                sent_authreq = true;
                let authreq = encode_iax(
                    SERVER,
                    client,
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
                    SERVER,
                    client,
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

/// Like `run_wt_peer`, but rejects the call after AUTHREP (sends IAX `Reject`
/// instead of `Accept`) so the client never reaches Answered — exercising the
/// `Failed` status path.
fn run_reject_peer(peer: &UdpSocket) {
    const SERVER: u16 = 22;
    let mut buf = [0u8; 4096];
    let mut sent_authreq = false;
    let mut rejected = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let Ok((n, src)) = peer.recv_from(&mut buf) else {
            break;
        };
        let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
            continue;
        };
        let client = ff.source_call;
        let _ = peer.send_to(&encode_ack(&ff, SERVER), src);
        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
                sent_authreq = true;
                let authreq = encode_iax(
                    SERVER,
                    client,
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
            Subclass::Iax(IaxCommand::AuthRep) if !rejected => {
                rejected = true;
                let reject = encode_iax(
                    SERVER,
                    client,
                    1,
                    ff.oseqno.wrapping_add(1),
                    IaxCommand::Reject,
                    Ies {
                        cause: Some("test reject"),
                        ..Ies::empty()
                    },
                );
                let _ = peer.send_to(&reject, src);
            }
            Subclass::Iax(IaxCommand::Hangup) | Subclass::Control(ControlSubclass::Hangup) => break,
            _ => {}
        }
    }
}

// ---- the tests ----------------------------------------------------------

#[test]
fn rejected_dial_reaches_failed_not_hangup() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let addr = peer.local_addr().unwrap();
    let peer_thread = thread::spawn(move || run_reject_peer(&peer));

    let mut session = ConsoleSession::new();
    let cfg = ConsoleConfig {
        node: "55553".into(),
        calling_node: "55553".into(),
        secret: "allstar".into(),
        name: "reject-test".into(),
        input_device: None,
        output_device: None,
        codec_policy: CodecPolicy::default(),
    };
    session
        .connect(Box::new(StubBackend), addr, cfg)
        .expect("connect");

    // Poll until the call resolves; a reject must land as Failed, never Answered.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut final_status = None;
    while Instant::now() < deadline {
        match session.snapshot().status {
            CallStatus::Failed { .. } => {
                final_status = Some("failed");
                break;
            }
            CallStatus::Hangup { .. } => {
                final_status = Some("hangup");
                break;
            }
            CallStatus::Answered => {
                final_status = Some("answered");
                break;
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        final_status,
        Some("failed"),
        "a rejected dial must surface as CallStatus::Failed (never Answered/Hangup)"
    );

    session.disconnect().ok();
    peer_thread.join().expect("peer joined");
}

#[test]
fn session_connects_answers_and_ptt_reflects() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let addr = peer.local_addr().unwrap();
    let peer_thread = thread::spawn(move || run_wt_peer(&peer));

    let mut session = ConsoleSession::new();
    let cfg = ConsoleConfig {
        node: "55553".into(),
        calling_node: "55553".into(),
        secret: "allstar".into(),
        name: "console-test".into(),
        input_device: None,
        output_device: None,
        codec_policy: CodecPolicy::default(),
    };
    // Inject the fake peer address directly (no DNS in tests).
    session
        .connect(Box::new(StubBackend), addr, cfg)
        .expect("connect");

    // Drive to Answered by polling the snapshot.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut answered = false;
    while Instant::now() < deadline {
        if matches!(session.snapshot().status, CallStatus::Answered) {
            answered = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(answered, "session should reach Answered");

    // The flat snapshot mirrors the active call's negotiated codec (iax-3e53).
    // The peer's ACCEPT carried no FORMAT IE, so negotiation lands on the
    // policy's preference: G.711 µ-law. It is published a beat after the
    // status flips to Answered, so poll for it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut negotiated = None;
    while Instant::now() < deadline {
        negotiated = session.snapshot().negotiated_format;
        if negotiated.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        negotiated,
        Some(astar_iax_core::VoiceFormat::G711U),
        "answered snapshot must carry the negotiated codec"
    );

    // A second connect must be rejected.
    assert!(
        session
            .connect(
                Box::new(StubBackend),
                addr,
                ConsoleConfig {
                    node: "55553".into(),
                    calling_node: "55553".into(),
                    secret: "allstar".into(),
                    name: "x".into(),
                    input_device: None,
                    output_device: None,
                    codec_policy: CodecPolicy::default(),
                }
            )
            .is_err()
    );

    session.set_ptt(true).expect("set_ptt");
    assert!(session.snapshot().ptt, "PTT state reflected");

    session.disconnect().expect("disconnect");
    assert_eq!(session.snapshot().status, CallStatus::Idle);

    // Levels report silence once the call is gone (Fix 1: no stale meters).
    let after = session.snapshot();
    assert!(
        (after.tx_level_db + 60.0).abs() < 1e-6,
        "tx silent after disconnect"
    );
    assert!(
        (after.rx_level_db + 60.0).abs() < 1e-6,
        "rx silent after disconnect"
    );
    // No active call → no negotiated codec (iax-3e53).
    assert_eq!(after.negotiated_format, None);

    peer_thread.join().expect("peer joined");
}
