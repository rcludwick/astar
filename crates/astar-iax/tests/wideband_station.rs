// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Wideband station mixed-rate integration test (iax-4348 Task 8): the pitch
//! canary.
//!
//! **Variant built: full wire-level integration** (the brief's fallback — split
//! to an `EdgeAudio` unit test — was NOT needed). The loopback harness CAN
//! inject RX voice on a fake peer:
//!   - `Reliability::on_frame_in` delivers every `Frame::Mini` unconditionally
//!     (no sequence-number bookkeeping at all — see
//!     `astar-iax-core/src/session/reliability.rs`), so a fake peer can stream
//!     voice without implementing the full ACK/VNAK dance.
//!   - The client's outbound `NewSent -> Accept` transition (no-auth path)
//!     moves straight to `SessionState::Active` and fires `AppEvent::Connected`
//!     — matching `wt_loopback.rs`'s existing peers, which complete a call with
//!     no ANSWER control frame at all.
//!   - `Manager::dial` always wires a call's RX onto its output bus
//!     (`router.open_monitor_call`), independent of `route()`/keying, so a
//!     monitor-only call still mixes received audio onto a bus a test backend
//!     can pull from (mirrors `tests/node_audio_path.rs`'s capturing backend).
//!
//! One `Manager::with_policy(.., CodecPolicy::PreferSlin16)` (16 kHz station)
//! drives BOTH calls, each against its own hand-built fake UDP peer:
//!   1. `slin16_negotiated_and_wire_frames_are_640_bytes`: peer ACCEPTs slin16
//!      -> `negotiated_format == Some(Slin16)` and TX wire frames are 640 bytes
//!      (320 bus samples @ 16 kHz, no resample needed).
//!   2. `ulaw_wire_frames_and_rx_pitch_preserved_at_bus_rate`: peer only
//!      ACCEPTs G711U -> TX wire bytes total ~160/frame on average
//!      (downsampled 16k->8k at the edge; individual frames vary because the
//!      resampler batches on its own 256-sample internal chunk, not our
//!      320-sample push — see the test's own comment and
//!      `codec_edge.rs`'s matching unit test), AND a 1 kHz tone the peer
//!      streams as RX (µ-law @ 8 kHz)
//!      arrives on the 16 kHz output bus still measuring as a 1 kHz tone
//!      (Goertzel `dominant_ratio`, shaped after `examples/slin_probe.rs`'s
//!      `goertzel_mag_sq`/`tone_concentration`) — the pitch canary: a missing
//!      or wrong-direction resample at the RX edge would double the apparent
//!      frequency (2 kHz) or otherwise smear the tone's energy.
//!   3. `default_manager_stays_narrowband`: regression pin — `Manager::new`
//!      (default `CodecPolicy`) still runs an 8 kHz station pipeline.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_iax::manager::DialSpec;
use astar_iax::{CallId, CallMode, CodecPolicy, Manager};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
    OutputSource, StreamConfig, StreamHandle,
};

use astar_codec::g711::ulaw_encode;
use astar_iax_core::VoiceFormat;
use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};

// ---------------------------------------------------------------------------
// Test backend: mic push (mirrors `wt_loopback.rs`'s `MicSinks`) + an
// output-bus capture handle (mirrors `node_audio_path.rs`'s capturing
// backend), so ONE backend can serve both the TX (mic -> wire) and RX
// (wire -> bus) halves of both calls.
// ---------------------------------------------------------------------------

fn dev(dir: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction: dir,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

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

/// Retains opened mic sinks by device id so the test can push PCM directly, as
/// the cpal capture callback would (mirrors `wt_loopback.rs`'s `MicSinks`).
#[derive(Clone, Default)]
struct MicSinks(Arc<Mutex<HashMap<String, Box<dyn InputSink>>>>);

impl MicSinks {
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

/// A background thread that pulls PCM from an `OutputSource` (as a real device
/// callback would) into a shared buffer, so a test can inspect what actually
/// landed on the bus (mirrors `node_audio_path.rs`'s `CapturingBackend`).
struct CaptureHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl StreamHandle for CaptureHandle {
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

struct TestBackend {
    mic: MicSinks,
    /// Output device id whose RX PCM gets captured (the other output is a
    /// pure `StubHandle` — this test only inspects one call's bus at a time).
    capture_output: String,
    rx_capture: Arc<Mutex<Vec<f32>>>,
}

impl AudioBackend for TestBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:a"),
            dev(Direction::Input, "in:b"),
            dev(Direction::Output, "out:s"),
            dev(Direction::Output, "out:rx"),
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
        _overruns: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.mic
            .0
            .lock()
            .unwrap()
            .insert(d.id.as_str().to_string(), sink);
        Ok(Box::new(StubHandle))
    }
    fn open_output(
        &self,
        d: &DeviceInfo,
        _c: StreamConfig,
        mut source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        if d.id.as_str() != self.capture_output {
            return Ok(Box::new(StubHandle));
        }
        let capture = Arc::clone(&self.rx_capture);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let join = thread::spawn(move || {
            let mut buf = vec![0f32; 320]; // 20 ms @ 16 kHz
            while !stop_t.load(Ordering::Relaxed) {
                let n = source.read(&mut buf);
                if n > 0 {
                    capture.lock().unwrap().extend_from_slice(&buf[..n]);
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        Ok(Box::new(CaptureHandle {
            stop,
            join: Some(join),
        }))
    }
}

// ---------------------------------------------------------------------------
// DialSpec + snapshot-polling helpers (mirrors `wt_loopback.rs`).
// ---------------------------------------------------------------------------

fn spec(id: u64, output: &str, peer: SocketAddr, policy: CodecPolicy) -> DialSpec {
    DialSpec {
        id: CallId::from_raw(id),
        node: "55553".to_string(),
        peer,
        output: OutputId::new(output),
        caller_id: "wideband-station".to_string(),
        secret: String::new(),
        mode: CallMode::Standard,
        dest: "55553".to_string(),
        frame_observer: None,
        codec_policy: policy,
    }
}

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

/// `negotiated_format` is published a beat after `state` flips to Active (both
/// updates land in the same runtime-loop pass, but on different lines), so
/// poll it separately rather than trusting the first snapshot after
/// `wait_active`.
fn wait_negotiated(mgr: &Manager, id: CallId, want: VoiceFormat) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if mgr
            .snapshot()
            .calls
            .iter()
            .any(|c| c.id == id && c.negotiated_format == Some(want))
        {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

// ---------------------------------------------------------------------------
// Fake-peer wire helpers (adapted from `wt_loopback.rs` / `examples/slin_probe.rs`).
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
    let mut out = Vec::with_capacity(64);
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

fn encode_voice_full(
    server_call: u16,
    client_call: u16,
    oseqno: u8,
    iseqno: u8,
    ts: u32,
    format: VoiceFormat,
    payload: &[u8],
) -> Vec<u8> {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: server_call,
        dest_call: client_call,
        retransmission: false,
        timestamp: ts,
        oseqno,
        iseqno,
        frame_type: FrameType::Voice,
        subclass: Subclass::Voice(format),
        ies: Ies::empty(),
        payload,
    }));
    let mut out = Vec::with_capacity(payload.len() + 16);
    encode(&frame, &mut out).expect("voice full frame encodes");
    out
}

/// Raw mini-frame voice, matching `examples/slin_probe.rs`'s manual encoding:
/// no oseq/iseq at all — `Reliability::on_frame_in` delivers every mini frame
/// unconditionally, so the peer doesn't need to track sequence state for these.
#[allow(clippy::cast_possible_truncation)]
fn encode_voice_mini(server_call: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(server_call & 0x7FFF).to_be_bytes());
    out.extend_from_slice(&((ts & 0xFFFF) as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn is_hangup(ff: &FullFrame) -> bool {
    matches!(
        (ff.frame_type, &ff.subclass),
        (FrameType::Iax, Subclass::Iax(IaxCommand::Hangup))
            | (
                FrameType::Control,
                Subclass::Control(ControlSubclass::Hangup)
            )
    )
}

/// Fake peer: no-auth ACCEPT with `format` in the FORMAT IE (the "peer
/// accepted the initial NEW directly" path in
/// `handlers_outbound.rs::on_new_sent`, which moves straight to
/// `SessionState::Active` — no AUTHREQ/AUTHREP round trip needed). Reports the
/// payload length of every Voice frame (full or mini) it receives from the
/// client on `sizes_tx` — the wire-frame-size assertion.
fn run_accept_and_capture_peer(
    peer: &UdpSocket,
    format: VoiceFormat,
    sizes_tx: &std::sync::mpsc::Sender<usize>,
) {
    const SERVER_CALL: u16 = 31;
    let mut buf = [0u8; 4096];
    let mut accepted = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // A per-call read timeout (`set_read_timeout`) is expected to fire
        // repeatedly while the test is otherwise quiet (e.g. between ACCEPT
        // and the caller keying up); only give up on the OUTER deadline or a
        // real socket error, never on a single timed-out read.
        let (n, src) = match peer.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        let Ok(frame) = parse_lenient(&buf[..n]) else {
            continue;
        };
        match frame {
            Frame::Mini(m) => {
                let _ = sizes_tx.send(m.payload.len());
            }
            Frame::Full(ff) => {
                let client_call = ff.source_call;
                if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
                    continue;
                }
                let ack = encode_ack(&ff, SERVER_CALL);
                let _ = peer.send_to(&ack, src);
                if ff.frame_type == FrameType::Voice {
                    let _ = sizes_tx.send(ff.payload.len());
                }
                match ff.subclass {
                    Subclass::Iax(IaxCommand::New) if !accepted => {
                        accepted = true;
                        let accept = encode_iax(
                            SERVER_CALL,
                            client_call,
                            0,
                            ff.oseqno.wrapping_add(1),
                            IaxCommand::Accept,
                            Ies {
                                format: Some(format.as_u32()),
                                ..Ies::empty()
                            },
                        );
                        let _ = peer.send_to(&accept, src);
                    }
                    _ if is_hangup(&ff) => break,
                    _ => {}
                }
            }
        }
    }
}

/// Like [`run_accept_and_capture_peer`], but also streams `tone_frames` (raw
/// wire payloads, already encoded for `format`) back to the client as RX voice
/// right after ACCEPT: one FULL Voice frame (establishes codec context) then
/// MINI frames for the rest.
#[allow(clippy::cast_possible_truncation)]
fn run_accept_stream_and_capture_peer(
    peer: &UdpSocket,
    format: VoiceFormat,
    tone_frames: &[Vec<u8>],
    sizes_tx: &std::sync::mpsc::Sender<usize>,
) {
    const SERVER_CALL: u16 = 33;
    let mut buf = [0u8; 4096];
    let mut accepted = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // See `run_accept_and_capture_peer`: a read timeout is expected while
        // the test is quiet between ACCEPT (+ the RX tone burst) and the
        // caller keying up several seconds later; only give up on the OUTER
        // deadline or a real socket error.
        let (n, src) = match peer.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        let Ok(frame) = parse_lenient(&buf[..n]) else {
            continue;
        };
        match frame {
            Frame::Mini(m) => {
                let _ = sizes_tx.send(m.payload.len());
            }
            Frame::Full(ff) => {
                let client_call = ff.source_call;
                if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
                    continue;
                }
                let ack = encode_ack(&ff, SERVER_CALL);
                let _ = peer.send_to(&ack, src);
                if ff.frame_type == FrameType::Voice {
                    let _ = sizes_tx.send(ff.payload.len());
                }
                match ff.subclass {
                    Subclass::Iax(IaxCommand::New) if !accepted => {
                        accepted = true;
                        let accept_iseq = ff.oseqno.wrapping_add(1);
                        let accept = encode_iax(
                            SERVER_CALL,
                            client_call,
                            0,
                            accept_iseq,
                            IaxCommand::Accept,
                            Ies {
                                format: Some(format.as_u32()),
                                ..Ies::empty()
                            },
                        );
                        let _ = peer.send_to(&accept, src);

                        // RX tone burst: first frame FULL (establishes the
                        // format for subsequent minis per the IAX2 rule), rest
                        // MINI (no seq bookkeeping needed).
                        if let Some((first, rest)) = tone_frames.split_first() {
                            let full = encode_voice_full(
                                SERVER_CALL,
                                client_call,
                                1,
                                accept_iseq,
                                0,
                                format,
                                first,
                            );
                            let _ = peer.send_to(&full, src);
                            for (i, payload) in rest.iter().enumerate() {
                                let ts = (i as u32 + 1) * 20;
                                let mini = encode_voice_mini(SERVER_CALL, ts, payload);
                                let _ = peer.send_to(&mini, src);
                            }
                        }
                    }
                    _ if is_hangup(&ff) => break,
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tone generation + Goertzel pitch check (shaped after
// `examples/slin_probe.rs`'s `goertzel_mag_sq`/`tone_concentration`, but
// parameterized on sample rate — the probe hardcodes 8 kHz).
// ---------------------------------------------------------------------------

/// `seconds` of a `hz` sine at `amp`, µ-law-encoded as 20 ms / 160-sample @
/// 8 kHz wire frames (one `Vec<u8>` per frame).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn ulaw_tone_frames(seconds: f64, hz: f64, amp: f64) -> Vec<Vec<u8>> {
    let n_frames = (seconds * 50.0).round() as usize; // 50 x 20 ms frames/sec
    (0..n_frames)
        .map(|f| {
            (0..160)
                .map(|i| {
                    let t = (f * 160 + i) as f64 / 8000.0;
                    let s = (amp * (std::f64::consts::TAU * hz * t).sin()).round() as i16;
                    ulaw_encode(s)
                })
                .collect()
        })
        .collect()
}

fn goertzel_mag_sq(samples: &[f64], target_hz: f64, sample_rate: f64) -> f64 {
    let w = std::f64::consts::TAU * target_hz / sample_rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let real = s1 - s2 * w.cos();
    let imag = s2 * w.sin();
    real * real + imag * imag
}

/// Fraction of `pcm`'s total energy concentrated at `hz` (~1.0 for a pure
/// tone). A missing or wrong-direction resample at the RX edge would leave the
/// tone's energy at 2x (or some other wrong) frequency, reading near 0 here.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn dominant_ratio(pcm: &[i16], hz: f32, fs: f32) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let samples: Vec<f64> = pcm.iter().map(|&s| f64::from(s)).collect();
    let total: f64 = samples.iter().map(|x| x * x).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let mag_sq = goertzel_mag_sq(&samples, f64::from(hz), f64::from(fs));
    ((2.0 / samples.len() as f64) * mag_sq / total) as f32
}

// ---------------------------------------------------------------------------
// Test 1: slin16 negotiation + 640-byte wire frames.
// ---------------------------------------------------------------------------

#[test]
fn slin16_negotiated_and_wire_frames_are_640_bytes() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let (sizes_tx, sizes_rx) = std::sync::mpsc::channel::<usize>();
    let peer_thread = thread::spawn(move || {
        run_accept_and_capture_peer(&peer, VoiceFormat::Slin16, &sizes_tx);
    });

    let mic = MicSinks::default();
    let backend = TestBackend {
        mic: mic.clone(),
        capture_output: "out:rx".to_string(), // unused by this call; nothing feeds it
        rx_capture: Arc::new(Mutex::new(Vec::new())),
    };
    let mut mgr = Manager::with_policy(Box::new(backend), CodecPolicy::PreferSlin16);
    assert_eq!(
        mgr.pipeline_sample_rate(),
        16_000,
        "PreferSlin16 pins a 16 kHz station"
    );

    let id = mgr
        .dial(spec(1, "out:s", peer_addr, CodecPolicy::PreferSlin16))
        .expect("dial");
    mgr.route(id, &MicId::new("in:a")).expect("route");
    assert!(wait_active(&mgr, id), "call must reach active");
    assert!(
        wait_negotiated(&mgr, id, VoiceFormat::Slin16),
        "a peer ACCEPTing slin16 must negotiate Slin16"
    );

    mgr.key(id).expect("key");
    // Push several 20 ms bus frames (320 samples @ 16 kHz) so at least one TX
    // voice frame reaches the wire.
    for _ in 0..10 {
        if !mic.push("in:a", &[0.3_f32; 320]) {
            thread::sleep(Duration::from_millis(20));
        }
    }

    let mut sizes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while sizes.len() < 3 && Instant::now() < deadline {
        if let Ok(s) = sizes_rx.recv_timeout(Duration::from_millis(300)) {
            sizes.push(s);
        }
    }
    assert!(
        !sizes.is_empty(),
        "peer must observe at least one TX voice frame"
    );
    assert!(
        sizes.iter().all(|&s| s == 640),
        "slin16 wire voice frames must be 640 bytes (320 samples @ 16 kHz, no resample); saw {sizes:?}"
    );

    mgr.hangup(id, None).expect("hangup should join cleanly");
    peer_thread.join().expect("peer thread joined");
}

// ---------------------------------------------------------------------------
// Test 2: G711U-only peer on the same (16 kHz) manager — 160-byte TX wire
// frames (downsampled at the edge) and the RX pitch canary (upsampled at the
// edge, pitch must survive).
// ---------------------------------------------------------------------------

#[test]
fn ulaw_wire_frames_and_rx_pitch_preserved_at_bus_rate() {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer_addr = peer.local_addr().unwrap();

    // 1.5 s of a 1 kHz / amp-8000 tone, µ-law @ 8 kHz — the RX stream the peer
    // will stream back right after ACCEPT.
    let tone_frames = ulaw_tone_frames(1.5, 1000.0, 8000.0);
    let (sizes_tx, sizes_rx) = std::sync::mpsc::channel::<usize>();
    let peer_thread = thread::spawn(move || {
        run_accept_stream_and_capture_peer(&peer, VoiceFormat::G711U, &tone_frames, &sizes_tx);
    });

    let mic = MicSinks::default();
    let rx_capture = Arc::new(Mutex::new(Vec::<f32>::new()));
    let backend = TestBackend {
        mic: mic.clone(),
        capture_output: "out:rx".to_string(),
        rx_capture: Arc::clone(&rx_capture),
    };
    let mut mgr = Manager::with_policy(Box::new(backend), CodecPolicy::PreferSlin16);

    let id = mgr
        .dial(spec(2, "out:rx", peer_addr, CodecPolicy::PreferSlin16))
        .expect("dial");
    // Route a mic too (separate device id — the previous test's "in:a" isn't
    // reused, but each test gets a fresh Manager anyway) so the TX 160-byte
    // assertion below has something to send.
    mgr.route(id, &MicId::new("in:b")).expect("route");
    assert!(wait_active(&mgr, id), "call must reach active");
    assert!(
        wait_negotiated(&mgr, id, VoiceFormat::G711U),
        "a peer only ACCEPTing G711U must negotiate G711U even on a slin16-preferring station"
    );

    // --- RX pitch canary -----------------------------------------------
    // The peer already started streaming its tone burst the instant it sent
    // ACCEPT (above), independent of anything the test does from here. Give it
    // time to fully decode+resample (8k -> 16k) onto the output bus; the
    // capturing backend has been pulling PCM (mostly silence, so far) since
    // `dial()` opened "out:rx", so nothing is missed by waiting instead of
    // clearing-then-waiting (silence contributes ~0 energy either way).
    thread::sleep(Duration::from_secs(2));
    let captured: Vec<f32> = rx_capture.lock().unwrap().clone();
    assert!(
        !captured.is_empty(),
        "RX tone must reach the 16 kHz output bus"
    );
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pcm: Vec<i16> = captured
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let ratio = dominant_ratio(&pcm, 1000.0, 16_000.0);
    assert!(
        ratio > 0.5,
        "RX 1 kHz tone must still read as 1 kHz after the 8k->16k upsample \
         (dominant_ratio={ratio:.3} over {} samples); a rate bug would read \
         near 2 kHz instead",
        pcm.len()
    );

    // --- TX wire-size assertion -----------------------------------------
    // `EdgeAudio`'s resampler batches on its own internal 256-sample CHUNK
    // (`astar-audio/src/resample.rs`), not on our 320-sample (20 ms @
    // 16 kHz) push size, so individual wire frames are NOT each exactly 160
    // bytes — they cycle irregularly (e.g. 128, 128, 128, 256, ...) as pushes
    // and internal chunks fall in and out of phase. `codec_edge.rs`'s own
    // `edge_audio_downsamples_tx_for_narrowband_wire_on_wideband_bus` unit test
    // hits the exact same batching and asserts on the AGGREGATE instead; do
    // the same here — over enough pushes, total wire bytes converges on
    // push_count * 160 (µ-law: 1 byte/sample @ 8 kHz).
    mgr.key(id).expect("key");
    let pushes = 25;
    for _ in 0..pushes {
        if !mic.push("in:b", &[0.3_f32; 320]) {
            thread::sleep(Duration::from_millis(20));
        }
    }
    let mut sizes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(4);
    while sizes.len() < pushes && Instant::now() < deadline {
        if let Ok(s) = sizes_rx.recv_timeout(Duration::from_millis(300)) {
            sizes.push(s);
        }
    }
    assert!(
        !sizes.is_empty(),
        "peer must observe at least one TX voice frame"
    );
    let total: usize = sizes.iter().sum();
    let expected = pushes * 160;
    assert!(
        total > expected.saturating_sub(400) && total <= expected,
        "G711U wire voice bytes must total ~{expected} (downsampled 16k->8k at \
         the edge, 1 byte/sample); saw {total} across {sizes:?}"
    );

    mgr.hangup(id, None).expect("hangup should join cleanly");
    peer_thread.join().expect("peer thread joined");
}

// ---------------------------------------------------------------------------
// Test 3: regression pin — `Manager::new` (default `CodecPolicy`) stays an
// 8 kHz station.
// ---------------------------------------------------------------------------

#[test]
fn default_manager_stays_narrowband() {
    let backend = TestBackend {
        mic: MicSinks::default(),
        capture_output: String::new(),
        rx_capture: Arc::new(Mutex::new(Vec::new())),
    };
    let mgr = Manager::new(Box::new(backend));
    assert_eq!(
        mgr.pipeline_sample_rate(),
        8_000,
        "Manager::new must stay an 8 kHz station even after slin16 support landed"
    );
}
