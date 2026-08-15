// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! WT call-behavior regression suite, driven over the iax-42e9 [`Manager`]
//! (iax-64b6 P3). Migrated off the deleted high-level `Client`: each test stands
//! up a real `Manager` (its own per-call blocking mio thread) against a fake UDP
//! peer that completes the IAX2 handshake, then exercises the wire-level call
//! behaviors via `Manager::dial(DialSpec) + route(CallId, &MicId)` and the PTT /
//! DTMF / hangup control surface.
//!
//! The valuable part is the WIRE assertions (NEW/AUTHREP handshake, DTMF frames,
//! keepalive PING/PONG + RTT, VNAK resend, and mic-capture waking the runtime on
//! a quiet link). Those are preserved verbatim; only the driver moved from
//! `Client::builder().dial()` to `Manager::dial(...) + route(...)`.
//!
//! The crate's `test_support` backends live behind `#[cfg(test)]` inside the
//! library and are not reachable from an integration binary, so this file
//! defines self-contained backends (mirroring `manager_loopback.rs`'s house
//! style): they return fixed devices and hand back no-op stream handles, except
//! where a test needs the opened input sink (mic-capture) or a record of which
//! device ids were opened (device-selection).

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_iax::manager::DialSpec;
use astar_iax::{CallId, CallMode, CodecPolicy, IaxError, LinkMode, LinkSpec, Manager};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
    OutputSource, StreamConfig, StreamHandle,
};

use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};

// ---------------------------------------------------------------------------
// Stub audio backend (self-contained; same device-id strings the manager
// loopback tests use: inputs in:a/in:b, output out:s).
// ---------------------------------------------------------------------------

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
impl StubBackend {
    fn new() -> Self {
        Self
    }
}

impl AudioBackend for StubBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:a"),
            dev(Direction::Input, "in:b"),
            dev(Direction::Output, "out:s"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:a"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:s"))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        // Drop the sink: the test does not feed real mic samples.
        Ok(Box::new(StubHandle))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(StubHandle))
    }
}

// ---------------------------------------------------------------------------
// DialSpec helper (mirrors manager_loopback.rs's `spec`). A Standard call onto
// `out:s`, caller-id "wt-loopback", no secret unless overridden by the caller.
// ---------------------------------------------------------------------------

fn spec(id: u64, peer: std::net::SocketAddr) -> DialSpec {
    DialSpec {
        id: CallId::from_raw(id),
        node: "55553".to_string(),
        peer,
        output: OutputId::new("out:s"),
        caller_id: "wt-loopback".to_string(),
        secret: String::new(),
        mode: CallMode::Standard,
        dest: "55553".to_string(),
        frame_observer: None,
        codec_policy: CodecPolicy::default(),
    }
}

/// Poll the manager snapshot until `id` reports active (handshake completed).
fn wait_active(mgr: &Manager, id: CallId) -> bool {
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if mgr
            .snapshot()
            .calls
            .iter()
            .any(|c| c.id == id && c.is_active())
        {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

// ---------------------------------------------------------------------------
// Fake-peer frame helpers (adapted from astar-conformance/tests/driver_loopback).
// ---------------------------------------------------------------------------

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

/// Build an ACK for a received full frame: mirror call numbers, use the peer's
/// `oseqno` as our `iseqno`.
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

// ---------------------------------------------------------------------------
// Test A (cheap, must-have): dial twice with the same CallId -> CallInProgress.
// ---------------------------------------------------------------------------

#[test]
fn dial_twice_is_call_in_progress() {
    // Bind a UDP socket as the fake peer just to get a real addr to dial.
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let mut mgr = Manager::new(Box::new(StubBackend::new()));

    let _id = mgr.dial(spec(1, peer_addr)).unwrap();
    // A second dial under the SAME CallId is rejected at the pool.
    assert!(matches!(
        mgr.dial(spec(1, peer_addr)),
        Err(IaxError::CallInProgress)
    ));
}

// ---------------------------------------------------------------------------
// Test B (the real proof): full handshake + DTMF + hangup against a fake peer.
// ---------------------------------------------------------------------------

/// Parsed classification of a frame the fake peer received from the client.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    New,
    AuthRep,
    DtmfBegin,
    DtmfEnd,
    Hangup,
    Other,
}

fn classify(bytes: &[u8]) -> Option<Seen> {
    let frame = parse_lenient(bytes).ok()?;
    let Frame::Full(ff) = frame else {
        return Some(Seen::Other);
    };
    Some(match (ff.frame_type, &ff.subclass) {
        (FrameType::Iax, Subclass::Iax(IaxCommand::New)) => Seen::New,
        (FrameType::Iax, Subclass::Iax(IaxCommand::AuthRep)) => Seen::AuthRep,
        (FrameType::DtmfBegin, _) => Seen::DtmfBegin,
        (FrameType::DtmfEnd, _) => Seen::DtmfEnd,
        (FrameType::Iax, Subclass::Iax(IaxCommand::Hangup))
        | (FrameType::Control, Subclass::Control(ControlSubclass::Hangup)) => Seen::Hangup,
        _ => Seen::Other,
    })
}

/// Fake-peer event loop: completes the IAX2 handshake (NEW -> AUTHREQ,
/// AUTHREP -> ACCEPT), ACKs every full frame so the client makes forward
/// progress, reports each observed frame kind on `saw_tx`, and exits on HANGUP
/// (or read timeout).
fn run_fake_peer(peer: &UdpSocket, saw_tx: &std::sync::mpsc::Sender<Seen>) {
    const SERVER_CALL: u16 = 7;
    let mut buf = [0u8; 4096];
    let mut sent_authreq = false;
    let mut sent_accept = false;

    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let Ok((n, src)) = peer.recv_from(&mut buf) else {
            break; // read timeout: client likely gone
        };
        let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
            continue; // ignore mini/voice/unparseable frames
        };
        let client_call = ff.source_call;

        if let Some(kind) = classify(&buf[..n]) {
            let _ = saw_tx.send(kind);
        }

        // ACK every full frame so the client stops retransmitting and can make
        // forward progress (DTMF, hangup, etc.).
        let ack = encode_ack(&ff, SERVER_CALL);
        let _ = peer.send_to(&ack, src);

        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
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
fn full_call_round_trip_dtmf_and_hangup() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    // Channel back from the peer thread reporting which frame kinds it saw.
    let (saw_tx, saw_rx) = std::sync::mpsc::channel::<Seen>();

    let peer_thread = thread::spawn(move || run_fake_peer(&peer, &saw_tx));

    let mut mgr = Manager::new(Box::new(StubBackend::new()));
    let id = mgr.dial(spec(1, peer_addr)).unwrap();
    mgr.route(id, &MicId::new("in:a")).unwrap();

    // Drive until the manager snapshot reports the call active (answered).
    assert!(wait_active(&mgr, id), "call should reach active");

    // PTT. As of iax-d4e9 the FSM emits a RADIO_KEY control frame for key();
    // here we only assert the public API path succeeds (the FSM-level
    // emit/coalescing is covered by unit tests in astar-iax-core). To prove
    // *outbound signalling* reaches the peer we use DTMF, which the FSM also
    // implements (DtmfBegin + DtmfEnd reliable frames).
    mgr.key(id).expect("key should not error");
    mgr.send_dtmf(id, '5').expect("send_dtmf should not error");

    // Hang up; this also joins the per-call runtime thread.
    mgr.hangup(id, None).expect("hangup should join cleanly");

    // The peer thread exits once it sees the HANGUP (or read timeout).
    peer_thread.join().expect("peer thread joined");

    // Collect everything the peer observed.
    let mut seen: Vec<Seen> = Vec::new();
    while let Ok(s) = saw_rx.try_recv() {
        seen.push(s);
    }

    assert!(
        seen.contains(&Seen::New),
        "peer should receive NEW; saw {seen:?}"
    );
    assert!(
        seen.contains(&Seen::AuthRep),
        "peer should receive AUTHREP after challenge; saw {seen:?}"
    );
    assert!(
        seen.contains(&Seen::DtmfBegin),
        "peer should receive a DTMF begin frame; saw {seen:?}"
    );
    assert!(
        seen.contains(&Seen::Hangup),
        "peer should receive HANGUP; saw {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Test C (iax-a307): keepalive PING/PONG + RTT + VNAK resend on the wire.
// ---------------------------------------------------------------------------

/// What the keepalive fake peer observed from the client.
#[derive(Debug, PartialEq, Eq)]
enum KaSeen {
    ClientPing,
    ClientPong,
    DtmfRetransmit,
}

/// Like `encode_iax` but with an explicit timestamp (PONG must echo the
/// PING's, RFC 5456 §6.7.3).
fn encode_iax_ts(
    server_call: u16,
    client_call: u16,
    oseqno: u8,
    iseqno: u8,
    cmd: IaxCommand,
    ts: u32,
) -> Vec<u8> {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: server_call,
        dest_call: client_call,
        retransmission: false,
        timestamp: ts,
        oseqno,
        iseqno,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(cmd),
        ies: Ies::empty(),
        payload: &[],
    }));
    let mut out = Vec::with_capacity(64);
    encode(&frame, &mut out).expect("test frame must encode");
    out
}

/// Keepalive fake peer: handshake like `run_fake_peer`, answers client PINGs
/// with PONGs, probes the client's PONG path with its own PING, and VNAKs the
/// first `DtmfBegin` instead of `ACK`ing it — then reports the client's
/// retransmission. While the VNAK dance is pending it withholds ACKs for
/// first-transmission frames so a cumulative ACK can't release the client's
/// in-flight before the resend happens.
#[allow(clippy::too_many_lines)]
fn run_keepalive_peer(peer: &UdpSocket, saw_tx: &std::sync::mpsc::Sender<KaSeen>) {
    const SERVER_CALL: u16 = 9;
    let mut buf = [0u8; 4096];
    let mut sent_authreq = false;
    let mut sent_accept = false;
    let mut sent_peer_ping = false;
    let mut vnak_oseqno: Option<u8> = None;
    let mut retransmit_seen = false;
    let mut peer_oseqno: u8 = 0;

    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let Ok((n, src)) = peer.recv_from(&mut buf) else {
            break; // read timeout: client likely gone
        };
        let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
            continue;
        };
        let client_call = ff.source_call;
        if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
            continue;
        }

        // VNAK dance bookkeeping.
        if ff.frame_type == FrameType::DtmfBegin {
            if vnak_oseqno.is_none() {
                vnak_oseqno = Some(ff.oseqno);
                // iseqno = the NAK'd oseqno: "resend from here" (§6.9.3).
                let vnak = encode_iax(
                    SERVER_CALL,
                    client_call,
                    peer_oseqno,
                    ff.oseqno,
                    IaxCommand::Vnak,
                    Ies::empty(),
                );
                let _ = peer.send_to(&vnak, src);
                continue; // deliberately no ACK
            }
            if ff.retransmission && Some(ff.oseqno) == vnak_oseqno {
                retransmit_seen = true;
                let _ = saw_tx.send(KaSeen::DtmfRetransmit);
            }
        }
        // While waiting for the resend, don't cumulative-ACK anything new —
        // an ACK past the NAK'd oseqno would release the client's in-flight
        // and leave nothing to resend.
        if vnak_oseqno.is_some() && !retransmit_seen && !ff.retransmission {
            continue;
        }

        let ack = encode_ack(&ff, SERVER_CALL);
        let _ = peer.send_to(&ack, src);

        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
                sent_authreq = true;
                let authreq = encode_iax(
                    SERVER_CALL,
                    client_call,
                    peer_oseqno,
                    ff.oseqno.wrapping_add(1),
                    IaxCommand::AuthReq,
                    Ies {
                        challenge: Some("c0ffee"),
                        authmethods: Some(2),
                        ..Ies::empty()
                    },
                );
                peer_oseqno = peer_oseqno.wrapping_add(1);
                let _ = peer.send_to(&authreq, src);
            }
            Subclass::Iax(IaxCommand::AuthRep) if !sent_accept => {
                sent_accept = true;
                let accept = encode_iax(
                    SERVER_CALL,
                    client_call,
                    peer_oseqno,
                    ff.oseqno.wrapping_add(1),
                    IaxCommand::Accept,
                    Ies::empty(),
                );
                peer_oseqno = peer_oseqno.wrapping_add(1);
                let _ = peer.send_to(&accept, src);
                // Probe the client's PONG path right away.
                let ping = encode_iax(
                    SERVER_CALL,
                    client_call,
                    peer_oseqno,
                    ff.oseqno.wrapping_add(1),
                    IaxCommand::Ping,
                    Ies::empty(),
                );
                peer_oseqno = peer_oseqno.wrapping_add(1);
                sent_peer_ping = true;
                let _ = peer.send_to(&ping, src);
            }
            Subclass::Iax(IaxCommand::Ping) => {
                let _ = saw_tx.send(KaSeen::ClientPing);
                let pong = encode_iax_ts(
                    SERVER_CALL,
                    client_call,
                    peer_oseqno,
                    ff.oseqno.wrapping_add(1),
                    IaxCommand::Pong,
                    ff.timestamp,
                );
                peer_oseqno = peer_oseqno.wrapping_add(1);
                let _ = peer.send_to(&pong, src);
            }
            Subclass::Iax(IaxCommand::Pong) if sent_peer_ping => {
                let _ = saw_tx.send(KaSeen::ClientPong);
            }
            Subclass::Iax(IaxCommand::Hangup) | Subclass::Control(ControlSubclass::Hangup) => {
                break;
            }
            _ => {}
        }
    }
}

#[test]
fn keepalive_rtt_and_vnak_resend_on_the_wire() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (saw_tx, saw_rx) = std::sync::mpsc::channel::<KaSeen>();
    let peer_thread = thread::spawn(move || run_keepalive_peer(&peer, &saw_tx));

    let mut mgr = Manager::new(Box::new(StubBackend::new()));
    let id = mgr.dial(spec(1, peer_addr)).unwrap();
    mgr.route(id, &MicId::new("in:a")).unwrap();

    // Drive to active (answered).
    assert!(wait_active(&mgr, id), "call should reach active");

    // The keepalive timer fires at ~2s; the peer PONGs; poll for the RTT.
    let rtt_deadline = Instant::now() + Duration::from_secs(6);
    let mut rtt = None;
    while Instant::now() < rtt_deadline {
        rtt = mgr.rtt(id);
        if rtt.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        rtt.is_some(),
        "Manager::rtt() must be populated after a PING/PONG exchange"
    );

    // VNAK dance: the peer VNAKs the first DtmfBegin; the client must
    // retransmit (not fail the call).
    mgr.send_dtmf(id, '5').expect("send_dtmf should not error");
    let mut saw: Vec<KaSeen> = Vec::new();
    let collect_deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < collect_deadline {
        if let Ok(s) = saw_rx.recv_timeout(Duration::from_millis(200)) {
            saw.push(s);
        }
        if saw.contains(&KaSeen::DtmfRetransmit) {
            break;
        }
    }
    // The call must not have died on the VNAK — it stays active in the pool.
    assert!(
        wait_active(&mgr, id),
        "VNAK must not end the call; it must still be active"
    );
    assert!(
        saw.contains(&KaSeen::ClientPing),
        "client must send keepalive PINGs; saw {saw:?}"
    );
    assert!(
        saw.contains(&KaSeen::ClientPong),
        "client must answer the peer's PING with a PONG; saw {saw:?}"
    );
    assert!(
        saw.contains(&KaSeen::DtmfRetransmit),
        "client must answer VNAK with a retransmission; saw {saw:?}"
    );

    mgr.hangup(id, None).expect("hangup should join cleanly");
    peer_thread.join().expect("peer thread joined");
}

// ---------------------------------------------------------------------------
// Test D (iax-fd34, re-aimed): dial+route opens the selected devices.
//
// The old Client-builder NAME-substring resolution ("usb"/"monitor") is gone
// (it now lives in `ConsoleSession`). The Manager takes explicit device ids, so
// this asserts the explicit-id model: dial(output: out:monitor) + route(mic:
// in:a) opens exactly those device ids on the backend — not the defaults.
// ---------------------------------------------------------------------------

/// Shared recorder for which device ids the backend was asked to open.
#[derive(Clone, Default)]
struct OpenedIds {
    inputs: Arc<Mutex<Vec<String>>>,
    outputs: Arc<Mutex<Vec<String>>>,
}

/// Multi-device stub backend that records the `DeviceInfo.id` it opens.
struct PickyBackend {
    opened: OpenedIds,
}

impl AudioBackend for PickyBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:a"),
            dev(Direction::Input, "in:b"),
            dev(Direction::Output, "out:s"),
            dev(Direction::Output, "out:monitor"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:a"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:s"))
    }
    fn open_input(
        &self,
        d: &DeviceInfo,
        _c: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.opened
            .inputs
            .lock()
            .unwrap()
            .push(d.id.as_str().to_string());
        Ok(Box::new(StubHandle))
    }
    fn open_output(
        &self,
        d: &DeviceInfo,
        _c: StreamConfig,
        _source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.opened
            .outputs
            .lock()
            .unwrap()
            .push(d.id.as_str().to_string());
        Ok(Box::new(StubHandle))
    }
}

#[test]
fn dial_opens_the_selected_devices() {
    // Fake peer socket just provides a real address; no handshake needed —
    // device opening happens on dial()/route() before any traffic.
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let opened = OpenedIds::default();
    let mut mgr = Manager::new(Box::new(PickyBackend {
        opened: opened.clone(),
    }));

    // Dial onto the non-default output bus, then route the non-default mic.
    let dial = DialSpec {
        output: OutputId::new("out:monitor"),
        ..spec(1, peer_addr)
    };
    let id = mgr.dial(dial).unwrap();
    mgr.route(id, &MicId::new("in:a")).unwrap();

    assert_eq!(
        opened.outputs.lock().unwrap().as_slice(),
        ["out:monitor"],
        "dial(output: out:monitor) must open the monitor bus, not the default"
    );
    assert_eq!(
        opened.inputs.lock().unwrap().as_slice(),
        ["in:a"],
        "route(mic: in:a) must open the in:a mic lane"
    );
    // The snapshot reflects the routed mic + output the test asked for.
    let snap = mgr.snapshot();
    assert_eq!(snap.mic_of(id).as_deref(), Some("in:a"));
    assert_eq!(snap.output_of(id).as_deref(), Some("out:monitor"));
}

#[test]
fn dial_with_unknown_device_name_fails_before_opening_anything() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let opened = OpenedIds::default();
    let mut mgr = Manager::new(Box::new(PickyBackend {
        opened: opened.clone(),
    }));

    // An OutputId the backend doesn't expose fails at dial(): `open_monitor_call`
    // resolves the device BEFORE opening any stream or spawning the call thread,
    // so nothing is opened and no connection is pooled. (For an unknown MIC the
    // failure point would instead be `route`; we pick the output-at-dial path
    // because it cleanly preserves "fails before opening anything".)
    let dial = DialSpec {
        output: OutputId::new("out:not-plugged-in"),
        ..spec(1, peer_addr)
    };
    let err = mgr.dial(dial).expect_err("dial must fail");
    match err {
        IaxError::Audio(AudioError::DeviceNotFound(id)) => {
            assert_eq!(id, "out:not-plugged-in", "error names the missing device");
        }
        other => panic!("expected Audio(DeviceNotFound), got {other:?}"),
    }
    assert!(
        opened.inputs.lock().unwrap().is_empty() && opened.outputs.lock().unwrap().is_empty(),
        "nothing may be opened when resolution fails"
    );
    assert_eq!(
        mgr.call_count(),
        0,
        "no connection may be pooled on failure"
    );
}

// ---------------------------------------------------------------------------
// Test F (iax-3fca): web-transceiver dial sends the WT NEW shape + correct md5.
// ---------------------------------------------------------------------------

/// Captured fields from the client's NEW frame, shared back from the peer.
#[derive(Default)]
struct WtCapture {
    new: Arc<Mutex<Option<CapturedNew>>>,
    authrep_md5: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct CapturedNew {
    called_number: Option<String>,
    calling_number: Option<String>,
    calling_name: Option<String>,
    username: Option<String>,
    has_capability: bool,
    format: Option<u32>,
}

/// Fake web-transceiver peer: like `run_fake_peer`, but captures the NEW's IEs
/// and the AUTHREP's `md5_result` for assertion. Challenge is `"c0ffee"`.
fn run_wt_peer(peer: &UdpSocket, cap: &WtCapture) {
    const SERVER_CALL: u16 = 13;
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
        let client_call = ff.source_call;

        let ack = encode_ack(&ff, SERVER_CALL);
        let _ = peer.send_to(&ack, src);

        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
                *cap.new.lock().unwrap() = Some(CapturedNew {
                    called_number: ff.ies.called_number.map(String::from),
                    calling_number: ff.ies.calling_number.map(String::from),
                    calling_name: ff.ies.calling_name.map(String::from),
                    username: ff.ies.username.map(String::from),
                    has_capability: ff.ies.capability.is_some(),
                    format: ff.ies.format,
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
                *cap.authrep_md5.lock().unwrap() = ff.ies.md5_result.map(String::from);
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
            Subclass::Iax(IaxCommand::Hangup) | Subclass::Control(ControlSubclass::Hangup) => {
                break;
            }
            _ => {}
        }
    }
}

#[test]
fn web_transceiver_dial_sends_wt_shape_and_correct_md5() {
    use astar_iax_core::session::auth::md5_response;

    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let cap = WtCapture::default();
    let cap_for_thread = WtCapture {
        new: Arc::clone(&cap.new),
        authrep_md5: Arc::clone(&cap.authrep_md5),
    };
    let peer_thread = thread::spawn(move || run_wt_peer(&peer, &cap_for_thread));

    let mut mgr = Manager::new(Box::new(StubBackend::new()));
    // WT mode forces called_number="s"; the dest field is inert. caller_id is
    // the username, secret feeds the AUTHREP md5.
    let dial = DialSpec {
        caller_id: "allstar-public".to_string(),
        secret: "allstar".to_string(),
        mode: CallMode::WebTransceiver {
            node: "55553".into(),
            name: "astar".into(),
        },
        dest: String::new(),
        ..spec(1, peer_addr)
    };
    let id = mgr.dial(dial).unwrap();
    mgr.route(id, &MicId::new("in:a")).unwrap();

    // Drive to active (answered).
    assert!(wait_active(&mgr, id), "WT call should reach active");

    mgr.hangup(id, None).expect("hangup should join cleanly");
    peer_thread.join().expect("peer thread joined");

    let new = cap.new.lock().unwrap().take().expect("peer captured a NEW");
    assert_eq!(
        new.called_number.as_deref(),
        Some("s"),
        "WT forces called=\"s\""
    );
    assert_eq!(new.calling_number.as_deref(), Some("55553"));
    assert_eq!(new.calling_name.as_deref(), Some("astar"));
    assert_eq!(new.username.as_deref(), Some("allstar-public"));
    assert!(!new.has_capability, "WT NEW must omit the CAPABILITY IE");
    assert_eq!(new.format, Some(4), "FORMAT must be G.711 mu-law (4)");

    let md5 = cap
        .authrep_md5
        .lock()
        .unwrap()
        .take()
        .expect("peer captured an AUTHREP md5");
    assert_eq!(
        md5,
        md5_response("c0ffee", "allstar"),
        "AUTHREP must be md5(challenge + secret)"
    );
}

// ---------------------------------------------------------------------------
// Test E (iax-158c): mic capture must wake the runtime — voice frames reach
// the wire promptly even when the link is otherwise quiet (no inbound traffic,
// no near timers). Regression for the TX-starvation bug observed live against
// parrot 55553: ~1.9s of voice flushed in one burst AFTER RADIO_UNKEY.
// ---------------------------------------------------------------------------

/// Backend that stashes opened input sinks by device id so the test can push
/// mic samples directly (the cpal callback stand-in). Mirrors
/// `manager_loopback.rs`'s `MicSinks`/`CapturingBackend`.
#[derive(Clone, Default)]
struct MicSinks(Arc<Mutex<HashMap<String, Box<dyn InputSink>>>>);

impl MicSinks {
    /// Drive the mic lane for `device` with `samples`, as the cpal capture
    /// callback would. Returns false if the lane isn't open yet.
    fn push(&self, device: &str, samples: &[f32]) -> bool {
        let mut g = self.0.lock().unwrap();
        if let Some(mut sink) = g.remove(device) {
            drop(g);
            sink.write(samples, 0.5);
            self.0.lock().unwrap().insert(device.to_string(), sink);
            true
        } else {
            false
        }
    }
}

struct CapturingBackend {
    sinks: MicSinks,
}

impl AudioBackend for CapturingBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:a"),
            dev(Direction::Input, "in:b"),
            dev(Direction::Output, "out:s"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:a"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:s"))
    }
    fn open_input(
        &self,
        d: &DeviceInfo,
        _c: StreamConfig,
        sink: Box<dyn InputSink>,
        _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.sinks
            .0
            .lock()
            .unwrap()
            .insert(d.id.as_str().to_string(), sink);
        Ok(Box::new(StubHandle))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(StubHandle))
    }
}

/// Quiet fake peer: completes the handshake and ACKs full frames (responses
/// only — it never originates traffic), and reports the arrival `Instant` of
/// the first VOICE frame (full or mini) it sees.
fn run_quiet_peer(peer: &UdpSocket, voice_at_tx: &std::sync::mpsc::Sender<Instant>) {
    const SERVER_CALL: u16 = 11;
    let mut buf = [0u8; 4096];
    let mut sent_authreq = false;
    let mut sent_accept = false;
    let mut reported_voice = false;

    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let Ok((n, src)) = peer.recv_from(&mut buf) else {
            break;
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
                    let _ = voice_at_tx.send(Instant::now());
                }
                continue;
            }
        };
        if ff.frame_type == FrameType::Voice && !reported_voice {
            reported_voice = true;
            let _ = voice_at_tx.send(Instant::now());
        }
        let client_call = ff.source_call;
        if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
            continue;
        }
        // ACK every non-ACK full frame (pure response — never wakes a loop
        // that isn't already sending).
        let ack = encode_ack(&ff, SERVER_CALL);
        let _ = peer.send_to(&ack, src);

        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
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
fn mic_capture_wakes_runtime_and_voice_flows_on_quiet_link() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let (voice_at_tx, voice_at_rx) = std::sync::mpsc::channel::<Instant>();
    let peer_thread = thread::spawn(move || run_quiet_peer(&peer, &voice_at_tx));

    let sinks = MicSinks::default();
    let mut mgr = Manager::new(Box::new(CapturingBackend {
        sinks: sinks.clone(),
    }));

    let id = mgr.dial(spec(1, peer_addr)).unwrap();
    mgr.route(id, &MicId::new("in:a")).unwrap();

    // Drive until active (answered).
    assert!(wait_active(&mgr, id), "call should reach active");

    // Key up (this wakes the loop once for the RADIO_KEY), then let the link
    // go quiet so the only thing that can wake the runtime is mic capture.
    mgr.key(id).expect("key");
    thread::sleep(Duration::from_millis(300));

    // Push mic audio as the cpal callback would. The mic lane opened on route()
    // so its sink is captured under "in:a".
    let t0 = Instant::now();
    for _ in 0..10 {
        if !sinks.push("in:a", &[0.25_f32; 160]) {
            thread::sleep(Duration::from_millis(20));
        }
    }

    // The first voice frame must hit the wire promptly. Without the mic-side
    // waker (iax-158c) nothing wakes the loop until the next keepalive timer
    // (~10s) and this times out.
    let arrival = voice_at_rx
        .recv_timeout(Duration::from_millis(1500))
        .expect("voice frame should reach the peer promptly after mic capture");
    let latency = arrival.saturating_duration_since(t0);
    assert!(
        latency < Duration::from_millis(1500),
        "voice should flow within 1.5s of capture, took {latency:?}"
    );

    mgr.hangup(id, None).expect("hangup");
    peer_thread.join().expect("peer thread joined");
}

// ---------------------------------------------------------------------------
// Test G (iax-5029): a LINK dial threads `LinkSpec::mode_shape` verbatim into
// the NEW frame the console WT dial path emits, in both call shapes.
// ---------------------------------------------------------------------------

#[test]
fn link_dial_wt_shape_sends_wt_new_and_correct_md5() {
    // iax-5029: a WT-shaped LINK dial must emit the same NEW the console WT
    // path emits (the shape that passes `AllStar` guest auth).
    use astar_iax_core::session::auth::md5_response;

    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let cap = WtCapture::default();
    let cap_for_thread = WtCapture {
        new: Arc::clone(&cap.new),
        authrep_md5: Arc::clone(&cap.authrep_md5),
    };
    let peer_thread = thread::spawn(move || run_wt_peer(&peer, &cap_for_thread));

    let mut mgr = Manager::new(Box::new(StubBackend::new()));
    let id = mgr
        .connect_link(
            LinkSpec {
                node: "55553".to_string(),
                mode: LinkMode::Transceive,
                output: OutputId::new("out:s"),
                caller_id: "allstar-public".to_string(),
                secret: "allstar".to_string(),
                dest: "55553".to_string(), // inert: WT forces called="s"
                mode_shape: CallMode::WebTransceiver {
                    node: "55553".into(),
                    name: "77777".into(),
                },
                permanent: false,
            },
            &|_| Ok(peer_addr),
        )
        .unwrap();
    assert!(wait_active(&mgr, id), "WT link should reach active");
    mgr.disconnect_link(id).unwrap();
    peer_thread.join().unwrap();

    let new = cap.new.lock().unwrap().take().expect("peer captured a NEW");
    assert_eq!(
        new.called_number.as_deref(),
        Some("s"),
        "WT forces called=\"s\""
    );
    assert_eq!(new.calling_number.as_deref(), Some("55553"));
    assert_eq!(new.calling_name.as_deref(), Some("77777"));
    assert_eq!(new.username.as_deref(), Some("allstar-public"));
    assert!(!new.has_capability, "WT NEW must omit the CAPABILITY IE");
    assert_eq!(new.format, Some(4), "FORMAT must be G.711 mu-law (4)");

    let md5 = cap
        .authrep_md5
        .lock()
        .unwrap()
        .take()
        .expect("captured md5");
    assert_eq!(md5, md5_response("c0ffee", "allstar"));
}

#[test]
fn link_dial_standard_shape_pins_todays_new() {
    // iax-5029 regression: the DEFAULT link dial must stay byte-identical —
    // CALLED_NUMBER = node, USERNAME = caller_id, CAPABILITY present.
    use astar_iax_core::session::auth::md5_response;

    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let cap = WtCapture::default();
    let cap_for_thread = WtCapture {
        new: Arc::clone(&cap.new),
        authrep_md5: Arc::clone(&cap.authrep_md5),
    };
    let peer_thread = thread::spawn(move || run_wt_peer(&peer, &cap_for_thread));

    let mut mgr = Manager::new(Box::new(StubBackend::new()));
    let id = mgr
        .connect_link(
            LinkSpec {
                node: "1999".to_string(),
                mode: LinkMode::Monitor,
                output: OutputId::new("out:s"),
                caller_id: "77777".to_string(),
                secret: "linkpw".to_string(),
                dest: "1999".to_string(),
                mode_shape: CallMode::Standard,
                permanent: false,
            },
            &|_| Ok(peer_addr),
        )
        .unwrap();
    assert!(wait_active(&mgr, id), "standard link should reach active");
    mgr.disconnect_link(id).unwrap();
    peer_thread.join().unwrap();

    let new = cap.new.lock().unwrap().take().expect("peer captured a NEW");
    assert_eq!(new.called_number.as_deref(), Some("1999"));
    assert_eq!(new.username.as_deref(), Some("77777"));
    assert!(new.has_capability, "standard NEW carries CAPABILITY");

    let md5 = cap
        .authrep_md5
        .lock()
        .unwrap()
        .take()
        .expect("captured md5");
    assert_eq!(md5, md5_response("c0ffee", "linkpw"));
}
