// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration test for the iax-42e9 [`Manager`]: re-routing a mic redirects
//! voice to the new node on the wire and silences the old one.
//!
//! Two fake "quiet" UDP peers complete the IAX2 handshake and report the
//! arrival of the first VOICE frame they see (mirroring `client_loopback.rs`'s
//! `run_quiet_peer`). A `CapturingBackend` stashes each opened mic lane sink by
//! device id so the test can push mic samples as the cpal callback would. The
//! Manager dials both peers monitor-only, routes a mic to call X (voice reaches
//! peer A), then re-routes the SAME mic to call Y (voice reaches peer B and
//! peer A goes quiet).

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_iax::manager::DialSpec;
use astar_iax::{CallId, CallMode, CodecPolicy, Manager};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
    OutputSource, StreamConfig, StreamHandle,
};

use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};

// ---------------------------------------------------------------------------
// Capturing backend: stashes opened input sinks (mic lanes) by device id.
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
        _s: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(StubHandle))
    }
}

// ---------------------------------------------------------------------------
// Fake-peer helpers (mirrors client_loopback's run_quiet_peer).
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

/// Quiet fake peer: completes the handshake, ACKs full frames (never
/// originates traffic), and pings `voice_at_tx` with the `Instant` of EACH
/// VOICE frame (full or mini) it sees — so the test can tell "no voice after T".
fn run_quiet_peer(
    peer: &UdpSocket,
    server_call: u16,
    voice_at_tx: &std::sync::mpsc::Sender<Instant>,
) {
    let mut buf = [0u8; 4096];
    let mut sent_authreq = false;
    let mut sent_accept = false;

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        // Read timeout (Err): stay alive until the overall deadline (the other
        // call may be transmitting; this peer is just idle right now).
        let Ok((n, src)) = peer.recv_from(&mut buf) else {
            continue;
        };
        let Ok(frame) = parse_lenient(&buf[..n]) else {
            continue;
        };
        let ff = match frame {
            Frame::Full(ff) => ff,
            Frame::Mini(_) => {
                let _ = voice_at_tx.send(Instant::now());
                continue;
            }
        };
        if ff.frame_type == FrameType::Voice {
            let _ = voice_at_tx.send(Instant::now());
        }
        let client_call = ff.source_call;
        if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
            continue;
        }
        let ack = encode_ack(&ff, server_call);
        let _ = peer.send_to(&ack, src);

        match ff.subclass {
            Subclass::Iax(IaxCommand::New) if !sent_authreq => {
                sent_authreq = true;
                let authreq = encode_iax(
                    server_call,
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
                    server_call,
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

fn spec(id: u64, node: &str, peer: std::net::SocketAddr, out: &str) -> DialSpec {
    DialSpec {
        id: CallId::from_raw(id),
        node: node.to_string(),
        peer,
        output: OutputId::new(out),
        caller_id: "mgr-loopback".to_string(),
        secret: String::new(),
        mode: CallMode::Standard,
        dest: "1000".to_string(),
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

#[test]
fn re_routing_a_mic_redirects_voice_to_the_new_node() {
    // Two quiet peers, each reporting voice arrivals.
    let peer_a = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer_a
        .set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let addr_a = peer_a.local_addr().unwrap();
    let peer_b = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer_b
        .set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let addr_b = peer_b.local_addr().unwrap();

    let (va_tx, va_rx) = std::sync::mpsc::channel::<Instant>();
    let (vb_tx, vb_rx) = std::sync::mpsc::channel::<Instant>();
    let ta = thread::spawn(move || run_quiet_peer(&peer_a, 21, &va_tx));
    let tb = thread::spawn(move || run_quiet_peer(&peer_b, 22, &vb_tx));

    let sinks = MicSinks::default();
    let backend = Box::new(CapturingBackend {
        sinks: sinks.clone(),
    });
    let mut mgr = Manager::new(backend);

    let x = mgr.dial(spec(1, "in:a", addr_a, "out:s")).unwrap();
    let y = mgr.dial(spec(2, "in:b", addr_b, "out:s")).unwrap();
    assert!(wait_active(&mgr, x), "call X must reach active");
    assert!(wait_active(&mgr, y), "call Y must reach active");

    let mic = MicId::new("in:a");

    // Snapshot helper: a call's `keyed` flag (false if the call is gone).
    let keyed = |m: &Manager, id: CallId| {
        m.snapshot()
            .calls
            .iter()
            .find(|c| c.id == id)
            .is_some_and(|c| c.keyed)
    };

    // Route the mic to X and key it; pushing mic audio must reach peer A.
    mgr.route(x, &mic).unwrap();
    mgr.key(x).unwrap();
    assert!(keyed(&mgr, x), "X is keyed while routed + keyed");
    // The mic lane opens on first route → its sink is now captured. Push a few
    // full 160-sample frames so a complete 160-byte voice frame is emitted.
    for _ in 0..20 {
        if !sinks.push("in:a", &[0.4_f32; 160]) {
            thread::sleep(Duration::from_millis(20));
        }
    }
    let a_voice = va_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("peer A must receive voice while the mic is routed to X");
    let _ = a_voice;

    // Re-route the SAME mic to Y, key Y; X drops to monitor-only.
    mgr.route(y, &mic).unwrap();
    mgr.key(y).unwrap();
    assert_eq!(mgr.snapshot().mic_of(x), None, "X is now monitor-only");
    assert_eq!(mgr.snapshot().mic_of(y).as_deref(), Some("in:a"));
    // PTT auto-clear: X lost its mic, so it must no longer report keyed; Y,
    // freshly routed + keyed, is the only keyed call.
    assert!(
        !keyed(&mgr, x),
        "X must auto-unkey when its mic is re-routed away"
    );
    assert!(keyed(&mgr, y), "Y is keyed after the re-route");

    // Let X's pipeline quiesce (in-flight voice + retransmits from the first
    // burst flush to peer A), then drain A's queue and mark the swap point so
    // any voice A sees afterward is genuinely post-swap (and must be none).
    thread::sleep(Duration::from_millis(800));
    while va_rx.try_recv().is_ok() {}
    let swap = Instant::now();
    for _ in 0..20 {
        sinks.push("in:a", &[0.4_f32; 160]);
        thread::sleep(Duration::from_millis(5));
    }

    // Peer B must now receive voice.
    let b_voice = vb_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("peer B must receive voice after the re-route");
    assert!(b_voice >= swap, "B's voice arrives after the swap");

    // Peer A must NOT receive any voice that originated after the swap.
    let mut a_after = false;
    while let Ok(t) = va_rx.try_recv() {
        if t >= swap {
            a_after = true;
        }
    }
    assert!(
        !a_after,
        "peer A must go quiet after the mic is re-routed to Y"
    );

    // Teardown: hang up both, join peers.
    mgr.hangup(x, None).unwrap();
    mgr.hangup(y, None).unwrap();
    let _ = ta.join();
    let _ = tb.join();
}
