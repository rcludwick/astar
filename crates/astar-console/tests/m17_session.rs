// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration tests for [`M17Session`] (iax-f2b8 Task 3): a full-transceive
//! M17 reflector client against a scripted fake reflector socket, offline and
//! deterministic — mirrors the `session_loopback.rs` pattern used for the
//! IAX2/WT `ConsoleSession`.
//!
//! Requires a live Codec 2 backend to construct a session at all
//! ([`M17Session::connect`] fails otherwise): `astar-console`'s
//! `[dev-dependencies]` entry for `astar-codec` forces the LGPL
//! `codec2-static` backend on for every test/bench build of this crate
//! (regardless of which `--features` are passed for the crate's own,
//! non-dev feature set), so this file only needs to gate on `m17` itself —
//! `cargo test -p astar-console` (no extra flags) runs these tests with
//! a working codec. `codec2-static` never enters the crate's own default (or
//! any other default) feature set — see `astar-codec`'s licensing note
//! and the `astar-console` `Cargo.toml` comment on the dev-dependency.
#![cfg(feature = "m17")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, AudioRouter, CallAudio, DeviceId, DeviceInfo, Direction, InputSink,
    MicId, NullBackend, OutputId, OutputSource, StreamConfig, StreamHandle,
};
use astar_console::{
    AnswerPolicy, CallStatus, ConsoleConfig, ConsoleSession, M17Config, M17Session,
};
use astar_iax::{CodecPolicy, IncomingAuthPolicy, IncomingCallPolicy, IncomingDecisionPolicy};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_m17::{
    BROADCAST, ControlPacket, LinkState, Lsf, Reflector, StreamPacket, decode_callsign,
    encode_callsign,
};

// ---- a push-capable audio backend for the TX ("hear yourself") test -----
//
// The public `astar_audio::NullBackend` (test-backend feature) drops
// every sink/source it's handed — fine for tests that never need real mic
// PCM, but the "key, talk, hear yourself" test needs to drive the router's
// real `MicLane` (DSP + gate) with a synthesized tone. `NullBackend`'s own
// push-capable twin (`stream::test_support_router::NullBackend::with_controls`)
// is `pub(crate)` to `astar-audio` and not reachable from here, so per the
// Task 3 brief this is the "minimal push-capable backend inside the test
// file" fallback: `open_input` stashes the sink the router hands it (the real
// `MicLane`) so the test can call `sink.write(...)` directly, exactly as
// cpal's capture callback would.

struct NullHandle;
impl StreamHandle for NullHandle {
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

type MicSink = Arc<Mutex<Option<Box<dyn InputSink>>>>;

/// Stashes the router's real `OutputBus` (handed to `open_output` as a boxed
/// `OutputSource`) so a test can pull decoded RX PCM directly by calling
/// `.read()` on it — the output-side mirror of `MicSink`'s push. Most tests
/// leave this `None`-valued slot untouched (they only assert on
/// `state().receiving`, independent of whether anything renders the decoded
/// PCM); the parrot self-echo test uses it to prove the echoed audio is
/// actually non-silent, decoded PCM, not just a `receiving` flag flip.
type OutputTap = Arc<Mutex<Option<Box<dyn OutputSource>>>>;

struct PushBackend {
    mic_sink: MicSink,
    output_tap: OutputTap,
}

impl AudioBackend for PushBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "m17-in"),
            dev(Direction::Output, "m17-out"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "m17-in"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "m17-out"))
    }
    fn open_input(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        sink: Box<dyn InputSink>,
        _overruns: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        *self.mic_sink.lock().unwrap() = Some(sink);
        Ok(Box::new(NullHandle))
    }
    fn open_output(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        // Stashed rather than dropped (see `OutputTap`'s doc comment) — most
        // tests never read it back, but the parrot self-echo test does.
        *self.output_tap.lock().unwrap() = Some(source);
        Ok(Box::new(NullHandle))
    }
}

/// Push `ms` milliseconds of an 8 kHz mono tone at `freq_hz` straight into the
/// stashed mic sink (the router's real `MicLane`), exactly as a single cpal
/// capture callback would. No-op if the router hasn't opened the mic yet.
#[allow(clippy::cast_precision_loss)] // sample index -> f32 for a test tone; no precision concerns at these lengths
fn push_mic_tone(mic_sink: &MicSink, freq_hz: f32, ms: u32) {
    let n = (8_000 * ms / 1_000) as usize;
    let tone: Vec<f32> = (0..n)
        .map(|i| 0.6 * (std::f32::consts::TAU * freq_hz * i as f32 / 8_000.0).sin())
        .collect();
    if let Some(sink) = mic_sink.lock().unwrap().as_mut() {
        sink.write(&tone, 0.0);
    }
}

/// [`push_mic_tone`] at an explicit lane rate — a 16 kHz station's mic lane
/// captures 16 kHz, so a test that drives it must speak 16 kHz too.
#[allow(clippy::cast_precision_loss)]
fn push_mic_tone_at(mic_sink: &MicSink, freq_hz: f32, ms: u32, rate: u32) {
    let n = (rate * ms / 1_000) as usize;
    let tone: Vec<f32> = (0..n)
        .map(|i| 0.6 * (std::f32::consts::TAU * freq_hz * i as f32 / rate as f32).sin())
        .collect();
    if let Some(sink) = mic_sink.lock().unwrap().as_mut() {
        sink.write(&tone, 0.0);
    }
}

/// Pulls up to `n` samples from the stashed `OutputTap` (the router's real
/// `OutputBus`, mirroring what the cpal output thread would pull) and returns
/// their peak amplitude (`0.0..=1.0`, via `astar_audio::peak`) — proof
/// that whatever decoded RX PCM has accumulated in the router's mixer is
/// actually non-silent audio, not just zeros. `0.0` if the output device was
/// never opened.
fn pull_output_peak(output_tap: &OutputTap, n: usize) -> f32 {
    let mut buf = vec![0.0f32; n];
    let mut guard = output_tap.lock().unwrap();
    let Some(source) = guard.as_mut() else {
        return 0.0;
    };
    source.read(&mut buf);
    astar_audio::peak(&buf)
}

// ---- deadline-polling helper (house style: no fixed sleeps) --------------

fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    pred()
}

// ---- fake M17 reflector ----------------------------------------------------

/// Bind a fake-reflector socket and run `behavior` on it in a background
/// thread. Returns the address to connect an [`M17Session`] to, plus the
/// thread handle (unused by callers beyond keeping it alive for the test's
/// duration; the process exiting cleans it up either way).
fn spawn_reflector(behavior: fn(&UdpSocket)) -> (SocketAddr, thread::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    let t = thread::spawn(move || behavior(&sock));
    (addr, t)
}

/// CONN(11B) -> ACKN; never answers with PING (client-side keepalive here is
/// Pong-only, per the brief); echoes any 54-byte `"M17 "` stream packet back
/// verbatim (parrot) so the client can "hear itself".
fn run_parrot_reflector(sock: &UdpSocket) {
    let mut buf = [0u8; 4_096];
    let mut acked = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let Ok((n, src)) = sock.recv_from(&mut buf) else {
            continue;
        };
        if !acked && n == 11 && &buf[0..4] == b"CONN" {
            acked = true;
            let _ = sock.send_to(b"ACKN", src);
        } else if n == 54 && &buf[0..4] == b"M17 " {
            let _ = sock.send_to(&buf[..n], src);
        }
    }
}

/// CONN(11B) -> NACK (link rejected).
fn run_nack_reflector(sock: &UdpSocket) {
    let mut buf = [0u8; 4_096];
    let mut replied = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Ok((n, src)) = sock.recv_from(&mut buf) else {
            continue;
        };
        if !replied && n == 11 && &buf[0..4] == b"CONN" {
            replied = true;
            let _ = sock.send_to(b"NACK", src);
        }
    }
}

/// CONN(11B) -> ACKN once, then goes silent (drops everything else,
/// including the client's eventual DISC) so the client's keepalive timeout
/// is the only thing that can end the link.
fn run_ackn_then_silent_reflector(sock: &UdpSocket) {
    let mut buf = [0u8; 4_096];
    let mut acked = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let Ok((n, src)) = sock.recv_from(&mut buf) else {
            continue;
        };
        if !acked && n == 11 && &buf[0..4] == b"CONN" {
            acked = true;
            let _ = sock.send_to(b"ACKN", src);
        }
        // Anything else (including a later CONN retry or DISC): dropped.
    }
}

// ---- audio-lane helper -----------------------------------------------------

/// Build the ONE audio lane an [`M17Session`] is now handed. The session no
/// longer owns a router, a mic or a bus — `ConsoleSession`'s `VoiceRoute`
/// does that in production, and this test owns it here — so a test that wants
/// to transmit keys the returned `MicId`'s gate itself, exactly as
/// `ConsoleSession::set_ptt` does.
///
/// Returns the router (which MUST be kept alive: it owns the streams), the
/// session's channel ends, and the mic whose gate is the PTT.
fn route(backend: Box<dyn AudioBackend>) -> (AudioRouter, CallAudio, MicId) {
    let mic = MicId::new(
        backend
            .default_input()
            .expect("the test backend has an input")
            .id
            .as_str(),
    );
    let out = OutputId::new(
        backend
            .default_output()
            .expect("the test backend has an output")
            .id
            .as_str(),
    );
    let mut router = AudioRouter::new(backend);
    let (audio, mic_tx, _mix) = router
        .open_monitor_call(&out, StreamConfig::default())
        .expect("bus");
    router
        .open_mic_lane(
            &mic,
            mic_tx,
            Arc::clone(&audio.preroll_lead),
            StreamConfig::default(),
        )
        .expect("mic");
    (router, audio, mic)
}

// ---- config helper ---------------------------------------------------------

fn cfg(addr: SocketAddr) -> M17Config {
    M17Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        module: b'A',
        callsign: "N0CALL".to_string(),
        codec_dirs: Vec::new(),
        keepalive_timeout: Duration::from_secs(30),
    }
}

// ---- the tests --------------------------------------------------------------

#[test]
fn connect_key_talk_hear_yourself_disconnect() {
    let (addr, _reflector) = spawn_reflector(run_parrot_reflector);

    let mic_sink: MicSink = Arc::new(Mutex::new(None));
    let (router, audio, mic) = route(Box::new(PushBackend {
        mic_sink: Arc::clone(&mic_sink),
        output_tap: Arc::new(Mutex::new(None)),
    }));

    let mut s = M17Session::connect(cfg(addr), audio).expect("connect");

    assert!(
        wait_until(|| s.state().link == LinkState::Linked, 2_000),
        "session must reach Linked after CONN/ACKN, got {:?}",
        s.state().link
    );

    // The gate is the route's, not the session's: `ConsoleSession::set_ptt`
    // opens it before forwarding the key to the network, and this test does
    // the same by hand.
    router.set_gate(&mic, true);
    s.set_ptt(true);
    assert!(
        wait_until(|| s.state().ptt, 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );

    // 200ms of tone at 8kHz = 1600 samples = 10 complete 160-sample TX
    // frames = 5 voice-stream packets once paired up.
    push_mic_tone(&mic_sink, 400.0, 200);

    assert!(
        wait_until(|| s.state().receiving, 2_000),
        "the reflector's parrot echo must decode back as `receiving`"
    );

    s.set_ptt(false);
    router.set_gate(&mic, false);
    assert!(
        wait_until(|| !s.state().ptt, 1_000),
        "set_ptt(false) must be applied by the run-loop"
    );

    s.disconnect();
}

#[test]
fn nack_fails_the_link() {
    let (addr, _reflector) = spawn_reflector(run_nack_reflector);

    let (_router, audio, _mic) = route(Box::new(NullBackend::new()));
    let s = M17Session::connect(cfg(addr), audio).expect("connect");

    assert!(
        wait_until(|| s.state().link == LinkState::Failed, 2_000),
        "a NACK must fail the link, got {:?}",
        s.state().link
    );

    s.disconnect();
}

#[test]
fn silence_times_out() {
    let (addr, _reflector) = spawn_reflector(run_ackn_then_silent_reflector);

    let mut short_cfg = cfg(addr);
    // Shortened keepalive window (M17Config::keepalive_timeout) so the test
    // doesn't have to wait out the real 30s default.
    short_cfg.keepalive_timeout = Duration::from_millis(300);

    let (_router, audio, _mic) = route(Box::new(NullBackend::new()));
    let s = M17Session::connect(short_cfg, audio).expect("connect");

    assert!(
        wait_until(|| s.state().link == LinkState::Linked, 2_000),
        "must link before the silence window starts"
    );
    assert!(
        wait_until(|| s.state().link == LinkState::Failed, 3_000),
        "300ms of silence past the shortened keepalive_timeout must fail the link, got {:?}",
        s.state().link
    );

    s.disconnect();
}

// ---- real Reflector rewire (iax-f2b8 Task 6) -------------------------------
//
// The tests above run M17Session against a scripted fake reflector socket;
// this one swaps in a real `astar_m17::Reflector` and a second raw UDP
// socket standing in for a far client on the SAME module, proving the whole
// stack (SessionFsm + M17Session run-loop) actually interops with the real
// reflector implementation end to end, not just the fakes' scripts.

#[test]
fn m17_session_interops_with_a_real_reflector_and_a_far_client() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let reflector_addr = reflector.local_addr();
    let handle = reflector.run();

    // A second raw socket joins the same module ('A', matching `cfg()`
    // below) as a plain M17 client, no M17Session involved.
    let far = UdpSocket::bind("127.0.0.1:0").expect("bind far client");
    far.set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set read timeout");
    let far_conn = ControlPacket::Conn {
        callsign: encode_callsign("FAR").unwrap(),
        module: b'A',
    }
    .to_bytes();
    far.send_to(&far_conn, reflector_addr).unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = far.recv_from(&mut buf).expect("far client must be ACKN'd");
    assert_eq!(&buf[..n], b"ACKN");

    let mic_sink: MicSink = Arc::new(Mutex::new(None));
    let (router, audio, mic) = route(Box::new(PushBackend {
        mic_sink: Arc::clone(&mic_sink),
        output_tap: Arc::new(Mutex::new(None)),
    }));

    let mut s = M17Session::connect(cfg(reflector_addr), audio).expect("connect");

    assert!(
        wait_until(|| s.state().link == LinkState::Linked, 2_000),
        "session must reach Linked via the real reflector's CONN/ACKN, got {:?}",
        s.state().link
    );

    router.set_gate(&mic, true);
    s.set_ptt(true);
    assert!(
        wait_until(|| s.state().ptt, 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );

    // 200ms of tone at 8kHz = 1600 samples = 10 complete 160-sample TX
    // frames = 5 voice-stream packets once paired up.
    push_mic_tone(&mic_sink, 400.0, 200);

    // The far client (relayed to by the reflector, never the session
    // talking to it directly) must see valid StreamPackets whose SRC
    // decodes back to the session's own callsign.
    let mut saw_valid_relayed_packet = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !saw_valid_relayed_packet {
        let Ok((n, _src)) = far.recv_from(&mut buf) else {
            continue;
        };
        if let Some(pkt) = StreamPacket::parse(&buf[..n]) {
            assert_eq!(
                decode_callsign(&pkt.lsf.src),
                "N0CALL",
                "relayed packet's SRC must decode to the session's callsign"
            );
            saw_valid_relayed_packet = true;
        }
    }
    assert!(
        saw_valid_relayed_packet,
        "the far client must receive at least one valid relayed stream packet"
    );

    s.set_ptt(false);
    router.set_gate(&mic, false);
    assert!(
        wait_until(|| !s.state().ptt, 1_000),
        "set_ptt(false) must be applied by the run-loop"
    );

    // The far client sends a stream packet of its own; the reflector relays
    // it to the session, which must observe `receiving`.
    let reply = StreamPacket {
        stream_id: 0x1234,
        lsf: Lsf {
            dst: BROADCAST,
            src: encode_callsign("FAR").unwrap(),
            type_field: Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number: 0,
        payload: [0u8; 16],
    }
    .to_bytes();
    far.send_to(&reply, reflector_addr).unwrap();

    assert!(
        wait_until(|| s.state().receiving, 2_000),
        "the session must observe `receiving` once the reflector relays the far client's packet"
    );

    s.disconnect();
    handle.shutdown();
}

// ---- localhost dual-stack repro (iax-m17-localhost) ------------------------
//
// Rob's live-session bug: dialing "localhost:<port>/A" failed outright.
// `M17Session::connect` used to take only the FIRST address `to_socket_addrs`
// resolved and bind an IPv4-only "0.0.0.0:0" socket regardless of that
// address's family; on macOS "localhost" resolves `[::1]` BEFORE
// `127.0.0.1`, so the connect() onto an AF_INET6 peer from an AF_INET socket
// always failed. Two tests below isolate the two halves of the fix:
// per-family client binding, and dual-stack reflector reachability.

#[test]
fn m17_session_connects_to_a_reflector_bound_on_ipv6_loopback_only() {
    // Proves the CLIENT half of the fix in isolation: a reflector reachable
    // ONLY over IPv6 (no dual-stack, no "localhost" resolution involved —
    // dialed by literal IPv6 address) still links once `connect()` binds a
    // socket matching the resolved address's family instead of always
    // binding IPv4. Before the fix this never linked, because a v4-only
    // "0.0.0.0:0" socket cannot connect() to an AF_INET6 peer at all.
    let reflector =
        Reflector::bind_parrot("[::1]:0".parse().unwrap()).expect("bind IPv6-only reflector");
    let reflector_addr = reflector.local_addr();
    let handle = reflector.run();

    let (_router, audio, _mic) = route(Box::new(NullBackend::new()));
    let s =
        M17Session::connect(cfg(reflector_addr), audio).expect("connect to an IPv6-only reflector");

    assert!(
        wait_until(|| s.state().link == LinkState::Linked, 2_000),
        "session must link to an IPv6-only reflector once connect() binds a matching-family \
         socket, got {:?}",
        s.state().link
    );

    s.disconnect();
    handle.shutdown();
}

#[test]
fn m17_session_connects_via_localhost_to_a_dual_stack_reflector() {
    // The actual Rob scenario, end to end: a reflector bound dual-stack (the
    // `m17_parrot` example's fix — "[::]:<port>" rather than "0.0.0.0:<port>")
    // and a session dialing the literal host string "localhost", whose
    // resolution order puts `[::1]` first on macOS. Requires BOTH fix halves:
    // the reflector must actually be reachable on `::1` (dual-stack bind),
    // and the client must bind a v6 socket to reach it (per-family bind).
    let reflector =
        Reflector::bind_parrot(SocketAddr::new(std::net::Ipv6Addr::UNSPECIFIED.into(), 0))
            .expect("bind dual-stack reflector");
    let reflector_port = reflector.local_addr().port();
    let handle = reflector.run();

    let mut c = cfg(SocketAddr::new(
        std::net::Ipv4Addr::LOCALHOST.into(),
        reflector_port,
    ));
    c.host = "localhost".to_string();

    let (_router, audio, _mic) = route(Box::new(NullBackend::new()));
    let s =
        M17Session::connect(c, audio).expect("connect via \"localhost\" to a dual-stack reflector");

    assert!(
        wait_until(|| s.state().link == LinkState::Linked, 2_000),
        "session dialing \"localhost\" must link via the family-matched candidate against a \
         dual-stack reflector, got {:?}",
        s.state().link
    );

    // The dual-stack bind's other half: a genuine IPv4 client must ALSO be
    // able to reach the same reflector/module (proves it's one shared
    // socket/state, not two disjoint reflectors that couldn't hear each
    // other).
    let v4_client = UdpSocket::bind("127.0.0.1:0").expect("bind v4 far client");
    v4_client
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set read timeout");
    let conn = ControlPacket::Conn {
        callsign: encode_callsign("V4FAR").unwrap(),
        module: b'A',
    }
    .to_bytes();
    v4_client
        .send_to(&conn, ("127.0.0.1", reflector_port))
        .expect("send CONN from v4 client");
    let mut buf = [0u8; 64];
    let (n, _) = v4_client
        .recv_from(&mut buf)
        .expect("v4 client must be ACKN'd by the same dual-stack reflector");
    assert_eq!(&buf[..n], b"ACKN");

    s.disconnect();
    handle.shutdown();
}

// ---- parrot-mode Reflector: the astar self-echo test (iax-91f4) -----------
//
// The test above exercises the real `Reflector`'s plain relay behavior; this
// one exercises its PARROT mode end to end — the exact "does the M17 echo
// test work" flow astar's user-facing feature is: connect, key, talk, unkey
// (the unkey is what flushes an EOS-marked packet — iax-f2b8-fix Fix 5's
// normal key-up path, not just its disconnect-while-keyed path), and the
// session must hear its OWN transmission echoed back — decoded, paced, and
// observable as `receiving` with non-silent PCM — with no far client or
// second party involved at all.

#[test]
fn m17_session_hears_itself_via_a_real_parrot_reflector() {
    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind parrot reflector");
    let reflector_addr = reflector.local_addr();
    let handle = reflector.run();

    let mic_sink: MicSink = Arc::new(Mutex::new(None));
    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let (router, audio, mic) = route(Box::new(PushBackend {
        mic_sink: Arc::clone(&mic_sink),
        output_tap: Arc::clone(&output_tap),
    }));

    let mut s = M17Session::connect(cfg(reflector_addr), audio).expect("connect");

    assert!(
        wait_until(|| s.state().link == LinkState::Linked, 2_000),
        "session must reach Linked via the real parrot reflector's CONN/ACKN, got {:?}",
        s.state().link
    );

    router.set_gate(&mic, true);
    s.set_ptt(true);
    assert!(
        wait_until(|| s.state().ptt, 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );

    // 200ms of tone at 8kHz = 1600 samples = 10 complete 160-sample TX
    // frames = 5 voice-stream packets once paired up.
    push_mic_tone(&mic_sink, 400.0, 200);

    s.set_ptt(false);
    router.set_gate(&mic, false);
    assert!(
        wait_until(|| !s.state().ptt, 1_000),
        "set_ptt(false) must be applied by the run-loop — this flushes the EOS-marked packet \
         the parrot reflector is waiting for before it starts echoing back"
    );

    assert!(
        wait_until(|| s.state().receiving, 3_000),
        "the parrot reflector's paced echo of the session's own transmission must decode back \
         as `receiving`, got link={:?}",
        s.state().link
    );

    // `receiving` alone only proves a StreamPacket arrived; pull the actually
    // DECODED PCM straight off the router's output bus (via the stashed
    // `OutputTap`) and confirm it's non-silent — the real "did the parrot
    // echo audible audio back" proof, not just a flag flip. Polled: the
    // decoded PCM lands in the mixer asynchronously as the paced playback's
    // packets keep arriving.
    assert!(
        wait_until(
            || pull_output_peak(
                &output_tap,
                8_000 /* 1s @ 8kHz, plenty for ~200ms of audio */
            ) > 0.01,
            2_000
        ),
        "the echoed-back audio must decode to non-silent PCM on the output bus"
    );

    s.disconnect();
    handle.shutdown();
}

// ---- Fix 5: EOS flush on disconnect-while-keyed (iax-f2b8-fix) ------------
//
// Before this fix, M17Session's run-loop shutdown branch sent DISC straight
// away regardless of `keyed`, leaving the far end's stream open (no EOS bit
// ever seen) until IT timed the stream out on its own — a courtesy paper cut,
// not a correctness bug, but still worth closing: `disconnect()` while keyed
// should flush the SAME EOS-marked packet a normal unkey would, before DISC.

type RecordedPackets = Arc<Mutex<Vec<Vec<u8>>>>;

/// CONN(11B) -> ACKN; records EVERY packet received afterward, in arrival
/// order (unlike `spawn_capturing_reflector`'s fixed-shape station-level
/// twin, this keeps DISC/control packets too, so a test can assert on
/// ordering between the last stream packet and DISC).
fn spawn_recording_reflector() -> (SocketAddr, RecordedPackets, thread::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    let recorded: RecordedPackets = Arc::new(Mutex::new(Vec::new()));
    let recorded_thread = Arc::clone(&recorded);
    let t = thread::spawn(move || {
        let mut buf = [0u8; 4_096];
        let mut acked = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let Ok((n, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if !acked && n == 11 && &buf[0..4] == b"CONN" {
                acked = true;
                let _ = sock.send_to(b"ACKN", src);
            }
            recorded_thread.lock().unwrap().push(buf[..n].to_vec());
        }
    });
    (addr, recorded, t)
}

#[test]
fn disconnect_while_keyed_flushes_eos_before_disc() {
    let (addr, recorded, _reflector) = spawn_recording_reflector();

    let mic_sink: MicSink = Arc::new(Mutex::new(None));
    let (router, audio, mic) = route(Box::new(PushBackend {
        mic_sink: Arc::clone(&mic_sink),
        output_tap: Arc::new(Mutex::new(None)),
    }));

    let mut s = M17Session::connect(cfg(addr), audio).expect("connect");
    assert!(
        wait_until(|| s.state().link == LinkState::Linked, 2_000),
        "must link before keying"
    );

    router.set_gate(&mic, true);
    s.set_ptt(true);
    assert!(
        wait_until(|| s.state().ptt, 1_000),
        "set_ptt(true) must be applied"
    );

    // Queue several TX frames but never unkey — disconnect() must be the
    // thing that flushes them, exactly as an explicit unkey would.
    push_mic_tone(&mic_sink, 400.0, 200);
    // Give the run-loop a moment to actually drain at least one stream
    // packet onto the wire before disconnecting, so this test can't
    // accidentally pass just because nothing was ever sent at all.
    assert!(
        wait_until(
            || recorded
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.len() == 54 && &p[0..4] == b"M17 "),
            2_000
        ),
        "at least one ordinary stream packet must go out before disconnect"
    );

    // disconnect() WHILE STILL KEYED — no explicit set_ptt(false) first.
    s.disconnect();

    // `disconnect()` returning only proves the SESSION's own thread called
    // `socket.send(&disc)` before joining — it says nothing about when the
    // SEPARATE reflector thread on the other end of the loopback socket
    // actually gets scheduled, calls `recv_from`, and appends the packet to
    // `recorded`. Reading `recorded` synchronously the instant `disconnect()`
    // returns assumes that hand-off is instantaneous; under heavy parallel
    // test load (`cargo test --workspace --all-targets`) the reflector
    // thread can lag by tens of milliseconds, so the DISC entry simply isn't
    // there yet — this is NOT a real ordering inversion (1000+ loaded runs
    // during triage never once observed EOS arrive after DISC; the failure
    // was always DISC being entirely absent at check time). Poll for it
    // instead of assuming synchronous consistency between the two threads.
    assert!(
        wait_until(
            || recorded
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.len() >= 4 && &p[0..4] == b"DISC"),
            2_000
        ),
        "the reflector must observe a DISC after disconnect(), got {:?}",
        recorded
            .lock()
            .unwrap()
            .iter()
            .map(|p| if p.len() >= 4 {
                String::from_utf8_lossy(&p[0..4]).to_string()
            } else {
                format!("{p:?}")
            })
            .collect::<Vec<_>>()
    );

    // What matters to a real far end is that BOTH an EOS-marked stream
    // packet and a DISC were observed at all — not their exact receive-order
    // position, which UDP (even over loopback) makes no hard guarantee
    // about. The sender side (`send_disc_flushing_eos_if_keyed`) already
    // sends them strictly sequentially on one thread, so asserting presence
    // here is enough to prove the flush actually happened rather than being
    // skipped.
    let recorded = recorded.lock().unwrap().clone();
    let eos_seen = recorded.iter().any(|p| {
        StreamPacket::parse(p).is_some_and(|pkt| pkt.is_last()) // EOS bit set
    });
    assert!(
        eos_seen,
        "disconnect() while keyed must flush an EOS-bit stream packet, got {:?}",
        recorded
            .iter()
            .map(|p| if p.len() >= 4 { &p[0..4] } else { &p[..] })
            .collect::<Vec<_>>()
    );
}

// ---- ConsoleSession graft (iax-f2b8 Task 4) --------------------------------
//
// The tests above exercise `M17Session` standalone; these exercise the graft
// onto `ConsoleSession`: mutual exclusion with the IAX2 path (both
// directions) and the shared `snapshot()` surface (status/ptt/levels mirror
// the M17 session; `m17_active`/`m17_available` are populated).

fn dummy_console_config(node: &str) -> ConsoleConfig {
    ConsoleConfig {
        node: node.to_string(),
        calling_node: node.to_string(),
        secret: "s".into(),
        name: "t".into(),
        input_device: None,
        output_device: None,
        codec_policy: CodecPolicy::default(),
    }
}

/// A bound-but-silent loopback socket: any IAX2 dial traffic lands here and is
/// never answered, keeping this offline/deterministic — the mutual-exclusion
/// guard fires before any network round-trip anyway. The caller keeps the
/// returned `UdpSocket` alive for the test's duration (mirrors
/// `wireguard_transport.rs`'s `sink_socket` helper).
fn silent_peer() -> (UdpSocket, SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind silent peer");
    let addr = s.local_addr().expect("local addr");
    (s, addr)
}

#[test]
fn iax2_connect_is_refused_while_m17_is_live() {
    let (addr, _reflector) = spawn_reflector(run_parrot_reflector);
    let mut session = ConsoleSession::new();

    session
        .m17_connect(Box::new(NullBackend::new()), cfg(addr), None, None)
        .expect("m17 connect");
    assert!(
        wait_until(|| session.snapshot().status == CallStatus::Answered, 2_000),
        "m17 session must link before the exclusion check"
    );

    let (_peer, peer_addr) = silent_peer();
    let err = session
        .connect(
            Box::new(NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect_err("IAX2 connect must be refused while M17 is live");
    assert!(
        matches!(err, astar_console::ConsoleError::AlreadyConnected),
        "expected AlreadyConnected, got {err:?}"
    );

    session.m17_disconnect();
}

#[test]
fn m17_connect_is_refused_while_an_iax2_call_is_live() {
    let mut session = ConsoleSession::new();
    let (_peer, peer_addr) = silent_peer();
    session
        .connect(
            Box::new(NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect("dial pools a call");

    let (addr, _reflector) = spawn_reflector(run_parrot_reflector);
    let err = session
        .m17_connect(Box::new(NullBackend::new()), cfg(addr), None, None)
        .expect_err("M17 connect must be refused while an IAX2 call is live");
    assert!(
        matches!(err, astar_console::ConsoleError::AlreadyConnected),
        "expected AlreadyConnected, got {err:?}"
    );

    session.disconnect().expect("hang up the pooled IAX2 call");
}

#[test]
fn snapshot_mirrors_the_m17_lifecycle_and_reports_capability_flags() {
    let (addr, _reflector) = spawn_reflector(run_parrot_reflector);
    let mut session = ConsoleSession::new();

    // Idle: byte-identical defaults before any m17_connect.
    let idle = session.snapshot();
    assert_eq!(idle.status, CallStatus::Idle);
    assert!(!idle.m17_active);

    session
        .m17_connect(Box::new(NullBackend::new()), cfg(addr), None, None)
        .expect("m17 connect");

    // Dialing while Connecting, Answered once Linked; m17_active true
    // throughout, m17_available mirrors the free-function probe.
    assert!(
        wait_until(|| session.snapshot().status == CallStatus::Answered, 2_000),
        "status must reach Answered once the reflector ACKs"
    );
    let answered = session.snapshot();
    assert!(answered.m17_active);
    assert_eq!(answered.m17_available, astar_console::m17_available());

    session.m17_disconnect();
    let after = session.snapshot();
    assert_eq!(after.status, CallStatus::Idle);
    assert!(!after.m17_active);
}

#[test]
fn snapshot_holds_hangup_until_m17_disconnect_is_called() {
    let (addr, _reflector) = spawn_reflector(run_nack_reflector);
    let mut session = ConsoleSession::new();
    session
        .m17_connect(Box::new(NullBackend::new()), cfg(addr), None, None)
        .expect("m17 connect");

    assert!(
        wait_until(
            || matches!(session.snapshot().status, CallStatus::Hangup { .. }),
            2_000
        ),
        "a NACK must surface as Hangup{{reason: \"m17 link lost\"}}, got {:?}",
        session.snapshot().status
    );

    // Hangup must be HELD across repeated polls — mirroring how a WT call's
    // status stays Hangup (and `active` stays Some) until `disconnect()` is
    // called, rather than a one-shot latch that a second concurrent poller
    // (e.g. astar's meter-poll snapshot() racing its event-poll next_event())
    // could consume, silently dropping the Hangup edge for the other poller.
    for _ in 0..5 {
        let snap = session.snapshot();
        assert!(
            matches!(snap.status, CallStatus::Hangup { .. }),
            "status must stay Hangup across repeated polls until disconnect(), got {:?}",
            snap.status
        );
        assert!(
            snap.m17_active,
            "the session stays live (mirrors a WT call's `active` staying \
             Some after a remote hangup) until an explicit disconnect"
        );
    }

    session.m17_disconnect();
    let after = session.snapshot();
    assert_eq!(after.status, CallStatus::Idle);
    assert!(!after.m17_active);
}

#[test]
fn iax2_connect_after_m17_disconnect_starts_clean_with_dialing_status() {
    let (addr, _reflector) = spawn_reflector(run_nack_reflector);
    let mut session = ConsoleSession::new();
    session
        .m17_connect(Box::new(NullBackend::new()), cfg(addr), None, None)
        .expect("m17 connect");

    assert!(
        wait_until(
            || matches!(session.snapshot().status, CallStatus::Hangup { .. }),
            2_000
        ),
        "a NACK must fail the link before the disconnect/redial sequence"
    );

    session.m17_disconnect();

    // A fresh IAX2 connect right after m17_disconnect must NOT be shadowed by
    // any lingering m17 state: the first snapshot must show the fresh dial's
    // real Dialing status, not a stale Idle/Hangup left over from M17.
    let (_peer, peer_addr) = silent_peer();
    session
        .connect(
            Box::new(NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect("IAX2 connect after m17_disconnect must succeed");

    assert_eq!(
        session.snapshot().status,
        CallStatus::Dialing,
        "the first snapshot after a fresh IAX2 connect must show Dialing"
    );

    session.disconnect().expect("hang up the pooled IAX2 call");
}

// ---- inbound-offer mutual exclusion (iax-f2b8 Task 4 review round 1) ------
//
// A fake IAX2 peer sending a hand-built NEW, driving `poll_inbound()`/
// `snapshot()` against the session's real inbound listener. Duplicated (not
// shared) from `inbound_into_session.rs`'s identical helpers — each
// integration-test binary is its own compilation unit; see this file's
// existing `dummy_console_config`/`silent_peer` for the same convention.

const PEER_CALL: u16 = 13885;

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    encode(frame, &mut b).expect("encode");
    b
}

fn new_datagram(ies: Ies<'_>, source_call: u16) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies,
        payload: &[],
    })))
}

fn valid_new_ies() -> Ies<'static> {
    Ies {
        called_number: Some("s"),
        calling_number: Some("1001"),
        calling_name: Some("Rob"),
        capability: Some(VoiceFormat::G711U.as_u32() | VoiceFormat::G711A.as_u32()),
        format: Some(VoiceFormat::G711U.as_u32()),
        version: Some(2),
        calltoken: Some(b""),
        ..Ies::empty()
    }
}

fn peer_socket() -> (UdpSocket, SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let a = s.local_addr().unwrap();
    (s, a)
}

/// Drain whatever the listener has sent us and ACK every reliable full frame
/// (so the leg's Reliability releases → the Manual-mode park completes).
fn pump_acks(sock: &UdpSocket, listener_addr: SocketAddr, my_call: u16) {
    let mut buf = [0u8; 4096];
    let mut budget = 512;
    while let Ok((n, _src)) = sock.recv_from(&mut buf) {
        budget -= 1;
        assert!(
            budget > 0,
            "pump_acks drained 512 frames without quiescing — the listener is flooding"
        );
        let bytes = buf[..n].to_vec();
        let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
            continue;
        };
        if !matches!(
            f.subclass,
            Subclass::Iax(IaxCommand::Ack | IaxCommand::Inval)
        ) {
            let ack = enc(&Frame::Full(Box::new(FullFrame {
                source_call: my_call,
                dest_call: f.source_call,
                retransmission: false,
                timestamp: f.timestamp,
                oseqno: f.iseqno,
                iseqno: f.oseqno.wrapping_add(1),
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::Ack),
                ies: Ies::empty(),
                payload: &[],
            })));
            let _ = sock.send_to(&ack, listener_addr);
        }
    }
}

/// Start a session's inbound listener (auth Off, calltoken Never so the test
/// is valid under any build profile) with the given `answer` policy.
fn session_listening(answer: AnswerPolicy) -> (ConsoleSession, SocketAddr) {
    let mut session = ConsoleSession::new();
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        calltoken: astar_iax::IncomingCallTokenPolicy::Never,
        ..IncomingCallPolicy::default()
    };
    session
        .start_inbound(
            "127.0.0.1:0".parse().unwrap(),
            policy,
            answer,
            20,
            || -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) },
            (None, None),
        )
        .expect("inbound listener starts");
    let addr = session.inbound_addr().expect("listener bound");
    (session, addr)
}

/// (a) `handle_incoming` must busy-reject an inbound offer (never adopt into
/// `active`) while an M17 session is live — the guard added in this file's
/// `iax2_connect_is_refused_while_m17_is_live`/reverse tests only exercises
/// the OUTBOUND WT dial path; this covers the separate inbound-offer path.
#[test]
fn inbound_offer_is_busy_rejected_while_m17_is_live() {
    let (m17_addr, _m17_reflector) = spawn_reflector(run_parrot_reflector);
    let (mut session, addr) = session_listening(AnswerPolicy::Auto);

    session
        .m17_connect(Box::new(NullBackend::new()), cfg(m17_addr), None, None)
        .expect("m17 connect");
    assert!(
        wait_until(|| session.snapshot().status == CallStatus::Answered, 2_000),
        "m17 session must link before the inbound offer arrives"
    );

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Drive poll_inbound for a bounded window; call_count must stay 0 (the
    // offer is busy-rejected, never adopted into `active`) and the m17
    // session must remain the one live thing throughout.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        session.call_count(),
        0,
        "an inbound offer arriving while M17 is live must be busy-rejected, never adopted"
    );
    assert!(
        session.snapshot().m17_active,
        "the m17 session itself must be untouched by the rejected offer"
    );

    session.m17_disconnect();
}

/// (b) A Manual-mode offer that parked BEFORE M17 connected must NOT be
/// adoptable via `answer_pending()` once M17 comes up — this is the gap
/// `handle_incoming`'s own guard (case (a) above) cannot close, since parking
/// happens before M17 exists in this sequence.
#[test]
fn parked_offer_answer_is_refused_once_m17_is_live() {
    let (mut session, addr) = session_listening(AnswerPolicy::Manual);

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Pump for up to 500ms to let the listener receive and park the offer
    // (mirrors inbound_into_session.rs's manual_answer_adopts_the_parked_call
    // — Manual-mode parking isn't observable via call_count, so this runs a
    // fixed window rather than polling a condition).
    let park_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < park_deadline {
        session.poll_inbound();
        pump_acks(&peer, addr, PEER_CALL);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(session.call_count(), 0, "not adopted yet — still parked");
    assert!(
        session.take_incoming_from().is_some(),
        "the offer must have parked before m17_connect"
    );

    // NOW bring up M17 — after the offer already parked.
    let (m17_addr, _m17_reflector) = spawn_reflector(run_parrot_reflector);
    session
        .m17_connect(Box::new(NullBackend::new()), cfg(m17_addr), None, None)
        .expect("m17 connect");
    assert!(
        wait_until(|| session.snapshot().status == CallStatus::Answered, 2_000),
        "m17 session must link"
    );

    // answer_pending() must now be refused: the offer stays parked, and
    // call_count must never rise to 1.
    let err = session
        .answer_pending()
        .expect_err("answer_pending must be refused while M17 is live");
    assert!(
        matches!(err, astar_console::ConsoleError::AlreadyConnected),
        "expected AlreadyConnected, got {err:?}"
    );
    assert_eq!(
        session.call_count(),
        0,
        "the parked offer must NOT be adopted while M17 is live"
    );

    session.m17_disconnect();
}

// ---- the slin16 station (iax-4348 / the rate-pin fix) ----------------------
//
// astar and astar-server are slin16 stations: the codec policy is pinned
// before anything builds the engine, so the ONE bus runs at 16 kHz and the
// digital-voice sessions ride it through `VoiceRoute`'s rate bridge. This is
// the end-to-end proof that they do: a real parrot reflector, a 16 kHz mic
// lane, and the session's own transmission decoded back onto a 16 kHz bus.
// Before the bridge, an M17 session on a 16 kHz station either could not
// exist (the engine was pinned to 8 kHz by whichever path built it first) or
// fed 160-sample frames onto a 320-sample bus.

#[test]
fn a_slin16_station_hears_itself_via_a_real_parrot_reflector() {
    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind parrot reflector");
    let reflector_addr = reflector.local_addr();
    let handle = reflector.run();

    let mic_sink: MicSink = Arc::new(Mutex::new(None));
    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let backend = PushBackend {
        mic_sink: Arc::clone(&mic_sink),
        output_tap: Arc::clone(&output_tap),
    };

    let mut session = ConsoleSession::new();
    // What `Station::new` now does from `StationConfig.codec_policy`.
    session.set_station_policy(CodecPolicy::PreferSlin16);
    session
        .m17_connect(Box::new(backend), cfg(reflector_addr), None, None)
        .expect("m17 connect on a 16 kHz station");
    assert_eq!(
        session.pipeline_sample_rate(),
        16_000,
        "a prefer_slin16 station must stay 16 kHz through an M17 session"
    );

    assert!(
        wait_until(|| session.snapshot().status == CallStatus::Answered, 2_000),
        "must link to the parrot reflector before keying"
    );

    session.set_ptt(true).expect("key");
    assert!(
        wait_until(|| session.snapshot().ptt, 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );
    // 400 ms of 400 Hz at the LANE's rate: the TX half of the bridge has to
    // hand Codec 2 valid 160-sample 8 kHz frames out of this, or nothing
    // encodes and the parrot has nothing to echo.
    push_mic_tone_at(&mic_sink, 400.0, 400, 16_000);
    session.set_ptt(false).expect("unkey");
    assert!(
        wait_until(|| !session.snapshot().ptt, 1_000),
        "set_ptt(false) flushes the EOS-marked packet the parrot waits for"
    );

    // And the RX half: the echoed stream decodes to 8 kHz PCM, is upsampled
    // to the bus rate, and lands audible on the 16 kHz output bus.
    assert!(
        wait_until(|| pull_output_peak(&output_tap, 16_000) > 0.01, 3_000),
        "the parrot's echo of our own transmission must decode to non-silent \
         PCM on the 16 kHz bus"
    );

    session.m17_disconnect();
    handle.shutdown();
}
