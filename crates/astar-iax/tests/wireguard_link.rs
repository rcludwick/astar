// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end `WireGuard` link-transport tests (iax-927a): two `Manager`s in
//! one process — one dialing, one accepting inbound through its own shared
//! `WgStack` — connected via paired in-memory underlay transports (no real
//! network, modeled on `astar-wireguard`'s two-stack tests). Plus the
//! `also_bind_udp` escape hatch: while WG mode is active, a plain-UDP peer
//! completes a call against the extra OS listener socket.
//!
//! House style: wait-until polling (no fixed sleeps on the assertion path);
//! backends mirror `node_audio_path.rs`'s tone-in / capture-out shape.

use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
    OutputSource, StreamConfig, StreamHandle,
};
use astar_iax::manager::{DialSpec, Manager};
use astar_iax::{
    CallEvent, CallId, CallMode, CodecPolicy, IncomingAuthPolicy, IncomingCallEvent,
    IncomingCallListener, IncomingCallPolicy, IncomingDecisionPolicy, UdpTransport, WgLinkConfig,
    dial_raw,
};
use astar_wireguard::x25519::{PublicKey, StaticSecret};

use base64::Engine as _;

// ---------------------------------------------------------------------------
// Paired in-memory underlay (the "wire" between the two WgStacks).
// ---------------------------------------------------------------------------

type Queue = Arc<Mutex<VecDeque<Vec<u8>>>>;

/// One end of a crossed in-memory underlay: `send_to` lands in the peer's
/// queue, `recv_from` pops our own. The stacks' I/O threads poll, so datagrams
/// flow with no test-side pumping.
struct PairedTransport {
    rx: Queue,
    tx: Queue,
    peer: SocketAddr,
}

impl UdpTransport for PairedTransport {
    fn send_to(&mut self, data: &[u8], _dst: SocketAddr) -> io::Result<usize> {
        self.tx.lock().unwrap().push_back(data.to_vec());
        Ok(data.len())
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self.rx.lock().unwrap().pop_front() {
            Some(d) => {
                let n = d.len().min(buf.len());
                buf[..n].copy_from_slice(&d[..n]);
                Ok((n, self.peer))
            }
            None => Err(io::Error::new(io::ErrorKind::WouldBlock, "no data")),
        }
    }
}

/// Two crossed underlay ends: what A sends, B receives, and vice versa.
fn paired_underlays() -> (Box<dyn UdpTransport>, Box<dyn UdpTransport>) {
    let a_to_b: Queue = Arc::default();
    let b_to_a: Queue = Arc::default();
    let peer: SocketAddr = "192.0.2.9:51820".parse().unwrap();
    let a = PairedTransport {
        rx: Arc::clone(&b_to_a),
        tx: Arc::clone(&a_to_b),
        peer,
    };
    let b = PairedTransport {
        rx: a_to_b,
        tx: b_to_a,
        peer,
    };
    (Box::new(a), Box::new(b))
}

// ---------------------------------------------------------------------------
// WG configs: A (10.88.0.1, key seed 1) <-> B (10.88.0.2, key seed 2).
// ---------------------------------------------------------------------------

const A_TUNNEL: Ipv4Addr = Ipv4Addr::new(10, 88, 0, 1);
const B_TUNNEL: Ipv4Addr = Ipv4Addr::new(10, 88, 0, 2);

fn b64(k: [u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(k)
}

fn wg_config(ip: Ipv4Addr, peer_seed: u8) -> WgLinkConfig {
    let peer_pub = PublicKey::from(&StaticSecret::from([peer_seed; 32]));
    WgLinkConfig::new(
        "WG_KEY",
        &format!("{ip}/32"),
        &b64(peer_pub.to_bytes()),
        "192.0.2.9:51820",
        &["10.88.0.0/24".to_string()],
        25,
    )
    .expect("valid config")
}

/// A resolver for key seed `seed` — consulted once at stack-build time.
fn resolver(seed: u8) -> impl Fn(&str) -> String {
    move |_: &str| b64(StaticSecret::from([seed; 32]).to_bytes())
}

// ---------------------------------------------------------------------------
// Tone-in / capture-out backend (mirrors node_audio_path.rs).
// ---------------------------------------------------------------------------

struct ThreadHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl StreamHandle for ThreadHandle {
    fn stop(mut self: Box<Self>) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
    fn pause(&self) -> Result<(), AudioError> {
        Ok(())
    }
    fn resume(&self) -> Result<(), AudioError> {
        Ok(())
    }
}

fn dev(direction: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

/// Shared observation cells for one manager's audio: output peak + count of
/// audible output reads.
#[derive(Clone, Default)]
struct AudioProbe {
    peak_milli: Arc<AtomicU64>,
    loud_reads: Arc<AtomicUsize>,
}

impl AudioProbe {
    fn peak(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            self.peak_milli.load(Ordering::Relaxed) as f32 / 1000.0
        }
    }
    fn loud_reads(&self) -> usize {
        self.loud_reads.load(Ordering::Relaxed)
    }
}

/// INPUT emits a loud 440 Hz tone (the operator "talking"); OUTPUT records the
/// peak + audible-read count into the probe (what the operator would hear).
struct ToneCaptureBackend {
    probe: AudioProbe,
}

impl AudioBackend for ToneCaptureBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:cap"),
            dev(Direction::Output, "out:cap"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:cap"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:cap"))
    }
    fn open_input(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        mut sink: Box<dyn InputSink>,
        _overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            let mut t: f32 = 0.0;
            while !stop_t.load(Ordering::Relaxed) {
                // 20 ms = 160 samples of a 440 Hz sine at 0.5 amplitude.
                let mut buf = [0f32; 160];
                for s in &mut buf {
                    *s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                    t += 1.0 / 8000.0;
                    if t >= 1.0 {
                        t -= 1.0;
                    }
                }
                sink.write(&buf, 0.5);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(Box::new(ThreadHandle {
            stop,
            join: Some(join),
        }))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        mut source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        let probe = self.probe.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let join = std::thread::spawn(move || {
            let mut buf = vec![0f32; 160];
            while !stop_t.load(Ordering::Relaxed) {
                let n = source.read(&mut buf);
                let p = buf[..n].iter().fold(0f32, |m, &s| m.max(s.abs()));
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let p_milli = (p * 1000.0) as u64;
                probe.peak_milli.fetch_max(p_milli, Ordering::Relaxed);
                if p > 0.1 {
                    probe.loud_reads.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Ok(Box::new(ThreadHandle {
            stop,
            join: Some(join),
        }))
    }
}

fn tone_capture() -> (Box<dyn AudioBackend>, AudioProbe) {
    let probe = AudioProbe::default();
    (
        Box::new(ToneCaptureBackend {
            probe: probe.clone(),
        }),
        probe,
    )
}

/// Wait-until polling helper (house style — no fixed sleeps).
fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn auto_accept_policy() -> IncomingCallPolicy {
    IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn wg_call_between_two_managers_answers_and_audio_flows_both_ways() {
    let (underlay_a, underlay_b) = paired_underlays();

    // Acceptor: Manager B in WG mode; its inbound listener rides the tunnel
    // at inner port 4569.
    let (backend_b, probe_b) = tone_capture();
    let mut mgr_b = Manager::new(backend_b);
    mgr_b
        .set_wireguard_transport_over(&wg_config(B_TUNNEL, 1), &resolver(2), underlay_b)
        .expect("B enters WG mode");
    let (listener, levents) = IncomingCallListener::builder()
        .bind("0.0.0.0:4569".parse().unwrap())
        .net(mgr_b.net_stack())
        .policy(auto_accept_policy())
        .start()
        .expect("listener starts on the tunnel");
    assert_eq!(
        listener.local_addr(),
        SocketAddr::V4(SocketAddrV4::new(B_TUNNEL, 4569)),
        "listener addr is tunnel-inner"
    );

    // Dialer: Manager A in WG mode, dialing B's tunnel-inner address.
    let (backend_a, probe_a) = tone_capture();
    let mut mgr_a = Manager::new(backend_a);
    mgr_a
        .set_wireguard_transport_over(&wg_config(A_TUNNEL, 2), &resolver(1), underlay_a)
        .expect("A enters WG mode");
    let id_a = mgr_a
        .dial(DialSpec {
            id: CallId::from_raw(1),
            node: "wg-b".to_string(),
            peer: SocketAddr::V4(SocketAddrV4::new(B_TUNNEL, 4569)),
            output: OutputId::new("out:cap"),
            caller_id: "wg-test".to_string(),
            secret: String::new(),
            mode: CallMode::Standard,
            dest: "s".to_string(),
            frame_observer: None,
            codec_policy: CodecPolicy::default(),
        })
        .expect("dial through the tunnel");
    mgr_a
        .route(id_a, &MicId::new("in:cap"))
        .expect("route A mic");
    let a_events = mgr_a.take_events(id_a).expect("A event stream");

    // The call answers on the dial side AND B adopts+routes the inbound leg.
    let mgr_b = Mutex::new(mgr_b);
    let mut answered = false;
    let mut adopted: Option<CallId> = None;
    wait_for("answer + adopt", || {
        if adopted.is_none()
            && let Ok(IncomingCallEvent::Answered { call, .. }) = levents.try_recv()
        {
            let mut b = mgr_b.lock().unwrap();
            let id = b.adopt(call, &OutputId::new("out:cap")).expect("B adopts");
            b.route(id, &MicId::new("in:cap")).expect("route B mic");
            adopted = Some(id);
        }
        while let Ok(ev) = a_events.try_recv() {
            if matches!(ev, CallEvent::Answered { .. }) {
                answered = true;
            }
        }
        (answered && adopted.is_some()).then_some(())
    });
    let id_b = adopted.expect("adopted");

    // Media A -> B: key A's tone mic; B's speaker capture must go loud, and
    // stay a continuous stream (many audible reads, not one frame).
    mgr_a.key(id_a).expect("key A");
    wait_for("A's voice at B's output", || {
        (probe_b.peak() > 0.1 && probe_b.loud_reads() > 5).then_some(())
    });
    mgr_a.unkey(id_a).expect("unkey A");

    // Media B -> A: key B's tone mic; A's speaker capture must go loud.
    mgr_b.lock().unwrap().key(id_b).expect("key B");
    wait_for("B's voice at A's output", || {
        (probe_a.peak() > 0.1 && probe_a.loud_reads() > 5).then_some(())
    });
    mgr_b.lock().unwrap().unkey(id_b).expect("unkey B");

    // Both tunnels completed a handshake and carried traffic.
    let status_a = mgr_a.wg_status().expect("A status");
    assert!(status_a.last_handshake_age.is_some(), "A handshaked");
    assert!(status_a.tx_packets > 0 && status_a.rx_packets > 0);
    let status_b = mgr_b.lock().unwrap().wg_status().expect("B status");
    assert!(status_b.last_handshake_age.is_some(), "B handshaked");

    // Clean teardown, spec order: calls -> listener -> stacks (the Managers
    // drop their calls before the WgStack field by declaration order; the
    // test completing without hanging is the join assertion).
    mgr_a.hangup(id_a, None).expect("A hangs up");
    let mut mgr_b = mgr_b.into_inner().unwrap();
    mgr_b.hangup(id_b, None).expect("B drops the leg");
    drop(listener);
    drop(mgr_b);
    drop(mgr_a);
}

#[test]
fn also_bind_udp_admits_a_plain_udp_peer_while_wg_is_active() {
    // Manager in WG mode over a dead underlay (no WG peer involved here —
    // the point is the plain-UDP side door).
    let (backend, _probe) = tone_capture();
    let mut mgr = Manager::new(backend);
    let cfg = wg_config(A_TUNNEL, 2).with_also_bind_udp(Some("127.0.0.1:0".parse().unwrap()));
    mgr.set_wireguard_transport_over(
        &cfg,
        &resolver(1),
        Box::new(astar_wireguard::FakeTransport::new()),
    )
    .expect("WG mode active");

    // The listener: primary socket on the tunnel (inner 4569), extra plain OS
    // socket from the config's also_bind_udp — both feed one event channel.
    let (listener, levents) = IncomingCallListener::builder()
        .bind("0.0.0.0:4569".parse().unwrap())
        .net(mgr.net_stack())
        .also_bind_udp(mgr.also_bind_udp())
        .policy(auto_accept_policy())
        .start()
        .expect("listener starts");
    let extra = listener
        .extra_local_addr()
        .expect("extra plain-UDP socket is bound");
    assert!(extra.port() != 0, "ephemeral extra port resolved");

    // A plain OS-UDP peer (device-free raw dial) completes a call against the
    // extra socket while WG mode is active.
    let raw = dial_raw(extra, "plain-peer", "s", "", CallMode::Standard).expect("plain dial");

    // The auto-answered leg lands on the SAME adopt path as tunnel calls.
    let mgr = Mutex::new(mgr);
    let mut adopted: Option<CallId> = None;
    let mut answered = false;
    wait_for("plain peer answered + adopted", || {
        if adopted.is_none()
            && let Ok(IncomingCallEvent::Answered { call, .. }) = levents.try_recv()
        {
            let id = mgr
                .lock()
                .unwrap()
                .adopt(call, &OutputId::new("out:cap"))
                .expect("adopt plain leg");
            adopted = Some(id);
        }
        while let Ok(ev) = raw.events.try_recv() {
            if matches!(ev, CallEvent::Answered { .. }) {
                answered = true;
            }
        }
        (answered && adopted.is_some()).then_some(())
    });

    // Clean teardown: call -> listener -> stack.
    let _ = raw.call.hangup(None);
    let id = adopted.expect("adopted");
    let _ = mgr.lock().unwrap().hangup(id, None);
    drop(listener);
    drop(mgr);
}
